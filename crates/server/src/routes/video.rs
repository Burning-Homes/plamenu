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
            "/api/v1/videos/{id}/metadata",
            axum::routing::put(update_metadata),
        )
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
