//! Trending hashtags: the public `GET /api/v1/trends/tags` and the moderator
//! surface (`GET/PUT /api/v1/admin/tags`, `GET /api/v1/admin/trends/tags` +
//! approve/reject). Scores are maintained out of band by [`crate::trends`]; these
//! handlers read the ranked `tag_trends` and drive the tag registry.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, header};
use plamenu_db::preview_card::PreviewCard;
use plamenu_db::role::permission;
use plamenu_db::status::Status;
use plamenu_db::{
    preview_card, preview_card_provider, preview_card_trend, status, status_trend, tag, tag_trend,
};
use serde::Deserialize;
use serde_json::Value;

use crate::auth::{AdminUser, MaybeUser};
use crate::entities::{
    self, admin_tag_json, admin_tag_json_page, preview_card_provider_json, render_statuses,
    trends_link_json,
};
use crate::error::ApiError;
use crate::remote::host_of;
use crate::state::AppState;

/// Mastodon's `DEFAULT_TAGS_LIMIT` for the public trends listing.
const DEFAULT_TAGS_LIMIT: i64 = 10;
/// Mastodon's `DEFAULT_STATUSES_LIMIT`.
const DEFAULT_STATUSES_LIMIT: i64 = 20;
/// Mastodon's `DEFAULT_LINKS_LIMIT`.
const DEFAULT_LINKS_LIMIT: i64 = 10;
/// Mastodon's admin tag / provider page size.
const ADMIN_TAGS_LIMIT: i64 = 100;

#[derive(Deserialize)]
pub struct TrendsQuery {
    limit: Option<i64>,
    offset: Option<i64>,
}

/// `GET /api/v1/trends/tags` (and the deprecated `/api/v1/trends` alias) — the
/// allowed trending tags, highest score first, offset-paginated. Optional auth;
/// `following`/`featuring` are resolved for a signed-in viewer. Returns `[]`
/// (not a 404) when the operator has trends disabled, like Mastodon.
pub async fn tags_index(
    State(state): State<AppState>,
    MaybeUser(viewer): MaybeUser,
    Query(query): Query<TrendsQuery>,
) -> Result<(HeaderMap, Json<Value>), ApiError> {
    let settings = state.settings_cache.get(&state.pool).await?;
    let limit = query
        .limit
        .unwrap_or(DEFAULT_TAGS_LIMIT)
        .clamp(1, DEFAULT_TAGS_LIMIT * 2);
    let offset = query.offset.unwrap_or(0).max(0);

    let mut headers = HeaderMap::new();
    if !settings.trends_enabled {
        return Ok((headers, Json(Value::Array(Vec::new()))));
    }

    let tags = tag_trend::allowed(&state.pool, limit, offset).await?;
    let (following, featuring) = match &viewer {
        Some(user) => {
            let ids: Vec<i64> = tags.iter().map(|t| t.id).collect();
            (
                tag::followed_ids(&state.pool, user.account.id, &ids).await?,
                plamenu_db::featured_tag::featured_ids(&state.pool, user.account.id, &ids).await?,
            )
        }
        None => (
            std::collections::HashSet::new(),
            std::collections::HashSet::new(),
        ),
    };
    let entities = entities::tag_json_page(
        &state.pool,
        &state.config.domain,
        &tags,
        &following,
        &featuring,
        viewer.is_some(),
    )
    .await?;

    let mut links = Vec::new();
    if i64::try_from(entities.len()).unwrap_or(0) == limit {
        links.push(format!(
            "<https://{}/api/v1/trends/tags?limit={limit}&offset={}>; rel=\"next\"",
            state.config.domain,
            offset + limit
        ));
    }
    if offset > 0 {
        links.push(format!(
            "<https://{}/api/v1/trends/tags?limit={limit}&offset={}>; rel=\"prev\"",
            state.config.domain,
            (offset - limit).max(0)
        ));
    }
    if !links.is_empty()
        && let Ok(value) = links.join(", ").parse()
    {
        headers.insert(header::LINK, value);
    }
    Ok((headers, Json(Value::Array(entities))))
}

