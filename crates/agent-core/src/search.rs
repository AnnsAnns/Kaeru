//! Web search providers (M3): a trait, the HTTP backends, and a fake for
//! offline tests. Every backend returns short hits and can fetch a page's raw
//! text; the raw text is untrusted and is only ever read by the summarizer
//! worker (ADR-021), never by the main model.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::Deserialize;
use tokio_stream::StreamExt as _;

use crate::config::{SearchConfig, SearchProviderKind};
use crate::error::{ApiError, ApiErrorKind, Result};
use crate::util::truncate_chars;

/// Hard cap on fetched page text fed to the summarizer (context hygiene).
pub const MAX_PAGE_CHARS: usize = 20_000;

/// Hard cap on how many bytes of a fetched page are read from the network at
/// all; the tighter character cap applies later, after tag stripping.
pub const MAX_FETCH_BYTES: usize = 256 * 1024;

/// One search hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

pub type SearchFuture = Pin<Box<dyn Future<Output = Result<Vec<SearchResult>>> + Send>>;
pub type FetchFuture = Pin<Box<dyn Future<Output = Result<String>> + Send>>;

/// A web-search backend. Search is read-only egress; fetching a page returns
/// raw, untrusted text that must be fenced before the model sees it.
pub trait SearchProvider: Send + Sync {
    fn search(&self, query: String, max_results: usize) -> SearchFuture;
    fn fetch(&self, url: String) -> FetchFuture;
}

/// A provider that is not configured; every call fails with a clear message.
pub struct DisabledSearch;

impl SearchProvider for DisabledSearch {
    fn search(&self, _query: String, _max_results: usize) -> SearchFuture {
        Box::pin(async {
            Err(ApiError::config(
                "web search is not configured; set [search] provider in data/config.toml",
            ))
        })
    }

    fn fetch(&self, _url: String) -> FetchFuture {
        Box::pin(async {
            Err(ApiError::config(
                "web search is not configured; set [search] provider in data/config.toml",
            ))
        })
    }
}

/// Build the configured provider. An unknown or `off` provider yields the
/// disabled one, so the agent still runs (just without web search).
pub fn from_config(config: &SearchConfig) -> std::sync::Arc<dyn SearchProvider> {
    match config.kind() {
        SearchProviderKind::Off => std::sync::Arc::new(DisabledSearch),
        kind => match HttpSearch::new(kind, &config.api_key, &config.base_url) {
            Ok(search) => std::sync::Arc::new(search),
            Err(err) => {
                tracing::warn!(target: "agent_core::search", "web search disabled: {err}");
                std::sync::Arc::new(DisabledSearch)
            }
        },
    }
}

/// The HTTP-backed providers (Brave, Tavily, a self-hosted SearxNG).
pub struct HttpSearch {
    http: reqwest::Client,
    kind: SearchProviderKind,
    api_key: String,
    base_url: String,
}

impl HttpSearch {
    pub fn new(kind: SearchProviderKind, api_key: &str, base_url: &str) -> Result<Self> {
        if kind == SearchProviderKind::Off {
            // `from_config` never gets here; say what is actually wrong instead
            // of the misleading "api_key must be set for this provider".
            return Err(ApiError::config(
                "web search is off; there is no provider to configure",
            ));
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| ApiError::internal(format!("failed to build search client: {e}")))?;
        let missing = match kind {
            SearchProviderKind::Brave | SearchProviderKind::Tavily => api_key.trim().is_empty(),
            SearchProviderKind::Searxng => base_url.trim().is_empty(),
            SearchProviderKind::Off => unreachable!("the off kind is handled above"),
        };
        if missing {
            let what = match kind {
                SearchProviderKind::Searxng => "[search] base_url",
                _ => "[search] api_key",
            };
            return Err(ApiError::config(format!(
                "{what} must be set for this provider"
            )));
        }
        Ok(Self {
            http,
            kind,
            api_key: api_key.trim().to_owned(),
            base_url: base_url.trim().trim_end_matches('/').to_owned(),
        })
    }
}

impl SearchProvider for HttpSearch {
    fn search(&self, query: String, max_results: usize) -> SearchFuture {
        let http = self.http.clone();
        let kind = self.kind;
        let api_key = self.api_key.clone();
        let base_url = self.base_url.clone();
        Box::pin(async move {
            let results = match kind {
                SearchProviderKind::Brave => {
                    brave_search(&http, &api_key, &query, max_results).await?
                }
                SearchProviderKind::Tavily => {
                    tavily_search(&http, &api_key, &query, max_results).await?
                }
                SearchProviderKind::Searxng => {
                    searxng_search(&http, &base_url, &query, max_results).await?
                }
                SearchProviderKind::Off => {
                    return Err(ApiError::config("web search is not configured"));
                }
            };
            Ok(results)
        })
    }

    fn fetch(&self, url: String) -> FetchFuture {
        let http = self.http.clone();
        Box::pin(async move { fetch_page_text(&http, &url).await })
    }
}

