//! `/api/v1/notifications` and the grouped `/api/v2/notifications` family,
//! plus the read-state endpoints (`unread_count`, `dismiss`, `clear`).

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{HeaderMap, header};
use axum::response::IntoResponse;
use plamenu_db::notification::{
    self, GROUPABLE_KINDS, GroupData, Notification, NotificationFilter, effective_group_key,
};
use plamenu_db::{marker, status};
use serde::Deserialize;
use serde_json::{Value, json};

use super::params::truthy;
use crate::auth::CurrentUser;
use crate::entities::{render_accounts_by_ids, render_notifications, render_statuses, rfc3339};
use crate::error::ApiError;
use crate::state::AppState;

const DEFAULT_LIMIT: i64 = 40;
const MAX_LIMIT: i64 = 80;
const DEFAULT_COUNT_LIMIT: i64 = 100;
const MAX_COUNT_LIMIT: i64 = 1000;

/// Mastodon's `Notification::TYPES` — every type a 4.5 server can emit.
/// Plamenu only generates a subset, but the filtering semantics must match:
/// `types[]` is intersected with this list (so a request for an unknown
/// type yields nothing rather than everything) and the SQL filter is only
/// applied when the requested set is narrower than the whole list.
pub(crate) const TYPES: &[&str] = &[
    "mention",
    "status",
    "reblog",
    "follow",
    "follow_request",
    "favourite",
    "poll",
    "update",
    "severed_relationships",
    "moderation_warning",
    "annual_report",
    "admin.sign_up",
    "admin.report",
    "quote",
    "quoted_update",
    "live",
    "added_to_collection",
    "collection_update",
    // Plamenu also emits Pleroma's reaction kind (P1); listing it makes the
    // type filters and the push-alert whitelist accept it, like Pleroma does.
    "pleroma:emoji_reaction",
    // Event participation (E-track). Mastodon has no event concept, so these are
    // Plamenu extensions; listing them is what makes `types[]` filtering and the
    // push-alert whitelist accept them at all.
    "event.participation",
    "event.accepted",
    "event.rejected",
    "event.changed",
    "event.invite",
];

#[derive(Deserialize)]
pub struct NotificationParams {
    pub max_id: Option<i64>,
    pub since_id: Option<i64>,
    pub min_id: Option<i64>,
    pub limit: Option<i64>,
    pub account_id: Option<i64>,
}

/// The `types[]` / `exclude_types[]` / `grouped_types[]` narrowing,
/// recovered from the raw query string (repeated bracket keys don't fit a
/// serde struct), resolved into the allowed-kind set like Mastodon's
/// `Notification.browserable`.
struct TypeFilter {
    /// `Some` only when narrower than every type; ready for SQL.
    kinds: Option<Vec<String>>,
    /// `grouped_types[]` ∩ groupable kinds, sorted (Mastodon normalizes the
    /// same way); `None` when absent or empty — every groupable kind groups.
    grouped_kinds: Option<Vec<String>>,
    /// `include_filtered` (Mastodon's truthy query param): surface
    /// policy-filtered notifications in the listing.
    include_filtered: bool,
    /// The filter pairs, echoed into pagination links like Mastodon's
    /// `pagination_params`.
    echo: String,
}