/// `GET /api/v1/trends/statuses` — the allowed trending statuses, highest score
/// first, offset-paginated. Optional auth; hidden authors are filtered for a
/// signed-in viewer. Serves `[]` when trends are disabled, like Mastodon.
pub async fn statuses_index(
    State(state): State<AppState>,
    MaybeUser(viewer): MaybeUser,
    Query(query): Query<TrendsQuery>,
) -> Result<(HeaderMap, Json<Value>), ApiError> {
    let settings = state.settings_cache.get(&state.pool).await?;
    let limit = query
        .limit
        .unwrap_or(DEFAULT_STATUSES_LIMIT)
        .clamp(1, DEFAULT_STATUSES_LIMIT * 2);
    let offset = query.offset.unwrap_or(0).max(0);

    let mut headers = HeaderMap::new();
    if !settings.trends_enabled {
        return Ok((headers, Json(Value::Array(Vec::new()))));
    }

    let viewer_id = viewer.as_ref().map(|user| user.account.id);
    let statuses = status_trend::allowed(&state.pool, viewer_id, limit, offset).await?;
    let entities = render_statuses(&state.pool, &state.config.domain, &statuses, viewer_id).await?;

    let mut links = Vec::new();
    if i64::try_from(entities.len()).unwrap_or(0) == limit {
        links.push(format!(
            "<https://{}/api/v1/trends/statuses?limit={limit}&offset={}>; rel=\"next\"",
            state.config.domain,
            offset + limit
        ));
    }
    if offset > 0 {
        links.push(format!(
            "<https://{}/api/v1/trends/statuses?limit={limit}&offset={}>; rel=\"prev\"",
            state.config.domain,
            (offset - limit).max(0)
        ));
    }
    if !links.is_empty()
        && let Ok(value) = links.join(", ").parse()
    {
        headers.insert(header::LINK, value);
    }
    Ok((headers, Json(Value::Array(entities))))
}

/// `GET /api/v1/admin/trends/statuses` — all trending statuses (allowed or
/// pending), highest score first, as `Admin::Trends::Status` entities.
pub async fn admin_trends_statuses_index(
    State(state): State<AppState>,
    admin: AdminUser,
    Query(query): Query<TrendsQuery>,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_TAXONOMIES, false, "statuses")?;
    let limit = query
        .limit
        .unwrap_or(DEFAULT_STATUSES_LIMIT)
        .clamp(1, DEFAULT_STATUSES_LIMIT * 2);
    let offset = query.offset.unwrap_or(0).max(0);
    let statuses = status_trend::all_admin(&state.pool, limit, offset).await?;
    let entities = admin_trend_status_entities(&state, &admin, &statuses).await?;
    Ok(Json(Value::Array(entities)))
}

/// `POST /api/v1/admin/trends/statuses/{id}/approve`.
pub async fn admin_trends_statuses_approve(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    review_status(&state, &admin, id, true).await
}

/// `POST /api/v1/admin/trends/statuses/{id}/reject`.
pub async fn admin_trends_statuses_reject(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    review_status(&state, &admin, id, false).await
}

async fn review_status(
    state: &AppState,
    admin: &AdminUser,
    id: i64,
    trendable: bool,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_TAXONOMIES, true, "statuses")?;
    if !status_trend::set_trendable(&state.pool, id, trendable).await? {
        return Err(ApiError::NotFound);
    }
    let status = status::find_by_id(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let mut entities =
        admin_trend_status_entities(state, admin, std::slice::from_ref(&status)).await?;
    Ok(Json(entities.pop().unwrap_or(Value::Null)))
}

/// Renders a batch of statuses as `Admin::Trends::Status` entities — the Status
/// entity plus `requires_review` (Plamenu: the status's `trendable` is unset).
async fn admin_trend_status_entities(
    state: &AppState,
    admin: &AdminUser,
    statuses: &[Status],
) -> Result<Vec<Value>, ApiError> {
    let ids: Vec<i64> = statuses.iter().map(|s| s.id).collect();
    let unreviewed = status_trend::unreviewed_ids(&state.pool, &ids).await?;
    let mut entities = render_statuses(
        &state.pool,
        &state.config.domain,
        statuses,
        Some(admin.current.account.id),
    )
    .await?;
    for (entity, status) in entities.iter_mut().zip(statuses) {
        entity["requires_review"] = Value::Bool(unreviewed.contains(&status.id));
    }
    Ok(entities)
}

