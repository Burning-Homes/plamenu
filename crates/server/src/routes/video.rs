//! Plamenu's native-video API. Media upload retains the shared media endpoint;
//! publication adds explicit channel ownership and Video semantics.
use crate::{AppState, actions::PostParams, auth::CurrentUser, error::ApiError};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::HeaderMap,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/videos/{id}/distribution",
            get(distribution).put(save_distribution),
        )
        .route("/api/v1/video_channels", get(channels).post(create_channel))
        .route(
            "/api/v1/video_channels/{id}",
            get(channel).post(change_channel),
        )
        .route(
            "/api/v1/video_channel_invitations",
            get(channel_invitations),
        )
        .route("/api/v1/videos", post(publish))
        .route(
            "/api/v1/videos/{id}/content",
            get(content).put(replace_content),
        )
        .route("/api/v1/videos/{id}/edits", post(prepare_edit))
        .route(
            "/api/v1/video_edits/{id}",
            get(prepared_edit).delete(cancel_edit),
        )
        .route("/api/v1/video_edits/{id}/retry", post(retry_edit))
        .route(
            "/api/v1/video_viewer_preferences",
            get(viewer_preferences).put(update_viewer_preferences),
        )
        .route("/api/v1/video_history", get(history).delete(clear_history))
        .route(
            "/api/v1/video_history/{id}",
            axum::routing::put(save_position).delete(forget_position),
        )
        .route(
            "/api/v1/videos/{id}/metadata",
            axum::routing::put(update_metadata),
        )
}

