//! `/api/v1/filters` (deprecated) and `/api/v2/filters/*` — content filters,
//! Mastodon's model: a per-account filter with an action (warn/hide/blur) and
//! the contexts it applies in, matching keyword phrases or specific statuses.
//! v2 is the filter-with-rules model; v1 addresses a single keyword at a time.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::http::header::CONTENT_TYPE;
use plamenu_db::custom_filter::{
    self, ACTIONS, CustomFilter, CustomFilterKeyword, CustomFilterStatus, KEYWORD_LENGTH_LIMIT,
    KeywordChange, MAX_FILTERS_PER_ACCOUNT, MAX_KEYWORDS_PER_FILTER, NewKeyword,
    TITLE_LENGTH_LIMIT, VALID_CONTEXTS,
};
use plamenu_db::status;
use serde_json::{Map, Value, json};
use time::{Duration, OffsetDateTime};

use crate::auth::CurrentUser;
use crate::entities::{can_view, rfc3339};
use crate::error::ApiError;
use crate::state::AppState;

// --- Body parsing ----------------------------------------------------------

/// One segment of a Rails-bracketed form key.
enum Seg {
    Key(String),
    /// An empty `[]` — append to (or build) an array.
    Push,
}

fn tokenize(key: &str) -> Vec<Seg> {
    let Some(open) = key.find('[') else {
        return vec![Seg::Key(key.to_owned())];
    };
    let mut segs = vec![Seg::Key(key[..open].to_owned())];
    let mut rest = &key[open..];
    while let Some(stripped) = rest.strip_prefix('[') {
        let Some(close) = stripped.find(']') else {
            break;
        };
        let inner = &stripped[..close];
        segs.push(if inner.is_empty() {
            Seg::Push
        } else {
            Seg::Key(inner.to_owned())
        });
        rest = &stripped[close + 1..];
    }
    segs
}

/// Places a value into the growing tree at `segs` — Rack's nested-query rules
/// for the shapes the filter API uses: `a[]` scalar arrays, `a[k]` hashes, and
/// `a[][k]` arrays of hashes (a repeated key starts a new element).
fn place(slot: &mut Value, segs: &[Seg], val: String) {
    match segs.first() {
        None => *slot = Value::String(val),
        Some(Seg::Key(key)) => {
            if !slot.is_object() {
                *slot = Value::Object(Map::new());
            }
            let child = slot
                .as_object_mut()
                .unwrap()
                .entry(key.clone())
                .or_insert(Value::Null);
            place(child, &segs[1..], val);
        }
        Some(Seg::Push) => {
            if !slot.is_array() {
                *slot = Value::Array(Vec::new());
            }
            let rest = &segs[1..];
            let arr = slot.as_array_mut().unwrap();
            if rest.is_empty() {
                arr.push(Value::String(val));
                return;
            }
            let next_key = match &rest[0] {
                Seg::Key(k) => Some(k.as_str()),
                Seg::Push => None,
            };
            let start_new = match (next_key, arr.last()) {
                (Some(k), Some(Value::Object(m))) => m.contains_key(k),
                _ => true,
            };
            if start_new {
                arr.push(Value::Object(Map::new()));
            }
            place(arr.last_mut().unwrap(), rest, val);
        }
    }
}

/// The request body as a JSON value: native JSON, or a Rails-bracketed form
/// folded into one (`context[]`, `keywords_attributes[][keyword]`, …).
fn body_value(headers: &HeaderMap, body: &[u8]) -> Result<Value, ApiError> {
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if content_type.starts_with("application/json") {
        return serde_json::from_slice(body)
            .map_err(|e| ApiError::BadRequest(format!("invalid JSON body: {e}")));
    }
    if body.is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let pairs: Vec<(String, String)> = serde_urlencoded::from_bytes(body)
        .map_err(|e| ApiError::BadRequest(format!("invalid form body: {e}")))?;
    let mut root = Value::Object(Map::new());
    for (key, val) in pairs {
        place(&mut root, &tokenize(&key), val);
    }
    Ok(root)
}

fn str_param(value: &Value, key: &str) -> Option<String> {
    match value.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::Bool(b)) => Some(b.to_string()),
        _ => None,
    }
}

