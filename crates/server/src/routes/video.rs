//! Plamenu's native-video API. Media upload retains the shared media endpoint;
//! publication adds explicit channel ownership and Video semantics.
use crate::{AppState, actions::PostParams, auth::CurrentUser, error::ApiError};
use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::HeaderMap,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/video_channels", get(channels).post(create_channel))
        .route("/api/v1/videos", post(publish))
}

async fn channels(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:accounts")?;
    let rows = plamenu_db::video::owned_channels(&state.pool, current.account.id).await?;
    let ids = rows.iter().map(|c| c.account_id).collect::<Vec<_>>();
    let accounts = plamenu_db::account::find_by_ids(&state.pool, &ids).await?;
    Ok(Json(json!(
        accounts
            .iter()
            .map(|a| json!({
                "id":a.id.to_string(), "username":a.username, "display_name":a.display_name,
                "uri":a.uri, "owner_id":current.account.id.to_string()
            }))
            .collect::<Vec<_>>()
    )))
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
    let (stored, _) = crate::actions::post_video_for_application(
        &state,
        PostParams {
            username: &current.account.username,
            title: Some(&input.title),
            text: &input.description,
            visibility: input.visibility.as_deref().unwrap_or("public"),
            language: input.language.as_deref(),
            media_ids: &[media_id],
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
