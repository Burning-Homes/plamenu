//! Mount controls and scan progress remain inside the single Music workspace.
use super::{layout, session::WebUser, settings};
use crate::{AppState, error::ApiError, music_local::folders};
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::header,
    response::{IntoResponse, Response},
};
use maud::html;
use plamenu_db::role::permission;
pub async fn index(State(state): State<AppState>, user: WebUser) -> Result<Response, ApiError> {
    user.current.require_scope("read:statuses")?;
    let folders = folders::list(&state, user.current.account.id).await?;
    let l = user.locale;
    let operator = user.role.as_ref().is_some_and(|role| {
        role.can(permission::MANAGE_SETTINGS) && role.can(permission::MANAGE_USERS)
    });
    let body = html! {section.column.music-library{
        h1{(l.text("music-folders-title"))}(super::music::workspace_navigation(l,true,"folders"))
        p{(l.text("music-folders-hint"))}
        @if operator && state.config.music_local_roots.is_empty(){p.form-hint{(l.text("music-folders-no-roots"))}}
        @if operator && !state.config.music_local_roots.is_empty() {form.settings-form method="post" action="/web/music/folders/assign"{
            input type="hidden" name="csrf" value=(user.csrf);
            label for="music-folder-root"{(l.text("music-folders-root"))}
            select id="music-folder-root" name="root_id" required{@for root in &state.config.music_local_roots {option value=(root.id){(root.label)}}}
            label for="music-folder-owner"{(l.text("music-folders-owner"))}input id="music-folder-owner" name="username" type="text" maxlength="30" value=(user.current.account.username) required;
            label for="music-folder-directory"{(l.text("music-folders-directory"))}input id="music-folder-directory" name="directory" maxlength="2048";
            label for="music-folder-name"{(l.text("music-collection-name"))}input id="music-folder-name" name="name" maxlength="200" required;
            (scan_form(l,"new",&crate::music_local::options::Options::default()))
            button type="submit"{(l.text("music-folders-assign"))}
        }}
        @if folders.is_empty(){p{(l.text("music-folders-empty"))}}
        ul{@for folder in folders {li{
            a href=(format!("/music/collections/{}",folder.collection_id)){(state.config.music_local_roots.iter().find(|root|root.id==folder.root_id).map_or(folder.root_id.as_str(),|root|root.label.as_str()))" / "(folder.directory)}
            p{a href=(format!("/music/folders/{}/files",folder.id)){(l.text("music-folders-files"))}}
            p{(l.text(&format!("music-folders-state-{}",folder.state)))" · "(folder.indexed)" / "(folder.discovered)" · "(l.text("music-folders-failed"))" "(folder.failed)}
            @if folder.last_error.is_some(){p{(l.text(if folder.last_error.as_deref()==Some("Source mutation awaits recovery; check the original mount and files"){"music-folders-recovery-pending"}else{"music-folders-error"}))}}
            @if user.current.has_scope("write:media"){form method="post" action=(format!("/web/music/folders/{}/refresh",folder.id)){
                input type="hidden" name="csrf" value=(user.csrf);input type="hidden" name="revision" value=(folder.revision);
                label{input type="checkbox" name="watch" value="true" checked[folder.watch];(l.text("music-folders-watch"))}
                label{input type="checkbox" name="enabled" value="true" checked[folder.enabled];(l.text("music-folders-enabled"))}
                (scan_form(l,&folder.id.to_string(),&folder.options.0))
                button type="submit"{(l.text("music-folders-refresh"))}
            }form method="post" action=(format!("/web/music/folders/{}/disconnect",folder.id)){
                input type="hidden" name="csrf" value=(user.csrf);input type="hidden" name="revision" value=(folder.revision);p{(l.text("music-folders-disconnect-hint"))}button type="submit"{(l.text("music-folders-disconnect"))}
            }}
        }}}
    }};
    Ok((
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
        ],
        layout::shell(&l.text("music-folders-title"), Some(&user), &body),
    )
        .into_response())
}
fn form(body: &Bytes, user: &WebUser) -> Result<Vec<(String, String)>, ApiError> {
    let pairs = settings::form_pairs(body).map_err(ApiError::Unprocessable)?;
    if settings::field(&pairs, "csrf") != Some(user.csrf.as_str()) {
        return Err(ApiError::Forbidden("Invalid form token".into()));
    }
    Ok(pairs)
}
pub async fn assign(
    State(state): State<AppState>,
    user: WebUser,
    body: Bytes,
) -> Result<Response, ApiError> {
    user.current.require_scope("write:media")?;
    if !user.role.as_ref().is_some_and(|role| {
        role.can(permission::MANAGE_SETTINGS) && role.can(permission::MANAGE_USERS)
    }) {
        return Err(ApiError::Forbidden("Operator permission required".into()));
    }
    let pairs = form(&body, &user)?;
    let username = settings::field(&pairs, "username")
        .unwrap_or_default()
        .trim()
        .trim_start_matches('@');
    let owner = plamenu_db::account::find_local_by_username(&state.pool, username)
        .await?
        .ok_or_else(|| ApiError::Unprocessable("Choose an existing local account".into()))?;
    let input = folders::New {
        owner_id: owner.id,
        root_id: settings::field(&pairs, "root_id")
            .unwrap_or_default()
            .into(),
        directory: settings::field(&pairs, "directory")
            .unwrap_or_default()
            .into(),
        name: settings::field(&pairs, "name").unwrap_or_default().into(),
        watch: true,
        options: read_options(&pairs)?.unwrap_or_default(),
    };
    folders::assign(&state, &input).await?;
    Ok(settings::redirect_to("/music/folders"))
}
pub async fn refresh(
    State(state): State<AppState>,
    Path(folder): Path<i64>,
    user: WebUser,
    body: Bytes,
) -> Result<Response, ApiError> {
    user.current.require_scope("write:media")?;
    let pairs = form(&body, &user)?;
    let revision = settings::field(&pairs, "revision")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let choices = read_options(&pairs)?;
    folders::configure(
        &state,
        user.current.account.id,
        folder,
        revision,
        settings::field(&pairs, "watch") == Some("true"),
        settings::field(&pairs, "enabled") == Some("true"),
        choices.as_ref(),
    )
    .await?;
    Ok(settings::redirect_to("/music/folders"))
}
#[derive(Default, serde::Deserialize)]
pub struct FilePage {
    before: Option<i64>,
}
pub async fn files(
    State(state): State<AppState>,
    Path(folder): Path<i64>,
    user: WebUser,
    axum::extract::Query(page): axum::extract::Query<FilePage>,
) -> Result<Response, ApiError> {
    user.current.require_scope("read:statuses")?;
    let assignment = folders::get(&state, user.current.account.id, folder).await?;
    let mut files =
        crate::music_local::files::list(&state, user.current.account.id, folder, page.before)
            .await?;
    let next = if files.len() > 60 {
        files.truncate(60);
        files.last().map(|file| file.id)
    } else {
        None
    };
    let root = state
        .config
        .music_local_roots
        .iter()
        .find(|root| root.id == assignment.root_id);
    let l = user.locale;
    let body = html! {section.column.music-library{
        h1{(l.text("music-folders-files"))}(super::music::workspace_navigation(l,true,"folders"))
        p{a href="/music/folders"{(l.text("music-folders-title"))} " · " a href=(format!("/music/collections/{}",assignment.collection_id)){(l.text("music-folders-library"))}}
        ul{@for file in files{li{
            p{code{(file.relative_path)}" · "(l.text(&format!("music-folders-file-{}",file.state)))}
            @if file.last_error.is_some(){p{(l.text("music-folders-file-warning"))}}
            @if let Some(asset)=file.asset_id{a href=(format!("/music/assets/{asset}")){(l.text("music-folders-track"))}}
            @if assignment.enabled && file.state=="ignored" && user.current.has_scope("write:media"){form method="post" action=(format!("/web/music/folders/{folder}/files/{}/restore",file.id)){
                input type="hidden" name="csrf" value=(user.csrf);input type="hidden" name="revision" value=(assignment.revision);button type="submit"{(l.text("music-folders-restore"))}
            }}
            @if assignment.enabled && file.available && file.state=="ready" && user.current.has_scope("write:media"){
                @if root.is_some_and(|root|root.allow_rename){form method="post" action=(format!("/web/music/folders/{folder}/files/{}/mutate",file.id)){
                    input type="hidden" name="csrf" value=(user.csrf);input type="hidden" name="revision" value=(assignment.revision);input type="hidden" name="operation" value="rename";
                    label for=(format!("music-file-name-{}",file.id)){(l.text("music-folders-new-name"))}
                    input id=(format!("music-file-name-{}",file.id)) name="name" maxlength="255" required value=(file.relative_path.rsplit('/').next().unwrap_or_default());button type="submit"{(l.text("music-folders-rename"))}
                }}
                @if root.is_some_and(|root|root.allow_tag_writeback){form method="post" action=(format!("/web/music/folders/{folder}/files/{}/mutate",file.id)){
                    input type="hidden" name="csrf" value=(user.csrf);input type="hidden" name="revision" value=(assignment.revision);input type="hidden" name="operation" value="write_tags";
                    p{(l.text("music-folders-tags-hint"))}button type="submit"{(l.text("music-folders-write-tags"))}
                }}
                @if root.is_some_and(|root|root.allow_delete){form method="post" action=(format!("/web/music/folders/{folder}/files/{}/mutate",file.id)){
                    input type="hidden" name="csrf" value=(user.csrf);input type="hidden" name="revision" value=(assignment.revision);input type="hidden" name="operation" value="delete";
                    label{input type="checkbox" name="confirm" value="true" required;(l.text("music-folders-delete-confirm"))}
                    button type="submit"{(l.text("music-folders-delete-source"))}
                }}
            }
        }}}
        @if let Some(next)=next{a href=(format!("/music/folders/{folder}/files?before={next}")){(l.text("music-folders-more"))}}
    }};
    Ok((
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
        ],
        layout::shell(&l.text("music-folders-files"), Some(&user), &body),
    )
        .into_response())
}
pub async fn mutate_file(
    State(state): State<AppState>,
    Path((folder, file)): Path<(i64, i64)>,
    user: WebUser,
    body: Bytes,
) -> Result<Response, ApiError> {
    user.current.require_scope("write:media")?;
    let pairs = form(&body, &user)?;
    let revision = settings::field(&pairs, "revision")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let input = match settings::field(&pairs, "operation") {
        Some("rename") => crate::music_local::files::Mutation::Rename {
            revision,
            name: settings::field(&pairs, "name").unwrap_or_default().into(),
        },
        Some("write_tags") => crate::music_local::files::Mutation::WriteTags { revision },
        Some("delete") => crate::music_local::files::Mutation::Delete {
            revision,
            confirm: settings::field(&pairs, "confirm") == Some("true"),
        },
        _ => return Err(ApiError::Unprocessable("Invalid source operation".into())),
    };
    crate::music_local::files::mutate(&state, user.current.account.id, folder, file, &input)
        .await?;
    Ok(settings::redirect_to(&format!(
        "/music/folders/{folder}/files"
    )))
}

