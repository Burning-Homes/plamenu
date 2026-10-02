//! Discoverable, ordinary HTML recurring source controls; scripts are optional.
use super::{
    layout,
    session::{WebUser, csrf_rejection},
    settings::{field, form_pairs, redirect_to},
};
use crate::{AppState, error::ApiError};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use maud::html;
use plamenu_db::{account, video_source_sync as db};
use serde_json::json;

fn private(mut response: Response) -> Response {
    for (name, value) in [
        (header::CACHE_CONTROL, "private, no-store"),
        (header::VARY, "Cookie"),
        (header::REFERRER_POLICY, "no-referrer"),
    ] {
        response.headers_mut().insert(name, value.parse().unwrap());
    }
    response
}
async fn render(
    state: &AppState,
    user: &WebUser,
    pairs: &[(String, String)],
    error: Option<&str>,
) -> Result<Response, ApiError> {
    user.current.require_scope("read:media")?;
    let sources = db::list(&state.pool, user.current.account.id).await?;
    let available =
        plamenu_db::video_channel_access::available(&state.pool, user.current.account.id).await?;
    let channels = account::find_by_ids(
        &state.pool,
        &available.iter().map(|c| c.account_id).collect::<Vec<_>>(),
    )
    .await?;
    let destinations = account::find_by_ids(
        &state.pool,
        &sources
            .iter()
            .map(|source| source.channel_id)
            .collect::<Vec<_>>(),
    )
    .await?;
    let locale = user.locale;
    let value = |key| field(pairs, key).unwrap_or_default();
    let body = html! {
        section.column.video-workspace data-video-source-sync data-source-pending=(sources.iter().any(|s|s.enabled&&(s.lease_until.is_some()||s.last_result=="pending")))
            data-source-refresh-failed=(locale.text("video-sources-refresh-failed")) {
            h1{(locale.text("video-sources-title"))}
            (super::video_workspace::navigation("/videos/sources",locale))
            p{(locale.text("video-sources-hint"))}
            p{a href="/videos?view=drafts"{(locale.text("video-sources-review"))} " · " a href="/videos/sources"{(locale.text("video-sources-refresh"))}}
            p role="status" aria-live="polite" data-source-refresh-error hidden {}
            @if let Some(error)=error{p.settings__error role="alert"{(error)}}
            details.video-workspace__create id="source-create" open[error.is_some() || sources.is_empty()] {
                summary {(locale.text("video-sources-add"))}
            @if channels.is_empty(){p {a href="/videos/channels/new" {(locale.text("videos-new-channel"))}}}
            @else if sources.len()<usize::try_from(db::MAX_SOURCES).unwrap_or(5){
                form.settings-form method="post" action="/web/videos/sources"{
                    input type="hidden" name="csrf" value=(user.csrf);
                    label.settings-field{span.settings-field__label{(locale.text("videos-channel"))}
                        select name="channel" required{@for channel in &channels{option value=(channel.id) selected[value("channel")==channel.id.to_string()]{(channel.display_name)}}}}
                    label.settings-field{span.settings-field__label{(locale.text("video-sources-url"))}
                        input type="url" name="source_url" value=(value("source_url")) required maxlength="2048" aria-describedby="source-privacy";}
                    p.settings-field__hint id="source-privacy"{(locale.text("video-sources-privacy"))}
                    label.settings-field{span.settings-field__label{(locale.text("video-sources-interval"))}
                        select name="interval"{
                            @for (minutes,key) in [(15,"video-sources-quarter-hour"),(360,"video-sources-six-hours"),(1440,"video-sources-day"),(10080,"video-sources-week")]{
                                option value=(minutes) selected[value("interval")==minutes.to_string() || (value("interval").is_empty()&&minutes==360)]{(locale.text(key))}
                            }
                        }}
                    label.settings-field__choice{input type="checkbox" name="confirm" value="yes" required;span{(locale.text("video-sources-rights"))}}
                    button type="submit"{(locale.text("video-sources-add"))}
                }
            }@else{p{(locale.text("video-sources-limit"))}}
            }
            @for source in &sources{
                section.video-workspace__source aria-labelledby=(format!("source-title-{}",source.id)){
                    h2 id=(format!("source-title-{}",source.id)){(locale.text("video-sources-source")) " " (source.id)}
                    p.video-source-url{(source.source_url)}
                    @if let Some(channel) = destinations.iter().find(|channel| channel.id == source.channel_id) {
                        p{(locale.text("videos-channel")) ": " (channel.display_name)}
                    }
                    p role="status" aria-live="polite" data-source-state=(source.id){
                        (if source.enabled{locale.text("video-sources-active")}else{locale.text("video-sources-paused")}) " — "
                        (locale.text(&format!("video-sources-result-{}",source.last_result)))
                    }
                    details {summary {(locale.text("studio-source-details"))}
                    @if let Some(checked)=source.last_checked_at{p{(locale.text("video-sources-last")) " " (user.clock.element_absolute(checked))}}
                    @if source.enabled{p{(locale.text("video-sources-next")) " " (user.clock.element_absolute(source.next_check_at))}}
                    p{(locale.text("video-sources-interval")) ": " (source.interval_minutes) " " (locale.text("video-sources-minutes"))}
                    }
                    div.video-workspace__source-actions {
                    form method="post" action=(format!("/web/videos/sources/{}/{}",source.id,if source.enabled{"pause"}else{"resume"})){
                        input type="hidden" name="csrf" value=(user.csrf);
                        button type="submit"{(if source.enabled{locale.text("video-sources-pause")}else{locale.text("video-sources-resume")})}
                    }
                    @if source.enabled{form method="post" action=(format!("/web/videos/sources/{}/check",source.id)){
                        input type="hidden" name="csrf" value=(user.csrf);
                        button type="submit"{(locale.text("video-sources-check"))}
                    }}
                    details{
                        summary{(locale.text("video-sources-remove"))}
                        p{(locale.text("video-sources-remove-hint"))}
                        form.settings-form method="post" action=(format!("/web/videos/sources/{}/remove",source.id)){
                            input type="hidden" name="csrf" value=(user.csrf);
                            label.settings-field__choice{input type="checkbox" name="confirm" value="yes" required;span{(locale.text("video-sources-remove-confirm"))}}
                            button type="submit"{(locale.text("video-sources-remove"))}
                        }
                    }
                    }
                }
            }
        }
    };
    Ok((
        if error.is_some() {
            StatusCode::UNPROCESSABLE_ENTITY
        } else {
            StatusCode::OK
        },
        layout::shell(&locale.text("video-sources-title"), Some(user), &body),
    )
        .into_response())
}
pub async fn page(State(state): State<AppState>, user: WebUser) -> Response {
    private(
        render(&state, &user, &[], None)
            .await
            .unwrap_or_else(IntoResponse::into_response),
    )
}
pub async fn create(State(state): State<AppState>, user: WebUser, body: Bytes) -> Response {
    private(
        create_inner(&state, &user, &body)
            .await
            .unwrap_or_else(IntoResponse::into_response),
    )
}
async fn create_inner(
    state: &AppState,
    user: &WebUser,
    body: &Bytes,
) -> Result<Response, ApiError> {
    let pairs = form_pairs(body).map_err(ApiError::BadRequest)?;
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return Ok(csrf_rejection());
    }
    user.current.require_scope("write:media")?;
    let result = async {
        if field(&pairs, "confirm") != Some("yes") {
            return Err(ApiError::Unprocessable(
                user.locale.text("video-sources-rights"),
            ));
        }
        let url = crate::video_source_import::validate_url(
            field(&pairs, "source_url").unwrap_or_default(),
            state.config.allow_private_fetch,
        )?;
        let channel = field(&pairs, "channel")
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| ApiError::Unprocessable(user.locale.text("videos-channel")))?;
        let interval = field(&pairs, "interval")
            .and_then(|v| v.parse::<i32>().ok())
            .filter(|v| [15, 360, 1440, 10080].contains(v))
            .ok_or_else(|| ApiError::Unprocessable(user.locale.text("video-sources-interval")))?;
        if db::create(
            &state.pool,
            user.current.account.id,
            channel,
            &url,
            interval,
        )
        .await?
        .is_none()
        {
            return Err(ApiError::Unprocessable(
                user.locale.text("video-sources-limit"),
            ));
        }
        Ok(redirect_to("/videos/sources"))
    }
    .await;
    match result {
        Err(ApiError::Unprocessable(message)) => render(state, user, &pairs, Some(&message)).await,
        other => other,
    }
}
pub async fn action(
    State(state): State<AppState>,
    user: WebUser,
    Path((id, operation)): Path<(i64, String)>,
    body: Bytes,
) -> Response {
    private(
        action_inner(&state, &user, id, &operation, &body)
            .await
            .unwrap_or_else(IntoResponse::into_response),
    )
}
async fn action_inner(
    state: &AppState,
    user: &WebUser,
    id: i64,
    operation: &str,
    body: &Bytes,
) -> Result<Response, ApiError> {
    let pairs = form_pairs(body).map_err(ApiError::BadRequest)?;
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return Ok(csrf_rejection());
    }
    user.current.require_scope("write:media")?;
    let owner = user.current.account.id;
    let changed = match operation {
        "pause" => db::set_enabled(&state.pool, id, owner, false).await?,
        "resume" => db::set_enabled(&state.pool, id, owner, true).await?,
        "check" => db::check_now(&state.pool, id, owner).await?,
        "remove" if field(&pairs, "confirm") == Some("yes") => {
            db::remove(&state.pool, id, owner).await?
        }
        _ => return Err(ApiError::NotFound),
    };
    if !changed {
        return Err(ApiError::Conflict(
            user.locale.text("video-sources-action-conflict"),
        ));
    }
    Ok(redirect_to("/videos/sources"))
}
pub async fn state(State(state): State<AppState>, user: WebUser) -> Response {
    let result=async{user.current.require_scope("read:media")?;
        let sources=db::list(&state.pool,user.current.account.id).await?;
        let values=sources.iter().map(|s|json!({"id":s.id.to_string(),"checking":s.enabled&&(s.lease_until.is_some()||s.last_result=="pending"),
            "label":format!("{} — {}",if s.enabled{user.locale.text("video-sources-active")}else{user.locale.text("video-sources-paused")},user.locale.text(&format!("video-sources-result-{}",s.last_result)))})).collect::<Vec<_>>();
        Ok::<_,ApiError>(Json(values).into_response())
    }.await;
    private(result.unwrap_or_else(IntoResponse::into_response))
}