#[derive(Deserialize)]
pub struct LinkTimelineQuery {
    url: Option<String>,
    max_id: Option<i64>,
    limit: Option<i64>,
}

/// `GET /api/v1/timelines/link?url=` — public statuses linking to an
/// allowed-trending card, newest first (Mastodon's link timeline). Optional
/// auth; hidden authors are filtered for a signed-in viewer.
pub async fn link_timeline(
    State(state): State<AppState>,
    MaybeUser(viewer): MaybeUser,
    Query(query): Query<LinkTimelineQuery>,
) -> Result<(HeaderMap, Json<Value>), ApiError> {
    let Some(url) = query.url.filter(|u| !u.is_empty()) else {
        return Err(ApiError::BadRequest("url is required".into()));
    };
    let limit = query.limit.unwrap_or(DEFAULT_STATUSES_LIMIT).clamp(1, 40);
    let viewer_id = viewer.as_ref().map(|user| user.account.id);
    let statuses =
        preview_card_trend::link_timeline(&state.pool, &url, viewer_id, query.max_id, limit)
            .await?;
    let entities = render_statuses(&state.pool, &state.config.domain, &statuses, viewer_id).await?;

    let mut headers = HeaderMap::new();
    if statuses.len() == usize::try_from(limit).unwrap_or(usize::MAX)
        && let Some(last) = statuses.last()
    {
        let encoded_url = urlencoding_encode(&url);
        if let Ok(value) = format!(
            "<https://{}/api/v1/timelines/link?url={encoded_url}&limit={limit}&max_id={}>; rel=\"next\"",
            state.config.domain, last.id
        )
        .parse()
        {
            headers.insert(header::LINK, value);
        }
    }
    Ok((headers, Json(Value::Array(entities))))
}

/// Minimal percent-encoding for a URL placed in a Link header query string.
fn urlencoding_encode(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push('%');
                out.push(HEX[(byte >> 4) as usize] as char);
                out.push(HEX[(byte & 0x0f) as usize] as char);
            }
        }
    }
    out
}

/// The inclusive UTC window backing a link's 7-day usage history.
fn link_history_window() -> (time::Date, time::Date) {
    let today = time::OffsetDateTime::now_utc().date();
    (today - time::Duration::days(6), today)
}

/// `GET /api/v1/trends/links` — allowed trending links (preview cards) with
/// their 7-day usage history, highest score first. Serves `[]` when trends are
/// disabled, like Mastodon.
pub async fn links_index(
    State(state): State<AppState>,
    MaybeUser(_viewer): MaybeUser,
    Query(query): Query<TrendsQuery>,
) -> Result<(HeaderMap, Json<Value>), ApiError> {
    let settings = state.settings_cache.get(&state.pool).await?;
    let limit = query
        .limit
        .unwrap_or(DEFAULT_LINKS_LIMIT)
        .clamp(1, DEFAULT_LINKS_LIMIT * 2);
    let offset = query.offset.unwrap_or(0).max(0);

    let mut headers = HeaderMap::new();
    if !settings.trends_enabled {
        return Ok((headers, Json(Value::Array(Vec::new()))));
    }

    let cards = preview_card_trend::allowed(&state.pool, limit, offset).await?;
    let (from, today) = link_history_window();
    let ids: Vec<i64> = cards.iter().map(|c| c.id).collect();
    let histories = preview_card_trend::history_batch(&state.pool, &ids, from).await?;
    let empty: Vec<preview_card_trend::DayCount> = Vec::new();
    let mut entities = Vec::with_capacity(cards.len());
    for card in &cards {
        let counts = histories.get(&card.id).unwrap_or(&empty);
        entities.push(trends_link_json(
            &state.config.domain,
            card,
            counts,
            today,
            None,
        )?);
    }

    let mut links = Vec::new();
    if i64::try_from(entities.len()).unwrap_or(0) == limit {
        links.push(format!(
            "<https://{}/api/v1/trends/links?limit={limit}&offset={}>; rel=\"next\"",
            state.config.domain,
            offset + limit
        ));
    }
    if offset > 0 {
        links.push(format!(
            "<https://{}/api/v1/trends/links?limit={limit}&offset={}>; rel=\"prev\"",
            state.config.domain,
            (offset - limit).max(0)
        ));
    }
    if !links.is_empty()
        && let Ok(value) = links.join(", ").parse()
    {
        headers.insert(header::LINK, value);
    }
    Ok((headers, Json(Value::Array(entities))))
}

