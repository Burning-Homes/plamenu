//! `/api/v1/timelines/*`.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, header};
use axum::response::IntoResponse;
use plamenu_db::status::{self, Status};
use plamenu_db::user::{self, TimelineOrder};
use serde::Deserialize;
use serde_json::Value;

use crate::auth::{CurrentUser, MaybeUser};
use crate::entities::render_statuses;
use crate::error::ApiError;
use crate::state::AppState;

const DEFAULT_LIMIT: i64 = 20;
const MAX_LIMIT: i64 = 40;

async fn render_live_rows(
    state: &AppState,
    statuses: &[Status],
    viewer_id: Option<i64>,
) -> Result<Vec<Value>, ApiError> {
    let mut entities =
        render_statuses(&state.pool, &state.config.domain, statuses, viewer_id).await?;
    if crate::live_refresh::refresh_rendered_statuses(state, &entities).await {
        entities = render_statuses(&state.pool, &state.config.domain, statuses, viewer_id).await?;
    }
    Ok(entities)
}

#[derive(Deserialize)]
pub struct TimelineParams {
    pub max_id: Option<i64>,
    pub limit: Option<i64>,
    /// Public timeline only: restrict to local statuses.
    #[serde(default)]
    pub local: Option<String>,
}

fn limit_of(params: &TimelineParams) -> i64 {
    params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

/// The viewer's timeline-ordering preference; anonymous viewers read in the
/// default (publish-time) order.
pub(super) async fn order_for(
    state: &AppState,
    user_id: Option<i64>,
) -> Result<TimelineOrder, ApiError> {
    let Some(user_id) = user_id else {
        return Ok(TimelineOrder::default());
    };
    Ok(user::settings_by_user_id(&state.pool, user_id)
        .await?
        .map(|settings| settings.timeline_order)
        .unwrap_or_default())
}

/// Keyset pagination via the Link header, the way clients expect.
pub(super) fn link_header(domain: &str, path: &str, limit: i64, page: &[Status]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if page.len() == usize::try_from(limit).unwrap_or(usize::MAX)
        && let Some(last) = page.last()
    {
        let sep = if path.contains('?') { '&' } else { '?' };
        let link = format!(
            "<https://{domain}{path}{sep}limit={limit}&max_id={}>; rel=\"next\"",
            last.id
        );
        if let Ok(value) = link.parse() {
            headers.insert(header::LINK, value);
        }
    }
    headers
}

/// `GET /api/v1/timelines/home`.
pub async fn home(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(params): Query<TimelineParams>,
) -> Result<impl IntoResponse, ApiError> {
    current.require_scope("read:statuses")?;
    let limit = limit_of(&params);
    let order = order_for(&state, Some(current.user.id)).await?;
    let statuses =
        status::home_timeline(&state.pool, current.account.id, order, params.max_id, limit).await?;
    let entities = render_live_rows(&state, &statuses, Some(current.account.id)).await?;
    let headers = link_header(
        &state.config.domain,
        "/api/v1/timelines/home",
        limit,
        &statuses,
    );
    Ok((headers, Json(entities)))
}

/// `GET /api/v1/timelines/tag/{hashtag}` — public statuses carrying a tag.
pub async fn tag(
    State(state): State<AppState>,
    viewer: MaybeUser,
    axum::extract::Path(hashtag): axum::extract::Path<String>,
    Query(params): Query<TimelineParams>,
) -> Result<impl IntoResponse, ApiError> {
    let viewer = viewer.require_preview(state.timeline_preview_tag().await)?;
    if let Some(viewer) = &viewer {
        viewer.require_scope("read:statuses")?;
    }
    let limit = limit_of(&params);
    let viewer_id = viewer.as_ref().map(|v| v.account.id);
    let order = order_for(&state, viewer.as_ref().map(|v| v.user.id)).await?;
    let statuses = plamenu_db::tag::timeline(
        &state.pool,
        &hashtag,
        viewer_id,
        order,
        params.max_id,
        limit,
    )
    .await?;
    let entities = render_live_rows(&state, &statuses, viewer_id).await?;
    let path = format!("/api/v1/timelines/tag/{hashtag}");
    let headers = link_header(&state.config.domain, &path, limit, &statuses);
    Ok((headers, Json(entities)))
}

/// `GET /api/v1/timelines/public` (`?local=true` for the local timeline).
/// Anonymous reads require the matching per-type preview flag; otherwise 401.
pub async fn public(
    State(state): State<AppState>,
    viewer: MaybeUser,
    Query(params): Query<TimelineParams>,
) -> Result<impl IntoResponse, ApiError> {
    let limit = limit_of(&params);
    let local_only = params
        .local
        .as_deref()
        .is_some_and(|v| matches!(v, "true" | "1"));
    let preview = if local_only {
        state.timeline_preview_local().await
    } else {
        state.timeline_preview_federated().await
    };
    let viewer = viewer.require_preview(preview)?;
    if let Some(viewer) = &viewer {
        viewer.require_scope("read:statuses")?;
    }
    let viewer_id = viewer.as_ref().map(|v| v.account.id);
    let order = order_for(&state, viewer.as_ref().map(|v| v.user.id)).await?;
    let statuses = status::public_timeline(
        &state.pool,
        local_only,
        viewer_id,
        state.public_timeline_replies().await,
        order,
        params.max_id,
        limit,
    )
    .await?;
    let entities = render_live_rows(&state, &statuses, viewer_id).await?;
    let path = if local_only {
        "/api/v1/timelines/public?local=true"
    } else {
        "/api/v1/timelines/public"
    };
    let headers = link_header(&state.config.domain, path, limit, &statuses);
    Ok((headers, Json(entities)))
}