pub async fn disconnect(
    State(state): State<AppState>,
    Path(folder): Path<i64>,
    user: WebUser,
    body: Bytes,
) -> Result<Response, ApiError> {
    user.current.require_scope("write:media")?;
    let pairs = form(&body, &user)?;
    let revision = settings::field(&pairs, "revision")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    folders::disconnect(&state, user.current.account.id, folder, revision).await?;
    Ok(settings::redirect_to("/music/folders"))
}
pub async fn restore(
    State(state): State<AppState>,
    Path((folder, file)): Path<(i64, i64)>,
    user: WebUser,
    body: Bytes,
) -> Result<Response, ApiError> {
    user.current.require_scope("write:media")?;
    let pairs = form(&body, &user)?;
    let revision = settings::field(&pairs, "revision")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    crate::music_local::files::restore(&state, user.current.account.id, folder, file, revision)
        .await?;
    Ok(settings::redirect_to(&format!(
        "/music/folders/{folder}/files"
    )))
}

fn scan_form(
    l: super::i18n::Locale,
    prefix: &str,
    options: &crate::music_local::options::Options,
) -> maud::Markup {
    html! {details.music-folder-scan-options{summary{(l.text("music-folders-scan-options"))}
        input type="hidden" name="scan_options" value="true";
        label{input type="checkbox" name="recursive" value="true" checked[options.recursive];(l.text("music-folders-recursive"))}
        label for=(format!("music-extensions-{prefix}")){(l.text("music-folders-extensions"))}
        input id=(format!("music-extensions-{prefix}")) name="extensions" type="text" maxlength="100" value=(options.extensions.join(", "));
        label for=(format!("music-exclusions-{prefix}")){(l.text("music-folders-exclusions"))}
        textarea id=(format!("music-exclusions-{prefix}")) name="exclude" rows="3" maxlength="8192"{(options.exclude.join("\n"))}
        p{(l.text("music-folders-exclusions-hint"))}
    }}
}
fn read_options(
    pairs: &[(String, String)],
) -> Result<Option<crate::music_local::options::Options>, ApiError> {
    if settings::field(pairs, "scan_options") != Some("true") {
        return Ok(None);
    }
    let options = crate::music_local::options::Options {
        recursive: settings::field(pairs, "recursive") == Some("true"),
        extensions: settings::field(pairs, "extensions")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect(),
        exclude: settings::field(pairs, "exclude")
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect(),
    };
    options.validate()?;
    Ok(Some(options))
}