/// `GET /api/v1/admin/trends/links` — all trending links (allowed or pending) as
/// `Admin::Trends::Link` entities.
pub async fn admin_trends_links_index(
    State(state): State<AppState>,
    admin: AdminUser,
    Query(query): Query<TrendsQuery>,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_TAXONOMIES, false, "links")?;
    let limit = query
        .limit
        .unwrap_or(DEFAULT_LINKS_LIMIT)
        .clamp(1, DEFAULT_LINKS_LIMIT * 2);
    let offset = query.offset.unwrap_or(0).max(0);
    let cards = preview_card_trend::all_admin(&state.pool, limit, offset).await?;
    let entities = admin_link_entities(&state, &cards).await?;
    Ok(Json(Value::Array(entities)))
}

/// `POST /api/v1/admin/trends/links/{id}/approve`.
pub async fn admin_trends_links_approve(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    review_link(&state, &admin, id, true).await
}

/// `POST /api/v1/admin/trends/links/{id}/reject`.
pub async fn admin_trends_links_reject(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    review_link(&state, &admin, id, false).await
}

async fn review_link(
    state: &AppState,
    admin: &AdminUser,
    id: i64,
    trendable: bool,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_TAXONOMIES, true, "links")?;
    if !preview_card_trend::set_trendable(&state.pool, id, trendable).await? {
        return Err(ApiError::NotFound);
    }
    let card = preview_card::find_by_id(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let mut entities = admin_link_entities(state, std::slice::from_ref(&card)).await?;
    Ok(Json(entities.pop().unwrap_or(Value::Null)))
}

/// Renders a batch of cards as `Admin::Trends::Link` entities. A card
/// `requires_review` until it has its own `trendable` override or its publisher
/// domain has been reviewed.
async fn admin_link_entities(
    state: &AppState,
    cards: &[PreviewCard],
) -> Result<Vec<Value>, ApiError> {
    let (from, today) = link_history_window();
    let ids: Vec<i64> = cards.iter().map(|c| c.id).collect();
    let histories = preview_card_trend::history_batch(&state.pool, &ids, from).await?;
    let reviewed = preview_card_provider::reviewed_domains(&state.pool).await?;
    let empty: Vec<preview_card_trend::DayCount> = Vec::new();
    let mut out = Vec::with_capacity(cards.len());
    for card in cards {
        let counts = histories.get(&card.id).unwrap_or(&empty);
        let card_trendable = preview_card_trend::trendable_of(&state.pool, card.id)
            .await?
            .flatten();
        let domain_reviewed = host_of(&card.url).is_some_and(|host| reviewed.contains(host));
        let requires_review = card_trendable.is_none() && !domain_reviewed;
        out.push(trends_link_json(
            &state.config.domain,
            card,
            counts,
            today,
            Some(requires_review),
        )?);
    }
    Ok(out)
}

/// `GET /api/v1/admin/trends/links/publishers` — the link publishers
/// (`preview_card_providers`), id-keyset paginated.
pub async fn admin_link_publishers_index(
    State(state): State<AppState>,
    admin: AdminUser,
    Query(query): Query<AdminTagsQuery>,
) -> Result<(HeaderMap, Json<Value>), ApiError> {
    admin.require_resource(permission::MANAGE_TAXONOMIES, false, "links")?;
    let limit = query
        .limit
        .unwrap_or(ADMIN_TAGS_LIMIT)
        .clamp(1, ADMIN_TAGS_LIMIT);
    let providers = preview_card_provider::list(
        &state.pool,
        query.max_id,
        query.since_id,
        query.min_id,
        limit,
    )
    .await?;
    let mut entities = Vec::with_capacity(providers.len());
    for provider in &providers {
        entities.push(preview_card_provider_json(provider)?);
    }

    let mut headers = HeaderMap::new();
    let mut links = Vec::new();
    let base = format!(
        "https://{}/api/v1/admin/trends/links/publishers",
        state.config.domain
    );
    if i64::try_from(providers.len()).unwrap_or(0) == limit
        && let Some(last) = providers.last()
    {
        links.push(format!(
            "<{base}?limit={limit}&max_id={}>; rel=\"next\"",
            last.id
        ));
    }
    if let Some(first) = providers.first() {
        links.push(format!(
            "<{base}?limit={limit}&min_id={}>; rel=\"prev\"",
            first.id
        ));
    }
    if !links.is_empty()
        && let Ok(value) = links.join(", ").parse()
    {
        headers.insert(header::LINK, value);
    }
    Ok((headers, Json(Value::Array(entities))))
}

