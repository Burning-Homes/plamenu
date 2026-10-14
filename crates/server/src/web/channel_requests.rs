//! Follow requests target the channel identity, never its controller's account.
use super::{layout, session::WebUser, settings, view};
use crate::{AppState, error::ApiError};
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
};
use maud::html;
use plamenu_db::{account, follow, video_channel_access as access};

#[derive(Default, serde::Deserialize)]
pub struct Page {
    before: Option<i64>,
}

pub async fn index(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Query(query): Query<Page>,
) -> Result<Response, ApiError> {
    user.current.require_scope("read:accounts")?;
    if !matches!(
        access::role(&state.pool, id, user.current.account.id)
            .await?
            .as_deref(),
        Some("owner" | "editor")
    ) {
        return Err(ApiError::NotFound);
    }
    let channel = account::find_by_id(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let mut rows = follow::requests_of(&state.pool, id, query.before, None, 41).await?;
    let more = rows.len() > 40;
    rows.truncate(40);
    let ids = rows.iter().map(|r| r.account_id).collect::<Vec<_>>();
    let cards = crate::entities::render_accounts_by_ids(
        &state.pool,
        &state.config.domain,
        &ids,
        Some(user.current.account.id),
    )
    .await?;
    let l = user.locale;
    let body = html! {section.column {
        h1 {(l.text("actor-profile-follow-requests")) " — " (channel.display_name)}
        p {a href=(format!("/@{}",channel.username)) {(l.text("studio-view-channel"))} " · " a href=(format!("/videos/channels/{id}")) {(l.text("video-channel-manage"))}}
        @if cards.is_empty() {p.empty {(l.text("groups-requests-empty"))}}
        ul.relationships-list {
            @for value in &cards {li.relationships-list__item {
                (view::account_card(&view::Account(value)))
                @for (operation,label) in [("approve","groups-approve"),("reject","groups-reject")] {
                    form.inline-form method="post" action=(format!("/web/video-channels/{id}/requests")) {
                        input type="hidden" name="csrf" value=(user.csrf);
                        input type="hidden" name="account_id" value=(value["id"].as_str().unwrap_or_default());
                        input type="hidden" name="operation" value=(operation);
                        button type="submit" {(l.text(label))}
                    }
                }
            }}
        }
        @if more && let Some(last)=rows.last() {a.pager__more href=(format!("/videos/channels/{id}/requests?before={}",last.follow_id)) {(l.text("studio-older"))}}
    }};
    Ok((
        [("Cache-Control", "private, no-store"), ("Vary", "Cookie")],
        layout::shell(&l.text("actor-profile-follow-requests"), Some(&user), &body),
    )
        .into_response())
}

pub async fn action(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let form = settings::form_pairs(&body).map_err(ApiError::Unprocessable)?;
    if settings::field(&form, "csrf") != Some(user.csrf.as_str()) {
        return Ok(super::session::csrf_rejection());
    }
    user.current.require_scope("write:follows")?;
    // Hold the authority fence through the follow action and its federated
    // Accept/Reject. Revocation/transfer cannot overtake this decision.
    let mut authority = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    if !matches!(
        access::lock_access(&mut authority, id, user.current.account.id)
            .await?
            .as_deref(),
        Some("owner" | "editor")
    ) {
        return Err(ApiError::NotFound);
    }
    let target_id = settings::field(&form, "account_id")
        .and_then(|v| v.parse::<i64>().ok())
        .ok_or(ApiError::NotFound)?;
    let channel = account::find_by_id(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let requester = account::find_by_id(&state.pool, target_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    match settings::field(&form, "operation") {
        Some("approve") => {
            crate::actions::authorize_follow_request(&state, &channel, &requester).await?;
        }
        Some("reject") => {
            crate::actions::reject_follow_request(&state, &channel, &requester).await?;
        }
        _ => return Err(ApiError::Unprocessable("Unknown request action".into())),
    }
    authority
        .commit()
        .await
        .map_err(plamenu_db::DbError::from)?;
    Ok(settings::redirect_to(&format!(
        "/videos/channels/{id}/requests"
    )))
}
