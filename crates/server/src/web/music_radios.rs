//! Station catalogue and owner settings use ordinary forms.
use super::{
    layout,
    session::{MaybeWebUser, WebUser},
    settings,
};
use crate::{
    AppState,
    error::ApiError,
    music_radios::{self, New, Radio},
};
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
use serde::Deserialize;
#[derive(Default, Deserialize)]
pub struct Page {
    pub q: Option<String>,
    pub mine: Option<bool>,
    pub max_id: Option<i64>,
}
fn response(title: &str, user: Option<&WebUser>, body: &Markup) -> Response {
    (
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
        ],
        layout::shell(title, user, body),
    )
        .into_response()
}
fn invalid_name(name: &str) -> bool {
    name.trim().is_empty() || name.chars().count() > 200 || name.contains('\0')
}
fn fields(l: super::i18n::Locale, radio: Option<&Radio>, draft: Option<&New>) -> Markup {
    let name = draft
        .map(|r| r.name.as_str())
        .or_else(|| radio.map(|r| r.name.as_str()))
        .unwrap_or_default();
    let description = draft
        .map(|r| r.description.as_str())
        .or_else(|| radio.map(|r| r.description.as_str()))
        .unwrap_or_default();
    let config = draft
        .map(|r| r.config.as_str())
        .or_else(|| radio.map(|r| r.config.as_str()))
        .unwrap_or_default();
    let visibility = draft
        .map(|r| r.visibility.as_str())
        .or_else(|| radio.map(|r| r.visibility.as_str()))
        .unwrap_or("private");
    let name_invalid = draft.is_some() && invalid_name(name);
    html! {
        label for="music-radio-name"{(l.text("music-radio-name"))}
        input id="music-radio-name" name="name" required maxlength="200" value=(name) aria-invalid=[name_invalid.then_some("true")] aria-describedby=[name_invalid.then_some("radio-create-error")];
        label for="music-radio-description"{(l.text("music-radio-description"))}
        textarea id="music-radio-description" name="description" maxlength="4000"{(description)}
        label for="music-radio-visibility"{(l.text("music-radio-visibility"))}
        select id="music-radio-visibility" name="visibility"{
            @for (value,key) in [("private","music-assets-private"),("local","music-assets-local"),("public","music-public")]{option value=(value) selected[visibility==value]{(l.text(key))}}
        }
        label for="music-radio-config"{(l.text("music-radio-config"))}
        textarea id="music-radio-config" name="config" maxlength="50000" aria-describedby="music-radio-config-hint"{(config)}
        p.form-hint id="music-radio-config-hint"{(l.text("music-radio-config-hint"))}
        @if let Some(radio)=radio {input type="hidden" name="revision" value=(radio.revision);}
    }
}
fn creation_form(user: &WebUser, draft: Option<&New>) -> Markup {
    html! {form.settings-form method="post" action="/web/music/radios/create" {
        input type="hidden" name="csrf" value=(&user.csrf);
        (fields(user.locale, None, draft))
        button type="submit" {(user.locale.text("music-radio-create"))}
    }}
}
fn creation_problem(user: &WebUser, draft: &New, message: &str) -> Response {
    let l = user.locale;
    let body = html! {section.column.music-library {
        h1 {(l.text("music-radio-create"))}
        (super::music::workspace_navigation(l, true, "radios"))
        div.settings__error id="radio-create-error" role="alert" tabindex="-1" autofocus {
            @if invalid_name(&draft.name) {
                p {(l.text("music-radio-name-error"))}
            } @else {
                p {(l.text("music-radio-create-error"))}
                p lang="en" {(message)}
            }
        }
        (creation_form(user, Some(draft)))
        a href="/music/radios" {(l.text("music-radios-title"))}
    }};
    (
        axum::http::StatusCode::UNPROCESSABLE_ENTITY,
        response(&l.text("music-radio-create"), Some(user), &body),
    )
        .into_response()
}
pub async fn index(
    State(state): State<AppState>,
    MaybeWebUser(user): MaybeWebUser,
    Query(page): Query<Page>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let l = user
        .as_ref()
        .map_or_else(|| super::i18n::Locale::from_headers(&headers), |u| u.locale);
    if let Some(response) = super::session::preview_redirect(
        &user,
        state
            .settings_cache
            .get(&state.pool)
            .await?
            .timeline_preview_federated,
    ) {
        return Ok(response);
    }
    let mut items = music_radios::search(
        &state,
        user.as_ref().map(|u| u.current.account.id),
        page.mine.unwrap_or(false),
        page.max_id,
        page.q.as_deref().unwrap_or_default(),
    )
    .await?;
    let before = if items.len() > 50 {
        items.truncate(50);
        items.last().map(|r| r.id)
    } else {
        None
    };
    let body = html! {section.column.music-library{
        h1{(l.text("music-radios-title"))}
        p{(l.text("music-radios-hint"))}
        (super::music::workspace_navigation(l,user.is_some(),"radios"))
        nav.video-explore__links aria-label=(l.text("music-radios-title")){
            a href="/music"{(l.text("music-title"))}a href="/music/radios"{(l.text("music-public"))}
            @if user.is_some(){a href="/music/radios?mine=true"{(l.text("music-radios-mine"))}}
        }
        form.settings-form method="get" action="/music/radios"{
            @if page.mine.unwrap_or(false){input type="hidden" name="mine" value="true";}
            label for="music-radio-search"{(l.text("music-radios-search"))}input id="music-radio-search" type="search" name="q" maxlength="200" value=(page.q.as_deref().unwrap_or_default());button type="submit"{(l.text("music-search-submit"))}
        }
        @if let Some(user)=&user{details{summary{(l.text("music-radio-create"))}
            (creation_form(user, None))
        }}
        @if items.is_empty(){p{(l.text("music-radios-empty"))}}
        ul{@for radio in &items{li{a href=(format!("/music/radios/{}",radio.id)){(radio.name)}}}}
        @if let Some(before)=before{a href=(format!("/music/radios?mine={}&max_id={before}&{}",page.mine.unwrap_or(false),serde_urlencoded::to_string([("q",page.q.as_deref().unwrap_or_default())]).unwrap_or_default())){(l.text("music-older"))}}
    }};
    Ok(response(
        &l.text("music-radios-title"),
        user.as_ref(),
        &body,
    ))
}
pub async fn show(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    MaybeWebUser(user): MaybeWebUser,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let l = user
        .as_ref()
        .map_or_else(|| super::i18n::Locale::from_headers(&headers), |u| u.locale);
    let radio = music_radios::get(&state, id, user.as_ref().map(|u| u.current.account.id)).await?;
    let collected = super::music_saved::control(
        &state,
        user.as_ref(),
        crate::music_saved::Reference {
            kind: crate::music_saved::Kind::Radio,
            id: id.to_string(),
        },
        user.as_ref()
            .is_some_and(|user| user.current.account.id == radio.owner_id),
        &format!("/music/radios/{id}"),
    )
    .await?;
    let owner = user
        .as_ref()
        .filter(|u| u.current.account.id == radio.owner_id);
    let grants = if owner.is_some() {
        music_radios::grants(&state, radio.owner_id, id).await?
    } else {
        Vec::new()
    };
    let body = html! {section.column.music-library{
        h1{(radio.name)}
        (super::music::workspace_navigation(l,user.is_some(),"radios"))
        (super::music_activity::link(l,"radios",radio.id))
        (super::webmentions::for_target(
        &state,
        &format!("https://{}/music/radios/{id}", state.config.domain),
        l,
    )
    .await?)
        (collected)
        nav.video-explore__links aria-label=(l.text("music-radios-title")){a href="/music/radios"{(l.text("music-radios-title"))}a href="/music"{(l.text("music-title"))}}
        p.music-radio-description{(radio.description)}
        p{(l.text(match radio.visibility.as_str(){"public"=>"music-public","local"=>"music-assets-local",_=>"music-assets-private"}))}
        @if let Some(user)=owner{
            details{summary{(l.text("music-radio-edit"))}
                form.settings-form method="post" action=(format!("/web/music/radios/{id}/edit")){
                    input type="hidden" name="csrf" value=(user.csrf);(fields(l,Some(&radio),None))
                    button type="submit"{(l.text("music-radio-save"))}
                }
            }
            section{h2{(l.text("music-assets-share"))}
                form.settings-form method="post" action=(format!("/web/music/radios/{id}/grant")){
                    input type="hidden" name="csrf" value=(user.csrf);
                    label for="music-radio-member"{(l.text("music-assets-member"))}
                    input id="music-radio-member" name="member" required maxlength="200";
                    button type="submit" name="operation" value="grant"{(l.text("music-assets-grant"))}
                }
                ul{@for (_,member) in &grants{li{(member)
                    form.inline-form method="post" action=(format!("/web/music/radios/{id}/grant")){
                        input type="hidden" name="csrf" value=(user.csrf);input type="hidden" name="member" value=(member);
                        button type="submit" name="operation" value="revoke"{(l.text("music-assets-revoke"))}
                    }
                }}}
            }
            details{summary{(l.text("music-radio-delete"))}
                form.settings-form method="post" action=(format!("/web/music/radios/{id}/delete")){
                    input type="hidden" name="csrf" value=(user.csrf);input type="hidden" name="revision" value=(radio.revision);
                    p{(l.text("music-radio-delete-hint"))}button type="submit"{(l.text("music-radio-delete"))}
                }
            }
        }
    }};
    Ok(response(&radio.name, user.as_ref(), &body))
}
fn form(user: &WebUser, body: &Bytes) -> Result<Vec<(String, String)>, ApiError> {
    user.current.require_scope("write:statuses")?;
    let pairs = settings::form_pairs(body).map_err(ApiError::Unprocessable)?;
    if settings::field(&pairs, "csrf") != Some(user.csrf.as_str()) {
        return Err(ApiError::Forbidden("Invalid form token".into()));
    }
    Ok(pairs)
}
fn attributes(pairs: &[(String, String)]) -> New {
    let field = |key| settings::field(pairs, key).unwrap_or_default().to_owned();
    New {
        name: field("name"),
        description: field("description"),
        config: field("config"),
        visibility: field("visibility"),
    }
}
fn revision(pairs: &[(String, String)]) -> Result<i64, ApiError> {
    settings::field(pairs, "revision")
        .and_then(|s| s.parse().ok())
        .filter(|v| *v > 0)
        .ok_or_else(|| ApiError::Unprocessable("Invalid radio revision".into()))
}
pub async fn create(
    State(state): State<AppState>,
    user: WebUser,
    body: Bytes,
) -> Result<Response, ApiError> {
    let pairs = form(&user, &body)?;
    let draft = attributes(&pairs);
    let radio = match music_radios::create(&state, user.current.account.id, &draft).await {
        Ok(radio) => radio,
        Err(ApiError::Unprocessable(message)) => {
            return Ok(creation_problem(&user, &draft, &message));
        }
        Err(error) => return Err(error),
    };
    Ok(settings::redirect_to(&format!(
        "/music/radios/{}",
        radio.id
    )))
}
pub async fn edit(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let pairs = form(&user, &body)?;
    music_radios::update(
        &state,
        user.current.account.id,
        id,
        revision(&pairs)?,
        &attributes(&pairs),
    )
    .await?;
    Ok(settings::redirect_to(&format!("/music/radios/{id}")))
}
pub async fn delete(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let pairs = form(&user, &body)?;
    music_radios::delete(&state, user.current.account.id, id, revision(&pairs)?).await?;
    Ok(settings::redirect_to("/music/radios?mine=true"))
}
pub async fn grant(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let pairs = form(&user, &body)?;
    let member = plamenu_db::account::find_local_by_username(
        &state.pool,
        settings::field(&pairs, "member").unwrap_or_default(),
    )
    .await?
    .ok_or(ApiError::NotFound)?;
    let enabled = match settings::field(&pairs, "operation") {
        Some("grant") => true,
        Some("revoke") => false,
        _ => return Err(ApiError::Unprocessable("Choose grant or revoke".into())),
    };
    music_radios::grant(&state, user.current.account.id, id, member.id, enabled).await?;
    Ok(settings::redirect_to(&format!("/music/radios/{id}")))
}