/// `POST /api/v1/admin/trends/links/publishers/{id}/approve`.
pub async fn admin_link_publishers_approve(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    review_publisher(&state, &admin, id, true).await
}

/// `POST /api/v1/admin/trends/links/publishers/{id}/reject`.
pub async fn admin_link_publishers_reject(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    review_publisher(&state, &admin, id, false).await
}

async fn review_publisher(
    state: &AppState,
    admin: &AdminUser,
    id: i64,
    trendable: bool,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_TAXONOMIES, true, "links")?;
    let provider = preview_card_provider::set_trendable(
        &state.pool,
        id,
        trendable,
        time::OffsetDateTime::now_utc(),
    )
    .await?
    .ok_or(ApiError::NotFound)?;
    Ok(Json(preview_card_provider_json(&provider)?))
}

#[derive(Deserialize)]
pub struct AdminTagsQuery {
    max_id: Option<i64>,
    since_id: Option<i64>,
    min_id: Option<i64>,
    limit: Option<i64>,
}

/// `GET /api/v1/admin/tags` — every tag with its moderation registry, id-keyset
/// paginated (Mastodon's `Admin::TagsController#index`).
pub async fn admin_tags_index(
    State(state): State<AppState>,
    admin: AdminUser,
    Query(query): Query<AdminTagsQuery>,
) -> Result<(HeaderMap, Json<Value>), ApiError> {
    admin.require_resource(permission::MANAGE_TAXONOMIES, false, "tags")?;
    let limit = query
        .limit
        .unwrap_or(ADMIN_TAGS_LIMIT)
        .clamp(1, ADMIN_TAGS_LIMIT);
    let rows = tag::admin_list(
        &state.pool,
        query.max_id,
        query.since_id,
        query.min_id,
        limit,
    )
    .await?;
    let settings = state.settings_cache.get(&state.pool).await?;
    let entities = admin_tag_json_page(
        &state.pool,
        &state.config.domain,
        &rows,
        admin.current.account.id,
        settings.trendable_by_default,
    )
    .await?;

    let mut headers = HeaderMap::new();
    let mut links = Vec::new();
    if i64::try_from(rows.len()).unwrap_or(0) == limit
        && let Some(last) = rows.last()
    {
        links.push(format!(
            "<https://{}/api/v1/admin/tags?limit={limit}&max_id={}>; rel=\"next\"",
            state.config.domain, last.id
        ));
    }
    if let Some(first) = rows.first() {
        links.push(format!(
            "<https://{}/api/v1/admin/tags?limit={limit}&min_id={}>; rel=\"prev\"",
            state.config.domain, first.id
        ));
    }
    if !links.is_empty()
        && let Ok(value) = links.join(", ").parse()
    {
        headers.insert(header::LINK, value);
    }
    Ok((headers, Json(Value::Array(entities))))
}

/// `GET /api/v1/admin/tags/{id}`.
pub async fn admin_tags_show(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_TAXONOMIES, false, "tags")?;
    let tag = tag::admin_find(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    render_admin_tag(&state, &admin, &tag).await
}

/// The registry fields a moderator may set (Mastodon's `PERMITTED_PARAMS`).
#[derive(Default)]
struct TagUpdate {
    display_name: Option<String>,
    usable: Option<bool>,
    listable: Option<bool>,
    trendable: Option<bool>,
}

/// `PUT`/`PATCH /api/v1/admin/tags/{id}` — set any subset of
/// `display_name`/`usable`/`listable`/`trendable` and stamp `reviewed_at`.
pub async fn admin_tags_update(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_TAXONOMIES, true, "tags")?;
    let update = parse_tag_update(&headers, &body)?;
    let tag = tag::admin_update(
        &state.pool,
        id,
        update.display_name.as_deref(),
        update.usable,
        update.listable,
        update.trendable,
        time::OffsetDateTime::now_utc(),
    )
    .await?
    .ok_or(ApiError::NotFound)?;
    render_admin_tag(&state, &admin, &tag).await
}

