//! Human-facing Webxdc session flows. Merely seeing an invitation never
//! downloads or starts code: resolving, joining and opening are explicit.

use axum::Json;
use axum::extract::{DefaultBodyLimit, Form, FromRequest, Multipart, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use maud::{Markup, html};
use plamenu_db::account;
use plamenu_db::role::permission;
use plamenu_db::webxdc::{self, Guest, Membership, Session};
use serde::Deserialize;
use serde_json::{Value, json};

use super::i18n::Locale;
use super::layout;
use super::pages::anon_nav;
use super::session::{MaybeWebUser, WebUser};
use crate::error::ApiError;
use crate::{AppState, webxdc as protocol};

pub(crate) fn storage_size(bytes: i64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let (unit, name) = if bytes < 1024 * 1024 {
        (1024, "KiB")
    } else {
        (1024 * 1024, "MiB")
    };
    format!("{}.{:02} {name}", bytes / unit, (bytes % unit) * 100 / unit)
}

const GUEST_COOKIE_MAX_AGE: i64 = 30 * 24 * 60 * 60;

#[derive(Debug)]
struct GuestContext {
    guest: Guest,
    csrf: String,
}

fn local_session(state: &AppState, session: &Session) -> bool {
    session.coordinator_uri == format!("https://{}/webxdc/{}", state.config.domain, session.id)
}

fn guest_cookie_name(session_id: i64) -> String {
    format!("__Host-plamenu_webxdc_guest_{session_id}")
}

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|pair| pair.trim_start().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value)
}

fn guest_csrf(raw_token: &str) -> String {
    crate::auth::hash_secret(&format!("plamenu-webxdc-guest-csrf:{raw_token}"))
}

fn set_guest_cookie(session_id: i64, token: &str) -> String {
    format!(
        "{}={token}; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age={GUEST_COOKIE_MAX_AGE}",
        guest_cookie_name(session_id)
    )
}

fn clear_guest_cookie(session_id: i64) -> String {
    format!(
        "{}=; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=0",
        guest_cookie_name(session_id)
    )
}

async fn guest_context(
    state: &AppState,
    session_id: i64,
    headers: &HeaderMap,
) -> Result<Option<GuestContext>, ApiError> {
    let name = guest_cookie_name(session_id);
    let Some(raw_token) = cookie_value(headers, &name) else {
        return Ok(None);
    };
    let Some(guest) = webxdc::guest_by_token(
        &state.pool,
        session_id,
        &crate::auth::hash_secret(raw_token),
    )
    .await?
    else {
        return Ok(None);
    };
    Ok(Some(GuestContext {
        guest,
        csrf: guest_csrf(raw_token),
    }))
}

fn local_url(state: &AppState, session: &Session) -> String {
    if session.coordinator_uri == format!("https://{}/webxdc/{}", state.config.domain, session.id) {
        format!("/webxdc/{}", session.id)
    } else {
        format!("/webxdc/session/{}", session.id)
    }
}

fn require_csrf(user: &WebUser, submitted: &str) -> Result<(), ApiError> {
    if user.csrf_ok(submitted) {
        Ok(())
    } else {
        Err(ApiError::Forbidden("invalid CSRF token".into()))
    }
}

fn session_card(state: &AppState, session: &Session, membership: &Membership) -> Markup {
    let href = local_url(state, session);
    html! {
        article.webxdc-card {
            div.webxdc-card__body {
                h2.webxdc-card__title { a href=(href) { (&session.name) } }
                @if !session.summary.is_empty() {
                    p.webxdc-card__summary { (&session.summary) }
                }
                p.webxdc-card__meta {
                    span.webxdc-badge {
                        @if session.ended() { "Ended" }
                        @else if membership.accepted { "Joined" }
                        @else { "Waiting for approval" }
                    }

                }
            }
            @if membership.accepted && !session.ended() {
                a.pill-button.webxdc-open href={ "/webxdc/session/" (session.id) "/play" } { "Open app" }
            } @else {
                a.pill-button href=(href) { "View session" }
            }
        }
    }
}

#[derive(Deserialize, Default)]
pub struct IndexQuery {
    #[serde(default)]
    ended: bool,
}

pub async fn index(
    State(state): State<AppState>,
    user: WebUser,
    Query(query): Query<IndexQuery>,
) -> Result<Response, ApiError> {
    let sessions =
        webxdc::list_for_participant(&state.pool, user.current.account.id, query.ended).await?;
    let storage = webxdc::storage_usage(&state.pool, Some(user.current.account.id)).await?;
    let limits = webxdc::limits(&state.pool).await?;
    let body = html! {
        section.column.webxdc-page {
            header.webxdc-page__head {
                div {
                    h1 { (super::view::icon("apps")) @if query.ended { " Ended app sessions" } @else { " Apps" } }
                    p { @if query.ended { "Sessions that have finished. App data is kept for 30 days." } @else { "Apps you use together." } }
                }
            }
            nav.webxdc-actions aria-label="App session actions" {
                @if user.can(permission::CREATE_WEBXDC) { a.pill-button href="/webxdc/new" { "New session" } }
                a.pill-button href="/webxdc/library" { "App library" }
                a.pill-button href="/webxdc/open" { "Open invitation" }
                @if query.ended {
                    a.pill-button href="/webxdc" { "Active sessions" }
                } @else {
                    a.pill-button href="/webxdc?ended=true" { "Ended sessions" }
                }
            }
            @if user.can(permission::CREATE_WEBXDC) || storage.sessions > 0 {
                details {
                    summary { "Your app storage: " (storage_size(storage.total_bytes())) " of " (limits.account_mb) " MiB" }
                    p { "Identical apps count once across sessions and your personal library. Session updates count separately. Package files are released after the last session or saved app version using them is removed." }
                }
            }
            @if sessions.is_empty() {
                div.webxdc-empty {
                    h2 { @if query.ended { "No ended sessions" } @else { "No app sessions yet" } }
                    p { @if query.ended { "Closed sessions appear here during their 30-day deletion grace period." } @else if user.can(permission::CREATE_WEBXDC) { "Create one from a .xdc package, or open a session link someone shared with you." } @else { "Open a session link someone shared with you." } }
                }
            } @else {
                div.webxdc-grid {
                    @for (session, membership) in &sessions {
                        (session_card(&state, session, membership))
                    }
                }
            }
        }
    };
    Ok(layout::shell("Webxdc sessions", Some(&user), &body).into_response())
}

fn require_create(user: &WebUser) -> Result<(), ApiError> {
    if user.can(permission::CREATE_WEBXDC) {
        Ok(())
    } else {
        Err(ApiError::Forbidden(
            "Your role cannot create Webxdc sessions".into(),
        ))
    }
}

#[derive(Deserialize, Default)]
pub struct NewQuery {
    version: Option<i64>,
}

pub async fn new_page(
    State(state): State<AppState>,
    user: WebUser,
    Query(query): Query<NewQuery>,
) -> Result<Response, ApiError> {
    require_create(&user)?;
    let limits = webxdc::limits(&state.pool).await?;
    let apps = webxdc::library_for_account(&state.pool, user.current.account.id).await?;
    let mut form = CreateForm::default();
    if let Some(app) = query
        .version
        .and_then(|version| apps.iter().find(|app| app.version_id == version))
    {
        form.version_id = Some(app.version_id);
        app.name.clone_into(&mut form.name);
    }
    Ok(create_page(&user, &form, limits, &apps, None))
}

