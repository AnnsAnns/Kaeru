//! The `web_search` tool (M3): fetch matching pages, distill them with the
//! tool-free summarizer worker, and return only the bounded, cited result.
//! Raw pages never reach the main model (ADR-021).

use serde_json::{Value, json};
use tokio::task::JoinSet;

use crate::agent::fence;
use crate::error::ApiError;
use crate::events::Risk;
use crate::search::SearchResult;
use crate::tools::{Tool, ToolContext, ToolFuture};

/// Pages fetched (and summarized) per search; the rest are listed by snippet.
const MAX_FETCHED_PAGES: usize = 3;

pub struct WebSearchTool {
    max_results: usize,
}

impl WebSearchTool {
    pub fn new(max_results: usize) -> Self {
        Self {
            max_results: max_results.max(1),
        }
    }
}

impl Tool for WebSearchTool {
    fn name(&self) -> &'static str {
        "web_search"
    }

    fn description(&self) -> &'static str {
        "Search the web and return a short, cited summary of the most relevant \
         pages. Use this for current facts you do not already know."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query."
                }
            },
            "required": ["query"]
        })
    }

    /// Read-only egress: safe to run without consent.
    fn risk(&self, _input: &Value) -> Risk {
        Risk::Safe
    }

    fn execute(&self, input: Value, ctx: ToolContext) -> ToolFuture {
        let max_results = self.max_results;
        Box::pin(async move {
            let query = input
                .get("query")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|q| !q.is_empty())
                .ok_or_else(|| ApiError::config("web_search requires a non-empty \"query\""))?
                .to_owned();

            let hits = ctx.search.search(query, max_results).await?;
            if hits.is_empty() {
                return Ok("No web results were found for this query.".to_owned());
            }

            // Fetch the raw pages (untrusted) and build a cited corpus. Pages
            // are fetched concurrently, so three slow sites do not stack their
            // per-page timeouts into one long search.
            let fetched = hits.len().min(MAX_FETCHED_PAGES);
            let mut bodies: Vec<Option<String>> = vec![None; fetched];
            if fetched > 0 {
                let mut set = JoinSet::new();
                for (index, hit) in hits[..fetched].iter().enumerate() {
                    let search = std::sync::Arc::clone(&ctx.search);
                    let url = hit.url.clone();
                    set.spawn(async move { (index, search.fetch(url).await) });
                }
                while let Some(joined) = set.join_next().await {
                    match joined {
                        Ok((index, Ok(text))) if !text.trim().is_empty() => {
                            bodies[index] = Some(text);
                        }
                        Ok(_) => {}
                        Err(err) => tracing::warn!(
                            target: "agent_core::tools",
                            "page fetch task failed: {err}"
                        ),
                    }
                }
            }
            // Results stay in search order regardless of which fetch finished
            // first; a failed or empty fetch degrades to the hit's snippet.
            let mut corpus = String::new();
            for (index, hit) in hits[..fetched].iter().enumerate() {
                let body = bodies[index].as_deref().unwrap_or(hit.snippet.as_str());
                corpus.push_str(&format!("\n\n[{}] {}\n{}", hit.url, hit.title, body));
            }
            for hit in &hits[fetched..] {
                corpus.push_str(&format!("\n\n[{}] {}\n{}", hit.url, hit.title, hit.snippet));
            }

            // The corpus is raw untrusted page text: fence it before the worker
            // reads it, and let only the worker's distillation reach the model.
            let fenced = fence("web_search", &corpus);
            match ctx
                .workers
                .run("summarizer", &fenced, &ctx.audit, Some(ctx.turn_id))
                .await
            {
                Ok(output) => Ok(output.text),
                Err(err) => {
                    // Degrade to bounded, cited snippets — never to raw pages.
                    tracing::warn!(
                        target: "agent_core::tools",
                        "summarizer failed ({}); falling back to snippets: {}",
                        err.kind.as_str(),
                        err.message
                    );
                    Ok(fallback_snippets(&hits))
                }
            }
        })
    }
}

