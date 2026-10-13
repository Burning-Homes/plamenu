//! Downloads remain a child of the single Music workspace and work without JS.
use super::{layout, session::WebUser, settings};
use crate::{
    AppState,
    error::ApiError,
    music_downloads::{self, Download, Request},
    music_queues::Source,
};
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::header,
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
#[derive(Default, serde::Deserialize)]
pub struct Page {
    kind: Option<String>,
    id: Option<String>,
    before: Option<i64>,
}
fn request(
    kind: Option<&str>,
    id: Option<&str>,
    label: Option<String>,
) -> Result<Request, ApiError> {
    let mut input = Request {
        sources: Vec::new(),
        kind: None,
        id: None,
        label,
    };
    match (kind, id) {
        (Some(kind), Some(id)) if matches!(kind, "asset" | "episode" | "remote" | "status") => {
            let mut source = Source {
                asset_id: None,
                episode_id: None,
                remote_id: None,
                status_id: None,
            };
            match kind {
                "asset" => source.asset_id = Some(id.into()),
                "episode" => source.episode_id = Some(id.into()),
                "remote" => source.remote_id = Some(id.into()),
                _ => source.status_id = Some(id.into()),
            }
            input.sources.push(source);
        }
        (Some(kind), Some(id)) => {
            input.kind = Some(kind.into());
            input.id = Some(id.into());
        }
        (None, None) => {}
        _ => return Err(ApiError::NotFound),
    }
    Ok(input)
}
fn response(user: &WebUser, body: &Markup) -> Response {
    (
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
        ],
        layout::shell(&user.locale.text("music-downloads-title"), Some(user), body),
    )
        .into_response()
}
pub async fn index(
    State(state): State<AppState>,
    user: WebUser,
    Query(page): Query<Page>,
) -> Result<Response, ApiError> {
    user.current.require_scope("read:statuses")?;
    let l = user.locale;
    let preview = if page.kind.is_some() {
        Some(
            music_downloads::preview(
                &state,
                user.current.account.id,
                &request(page.kind.as_deref(), page.id.as_deref(), None)?,
            )
            .await?,
        )
    } else {
        None
    };
    let mut rows = music_downloads::list(&state, user.current.account.id, page.before).await?;
    let next = if rows.len() > 50 {
        rows.truncate(50);
        rows.last().map(|row| row.id)
    } else {
        None
    };
    let mut labels = Vec::new();
    for row in &rows {
        labels.push(row.display_label(&state).await?);
    }
    let body = html! {section.column.music-library {h1{(l.text("music-downloads-title"))}(super::music::workspace_navigation(l,true,"downloads"))
        p{(l.text("music-downloads-hint"))}
        @if let Some((label,items))=preview {
            form.settings-form method="post" action="/web/music/downloads/create" {
                input type="hidden" name="csrf" value=(user.csrf);input type="hidden" name="kind" value=(page.kind.as_deref().unwrap_or_default());input type="hidden" name="id" value=(page.id.as_deref().unwrap_or_default());
                label for="music-download-label"{(l.text("music-download-label"))}input id="music-download-label" name="label" required maxlength="256" value=(label);
                ol{@for item in items {li{(item.title) @if !item.artist.is_empty(){" — "(item.artist)}}}}
                button type="submit"{(l.text("music-download-create"))}
            }
        }
        @if rows.is_empty() {p{(l.text("music-downloads-empty"))}}
        ul{@for (row,label) in rows.iter().zip(labels) {li{a href=(format!("/music/downloads/{}",row.id)){(label)}" · "(l.text(status(&row.state)))}}}
        @if let Some(next)=next {a href=(format!("/music/downloads?before={next}")){(l.text("music-older"))}}
    }};
    Ok(response(&user, &body))
}
fn status(state: &str) -> &'static str {
    match state {
        "queued" => "music-download-queued",
        "running" => "music-download-running",
        "ready" => "music-download-ready",
        "failed" => "music-download-failed",
        _ => "music-download-cancelled",
    }
}
fn actions(user: &WebUser, row: &Download) -> Markup {
    let l = user.locale;
    html! {
        @if matches!(row.state.as_str(),"queued"|"running") {form method="post" action=(format!("/web/music/downloads/{}/cancel",row.id)){
            input type="hidden" name="csrf" value=(user.csrf);input type="hidden" name="revision" value=(row.revision);button{(l.text("music-download-cancel"))}
        }}
        form method="post" action=(format!("/web/music/downloads/{}/delete",row.id)){
            input type="hidden" name="csrf" value=(user.csrf);input type="hidden" name="revision" value=(row.revision);button{(l.text("music-download-delete"))}
        }
    }
}
pub async fn show(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    user.current.require_scope("read:statuses")?;
    let row = music_downloads::get(&state, user.current.account.id, id).await?;
    let value = row.entity(&state).await?;
    let l = user.locale;
    let ready = music_downloads::authorize_file(&state, user.current.account.id, id)
        .await
        .is_ok();
    let body = html! {section.column.music-library.music-download-detail data-state=(row.state) {h1{(value["label"].as_str().unwrap_or_default())}(super::music::workspace_navigation(l,true,"downloads"))
        p role="status"{(l.text(status(&row.state)))" · "(row.completed_items)" / "(value["item_count"])}
        progress value=(row.completed_items) max=(value["item_count"].as_u64().unwrap_or(1)) aria-label=(l.text("music-download-progress")) {}
        p{a href=(format!("/music/downloads/{id}")){(l.text("music-download-refresh"))}}
        @if ready {p{a href=(format!("/music/downloads/{id}/file")){(l.text("music-download-file"))}" · "(row.size_bytes)" bytes"}}
        p{(l.text("music-download-expires"))" "(value["expires_at"].as_str().unwrap_or_default())}
        @if row.error.is_some(){p.settings__error{(l.text("music-download-error"))}}
        ol{@for item in value["items"].as_array().into_iter().flatten() {li{
            @if let Some(title)=item["source"]["title"].as_str(){(title)}@else{(l.text("music-download-unavailable"))}
        }}}
        @if !row.item_errors.as_array().is_none_or(Vec::is_empty){p{(l.text("music-download-item-errors"))}}
        (actions(&user,&row))
    }};
    Ok(response(&user, &body))
}
fn fields(user: &WebUser, body: &Bytes) -> Result<Vec<(String, String)>, ApiError> {
    user.current.require_scope("write:statuses")?;
    let fields = settings::form_pairs(body).map_err(ApiError::Unprocessable)?;
    if settings::field(&fields, "csrf") != Some(user.csrf.as_str()) {
        return Err(ApiError::Forbidden("Invalid form token".into()));
    }
    Ok(fields)
}
pub async fn create(
    State(state): State<AppState>,
    user: WebUser,
    body: Bytes,
) -> Result<Response, ApiError> {
    let fields = fields(&user, &body)?;
    let mut input = request(
        settings::field(&fields, "kind"),
        settings::field(&fields, "id"),
        settings::field(&fields, "label").map(str::to_owned),
    )?;
    for (kind, value) in &fields {
        let mut source = Source {
            asset_id: None,
            status_id: None,
            remote_id: None,
            episode_id: None,
        };
        match kind.as_str() {
            "asset_id" => source.asset_id = Some(value.clone()),
            "status_id" => source.status_id = Some(value.clone()),
            "remote_id" => source.remote_id = Some(value.clone()),
            "episode_id" => source.episode_id = Some(value.clone()),
            _ => continue,
        }
        input.sources.push(source);
    }
    let row = music_downloads::create(&state, user.current.account.id, &input).await?;
    Ok(settings::redirect_to(&format!(
        "/music/downloads/{}",
        row.id
    )))
}
async fn mutate(
    state: &AppState,
    user: &WebUser,
    id: i64,
    body: &Bytes,
    cancel: bool,
) -> Result<Response, ApiError> {
    let fields = fields(user, body)?;
    let revision = settings::field(&fields, "revision")
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| ApiError::Unprocessable("Invalid music download revision".into()))?;
    music_downloads::remove(state, user.current.account.id, id, revision, cancel).await?;
    Ok(settings::redirect_to("/music/downloads"))
}
pub async fn cancel(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    mutate(&state, &user, id, &body, true).await
}
pub async fn delete(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    mutate(&state, &user, id, &body, false).await
}
pub(super) fn link(user: &WebUser, kind: &str, id: i64) -> Markup {
    html! {p{a href=(format!("/music/downloads?kind={kind}&id={id}")){(user.locale.text("music-download-create"))}}}
}

pub(super) fn selection_form(user: &WebUser) -> Markup {
    html! {form id="music-download-selection" method="post" action="/web/music/downloads/create" {
        input type="hidden" name="csrf" value=(user.csrf);
        button type="submit"{(user.locale.text("music-download-selected"))}
    }}
}
pub(super) fn checkbox(user: &WebUser, kind: &str, id: i64) -> Markup {
    html! {label {input form="music-download-selection" type="checkbox" name=(format!("{kind}_id")) value=(id);(user.locale.text("music-download-select"))}}
}
