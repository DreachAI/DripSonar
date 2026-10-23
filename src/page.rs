//! Pure HTML transforms: cleanup, main content, markdown, metadata, links.

use std::collections::BTreeMap;

#[derive(Clone, Debug, Default)]
pub struct RenderOptions {
    pub only_main_content: bool,
    pub include_tags: Vec<String>,
    pub exclude_tags: Vec<String>,
    pub remove_base64_images: bool,
    pub markdown: bool,
    pub html: bool,
    pub links: bool,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PageMetadata {
    pub title: Option<String>,
    pub description: Option<String>,
    pub language: Option<String>,
    pub keywords: Option<String>,
    pub robots: Option<String>,
    pub favicon: Option<String>,
    pub og_title: Option<String>,
    pub og_description: Option<String>,
    pub og_url: Option<String>,
    pub og_image: Option<String>,
    pub og_site_name: Option<String>,
    pub og_locale: Option<String>,
    pub published_time: Option<String>,
    pub modified_time: Option<String>,
    pub dc_date: Option<String>,
    pub dc_date_created: Option<String>,
    pub dc_terms_created: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default)]
pub struct Rendered {
    pub markdown: Option<String>,
    pub html: Option<String>,
    pub links: Option<Vec<String>>,
    pub metadata: PageMetadata,
}

/// Stub: returns an empty rendering until the page module is written.
pub fn render(_html: &str, _base_url: &url::Url, _opts: &RenderOptions) -> Rendered {
    Rendered::default()
}

/// Stub: reports no challenge until the page module is written.
pub fn is_challenge(_status: u16, _headers: &[(String, String)], _html: &str) -> bool {
    false
}