/* ---------- individual providers ---------- */

async fn brave_search(
    http: &reqwest::Client,
    api_key: &str,
    query: &str,
    max_results: usize,
) -> Result<Vec<SearchResult>> {
    let response = http
        .get("https://api.search.brave.com/res/v1/web/search")
        .query(&[("q", query), ("count", &max_results.to_string())])
        .header("Accept", "application/json")
        .header("X-Subscription-Token", api_key)
        .send()
        .await
        .map_err(search_transport_error)?;
    let body = checked_json(response).await?;
    let parsed: BraveResponse = serde_json::from_value(body).map_err(search_protocol_error)?;
    Ok(parsed
        .web
        .map(|web| web.results)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|hit| {
            let url = hit.url?;
            Some(SearchResult {
                title: hit.title.unwrap_or_default(),
                url,
                snippet: hit.description.unwrap_or_default(),
            })
        })
        .take(max_results)
        .collect())
}

async fn tavily_search(
    http: &reqwest::Client,
    api_key: &str,
    query: &str,
    max_results: usize,
) -> Result<Vec<SearchResult>> {
    let response = http
        .post("https://api.tavily.com/search")
        .json(&serde_json::json!({
            "api_key": api_key,
            "query": query,
            "max_results": max_results,
        }))
        .send()
        .await
        .map_err(search_transport_error)?;
    let body = checked_json(response).await?;
    let parsed: TavilyResponse = serde_json::from_value(body).map_err(search_protocol_error)?;
    Ok(parsed
        .results
        .into_iter()
        .filter_map(|hit| {
            Some(SearchResult {
                title: hit.title.unwrap_or_default(),
                url: hit.url?,
                snippet: hit.content.unwrap_or_default(),
            })
        })
        .take(max_results)
        .collect())
}

async fn searxng_search(
    http: &reqwest::Client,
    base_url: &str,
    query: &str,
    max_results: usize,
) -> Result<Vec<SearchResult>> {
    let url = format!("{base_url}/search");
    let response = http
        .get(&url)
        .query(&[("q", query), ("format", "json")])
        .send()
        .await
        .map_err(search_transport_error)?;
    let body = checked_json(response).await?;
    let parsed: SearxngResponse = serde_json::from_value(body).map_err(search_protocol_error)?;
    Ok(parsed
        .results
        .into_iter()
        .filter_map(|hit| {
            Some(SearchResult {
                title: hit.title.unwrap_or_default(),
                url: hit.url?,
                snippet: hit.content.unwrap_or_default(),
            })
        })
        .take(max_results)
        .collect())
}

/// Fetch a page and strip it to plain text. Non-`http(s)` schemes are refused
/// so a search hit cannot make the server read a local file.
pub async fn fetch_page_text(http: &reqwest::Client, url: &str) -> Result<String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| {
        ApiError::new(
            ApiErrorKind::Protocol,
            format!("bad result url {url:?}: {e}"),
        )
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ApiError::new(
            ApiErrorKind::Protocol,
            format!("refusing to fetch non-http(s) url {url:?}"),
        ));
    }
    let response = http
        .get(parsed)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(search_transport_error)?;
    let status = response.status();
    if !status.is_success() {
        return Err(ApiError::new(
            ApiErrorKind::Provider,
            format!("fetching {url} returned HTTP {}", status.as_u16()),
        ));
    }
    // Read at most `MAX_FETCH_BYTES` off the wire instead of buffering the
    // whole body: a hostile or enormous page cannot exhaust memory before the
    // text cap applies.
    let mut body: Vec<u8> = Vec::new();
    let mut stream = response.bytes_stream();
    while body.len() < MAX_FETCH_BYTES {
        let Some(chunk) = stream.next().await else {
            break;
        };
        let chunk = chunk
            .map_err(|e| ApiError::new(ApiErrorKind::Network, format!("cannot read {url}: {e}")))?;
        let take = chunk.len().min(MAX_FETCH_BYTES - body.len());
        body.extend_from_slice(&chunk[..take]);
    }
    Ok(strip_html(&String::from_utf8_lossy(&body)))
}

async fn checked_json(response: reqwest::Response) -> Result<serde_json::Value> {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(ApiError::new(
            ApiErrorKind::Provider,
            format!(
                "search provider returned HTTP {}: {}",
                status.as_u16(),
                truncate_chars(&body, 300)
            ),
        ));
    }
    serde_json::from_str(&body).map_err(search_protocol_error)
}

fn search_transport_error(e: reqwest::Error) -> ApiError {
    ApiError::new(ApiErrorKind::Network, format!("search request failed: {e}"))
}

fn search_protocol_error(e: serde_json::Error) -> ApiError {
    ApiError::new(
        ApiErrorKind::Protocol,
        format!("search provider response is not understood: {e}"),
    )
}