#[derive(Deserialize)]
struct DistributionInput {
    download_enabled: bool,
    embed_enabled: bool,
}
async fn distribution(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<axum::response::Response, ApiError> {
    current.require_scope("read:media")?;
    let (policy, _) = crate::video_distribution::owned(&state, current.account.id, id).await?;
    Ok(private_json(
        json!({"download_enabled":policy.download_enabled,"embed_enabled":policy.embed_enabled}),
    ))
}
async fn save_distribution(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<axum::response::Response, ApiError> {
    current.require_scope("write:media")?;
    let input: DistributionInput = super::params::parse_body(&headers, &body)?;
    crate::video_distribution::save(
        &state,
        current.account.id,
        plamenu_db::video_distribution::Policy {
            media_id: id,
            download_enabled: input.download_enabled,
            embed_enabled: input.embed_enabled,
        },
        current.require_scope("write:statuses").is_ok(),
    )
    .await?;
    Ok(private_json(
        json!({"download_enabled":input.download_enabled,"embed_enabled":input.embed_enabled}),
    ))
}

async fn content(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<axum::response::Response, ApiError> {
    current.require_scope("read:media")?;
    let editable = crate::video_edit::editable(&state, id, current.account.id).await?;
    let candidates = crate::video_edit::candidates(&state, id, current.account.id).await?;
    Ok(private_json(
        json!({"media_id":id.to_string(),"status_id":editable.status_id.to_string(),"revision":editable.revision,
        "candidates":candidates.iter().map(|c|json!({"media_id":c.media_id.to_string(),"title":c.title})).collect::<Vec<_>>() }),
    ))
}
#[derive(Deserialize)]
struct Replacement {
    candidate_media_id: String,
    revision: i64,
    confirm: bool,
}
async fn replace_content(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<axum::response::Response, ApiError> {
    current.require_scope("write:media")?;
    current.require_scope("write:statuses")?;
    let input: Replacement = super::params::parse_body(&headers, &body)?;
    if !input.confirm {
        return Err(ApiError::Unprocessable(
            "Confirm the reviewed replacement".into(),
        ));
    }
    let candidate = input
        .candidate_media_id
        .parse()
        .map_err(|_| ApiError::Unprocessable("Choose a replacement draft".into()))?;
    crate::video_edit::replace(&state, id, current.account.id, candidate, input.revision).await?;
    Ok(private_json(
        json!({"media_id":id.to_string(),"revision":input.revision+1}),
    ))
}

#[derive(Deserialize)]
struct Preparation {
    revision: i64,
    operation: String,
    start_ms: Option<i64>,
    end_ms: Option<i64>,
}
async fn prepare_edit(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<axum::response::Response, ApiError> {
    current.require_scope("write:media")?;
    current.require_scope("write:statuses")?;
    let input: Preparation = super::params::parse_body(&headers, &body)?;
    let operation = match input.operation.as_str() {
        "reprocess" if input.start_ms.is_none() && input.end_ms.is_none() => {
            crate::video_source_edit::Operation::Reprocess
        }
        "trim" => crate::video_source_edit::Operation::Trim(crate::video_clip::Window::new(
            input
                .start_ms
                .ok_or(ApiError::Unprocessable("Choose start_ms and end_ms".into()))?,
            input
                .end_ms
                .ok_or(ApiError::Unprocessable("Choose start_ms and end_ms".into()))?,
        )?),
        _ => {
            return Err(ApiError::Unprocessable(
                "Choose reprocess without a cut, or trim with a millisecond cut".into(),
            ));
        }
    };
    let candidate = crate::video_source_edit::prepare(
        &state,
        id,
        current.account.id,
        input.revision,
        operation,
    )
    .await?;
    let mut response = private_json(
        json!({"media_id":candidate.to_string(),"status_url":format!("/api/v1/video_edits/{candidate}"),"review_url":format!("/videos/drafts/{candidate}/review")}),
    );
    *response.status_mut() = axum::http::StatusCode::ACCEPTED;
    Ok(response)
}
async fn prepared_edit(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<axum::response::Response, ApiError> {
    current.require_scope("read:media")?;
    let edit = crate::video_source_edit::prepared(&state, id, current.account.id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(private_json(
        json!({"media_id":id.to_string(),"target_media_id":edit.target_media_id.map(|id|id.to_string()),"target_revision":edit.target_revision,"operation":edit.operation,"phase":edit.phase}),
    ))
}
async fn retry_edit(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<axum::response::Response, ApiError> {
    current.require_scope("write:media")?;
    current.require_scope("write:statuses")?;
    crate::video_source_edit::prepared(&state, id, current.account.id)
        .await?
        .ok_or(ApiError::NotFound)?;
    crate::video_source_edit::check_retry(&state, id, current.account.id).await?;
    if !plamenu_db::video_source_import::retry(&state.pool, id, current.account.id).await?
        && !plamenu_db::video_draft::retry(&state.pool, id, current.account.id).await?
    {
        return Err(ApiError::Conflict(
            "This edit is not a retryable private draft".into(),
        ));
    }
    let mut response = private_json(json!({"media_id":id.to_string(),"phase":"queued"}));
    *response.status_mut() = axum::http::StatusCode::ACCEPTED;
    Ok(response)
}
async fn cancel_edit(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<axum::response::Response, ApiError> {
    current.require_scope("write:media")?;
    crate::video_source_edit::prepared(&state, id, current.account.id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !plamenu_db::video_draft::cancel(&state.pool, id, current.account.id).await? {
        return Err(ApiError::Conflict(
            "This private edit cannot be cancelled".into(),
        ));
    }
    let mut response = private_json(json!({}));
    *response.status_mut() = axum::http::StatusCode::NO_CONTENT;
    *response.body_mut() = axum::body::Body::empty();
    Ok(response)
}
fn private_json(value: Value) -> axum::response::Response {
    use axum::response::IntoResponse;
    crate::web::video_history::private(Json(value).into_response())
}
async fn viewer_preferences(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<axum::response::Response, ApiError> {
    current.require_scope("read:accounts")?;
    let prefs = plamenu_db::video_history::preferences(&state.pool, current.account.id).await?;
    Ok(private_json(
        json!({"history_enabled":prefs.history_enabled,"p2p_enabled":prefs.p2p_enabled,"p2p_configured":prefs.p2p_configured,"history_max_entries":plamenu_db::video_history::MAX_ENTRIES,"history_retention_days":plamenu_db::video_history::RETENTION_DAYS}),
    ))
}
#[derive(Deserialize)]
struct ViewerPreferences {
    history_enabled: Option<bool>,
    p2p_enabled: Option<bool>,
}
async fn update_viewer_preferences(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<axum::response::Response, ApiError> {
    current.require_scope("write:accounts")?;
    let input: ViewerPreferences = super::params::parse_body(&headers, &body)?;
    if input.history_enabled.is_none() && input.p2p_enabled.is_none() {
        return Err(ApiError::Unprocessable(
            "Choose a video viewing preference to change".into(),
        ));
    }
    let prefs = plamenu_db::video_history::update_preferences(
        &state.pool,
        current.account.id,
        input.history_enabled,
        input.p2p_enabled,
    )
    .await?;
    Ok(private_json(
        json!({"history_enabled":prefs.history_enabled,"p2p_enabled":prefs.p2p_enabled,"p2p_configured":prefs.p2p_configured}),
    ))
}
async fn history(
    State(state): State<AppState>,
    current: CurrentUser,
    axum::extract::Query(q): axum::extract::Query<crate::web::video_history::HistoryQuery>,
) -> Result<axum::response::Response, ApiError> {
    current.require_scope("read:statuses")?;
    let (at, id) = q.cursor()?;
    let (entries, next) = crate::video_history::list(&state, current.account.id, at, id).await?;
    Ok(private_json(
        json!({"data":entries,"next":next.map(|e|json!({"before":e.updated_at.format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),"before_media":e.media_id.to_string()}))}),
    ))
}
async fn clear_history(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<axum::response::Response, ApiError> {
    current.require_scope("write:statuses")?;
    plamenu_db::video_history::forget(&state.pool, current.account.id, None).await?;
    Ok(private_json(json!({"ok":true})))
}
async fn forget_position(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<axum::response::Response, ApiError> {
    current.require_scope("write:statuses")?;
    plamenu_db::video_history::forget(&state.pool, current.account.id, Some(id)).await?;
    Ok(private_json(json!({"ok":true})))
}
#[derive(Deserialize)]
struct PlaybackPosition {
    position_seconds: i32,
    #[serde(default)]
    content_revision: i64,
    #[serde(default)]
    completed: bool,
}
async fn save_position(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<axum::response::Response, ApiError> {
    current.require_scope("write:statuses")?;
    let input: PlaybackPosition = super::params::parse_body(&headers, &body)?;
    let saved = crate::video_history::save_revision(
        &state,
        current.account.id,
        id,
        input.position_seconds,
        input.completed,
        input.content_revision,
    )
    .await?;
    Ok(private_json(json!({"saved":saved})))
}

async fn channels(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:accounts")?;
    let rows = plamenu_db::video_channel_access::available(&state.pool, current.account.id).await?;
    let ids = rows.iter().map(|c| c.account_id).collect::<Vec<_>>();
    let accounts = plamenu_db::account::find_by_ids(&state.pool, &ids).await?;
    let roles = plamenu_db::video_channel_access::roles(&state.pool, current.account.id).await?;
    let mut entities = Vec::with_capacity(accounts.len());
    for a in accounts {
        let owner = rows
            .iter()
            .find(|r| r.account_id == a.id)
            .map(|r| r.owner_account_id);
        entities.push(json!({"id":a.id.to_string(),"username":a.username,"display_name":a.display_name,"uri":a.uri,"owner_id":owner.map(|id|id.to_string()),"role":roles.get(&a.id)}));
    }
    Ok(Json(json!(entities)))
}

async fn channel_invitations(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:accounts")?;
    let rows:Vec<(i64,String,Option<String>,bool)>=sqlx::query_as("SELECT c.account_id,a.display_name,g.role,COALESCE(c.transfer_to=$1,false) FROM video_channels c JOIN accounts a ON a.id=c.account_id JOIN accounts o ON o.id=c.owner_account_id LEFT JOIN video_channel_grants g ON g.channel_id=c.account_id AND g.account_id=$1 AND NOT g.accepted WHERE a.suspended_at IS NULL AND o.suspended_at IS NULL AND (c.transfer_to=$1 OR g.account_id IS NOT NULL) ORDER BY c.account_id")
        .bind(current.account.id).fetch_all(&state.pool).await.map_err(plamenu_db::DbError::from)?;
    Ok(Json(json!(rows.into_iter().map(|(id,name,role,transfer)|json!({"channel_id":id.to_string(),"display_name":name,"role":role,"ownership_transfer":transfer})).collect::<Vec<_>>())))
}

async fn channel(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:accounts")?;
    let role = plamenu_db::video_channel_access::role(&state.pool, id, current.account.id).await?;
    let invitations =
        plamenu_db::video_channel_access::invitations(&state.pool, current.account.id).await?;
    if role.is_none() && !invitations.contains(&id) {
        return Err(ApiError::NotFound);
    }
    let ch = plamenu_db::video::channel(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let a = plamenu_db::account::find_by_id(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let grants = if role.as_deref() == Some("owner") {
        plamenu_db::video_channel_access::grants(&state.pool,id).await?.into_iter().map(|g|json!({"account_id":g.account_id.to_string(),"role":g.role,"accepted":g.accepted})).collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    Ok(Json(
        json!({"id":id.to_string(),"username":a.username,"display_name":a.display_name,"description":a.note_source,"owner_id":ch.owner_account_id.to_string(),"role":role,"grants":grants}),
    ))
}

async fn change_channel(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:accounts")?;
    let input = super::params::parse_body(&headers, &body)?;
    crate::video_channels::change(&state, &current.account, id, &input).await?;
    Ok(Json(json!({"ok":true})))
}

#[derive(Deserialize)]
struct ChannelInput {
    username: String,
    display_name: String,
}

async fn create_channel(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:accounts")?;
    let input: ChannelInput = super::params::parse_body(&headers, &body)?;
    let account = crate::video::create_channel(
        &state,
        &current.account,
        &input.username,
        &input.display_name,
    )
    .await?;
    Ok(Json(
        json!({"id":account.id.to_string(), "username":account.username, "display_name":account.display_name,
        "uri":account.uri, "owner_id":current.account.id.to_string()}),
    ))
}

#[derive(Deserialize)]
struct VideoInput {
    channel_id: String,
    media_id: String,
    title: String,
    #[serde(default)]
    description: String,
    visibility: Option<String>,
    language: Option<String>,
    scheduled_at: Option<String>,
    category: Option<Value>,
    licence: Option<Value>,
    spoiler_text: Option<String>,
    sensitive: Option<Value>,
}

async fn publish(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:statuses")?;
    let input: VideoInput = super::params::parse_body(&headers, &body)?;
    let channel_id = input
        .channel_id
        .parse()
        .map_err(|_| ApiError::Unprocessable("Invalid channel id".into()))?;
    let media_id = input
        .media_id
        .parse()
        .map_err(|_| ApiError::Unprocessable("Invalid media id".into()))?;
    let metadata = crate::video_metadata::draft(
        &state,
        current.account.id,
        media_id,
        &crate::video_metadata::Input {
            category: input.category,
            licence: input.licence,
            spoiler_text: input.spoiler_text,
            sensitive: input.sensitive,
        },
    )
    .await?;
    if let Some(raw) = input
        .scheduled_at
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        let instant = super::scheduled_statuses::parse_scheduled_at(raw)?;
        return Ok(Json(
            super::scheduled_statuses::create_scheduled_video(
                &state,
                &current.account,
                instant,
                channel_id,
                media_id,
                &input.title,
                &input.description,
                input.visibility.as_deref().unwrap_or("public"),
                input.language.as_deref(),
                Some(current.app_id),
            )
            .await?,
        ));
    }
    let (stored, _) = crate::actions::post_video_for_application(
        &state,
        PostParams {
            username: &current.account.username,
            title: Some(&input.title),
            text: &input.description,
            visibility: input.visibility.as_deref().unwrap_or("public"),
            language: input.language.as_deref(),
            media_ids: &[media_id],
            spoiler_text: &metadata.spoiler_text,
            sensitive: metadata.sensitive,
            ..Default::default()
        },
        channel_id,
        current.app_id,
    )
    .await?;
    Ok(Json(
        crate::entities::render_status(
            &state.pool,
            &state.config.domain,
            &stored,
            Some(current.account.id),
        )
        .await?,
    ))
}

async fn update_metadata(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:statuses")?;
    let input = super::params::parse_body(&headers, &body)?;
    crate::video_metadata::update(&state, current.account.id, id, &input).await?;
    let metadata = plamenu_db::video_metadata::for_media(&state.pool, &[id])
        .await?
        .remove(&id)
        .unwrap_or_default();
    Ok(Json(crate::video_metadata::entity(&metadata)))
}
