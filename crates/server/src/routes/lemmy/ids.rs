//! Lemmy's 32-bit ID boundary.
//!
//! Handlers and domain services continue to work with native snowflakes. The
//! response middleware translates all entity IDs in one batched pass per kind;
//! request handlers use [`resolve_required`] before calling native services.

use std::collections::{HashMap, HashSet};

use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use plamenu_db::lemmy_id::Kind;
use serde_json::{Map, Value, json};

use crate::AppState;

use super::error::LemmyError;

const MAX_COMPAT_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

/// Translate a client-supplied Lemmy ID into the native entity ID expected by
/// Plamenu's services.
pub async fn resolve_required(
    state: &AppState,
    kind: Kind,
    lemmy_id: i32,
    not_found: &'static str,
) -> Result<i64, LemmyError> {
    plamenu_db::lemmy_id::resolve(&state.pool, kind, lemmy_id)
        .await?
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, not_found))
}

/// Resolve a bounded request batch while preserving its order and duplicates.
/// Any missing or wrong-kind alias rejects the whole mutation atomically.
pub async fn resolve_required_many(
    state: &AppState,
    kind: Kind,
    lemmy_ids: &[i32],
    not_found: &'static str,
) -> Result<Vec<i64>, LemmyError> {
    let resolved = plamenu_db::lemmy_id::resolve_many(&state.pool, kind, lemmy_ids).await?;
    lemmy_ids
        .iter()
        .map(|id| {
            resolved
                .get(id)
                .copied()
                .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, not_found))
        })
        .collect()
}

/// Project native IDs in a successful `/api/v3` JSON response into Lemmy's
/// durable `i32` namespace.
///
/// Translation is deliberately response-wide: nested views reuse one alias
/// allocation/read per entity kind, independent of page size. Error responses
/// carry symbolic strings only and pass through untouched.
pub async fn translate_response(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let response = next.run(request).await;
    if !response.status().is_success() {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let bytes = match to_bytes(body, MAX_COMPAT_RESPONSE_BYTES).await {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::error!(%error, "could not buffer Lemmy compatibility response");
            return internal_error();
        }
    };
    let mut value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return Response::from_parts(parts, Body::from(bytes)),
    };
    if let Err(error) = translate_value(&state, &mut value).await {
        tracing::error!(%error, "could not allocate Lemmy compatibility IDs");
        return internal_error();
    }
    let bytes = match serde_json::to_vec(&value) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::error!(%error, "could not serialize Lemmy compatibility response");
            return internal_error();
        }
    };
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, Body::from(bytes))
}

fn internal_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(json!({ "error": "unknown" })),
    )
        .into_response()
}

async fn translate_value(state: &AppState, value: &mut Value) -> Result<(), plamenu_db::DbError> {
    let mut ids: HashMap<Kind, HashSet<i64>> = HashMap::new();
    collect_ids(value, None, &mut ids);
    let mut aliases: HashMap<Kind, HashMap<i64, i32>> = HashMap::new();
    for (kind, native) in ids {
        let native = native.into_iter().collect::<Vec<_>>();
        aliases.insert(
            kind,
            plamenu_db::lemmy_id::aliases_for(&state.pool, kind, &native).await?,
        );
    }
    replace_ids(value, None, &aliases);
    Ok(())
}

fn collect_ids(value: &Value, parent_key: Option<&str>, ids: &mut HashMap<Kind, HashSet<i64>>) {
    match value {
        Value::Object(object) => {
            let object_kind = object_kind(object, parent_key);
            for (key, child) in object {
                if key == "path" {
                    collect_path(child, ids);
                } else if key == "id" {
                    if let Some(kind) = object_kind {
                        collect_number(child, kind, ids);
                    }
                } else if let Some(kind) = reference_kind(key) {
                    collect_number(child, kind, ids);
                }
                collect_ids(child, Some(key), ids);
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_ids(child, parent_key, ids);
            }
        }
        _ => {}
    }
}

fn collect_number(value: &Value, kind: Kind, ids: &mut HashMap<Kind, HashSet<i64>>) {
    if let Some(id) = value.as_i64()
        && id > 0
    {
        ids.entry(kind).or_default().insert(id);
    }
}

fn collect_path(value: &Value, ids: &mut HashMap<Kind, HashSet<i64>>) {
    let Some(path) = value.as_str() else { return };
    for segment in path.split('.') {
        if let Ok(id) = segment.parse::<i64>()
            && id > 0
        {
            ids.entry(Kind::Status).or_default().insert(id);
        }
    }
}

fn replace_ids(
    value: &mut Value,
    parent_key: Option<&str>,
    aliases: &HashMap<Kind, HashMap<i64, i32>>,
) {
    match value {
        Value::Object(object) => {
            let object_kind = object_kind(object, parent_key);
            for (key, child) in object {
                if key == "path" {
                    replace_path(child, aliases);
                } else if key == "id" {
                    if let Some(kind) = object_kind {
                        replace_number(child, kind, aliases);
                    }
                } else if let Some(kind) = reference_kind(key) {
                    replace_number(child, kind, aliases);
                }
                replace_ids(child, Some(key), aliases);
            }
        }
        Value::Array(values) => {
            for child in values {
                replace_ids(child, parent_key, aliases);
            }
        }
        _ => {}
    }
}

