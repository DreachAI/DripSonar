//! Short browser sessions (`/v1/browser`) and screenshot downloads (`/v1/artifacts/:id`).

use axum::{
    Json,
    extract::{Path, State, rejection::BytesRejection},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::time::Instant;

use super::{ApiError, AppState, Format, Reply, SCRAPE_TIMEOUT_MS, parse, screenshot_format};
use crate::{
    browser::{Action, BrowserOutput, BrowserPlan, validate_plan},
    page::{self, RenderOptions},
    scrape,
};

/// `formats` may add `screenshot` or `screenshot@fullPage`; markdown is always returned.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenBody {
    url: String,
    timeout: Option<u64>,
    #[serde(default)]
    formats: Vec<Format>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StepBody {
    #[serde(default)]
    actions: Vec<Action>,
    timeout: Option<u64>,
    #[serde(default)]
    formats: Vec<Format>,
}

pub(super) async fn open(
    State(state): State<AppState>,
    body: Result<Bytes, BytesRejection>,
) -> Reply {
    let body: OpenBody = parse(body)?;
    let deadline = Instant::now() + state.timeout(body.timeout, SCRAPE_TIMEOUT_MS)?;
    let url = scrape::normalize(&body.url)?;
    let plan = plan(Vec::new(), &body.formats)?;
    let (id, output) = state.browser.open_session(&url, deadline, &plan).await?;
    let mut reply = view(&output, deadline).await?;
    reply.insert("id".into(), json!(id));
    Ok(Json(Value::Object(reply)))
}

pub(super) async fn step(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Result<Bytes, BytesRejection>,
) -> Reply {
    let body: StepBody = parse(body)?;
    let deadline = Instant::now() + state.timeout(body.timeout, SCRAPE_TIMEOUT_MS)?;
    let plan = plan(body.actions, &body.formats)?;
    let output = state.browser.session_step(&id, &plan, deadline).await?;
    let mut reply = view(&output, deadline).await?;
    let actions = output.extras.actions.unwrap_or_default();
    reply.insert("actions".into(), json!(actions));
    Ok(Json(Value::Object(reply)))
}

pub(super) async fn close(State(state): State<AppState>, Path(id): Path<String>) -> Reply {
    state.browser.close_session(&id)?;
    Ok(Json(json!({"success": true})))
}

/// Serves an unexpired artifact by its capability ID; no API key, as the ID is the secret.
pub(super) async fn artifact(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if id.len() != 64 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match state.store.artifact(&id).await {
        Ok(Some((media_type, body))) => {
            let headers = [
                (header::CONTENT_TYPE, media_type),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
                (header::CACHE_CONTROL, "private, max-age=86400".to_owned()),
            ];
            (headers, body).into_response()
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => {
            tracing::error!(%error, "artifact unreadable");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn plan(actions: Vec<Action>, formats: &[Format]) -> Result<BrowserPlan, ApiError> {
    validate_plan(actions, 0, screenshot_format(formats)?)
        .map_err(|error| ApiError::bad_request(error.to_string()))
}

/// The page as it stands: the whole page as markdown, navigation included, so an agent can
/// see what to click or type next.
async fn view(output: &BrowserOutput, deadline: Instant) -> Result<Map<String, Value>, ApiError> {
    let capture = &output.capture;
    let options = RenderOptions {
        markdown: true,
        remove_base64_images: true,
        ..RenderOptions::default()
    };
    let (text, url) = (capture.text(), capture.final_url.clone());
    let rendered =
        scrape::render_blocking(deadline, move || Ok(page::render(&text, &url, &options))).await?;
    let mut view = Map::new();
    view.insert("success".into(), json!(true));
    view.insert("url".into(), json!(capture.final_url.as_str()));
    view.insert(
        "markdown".into(),
        json!(rendered.markdown.unwrap_or_default()),
    );
    if let Some(screenshot) = &output.extras.screenshot {
        view.insert("screenshot".into(), json!(screenshot));
    }
    Ok(view)
}
