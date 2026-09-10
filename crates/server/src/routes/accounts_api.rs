//! `/api/v1/accounts/*` — the Mastodon accounts API.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{FromRequest, Multipart, Path, Query, RawQuery, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use plamenu_db::{account, follow, remote_history, status, user};
use serde::Deserialize;
use serde_json::{Value, json};

use super::params::truthy;
use super::timelines::link_header;
use crate::actions::{self, is_language_code};
use crate::auth::{CurrentUser, MaybeUser};
use crate::entities::{
    account_json, can_view, profile_json, relationship_json, render_accounts, render_relationships,
    render_statuses, with_source,
};
use crate::error::ApiError;
use crate::media_processing::MAX_UPLOAD_BYTES;
use crate::profile::{ProfileChanges, ProfileImage, clear_profile_image, update_profile};
use crate::state::AppState;

const DEFAULT_LIMIT: i64 = 20;
const MAX_LIMIT: i64 = 40;

/// `GET /api/v1/accounts/verify_credentials`.
pub async fn verify_credentials(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Json<Value>, ApiError> {
    // Mastodon allows either `read` (or finer) or the dedicated `profile`
    // scope for this endpoint.
    if current.require_scope("read:accounts").is_err() {
        current.require_scope("profile")?;
    }
    let entity = account_json(
        &state.pool,
        &state.config.domain,
        &current.account,
        Some(current.account.id),
    )
    .await?;
    let settings = user::settings_by_user_id(&state.pool, current.user.id)
        .await?
        .unwrap_or_default();
    Ok(Json(
        with_source(&state.pool, entity, &current.account, &settings).await?,
    ))
}

/// Folds one text parameter into a [`ProfileChanges`], collecting
/// Rails-style `fields_attributes[{i}][name|value]` keys by index. Shared with
/// the first-party web settings form, which submits the same field names.
pub(crate) fn apply_text_param(
    changes: &mut ProfileChanges,
    fields: &mut BTreeMap<String, (String, String)>,
    key: &str,
    value: String,
) {
    match key {
        "display_name" => changes.display_name = Some(value),
        "note" => changes.note = Some(value),
        "locked" => changes.locked = Some(truthy(Some(&value))),
        "discoverable" => changes.discoverable = Some(truthy(Some(&value))),
        "bot" => changes.bot = Some(truthy(Some(&value))),
        "indexable" => changes.indexable = Some(truthy(Some(&value))),
        "hide_collections" => changes.hide_collections = Some(truthy(Some(&value))),
        "avatar_description" => changes.avatar_description = Some(value),
        "header_description" => changes.header_description = Some(value),
        "show_media" => changes.show_media = Some(truthy(Some(&value))),
        "show_media_replies" => changes.show_media_replies = Some(truthy(Some(&value))),
        "show_featured" => changes.show_featured = Some(truthy(Some(&value))),
        // Rails array-parameter form; a bare empty value clears the list.
        "attribution_domains[]" | "attribution_domains" => {
            let list = changes.attribution_domains.get_or_insert_default();
            if !value.trim().is_empty() {
                list.push(value);
            }
            *list = crate::profile::normalize_attribution_domains(list);
        }
        _ => {
            if let Some(rest) = key.strip_prefix("fields_attributes[")
                && let Some((index, attr)) = rest.split_once(']')
            {
                // Mark fields as present even before any value lands, so an
                // all-empty submission still clears them.
                changes.fields.get_or_insert_default();
                let entry = fields.entry(index.to_owned()).or_default();
                match attr {
                    "[name]" => entry.0 = value,
                    "[value]" => entry.1 = value,
                    _ => {}
                }
            }
        }
    }
}

/// The `source[...]` posting-default changes riding along an
/// `update_credentials` request (Mastodon folds them into the user settings;
/// the profile endpoint does not accept them).
#[derive(Default)]
pub(crate) struct SourceChanges {
    privacy: Option<String>,
    sensitive: Option<bool>,
    language: Option<String>,
    quote_policy: Option<String>,
    show_application: Option<bool>,
}

impl SourceChanges {
    fn any(&self) -> bool {
        self.privacy.is_some()
            || self.sensitive.is_some()
            || self.language.is_some()
            || self.quote_policy.is_some()
            || self.show_application.is_some()
    }

    /// Merges the submitted changes over the stored settings. The enum-valued
    /// fields reject unknown values like Mastodon's `in:` setting validations.
    fn apply(self, mut settings: user::UserSettings) -> Result<user::UserSettings, ApiError> {
        if let Some(privacy) = self.privacy {
            if user::PostingDefaultVisibility::parse(&privacy).as_str() != privacy {
                return Err(ApiError::Unprocessable(
                    "Validation failed: Default privacy is not included in the list".into(),
                ));
            }
            settings.posting_default_visibility = user::PostingDefaultVisibility::parse(&privacy);
        }
        if let Some(sensitive) = self.sensitive {
            settings.posting_default_sensitive = sensitive;
        }
        if let Some(language) = self.language {
            settings.posting_default_language = language;
        }
        if let Some(quote_policy) = self.quote_policy {
            if user::DefaultQuotePolicy::parse(&quote_policy).as_str() != quote_policy {
                return Err(ApiError::Unprocessable(
                    "Validation failed: Default quote policy is not included in the list".into(),
                ));
            }
            settings.posting_default_quote_policy = user::DefaultQuotePolicy::parse(&quote_policy);
        }
        if let Some(show_application) = self.show_application {
            settings.show_application = show_application;
        }
        Ok(settings)
    }
}

/// Folds one `source[...]` text parameter into a [`SourceChanges`]; returns
/// whether the key was a `source[...]` key (consumed or not).
fn apply_source_param(sources: &mut SourceChanges, key: &str, value: &str) -> bool {
    let Some(rest) = key.strip_prefix("source[") else {
        return false;
    };
    match rest {
        "privacy]" => sources.privacy = Some(value.to_owned()),
        "sensitive]" => sources.sensitive = Some(truthy(Some(value))),
        "language]" => sources.language = Some(value.to_owned()),
        "quote_policy]" => sources.quote_policy = Some(value.to_owned()),
        "show_application]" => sources.show_application = Some(truthy(Some(value))),
        _ => {}
    }
    true
}

/// `fields_attributes` from a JSON body: an array of `{name, value}`, or the
/// Rails-style object keyed by index.
fn json_fields_attributes(value: &Value) -> Option<Vec<(String, String)>> {
    let pair = |entry: &Value| {
        (
            entry
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            entry
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        )
    };
    match value {
        Value::Array(entries) => Some(entries.iter().map(pair).collect()),
        Value::Object(map) => {
            let mut keyed: Vec<(&String, &Value)> = map.iter().collect();
            keyed.sort_by_key(|(index, _)| index.parse::<u64>().unwrap_or(u64::MAX));
            Some(keyed.into_iter().map(|(_, entry)| pair(entry)).collect())
        }
        _ => None,
    }
}

/// Parses an `update_credentials` request body — multipart (the only form
/// that can carry avatar/header files), JSON, or form-urlencoded. The
/// `source[...]` posting defaults are collected separately: only
/// `update_credentials` applies them (Mastodon's profile endpoint has no
/// `source` params).
#[allow(clippy::too_many_lines)]
async fn parse_profile_request(
    state: &AppState,
    request: Request,
) -> Result<(ProfileChanges, SourceChanges), ApiError> {
    let content_type = request
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let mut changes = ProfileChanges::default();
    let mut sources = SourceChanges::default();
    let mut fields: BTreeMap<String, (String, String)> = BTreeMap::new();

    if content_type.starts_with("multipart/form-data") {
        let mut multipart = Multipart::from_request(request, state)
            .await
            .map_err(|e| ApiError::BadRequest(format!("invalid multipart body: {e}")))?;
        while let Some(field) = multipart
            .next_field()
            .await
            .map_err(|e| ApiError::BadRequest(format!("invalid multipart body: {e}")))?
        {
            let name = field.name().unwrap_or_default().to_owned();
            match name.as_str() {
                "avatar" | "header" => {
                    let bytes = field
                        .bytes()
                        .await
                        .map_err(|e| ApiError::BadRequest(format!("upload failed: {e}")))?;
                    if bytes.len() > MAX_UPLOAD_BYTES {
                        return Err(ApiError::Unprocessable(
                            "Validation failed: File size exceeds the limit".into(),
                        ));
                    }
                    if !bytes.is_empty() {
                        let target = if name == "avatar" {
                            &mut changes.avatar
                        } else {
                            &mut changes.header
                        };
                        *target = Some(bytes.to_vec());
                    }
                }
                _ => {
                    let text = field.text().await.unwrap_or_default();
                    if !apply_source_param(&mut sources, &name, &text) {
                        apply_text_param(&mut changes, &mut fields, &name, text);
                    }
                }
            }
        }
    } else {
        let body = axum::body::to_bytes(request.into_body(), MAX_UPLOAD_BYTES)
            .await
            .map_err(|e| ApiError::BadRequest(format!("cannot read body: {e}")))?;
        if content_type.starts_with("application/json") {
            let parsed: Value = serde_json::from_slice(&body)
                .map_err(|e| ApiError::BadRequest(format!("invalid JSON body: {e}")))?;
            if let Some(name) = parsed.get("display_name").and_then(Value::as_str) {
                changes.display_name = Some(name.to_owned());
            }
            let json_str = |key| parsed.get(key).and_then(Value::as_str).map(str::to_owned);
            changes.note = json_str("note");
            changes.avatar_description = json_str("avatar_description");
            changes.header_description = json_str("header_description");
            match parsed.get("locked") {
                Some(Value::Bool(flag)) => changes.locked = Some(*flag),
                Some(Value::String(s)) => changes.locked = Some(truthy(Some(s))),
                _ => {}
            }
            match parsed.get("discoverable") {
                Some(Value::Bool(flag)) => changes.discoverable = Some(*flag),
                Some(Value::String(s)) => changes.discoverable = Some(truthy(Some(s))),
                _ => {}
            }
            let json_bool = |key: &str| match parsed.get(key) {
                Some(Value::Bool(flag)) => Some(*flag),
                Some(Value::String(s)) => Some(truthy(Some(s))),
                _ => None,
            };
            changes.bot = json_bool("bot");
            changes.indexable = json_bool("indexable");
            changes.hide_collections = json_bool("hide_collections");
            changes.show_media = json_bool("show_media");
            changes.show_media_replies = json_bool("show_media_replies");
            changes.show_featured = json_bool("show_featured");
            if let Some(parsed_fields) = parsed
                .get("fields_attributes")
                .and_then(json_fields_attributes)
            {
                changes.fields = Some(parsed_fields);
            }
            if let Some(domains) = parsed.get("attribution_domains").and_then(Value::as_array) {
                let raw: Vec<String> = domains
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect();
                changes.attribution_domains =
                    Some(crate::profile::normalize_attribution_domains(&raw));
            }
            if let Some(source) = parsed.get("source") {
                let source_str =
                    |key: &str| source.get(key).and_then(Value::as_str).map(str::to_owned);
                sources.privacy = source_str("privacy");
                sources.language = source_str("language");
                sources.quote_policy = source_str("quote_policy");
                sources.sensitive = match source.get("sensitive") {
                    Some(Value::Bool(flag)) => Some(*flag),
                    Some(Value::String(s)) => Some(truthy(Some(s))),
                    _ => None,
                };
                sources.show_application = match source.get("show_application") {
                    Some(Value::Bool(flag)) => Some(*flag),
                    Some(Value::String(s)) => Some(truthy(Some(s))),
                    _ => None,
                };
            }
        } else {
            let pairs: Vec<(String, String)> = serde_urlencoded::from_bytes(&body)
                .map_err(|e| ApiError::BadRequest(format!("invalid form body: {e}")))?;
            for (key, value) in pairs {
                if !apply_source_param(&mut sources, &key, &value) {
                    apply_text_param(&mut changes, &mut fields, &key, value);
                }
            }
        }
    }
    if !fields.is_empty() {
        changes.fields = Some(fields.into_values().collect());
    }
    Ok((changes, sources))
}

/// `PATCH /api/v1/accounts/update_credentials`.
pub async fn update_credentials(
    State(state): State<AppState>,
    current: CurrentUser,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:accounts")?;
    let (mut changes, sources) = parse_profile_request(&state, request).await?;
    // Only `/api/v1/profile` permits the profile-tab params; Mastodon's
    // `update_credentials` silently drops them.
    changes.show_media = None;
    changes.show_media_replies = None;
    changes.show_featured = None;
    let updated = update_profile(&state, &current.account, changes).await?;
    let entity = account_json(
        &state.pool,
        &state.config.domain,
        &updated,
        Some(current.account.id),
    )
    .await?;
    let mut settings = user::settings_by_user_id(&state.pool, current.user.id)
        .await?
        .unwrap_or_default();
    if sources.any() {
        settings = user::update_settings(&state.pool, current.user.id, sources.apply(settings)?)
            .await?
            .ok_or(ApiError::NotFound)?;
    }
    Ok(Json(
        with_source(&state.pool, entity, &updated, &settings).await?,
    ))
}

/// `GET /api/v1/profile` — the owner's profile-editing view (raw note and
/// fields alongside their formatted forms), Mastodon's `ProfilesController`.
pub async fn profile_show(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Json<Value>, ApiError> {
    // Mastodon allows `profile`, `read` or `read:accounts` here.
    if current.require_scope("read:accounts").is_err() {
        current.require_scope("profile")?;
    }
    Ok(Json(
        profile_json(&state.pool, &state.config.domain, &current.account).await?,
    ))
}

/// `PATCH /api/v1/profile` — updates the profile (same params as
/// `update_credentials`) and returns the Profile entity.
pub async fn profile_update(
    State(state): State<AppState>,
    current: CurrentUser,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:accounts")?;
    // `source[...]` is ignored here: Mastodon's profile endpoint permits only
    // the account params.
    let (changes, _) = parse_profile_request(&state, request).await?;
    let updated = update_profile(&state, &current.account, changes).await?;
    Ok(Json(
        profile_json(&state.pool, &state.config.domain, &updated).await?,
    ))
}

/// `DELETE /api/v1/profile/avatar` — removes the avatar and returns the
/// updated `CredentialAccount`, like Mastodon's `Profile::AvatarsController`.
pub async fn profile_delete_avatar(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Json<Value>, ApiError> {
    delete_profile_image(&state, &current, ProfileImage::Avatar).await
}

/// `DELETE /api/v1/profile/header`.
pub async fn profile_delete_header(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Json<Value>, ApiError> {
    delete_profile_image(&state, &current, ProfileImage::Header).await
}

async fn delete_profile_image(
    state: &AppState,
    current: &CurrentUser,
    which: ProfileImage,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:accounts")?;
    let updated = clear_profile_image(state, &current.account, which).await?;
    let entity = account_json(
        &state.pool,
        &state.config.domain,
        &updated,
        Some(current.account.id),
    )
    .await?;
    let settings = user::settings_by_user_id(&state.pool, current.user.id)
        .await?
        .unwrap_or_default();
    Ok(Json(
        with_source(&state.pool, entity, &updated, &settings).await?,
    ))
}

/// `GET /api/v1/accounts/{id}` — public, like Mastodon's.
/// Mastodon's `DEFAULT_ACCOUNTS_LIMIT` — the cap on `GET /api/v1/accounts`.
const MAX_BATCH_ACCOUNTS: usize = 40;

/// Cap on `id[]` entries for the "for-each-account" batch endpoints
/// (`familiar_followers`, `relationships`). More generous than
/// `MAX_BATCH_ACCOUNTS` so a client reconciling a full followers page keeps
/// working, but small enough that a client cannot pack a ~2 MiB body with
/// hundreds of thousands of ids and force per-id work.
const MAX_ID_BATCH: usize = 200;

/// Parses `id[]=`/`id=` query params into deduplicated ids in first-seen order,
/// rejecting with 422 as soon as the distinct count would exceed `max`. Using a
/// hash set for membership keeps parsing linear even for a hostile body; the
/// early cap bounds every downstream per-id operation.
fn parse_unique_id_batch(query: Option<&str>, max: usize) -> Result<Vec<i64>, ApiError> {
    let pairs: Vec<(String, String)> = serde_urlencoded::from_str(query.unwrap_or(""))
        .map_err(|e| ApiError::BadRequest(format!("invalid query string: {e}")))?;
    let mut requested: Vec<i64> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (key, value) in pairs {
        if key != "id[]" && key != "id" {
            continue;
        }
        if let Ok(id) = value.parse::<i64>()
            && seen.insert(id)
        {
            requested.push(id);
            if requested.len() > max {
                return Err(ApiError::Unprocessable("Validation failed".to_owned()));
            }
        }
    }
    Ok(requested)
}

/// Like [`parse_unique_id_batch`] but preserves order *and* duplicates, capped
/// at `max` total entries. `relationships` renders a repeated id twice to match
/// Mastodon, so the response size is bounded by the raw entry count.
fn parse_ordered_id_batch(query: Option<&str>, max: usize) -> Result<Vec<i64>, ApiError> {
    let pairs: Vec<(String, String)> = serde_urlencoded::from_str(query.unwrap_or(""))
        .map_err(|e| ApiError::BadRequest(format!("invalid query string: {e}")))?;
    let mut ids: Vec<i64> = Vec::new();
    for (key, value) in pairs {
        if key != "id[]" && key != "id" {
            continue;
        }
        if let Ok(id) = value.parse::<i64>() {
            ids.push(id);
            if ids.len() > max {
                return Err(ApiError::Unprocessable("Validation failed".to_owned()));
            }
        }
    }
    Ok(ids)
}

/// `GET /api/v1/accounts?id[]=…` — batch fetch (Mastodon's `accounts#index`).
/// Returns the requested accounts in request order; unknown and instance-
/// policy-hidden remote accounts are silently dropped. Accounts carry no
/// per-viewer block/mute visibility here (unlike the batch statuses endpoint).
/// Over the 40-id cap is a validation error.
pub async fn index(
    State(state): State<AppState>,
    MaybeUser(viewer): MaybeUser,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    if let Some(viewer) = &viewer {
        viewer.require_scope("read:accounts")?;
    }
    let viewer_id = viewer.as_ref().map(|u| u.account.id);
    let requested = parse_unique_id_batch(query.as_deref(), MAX_BATCH_ACCOUNTS)?;
    let mut fetched = account::find_publicly_available_by_ids(&state.pool, &requested).await?;
    // Reorder to match the request, like Mastodon's stable id ordering.
    fetched.sort_by_key(|a| requested.iter().position(|id| *id == a.id));
    let mut visible = Vec::with_capacity(fetched.len());
    for account in fetched {
        if crate::instance_policy::account_visible(&state.pool, &state.config.domain, &account)
            .await?
        {
            visible.push(account);
        }
    }
    let rendered = render_accounts(&state.pool, &state.config.domain, &visible, viewer_id).await?;
    Ok(Json(Value::Array(rendered)))
}

pub async fn show(
    State(state): State<AppState>,
    MaybeUser(viewer): MaybeUser,
    Path(account_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    if let Some(viewer) = &viewer {
        viewer.require_scope("read:accounts")?;
    }
    let account = account::find_publicly_available_by_id(&state.pool, account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if account::is_internal(&state.pool, account.id).await? {
        return Err(ApiError::NotFound);
    }
    if !crate::instance_policy::account_visible(&state.pool, &state.config.domain, &account).await?
    {
        return Err(ApiError::NotFound);
    }
    let viewer_id = viewer.as_ref().map(|u| u.account.id);
    Ok(Json(
        account_json(&state.pool, &state.config.domain, &account, viewer_id).await?,
    ))
}

/// `GET /api/v1/accounts/{id}/remote_history` — authenticated, local-only
/// hydration state. Reading it never enqueues and never contacts the origin.
pub async fn remote_history_show(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(account_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:statuses")?;
    let account = account::find_by_id(&state.pool, account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if account.has_local_account_on(&state.config.domain)
        || !crate::instance_policy::account_visible(&state.pool, &state.config.domain, &account)
            .await?
    {
        return Err(ApiError::NotFound);
    }
    let snapshot = remote_history::snapshot(&state.pool, account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(json!({
        "account_id": snapshot.account_id.to_string(),
        "enabled": snapshot.hydration_enabled,
        "state": snapshot.state,
        "available_local_items": snapshot.available_statuses,
        "available_statuses": snapshot.available_statuses,
        "reported_total_items": snapshot.reported_total_items,
        "last_attempt_at": snapshot.last_attempt_at,
        "last_success_at": snapshot.last_success_at,
        "retry_at": snapshot.retry_at,
        "error_class": snapshot.last_error_class,
        "pages_fetched": snapshot.pages_fetched,
        "items_seen": snapshot.items_seen,
        "items_accepted": snapshot.items_accepted,
        "bytes_fetched": snapshot.bytes_fetched,
        "can_load_older": snapshot.next_page_uri.is_some(),
    })))
}

#[derive(Debug, Deserialize, Default)]
pub struct RemoteHistoryFetchBody {
    #[serde(default)]
    mode: Option<String>,
}

/// `POST /api/v1/accounts/{id}/remote_history/fetch` — explicit authenticated
/// admission. The response reports queue state only; cursors stay server-side.
pub async fn remote_history_fetch(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(account_id): Path<i64>,
    body: Option<Json<RemoteHistoryFetchBody>>,
) -> Result<Response, ApiError> {
    current.require_scope("read:statuses")?;
    let account = account::find_by_id(&state.pool, account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !matches!(account.actor_type.as_deref(), Some("Person" | "Service"))
        || account.has_local_account_on(&state.config.domain)
    {
        return Err(ApiError::Conflict(
            "remote history is unsupported for this actor".into(),
        ));
    }
    let mode = body
        .as_ref()
        .and_then(|Json(body)| body.mode.as_deref())
        .unwrap_or("refresh");
    let kind = match mode {
        "initial" => remote_history::JobKind::Initial,
        "refresh" => remote_history::JobKind::Refresh,
        "older" => remote_history::JobKind::Older,
        _ => {
            return Err(ApiError::Unprocessable(
                "mode must be initial, refresh, or older".into(),
            ));
        }
    };
    let outcome =
        crate::remote_history::request(&state, &account, kind, Some(current.account.id)).await?;
    let (status, value) = match outcome {
        remote_history::EnqueueOutcome::Enqueued => {
            (StatusCode::ACCEPTED, json!({"state": "queued"}))
        }
        remote_history::EnqueueOutcome::Coalesced => (
            StatusCode::ACCEPTED,
            json!({"state": "queued", "coalesced": true}),
        ),
        remote_history::EnqueueOutcome::Fresh => (StatusCode::OK, json!({"state": "fresh"})),
        remote_history::EnqueueOutcome::Disabled => (
            StatusCode::CONFLICT,
            json!({"error": "remote history hydration is disabled"}),
        ),
        remote_history::EnqueueOutcome::Backoff(retry_at) => (
            StatusCode::TOO_MANY_REQUESTS,
            json!({"error": "remote origin is in backoff", "retry_at": retry_at}),
        ),
        remote_history::EnqueueOutcome::AutomaticCooldown(retry_at) => (
            StatusCode::TOO_MANY_REQUESTS,
            json!({"error": "automatic history hydration is cooling down", "retry_at": retry_at}),
        ),
        remote_history::EnqueueOutcome::OriginBusy(retry_at) => (
            StatusCode::TOO_MANY_REQUESTS,
            json!({"error": "remote origin history queue is full", "retry_at": retry_at}),
        ),
        remote_history::EnqueueOutcome::RateLimited(retry_at) => (
            StatusCode::TOO_MANY_REQUESTS,
            json!({"error": "remote history request limit exceeded", "retry_at": retry_at}),
        ),
    };
    Ok((status, Json(value)).into_response())
}

#[derive(Deserialize)]
pub struct LookupQuery {
    acct: Option<String>,
    // Clients also send `skip_webfinger`; Mastodon ignores it (lookup never
    // webfingers), and so do we.
}

/// `GET /api/v1/accounts/lookup` — resolves `acct` against already-known
/// accounts only. Unknown accounts are a 404, exactly like Mastodon's
/// `ResolveAccountService` with `skip_webfinger: true`.
pub async fn lookup(
    State(state): State<AppState>,
    MaybeUser(viewer): MaybeUser,
    Query(query): Query<LookupQuery>,
) -> Result<Json<Value>, ApiError> {
    if let Some(viewer) = &viewer {
        viewer.require_scope("read:accounts")?;
    }
    let raw = query.acct.as_deref().unwrap_or("").trim();
    // `!name@host` is an accepted Plamenu extension that asks for the Group; a
    // bare or `@name@host` handle prefers the person-like actor but falls back
    // to a same-named Group so a community-only handle (Lemmy) still resolves —
    // the behavior Mastodon's first-link WebFinger already gives for those.
    let group_wanted = raw.starts_with('!');
    let acct = raw.trim_start_matches(['@', '!']);
    let account = match acct.split_once('@') {
        None => account::find_public_local_account_by_username(&state.pool, acct).await?,
        Some((username, domain)) if state.config.is_local_domain(domain) => {
            account::find_public_local_account_by_username(&state.pool, username).await?
        }
        Some((username, domain)) if group_wanted => {
            account::find_remote_group_by_acct(&state.pool, username, domain).await?
        }
        Some((username, domain)) => {
            match account::find_remote_person_by_acct(&state.pool, username, domain).await? {
                Some(person) => Some(person),
                None => account::find_remote_group_by_acct(&state.pool, username, domain).await?,
            }
        }
    };
    let account = account.ok_or(ApiError::NotFound)?;
    if !crate::instance_policy::account_visible(&state.pool, &state.config.domain, &account).await?
    {
        return Err(ApiError::NotFound);
    }
    let viewer_id = viewer.as_ref().map(|u| u.account.id);
    Ok(Json(
        account_json(&state.pool, &state.config.domain, &account, viewer_id).await?,
    ))
}

#[derive(Deserialize)]
pub struct StatusesQuery {
    max_id: Option<i64>,
    since_id: Option<i64>,
    limit: Option<i64>,
    pinned: Option<String>,
    exclude_replies: Option<String>,
    exclude_reblogs: Option<String>,
    only_media: Option<String>,
    tagged: Option<String>,
    // Plamenu extensions, honored on group accounts only: the
    // vote-ranked sorts (`top`/`hot`), Top's window (`day`/`week`/`month`/
    // `all`) and their offset page — rank orders don't keyset.
    sort: Option<String>,
    t: Option<String>,
    page: Option<i64>,
}

/// Treat the first profile-timeline read from an authenticated Mastodon client
/// as the same bounded user intent as opening that profile in Plamenu's web UI.
/// This is deliberately best-effort: hydration must never make the compatible
/// account-status endpoint fail, wait on federation, or change its response.
fn maybe_request_remote_history(
    state: &AppState,
    account: &account::Account,
    viewer: Option<&CurrentUser>,
    query: &StatusesQuery,
) {
    let eligible_first_page = query.max_id.is_none()
        && query.since_id.is_none()
        && !truthy(query.pinned.as_deref())
        && account.domain.is_some()
        && !account.is_portable_on(&state.config.domain)
        && matches!(account.actor_type.as_deref(), Some("Person" | "Service"));
    let Some(viewer) = viewer.filter(|viewer| viewer.has_scope("read:statuses")) else {
        return;
    };
    if !eligible_first_page {
        return;
    }

    crate::remote_history::spawn_initial_request(state, account, viewer.account.id);
}

/// `GET /api/v1/accounts/{id}/statuses` — public, visibility-gated per
/// viewer like Mastodon's `AccountStatusesFilter`. An authenticated first-page
/// read may also enqueue bounded remote-history hydration; the response itself
/// remains an immediate read of local data.
pub async fn statuses(
    State(state): State<AppState>,
    MaybeUser(viewer): MaybeUser,
    Path(account_id): Path<i64>,
    Query(query): Query<StatusesQuery>,
) -> Result<impl IntoResponse, ApiError> {
    if let Some(viewer) = &viewer {
        viewer.require_scope("read:statuses")?;
    }
    let account = account::find_publicly_available_by_id(&state.pool, account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !crate::instance_policy::account_visible(&state.pool, &state.config.domain, &account).await?
    {
        return Err(ApiError::NotFound);
    }
    if account.suspended() {
        return Ok((HeaderMap::new(), Json(Vec::new())));
    }
    // The listing honors the viewer's timeline-ordering preference, like the
    // home/public/list/tag timelines; anonymous viewers read in publish order.
    let order = super::timelines::order_for(&state, viewer.as_ref().map(|v| v.user.id)).await?;
    let viewer_id = viewer.as_ref().map(|v| v.account.id);
    // An account that blocks the viewer shows them an empty listing, like
    // Mastodon's `AccountStatusesFilter`.
    if let Some(viewer_id) = viewer_id
        && plamenu_db::block::exists(&state.pool, account_id, viewer_id).await?
    {
        return Ok((HeaderMap::new(), Json(Vec::new())));
    }
    maybe_request_remote_history(&state, &account, viewer.as_ref(), &query);
    if truthy(query.pinned.as_deref()) {
        // Pinned statuses, most recently pinned first, visibility-gated per
        // viewer; at most 5 exist, so no pagination headers.
        let mut pinned = Vec::new();
        for item in plamenu_db::pin::pinned_statuses(&state.pool, account_id).await? {
            if can_view(&state.pool, &item, viewer_id).await? {
                pinned.push(item);
            }
        }
        let entities =
            render_statuses(&state.pool, &state.config.domain, &pinned, viewer_id).await?;
        return Ok((HeaderMap::new(), Json(entities)));
    }
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    // The vote-ranked group sorts: the group's boost rows by score.
    // No Link header — offset paging rides the `page` parameter.
    if account.is_group()
        && let Some(sort) = query
            .sort
            .as_deref()
            .filter(|sort| matches!(*sort, "top" | "hot"))
    {
        let offset = query.page.unwrap_or(0).max(0) * limit;
        let ranked = match sort {
            "top" => {
                let window =
                    plamenu_db::group::TopWindow::parse(query.t.as_deref().unwrap_or("week"));
                plamenu_db::group::timeline_top(&state.pool, account_id, window, limit, offset)
                    .await?
            }
            _ => plamenu_db::group::timeline_hot(&state.pool, account_id, limit, offset).await?,
        };
        let entities =
            render_statuses(&state.pool, &state.config.domain, &ranked, viewer_id).await?;
        return Ok((HeaderMap::new(), Json(entities)));
    }
    let filter = status::AccountStatusesFilter {
        exclude_replies: truthy(query.exclude_replies.as_deref()),
        exclude_reblogs: truthy(query.exclude_reblogs.as_deref()),
        only_media: truthy(query.only_media.as_deref()),
        media_through_reblog: false,
        tagged: query.tagged,
        max_id: query.max_id,
        since_id: query.since_id,
    };
    let page =
        status::by_account(&state.pool, account_id, viewer_id, &filter, order, limit).await?;
    let entities = render_statuses(&state.pool, &state.config.domain, &page, viewer_id).await?;
    let path = format!("/api/v1/accounts/{account_id}/statuses");
    let headers = link_header(&state.config.domain, &path, limit, &page);
    Ok((headers, Json(entities)))
}

/// Mastodon's `DEFAULT_ACCOUNTS_LIMIT` (and its doubled hard cap).
const ACCOUNTS_LIMIT: i64 = 40;
const ACCOUNTS_MAX_LIMIT: i64 = 80;

#[derive(Deserialize)]
pub struct FollowListQuery {
    max_id: Option<i64>,
    since_id: Option<i64>,
    limit: Option<i64>,
}

/// `GET /api/v1/accounts/{id}/followers` — public like Mastodon's, but a
/// provided token must still carry `read`.
pub async fn followers(
    State(state): State<AppState>,
    MaybeUser(viewer): MaybeUser,
    Path(account_id): Path<i64>,
    Query(query): Query<FollowListQuery>,
) -> Result<impl IntoResponse, ApiError> {
    follow_list(&state, viewer, account_id, &query, "followers").await
}

/// `GET /api/v1/accounts/{id}/following`.
pub async fn following(
    State(state): State<AppState>,
    MaybeUser(viewer): MaybeUser,
    Path(account_id): Path<i64>,
    Query(query): Query<FollowListQuery>,
) -> Result<impl IntoResponse, ApiError> {
    follow_list(&state, viewer, account_id, &query, "following").await
}

async fn follow_list(
    state: &AppState,
    viewer: Option<CurrentUser>,
    account_id: i64,
    query: &FollowListQuery,
    which: &str,
) -> Result<(HeaderMap, Json<Value>), ApiError> {
    if let Some(viewer) = &viewer {
        viewer.require_scope("read:accounts")?;
    }
    let account = account::find_publicly_available_by_id(&state.pool, account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    // `hide_collections` keeps the follows/followers lists empty for everyone
    // but the owner, like Mastodon's `hides_following?` / `hides_followers?`.
    let is_owner = viewer.as_ref().is_some_and(|v| v.account.id == account_id);
    let viewer_id = viewer.as_ref().map(|v| v.account.id);
    if account.hide_collections && !is_owner {
        let path = format!("/api/v1/accounts/{account_id}/{which}");
        return render_account_page(state, &path, ACCOUNTS_LIMIT, &[], viewer_id).await;
    }
    let limit = query
        .limit
        .unwrap_or(ACCOUNTS_LIMIT)
        .clamp(1, ACCOUNTS_MAX_LIMIT);
    // Members who hid their own social graph drop out of everyone else's
    // lists, but still see themselves and are always visible to the list owner
    // — `viewer_id` carries that identity into the query.
    let entries = if which == "followers" {
        follow::followers_of(
            &state.pool,
            account_id,
            query.max_id,
            query.since_id,
            limit,
            viewer_id,
        )
        .await?
    } else {
        follow::following_of(
            &state.pool,
            account_id,
            query.max_id,
            query.since_id,
            limit,
            viewer_id,
        )
        .await?
    };
    let pairs: Vec<(i64, i64)> = entries
        .iter()
        .map(|entry| (entry.follow_id, entry.account_id))
        .collect();
    let path = format!("/api/v1/accounts/{account_id}/{which}");
    render_account_page(state, &path, limit, &pairs, viewer_id).await
}

#[derive(Deserialize, Default)]
pub struct FollowParams {
    /// Show this account's boosts on the home timeline; absent keeps the
    /// stored value (default true on a fresh follow).
    reblogs: Option<Value>,
    /// Notify me when this account posts; absent keeps the stored value
    /// (default false on a fresh follow).
    notify: Option<Value>,
    /// Show this account's replies on the home timeline (our extension);
    /// absent keeps the stored value, which on a fresh follow depends on who
    /// was followed — a person starts true, a community or bot false.
    replies: Option<Value>,
    /// Only show posts in these languages; an empty list clears the filter,
    /// absent keeps the stored value. JSON bodies land here; form bodies
    /// send `languages[]` (recovered separately).
    languages: Option<Vec<String>>,
}

/// `POST /api/v1/accounts/{id}/follow` — follows, or (like Mastodon's
/// `FollowService` on an existing follow) just updates the per-follow
/// `reblogs`/`notify`/`languages` settings, plus our `replies` extension.
pub async fn follow(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(account_id): Path<i64>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    // Mastodon accepts `follow`, `write` or `write:follows` here.
    if current.require_scope("write:follows").is_err() {
        current.require_scope("follow")?;
    }
    let mut params: FollowParams = if body.is_empty() {
        FollowParams::default()
    } else {
        super::params::parse_body(&headers, &body)?
    };
    if params.languages.is_none() && super::params::form_has_field(&headers, &body, "languages") {
        params.languages = Some(super::params::repeated_form_field(
            &headers,
            &body,
            "languages",
        ));
    }
    let languages: Option<Vec<String>> = params
        .languages
        .map(|langs| langs.into_iter().filter(|code| !code.is_empty()).collect());
    // Mastodon's `validates :languages, language: true` wording.
    if languages
        .as_deref()
        .is_some_and(|langs| langs.iter().any(|code| !is_language_code(code)))
    {
        return Err(ApiError::Unprocessable(
            "Validation failed: Languages is invalid".into(),
        ));
    }
    let reblogs = params.reblogs.as_ref().map(bool_value);
    let notify = params.notify.as_ref().map(bool_value);
    let replies = params.replies.as_ref().map(bool_value);
    let target = account::find_by_id(&state.pool, account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    actions::follow_account(&state, &current.account, &target).await?;
    if reblogs.is_some() || notify.is_some() || replies.is_some() || languages.is_some() {
        follow::update_settings(
            &state.pool,
            current.account.id,
            target.id,
            reblogs,
            replies,
            notify,
            languages.as_deref(),
        )
        .await?;
    }
    plamenu_db::metrics::record_interaction(&state.pool)
        .await
        .ok();
    let mut relationship = relationship_json(&state.pool, current.account.id, &target).await?;
    // Mastodon reports unlocked follows optimistically — `following: true`
    // even while the remote Accept is still in flight. Locked targets get
    // the real (requested) state.
    if !target.locked {
        relationship["following"] = Value::Bool(true);
        relationship["requested"] = Value::Bool(false);
    }
    Ok(Json(relationship))
}

/// Mastodon's boolean param cast for JSON-or-form bodies.
fn bool_value(value: &Value) -> bool {
    match value {
        Value::Bool(flag) => *flag,
        Value::String(s) => truthy(Some(s)),
        _ => false,
    }
}

/// `POST /api/v1/accounts/{id}/unfollow`.
pub async fn unfollow(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(account_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    if current.require_scope("write:follows").is_err() {
        current.require_scope("follow")?;
    }
    let target = account::find_by_id(&state.pool, account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    actions::unfollow_account(&state, &current.account, &target).await?;
    Ok(Json(
        relationship_json(&state.pool, current.account.id, &target).await?,
    ))
}

/// `POST /api/v1/accounts/{id}/block`.
pub async fn block(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(account_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    // Mastodon accepts `follow`, `write` or `write:blocks` here.
    if current.require_scope("write:blocks").is_err() {
        current.require_scope("follow")?;
    }
    let target = account::find_by_id(&state.pool, account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    actions::block_account(&state, &current.account, &target).await?;
    Ok(Json(
        relationship_json(&state.pool, current.account.id, &target).await?,
    ))
}

/// `POST /api/v1/accounts/{id}/unblock`.
pub async fn unblock(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(account_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    if current.require_scope("write:blocks").is_err() {
        current.require_scope("follow")?;
    }
    let target = account::find_by_id(&state.pool, account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    actions::unblock_account(&state, &current.account, &target).await?;
    Ok(Json(
        relationship_json(&state.pool, current.account.id, &target).await?,
    ))
}

#[derive(Deserialize, Default)]
pub struct MuteParams {
    /// Hide notifications too; Mastodon defaults this to true.
    notifications: Option<Value>,
    /// Seconds until the mute expires; 0 (the default) mutes forever.
    duration: Option<Value>,
}

/// `POST /api/v1/accounts/{id}/mute`.
pub async fn mute(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(account_id): Path<i64>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    // Mastodon accepts `follow`, `write` or `write:mutes` here.
    if current.require_scope("write:mutes").is_err() {
        current.require_scope("follow")?;
    }
    let params: MuteParams = if body.is_empty() {
        MuteParams::default()
    } else {
        super::params::parse_body(&headers, &body)?
    };
    let hide_notifications = match &params.notifications {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(s)) => truthy(Some(s)),
        _ => true,
    };
    // Mastodon's `params[:duration].to_i`: anything non-numeric is 0.
    let duration_secs = match &params.duration {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    };
    let target = account::find_by_id(&state.pool, account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    actions::mute_account(
        &state,
        &current.account,
        &target,
        hide_notifications,
        duration_secs,
    )
    .await?;
    Ok(Json(
        relationship_json(&state.pool, current.account.id, &target).await?,
    ))
}

/// `POST /api/v1/accounts/{id}/unmute`.
pub async fn unmute(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(account_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    if current.require_scope("write:mutes").is_err() {
        current.require_scope("follow")?;
    }
    let target = account::find_by_id(&state.pool, account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    actions::unmute_account(&state, &current.account, &target).await?;
    Ok(Json(
        relationship_json(&state.pool, current.account.id, &target).await?,
    ))
}

#[derive(Deserialize, Default)]
pub struct NoteParams {
    comment: Option<String>,
}

/// `POST /api/v1/accounts/{id}/note` — sets (or, when blank, clears) the
/// viewer's private note about an account, returning the updated
/// relationship, like Mastodon's `NotesController`.
pub async fn note(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(account_id): Path<i64>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    // Mastodon accepts `write` or `write:accounts` here.
    current.require_scope("write:accounts")?;
    let params: NoteParams = if body.is_empty() {
        NoteParams::default()
    } else {
        super::params::parse_body(&headers, &body)?
    };
    let target = account::find_by_id(&state.pool, account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    // Mastodon's `blank?` test: a whitespace-only comment clears the note,
    // otherwise the value is stored verbatim.
    let comment = params.comment.as_deref().unwrap_or("");
    let stored = if comment.trim().is_empty() {
        ""
    } else {
        comment
    };
    plamenu_db::account_note::set(&state.pool, current.account.id, target.id, stored).await?;
    Ok(Json(
        relationship_json(&state.pool, current.account.id, &target).await?,
    ))
}

/// `POST /api/v1/accounts/{id}/remove_from_followers` — drops the given
/// account from the viewer's followers, returning the updated relationship
/// like Mastodon's `FollowerAccountsController#destroy`.
pub async fn remove_from_followers(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(account_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    // Mastodon accepts `follow`, `write` or `write:follows` here.
    if current.require_scope("write:follows").is_err() {
        current.require_scope("follow")?;
    }
    let follower = account::find_by_id(&state.pool, account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    actions::remove_from_followers(&state, &current.account, &follower).await?;
    Ok(Json(
        relationship_json(&state.pool, current.account.id, &follower).await?,
    ))
}

/// `POST /api/v1/accounts/{id}/endorse` (and its `/pin` alias) — pins an
/// account to the viewer's profile, returning the updated relationship like
/// Mastodon's `AccountPinsController#create`.
pub async fn endorse(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(account_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    // Mastodon accepts `write` or `write:accounts` here.
    current.require_scope("write:accounts")?;
    let target = account::find_by_id(&state.pool, account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    plamenu_db::endorsement::endorse(&state.pool, current.account.id, target.id).await?;
    Ok(Json(
        relationship_json(&state.pool, current.account.id, &target).await?,
    ))
}

/// `POST /api/v1/accounts/{id}/unendorse` (and its `/unpin` alias).
pub async fn unendorse(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(account_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:accounts")?;
    let target = account::find_by_id(&state.pool, account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    plamenu_db::endorsement::unendorse(&state.pool, current.account.id, target.id).await?;
    Ok(Json(
        relationship_json(&state.pool, current.account.id, &target).await?,
    ))
}

/// `GET /api/v1/endorsements` — accounts the viewer has pinned to their own
/// profile, most recently pinned first, like Mastodon's
/// `EndorsementsController`.
pub async fn endorsements_index(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(query): Query<FollowListQuery>,
) -> Result<(HeaderMap, Json<Value>), ApiError> {
    // Mastodon accepts `read` or `read:accounts` here.
    current.require_scope("read:accounts")?;
    let limit = query
        .limit
        .unwrap_or(ACCOUNTS_LIMIT)
        .clamp(1, ACCOUNTS_MAX_LIMIT);
    let entries = plamenu_db::endorsement::list(
        &state.pool,
        current.account.id,
        query.max_id,
        query.since_id,
        limit,
    )
    .await?;
    let pairs: Vec<(i64, i64)> = entries
        .iter()
        .map(|e| (e.id, e.target_account_id))
        .collect();
    render_account_page(
        &state,
        "/api/v1/endorsements",
        limit,
        &pairs,
        Some(current.account.id),
    )
    .await
}

/// `GET /api/v1/accounts/{id}/endorsements` — the accounts pinned to a given
/// profile. Public like Mastodon's `Accounts::EndorsementsController`.
pub async fn account_endorsements(
    State(state): State<AppState>,
    MaybeUser(viewer): MaybeUser,
    Path(account_id): Path<i64>,
    Query(query): Query<FollowListQuery>,
) -> Result<(HeaderMap, Json<Value>), ApiError> {
    if let Some(viewer) = &viewer {
        viewer.require_scope("read:accounts")?;
    }
    account::find_publicly_available_by_id(&state.pool, account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let limit = query
        .limit
        .unwrap_or(ACCOUNTS_LIMIT)
        .clamp(1, ACCOUNTS_MAX_LIMIT);
    let entries =
        plamenu_db::endorsement::list(&state.pool, account_id, query.max_id, query.since_id, limit)
            .await?;
    let pairs: Vec<(i64, i64)> = entries
        .iter()
        .map(|e| (e.id, e.target_account_id))
        .collect();
    let path = format!("/api/v1/accounts/{account_id}/endorsements");
    let viewer_id = viewer.as_ref().map(|v| v.account.id);
    render_account_page(&state, &path, limit, &pairs, viewer_id).await
}

/// `GET /api/v1/blocks` — accounts the user blocks, newest first.
pub async fn blocks_index(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(query): Query<FollowListQuery>,
) -> Result<(HeaderMap, Json<Value>), ApiError> {
    // Mastodon accepts `follow`, `read` or `read:blocks` here.
    if current.require_scope("read:blocks").is_err() {
        current.require_scope("follow")?;
    }
    let limit = query
        .limit
        .unwrap_or(ACCOUNTS_LIMIT)
        .clamp(1, ACCOUNTS_MAX_LIMIT);
    let entries = plamenu_db::block::list(
        &state.pool,
        current.account.id,
        query.max_id,
        query.since_id,
        limit,
    )
    .await?;
    let pairs: Vec<(i64, i64)> = entries
        .iter()
        .map(|e| (e.row_id, e.target_account_id))
        .collect();
    render_account_page(
        &state,
        "/api/v1/blocks",
        limit,
        &pairs,
        Some(current.account.id),
    )
    .await
}

/// `GET /api/v1/mutes` — accounts the user mutes (active mutes only).
pub async fn mutes_index(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(query): Query<FollowListQuery>,
) -> Result<(HeaderMap, Json<Value>), ApiError> {
    // Mastodon accepts `follow`, `read` or `read:mutes` here.
    if current.require_scope("read:mutes").is_err() {
        current.require_scope("follow")?;
    }
    let limit = query
        .limit
        .unwrap_or(ACCOUNTS_LIMIT)
        .clamp(1, ACCOUNTS_MAX_LIMIT);
    let entries = plamenu_db::mute::list(
        &state.pool,
        current.account.id,
        query.max_id,
        query.since_id,
        limit,
    )
    .await?;
    let pairs: Vec<(i64, i64)> = entries
        .iter()
        .map(|e| (e.row_id, e.target_account_id))
        .collect();
    render_account_page(
        &state,
        "/api/v1/mutes",
        limit,
        &pairs,
        Some(current.account.id),
    )
    .await
}

/// `GET /api/v1/follow_requests` — accounts with a pending follow request
/// toward the user, newest first.
pub async fn follow_requests_index(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(query): Query<FollowListQuery>,
) -> Result<(HeaderMap, Json<Value>), ApiError> {
    // Mastodon accepts `follow`, `read` or `read:follows` here.
    if current.require_scope("read:follows").is_err() {
        current.require_scope("follow")?;
    }
    let limit = query
        .limit
        .unwrap_or(ACCOUNTS_LIMIT)
        .clamp(1, ACCOUNTS_MAX_LIMIT);
    let entries = follow::requests_of(
        &state.pool,
        current.account.id,
        query.max_id,
        query.since_id,
        limit,
    )
    .await?;
    let pairs: Vec<(i64, i64)> = entries
        .iter()
        .map(|entry| (entry.follow_id, entry.account_id))
        .collect();
    render_account_page(
        &state,
        "/api/v1/follow_requests",
        limit,
        &pairs,
        Some(current.account.id),
    )
    .await
}

/// `POST /api/v1/follow_requests/{id}/authorize` — `{id}` is the requester's
/// account id, like Mastodon's. A missing request is a 404.
pub async fn follow_requests_authorize(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(account_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    // Mastodon accepts `follow`, `write` or `write:follows` here.
    if current.require_scope("write:follows").is_err() {
        current.require_scope("follow")?;
    }
    let requester = account::find_by_id(&state.pool, account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    actions::authorize_follow_request(&state, &current.account, &requester).await?;
    Ok(Json(
        relationship_json(&state.pool, current.account.id, &requester).await?,
    ))
}

/// `POST /api/v1/follow_requests/{id}/reject`.
pub async fn follow_requests_reject(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(account_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    if current.require_scope("write:follows").is_err() {
        current.require_scope("follow")?;
    }
    let requester = account::find_by_id(&state.pool, account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    actions::reject_follow_request(&state, &current.account, &requester).await?;
    Ok(Json(
        relationship_json(&state.pool, current.account.id, &requester).await?,
    ))
}

/// Renders a `(row id, account id)` page as Account entities with the
/// keyset-pagination Link header over the row ids.
pub(super) async fn render_account_page(
    state: &AppState,
    path: &str,
    limit: i64,
    pairs: &[(i64, i64)],
    viewer: Option<i64>,
) -> Result<(HeaderMap, Json<Value>), ApiError> {
    let ids: Vec<i64> = pairs.iter().map(|(_, account_id)| *account_id).collect();
    let mut listed = account::find_publicly_available_by_ids(&state.pool, &ids).await?;
    // The relationship row's foreign key guarantees its target exists.
    // Publication is separate: an account whose activation was revoked must
    // disappear from every rendered relationship page without adding an N+1.
    listed.retain(|account| !account.suspended());
    listed.sort_by_key(|a| ids.iter().position(|id| *id == a.id));
    let accounts = render_accounts(&state.pool, &state.config.domain, &listed, viewer).await?;
    let mut links = Vec::new();
    if pairs.len() == usize::try_from(limit).unwrap_or(usize::MAX)
        && let Some((last_id, _)) = pairs.last()
    {
        links.push(format!(
            "<https://{}{path}?limit={limit}&max_id={last_id}>; rel=\"next\"",
            state.config.domain
        ));
    }
    if let Some((first_id, _)) = pairs.first() {
        links.push(format!(
            "<https://{}{path}?limit={limit}&since_id={first_id}>; rel=\"prev\"",
            state.config.domain
        ));
    }
    let mut headers = HeaderMap::new();
    if !links.is_empty()
        && let Ok(value) = links.join(", ").parse()
    {
        headers.insert(axum::http::header::LINK, value);
    }
    Ok((headers, Json(Value::Array(accounts))))
}

/// `GET /api/v1/accounts/familiar_followers?id[]=…` — for each requested
/// account, the accounts the viewer follows who also follow it. Unknown ids
/// still get an entry with an empty list, like Mastodon's presenter over
/// `Account.where(id:)`.
pub async fn familiar_followers(
    State(state): State<AppState>,
    current: CurrentUser,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    // Mastodon accepts `read` or `read:follows` here.
    if current.require_scope("read:follows").is_err() {
        current.require_scope("follow")?;
    }
    // Deduplicated, capped, first-seen order. Unknown ids stay in the list so
    // they receive the documented empty entry; the query below simply returns
    // no familiar followers for a target that does not exist, so no separate
    // per-id existence probe is needed.
    let requested = parse_unique_id_batch(query.as_deref(), MAX_ID_BATCH)?;
    let mut by_target: std::collections::HashMap<i64, Vec<i64>> = std::collections::HashMap::new();
    for row in follow::familiar_followers(&state.pool, current.account.id, &requested).await? {
        by_target
            .entry(row.target_id)
            .or_default()
            .push(row.follower_id);
    }
    // Render every distinct familiar follower once, then assemble each
    // target's list from that shared map — no per-(target, follower) query.
    let follower_ids: Vec<i64> = {
        let mut seen = std::collections::HashSet::new();
        by_target
            .values()
            .flatten()
            .copied()
            .filter(|id| seen.insert(*id))
            .collect()
    };
    let follower_accounts = account::find_by_ids(&state.pool, &follower_ids).await?;
    let rendered = render_accounts(
        &state.pool,
        &state.config.domain,
        &follower_accounts,
        Some(current.account.id),
    )
    .await?;
    let by_id: std::collections::HashMap<i64, Value> = follower_accounts
        .iter()
        .map(|a| a.id)
        .zip(rendered)
        .collect();
    let mut results = Vec::with_capacity(requested.len());
    for id in requested {
        let accounts: Vec<Value> = by_target
            .get(&id)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(|follower_id| by_id.get(follower_id).cloned())
            .collect();
        results.push(json!({ "id": id.to_string(), "accounts": accounts }));
    }
    Ok(Json(Value::Array(results)))
}

/// `GET /api/v1/accounts/relationships?id[]=…` — unknown ids are silently
/// dropped, like Mastodon's `Account.where(id:)`.
pub async fn relationships(
    State(state): State<AppState>,
    current: CurrentUser,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    if current.require_scope("read:follows").is_err() {
        current.require_scope("follow")?;
    }
    // Requested ids in request order (Mastodon preserves order and renders a
    // repeated id twice); non-numeric params dropped, total entries capped.
    let ordered_ids = parse_ordered_id_batch(query.as_deref(), MAX_ID_BATCH)?;
    let with_suspended =
        serde_urlencoded::from_str::<Vec<(String, String)>>(query.as_deref().unwrap_or(""))
            .map_err(|e| ApiError::BadRequest(format!("invalid query string: {e}")))?
            .iter()
            .any(|(key, value)| key == "with_suspended" && truthy(Some(value)));
    // Fetch the distinct existing accounts once, then rebuild the ordered list
    // (dropping unknown ids, keeping duplicates) and render in one batch.
    let found = account::find_by_ids(&state.pool, &ordered_ids).await?;
    let by_id: std::collections::HashMap<i64, account::Account> =
        found.into_iter().map(|a| (a.id, a)).collect();
    let targets: Vec<account::Account> = ordered_ids
        .iter()
        .filter_map(|id| by_id.get(id).cloned())
        .filter(|account| with_suspended || !account.suspended())
        .collect();
    let entities = render_relationships(&state.pool, current.account.id, &targets).await?;
    Ok(Json(Value::Array(entities)))
}
