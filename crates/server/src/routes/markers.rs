//! `/api/v1/markers` — per-timeline read positions (home, notifications).

use axum::Json;
use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::http::HeaderMap;
use axum::http::header::CONTENT_TYPE;
use plamenu_db::marker::{self, Marker};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::auth::CurrentUser;
use crate::entities::rfc3339;
use crate::error::ApiError;
use crate::state::AppState;

fn marker_json(marker: &Marker) -> Result<Value, ApiError> {
    Ok(json!({
        "last_read_id": marker.last_read_id.to_string(),
        "version": marker.version,
        "updated_at": rfc3339(marker.updated_at)?,
    }))
}

/// `GET /api/v1/markers?timeline[]=home&timeline[]=notifications` — a map
/// with one entry per requested timeline that has a marker. No `timeline`
/// param means an empty map, like Mastodon.
pub async fn index(
    State(state): State<AppState>,
    current: CurrentUser,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:statuses")?;
    let pairs: Vec<(String, String)> =
        serde_urlencoded::from_str(query.as_deref().unwrap_or(""))
            .map_err(|e| ApiError::BadRequest(format!("invalid query string: {e}")))?;
    let timelines: Vec<String> = pairs
        .into_iter()
        .filter(|(key, _)| key == "timeline[]" || key == "timeline")
        .map(|(_, value)| value)
        .collect();
    let markers = marker::list(&state.pool, current.user.id, &timelines).await?;
    let mut map = Map::new();
    for item in &markers {
        map.insert(item.timeline.clone(), marker_json(item)?);
    }
    Ok(Json(Value::Object(map)))
}

/// `last_read_id` arrives as a string from real clients but is a number in
/// some; Rails casts either (and garbage becomes 0, which we mirror).
#[derive(Deserialize)]
#[serde(untagged)]
enum LastReadId {
    Int(i64),
    Str(String),
}

impl LastReadId {
    fn cast(&self) -> i64 {
        match self {
            Self::Int(value) => *value,
            Self::Str(value) => value.parse().unwrap_or(0),
        }
    }
}

#[derive(Deserialize, Default)]
struct TimelineParams {
    last_read_id: Option<LastReadId>,
}

#[derive(Deserialize, Default)]
struct CreateParams {
    home: Option<TimelineParams>,
    notifications: Option<TimelineParams>,
}

/// The Rails-style bracketed form Mastodon also accepts:
/// `home[last_read_id]=123&notifications[last_read_id]=456`.
fn parse_form(body: &[u8]) -> Result<CreateParams, ApiError> {
    let pairs: Vec<(String, String)> = serde_urlencoded::from_bytes(body)
        .map_err(|e| ApiError::BadRequest(format!("invalid form body: {e}")))?;
    let mut params = CreateParams::default();
    for (key, value) in pairs {
        let timeline = match key.as_str() {
            "home[last_read_id]" => &mut params.home,
            "notifications[last_read_id]" => &mut params.notifications,
            _ => continue,
        };
        *timeline = Some(TimelineParams {
            last_read_id: Some(LastReadId::Str(value)),
        });
    }
    Ok(params)
}

/// `POST /api/v1/markers` — saves the markers in the body and returns them
/// in the same shape `GET` uses. Unknown timeline keys are ignored.
pub async fn create(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:statuses")?;
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let params: CreateParams = if content_type.starts_with("application/json") {
        serde_json::from_slice(&body)
            .map_err(|e| ApiError::BadRequest(format!("invalid JSON body: {e}")))?
    } else {
        parse_form(&body)?
    };
    let mut map = Map::new();
    for (timeline, timeline_params) in [
        ("home", params.home),
        ("notifications", params.notifications),
    ] {
        let Some(timeline_params) = timeline_params else {
            continue;
        };
        let last_read_id = timeline_params.last_read_id.as_ref().map(LastReadId::cast);
        let saved = marker::upsert(&state.pool, current.user.id, timeline, last_read_id).await?;
        map.insert(timeline.to_owned(), marker_json(&saved)?);
    }
    Ok(Json(Value::Object(map)))
}