/// Strip an HTML document to plain text: drop `<script>`/`<style>` bodies,
/// then all tags, collapse whitespace, and cap the length. A simple state
/// machine avoids a parser dependency (C15) for what is a best-effort excerpt.
pub fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len().min(MAX_PAGE_CHARS));
    let mut chars = html.chars().peekable();
    let mut skip_tag = false;
    let mut skip_body_until: Option<&str> = None;
    while let Some(c) = chars.next() {
        if let Some(closer) = skip_body_until {
            if c == '<' {
                let mut tag = String::from("<");
                for c in chars.by_ref() {
                    tag.push(c);
                    if c == '>' {
                        break;
                    }
                }
                let lower = tag.to_ascii_lowercase();
                if lower.starts_with(closer) {
                    skip_body_until = None;
                }
            }
            continue;
        }
        if skip_tag {
            if c == '>' {
                skip_tag = false;
            }
            continue;
        }
        if c == '<' {
            let mut tag = String::from("<");
            for c in chars.by_ref() {
                tag.push(c);
                if c == '>' {
                    break;
                }
            }
            let lower = tag.to_ascii_lowercase();
            if lower.starts_with("<script") {
                skip_body_until = Some("</script");
            } else if lower.starts_with("<style") {
                skip_body_until = Some("</style");
            }
            continue;
        }
        out.push(c);
        if out.len() >= MAX_PAGE_CHARS {
            break;
        }
    }
    decode_entities(&out)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Decode the handful of HTML entities that matter for readable text.
fn decode_entities(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

/* ---------- provider response shapes ---------- */

#[derive(Debug, Default, Deserialize)]
struct BraveResponse {
    #[serde(default)]
    web: Option<BraveWeb>,
}

#[derive(Debug, Default, Deserialize)]
struct BraveWeb {
    #[serde(default)]
    results: Vec<BraveHit>,
}

#[derive(Debug, Default, Deserialize)]
struct BraveHit {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct TavilyResponse {
    #[serde(default)]
    results: Vec<GenericHit>,
}

#[derive(Debug, Default, Deserialize)]
struct SearxngResponse {
    #[serde(default)]
    results: Vec<GenericHit>,
}

#[derive(Debug, Default, Deserialize)]
struct GenericHit {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    content: Option<String>,
}

/// In-memory provider for tests and the `--fake` UI path.
pub struct FakeSearch {
    results: Vec<SearchResult>,
    pages: HashMap<String, String>,
}

impl FakeSearch {
    pub fn new(results: Vec<SearchResult>, pages: HashMap<String, String>) -> Self {
        Self { results, pages }
    }

    /// A provider that returns `results` and serves their snippets as pages.
    pub fn from_results(results: Vec<SearchResult>) -> Self {
        let pages = results
            .iter()
            .map(|hit| (hit.url.clone(), hit.snippet.clone()))
            .collect();
        Self { results, pages }
    }
}

impl SearchProvider for FakeSearch {
    fn search(&self, _query: String, max_results: usize) -> SearchFuture {
        let results: Vec<SearchResult> = self.results.iter().take(max_results).cloned().collect();
        Box::pin(async move { Ok(results) })
    }

    fn fetch(&self, url: String) -> FetchFuture {
        let page = self.pages.get(&url).cloned();
        Box::pin(async move {
            page.ok_or_else(|| {
                ApiError::new(
                    ApiErrorKind::NotFound,
                    format!("fake search has no page for {url}"),
                )
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_html_removes_tags_scripts_and_entities() {
        let html = "<html><head><style>p{color:red}</style></head>\
                    <body><h1>Title</h1><script>alert('x')</script>\
                    <p>Hello &amp; welcome</p></body></html>";
        let text = strip_html(html);
        assert!(text.contains("Title"));
        assert!(text.contains("Hello & welcome"));
        assert!(!text.contains("alert"));
        assert!(!text.contains("color:red"));
        assert!(!text.contains('<'));
    }

    #[tokio::test]
    async fn disabled_search_reports_that_it_is_unconfigured() {
        let err = DisabledSearch.search("x".into(), 5).await.unwrap_err();
        assert_eq!(err.kind, ApiErrorKind::Config);
    }

    #[tokio::test]
    async fn fake_search_returns_hits_and_serves_pages() {
        let provider = FakeSearch::from_results(vec![SearchResult {
            title: "Frogs".into(),
            url: "https://example.test/frogs".into(),
            snippet: "frogs are amphibians".into(),
        }]);
        let hits = provider.search("frogs".into(), 5).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Frogs");
        let page = provider.fetch(hits[0].url.clone()).await.unwrap();
        assert_eq!(page, "frogs are amphibians");
    }

    #[test]
    fn http_provider_requires_credentials() {
        assert!(HttpSearch::new(SearchProviderKind::Brave, "", "").is_err());
        assert!(HttpSearch::new(SearchProviderKind::Searxng, "", "").is_err());
        assert!(HttpSearch::new(SearchProviderKind::Brave, "key", "").is_ok());
        assert!(HttpSearch::new(SearchProviderKind::Searxng, "", "http://x").is_ok());
    }
}
