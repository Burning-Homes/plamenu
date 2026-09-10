//! `/api/v1/scheduled_statuses` — listing, inspecting, rescheduling and
//! cancelling statuses queued for future publishing. Creation happens through
//! `POST /api/v1/statuses` with a far-enough-future `scheduled_at`
//! (see `statuses_api::create`); the publish sweeper
//! (`crate::scheduled_status_publish`) replays due rows.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use plamenu_db::media;
use plamenu_db::scheduled_status::{self, NewScheduledStatus};
use serde::Deserialize;
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::params::parse_body;
use crate::actions::{self, PollParams};
use crate::auth::CurrentUser;
use crate::entities::render_scheduled_status;
use crate::error::ApiError;
use crate::state::AppState;

/// Mastodon's `ScheduledStatus::MINIMUM_OFFSET` — a status must be scheduled at
/// least this far ahead, otherwise `POST /statuses` posts it immediately.
pub const MINIMUM_OFFSET: time::Duration = time::Duration::minutes(5);
/// Mastodon's `ScheduledStatus::TOTAL_LIMIT`.
pub const TOTAL_LIMIT: i64 = 300;
/// Mastodon's `ScheduledStatus::DAILY_LIMIT`.
pub const DAILY_LIMIT: i64 = 25;

const DEFAULT_LIMIT: i64 = 20;
const MAX_LIMIT: i64 = 40;

#[derive(Deserialize)]
pub struct IndexQuery {
    max_id: Option<i64>,
    since_id: Option<i64>,
    min_id: Option<i64>,
    limit: Option<i64>,
}

/// `GET /api/v1/scheduled_statuses` — the caller's queue, newest first,
/// keyset-paginated by scheduled-status id.
pub async fn index(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(query): Query<IndexQuery>,
) -> Result<(HeaderMap, Json<Value>), ApiError> {
    current.require_scope("read:statuses")?;
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let rows = scheduled_status::list_for_account(
        &state.pool,
        current.account.id,
        query.max_id,
        query.since_id,
        query.min_id,
        limit,
    )
    .await?;
    let mut entities = Vec::with_capacity(rows.len());
    for row in &rows {
        entities.push(render_scheduled_status(&state.pool, &state.config.domain, row).await?);
    }
    let mut links = Vec::new();
    if rows.len() == usize::try_from(limit).unwrap_or(usize::MAX)
        && let Some(last) = rows.last()
    {
        links.push(format!(
            "<https://{}/api/v1/scheduled_statuses?limit={limit}&max_id={}>; rel=\"next\"",
            state.config.domain, last.id
        ));
    }
    if let Some(first) = rows.first() {
        links.push(format!(
            "<https://{}/api/v1/scheduled_statuses?limit={limit}&min_id={}>; rel=\"prev\"",
            state.config.domain, first.id
        ));
    }
    let mut headers = HeaderMap::new();
    if !links.is_empty()
        && let Ok(value) = links.join(", ").parse()
    {
        headers.insert(axum::http::header::LINK, value);
    }
    Ok((headers, Json(Value::Array(entities))))
}

/// `GET /api/v1/scheduled_statuses/{id}`.
pub async fn show(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:statuses")?;
    let row = scheduled_status::find_for_account(&state.pool, current.account.id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(
        render_scheduled_status(&state.pool, &state.config.domain, &row).await?,
    ))
}

#[derive(Deserialize)]
pub struct UpdateParams {
    scheduled_at: Option<String>,
}

/// `PUT /api/v1/scheduled_statuses/{id}` — Mastodon only permits changing
/// `scheduled_at`, and re-validates the minimum offset.
pub async fn update(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:statuses")?;
    let params: UpdateParams = parse_body(&headers, &body)?;
    let scheduled_at = params
        .scheduled_at
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(parse_scheduled_at)
        .transpose()?
        .ok_or_else(|| {
            ApiError::Unprocessable("Validation failed: Scheduled at can't be blank".into())
        })?;
    validate_future(scheduled_at)?;
    let row =
        scheduled_status::update_scheduled_at(&state.pool, current.account.id, id, scheduled_at)
            .await?
            .ok_or(ApiError::NotFound)?;
    Ok(Json(
        render_scheduled_status(&state.pool, &state.config.domain, &row).await?,
    ))
}

/// `DELETE /api/v1/scheduled_statuses/{id}` — cancels a queued status.
pub async fn destroy(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:statuses")?;
    if scheduled_status::delete(&state.pool, current.account.id, id).await? {
        Ok(Json(json!({})))
    } else {
        Err(ApiError::NotFound)
    }
}

/// Parses an RFC 3339 `scheduled_at`, rejecting malformed values like Mastodon.
pub fn parse_scheduled_at(value: &str) -> Result<OffsetDateTime, ApiError> {
    OffsetDateTime::parse(value, &Rfc3339)
        .map_err(|_| ApiError::Unprocessable("Validation failed: Scheduled at is invalid".into()))
}

/// Mastodon's `validate_future_date`: the time must be more than
/// [`MINIMUM_OFFSET`] ahead of now.
pub fn validate_future(scheduled_at: OffsetDateTime) -> Result<(), ApiError> {
    if scheduled_at <= OffsetDateTime::now_utc() + MINIMUM_OFFSET {
        return Err(ApiError::Unprocessable(
            "Validation failed: Scheduled at must be in the future".into(),
        ));
    }
    Ok(())
}

