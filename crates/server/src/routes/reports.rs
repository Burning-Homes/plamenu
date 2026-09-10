//! `POST /api/v1/reports` — file a moderation report (Mastodon's reports API).

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::header::CONTENT_TYPE;
use plamenu_db::account;
use serde_json::Value;

use super::params::truthy;
use crate::AppState;
use crate::actions::{self, ReportParams};
use crate::auth::CurrentUser;
use crate::entities::report_json;
use crate::error::ApiError;

/// The normalized inputs of a report request, decoded from either a JSON or a
/// form-encoded body.
struct ReportInput {
    account_id: i64,
    comment: String,
    category: Option<String>,
    forward: bool,
    status_ids: Vec<i64>,
    rule_ids: Vec<i64>,
}

/// `POST /api/v1/reports`.
pub async fn create(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    // Mastodon: `doorkeeper_authorize! :write, :'write:reports'`.
    current.require_scope("write:reports")?;

    let input = parse_input(&headers, &body)?;
    // `Account.find` 404s on a missing or unknown account.
    let target = account::find_by_id(&state.pool, input.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;

    let rule_ids = (!input.rule_ids.is_empty()).then_some(input.rule_ids.as_slice());
    let report = actions::create_report(
        &state,
        &current.account,
        &target,
        ReportParams {
            comment: &input.comment,
            category: input.category.as_deref(),
            forward: input.forward,
            status_ids: &input.status_ids,
            rule_ids,
        },
    )
    .await?;
    Ok(Json(
        report_json(&state.pool, &state.config.domain, &report).await?,
    ))
}

fn parse_input(headers: &HeaderMap, body: &[u8]) -> Result<ReportInput, ApiError> {
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if content_type.starts_with("application/json") {
        parse_json(body)
    } else {
        parse_form(body)
    }
}

fn parse_json(body: &[u8]) -> Result<ReportInput, ApiError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|e| ApiError::BadRequest(format!("invalid JSON body: {e}")))?;
    let account_id = value
        .get("account_id")
        .and_then(value_to_id)
        .ok_or(ApiError::NotFound)?;
    Ok(ReportInput {
        account_id,
        comment: value
            .get("comment")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        category: value
            .get("category")
            .and_then(Value::as_str)
            .map(str::to_owned),
        forward: value.get("forward").is_some_and(value_truthy),
        status_ids: json_id_array(value.get("status_ids"))?,
        rule_ids: json_id_array(value.get("rule_ids"))?,
    })
}

fn parse_form(body: &[u8]) -> Result<ReportInput, ApiError> {
    let pairs: Vec<(String, String)> = serde_urlencoded::from_bytes(body)
        .map_err(|e| ApiError::BadRequest(format!("invalid form body: {e}")))?;
    let mut account_id = None;
    let mut comment = String::new();
    let mut category = None;
    let mut forward = false;
    let mut status_ids = Vec::new();
    let mut rule_ids = Vec::new();
    for (key, value) in pairs {
        match key.as_str() {
            "account_id" => account_id = Some(value.parse().map_err(|_| ApiError::NotFound)?),
            "comment" => comment = value,
            "category" => category = Some(value),
            "forward" => forward = truthy(Some(&value)),
            "status_ids[]" | "status_ids" => {
                status_ids.push(value.parse().map_err(|_| ApiError::NotFound)?);
            }
            "rule_ids[]" | "rule_ids" => {
                rule_ids.push(value.parse().map_err(|_| ApiError::NotFound)?);
            }
            _ => {}
        }
    }
    Ok(ReportInput {
        account_id: account_id.ok_or(ApiError::NotFound)?,
        comment,
        category,
        forward,
        status_ids,
        rule_ids,
    })
}

/// A JSON array of ids (strings or numbers). Absent is empty; an
/// unparseable id 404s like `Status.find` would.
fn json_id_array(value: Option<&Value>) -> Result<Vec<i64>, ApiError> {
    let Some(array) = value.and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    array
        .iter()
        .map(|entry| value_to_id(entry).ok_or(ApiError::NotFound))
        .collect()
}

fn value_to_id(value: &Value) -> Option<i64> {
    match value {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_i64(),
        _ => None,
    }
}

fn value_truthy(value: &Value) -> bool {
    match value {
        Value::Bool(b) => *b,
        Value::String(s) => truthy(Some(s)),
        _ => false,
    }
}
