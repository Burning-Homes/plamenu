//! HTML-first channel authority. Every action is reachable without scripts.
use super::{
    layout,
    session::{WebUser, csrf_rejection},
    settings::{field, form_pairs, redirect_to},
};
use crate::{AppState, error::ApiError};
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
use plamenu_db::{account, video_channel_access as access};

fn private(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "private, no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::VARY, "Cookie".parse().unwrap());
    response
}

pub async fn index(State(state): State<AppState>, user: WebUser) -> Result<Response, ApiError> {
    let mut ids = access::available(&state.pool, user.current.account.id)
        .await?
        .into_iter()
        .map(|c| c.account_id)
        .collect::<Vec<_>>();
    let invitations = access::invitations(&state.pool, user.current.account.id).await?;
    ids.extend(&invitations);
    ids.sort_unstable();
    ids.dedup();
    let accounts = account::find_by_ids(&state.pool, &ids).await?;
    let locale = user.locale;
    let body = html! {
        section.column {
            h1 { (locale.text("video-channel-manage")) }
            p { a href="/videos" { (locale.text("videos-title")) } }
            ul {
                @for channel in &accounts {
                    li { a href=(format!("/videos/channels/{}",channel.id)) { (channel.display_name) " (@" (channel.username) ")" }
                        @if invitations.contains(&channel.id) { " — " (locale.text("video-channel-invitation")) }
                    }
                }
            }
            @if accounts.is_empty() { p { (locale.text("video-channel-empty")) } }
        }
    };
    Ok(private(
        layout::shell(&locale.text("video-channel-manage"), Some(&user), &body).into_response(),
    ))
}

pub async fn manage(State(state): State<AppState>, user: WebUser, Path(id): Path<i64>) -> Response {
    private(
        render(&state, &user, id, None)
            .await
            .unwrap_or_else(IntoResponse::into_response),
    )
}

fn action_button(id: i64, csrf: &str, operation: &str, label: &str) -> Markup {
    html! { form.settings-form method="post" action=(format!("/web/video-channels/{id}")) {
        input type="hidden" name="csrf" value=(csrf);
        input type="hidden" name="operation" value=(operation);
        button type="submit" { (label) }
    } }
}

