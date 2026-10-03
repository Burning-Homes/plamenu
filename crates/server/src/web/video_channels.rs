//! HTML-first channel authority. Every action is reachable without scripts.
use super::{
    layout,
    session::{WebUser, csrf_rejection},
    settings::{field, form_pairs, redirect_to},
};
use crate::{AppState, error::ApiError};
use axum::{
    body::Bytes,
    extract::{Multipart, Path, Query, State},
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

#[derive(Default, serde::Deserialize)]
pub struct ChannelQuery {
    before: Option<i64>,
}
pub async fn index(
    State(state): State<AppState>,
    user: WebUser,
    Query(query): Query<ChannelQuery>,
) -> Result<Response, ApiError> {
    let mut ids = access::available(&state.pool, user.current.account.id)
        .await?
        .into_iter()
        .map(|c| c.account_id)
        .collect::<Vec<_>>();
    let roles = access::roles(&state.pool, user.current.account.id).await?;
    let invitations = access::invitations(&state.pool, user.current.account.id).await?;
    ids.extend(&invitations);
    ids.sort_unstable();
    ids.dedup();
    ids.reverse();
    ids.retain(|id| query.before.is_none_or(|before| *id < before));
    let more = ids.len() > 20;
    ids.truncate(20);
    let mut accounts = account::find_by_ids(&state.pool, &ids).await?;
    accounts.sort_unstable_by_key(|account| std::cmp::Reverse(account.id));
    let locale = user.locale;
    let body = html! {
        section.column.video-workspace {
            h1 { (locale.text("video-channel-manage")) }
            nav.video-workspace__actions {a.video-workspace__primary href="/videos/channels/new" {(locale.text("videos-new-channel"))}}
            (super::video_workspace::navigation("/videos/channels",locale))
            ul.video-workspace__library.video-channel-list {
                @for channel in &accounts {
                    li.video-workspace__row {
                        img.video-channel__avatar src=(crate::entities::avatar_url(&state.config.domain,channel,false).unwrap_or_else(||"/static/missing.png".into())) alt="" width="48" height="48" loading="lazy";
                        div.video-workspace__item {
                            a.video-workspace__title href=(format!("/videos/channels/{}",channel.id)) {(channel.display_name)}
                            p.video-workspace__meta {"@" (channel.username) " · "
                                @if invitations.contains(&channel.id) {(locale.text("video-channel-invitation"))}
                                @else if let Some(role)=roles.get(&channel.id) {(locale.text(&format!("video-channel-role-{role}")))}
                            }
                        }
                        a.video-channel__view href=(format!("/videos/explore?channel={}",channel.id)) aria-label=(format!("{}: {}",locale.text("studio-view-channel"),channel.display_name)) {(locale.text("studio-view-channel"))}
                    }
                }
            }
            @if accounts.is_empty() { p { (locale.text("video-channel-empty")) } }
            @if more && let Some(last)=ids.last(){a.pager__more href=(format!("/videos/channels?before={last}")) {(locale.text("studio-older"))}}
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
        section.column.video-channel.video-form {
            h1 { (locale.text("video-channel-manage")) " — " (channel.display_name) }
            p { a href="/videos/channels" { (locale.text("video-channel-manage")) } " · "
                a href=(format!("/videos/explore?channel={id}")) { (locale.text("videos-explore-browse")) }
                @if role.as_deref().is_some_and(|role| matches!(role,"owner" | "editor")) {
                    " · " a href=(format!("/videos/channels/{id}/analytics")) { (locale.text("video-analytics-title")) }
                }
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
                form.settings-form method="post" enctype="multipart/form-data" action=(format!("{target}/profile")) {
                    input type="hidden" name="csrf" value=(user.csrf);
                    fieldset.settings-form__group {
                    legend {(locale.text("video-channel-profile"))}
                    label.settings-field { span.settings-field__label { (locale.text("videos-channel-name")) }
                        input name="display_name" value=(channel.display_name) maxlength="120" required;
                    }
                    label.settings-field { span.settings-field__label { (locale.text("videos-description")) }
                        textarea name="description" rows="5" maxlength="1000" { (channel.note_source) }
                    }
                    (super::profile_media::fields(&channel, locale))
                    div.settings-form__actions {
                        button type="submit" { (locale.text("video-channel-save")) }
                        a href="/videos/channels" {(locale.text("studio-cancel"))}
                    }
                    }
                }
            }
            @if role.as_deref()==Some("owner") {
                div.video-form__heading {
                    h2 { (locale.text("video-channel-collaborators")) }
                    (super::video_workspace::help(locale,"video-channel-collaborators",&html! {p {(locale.text("video-channel-roles-hint"))}}))
                }
                @if grants.is_empty() {
                    p.settings-field__hint {(locale.text("studio-collaborators-empty"))}
                } @else {
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

/// Multipart profile edit; collaborator actions retain their established wire format.
pub async fn profile(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    mut multipart: Multipart,
) -> Response {
    let form = match super::profile_media::read(&mut multipart).await {
        Ok(form) => form,
        Err(response) => return private(response),
    };
    if !user.csrf_ok(field(&form.pairs, "csrf").unwrap_or_default()) {
        return private(csrf_rejection());
    }
    let changes = crate::video_channels::ProfileEdit {
        display_name: field(&form.pairs, "display_name").map(str::to_owned),
        description: field(&form.pairs, "description").map(str::to_owned),
        avatar: form.avatar,
        header: form.header,
        avatar_description: field(&form.pairs, "avatar_description").map(str::to_owned),
        header_description: field(&form.pairs, "header_description").map(str::to_owned),
    };
    private(
        match crate::video_channels::update_profile(&state, &user.current.account, id, changes)
            .await
        {
            Ok(()) => redirect_to(&format!("/videos/channels/{id}")),
            Err(error) => error.into_response(),
        },
    )
}