impl TypeFilter {
    fn from_raw_query(raw: Option<&str>) -> Self {
        let pairs: Vec<(String, String)> =
            serde_urlencoded::from_str(raw.unwrap_or("")).unwrap_or_default();
        let mut types = Vec::new();
        let mut exclude_types = Vec::new();
        let mut grouped_types = Vec::new();
        let mut include_filtered = false;
        let mut echo_pairs = Vec::new();
        for (key, value) in pairs {
            match key.as_str() {
                "types[]" | "types" => types.push(value.clone()),
                "exclude_types[]" | "exclude_types" => exclude_types.push(value.clone()),
                "grouped_types[]" | "grouped_types" => grouped_types.push(value.clone()),
                "include_filtered" => include_filtered = truthy(Some(&value)),
                "account_id" => {}
                _ => continue,
            }
            echo_pairs.push((key, value));
        }
        let mut requested: Vec<&str> = if types.is_empty() {
            TYPES.to_vec()
        } else {
            TYPES
                .iter()
                .filter(|t| types.iter().any(|x| x == *t))
                .copied()
                .collect()
        };
        requested.retain(|t| !exclude_types.iter().any(|x| x == t));
        let kinds = (requested.len() != TYPES.len())
            .then(|| requested.into_iter().map(str::to_owned).collect());
        let mut grouped: Vec<String> = GROUPABLE_KINDS
            .iter()
            .filter(|k| grouped_types.iter().any(|x| x == *k))
            .map(|k| (*k).to_owned())
            .collect();
        grouped.sort_unstable();
        let echo = serde_urlencoded::to_string(&echo_pairs)
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| format!("&{s}"))
            .unwrap_or_default();
        Self {
            kinds,
            grouped_kinds: (!grouped_types.is_empty()).then_some(grouped),
            include_filtered,
            echo,
        }
    }

    fn db_filter(&self, from_account_id: Option<i64>) -> NotificationFilter<'_> {
        NotificationFilter {
            kinds: self.kinds.as_deref(),
            from_account_id,
            include_filtered: self.include_filtered,
        }
    }
}

/// Mastodon's pagination headers: `next` walks older pages by `max_id`,
/// `prev` newer ones by `min_id`. Active type/sender filters carry over
/// into both links.
fn link_header_for(
    domain: &str,
    path: &str,
    limit: i64,
    echo: &str,
    page: &[Notification],
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let mut parts = Vec::new();
    if let Some(last) = page.last() {
        parts.push(format!(
            "<https://{domain}{path}?limit={limit}&max_id={}{echo}>; rel=\"next\"",
            last.id
        ));
    }
    if let Some(first) = page.first() {
        parts.push(format!(
            "<https://{domain}{path}?limit={limit}&min_id={}{echo}>; rel=\"prev\"",
            first.id
        ));
    }
    if !parts.is_empty()
        && let Ok(value) = parts.join(", ").parse()
    {
        headers.insert(header::LINK, value);
    }
    headers
}

fn link_header(domain: &str, limit: i64, echo: &str, page: &[Notification]) -> HeaderMap {
    link_header_for(domain, "/api/v1/notifications", limit, echo, page)
}

pub async fn index(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(params): Query<NotificationParams>,
    RawQuery(raw): RawQuery,
) -> Result<impl IntoResponse, ApiError> {
    current.require_scope("read:notifications")?;
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let filter = TypeFilter::from_raw_query(raw.as_deref());
    let items = notification::list(
        &state.pool,
        current.account.id,
        params.max_id,
        params.since_id,
        params.min_id,
        filter.db_filter(params.account_id),
        limit,
    )
    .await?;
    let entities = render_notifications(
        &state.pool,
        &state.config.domain,
        &items,
        current.account.id,
    )
    .await?;
    let headers = link_header(&state.config.domain, limit, &filter.echo, &items);
    Ok((headers, Json(Value::Array(entities))))
}