/// Bounded, cited fallback when the worker cannot summarize.
fn fallback_snippets(hits: &[SearchResult]) -> String {
    let mut out = String::from("Web search results (summary unavailable; snippets only):");
    for hit in hits {
        out.push_str(&format!(
            "\n- {} ({}) — {}",
            hit.title, hit.url, hit.snippet
        ));
        if out.chars().count() > 4000 {
            out.push_str("\n…");
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::workers::{SUMMARIZER_SYSTEM, Workers};
    use crate::audit::AuditLog;
    use crate::config::{DEFAULT_MODEL, WorkersConfig};
    use crate::events::CoreEvent;
    use crate::llm::{Cassette, ChatMessage, ChatRequest, FakeProvider, Interaction};
    use crate::search::FakeSearch;
    use std::sync::Arc;

    fn hits() -> Vec<SearchResult> {
        vec![SearchResult {
            title: "Frogs".into(),
            url: "https://example.test/frogs".into(),
            snippet: "RAW-PAGE-SECRET".into(),
        }]
    }

    /// The exact fenced corpus the tool builds for `hits` (fetch returns the
    /// snippet, so the body is the snippet).
    fn expected_fenced_corpus(hits: &[SearchResult]) -> String {
        let mut corpus = String::new();
        for hit in hits {
            corpus.push_str(&format!("\n\n[{}] {}\n{}", hit.url, hit.title, hit.snippet));
        }
        fence("web_search", &corpus)
    }

    fn context(worker_events: Option<Vec<CoreEvent>>) -> ToolContext {
        let model = DEFAULT_MODEL;
        let interactions = worker_events
            .map(|events| {
                vec![Interaction {
                    request: ChatRequest::new(
                        model,
                        vec![
                            ChatMessage::system(SUMMARIZER_SYSTEM),
                            ChatMessage::user(expected_fenced_corpus(&hits())),
                        ],
                    )
                    .with_max_tokens(Some(600)),
                    events,
                }]
            })
            .unwrap_or_default();
        let client: Arc<dyn crate::llm::LlmClient> =
            Arc::new(FakeProvider::from_cassette(Cassette {
                interactions,
                ..Cassette::new()
            }));
        let workers = Workers::from_config(Arc::clone(&client), model, &WorkersConfig::default());
        ToolContext {
            client,
            search: Arc::new(FakeSearch::from_results(hits())),
            workers: Arc::new(workers),
            audit: AuditLog::disabled(),
            turn_id: 1,
        }
    }

    #[tokio::test]
    async fn search_without_query_is_a_config_error() {
        let tool = WebSearchTool::new(5);
        let err = tool.execute(json!({}), context(None)).await.unwrap_err();
        assert_eq!(err.kind, crate::error::ApiErrorKind::Config);
    }

    #[tokio::test]
    async fn empty_results_are_reported() {
        let tool = WebSearchTool::new(5);
        let ctx = ToolContext {
            search: Arc::new(FakeSearch::new(vec![], Default::default())),
            ..context(None)
        };
        let out = tool.execute(json!({"query": "frogs"}), ctx).await.unwrap();
        assert!(out.contains("No web results"));
    }

    #[tokio::test]
    async fn distillation_replaces_raw_pages() {
        let tool = WebSearchTool::new(5);
        let ctx = context(Some(vec![
            CoreEvent::Delta {
                text: "DISTILLED ANSWER".into(),
            },
            CoreEvent::TurnDone { usage: None },
        ]));
        let out = tool.execute(json!({"query": "frogs"}), ctx).await.unwrap();
        assert_eq!(out, "DISTILLED ANSWER");
        assert!(
            !out.contains("RAW-PAGE-SECRET"),
            "raw page text leaked into the tool result"
        );
    }

    #[tokio::test]
    async fn summary_failure_degrades_to_snippets() {
        let tool = WebSearchTool::new(5);
        // No cassette interaction: the worker fails, so the tool falls back.
        let out = tool
            .execute(json!({"query": "frogs"}), context(None))
            .await
            .unwrap();
        assert!(out.contains("snippets only"), "got: {out}");
        assert!(out.contains("https://example.test/frogs"));
        assert!(out.contains("RAW-PAGE-SECRET"));
    }
}