fn array_param(value: &Value, key: &str) -> Option<Vec<String>> {
    match value.get(key) {
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .filter_map(|item| match item {
                    Value::String(s) => Some(s.clone()),
                    Value::Number(n) => Some(n.to_string()),
                    _ => None,
                })
                .collect(),
        ),
        _ => None,
    }
}

/// Rails' `ActiveModel::Type::Boolean` cast over a JSON value (absent = false).
fn cast_bool(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => super::params::truthy(Some(s)),
        _ => false,
    }
}

fn whole_word_param(entry: &Value) -> bool {
    entry.get("whole_word").is_none_or(|v| cast_bool(Some(v)))
}

/// Mastodon's `Expireable#expires_in=`: absent keeps the value, blank clears
/// it, a number is seconds from now.
enum ExpiresIn {
    Unset,
    Clear,
    In(i64),
}

fn parse_expires_in(value: &Value) -> ExpiresIn {
    match value.get("expires_in") {
        Some(Value::String(s)) if s.trim().is_empty() => ExpiresIn::Clear,
        Some(Value::String(s)) => ExpiresIn::In(s.trim().parse().unwrap_or(0)),
        Some(Value::Number(n)) => ExpiresIn::In(n.as_i64().unwrap_or(0)),
        None | Some(_) => ExpiresIn::Unset,
    }
}

fn expires_at_on_create(expires_in: &ExpiresIn) -> Option<OffsetDateTime> {
    match expires_in {
        ExpiresIn::In(seconds) => Some(OffsetDateTime::now_utc() + Duration::seconds(*seconds)),
        ExpiresIn::Unset | ExpiresIn::Clear => None,
    }
}

fn expires_at_on_update(
    expires_in: &ExpiresIn,
    existing: Option<OffsetDateTime>,
) -> Option<OffsetDateTime> {
    match expires_in {
        ExpiresIn::Unset => existing,
        ExpiresIn::Clear => None,
        ExpiresIn::In(seconds) => Some(OffsetDateTime::now_utc() + Duration::seconds(*seconds)),
    }
}

// --- Serializers -----------------------------------------------------------

fn keyword_json(keyword: &CustomFilterKeyword) -> Value {
    json!({
        "id": keyword.id.to_string(),
        "keyword": keyword.keyword,
        "whole_word": keyword.whole_word,
    })
}

fn status_entry_json(entry: &CustomFilterStatus) -> Value {
    json!({
        "id": entry.id.to_string(),
        "status_id": entry.status_id.to_string(),
    })
}

/// The full `Filter` entity (`FilterSerializer` with `rules_requested`).
fn filter_json(
    filter: &CustomFilter,
    keywords: &[CustomFilterKeyword],
    statuses: &[CustomFilterStatus],
) -> Result<Value, ApiError> {
    let mut entity = crate::filters::filter_summary_json(filter)?;
    entity["keywords"] = Value::Array(keywords.iter().map(keyword_json).collect());
    entity["statuses"] = Value::Array(statuses.iter().map(status_entry_json).collect());
    Ok(entity)
}

/// The deprecated v1 `Filter` entity — one keyword presented with its filter's
/// context/expiry/irreversibility.
fn v1_filter_json(keyword: &CustomFilterKeyword, filter: &CustomFilter) -> Result<Value, ApiError> {
    let expires_at = match filter.expires_at {
        Some(at) => Some(rfc3339(at)?),
        None => None,
    };
    Ok(json!({
        "id": keyword.id.to_string(),
        "phrase": keyword.keyword,
        "context": filter.context,
        "whole_word": keyword.whole_word,
        "expires_at": expires_at,
        "irreversible": filter.action == "hide",
    }))
}

async fn serialize_filter(state: &AppState, filter: &CustomFilter) -> Result<Value, ApiError> {
    let keywords = custom_filter::keywords_for(&state.pool, filter.id).await?;
    let statuses = custom_filter::statuses_for(&state.pool, filter.id).await?;
    filter_json(filter, &keywords, &statuses)
}

// --- Validation ------------------------------------------------------------

fn invalid(messages: &[String]) -> Result<(), ApiError> {
    if messages.is_empty() {
        Ok(())
    } else {
        Err(ApiError::Unprocessable(format!(
            "Validation failed: {}",
            messages.join(", ")
        )))
    }
}

