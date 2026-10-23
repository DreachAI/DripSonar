#![allow(clippy::unwrap_used)]

use dripsonar::page::{RenderOptions, is_challenge, render};
use url::Url;

#[test]
fn renders_main_content_metadata_and_links() {
    let html = include_str!("fixtures/page/sample.html");
    let base = Url::parse("https://example.com/").unwrap();
    let opts = RenderOptions {
        only_main_content: true,
        remove_base64_images: true,
        markdown: true,
        html: true,
        links: true,
        ..Default::default()
    };
    let r = render(html, &base, &opts);

    let md = r.markdown.unwrap();
    assert!(
        md.contains("MAINTEXT") && md.contains("Gizmo") && md.contains("| Name  | Price |"),
        "{md}"
    );
    assert!(md.contains("https://example.com/docs/guide/start"), "{md}");
    assert!(md.contains("https://example.com/docs/big.png"), "{md}");
    for gone in [
        "NAVTEXT",
        "HEADERTEXT",
        "FOOTERTEXT",
        "COOKIETEXT",
        "SCRIPTTEXT",
        "AAAA",
    ] {
        assert!(!md.contains(gone), "{gone} leaked: {md}");
    }
    assert!(!r.html.unwrap().contains("data:image"));
    // Links come from the original document, so nav/footer links stay.
    assert_eq!(
        r.links.unwrap(),
        [
            "https://example.com/nav",
            "https://example.com/docs/guide/start",
            "https://example.com/guide/start",
            "https://other.test/x",
        ]
    );

    let m = r.metadata;
    assert_eq!(m.title.as_deref(), Some("Acme Widgets | Home"));
    assert_eq!(m.description.as_deref(), Some("Acme makes widgets."));
    assert_eq!(m.language.as_deref(), Some("en"));
    assert_eq!(
        m.favicon.as_deref(),
        Some("https://example.com/favicon.ico")
    );
    assert_eq!(m.og_title.as_deref(), Some("Acme OG"));
    assert_eq!(m.published_time.as_deref(), Some("2024-03-01T10:00:00Z"));
    assert_eq!(m.dc_date.as_deref(), Some("2024-02-01"));
    assert_eq!(m.modified_time, None);
    assert_eq!(
        m.extra.get("twitter:card").map(String::as_str),
        Some("summary")
    );
}

#[test]
fn challenge_detection() {
    // (status, cf-mitigated header value, body, expected)
    let cases = [
        (403, Some("challenge"), "", true),
        (503, None, "<title>Just a moment...</title>", true),
        (200, None, "<title>Home</title><p>hello</p>", false),
        (403, None, "<title>Forbidden</title>", false),
    ];
    for (status, header, body, want) in cases {
        let headers: Vec<_> = header
            .map(|v| ("CF-Mitigated".to_string(), v.to_string()))
            .into_iter()
            .collect();
        assert_eq!(
            is_challenge(status, &headers, body),
            want,
            "{status} {body}"
        );
    }
}
