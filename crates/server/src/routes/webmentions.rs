use crate::{AppState, auth::CurrentUser, error::ApiError, webmentions};
use axum::{
    Form, Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
#[derive(serde::Deserialize)]
struct Incoming {
    source: String,
    target: String,
}
#[derive(Default, serde::Deserialize)]
struct Page {
    target: Option<String>,
    before: Option<i64>,
}
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/webmentions", post(receive))
        .route("/webmentions/{id}/photo", get(photo))
        .route("/api/v1/webmentions", get(list))
        .route("/api/v1/webmentions/archive", get(archive))
        .route("/api/v1/webmentions/{id}/interact", post(interact))
        .layer(DefaultBodyLimit::max(8192))
}
async fn receive(
    State(state): State<AppState>,
    Form(input): Form<Incoming>,
) -> Result<Response, ApiError> {
    let id = webmentions::receive(&state, &input.source, &input.target).await?;
    Ok((
        StatusCode::ACCEPTED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({"id":id.to_string()})),
    )
        .into_response())
}
fn response(page: &webmentions::Page) -> Response {
    (
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Authorization, Cookie"),
        ],
        Json(serde_json::json!({"items":page.items.iter().map(webmentions::Mention::entity).collect::<Vec<_>>(),"before":page.before.map(|id|id.to_string())})),
    )
        .into_response()
}
async fn list(
    State(state): State<AppState>,
    Query(page): Query<Page>,
) -> Result<Response, ApiError> {
    Ok(response(
        &webmentions::page(&state, None, page.target.as_deref(), page.before).await?,
    ))
}
async fn archive(
    State(state): State<AppState>,
    user: CurrentUser,
    Query(page): Query<Page>,
) -> Result<Response, ApiError> {
    user.require_scope("read:notifications")?;
    Ok(response(
        &webmentions::page(
            &state,
            Some(user.account.id),
            page.target.as_deref(),
            page.before,
        )
        .await?,
    ))
}

#[derive(serde::Deserialize)]
struct Interaction {
    kind: String,
    #[serde(default)]
    text: String,
}
async fn interact(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<i64>,
    Json(input): Json<Interaction>,
) -> Result<Response, ApiError> {
    user.require_scope("write:statuses")?;
    let item =
        webmentions::interactions::publish(&state, &user.account, id, &input.kind, &input.text)
            .await?;
    Ok(([(header::CACHE_CONTROL,"private, no-store")],Json(serde_json::json!({"id":item.id.to_string(),"url":crate::entities::status_web_url(&state.config.domain,&item,&user.account.username)}))).into_response())
}

async fn photo(State(state): State<AppState>, Path(id): Path<i64>) -> Result<Response, ApiError> {
    let (bytes, content_type) = webmentions::photo::get(&state, id).await?;
    Ok((
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-store".into()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".into()),
        ],
        bytes,
    )
        .into_response())
}