/// The `CustomFilter` validations, collected in Rails' declaration order.
fn validate_filter(
    title: &str,
    action: &str,
    action_given: bool,
    context: &[String],
) -> Result<(), ApiError> {
    let mut errors = Vec::new();
    if action_given && !ACTIONS.contains(&action) {
        errors.push("Action is not included in the list".to_owned());
    }
    if title.trim().is_empty() {
        errors.push("Title can't be blank".to_owned());
    }
    if context.is_empty() {
        errors.push("Context can't be blank".to_owned());
    }
    if title.chars().count() > TITLE_LENGTH_LIMIT {
        errors.push(format!(
            "Title is too long (maximum is {TITLE_LENGTH_LIMIT} characters)"
        ));
    }
    if context.is_empty()
        || context
            .iter()
            .any(|value| !VALID_CONTEXTS.contains(&value.as_str()))
    {
        errors.push("Context None or invalid context supplied".to_owned());
    }
    invalid(&errors)
}

fn validate_keyword(keyword: &str) -> Result<(), ApiError> {
    let mut errors = Vec::new();
    if keyword.trim().is_empty() {
        errors.push("Keyword can't be blank".to_owned());
    }
    if keyword.chars().count() > KEYWORD_LENGTH_LIMIT {
        errors.push(format!(
            "Keyword is too long (maximum is {KEYWORD_LENGTH_LIMIT} characters)"
        ));
    }
    invalid(&errors)
}

// --- Cardinality guards (audit #58) ----------------------------------------
// Custom-filter counts were uncapped: one body could persist unbounded keyword
// rows, and an account could accumulate unbounded filters. Because every
// signed-in status render loads and recompiles the whole set, that is a
// self-inflicted DoS as well as storage abuse. These bounds are far above
// honest use but keep both the durable rows and the per-render compile cost
// finite. (Caching the compiled representation across renders remains #58's
// deferred second half.)

/// Refuses a new filter once the account already owns [`MAX_FILTERS_PER_ACCOUNT`].
fn within_filter_limit(existing: i64) -> Result<(), ApiError> {
    let existing = usize::try_from(existing).unwrap_or(usize::MAX);
    if existing >= MAX_FILTERS_PER_ACCOUNT {
        return Err(ApiError::Unprocessable(format!(
            "Validation failed: You have reached the limit of {MAX_FILTERS_PER_ACCOUNT} filters"
        )));
    }
    Ok(())
}

/// Refuses a keyword set that would leave a filter with more than
/// [`MAX_KEYWORDS_PER_FILTER`] keywords (`total` is the resulting count).
fn within_keyword_limit(total: usize) -> Result<(), ApiError> {
    if total > MAX_KEYWORDS_PER_FILTER {
        return Err(ApiError::Unprocessable(format!(
            "Validation failed: Keywords are too many (maximum is {MAX_KEYWORDS_PER_FILTER} per filter)"
        )));
    }
    Ok(())
}

// --- Nested keyword attributes ---------------------------------------------

/// `keywords_attributes` as a list of entries — a JSON array, or a Rails
/// hash keyed by index (`keywords_attributes[0][...]`).
fn keyword_attribute_entries(value: &Value) -> Vec<Value> {
    match value.get("keywords_attributes") {
        Some(Value::Array(entries)) => entries.clone(),
        Some(Value::Object(map)) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by_key(|(key, _)| key.parse::<i64>().unwrap_or(i64::MAX));
            entries.into_iter().map(|(_, v)| v.clone()).collect()
        }
        _ => Vec::new(),
    }
}

/// The new keywords of a create. Entirely blank entries are dropped, like
/// Mastodon's `reject_if: :all_blank`.
fn new_keywords(value: &Value) -> Result<Vec<NewKeyword>, ApiError> {
    let mut keywords = Vec::new();
    for entry in keyword_attribute_entries(value) {
        if cast_bool(entry.get("_destroy")) {
            continue;
        }
        let keyword = str_param(&entry, "keyword").unwrap_or_default();
        if keyword.trim().is_empty() {
            continue;
        }
        validate_keyword(&keyword)?;
        keywords.push(NewKeyword {
            keyword,
            whole_word: whole_word_param(&entry),
        });
    }
    Ok(keywords)
}