/// The already-resolved post options for a status being scheduled, mirroring
/// the fields `statuses_api::create` computes for an immediate post.
pub struct ScheduledDraft<'a> {
    pub kind: plamenu_ap::activity::PostKind,
    pub title: Option<&'a str>,
    pub text: &'a str,
    /// The format `text` is authored in (P4), resolved at schedule time like
    /// the quote policy and replayed on publish.
    pub content_type: &'a str,
    pub visibility: &'a str,
    pub in_reply_to_id: Option<i64>,
    pub quoted_status_id: Option<i64>,
    pub media_ids: &'a [i64],
    pub spoiler_text: &'a str,
    pub sensitive: bool,
    pub language: Option<&'a str>,
    /// `None` for the built-in web client (no OAuth application).
    pub application_id: Option<i64>,
    pub poll: Option<&'a PollParams>,
    /// Already-resolved quote-approval bitmap — Mastodon resolves the string
    /// (or the user preference) at schedule time and stores the result.
    pub quote_approval_policy: i32,
}

/// Queues a status for future publishing: validates the minimum offset, the
/// per-account total/daily caps and the poll, stores the row, and reserves any
/// uploaded media from the orphan vacuum. Returns the `ScheduledStatus` entity.
pub async fn create_scheduled(
    state: &AppState,
    account_id: i64,
    scheduled_at: OffsetDateTime,
    draft: ScheduledDraft<'_>,
) -> Result<Value, ApiError> {
    validate_future(scheduled_at)?;
    let article = draft.kind == plamenu_ap::activity::PostKind::Article;
    let title = draft.title.map(str::trim).filter(|t| !t.is_empty());
    if !matches!(
        draft.kind,
        plamenu_ap::activity::PostKind::Note | plamenu_ap::activity::PostKind::Article
    ) {
        return Err(ApiError::Unprocessable(
            "Validation failed: Events can't be scheduled".into(),
        ));
    }
    if article {
        if title.is_none() {
            return Err(ApiError::Unprocessable(
                "Validation failed: An article needs a title".into(),
            ));
        }
        if title.is_some_and(|t| t.chars().count() > 200) {
            return Err(ApiError::Unprocessable(
                "Validation failed: Title is too long (maximum is 200 characters)".into(),
            ));
        }
        if draft.in_reply_to_id.is_some() || draft.poll.is_some() {
            return Err(ApiError::Unprocessable(
                "Validation failed: Articles can't be replies or contain polls".into(),
            ));
        }
    } else if title.is_some() {
        return Err(ApiError::Unprocessable(
            "Validation failed: Notes can't have a title".into(),
        ));
    }
    let limits = state.settings_cache.get(&state.pool).await?;
    actions::ensure_within_character_limit(
        draft.text,
        draft.spoiler_text,
        if article {
            limits.max_characters_long_form
        } else {
            limits.max_characters
        },
    )?;
    if draft.text.is_empty() && draft.media_ids.is_empty() && title.is_none() {
        return Err(ApiError::Unprocessable(
            "Validation failed: Text can't be blank".into(),
        ));
    }
    // Reject a malformed poll now rather than silently dropping it at publish.
    if let Some(poll) = draft.poll {
        actions::validate_poll(poll, limits.poll_max_options)?;
    }
    if scheduled_status::count_total(&state.pool, account_id).await? >= TOTAL_LIMIT {
        return Err(ApiError::Unprocessable(format!(
            "Validation failed: Limit of {TOTAL_LIMIT} scheduled statuses exceeded"
        )));
    }
    // The per-day cap is counted in the owner's own day, so a user east of UTC
    // doesn't hit it mid-afternoon (TZ §4.4). `normalize` keeps the identifier
    // an inventory entry, which is what makes the `AT TIME ZONE` interpolation
    // safe.
    let owner_zone = plamenu_db::user::time_zone_by_account_id(&state.pool, account_id)
        .await?
        .as_deref()
        .and_then(crate::time_zones::normalize)
        .unwrap_or("UTC");
    if scheduled_status::count_on_day(&state.pool, account_id, scheduled_at, owner_zone).await?
        >= DAILY_LIMIT
    {
        return Err(ApiError::Unprocessable(format!(
            "Validation failed: Limit of {DAILY_LIMIT} scheduled statuses per day exceeded"
        )));
    }
    let row = scheduled_status::create(
        &state.pool,
        NewScheduledStatus {
            account_id,
            scheduled_at,
            text: draft.text,
            object_type: if article { "Article" } else { "Note" },
            title,
            content_type: draft.content_type,
            visibility: draft.visibility,
            in_reply_to_id: draft.in_reply_to_id,
            quoted_status_id: draft.quoted_status_id,
            spoiler_text: draft.spoiler_text,
            sensitive: draft.sensitive,
            language: draft.language,
            application_id: draft.application_id,
            media_ids: draft.media_ids,
            poll_options: draft.poll.map(|p| p.options.as_slice()),
            poll_expires_in: draft.poll.and_then(|p| p.expires_in),
            poll_multiple: draft.poll.is_some_and(|p| p.multiple),
            poll_hide_totals: draft.poll.is_some_and(|p| p.hide_totals),
            quote_approval_policy: Some(draft.quote_approval_policy),
        },
    )
    .await?;
    if !draft.media_ids.is_empty() {
        media::bind_to_scheduled(&state.pool, draft.media_ids, row.id, account_id).await?;
    }
    render_scheduled_status(&state.pool, &state.config.domain, &row).await
}