fn create_page(
    user: &WebUser,
    form: &CreateForm,
    limits: webxdc::Limits,
    apps: &[webxdc::LibraryApp],
    error: Option<&str>,
) -> Response {
    let body = html! {
        section.column.settings {
            header.settings__head { h1 { "Create an app session" } }
            div.settings__body {
                p { "Choose a saved app or upload a .xdc package to start using it together." }
                @if let Some(error) = error {
                    p.settings__error role="alert" { (error) " Select the package again to retry." }
                }
                form.settings-form method="post" action="/web/webxdc" enctype="multipart/form-data" {
                    input type="hidden" name="csrf" value=(&user.csrf);
                    fieldset.settings-form__group {
                        legend { "Session" }
                        @if !apps.is_empty() {
                            label.settings-field {
                                span.settings-field__label { "App" }
                                select name="version_id" {
                                    option value="" selected[form.version_id.is_none()] { "One-off package upload" }
                                    @for app in apps {
                                        option value=(app.version_id) selected[form.version_id == Some(app.version_id)] {
                                            (&app.name)
                                            @if !app.version.is_empty() { " · " (&app.version) }
                                            @if app.owner_account_id.is_some() { " · Personal" } @else { " · Instance" }
                                        }
                                    }
                                }
                                span.settings-field__hint { "Saved packages are reused without uploading or copying them." }
                            }
                        }
                        label.settings-field {
                            span.settings-field__label { "Name" }
                            input required name="name" maxlength="120" placeholder="Shopping list" value=(&form.name);
                        }
                        label.settings-field {
                            span.settings-field__label { "Description" }
                            textarea name="summary" maxlength="2000" rows="3" placeholder="What this session is for" { (&form.summary) }
                        }
                        label.settings-field {
                            span.settings-field__label { "Webxdc package" }
                            input type="file" name="bundle" data-max-bytes=(limits.bundle_bytes()) accept=".xdc,application/webxdc+zip,application/x-webxdc,application/zip";
                            span.settings-field__hint { "Required only for a one-off upload. Choose a .xdc file up to " (limits.bundle_bytes() / (1024 * 1024)) " MiB." }
                        }
                        label.settings-field {
                            span.settings-field__label { "Joining" }
                            select name="membership_policy" {
                                option value="open" selected[form.membership_policy == "open"] { "Anyone with the link can join" }
                                option value="approval" selected[form.membership_policy == "approval"] { "I approve each join request" }
                            }
                        }
                    }
                    details.webxdc-advanced {
                        summary { "Advanced settings" }
                        div.settings-form__group {
                            label.settings-field {
                                span.settings-field__label { "Minimum interval between updates" }
                                input type="number" name="send_update_interval" value=(form.send_update_interval) min="0" max="86400000";
                                span.settings-field__hint { "Milliseconds. Most apps should keep the default." }
                            }
                            label.settings-field {
                                span.settings-field__label { "Maximum update size" }
                                input type="number" name="send_update_max_size" value=(form.send_update_max_size) min="256" max="1048576";
                                span.settings-field__hint { "Bytes of JSON per durable update." }
                            }
                        }
                    }
                    div.settings-form__actions {
                        a.settings-button--plain href="/webxdc" { "Cancel" }
                        button type="submit" { "Create session" }
                    }
                    p.settings-field__hint { "Want to reuse an upload later? " a href="/webxdc/library" { "Save it to your app library first." } }
                }
            }
        }
    };
    layout::shell("Create Webxdc session", Some(user), &body).into_response()
}

struct CreateForm {
    csrf: String,
    name: String,
    summary: String,
    membership_policy: String,
    send_update_interval: i32,
    send_update_max_size: i32,
    bundle_name: String,
    bundle: axum::body::Bytes,
    version_id: Option<i64>,
}

impl Default for CreateForm {
    fn default() -> Self {
        Self {
            csrf: String::new(),
            name: String::new(),
            summary: String::new(),
            membership_policy: "open".into(),
            send_update_interval: 1000,
            send_update_max_size: 32_768,
            bundle_name: String::new(),
            bundle: axum::body::Bytes::new(),
            version_id: None,
        }
    }
}

pub async fn create(
    State(state): State<AppState>,
    user: WebUser,
    mut request: Request,
) -> Result<Response, ApiError> {
    require_create(&user)?;
    let limits = webxdc::limits(&state.pool).await?;
    let apps = webxdc::library_for_account(&state.pool, user.current.account.id).await?;
    DefaultBodyLimit::max(limits.bundle_bytes() + 1024 * 1024).apply(&mut request);
    let mut form = CreateForm::default();
    let result = match Multipart::from_request(request, &state).await {
        Ok(mut multipart) => {
            create_uploaded(&state, &user, &mut multipart, &mut form, limits).await
        }
        Err(_) => Err(ApiError::BadRequest(
            "Choose a .xdc package to upload.".into(),
        )),
    };
    Ok(match result {
        Ok(response) => response,
        Err(error) => {
            let message = error.to_string();
            let response = error.into_response();
            if response.status().is_client_error() {
                (
                    response.status(),
                    create_page(&user, &form, limits, &apps, Some(&message)),
                )
                    .into_response()
            } else {
                response
            }
        }
    })
}

fn upload_too_large(limits: webxdc::Limits) -> ApiError {
    ApiError::PayloadTooLargeWithMessage(format!(
        "The Webxdc package must be at most {} MiB.",
        limits.bundle_bytes() / (1024 * 1024)
    ))
}

fn multipart_error(
    error: &axum::extract::multipart::MultipartError,
    limits: webxdc::Limits,
) -> ApiError {
    if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
        upload_too_large(limits)
    } else {
        ApiError::BadRequest(
            "The package upload was incomplete or malformed. Please try again.".into(),
        )
    }
}

async fn parse_create_fields(
    multipart: &mut Multipart,
    form: &mut CreateForm,
    limits: webxdc::Limits,
) -> Result<(), ApiError> {
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|error| multipart_error(&error, limits))?
    {
        let name = field.name().unwrap_or_default().to_owned();
        if name == "bundle" {
            if !form.bundle.is_empty() {
                return Err(ApiError::BadRequest(
                    "Upload one Webxdc package at a time.".into(),
                ));
            }
            field
                .file_name()
                .unwrap_or("package.xdc")
                .clone_into(&mut form.bundle_name);
            form.bundle = field
                .bytes()
                .await
                .map_err(|error| multipart_error(&error, limits))?;
            continue;
        }
        let value = field
            .text()
            .await
            .map_err(|error| multipart_error(&error, limits))?;
        match name.as_str() {
            "csrf" => form.csrf = value,
            "name" => form.name = value,
            "summary" => form.summary = value,
            "membership_policy" => form.membership_policy = value,
            "send_update_interval" => {
                form.send_update_interval = value
                    .parse()
                    .map_err(|_| ApiError::Unprocessable("Invalid update interval".into()))?;
            }
            "send_update_max_size" => {
                form.send_update_max_size = value
                    .parse()
                    .map_err(|_| ApiError::Unprocessable("Invalid update size".into()))?;
            }
            "version_id" => {
                form.version_id = if value.is_empty() {
                    None
                } else {
                    Some(
                        value
                            .parse()
                            .map_err(|_| ApiError::Unprocessable("Invalid saved app".into()))?,
                    )
                };
            }
            _ => {}
        }
    }
    Ok(())
}