/// The keyword changes of an update: create (no id), update (id) or destroy
/// (id + `_destroy`).
fn keyword_changes(value: &Value) -> Result<Vec<KeywordChange>, ApiError> {
    let mut changes = Vec::new();
    for entry in keyword_attribute_entries(value) {
        let id = str_param(&entry, "id").and_then(|s| s.parse::<i64>().ok());
        let destroy = cast_bool(entry.get("_destroy"));
        match (id, destroy) {
            (Some(id), true) => changes.push(KeywordChange::Destroy { id }),
            (Some(id), false) => {
                let keyword = str_param(&entry, "keyword");
                if let Some(text) = &keyword {
                    validate_keyword(text)?;
                }
                let whole_word = entry.get("whole_word").map(|v| cast_bool(Some(v)));
                changes.push(KeywordChange::Update {
                    id,
                    keyword,
                    whole_word,
                });
            }
            (None, _) => {
                let keyword = str_param(&entry, "keyword").unwrap_or_default();
                if keyword.trim().is_empty() {
                    continue;
                }
                validate_keyword(&keyword)?;
                changes.push(KeywordChange::Create {
                    keyword,
                    whole_word: whole_word_param(&entry),
                });
            }
        }
    }
    Ok(changes)
}

async fn owned_filter(
    state: &AppState,
    account_id: i64,
    filter_id: i64,
) -> Result<CustomFilter, ApiError> {
    custom_filter::find_owned(&state.pool, account_id, filter_id)
        .await?
        .ok_or(ApiError::NotFound)
}

// --- v2 filters ------------------------------------------------------------

/// `GET /api/v2/filters`.
pub async fn index_v2(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:filters")?;
    let filters = custom_filter::owned_by(&state.pool, current.account.id).await?;
    let mut entities = Vec::with_capacity(filters.len());
    for filter in &filters {
        entities.push(serialize_filter(&state, filter).await?);
    }
    Ok(Json(Value::Array(entities)))
}

/// `POST /api/v2/filters`.
pub async fn create_v2(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:filters")?;
    let value = body_value(&headers, &body)?;
    let title = str_param(&value, "title").unwrap_or_default();
    let context = array_param(&value, "context").unwrap_or_default();
    let action_param = str_param(&value, "filter_action");
    let action = action_param.clone().unwrap_or_else(|| "warn".to_owned());
    validate_filter(&title, &action, action_param.is_some(), &context)?;
    within_filter_limit(custom_filter::count_owned(&state.pool, current.account.id).await?)?;
    let keywords = new_keywords(&value)?;
    within_keyword_limit(keywords.len())?;
    let expires_at = expires_at_on_create(&parse_expires_in(&value));
    let filter = custom_filter::create(
        &state.pool,
        current.account.id,
        &title,
        &action,
        &context,
        expires_at,
        &keywords,
    )
    .await?;
    crate::filters::invalidate(current.account.id);
    Ok(Json(serialize_filter(&state, &filter).await?))
}

