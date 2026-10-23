use dripsonar::search::{parse_duckduckgo, parse_searxng};

#[test]
fn searxng_fixture_normalizes_and_deduplicates() -> Result<(), Box<dyn std::error::Error>> {
    let hits = parse_searxng(include_bytes!("fixtures/search/searxng.json"))?;
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].url, "https://example.com/story");
    assert_eq!(hits[0].title, "Example company");
    assert_eq!(hits[0].description, "A new office");
    assert_eq!(hits[1].url, "https://example.org/next");
    for body in [b"{}".as_slice(), b"not json", b"{\"results\":null}"] {
        assert!(parse_searxng(body).is_err());
    }
    let body = serde_json::json!({"results": [{"url": "https://example.com", "title": "é".repeat(501), "content": "界".repeat(1001)}]});
    let hits = parse_searxng(&serde_json::to_vec(&body)?)?;
    assert_eq!(hits[0].title.chars().count(), 500);
    assert_eq!(hits[0].description.chars().count(), 1000);
    Ok(())
}

#[test]
fn duckduckgo_fixture_unwraps_links_and_skips_ads() {
    let hits = parse_duckduckgo(include_str!("fixtures/search/duckduckgo.html"));
    assert_eq!(hits.len(), 2, "{hits:?}");
    assert_eq!(hits[0].url, "https://example.com/story?q=a%2Bb&x=1");
    assert_eq!(hits[0].title, "Example company");
    assert_eq!(hits[0].description, "A new office & jobs");
    assert_eq!(hits[1].description, "Second result");
}