async fn create_uploaded(
    state: &AppState,
    user: &WebUser,
    multipart: &mut Multipart,
    form: &mut CreateForm,
    limits: webxdc::Limits,
) -> Result<Response, ApiError> {
    parse_create_fields(multipart, form, limits).await?;
    require_csrf(user, &form.csrf)?;
    if form.version_id.is_some() && !form.bundle.is_empty() {
        return Err(ApiError::Unprocessable(
            "Choose either a saved app or a one-off package, not both.".into(),
        ));
    }
    if form.version_id.is_none() && form.bundle.is_empty() {
        return Err(ApiError::Unprocessable(
            "Choose a non-empty .xdc package.".into(),
        ));
    }
    if form.bundle.len() > limits.bundle_bytes() {
        return Err(upload_too_large(limits));
    }
    if form.send_update_interval > 86_400_000
        || !(256..=1_048_576).contains(&form.send_update_max_size)
    {
        return Err(ApiError::Unprocessable(
            "Invalid durable update limits".into(),
        ));
    }
    let session = if let Some(version_id) = form.version_id {
        protocol::create_local_from_library(
            state,
            protocol::CreateLocalFromLibrary {
                creator: &user.current.account,
                name: &form.name,
                summary: &form.summary,
                version_id,
                membership_policy: &form.membership_policy,
                send_update_interval: form.send_update_interval,
                send_update_max_size: form.send_update_max_size,
            },
        )
        .await?
    } else {
        protocol::create_local(
            state,
            protocol::CreateLocal {
                creator: &user.current.account,
                name: &form.name,
                summary: &form.summary,
                bundle_name: &form.bundle_name,
                bundle_bytes: &form.bundle,
                membership_policy: &form.membership_policy,
                send_update_interval: form.send_update_interval,
                send_update_max_size: form.send_update_max_size,
            },
        )
        .await?
    };
    Ok(Redirect::to(&local_url(state, &session)).into_response())
}

#[derive(Deserialize, Default)]
pub struct LibraryQuery {
    flash: Option<String>,
}

#[derive(Default)]
struct LibraryUploadForm {
    csrf: String,
    summary: String,
    category: String,
    bundle_name: String,
    bundle: axum::body::Bytes,
}

fn library_card(user: &WebUser, app: &webxdc::LibraryApp) -> Markup {
    let personal = app.owner_account_id.is_some();
    html! {
        article.webxdc-library-card {
            div.webxdc-library-card__icon {
                @if app.icon_path.is_some() {
                    img src={ "/webxdc/library/version/" (app.version_id) "/icon" }
                        alt="" loading="lazy" width="72" height="72";
                } @else {
                    span aria-hidden="true" { (super::view::icon("apps")) }
                }
            }
            div.webxdc-library-card__body {
                div.webxdc-library-card__head {
                    h2 { (&app.name) }
                    span.webxdc-badge { @if personal { "Personal" } @else { "Instance" } }
                }
                @if !app.summary.is_empty() { p { (&app.summary) } }
                p.webxdc-card__meta {
                    @if !app.version.is_empty() { "Version " (&app.version) " · " }
                    (storage_size(app.package_bytes))
                    @if let Some(category) = &app.category { " · " (category) }
                }
                @if app.update_available { p.webxdc-card__meta { "A newer version is awaiting instance review." } }
                div.webxdc-actions {
                    a.pill-button href={ "/webxdc/new?version=" (app.version_id) } { "New session" }
                    @if let Some(source) = &app.source_code_url {
                        a href=(source) rel="noopener noreferrer" { "Source code" }
                    }
                }
                details.webxdc-library-details {
                    summary { "Package details" }
                    dl.webxdc-facts {
                        div { dt { "File" } dd { (&app.filename) } }
                        div { dt { "Digest" } dd { code { (&app.digest_multibase) } } }
                    }
                    @if personal {
                        form.settings-form method="post" action={ "/web/webxdc/library/" (app.id) "/version" } enctype="multipart/form-data" {
                            input type="hidden" name="csrf" value=(&user.csrf);
                            label.settings-field {
                                span.settings-field__label { "Add a new version" }
                                input required type="file" name="bundle" accept=".xdc,application/webxdc+zip,application/x-webxdc,application/zip";
                            }
                            button type="submit" { "Add version" }
                        }
                        form method="post" action={ "/web/webxdc/library/" (app.id) "/delete" } {
                            input type="hidden" name="csrf" value=(&user.csrf);
                            button.settings-button--plain type="submit" { "Remove from my library" }
                        }
                    }
                }
            }
        }
    }
}

async fn library_response(
    state: &AppState,
    user: &WebUser,
    form: &LibraryUploadForm,
    flash: Option<&str>,
    error: Option<&str>,
) -> Result<Response, ApiError> {
    let apps = webxdc::library_for_account(&state.pool, user.current.account.id).await?;
    let storage = webxdc::storage_usage(&state.pool, Some(user.current.account.id)).await?;
    let limits = webxdc::limits(&state.pool).await?;
    let sources = webxdc::catalog_sources(&state.pool).await?;
    let personal: Vec<_> = apps
        .iter()
        .filter(|app| app.owner_account_id.is_some())
        .collect();
    let instance: Vec<_> = apps
        .iter()
        .filter(|app| app.owner_account_id.is_none())
        .collect();
    let body = html! {
        section.column.webxdc-page {
            a.webxdc-back href="/webxdc" { "← App sessions" }
            header.webxdc-page__head {
                div { h1 { "App library" } p { "Save trusted packages once and start new sessions without uploading them again." } }
                a.pill-button href="/webxdc/new" { "New session" }
            }
            @if flash == Some("saved") { p.admin-flash role="status" { "App saved to your library." } }
            @if flash == Some("updated") { p.admin-flash role="status" { "The new version is now selected for future sessions." } }
            @if flash == Some("removed") { p.admin-flash role="status" { "App removed from your library. Existing sessions were not changed." } }
            @if let Some(error) = error { p.settings__error role="alert" { (error) } }
            details.webxdc-details open[personal.is_empty()] {
                summary { "Save a personal app" }
                div.webxdc-details__body {
                    p.webxdc-muted { "The package is private to you until a moderator promotes it. Running sessions always keep the exact version they started with." }
                    form.settings-form method="post" action="/web/webxdc/library" enctype="multipart/form-data" {
                        input type="hidden" name="csrf" value=(&user.csrf);
                        label.settings-field {
                            span.settings-field__label { "Webxdc package" }
                            input required type="file" name="bundle" data-max-bytes=(limits.bundle_bytes()) accept=".xdc,application/webxdc+zip,application/x-webxdc,application/zip";
                            span.settings-field__hint { "Name, source link, version label and icon are read from the package. Maximum " (limits.bundle_mb) " MiB." }
                        }
                        label.settings-field {
                            span.settings-field__label { "Description" }
                            textarea name="summary" maxlength="2000" rows="3" { (&form.summary) }
                        }
                        label.settings-field {
                            span.settings-field__label { "Category" }
                            input name="category" maxlength="80" value=(&form.category) placeholder="Game, tool, productivity…";
                        }
                        button type="submit" { "Save app" }
                    }
                }
            }
            p.webxdc-muted { (personal.len()) " of " (limits.personal_apps) " personal apps · " (storage_size(storage.total_bytes())) " of " (limits.account_mb) " MiB account storage" }
            section {
                h2 { "Your apps" }
                @if personal.is_empty() { p.webxdc-muted { "No personal apps saved yet." } }
                div.webxdc-library-grid { @for app in personal { (library_card(user, app)) } }
            }
            section {
                h2 { "From this instance" }
                @if instance.is_empty() { p.webxdc-muted { "No instance apps are available yet." } }
                div.webxdc-library-grid { @for app in instance { (library_card(user, app)) } }
            }
            @if !sources.is_empty() {
                section {
                    h2 { "Browse external catalogs" }
                    p.webxdc-muted { "Catalog details come from external sources. Plamenu downloads and validates a package only when you choose to save it." }
                    div.webxdc-actions {
                        @for source in sources.iter().filter(|source| source.enabled) {
                            a.pill-button href={ "/webxdc/library/catalog/" (source.id) } { (&source.name) }
                        }
                    }
                }
            }
        }
    };
    Ok(layout::shell("Webxdc app library", Some(user), &body).into_response())
}

