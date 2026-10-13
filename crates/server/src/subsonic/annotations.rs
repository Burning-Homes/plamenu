//! Personal star dates and counts are batched over already-authorized songs.
use crate::{AppState, error::ApiError};
use plamenu_db::DbError;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
fn ids(value: &Value, result: &mut HashSet<i64>) {
    match value {
        Value::Object(fields) => {
            if fields.get("isDir") == Some(&Value::Bool(false))
                && let Some(id) = fields
                    .get("id")
                    .and_then(Value::as_str)
                    .and_then(|id| id.parse::<i64>().ok())
            {
                result.insert(id);
            }
            for child in fields.values() {
                ids(child, result);
            }
        }
        Value::Array(items) => {
            for item in items {
                ids(item, result);
            }
        }
        _ => {}
    }
}
fn apply(value: &mut Value, metadata: &HashMap<i64, Value>) {
    match value {
        Value::Object(fields) => {
            if fields.get("isDir") == Some(&Value::Bool(false))
                && let Some(id) = fields
                    .get("id")
                    .and_then(Value::as_str)
                    .and_then(|id| id.parse::<i64>().ok())
                && let Some(Value::Object(extra)) = metadata.get(&id)
            {
                fields.extend(extra.clone());
            }
            for child in fields.values_mut() {
                apply(child, metadata);
            }
        }
        Value::Array(items) => {
            for item in items {
                apply(item, metadata);
            }
        }
        _ => {}
    }
}
pub async fn songs(state: &AppState, owner: i64, mut value: Value) -> Result<Value, ApiError> {
    let mut targets = HashSet::new();
    ids(&value, &mut targets);
    if targets.is_empty() {
        return Ok(value);
    }
    let targets = targets.into_iter().collect::<Vec<_>>();
    let mut metadata = HashMap::new();
    for page in targets.chunks(100) {
        let rows:Vec<(i64,time::OffsetDateTime,Option<time::OffsetDateTime>,i64)>=sqlx::query_as("SELECT t.id,t.created_at,f.created_at,(SELECT count(*) FROM music_listen_sessions s WHERE s.account_id=$2 AND s.asset_id=t.id AND s.counted_at IS NOT NULL) FROM music_assets t LEFT JOIN music_favourites f ON f.account_id=$2 AND f.asset_id=t.id WHERE t.id=ANY($1)")
            .bind(page).bind(owner).fetch_all(&state.pool).await.map_err(DbError::from)?;
        for (id, created, starred, count) in rows {
            let mut extra = json!({"created":crate::entities::rfc3339(created)?,"playCount":count});
            if let Some(starred) = starred {
                extra["starred"] = json!(crate::entities::rfc3339(starred)?);
            }
            metadata.insert(id, extra);
        }
    }
    apply(&mut value, &metadata);
    Ok(value)
}