#[allow(
    clippy::too_many_lines,
    reason = "accessible channel forms in a single HTML template"
)]
async fn render(
    state: &AppState,
    user: &WebUser,
    id: i64,
    error: Option<&str>,
) -> Result<Response, ApiError> {
    let role = access::role(&state.pool, id, user.current.account.id).await?;
    let invitations = access::invitations(&state.pool, user.current.account.id).await?;
    if role.is_none() && !invitations.contains(&id) {
        return Err(ApiError::NotFound);
    }
    let channel = account::find_by_id(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let grants = access::grants(&state.pool, id).await?;
    let transfer_to: Option<i64> =
        sqlx::query_scalar("SELECT transfer_to FROM video_channels WHERE account_id=$1")
            .bind(id)
            .fetch_one(&state.pool)
            .await
            .map_err(plamenu_db::DbError::from)?;
    let ids = grants
        .iter()
        .map(|g| g.account_id)
        .chain(transfer_to)
        .collect::<Vec<_>>();
    let people = account::find_by_ids(&state.pool, &ids).await?;
    let locale = user.locale;
    let target = format!("/web/video-channels/{id}");
    let body = html! {
        section.column.video-channel {
            h1 { (locale.text("video-channel-manage")) " — " (channel.display_name) }
            p { a href="/videos/channels" { (locale.text("video-channel-manage")) } " · "
                a href=(format!("/videos/explore?channel={id}")) { (locale.text("videos-explore-browse")) }
            }
            @if let Some(error)=error { p.settings__error role="alert" { (error) } }
            @if let Some(role)=role.as_deref() { p { (locale.text("video-channel-your-role")) " " (locale.text(&format!("video-channel-role-{role}"))) } }
            @if grants.iter().any(|g|g.account_id==user.current.account.id && !g.accepted) {
                section aria-labelledby="channel-invitation" {
                    h2 id="channel-invitation" { (locale.text("video-channel-invitation")) }
                    p { (locale.text("video-channel-roles-hint")) }
                    @if let Some(grant)=grants.iter().find(|g|g.account_id==user.current.account.id) {
                        p { (locale.text("video-channel-your-role")) " " (locale.text(&format!("video-channel-role-{}",grant.role))) }
                    }
                    (action_button(id,&user.csrf,"accept",&locale.text("video-channel-accept")))
                    (action_button(id,&user.csrf,"refuse",&locale.text("video-channel-refuse")))
                }
            }
            @if transfer_to==Some(user.current.account.id) {
                section aria-labelledby="channel-transfer" {
                    h2 id="channel-transfer" { (locale.text("video-channel-transfer-invitation")) }
                    p { (locale.text("video-channel-transfer-hint")) }
                    form.settings-form method="post" action=(&target) {
                        input type="hidden" name="csrf" value=(user.csrf);
                        input type="hidden" name="operation" value="accept_transfer";
                        label.settings-field { span.settings-field__label { (locale.text("video-channel-confirm")) " (@" (channel.username) ")" }
                            input name="confirm" required autocomplete="off" pattern=(channel.username);
                        }
                        button type="submit" { (locale.text("video-channel-accept-transfer")) }
                    }
                    (action_button(id,&user.csrf,"refuse_transfer",&locale.text("video-channel-refuse")))
                }
            }
            @if matches!(role.as_deref(),Some("owner"|"editor")) {
                h2 { (locale.text("video-channel-profile")) }
                form.settings-form method="post" action=(&target) {
                    input type="hidden" name="csrf" value=(user.csrf);
                    input type="hidden" name="operation" value="profile";
                    label.settings-field { span.settings-field__label { (locale.text("videos-channel-name")) }
                        input name="display_name" value=(channel.display_name) maxlength="120" required;
                    }
                    label.settings-field { span.settings-field__label { (locale.text("videos-description")) }
                        textarea name="description" rows="5" maxlength="1000" { (channel.note_source) }
                    }
                    button type="submit" { (locale.text("video-channel-save")) }
                }
            }
            @if role.as_deref()==Some("owner") {
                h2 { (locale.text("video-channel-collaborators")) }
                p { (locale.text("video-channel-roles-hint")) }
                ul {
                    @for grant in &grants {
                        @if let Some(person)=people.iter().find(|a|a.id==grant.account_id) {
                            li { "@" (person.username) " — " (locale.text(&format!("video-channel-role-{}",grant.role)))
                                " — " (locale.text(if grant.accepted {"video-channel-accepted"} else {"video-channel-pending"}))
                                form.settings-form method="post" action=(&target) {
                                    input type="hidden" name="csrf" value=(user.csrf);
                                    input type="hidden" name="operation" value="revoke";
                                    input type="hidden" name="username" value=(person.username);
                                    button type="submit" { (locale.text("video-channel-revoke")) " @" (person.username) }
                                }
                            }
                        }
                    }
                }
                form.settings-form method="post" action=(&target) {
                    input type="hidden" name="csrf" value=(user.csrf);
                    input type="hidden" name="operation" value="invite";
                    label.settings-field { span.settings-field__label { (locale.text("video-channel-local-person")) }
                        input name="username" maxlength="30" required autocomplete="off";
                    }
                    label.settings-field { span.settings-field__label { (locale.text("video-channel-role")) }
                        select name="role" {
                            option value="publisher" { (locale.text("video-channel-role-publisher")) }
                            option value="editor" { (locale.text("video-channel-role-editor")) }
                        }
                    }
                    button type="submit" { (locale.text("video-channel-invite")) }
                }
                details {
                    summary { (locale.text("video-channel-transfer")) }
                    p { (locale.text("video-channel-transfer-hint")) }
                    @if let Some(person)=people.iter().find(|a|Some(a.id)==transfer_to) {
                        p { (locale.text("video-channel-pending")) " — @" (person.username) }
                        (action_button(id,&user.csrf,"cancel_transfer",&locale.text("video-channel-cancel-transfer")))
                    }
                    form.settings-form method="post" action=(&target) {
                        input type="hidden" name="csrf" value=(user.csrf);
                        input type="hidden" name="operation" value="transfer";
                        label.settings-field { span.settings-field__label { (locale.text("video-channel-local-person")) }
                            input name="username" required maxlength="30" autocomplete="off";
                        }
                        button type="submit" { (locale.text("video-channel-request-transfer")) }
                    }
                }
                details {
                    summary { (locale.text("video-channel-delete")) }
                    p { (locale.text("video-channel-delete-hint")) }
                    form.settings-form method="post" action=(&target) {
                        input type="hidden" name="csrf" value=(user.csrf);
                        input type="hidden" name="operation" value="delete";
                        label.settings-field { span.settings-field__label { (locale.text("video-channel-confirm")) " (@" (channel.username) ")" }
                            input name="confirm" required autocomplete="off" pattern=(channel.username);
                        }
                        button type="submit" { (locale.text("video-channel-delete")) }
                    }
                }
            }
        }
    };
    Ok(layout::shell(&locale.text("video-channel-manage"), Some(user), &body).into_response())
}

pub async fn change(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Response {
    private(
        change_inner(&state, &user, id, &body)
            .await
            .unwrap_or_else(IntoResponse::into_response),
    )
}
async fn change_inner(
    state: &AppState,
    user: &WebUser,
    id: i64,
    body: &Bytes,
) -> Result<Response, ApiError> {
    let pairs = form_pairs(body).map_err(ApiError::BadRequest)?;
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return Ok(csrf_rejection());
    }
    let input = crate::video_channels::Change {
        operation: field(&pairs, "operation").unwrap_or_default().into(),
        username: field(&pairs, "username").map(str::to_owned),
        role: field(&pairs, "role").map(str::to_owned),
        display_name: field(&pairs, "display_name").map(str::to_owned),
        description: field(&pairs, "description").map(str::to_owned),
        confirm: field(&pairs, "confirm").map(str::to_owned),
    };
    match crate::video_channels::change(state, &user.current.account, id, &input).await {
        Ok(())
            if matches!(
                input.operation.as_str(),
                "delete" | "refuse" | "refuse_transfer"
            ) =>
        {
            Ok(redirect_to("/videos/channels"))
        }
        Ok(()) => Ok(redirect_to(&format!("/videos/channels/{id}"))),
        Err(ApiError::Unprocessable(message) | ApiError::Conflict(message)) => {
            let mut response = render(state, user, id, Some(&message)).await?;
            *response.status_mut() = StatusCode::UNPROCESSABLE_ENTITY;
            Ok(response)
        }
        Err(error) => Err(error),
    }
}