pub async fn library(
    State(state): State<AppState>,
    user: WebUser,
    Query(query): Query<LibraryQuery>,
) -> Result<Response, ApiError> {
    library_response(
        &state,
        &user,
        &LibraryUploadForm::default(),
        query.flash.as_deref(),
        None,
    )
    .await
}

async fn parse_library_upload(
    multipart: &mut Multipart,
    form: &mut LibraryUploadForm,
    limits: webxdc::Limits,
) -> Result<(), ApiError> {
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|error| multipart_error(&error, limits))?
    {
        let name = field.name().unwrap_or_default().to_owned();
        if name == "bundle" {
            if !form.bundle.is_empty() {
                return Err(ApiError::BadRequest(
                    "Upload one Webxdc package at a time.".into(),
                ));
            }
            field
                .file_name()
                .unwrap_or("application.xdc")
                .clone_into(&mut form.bundle_name);
            form.bundle = field
                .bytes()
                .await
                .map_err(|error| multipart_error(&error, limits))?;
            continue;
        }
        let value = field
            .text()
            .await
            .map_err(|error| multipart_error(&error, limits))?;
        match name.as_str() {
            "csrf" => form.csrf = value,
            "summary" => form.summary = value,
            "category" => form.category = value,
            _ => {}
        }
    }
    Ok(())
}

pub async fn save_library_app(
    State(state): State<AppState>,
    user: WebUser,
    mut request: Request,
) -> Result<Response, ApiError> {
    require_create(&user)?;
    let limits = webxdc::limits(&state.pool).await?;
    DefaultBodyLimit::max(limits.bundle_bytes() + 1024 * 1024).apply(&mut request);
    let mut form = LibraryUploadForm::default();
    let result = async {
        let mut multipart = Multipart::from_request(request, &state)
            .await
            .map_err(|error| {
                if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
                    upload_too_large(limits)
                } else {
                    ApiError::BadRequest("The package upload was malformed.".into())
                }
            })?;
        parse_library_upload(&mut multipart, &mut form, limits).await?;
        require_csrf(&user, &form.csrf)?;
        if form.bundle.is_empty() {
            return Err(ApiError::Unprocessable(
                "Choose a non-empty .xdc package.".into(),
            ));
        }
        protocol::save_personal_app(
            &state,
            protocol::LibraryUpload {
                actor: &user.current.account,
                bundle_name: &form.bundle_name,
                bundle_bytes: &form.bundle,
                summary: &form.summary,
                category: (!form.category.trim().is_empty()).then_some(form.category.as_str()),
            },
        )
        .await?;
        Ok::<_, ApiError>(Redirect::to("/webxdc/library?flash=saved").into_response())
    }
    .await;
    match result {
        Ok(response) => Ok(response),
        Err(error) => {
            let message = error.to_string();
            let status = error.into_response().status();
            let mut response = library_response(&state, &user, &form, None, Some(&message)).await?;
            *response.status_mut() = status;
            Ok(response)
        }
    }
}

pub async fn add_library_version(
    State(state): State<AppState>,
    user: WebUser,
    Path(app_id): Path<i64>,
    mut request: Request,
) -> Result<Response, ApiError> {
    require_create(&user)?;
    let limits = webxdc::limits(&state.pool).await?;
    DefaultBodyLimit::max(limits.bundle_bytes() + 1024 * 1024).apply(&mut request);
    let mut form = LibraryUploadForm::default();
    let mut multipart = Multipart::from_request(request, &state)
        .await
        .map_err(|error| {
            if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
                upload_too_large(limits)
            } else {
                ApiError::BadRequest("The package upload was malformed.".into())
            }
        })?;
    parse_library_upload(&mut multipart, &mut form, limits).await?;
    require_csrf(&user, &form.csrf)?;
    if form.bundle.is_empty() {
        return Err(ApiError::Unprocessable(
            "Choose a non-empty .xdc package.".into(),
        ));
    }
    protocol::add_personal_app_version(
        &state,
        app_id,
        protocol::LibraryUpload {
            actor: &user.current.account,
            bundle_name: &form.bundle_name,
            bundle_bytes: &form.bundle,
            summary: "",
            category: None,
        },
    )
    .await?;
    Ok(Redirect::to("/webxdc/library?flash=updated").into_response())
}