/// `GET /api/v1/notifications/{id}`.
pub async fn show(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:notifications")?;
    let item = notification::find_by_id(&state.pool, current.account.id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let mut entities = render_notifications(
        &state.pool,
        &state.config.domain,
        std::slice::from_ref(&item),
        current.account.id,
    )
    .await?;
    entities.pop().ok_or(ApiError::NotFound).map(Json)
}

#[derive(Deserialize)]
pub struct UnreadCountParams {
    pub limit: Option<i64>,
    pub account_id: Option<i64>,
}

/// `GET /api/v1/notifications/unread_count` — notifications newer than the
/// `notifications` marker, counted up to `limit` like Mastodon. The same
/// type/sender narrowing as the listing applies (Mastodon reuses its
/// `browserable` scope here).
pub async fn unread_count(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(params): Query<UnreadCountParams>,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:notifications")?;
    let limit = params
        .limit
        .unwrap_or(DEFAULT_COUNT_LIMIT)
        .clamp(1, MAX_COUNT_LIMIT);
    let filter = TypeFilter::from_raw_query(raw.as_deref());
    let last_read_id = marker::find(&state.pool, current.user.id, "notifications")
        .await?
        .map(|m| m.last_read_id);
    let count = notification::unread_count(
        &state.pool,
        current.account.id,
        last_read_id,
        filter.db_filter(params.account_id),
        limit,
    )
    .await?;
    Ok(Json(json!({ "count": count })))
}

/// `POST /api/v1/notifications/clear`.
pub async fn clear(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:notifications")?;
    notification::clear(&state.pool, current.account.id).await?;
    Ok(Json(json!({})))
}

/// `POST /api/v1/notifications/{id}/dismiss`.
pub async fn dismiss(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:notifications")?;
    if !notification::dismiss(&state.pool, current.account.id, id).await? {
        return Err(ApiError::NotFound);
    }
    Ok(Json(json!({})))
}

// ---------------------------------------------------------------------------
// Grouped notifications — /api/v2/notifications

/// Kinds whose group entity carries a `status_id` — Mastodon's
/// `NotificationGroupSerializer#status_type?`.
const STATUS_ID_KINDS: &[&str] = &[
    "favourite",
    "reblog",
    "status",
    "mention",
    "poll",
    "update",
    "quote",
    "quoted_update",
    "live",
    // Every event notification points at the event itself — that is the only
    // thing the recipient can act on.
    "event.participation",
    "event.accepted",
    "event.rejected",
    "event.changed",
    "event.invite",
];

/// Kinds whose group entity carries a `collection` — Mastodon's
/// `NotificationGroupSerializer#collection_type?`.
const COLLECTION_ID_KINDS: &[&str] = &["added_to_collection", "collection_update"];

#[derive(Deserialize)]
pub struct GroupedParams {
    pub max_id: Option<i64>,
    pub since_id: Option<i64>,
    pub min_id: Option<i64>,
    pub limit: Option<i64>,
    pub expand_accounts: Option<String>,
}

/// The id window of the current page: group aggregates are capped to it so
/// a page stays self-consistent while newer notifications keep arriving
/// (Mastodon's `pagination_range`).
struct PageWindow {
    lower: i64,
    /// Inclusive; `None` = unbounded (the page touches the newest row).
    upper: Option<i64>,
}

/// Renders Mastodon's dedup-grouped payload: top-level `accounts` (and
/// `partial_accounts` under `expand_accounts=partial_avatars`), `statuses`,
/// and one `notification_groups` entry per page row. `window` carries the
/// page bounds of the index listing; `None` (the show endpoint) renders
/// group totals without page fields.
#[allow(clippy::too_many_lines)]
async fn render_grouped_page(
    state: &AppState,
    viewer: i64,
    page: &[Notification],
    grouped_kinds: Option<&[String]>,
    window: Option<PageWindow>,
    partial_avatars: bool,
) -> Result<Value, ApiError> {
    // Aggregates for the rows that group; ungrouped rows synthesize theirs.
    let keys: Vec<String> = page
        .iter()
        .filter(|n| n.group_key.as_deref() == Some(effective_group_key(n, grouped_kinds).as_str()))
        .filter_map(|n| n.group_key.clone())
        .collect();
    let (lower, upper) = window.as_ref().map_or((0, None), |w| (w.lower, w.upper));
    let data: HashMap<String, GroupData> =
        notification::groups_data(&state.pool, viewer, &keys, lower, upper)
            .await?
            .into_iter()
            .map(|d| (d.group_key.clone(), d))
            .collect();

    // Target statuses, deduplicated in page order.
    let mut status_ids = Vec::new();
    let mut seen_statuses = std::collections::HashSet::new();
    for item in page {
        if STATUS_ID_KINDS.contains(&item.kind.as_str())
            && let Some(status_id) = item.status_id
            && seen_statuses.insert(status_id)
        {
            status_ids.push(status_id);
        }
    }
    // One query for the page's target statuses, restored to page order (rows
    // deleted since the notification are simply absent, like the per-id loop
    // this replaced).
    let mut found_by_id: HashMap<i64, plamenu_db::status::Status> =
        status::find_by_ids(&state.pool, &status_ids)
            .await?
            .into_iter()
            .map(|s| (s.id, s))
            .collect();
    let statuses: Vec<plamenu_db::status::Status> = status_ids
        .iter()
        .filter_map(|id| found_by_id.remove(id))
        .collect();
    let found_statuses: std::collections::HashSet<i64> = statuses.iter().map(|s| s.id).collect();
    let rendered_statuses =
        render_statuses(&state.pool, &state.config.domain, &statuses, Some(viewer)).await?;

    // Target collections for `added_to_collection`/`collection_update` groups —
    // Mastodon inlines the full `CollectionSerializer` (incl. `items`, the data
    // a client needs for the "remove me" action) into each such group. Batched
    // over the page's distinct collection ids (`collections_json_map`).
    let collection_ids: Vec<i64> = {
        let mut ids: Vec<i64> = page
            .iter()
            .filter(|i| COLLECTION_ID_KINDS.contains(&i.kind.as_str()))
            .filter_map(|i| i.collection_id)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };
    let rendered_collections = crate::collections::collections_json_map(
        &state.pool,
        &state.config.domain,
        &collection_ids,
        Some(viewer),
    )
    .await?;

    // Embedded strikes for `moderation_warning` groups (never grouped, so one
    // per row at most), likewise batched over the page's distinct strike ids.
    let warning_ids: Vec<i64> = {
        let mut ids: Vec<i64> = page
            .iter()
            .filter(|i| i.kind == "moderation_warning")
            .filter_map(|i| i.account_warning_id)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };
    let rendered_warnings = crate::entities::account_warnings_json_map(
        &state.pool,
        &state.config.domain,
        &warning_ids,
        Some(viewer),
    )
    .await?;

    // Sample accounts: under `partial_avatars` only each group's most
    // recent sender gets the full entity; the rest become partial entities
    // (unless they are a full entity elsewhere) — Mastodon's presenter.
    let samples_of = |item: &Notification| -> Vec<i64> {
        data.get(&effective_group_key(item, grouped_kinds))
            .map_or_else(
                || vec![item.from_account_id],
                |d| d.sample_account_ids.clone(),
            )
    };
    let mut full_ids: Vec<i64> = Vec::new();
    let mut seen_accounts = std::collections::HashSet::new();
    for item in page {
        let samples = samples_of(item);
        let full = if partial_avatars {
            &samples[..samples.len().min(1)]
        } else {
            &samples[..]
        };
        full_ids.extend(full.iter().filter(|id| seen_accounts.insert(**id)));
    }
    let mut partial_ids: Vec<i64> = Vec::new();
    if partial_avatars {
        for item in page {
            partial_ids.extend(
                samples_of(item)
                    .iter()
                    .skip(1)
                    .filter(|id| seen_accounts.insert(**id)),
            );
        }
    }
    let accounts =
        render_accounts_by_ids(&state.pool, &state.config.domain, &full_ids, Some(viewer)).await?;
    let partial_accounts: Vec<Value> = render_accounts_by_ids(
        &state.pool,
        &state.config.domain,
        &partial_ids,
        Some(viewer),
    )
    .await?
    .iter()
    .map(partial_account_json)
    .collect();

    let mut groups = Vec::with_capacity(page.len());
    for item in page {
        let key = effective_group_key(item, grouped_kinds);
        groups.push(group_entity(
            item,
            &key,
            data.get(&key),
            window.is_some(),
            &found_statuses,
            &rendered_collections,
            &rendered_warnings,
        )?);
    }

    let mut body = json!({
        "accounts": accounts,
        "statuses": rendered_statuses,
        "notification_groups": groups,
    });
    if partial_avatars {
        body["partial_accounts"] = Value::Array(partial_accounts);
    }
    Ok(body)
}

/// Mastodon's `PartialAccountSerializer` field set, projected out of the
/// full Account entity.
fn partial_account_json(full: &Value) -> Value {
    let field = |name: &str| full.get(name).cloned().unwrap_or(Value::Null);
    json!({
        "id": field("id"),
        "acct": field("acct"),
        "locked": field("locked"),
        "bot": field("bot"),
        "url": field("url"),
        "avatar": field("avatar"),
        "avatar_static": field("avatar_static"),
    })
}

/// One `NotificationGroup` entity. `data` is `None` for rows rendered
/// ungrouped (synthetic single-member group).
fn group_entity(
    head: &Notification,
    key: &str,
    data: Option<&GroupData>,
    include_page_fields: bool,
    found_statuses: &std::collections::HashSet<i64>,
    collections: &HashMap<i64, Value>,
    warnings: &HashMap<i64, Value>,
) -> Result<Value, ApiError> {
    let (count, most_recent, samples, min_id, latest_at) = match data {
        Some(d) => (
            d.notifications_count,
            d.most_recent_id,
            d.sample_account_ids.clone(),
            d.min_id,
            d.latest_at,
        ),
        None => (
            1,
            head.id,
            vec![head.from_account_id],
            Some(head.id),
            head.created_at,
        ),
    };
    let mut entity = json!({
        "group_key": key,
        "notifications_count": count,
        "type": head.kind,
        "most_recent_notification_id": most_recent,
        "sample_account_ids": samples.iter().map(ToString::to_string).collect::<Vec<_>>(),
    });
    if include_page_fields {
        entity["page_min_id"] = min_id.map_or(Value::Null, |id| Value::String(id.to_string()));
        entity["page_max_id"] = Value::String(most_recent.to_string());
        entity["latest_page_notification_at"] = Value::String(rfc3339(latest_at)?);
    }
    if STATUS_ID_KINDS.contains(&head.kind.as_str()) {
        entity["status_id"] = head
            .status_id
            .filter(|id| found_statuses.contains(id))
            .map_or(Value::Null, |id| Value::String(id.to_string()));
    }
    if COLLECTION_ID_KINDS.contains(&head.kind.as_str()) {
        entity["collection"] = head
            .collection_id
            .and_then(|id| collections.get(&id).cloned())
            .unwrap_or(Value::Null);
    }
    if head.kind == "moderation_warning" {
        entity["moderation_warning"] = head
            .account_warning_id
            .and_then(|id| warnings.get(&id).cloned())
            .unwrap_or(Value::Null);
    }
    Ok(entity)
}

/// `GET /api/v2/notifications` — at most one entry per group on each page.
pub async fn index_v2(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(params): Query<GroupedParams>,
    RawQuery(raw): RawQuery,
) -> Result<impl IntoResponse, ApiError> {
    current.require_scope("read:notifications")?;
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let partial_avatars = match params.expand_accounts.as_deref() {
        None | Some("full") => false,
        Some("partial_avatars") => true,
        Some(other) => {
            return Err(ApiError::BadRequest(format!(
                "Invalid value for 'expand_accounts': '{other}', \
                 allowed values are 'full' and 'partial_avatars'"
            )));
        }
    };
    let filter = TypeFilter::from_raw_query(raw.as_deref());
    let grouped_kinds = filter.grouped_kinds.as_deref();
    // The v2 listing has no sender narrowing (Mastodon ignores account_id).
    let page = if let Some(min_id) = params.min_id {
        notification::list_grouped_above(
            &state.pool,
            current.account.id,
            min_id,
            params.max_id,
            filter.db_filter(None),
            grouped_kinds,
            limit,
        )
        .await?
    } else {
        notification::list_grouped(
            &state.pool,
            current.account.id,
            params.max_id,
            params.since_id,
            filter.db_filter(None),
            grouped_kinds,
            limit,
        )
        .await?
    };

    // A full page is windowed to its own ids. An incomplete page is the
    // edge of the listing, so the window stretches to the request bound
    // (Mastodon's `load_grouped_notifications`).
    let newest = page.first().map(|n| n.id);
    let oldest = page.last().map_or(0, |n| n.id);
    let window = if page.len() == usize::try_from(limit).unwrap_or(usize::MAX) {
        PageWindow {
            lower: oldest,
            upper: newest,
        }
    } else if params.min_id.is_some() {
        PageWindow {
            lower: oldest,
            upper: params.max_id.map(|max_id| max_id - 1),
        }
    } else {
        PageWindow {
            lower: params.since_id.map_or(0, |since_id| since_id + 1),
            upper: newest,
        }
    };

    let body = render_grouped_page(
        &state,
        current.account.id,
        &page,
        grouped_kinds,
        Some(window),
        partial_avatars,
    )
    .await?;
    let headers = link_header_for(
        &state.config.domain,
        "/api/v2/notifications",
        limit,
        &filter.echo,
        &page,
    );
    Ok((headers, Json(body)))
}

/// `GET /api/v2/notifications/{group_key}` — one group, totals over its
/// whole history, no page fields.
pub async fn show_v2(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(group_key): Path<String>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:notifications")?;
    let head = notification::find_group_head(&state.pool, current.account.id, &group_key)
        .await?
        .ok_or(ApiError::NotFound)?;
    let body = render_grouped_page(
        &state,
        current.account.id,
        std::slice::from_ref(&head),
        None,
        None,
        false,
    )
    .await?;
    Ok(Json(body))
}

/// `GET /api/v2/notifications/unread_count` — count of unread *groups*
/// above the `notifications` marker, capped like the v1 count.
pub async fn unread_count_v2(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(params): Query<UnreadCountParams>,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:notifications")?;
    let limit = params
        .limit
        .unwrap_or(DEFAULT_COUNT_LIMIT)
        .clamp(1, MAX_COUNT_LIMIT);
    let filter = TypeFilter::from_raw_query(raw.as_deref());
    let last_read_id = marker::find(&state.pool, current.user.id, "notifications")
        .await?
        .map_or(0, |m| m.last_read_id);
    let groups = notification::list_grouped_above(
        &state.pool,
        current.account.id,
        last_read_id,
        None,
        filter.db_filter(None),
        filter.grouped_kinds.as_deref(),
        limit,
    )
    .await?;
    Ok(Json(json!({ "count": groups.len() })))
}

/// `POST /api/v2/notifications/{group_key}/dismiss` — removes the whole
/// group; dismissing a missing group is a no-op, like Mastodon.
pub async fn dismiss_v2(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(group_key): Path<String>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:notifications")?;
    notification::dismiss_group(&state.pool, current.account.id, &group_key).await?;
    Ok(Json(json!({})))
}

const DEFAULT_ACCOUNTS_LIMIT: i64 = 40;
const MAX_ACCOUNTS_LIMIT: i64 = 80;

/// `GET /api/v2/notifications/{group_key}/accounts` — every sender in a
/// group, paginated by notification id. Synthetic `ungrouped-…` keys list
/// nothing (Mastodon queries the raw column here too).
pub async fn group_accounts_v2(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(group_key): Path<String>,
    Query(params): Query<NotificationParams>,
) -> Result<impl IntoResponse, ApiError> {
    current.require_scope("read:notifications")?;
    let limit = params
        .limit
        .unwrap_or(DEFAULT_ACCOUNTS_LIMIT)
        .clamp(1, MAX_ACCOUNTS_LIMIT);
    let members = notification::group_members(
        &state.pool,
        current.account.id,
        &group_key,
        params.max_id,
        params.since_id,
        limit,
    )
    .await?;
    let member_ids: Vec<i64> = members.iter().map(|m| m.from_account_id).collect();
    let accounts = render_accounts_by_ids(
        &state.pool,
        &state.config.domain,
        &member_ids,
        Some(current.account.id),
    )
    .await?;

    let domain = &state.config.domain;
    let path = format!("/api/v2/notifications/{group_key}/accounts");
    let mut headers = HeaderMap::new();
    let mut parts = Vec::new();
    // `next` only when the page is full (Mastodon's `records_continue?`).
    if members.len() == usize::try_from(limit).unwrap_or(usize::MAX)
        && let Some(last) = members.last()
    {
        parts.push(format!(
            "<https://{domain}{path}?limit={limit}&max_id={}>; rel=\"next\"",
            last.id
        ));
    }
    if let Some(first) = members.first() {
        parts.push(format!(
            "<https://{domain}{path}?limit={limit}&min_id={}>; rel=\"prev\"",
            first.id
        ));
    }
    if !parts.is_empty()
        && let Ok(value) = parts.join(", ").parse()
    {
        headers.insert(header::LINK, value);
    }
    Ok((headers, Json(Value::Array(accounts))))
}