fn replace_number(value: &mut Value, kind: Kind, aliases: &HashMap<Kind, HashMap<i64, i32>>) {
    let Some(native) = value.as_i64() else {
        return;
    };
    let Some(alias) = aliases.get(&kind).and_then(|values| values.get(&native)) else {
        return;
    };
    *value = Value::from(*alias);
}

fn replace_path(value: &mut Value, aliases: &HashMap<Kind, HashMap<i64, i32>>) {
    let Some(path) = value.as_str() else { return };
    let Some(statuses) = aliases.get(&Kind::Status) else {
        return;
    };
    let translated = path
        .split('.')
        .map(|segment| {
            segment
                .parse::<i64>()
                .ok()
                .and_then(|id| statuses.get(&id))
                .map_or_else(|| segment.to_owned(), ToString::to_string)
        })
        .collect::<Vec<_>>()
        .join(".");
    *value = Value::String(translated);
}

fn reference_kind(key: &str) -> Option<Kind> {
    match key {
        "person_id" | "creator_id" | "community_id" | "moderator_id" | "resolver_id"
        | "admin_id" | "banned_person_id" => Some(Kind::Account),
        "post_id" | "comment_id" | "parent_id" => Some(Kind::Status),
        "local_user_id" => Some(Kind::User),
        "report_id" => Some(Kind::Report),
        "comment_reply_id" | "person_mention_id" => Some(Kind::Notification),
        "private_message_id" => Some(Kind::PrivateMessage),
        _ => None,
    }
}

fn object_kind(object: &Map<String, Value>, parent_key: Option<&str>) -> Option<Kind> {
    match parent_key {
        Some("person" | "creator" | "moderator" | "post_creator" | "comment_creator") => {
            return Some(Kind::Account);
        }
        Some("community") => return Some(Kind::Account),
        Some("post" | "comment") => return Some(Kind::Status),
        Some("local_user" | "creator_local_user") => return Some(Kind::User),
        Some("post_report" | "comment_report") => return Some(Kind::Report),
        Some("registration_application") => return Some(Kind::Registration),
        Some("comment_reply" | "person_mention") => return Some(Kind::Notification),
        Some("private_message") => return Some(Kind::PrivateMessage),
        _ => {}
    }
    if object.contains_key("actor_id") && object.contains_key("instance_id") {
        Some(Kind::Account)
    } else if object.contains_key("ap_id")
        && (object.contains_key("community_id") || object.contains_key("post_id"))
    {
        Some(Kind::Status)
    } else if object.contains_key("shortcode") && object.contains_key("image_url") {
        Some(Kind::CustomEmoji)
    } else if object.contains_key("answer") && object.contains_key("local_user_id") {
        Some(Kind::Registration)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{collect_ids, object_kind, reference_kind, replace_ids};
    use plamenu_db::lemmy_id::Kind;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn classifies_nested_lemmy_entities_without_touching_catalog_ids() {
        let value = json!({
            "post": { "id": 99, "creator_id": 88, "community_id": 77, "ap_id": "https://x/p/99", "language_id": 1 },
            "creator": { "id": 88, "actor_id": "https://x/u/a", "instance_id": 2 },
            "counts": { "post_id": 99 },
            "all_languages": [{ "id": 1 }],
            "site_view": { "site": { "id": 1, "instance_id": 1 } },
        });
        let mut ids = HashMap::new();
        collect_ids(&value, None, &mut ids);
        assert_eq!(ids[&Kind::Status].len(), 1);
        assert!(ids[&Kind::Status].contains(&99));
        assert_eq!(ids[&Kind::Account].len(), 2);
        assert!(ids[&Kind::Account].contains(&88));
        assert!(ids[&Kind::Account].contains(&77));
        assert!(!ids.values().any(|values| values.contains(&1)));
        assert_eq!(reference_kind("language_id"), None);
        assert_eq!(object_kind(value.as_object().unwrap(), None), None);
    }

    #[test]
    fn comment_paths_are_status_ids() {
        let mut value = json!({ "comment": { "id": 30, "post_id": 10, "creator_id": 20, "ap_id": "https://x/c/30", "path": "0.10.25.30" } });
        let mut ids = HashMap::new();
        collect_ids(&value, None, &mut ids);
        assert_eq!(ids[&Kind::Status].len(), 3);
        assert!(ids[&Kind::Status].contains(&10));
        assert!(ids[&Kind::Status].contains(&25));
        assert!(ids[&Kind::Status].contains(&30));

        let aliases = HashMap::from([
            (Kind::Status, HashMap::from([(10, 1), (25, 2), (30, 3)])),
            (Kind::Account, HashMap::from([(20, 4)])),
        ]);
        replace_ids(&mut value, None, &aliases);
        assert_eq!(value["comment"]["id"], 3);
        assert_eq!(value["comment"]["post_id"], 1);
        assert_eq!(value["comment"]["creator_id"], 4);
        assert_eq!(value["comment"]["path"], "0.1.2.3");
    }
}