pub async fn delete_library_app(
    State(state): State<AppState>,
    user: WebUser,
    Path(app_id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, ApiError> {
    require_csrf(&user, &form.csrf)?;
    if !webxdc::delete_personal_app(&state.pool, app_id, user.current.account.id).await? {
        return Err(ApiError::NotFound);
    }
    Ok(Redirect::to("/webxdc/library?flash=removed").into_response())
}

pub async fn library_icon(
    State(state): State<AppState>,
    user: WebUser,
    Path(version_id): Path<i64>,
) -> Result<Response, ApiError> {
    let asset = webxdc::library_icon_for_account(&state.pool, version_id, user.current.account.id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok((
        [
            (header::CONTENT_TYPE, asset.media_type),
            (header::CACHE_CONTROL, "private, max-age=86400".to_owned()),
        ],
        asset.bytes,
    )
        .into_response())
}

const CATALOG_ICON_MAX_BYTES: u64 = 512 * 1024;

#[derive(Deserialize)]
pub struct CatalogIconQuery {
    source_id: i64,
    external_app_id: String,
}

fn catalog_icon_media_type(bytes: &[u8]) -> Option<&'static str> {
    match image::guess_format(bytes).ok()? {
        image::ImageFormat::Png => Some("image/png"),
        image::ImageFormat::Jpeg => Some("image/jpeg"),
        image::ImageFormat::Gif => Some("image/gif"),
        image::ImageFormat::WebP => Some("image/webp"),
        _ => None,
    }
}

/// Same-origin, lazy catalog-icon delivery. External discovery metadata never
/// becomes a direct browser request: the guarded federation client applies
/// SSRF/redirect/timeout limits, validates a small raster image, then the
/// candidate row caches it until the next manual catalog refresh.
pub async fn catalog_icon(
    State(state): State<AppState>,
    _user: WebUser,
    Query(query): Query<CatalogIconQuery>,
) -> Result<Response, ApiError> {
    let icon =
        webxdc::catalog_candidate_icon(&state.pool, query.source_id, query.external_app_id.trim())
            .await?
            .ok_or(ApiError::NotFound)?;
    let icon_url = icon.icon_url.ok_or(ApiError::NotFound)?;
    let (media_type, bytes) = match (icon.icon_media_type, icon.icon_bytes) {
        (Some(media_type), Some(bytes)) => (media_type, bytes),
        _ => {
            let fetched = state
                .federation
                .fetch_media_limited(&icon_url, CATALOG_ICON_MAX_BYTES)
                .await
                .map_err(|error| ApiError::BadGateway(error.to_string()))?;
            let media_type = catalog_icon_media_type(&fetched.bytes)
                .ok_or_else(|| {
                    ApiError::Unprocessable(
                        "The catalog icon is not a supported raster image".into(),
                    )
                })?
                .to_owned();
            if !webxdc::cache_catalog_candidate_icon(
                &state.pool,
                query.source_id,
                query.external_app_id.trim(),
                &icon_url,
                &media_type,
                &fetched.bytes,
            )
            .await?
            {
                return Err(ApiError::NotFound);
            }
            (media_type, fetched.bytes)
        }
    };
    Ok((
        [
            (header::CONTENT_TYPE, media_type),
            (header::CACHE_CONTROL, "private, max-age=86400".to_owned()),
        ],
        bytes,
    )
        .into_response())
}

pub async fn external_catalog(
    State(state): State<AppState>,
    user: WebUser,
    Path(source_id): Path<i64>,
) -> Result<Response, ApiError> {
    require_create(&user)?;
    let source = webxdc::catalog_source(&state.pool, source_id)
        .await?
        .filter(|source| source.enabled)
        .ok_or(ApiError::NotFound)?;
    let candidates = webxdc::catalog_candidates(&state.pool, source_id).await?;
    let personal = webxdc::personal_library(&state.pool, user.current.account.id).await?;
    let body = html! {
        section.column.webxdc-page {
            a.webxdc-back href="/webxdc/library" { "← App library" }
            header.webxdc-page__head {
                div { h1 { (&source.name) } p { "External catalog · package details are verified only when you save an app." } }
            }
            @if candidates.is_empty() {
                div.webxdc-empty { h2 { "No catalog entries" } p { "A moderator has not refreshed this source yet." } }
            } @else {
                div.webxdc-library-grid {
                    @for candidate in &candidates {
                        @let saved = personal.iter().find(|app| app.catalog_source_id == Some(source_id)
                            && app.external_app_id.as_deref() == Some(candidate.external_app_id.as_str()));
                        article.webxdc-library-card {
                            div.webxdc-library-card__icon {
                                @if candidate.icon_url.is_some() {
                                    @let query = url::form_urlencoded::Serializer::new(String::new())
                                        .append_pair("source_id", &candidate.source_id.to_string())
                                        .append_pair("external_app_id", &candidate.external_app_id)
                                        .finish();
                                    img src={ "/webxdc/library/catalog-icon?" (query) }
                                        alt="" width="72" height="72" loading="lazy";
                                } @else {
                                    span aria-hidden="true" { (super::view::icon("apps")) }
                                }
                            }
                            div.webxdc-library-card__body {
                                div.webxdc-library-card__head { h2 { (&candidate.name) } @if saved.is_some() { span.webxdc-badge { "Saved" } } }
                                @if !candidate.summary.is_empty() { p { (&candidate.summary) } }
                                p.webxdc-card__meta {
                                    @if !candidate.version.is_empty() { (&candidate.version) }
                                    @if let Some(size) = candidate.advertised_size { " · About " (storage_size(size)) }
                                    @if let Some(category) = &candidate.category { " · " (category) }
                                }
                                @if let Some(source_url) = &candidate.source_code_url { a href=(source_url) rel="noopener noreferrer" { "Source listed by catalog" } }
                                form method="post" action={ "/web/webxdc/library/catalog/" (source_id) "/import" } {
                                    input type="hidden" name="csrf" value=(&user.csrf);
                                    input type="hidden" name="external_app_id" value=(&candidate.external_app_id);
                                    button type="submit" { @if saved.is_some() { "Revalidate / check update" } @else { "Validate and save" } }
                                }
                            }
                        }
                    }
                }
            }
        }
    };
    Ok(layout::shell(&source.name, Some(&user), &body).into_response())
}

#[derive(Deserialize)]
pub struct CatalogImportForm {
    csrf: String,
    external_app_id: String,
}

pub async fn import_external_catalog_app(
    State(state): State<AppState>,
    user: WebUser,
    Path(source_id): Path<i64>,
    Form(form): Form<CatalogImportForm>,
) -> Result<Response, ApiError> {
    require_create(&user)?;
    require_csrf(&user, &form.csrf)?;
    protocol::import_personal_catalog_candidate(
        &state,
        &user.current.account,
        source_id,
        &form.external_app_id,
    )
    .await?;
    Ok(Redirect::to("/webxdc/library?flash=saved").into_response())
}

#[derive(Deserialize, Default)]
pub struct OpenQuery {
    url: Option<String>,
}

pub async fn open_page(user: WebUser, Query(query): Query<OpenQuery>) -> Response {
    let body = html! {
        section.column.settings {
            header.settings__head { h1 { "Open an app invitation" } }
            div.settings__body {
                p { "Paste a session link to see the app and join." }
                form.settings-form method="post" action="/web/webxdc/open" {
                    input type="hidden" name="csrf" value=(&user.csrf);
                    fieldset.settings-form__group {
                        label.settings-field {
                            span.settings-field__label { "Session link" }
                            input required type="url" name="url" value=(query.url.as_deref().unwrap_or_default()) placeholder="https://social.example/webxdc/…";
                        }
                    }
                    div.settings-form__actions {
                        a.settings-button--plain href="/webxdc" { "Cancel" }
                        button type="submit" { "Continue" }
                    }
                }
            }
        }
    };
    layout::shell("Open remote Webxdc", Some(&user), &body).into_response()
}

#[derive(Deserialize)]
pub struct OpenForm {
    csrf: String,
    url: String,
}

pub async fn open(
    State(state): State<AppState>,
    user: WebUser,
    Form(form): Form<OpenForm>,
) -> Result<Response, ApiError> {
    require_csrf(&user, &form.csrf)?;
    let session = protocol::fetch_remote(&state, form.url.trim()).await?;
    Ok(Redirect::to(&local_url(&state, &session)).into_response())
}

pub async fn remote_landing(
    State(state): State<AppState>,
    MaybeWebUser(user): MaybeWebUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if let Some(tombstone) = webxdc::tombstone_by_id(&state.pool, id).await? {
        return deleted_landing(&state, user, &tombstone, &headers).await;
    }
    let session = webxdc::find(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    landing(&state, user, &session, &headers).await
}

pub async fn deleted_landing(
    state: &AppState,
    user: Option<WebUser>,
    tombstone: &webxdc::Tombstone,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let body = html! {
        section.column.webxdc-page {
            header.webxdc-page__head {
                div {
                    p.webxdc-eyebrow { "Webxdc session" }
                    h1 { "Session deleted" }
                    p { "The creator deleted this session for everyone." }
                }
                span.webxdc-badge.webxdc-badge--ended { "Gone" }
            }
            section.webxdc-panel {
                h2 { "Application data removed" }
                p { "The app package, durable update history, memberships, and guest access have been deleted. This session cannot be reopened." }
                p.webxdc-muted { "Invitation posts are separate social posts and may still contain this now-inactive link." }
                @if user.is_some() {
                    a.pill-button href="/webxdc" { "Back to Apps" }
                }
            }
        }
    };
    let locale = user
        .as_ref()
        .map_or_else(|| Locale::from_headers(headers), |user| user.locale);
    let mut response = layout::shell_visitor_localized(
        "Session deleted",
        user.as_ref(),
        anon_nav(state).await,
        &body,
        locale,
    )
    .into_response();
    *response.status_mut() = StatusCode::GONE;
    response.headers_mut().insert(
        "x-plamenu-webxdc-deleted-at",
        tombstone
            .deleted_at
            .unix_timestamp()
            .to_string()
            .parse()
            .expect("timestamp is a valid header value"),
    );
    Ok(response)
}

#[allow(clippy::too_many_lines)]
pub async fn landing(
    state: &AppState,
    user: Option<WebUser>,
    session: &Session,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let membership = match &user {
        Some(user) => webxdc::membership(&state.pool, session.id, user.current.account.id).await?,
        None => None,
    };
    let guest = if user.is_none() && local_session(state, session) {
        guest_context(state, session.id, headers).await?
    } else {
        None
    };
    let owner = user
        .as_ref()
        .is_some_and(|user| session.creator_account_id == Some(user.current.account.id));
    let participants = if owner {
        webxdc::participants(&state.pool, session.id).await?
    } else {
        Vec::new()
    };
    let guests = if owner {
        webxdc::guests(&state.pool, session.id).await?
    } else {
        Vec::new()
    };
    let pending = participants
        .iter()
        .filter(|(member, _)| !member.accepted)
        .count()
        + guests.iter().filter(|guest| !guest.accepted).count();
    let joined = membership.as_ref().is_some_and(|m| m.accepted);
    let body = html! {
        section.column.webxdc-page {
            a.webxdc-back href="/webxdc" { "← Apps" }
            header.webxdc-page__head {
                div {
                    h1 { (&session.name) }
                    @if !session.summary.is_empty() { p { (&session.summary) } }
                }
                @if session.ended() { span.webxdc-badge.webxdc-badge--ended { "Ended" } }
            }
            section.webxdc-panel.webxdc-launch {
                @if session.ended() {
                    p role="status" { "This session has ended." }
                } @else if let (Some(user), Some(membership)) = (&user, &membership) {
                    @if membership.accepted {
                        div.webxdc-actions {
                            a.pill-button.webxdc-open href={ "/webxdc/session/" (session.id) "/play" } { "Open app" }
                            a.pill-button href={ "/compose?webxdc=" (session.id) } { "Invite people" }
                        }
                    } @else {
                        p role="status" { @if session.membership_policy == "approval" { "Waiting for the creator to approve your request." } @else { "Joining the session on its host server…" } }
                        a href=(local_url(state, session)) { "Check status" }
                        form method="post" action={ "/web/webxdc/" (session.id) "/leave" } {
                            input type="hidden" name="csrf" value=(&user.csrf);
                            button.settings-button--plain type="submit" { "Cancel request" }
                        }
                    }
                } @else if let Some(user) = &user {
                    p { @if session.membership_policy == "approval" { "The creator approves each join request." } @else { "Anyone with the link can join." } }
                    form method="post" action={ "/web/webxdc/" (session.id) "/join" } {
                        input type="hidden" name="csrf" value=(&user.csrf);
                        button type="submit" { @if session.membership_policy == "approval" { "Request to join" } @else { "Join session" } }
                    }
                } @else if let Some(guest) = &guest {
                    @if guest.guest.accepted {
                        p.webxdc-muted { "Joining as " strong { (&guest.guest.display_name) } }
                        a.pill-button.webxdc-open href={ "/webxdc/session/" (session.id) "/play" } { "Open app" }
                    } @else {
                        p role="status" { @if session.membership_policy == "approval" { "Waiting for the creator to approve your request." } @else { "Joining the session on its host server…" } }
                        a href=(local_url(state, session)) { "Check status" }
                        form method="post" action={ "/web/webxdc/" (session.id) "/guest/leave" } {
                            input type="hidden" name="csrf" value=(&guest.csrf);
                            button.settings-button--plain type="submit" { "Cancel request" }
                        }
                    }
                } @else if local_session(state, session) {
                    h2 { "Join as a guest" }
                    p.webxdc-muted { "No account needed. Your guest access stays in this browser for 30 days." }
                    form.settings-form method="post" action={ "/web/webxdc/" (session.id) "/guest" } {
                        label.settings-field {
                            span.settings-field__label { "Your name" }
                            input required name="display_name" maxlength="80" autocomplete="nickname" placeholder="Guest";
                        }
                        label.webxdc-consent {
                            input required type="checkbox" name="accept_risk" value="yes";
                            span { "I understand the app can see what I share." }
                        }
                        button type="submit" { "Join as guest" }
                    }
                    a href={ "/login?redirect=" (local_url(state, session)) } { "Sign in with an account instead" }
                } @else {
                    p { "Sign in here, or join as a guest on the host server." }
                    div.webxdc-actions {
                        a.pill-button href={ "/login?redirect=" (local_url(state, session)) } { "Sign in" }
                        a.pill-button href=(&session.coordinator_uri) rel="noopener" { "Join on host server" }
                    }
                }
            }
            @if !session.ended() {
                details.webxdc-details {
                    summary { "Share link" }
                    div.webxdc-details__body {
                        label.settings-field {
                            span.settings-field__label { "Session link" }
                            input.webxdc-share-link readonly value=(&session.coordinator_uri);
                        }
                        div.webxdc-actions {
                            button.pill-button.js-only type="button" data-copy-link=(&session.coordinator_uri) { "Copy link" }
                            @if joined { a href={ "/compose?webxdc=" (session.id) } { "Write an invitation post" } }
                        }
                    }
                }
            }
            @if owner {
                details.webxdc-details open[pending > 0] {
                    summary { "Participants" @if pending > 0 { " · " (pending) " waiting" } }
                    div.webxdc-details__body {
                        @if participants.len() <= 1 && guests.is_empty() { p.webxdc-muted { "No one else has joined yet." } }
                        @for (member, account) in &participants {
                            @if account.id != session.creator_account_id.unwrap_or_default() {
                                div.webxdc-participant {
                                    div {
                                        strong { @if account.display_name.is_empty() { (&account.username) } @else { (&account.display_name) } }
                                        p.webxdc-muted { "@" (&account.username) @if let Some(domain) = &account.domain { "@" (domain) } }
                                    }
                                    div.webxdc-actions {
                                        @if !member.accepted {
                                            form.webxdc-inline-form method="post" action={ "/web/webxdc/" (session.id) "/approve/" (account.id) } {
                                                input type="hidden" name="csrf" value=(user.as_ref().unwrap().csrf.as_str());
                                                button type="submit" { "Approve" }
                                            }
                                        }
                                        form.webxdc-inline-form method="post" action={ "/web/webxdc/" (session.id) "/remove/" (account.id) } {
                                            input type="hidden" name="csrf" value=(user.as_ref().unwrap().csrf.as_str());
                                            button.settings-button--plain type="submit" { "Remove" }
                                        }
                                    }
                                }
                            }
                        }
                        @for member in &guests {
                            div.webxdc-participant {
                                div { strong { (&member.display_name) } p.webxdc-muted { "Guest" } }
                                div.webxdc-actions {
                                    @if !member.accepted {
                                        form.webxdc-inline-form method="post" action={ "/web/webxdc/" (session.id) "/guest/" (member.id) "/approve" } {
                                            input type="hidden" name="csrf" value=(user.as_ref().unwrap().csrf.as_str());
                                            button type="submit" { "Approve" }
                                        }
                                    }
                                    form.webxdc-inline-form method="post" action={ "/web/webxdc/" (session.id) "/guest/" (member.id) "/remove" } {
                                        input type="hidden" name="csrf" value=(user.as_ref().unwrap().csrf.as_str());
                                        button.settings-button--plain type="submit" { "Remove" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            details.webxdc-details {
                summary { "About this session" }
                div.webxdc-details__body {
                    dl.webxdc-facts {
                        div { dt { "App file" } dd { (&session.bundle_name) } }
                        div { dt { "Host" } dd { (&session.coordinator_uri) } }
                        div { dt { "Joining" } dd { @if session.membership_policy == "approval" { "Creator approval" } @else { "Anyone with the link" } } }
                        div { dt { "Saved updates" } dd { (session.last_serial) } }
                    }
                    @if !owner && joined {
                        form method="post" action={ "/web/webxdc/" (session.id) "/leave" } {
                            input type="hidden" name="csrf" value=(user.as_ref().unwrap().csrf.as_str());
                            button.settings-button--plain type="submit" { "Leave session" }
                        }
                    }
                    @if let Some(guest) = &guest {
                        form method="post" action={ "/web/webxdc/" (session.id) "/guest/leave" } {
                            input type="hidden" name="csrf" value=(&guest.csrf);
                            button.settings-button--plain type="submit" { "Leave and forget guest" }
                        }
                    }
                }
            }
            (privacy_details())
            @if owner {
                details.webxdc-details {
                    summary { "Manage session" }
                    div.webxdc-details__body {
                        @if !session.ended() {
                            p { "Closing ends the session for everyone. App data is deleted after 30 days." }
                            form method="post" action={ "/web/webxdc/" (session.id) "/close" } {
                                input type="hidden" name="csrf" value=(user.as_ref().unwrap().csrf.as_str());
                                button.settings-button--plain type="submit" { "Close session" }
                            }
                        }
                        div.webxdc-danger-zone {
                            h3 { "Delete session" }
                            p { "Permanently removes the app, saved updates, and access for everyone. Invitation posts remain." }
                            form.settings-form method="post" action={ "/web/webxdc/" (session.id) "/delete" } {
                                input type="hidden" name="csrf" value=(user.as_ref().unwrap().csrf.as_str());
                                label.webxdc-consent {
                                    input required type="checkbox" name="confirm" value="delete";
                                    span { "I understand this cannot be undone." }
                                }
                                button.settings-button--danger type="submit" { "Delete for everyone" }
                            }
                        }
                    }
                }
            }
        }
    };
    let locale = user
        .as_ref()
        .map_or_else(|| Locale::from_headers(headers), |user| user.locale);
    Ok(layout::shell_visitor_localized(
        &session.name,
        user.as_ref(),
        anon_nav(state).await,
        &body,
        locale,
    )
    .into_response())
}

#[derive(Deserialize)]
pub struct CsrfForm {
    csrf: String,
}

async fn get_session(state: &AppState, id: i64) -> Result<Session, ApiError> {
    if webxdc::tombstone_by_id(&state.pool, id).await?.is_some() {
        return Err(ApiError::Gone);
    }
    webxdc::find(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)
}

#[derive(Deserialize)]
pub struct GuestJoinForm {
    display_name: String,
    accept_risk: Option<String>,
}

pub async fn guest_join(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<GuestJoinForm>,
) -> Result<Response, ApiError> {
    if !crate::auth::same_origin_request(&headers, &state.config.domain) {
        return Err(ApiError::Forbidden("cross-site guest join refused".into()));
    }
    if form.accept_risk.as_deref() != Some("yes") {
        return Err(ApiError::Unprocessable(
            "Guest privacy and persistence disclosure must be accepted".into(),
        ));
    }
    let display_name = form.display_name.trim();
    if display_name.is_empty() || display_name.chars().count() > 80 {
        return Err(ApiError::Unprocessable(
            "Guest display name must be between 1 and 80 characters".into(),
        ));
    }
    let session = get_session(&state, id).await?;
    if !local_session(&state, &session) {
        return Err(ApiError::Forbidden(
            "Guest mode is available only on the session coordinator".into(),
        ));
    }
    if session.ended() {
        return Err(ApiError::Gone);
    }
    let guest_id = plamenu_db::id::next();
    let raw_token = crate::auth::generate_secret();
    let participant_uri = format!("{}#guest-{guest_id}", session.coordinator_uri);
    let self_addr = crate::auth::generate_secret();
    webxdc::create_guest(
        &state.pool,
        guest_id,
        session.id,
        display_name,
        &crate::auth::hash_secret(&raw_token),
        &participant_uri,
        &self_addr,
        session.membership_policy == "open",
    )
    .await?;
    state.webxdc_realtime.refresh(session.id);
    let mut response = Redirect::to(&local_url(&state, &session)).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        set_guest_cookie(session.id, &raw_token)
            .parse()
            .expect("generated guest cookie is valid"),
    );
    Ok(response)
}

pub async fn guest_leave(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> Result<Response, ApiError> {
    let context = guest_context(&state, id, &headers)
        .await?
        .ok_or_else(|| ApiError::Forbidden("No guest identity for this session".into()))?;
    if context.csrf != form.csrf {
        return Err(ApiError::Forbidden("invalid CSRF token".into()));
    }
    webxdc::remove_guest(&state.pool, id, context.guest.id).await?;
    state.webxdc_realtime.invalidate(
        id,
        Some(crate::webxdc_realtime::Peer::Guest(context.guest.id)),
    );
    let session = get_session(&state, id).await?;
    let mut response = Redirect::to(&local_url(&state, &session)).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        clear_guest_cookie(id)
            .parse()
            .expect("generated guest cookie is valid"),
    );
    Ok(response)
}

pub async fn approve_guest(
    State(state): State<AppState>,
    user: WebUser,
    Path((id, guest_id)): Path<(i64, i64)>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, ApiError> {
    require_csrf(&user, &form.csrf)?;
    let session = get_session(&state, id).await?;
    if session.creator_account_id != Some(user.current.account.id) {
        return Err(ApiError::Forbidden(
            "Only the session creator can approve guests".into(),
        ));
    }
    webxdc::approve_guest(&state.pool, id, guest_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    state.webxdc_realtime.refresh(id);
    Ok(Redirect::to(&local_url(&state, &session)).into_response())
}

pub async fn remove_guest(
    State(state): State<AppState>,
    user: WebUser,
    Path((id, guest_id)): Path<(i64, i64)>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, ApiError> {
    require_csrf(&user, &form.csrf)?;
    let session = get_session(&state, id).await?;
    if session.creator_account_id != Some(user.current.account.id) {
        return Err(ApiError::Forbidden(
            "Only the session creator can remove guests".into(),
        ));
    }
    if !webxdc::remove_guest(&state.pool, id, guest_id).await? {
        return Err(ApiError::NotFound);
    }
    state
        .webxdc_realtime
        .invalidate(id, Some(crate::webxdc_realtime::Peer::Guest(guest_id)));
    Ok(Redirect::to(&local_url(&state, &session)).into_response())
}

pub async fn join(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, ApiError> {
    require_csrf(&user, &form.csrf)?;
    let session = get_session(&state, id).await?;
    protocol::join(&state, &session, &user.current.account).await?;
    Ok(Redirect::to(&local_url(&state, &session)).into_response())
}

pub async fn leave(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, ApiError> {
    require_csrf(&user, &form.csrf)?;
    let session = get_session(&state, id).await?;
    protocol::leave(&state, &session, &user.current.account).await?;
    Ok(Redirect::to("/webxdc").into_response())
}

pub async fn approve(
    State(state): State<AppState>,
    user: WebUser,
    Path((id, participant)): Path<(i64, i64)>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, ApiError> {
    require_csrf(&user, &form.csrf)?;
    let session = get_session(&state, id).await?;
    if session.creator_account_id != Some(user.current.account.id) {
        return Err(ApiError::Forbidden(
            "Only the session creator can approve members".into(),
        ));
    }
    protocol::approve(&state, &session, participant).await?;
    Ok(Redirect::to(&local_url(&state, &session)).into_response())
}

pub async fn remove(
    State(state): State<AppState>,
    user: WebUser,
    Path((id, participant)): Path<(i64, i64)>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, ApiError> {
    require_csrf(&user, &form.csrf)?;
    let session = get_session(&state, id).await?;
    if session.creator_account_id != Some(user.current.account.id) {
        return Err(ApiError::Forbidden(
            "Only the session creator can remove members".into(),
        ));
    }
    let participant = account::find_by_id(&state.pool, participant)
        .await?
        .ok_or(ApiError::NotFound)?;
    protocol::remove_participant(&state, &session, &participant).await?;
    Ok(Redirect::to(&local_url(&state, &session)).into_response())
}

pub async fn close(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, ApiError> {
    require_csrf(&user, &form.csrf)?;
    let session = get_session(&state, id).await?;
    if session.creator_account_id != Some(user.current.account.id) {
        return Err(ApiError::Forbidden(
            "Only the session creator can close it".into(),
        ));
    }
    protocol::close_local(&state, &session).await?;
    Ok(Redirect::to(&local_url(&state, &session)).into_response())
}

#[derive(Deserialize)]
pub struct DeleteForm {
    csrf: String,
    confirm: Option<String>,
}

pub async fn delete(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<DeleteForm>,
) -> Result<Response, ApiError> {
    require_csrf(&user, &form.csrf)?;
    if form.confirm.as_deref() != Some("delete") {
        return Err(ApiError::Unprocessable(
            "Deletion must be explicitly confirmed".into(),
        ));
    }
    let session = get_session(&state, id).await?;
    if session.creator_account_id != Some(user.current.account.id) {
        return Err(ApiError::Forbidden(
            "Only the session creator can delete it".into(),
        ));
    }
    protocol::delete_local(&state, &session).await?;
    Ok(Redirect::to("/webxdc").into_response())
}

pub async fn play(
    State(state): State<AppState>,
    MaybeWebUser(user): MaybeWebUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let session = get_session(&state, id).await?;
    let (replay_boundary, self_addr, display_name, csrf) = if let Some(user) = &user {
        let membership = webxdc::membership(&state.pool, id, user.current.account.id)
            .await?
            .filter(|membership| membership.accepted)
            .ok_or_else(|| ApiError::Forbidden("Join the session before opening its app".into()))?;
        let display_name = if user.current.account.display_name.is_empty() {
            user.current.account.username.clone()
        } else {
            user.current.account.display_name.clone()
        };
        (
            membership.replay_boundary,
            membership.self_addr,
            display_name,
            user.csrf.clone(),
        )
    } else {
        let guest = guest_context(&state, id, &headers)
            .await?
            .filter(|context| context.guest.accepted)
            .ok_or_else(|| ApiError::Forbidden("Join the session before opening its app".into()))?;
        if !local_session(&state, &session) {
            return Err(ApiError::Forbidden(
                "Guest mode is available only on the session coordinator".into(),
            ));
        }
        (
            guest.guest.replay_boundary,
            guest.guest.self_addr,
            guest.guest.display_name,
            guest.csrf,
        )
    };
    if !webxdc::has_complete_prefix(&state.pool, id, replay_boundary).await? {
        return Err(ApiError::ServiceUnavailable(
            "Session history is still synchronizing".into(),
        ));
    }
    if session.ended() {
        return Err(ApiError::Gone);
    }
    let addr: String = url::form_urlencoded::byte_serialize(self_addr.as_bytes()).collect();
    let self_name: String = url::form_urlencoded::byte_serialize(display_name.as_bytes()).collect();
    let runtime_origin = format!("https://{}", state.config.webxdc_session_domain(id));
    let src = format!("{runtime_origin}/index.html?self_addr={addr}&self_name={self_name}");
    let body = html! {
        section.column.webxdc-player {
            header.webxdc-player__head {
                div { h1 { (&session.name) } }
                div.webxdc-actions {
                    button.pill-button type="button" data-webxdc-fullscreen hidden aria-pressed="false" { "Fullscreen" }
                    a.pill-button href=(local_url(&state, &session)) { "Session details" }
                }
            }
            p.webxdc-fullscreen-error role="status" hidden { "Fullscreen could not be opened. Try again or use your browser’s fullscreen control." }
            p.webxdc-player__notice role="status" hidden {}
            iframe src=(src) title=(&session.name) sandbox="allow-scripts allow-same-origin allow-pointer-lock" referrerpolicy="no-referrer"
                data-webxdc-session=(id) data-webxdc-origin=(runtime_origin) data-webxdc-csrf=(csrf)
                class="webxdc-frame" {}
            (privacy_details())
            script defer src="/assets/webxdc-host.js" {}
        }
    };
    Ok(layout::shell(&session.name, user.as_ref(), &body).into_response())
}

#[derive(Deserialize)]
pub struct UpdatesQuery {
    #[serde(default)]
    after: i64,
}

pub async fn updates(
    State(state): State<AppState>,
    MaybeWebUser(user): MaybeWebUser,
    Path(id): Path<i64>,
    Query(query): Query<UpdatesQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let session = get_session(&state, id).await?;
    let replay_boundary = if let Some(user) = &user {
        webxdc::membership(&state.pool, id, user.current.account.id)
            .await?
            .filter(|membership| membership.accepted)
            .ok_or_else(|| ApiError::Forbidden("You have not joined this Webxdc session".into()))?
            .replay_boundary
    } else {
        guest_context(&state, id, &headers)
            .await?
            .filter(|context| context.guest.accepted)
            .ok_or_else(|| ApiError::Forbidden("You have not joined this Webxdc session".into()))?
            .guest
            .replay_boundary
    };
    let updates = webxdc::updates_after(&state.pool, id, query.after.max(0)).await?;
    let synchronized = webxdc::has_complete_prefix(&state.pool, id, replay_boundary).await?;
    Ok(Json(json!({
        "updates": updates.into_iter().map(|update| json!({
            "serial": update.serial,
            "webxdcUpdate": update.webxdc_update,
        })).collect::<Vec<_>>(),
        "maxSerial": session.last_serial,
        "replayBoundary": replay_boundary,
        "synchronized": synchronized,
        "ended": session.ended(),
    }))
    .into_response())
}

pub async fn send_update(
    State(state): State<AppState>,
    MaybeWebUser(user): MaybeWebUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Json(update): Json<Value>,
) -> Result<Response, ApiError> {
    let csrf = headers
        .get("x-csrf-token")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let session = get_session(&state, id).await?;
    let serial = if let Some(user) = &user {
        require_csrf(user, csrf)?;
        protocol::submit_update(&state, &session, &user.current.account, update)
            .await?
            .map(|stored| stored.serial)
    } else {
        let guest = guest_context(&state, id, &headers)
            .await?
            .filter(|context| context.guest.accepted)
            .ok_or_else(|| ApiError::Forbidden("You have not joined this Webxdc session".into()))?;
        if guest.csrf != csrf {
            return Err(ApiError::Forbidden("invalid CSRF token".into()));
        }
        Some(
            protocol::submit_guest_update(&state, &session, &guest.guest, update)
                .await?
                .serial,
        )
    };
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "accepted": true,
            "serial": serial,
        })),
    )
        .into_response())
}

/// Same-origin, cookie-authenticated socket; app code only sees its `MessagePort`.
pub async fn realtime(
    State(state): State<AppState>,
    MaybeWebUser(user): MaybeWebUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    upgrade: axum::extract::ws::WebSocketUpgrade,
) -> Result<Response, ApiError> {
    if headers.get(header::ORIGIN).and_then(|v| v.to_str().ok())
        != Some(format!("https://{}", state.config.domain).as_str())
    {
        return Err(ApiError::Forbidden(
            "cross-origin realtime connection refused".into(),
        ));
    }
    let session = get_session(&state, id).await?;
    if session.ended() {
        return Err(ApiError::Gone);
    }
    let peer = if let Some(user) = user {
        webxdc::membership(&state.pool, id, user.current.account.id)
            .await?
            .filter(|m| m.accepted)
            .ok_or_else(|| {
                ApiError::Forbidden("Join the session before opening its channel".into())
            })?;
        crate::webxdc_realtime::Peer::Account(user.current.account.id)
    } else {
        let guest = guest_context(&state, id, &headers)
            .await?
            .filter(|g| g.guest.accepted && local_session(&state, &session))
            .ok_or_else(|| {
                ApiError::Forbidden("Join the session before opening its channel".into())
            })?;
        crate::webxdc_realtime::Peer::Guest(guest.guest.id)
    };
    Ok(upgrade
        .max_message_size(crate::webxdc_realtime::MAX_BYTES)
        .max_frame_size(crate::webxdc_realtime::MAX_BYTES)
        .on_upgrade(move |socket| {
            crate::webxdc_realtime::socket(state, session.coordinator_uri, peer, socket)
        }))
}

fn privacy_details() -> Markup {
    html! {
        details.webxdc-details {
            summary { "App privacy" }
            div.webxdc-details__body {
                p { "Apps can see your name and what you share inside the session. Plamenu keeps your account credentials and unrelated data separate." }
                p { "Browser isolation cannot block every external connection. Only open apps you trust with what you enter or import." }
                p { "Guest access uses a browser cookie and lasts 30 days. Clearing cookies loses that access. The host can see your IP address and remove participants." }
            }
        }
    }
}