/// `GET /api/v1/admin/trends/tags` — all trending tags (allowed or pending),
/// highest score first, as `Admin::Tag` entities for review.
pub async fn admin_trends_index(
    State(state): State<AppState>,
    admin: AdminUser,
    Query(query): Query<TrendsQuery>,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_TAXONOMIES, false, "tags")?;
    let limit = query
        .limit
        .unwrap_or(DEFAULT_TAGS_LIMIT)
        .clamp(1, DEFAULT_TAGS_LIMIT * 2);
    let offset = query.offset.unwrap_or(0).max(0);
    let rows = tag_trend::all_admin(&state.pool, limit, offset).await?;
    let settings = state.settings_cache.get(&state.pool).await?;
    let entities = admin_tag_json_page(
        &state.pool,
        &state.config.domain,
        &rows,
        admin.current.account.id,
        settings.trendable_by_default,
    )
    .await?;
    Ok(Json(Value::Array(entities)))
}

/// `POST /api/v1/admin/trends/tags/{id}/approve` — mark the tag trendable (it
/// surfaces publicly after the next refresh) and reviewed.
pub async fn admin_trends_approve(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    review_tag(&state, &admin, id, true).await
}

/// `POST /api/v1/admin/trends/tags/{id}/reject` — mark the tag not trendable and
/// reviewed.
pub async fn admin_trends_reject(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    review_tag(&state, &admin, id, false).await
}

async fn review_tag(
    state: &AppState,
    admin: &AdminUser,
    id: i64,
    trendable: bool,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_TAXONOMIES, true, "tags")?;
    let tag = tag::admin_update(
        &state.pool,
        id,
        None,
        None,
        None,
        Some(trendable),
        time::OffsetDateTime::now_utc(),
    )
    .await?
    .ok_or(ApiError::NotFound)?;
    render_admin_tag(state, admin, &tag).await
}

async fn render_admin_tag(
    state: &AppState,
    admin: &AdminUser,
    tag: &tag::AdminTag,
) -> Result<Json<Value>, ApiError> {
    let settings = state.settings_cache.get(&state.pool).await?;
    let entity = admin_tag_json(
        &state.pool,
        &state.config.domain,
        tag,
        admin.current.account.id,
        settings.trendable_by_default,
    )
    .await?;
    Ok(Json(entity))
}

/// Parses a tag-registry update from a JSON or form body (moderator-set fields
/// only; absent keys are left unchanged).
fn parse_tag_update(headers: &HeaderMap, body: &[u8]) -> Result<TagUpdate, ApiError> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if content_type.starts_with("application/json") {
        let value: Value = serde_json::from_slice(body)
            .map_err(|e| ApiError::BadRequest(format!("invalid JSON body: {e}")))?;
        Ok(TagUpdate {
            display_name: value
                .get("display_name")
                .and_then(Value::as_str)
                .map(str::to_owned),
            usable: json_bool(&value, "usable"),
            listable: json_bool(&value, "listable"),
            trendable: json_bool(&value, "trendable"),
        })
    } else {
        let pairs: Vec<(String, String)> = serde_urlencoded::from_bytes(body)
            .map_err(|e| ApiError::BadRequest(format!("invalid form body: {e}")))?;
        let mut update = TagUpdate::default();
        for (key, value) in pairs {
            match key.as_str() {
                "display_name" => update.display_name = Some(value),
                "usable" => update.usable = Some(form_bool(&value)),
                "listable" => update.listable = Some(form_bool(&value)),
                "trendable" => update.trendable = Some(form_bool(&value)),
                _ => {}
            }
        }
        Ok(update)
    }
}

/// A JSON boolean, accepting `true`/`false` or their truthy/falsy string forms.
fn json_bool(value: &Value, key: &str) -> Option<bool> {
    match value.get(key)? {
        Value::Bool(b) => Some(*b),
        Value::String(s) => Some(form_bool(s)),
        _ => None,
    }
}

fn form_bool(value: &str) -> bool {
    matches!(value, "1" | "true" | "on" | "yes")
}
