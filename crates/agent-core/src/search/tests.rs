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