/// `GET /api/v2/filters/{id}`.
pub async fn show_v2(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(filter_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:filters")?;
    let filter = owned_filter(&state, current.account.id, filter_id).await?;
    Ok(Json(serialize_filter(&state, &filter).await?))
}

/// `PUT /api/v2/filters/{id}` — absent attributes keep their value.
pub async fn update_v2(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(filter_id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:filters")?;
    let existing = owned_filter(&state, current.account.id, filter_id).await?;
    let value = body_value(&headers, &body)?;
    let title = str_param(&value, "title").unwrap_or(existing.title);
    let context = array_param(&value, "context").unwrap_or(existing.context);
    let action_param = str_param(&value, "filter_action");
    let action = action_param.clone().unwrap_or(existing.action);
    validate_filter(&title, &action, action_param.is_some(), &context)?;
    let changes = keyword_changes(&value)?;
    let added = changes
        .iter()
        .filter(|c| matches!(c, KeywordChange::Create { .. }))
        .count();
    if added > 0 {
        let current = custom_filter::count_keywords(&state.pool, filter_id).await?;
        let current = usize::try_from(current).unwrap_or(usize::MAX);
        within_keyword_limit(current.saturating_add(added))?;
    }
    let expires_at = expires_at_on_update(&parse_expires_in(&value), existing.expires_at);
    let filter = custom_filter::update(
        &state.pool,
        filter_id,
        &title,
        &action,
        &context,
        expires_at,
        &changes,
    )
    .await?;
    crate::filters::invalidate(current.account.id);
    Ok(Json(serialize_filter(&state, &filter).await?))
}

/// `DELETE /api/v2/filters/{id}`.
pub async fn destroy_v2(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(filter_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:filters")?;
    if !custom_filter::delete(&state.pool, current.account.id, filter_id).await? {
        return Err(ApiError::NotFound);
    }
    crate::filters::invalidate(current.account.id);
    Ok(Json(json!({})))
}

// --- v2 keywords -----------------------------------------------------------

/// `GET /api/v2/filters/{filter_id}/keywords`.
pub async fn keywords_index(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(filter_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:filters")?;
    owned_filter(&state, current.account.id, filter_id).await?;
    let keywords = custom_filter::keywords_for(&state.pool, filter_id).await?;
    Ok(Json(Value::Array(
        keywords.iter().map(keyword_json).collect(),
    )))
}

/// `POST /api/v2/filters/{filter_id}/keywords`.
pub async fn keyword_create(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(filter_id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:filters")?;
    owned_filter(&state, current.account.id, filter_id).await?;
    let value = body_value(&headers, &body)?;
    let keyword = str_param(&value, "keyword").unwrap_or_default();
    validate_keyword(&keyword)?;
    let current_count = custom_filter::count_keywords(&state.pool, filter_id).await?;
    let current_count = usize::try_from(current_count).unwrap_or(usize::MAX);
    within_keyword_limit(current_count.saturating_add(1))?;
    let created =
        custom_filter::create_keyword(&state.pool, filter_id, &keyword, whole_word_param(&value))
            .await?;
    crate::filters::invalidate(current.account.id);
    Ok(Json(keyword_json(&created)))
}

async fn owned_keyword(
    state: &AppState,
    account_id: i64,
    keyword_id: i64,
) -> Result<CustomFilterKeyword, ApiError> {
    custom_filter::find_owned_keyword(&state.pool, account_id, keyword_id)
        .await?
        .ok_or(ApiError::NotFound)
}

/// `GET /api/v2/filters/keywords/{id}`.
pub async fn keyword_show(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(keyword_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:filters")?;
    let keyword = owned_keyword(&state, current.account.id, keyword_id).await?;
    Ok(Json(keyword_json(&keyword)))
}

/// `PUT /api/v2/filters/keywords/{id}`.
pub async fn keyword_update(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(keyword_id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:filters")?;
    let existing = owned_keyword(&state, current.account.id, keyword_id).await?;
    let value = body_value(&headers, &body)?;
    let keyword = str_param(&value, "keyword").unwrap_or(existing.keyword);
    let whole_word = value
        .get("whole_word")
        .map_or(existing.whole_word, |v| cast_bool(Some(v)));
    validate_keyword(&keyword)?;
    let updated =
        custom_filter::update_keyword(&state.pool, keyword_id, &keyword, whole_word).await?;
    crate::filters::invalidate(current.account.id);
    Ok(Json(keyword_json(&updated)))
}

/// `DELETE /api/v2/filters/keywords/{id}`.
pub async fn keyword_destroy(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(keyword_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:filters")?;
    owned_keyword(&state, current.account.id, keyword_id).await?;
    custom_filter::delete_keyword(&state.pool, keyword_id).await?;
    crate::filters::invalidate(current.account.id);
    Ok(Json(json!({})))
}

// --- v2 statuses -----------------------------------------------------------

/// `GET /api/v2/filters/{filter_id}/statuses`.
pub async fn statuses_index(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(filter_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:filters")?;
    owned_filter(&state, current.account.id, filter_id).await?;
    let entries = custom_filter::statuses_for(&state.pool, filter_id).await?;
    Ok(Json(Value::Array(
        entries.iter().map(status_entry_json).collect(),
    )))
}

/// `POST /api/v2/filters/{filter_id}/statuses`.
pub async fn status_create(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(filter_id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:filters")?;
    owned_filter(&state, current.account.id, filter_id).await?;
    let value = body_value(&headers, &body)?;
    let status_id = str_param(&value, "status_id").and_then(|s| s.parse::<i64>().ok());
    // `belongs_to :status` (required), then the StatusPolicy `show?` check.
    let Some(status_id) = status_id else {
        return Err(ApiError::Unprocessable(
            "Validation failed: Status must exist".into(),
        ));
    };
    let target = status::find_by_id(&state.pool, status_id).await?;
    let Some(target) = target else {
        return Err(ApiError::Unprocessable(
            "Validation failed: Status must exist".into(),
        ));
    };
    if !can_view(&state.pool, &target, Some(current.account.id)).await? {
        return Err(ApiError::Unprocessable(
            "Validation failed: Status is invalid".into(),
        ));
    }
    match custom_filter::create_status(&state.pool, filter_id, status_id).await? {
        Some(entry) => {
            crate::filters::invalidate(current.account.id);
            Ok(Json(status_entry_json(&entry)))
        }
        None => Err(ApiError::Unprocessable(
            "Validation failed: Status has already been taken".into(),
        )),
    }
}

async fn owned_status_entry(
    state: &AppState,
    account_id: i64,
    entry_id: i64,
) -> Result<CustomFilterStatus, ApiError> {
    custom_filter::find_owned_status(&state.pool, account_id, entry_id)
        .await?
        .ok_or(ApiError::NotFound)
}

/// `GET /api/v2/filters/statuses/{id}`.
pub async fn status_show(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(entry_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:filters")?;
    let entry = owned_status_entry(&state, current.account.id, entry_id).await?;
    Ok(Json(status_entry_json(&entry)))
}

/// `DELETE /api/v2/filters/statuses/{id}`.
pub async fn status_destroy(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(entry_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:filters")?;
    let entry = owned_status_entry(&state, current.account.id, entry_id).await?;
    custom_filter::delete_status(&state.pool, entry.id).await?;
    crate::filters::invalidate(current.account.id);
    Ok(Json(json!({})))
}

// --- v1 filters (deprecated) -----------------------------------------------

/// The filter that owns a keyword, re-checked against `account_id`.
async fn keyword_filter(
    state: &AppState,
    account_id: i64,
    keyword: &CustomFilterKeyword,
) -> Result<CustomFilter, ApiError> {
    custom_filter::find_owned(&state.pool, account_id, keyword.custom_filter_id)
        .await?
        .ok_or(ApiError::NotFound)
}

/// `GET /api/v1/filters`.
pub async fn index_v1(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:filters")?;
    let filters = custom_filter::owned_by(&state.pool, current.account.id).await?;
    let by_id: std::collections::HashMap<i64, &CustomFilter> =
        filters.iter().map(|f| (f.id, f)).collect();
    let filter_ids: Vec<i64> = filters.iter().map(|f| f.id).collect();
    let mut keywords = custom_filter::keywords_for_filters(&state.pool, &filter_ids).await?;
    keywords.sort_by_key(|k| k.id);
    let mut entities = Vec::with_capacity(keywords.len());
    for keyword in &keywords {
        if let Some(filter) = by_id.get(&keyword.custom_filter_id) {
            entities.push(v1_filter_json(keyword, filter)?);
        }
    }
    Ok(Json(Value::Array(entities)))
}

/// `POST /api/v1/filters` — creates a filter and its single keyword.
pub async fn create_v1(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:filters")?;
    let value = body_value(&headers, &body)?;
    let phrase = str_param(&value, "phrase").unwrap_or_default();
    let context = array_param(&value, "context").unwrap_or_default();
    let action = if cast_bool(value.get("irreversible")) {
        "hide"
    } else {
        "warn"
    };
    // The filter carries `phrase` as its title; the keyword carries it too.
    validate_filter(&phrase, action, false, &context)?;
    validate_keyword(&phrase)?;
    within_filter_limit(custom_filter::count_owned(&state.pool, current.account.id).await?)?;
    let whole_word = whole_word_param(&value);
    let expires_at = expires_at_on_create(&parse_expires_in(&value));
    let filter = custom_filter::create(
        &state.pool,
        current.account.id,
        &phrase,
        action,
        &context,
        expires_at,
        &[NewKeyword {
            keyword: phrase.clone(),
            whole_word,
        }],
    )
    .await?;
    crate::filters::invalidate(current.account.id);
    let keyword = custom_filter::keywords_for(&state.pool, filter.id)
        .await?
        .into_iter()
        .next()
        .ok_or(ApiError::NotFound)?;
    Ok(Json(v1_filter_json(&keyword, &filter)?))
}

/// `GET /api/v1/filters/{id}` — `id` addresses a keyword.
pub async fn show_v1(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(keyword_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:filters")?;
    let keyword = owned_keyword(&state, current.account.id, keyword_id).await?;
    let filter = keyword_filter(&state, current.account.id, &keyword).await?;
    Ok(Json(v1_filter_json(&keyword, &filter)?))
}

/// `PUT /api/v1/filters/{id}`.
pub async fn update_v1(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(keyword_id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:filters")?;
    let keyword = owned_keyword(&state, current.account.id, keyword_id).await?;
    let filter = keyword_filter(&state, current.account.id, &keyword).await?;
    let value = body_value(&headers, &body)?;

    let phrase = str_param(&value, "phrase").unwrap_or(keyword.keyword);
    let whole_word = value
        .get("whole_word")
        .map_or(keyword.whole_word, |v| cast_bool(Some(v)));

    // Whether the filter-level attributes actually change (Mastodon's
    // `custom_filter.changed?`): a multi-keyword filter refuses those via the
    // deprecated v1 API.
    let context_param = array_param(&value, "context");
    let action_param = value.get("irreversible").map(|v| {
        if cast_bool(Some(v)) {
            "hide".to_owned()
        } else {
            "warn".to_owned()
        }
    });
    let expires_in = parse_expires_in(&value);
    let filter_changed = context_param.as_ref().is_some_and(|c| *c != filter.context)
        || action_param.as_ref().is_some_and(|a| *a != filter.action)
        || !matches!(expires_in, ExpiresIn::Unset);
    if filter_changed && custom_filter::count_keywords(&state.pool, filter.id).await? > 1 {
        return Err(ApiError::Unprocessable(
            "These parameters cannot be changed from this application because they apply to more than one filter keyword. Use a more recent application or the web interface.".into(),
        ));
    }

    let context = context_param.unwrap_or(filter.context);
    let action = action_param.unwrap_or(filter.action);
    let expires_at = expires_at_on_update(&expires_in, filter.expires_at);
    validate_filter(&phrase, &action, false, &context)?;
    validate_keyword(&phrase)?;
    let updated_keyword =
        custom_filter::update_keyword(&state.pool, keyword_id, &phrase, whole_word).await?;
    let updated_filter = custom_filter::update(
        &state.pool,
        filter.id,
        &phrase,
        &action,
        &context,
        expires_at,
        &[],
    )
    .await?;
    crate::filters::invalidate(current.account.id);
    Ok(Json(v1_filter_json(&updated_keyword, &updated_filter)?))
}

/// `DELETE /api/v1/filters/{id}` — destroys the keyword.
pub async fn destroy_v1(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(keyword_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:filters")?;
    owned_keyword(&state, current.account.id, keyword_id).await?;
    custom_filter::delete_keyword(&state.pool, keyword_id).await?;
    crate::filters::invalidate(current.account.id);
    Ok(Json(json!({})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_limit_admits_below_cap_and_refuses_at_cap() {
        let cap = i64::try_from(MAX_FILTERS_PER_ACCOUNT).unwrap();
        assert!(within_filter_limit(0).is_ok());
        assert!(within_filter_limit(cap - 1).is_ok());
        // At the cap a further create would exceed it.
        assert!(matches!(
            within_filter_limit(cap),
            Err(ApiError::Unprocessable(_))
        ));
        assert!(within_filter_limit(cap + 5).is_err());
        // A negative count (impossible in practice) is treated as saturated.
        assert!(within_filter_limit(-1).is_err());
    }

    #[test]
    fn keyword_limit_admits_at_cap_and_refuses_above() {
        assert!(within_keyword_limit(0).is_ok());
        assert!(within_keyword_limit(MAX_KEYWORDS_PER_FILTER).is_ok());
        assert!(matches!(
            within_keyword_limit(MAX_KEYWORDS_PER_FILTER + 1),
            Err(ApiError::Unprocessable(_))
        ));
    }
}
