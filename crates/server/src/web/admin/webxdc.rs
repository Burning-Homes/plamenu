//! Server-wide Webxdc storage and lifecycle controls.
use axum::extract::{DefaultBodyLimit, Form, FromRequest, Multipart, Path, Query, Request, State};
use axum::response::{IntoResponse, Redirect, Response};
use maud::{Markup, html};
use plamenu_db::{account, role::permission, webxdc};
use serde::Deserialize;

use super::{WebAdmin, admin_shell};
use crate::web::webxdc::storage_size as size;
use crate::{AppState, admin_log, error::ApiError, web::session::csrf_rejection};

#[derive(Default, Deserialize)]
pub struct IndexQuery {
    #[serde(default)]
    search: String,
    #[serde(default)]
    kind: String,
    max_id: Option<i64>,
    flash: Option<String>,
    source: Option<i64>,
}

pub async fn index(
    State(state): State<AppState>,
    admin: WebAdmin,
    Query(query): Query<IndexQuery>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_WEBXDC)?;
    let usage = webxdc::storage_usage(&state.pool, None)
        .await
        .map_err(api_err)?;
    let limits = webxdc::limits(&state.pool).await.map_err(api_err)?;
    let rows = webxdc::admin_sessions(&state.pool, &query.search, &query.kind, query.max_id, 40)
        .await
        .map_err(api_err)?;
    let body = html! {
        (super::flash_banner(query.flash.as_deref(), "The session action could not be completed."))
        p.admin__lead { (size(usage.total_bytes())) " of " (limits.total_mb) " MiB used · " (usage.sessions) " sessions · " (usage.packages) " apps" }
        p { "Apps: " (size(usage.package_bytes)) " · Session data: " (size(usage.data_bytes)) }
        p { a.pill-button href="/admin/webxdc/apps" { "Manage app library" } }
        @if admin.role.can(permission::MANAGE_SETTINGS) {
            p { a href="/admin/settings?open=webxdc" { "Configure Webxdc quotas and upload limits →" } }
        }
        details { summary { "How storage is counted" }
            p { "Each identical app is stored once, including its ZIP and expanded files. Durable updates belong to individual sessions. Totals measure retained payload bytes; database overhead and storage in users’ browsers are excluded. Ended sessions keep their data until deleted." }
        }
        form.admin-filter method="get" action="/admin/webxdc" {
            label { "Session or creator" input name="search" value=(&query.search); }
            label { "Show" select name="kind" {
                @for (value,label) in [("","All sessions"),("local","Hosted here"),("remote","Remote caches"),("ended","Ended")] {
                    option value=(value) selected[query.kind == value] { (label) }
                }
            } }
            button type="submit" { "Search" }
        }
        (crate::web::view::data_table(&html! {
            thead { tr { th { "Session" } th { "Status" } th { "App storage" } th { "Session data" } } }
            tbody {
                @if rows.is_empty() { tr { td colspan="4" { "No sessions match." } } }
                @for row in &rows {
                    tr {
                        td { a href=(format!("/admin/webxdc/{}",row.id)) { (&row.name) }
                            span.admin-table__sub title=(&row.creator_uri) { (&row.creator_label) } }
                        td { @if row.local { "Hosted here" } @else { "Remote cache" }
                            span.admin-table__sub { @if row.ended_at.is_some() { "Ended" } @else { "Active" } } }
                        td { (size(row.package_bytes)) span.admin-table__sub { @if row.package_sessions == 1 { "One session" } @else { "Shared by " (row.package_sessions) " sessions" } } }
                        td { (size(row.data_bytes)) }
                    }
                }
            }
        }))
        @if rows.len() == 40 {
            @if let Some(last) = rows.last() {
                p.admin-pager { a href=(format!("/admin/webxdc?max_id={}&search={}&kind={}", last.id,
                    url::form_urlencoded::byte_serialize(query.search.as_bytes()).collect::<String>(), url::form_urlencoded::byte_serialize(query.kind.as_bytes()).collect::<String>())) { "Older →" } }
            }
        }
    };
    Ok(admin_shell(&admin, "/admin/webxdc", "Webxdc sessions", &body).into_response())
}

fn instance_library_section(
    apps: &[webxdc::LibraryApp],
    csrf: &str,
    limits: webxdc::Limits,
) -> Markup {
    html! {
        section.admin-form {
            h3 { "Add an instance app" }
            p { "Every package is validated locally. New sessions pin the selected version; adding an update never changes a running session." }
            form method="post" action="/web/admin/webxdc/apps" enctype="multipart/form-data" {
                input type="hidden" name="csrf" value=(csrf);
                div.admin-form__grid {
                    label { "Package" input required type="file" name="bundle" accept=".xdc,application/webxdc+zip,application/x-webxdc,application/zip"; }
                    label { "Category" input name="category" maxlength="80"; }
                    label { "Initial visibility" select name="visibility" {
                        option value="hidden" { "Hidden for review" }
                        option value="instance" { "Available on this instance" }
                        option value="public" { "Public catalog" }
                    } }
                }
                label { "Description" textarea name="summary" maxlength="2000" rows="3" {} }
                p.settings-field__hint { "Maximum package size: " (limits.bundle_mb) " MiB." }
                button type="submit" { "Validate and add" }
            }
        }
        section.admin-list {
            h3 { "Instance library" }
            @if apps.is_empty() { p.empty { "No instance apps." } }
            @for app in apps {
                article.admin-record {
                    div.admin-record__head {
                        div { h4 { (&app.name) } p.admin-table__sub { (&app.visibility) " · " (size(app.package_bytes)) @if !app.version.is_empty() { " · " (&app.version) } } }
                        @if app.icon_path.is_some() { img src={ "/web/admin/webxdc/apps/version/" (app.version_id) "/icon" } alt="" width="48" height="48" loading="lazy"; }
                    }
                    @if !app.summary.is_empty() { p { (&app.summary) } }
                    @if app.update_available { p.admin-flash { "A newer personal version is ready for moderator review." } }
                    p.admin-table__sub { code { (&app.digest_multibase) } }
                    div.admin-actions {
                        @for (value,label) in [("hidden","Hide"),("instance","Instance only"),("public","Publish")] {
                            @if app.visibility != value {
                                form method="post" action={ "/web/admin/webxdc/apps/" (app.id) "/op" } {
                                    input type="hidden" name="csrf" value=(csrf);
                                    input type="hidden" name="op" value=(value);
                                    button type="submit" { (label) }
                                }
                            }
                        }
                    }
                    details {
                        summary { "Update or remove" }
                        form method="post" action={ "/web/admin/webxdc/apps/" (app.id) "/version" } enctype="multipart/form-data" {
                            input type="hidden" name="csrf" value=(csrf);
                            label { "New .xdc version" input required type="file" name="bundle" accept=".xdc,application/webxdc+zip,application/x-webxdc,application/zip"; }
                            button type="submit" { "Add version" }
                        }
                        form method="post" action={ "/web/admin/webxdc/apps/" (app.id) "/op" } {
                            input type="hidden" name="csrf" value=(csrf);
                            input type="hidden" name="op" value="delete";
                            button.admin-danger type="submit" { "Remove app" }
                        }
                    }
                }
            }
        }
    }
}

fn personal_promotions_section(apps: &[webxdc::LibraryApp], csrf: &str) -> Markup {
    html! {
        section.admin-list {
            h3 { "Personal apps eligible for promotion" }
            p { "Promotion adds an instance-library reference. The member keeps their personal copy and existing sessions do not change." }
            @if apps.is_empty() { p.empty { "No personal apps." } }
            @for app in apps {
                article.admin-record {
                    div.admin-record__head { h4 { (&app.name) } span.admin-table__sub { "Account #" (app.owner_account_id.unwrap_or_default()) } }
                    p.admin-table__sub { (size(app.package_bytes)) @if !app.version.is_empty() { " · " (&app.version) } }
                    @if app.promoted_instance_app_id.is_none() || app.update_available {
                        form method="post" action={ "/web/admin/webxdc/apps/" (app.id) "/promote" } {
                            input type="hidden" name="csrf" value=(csrf);
                            button type="submit" { @if app.promoted_instance_app_id.is_some() { "Promote new version" } @else { "Promote to instance library" } }
                        }
                    } @else {
                        span.webxdc-badge { "Promoted" }
                    }
                }
            }
        }
    }
}

fn catalog_sources_section(
    admin: &WebAdmin,
    sources: &[webxdc::CatalogSource],
    selected_source: Option<i64>,
    csrf: &str,
) -> Markup {
    html! {
        section.admin-form {
            h3 { "External catalog sources" }
            p { "Catalogs are opt-in and refresh only when a moderator asks. Discovery metadata is advisory; imported packages pass the same guarded download and ZIP validation as uploads." }
            form method="post" action="/web/admin/webxdc/catalog-sources" {
                input type="hidden" name="csrf" value=(csrf);
                div.admin-form__grid {
                    label { "Name" input required name="name" maxlength="120" placeholder="Webxdc apps"; }
                    label { "JSON feed URL" input required type="url" name="feed_url" maxlength="2048" placeholder="https://example.org/catalog.json"; }
                }
                button type="submit" { "Add source" }
            }
            @for source in sources {
                article.admin-record {
                    div.admin-record__head {
                        div { h4 { (&source.name) } p.admin-table__sub { (&source.feed_url) } }
                        @if selected_source == Some(source.id) { span.webxdc-badge { "Selected" } }
                    }
                    p.admin-table__sub {
                        @if let Some(at) = source.last_fetched_at { "Last refreshed " (admin.clock().element(at)) }
                        @else { "Not refreshed yet" }
                        @if let Some(error) = &source.last_error { " · Last error: " (error) }
                    }
                    div.admin-actions {
                        a.pill-button href={ "/admin/webxdc/apps?source=" (source.id) } { "Browse" }
                        form method="post" action={ "/web/admin/webxdc/catalog-sources/" (source.id) "/refresh" } {
                            input type="hidden" name="csrf" value=(csrf);
                            button type="submit" { "Refresh" }
                        }
                        form method="post" action={ "/web/admin/webxdc/catalog-sources/" (source.id) "/delete" } {
                            input type="hidden" name="csrf" value=(csrf);
                            button.settings-button--plain type="submit" { "Remove source" }
                        }
                    }
                }
            }
        }
    }
}

fn catalog_candidates_section(
    candidates: &[webxdc::CatalogCandidate],
    source_id: Option<i64>,
    csrf: &str,
) -> Markup {
    html! {
        @if let Some(source_id) = source_id {
            section.admin-list {
                h3 { "Catalog apps" }
                @if candidates.is_empty() { p.empty { "Refresh this source to browse its apps." } }
                @for candidate in candidates {
                    article.admin-record {
                        div.admin-record__head {
                            div { h4 { (&candidate.name) } p.admin-table__sub { @if !candidate.version.is_empty() { (&candidate.version) " · " } @if let Some(size) = candidate.advertised_size { (crate::web::webxdc::storage_size(size)) } } }
                            @if candidate.imported_app_id.is_some() { span.webxdc-badge { "Imported" } }
                        }
                        @if !candidate.summary.is_empty() { p { (&candidate.summary) } }
                        @if let Some(source) = &candidate.source_code_url { p { a href=(source) rel="noopener noreferrer" { "Source code" } } }
                        form method="post" action={ "/web/admin/webxdc/catalog-sources/" (source_id) "/import" } {
                            input type="hidden" name="csrf" value=(csrf);
                            input type="hidden" name="external_app_id" value=(&candidate.external_app_id);
                            button type="submit" { @if candidate.imported_app_id.is_some() { "Check for update" } @else { "Import for review" } }
                        }
                    }
                }
            }
        }
    }
}

pub async fn apps(
    State(state): State<AppState>,
    admin: WebAdmin,
    Query(query): Query<IndexQuery>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_WEBXDC)?;
    let apps = webxdc::admin_library(&state.pool).await.map_err(api_err)?;
    let personal = webxdc::personal_apps_for_admin(&state.pool)
        .await
        .map_err(api_err)?;
    let limits = webxdc::limits(&state.pool).await.map_err(api_err)?;
    let sources = webxdc::catalog_sources(&state.pool)
        .await
        .map_err(api_err)?;
    let selected_source = query
        .source
        .or_else(|| sources.first().map(|source| source.id));
    let candidates = match selected_source {
        Some(source_id) => webxdc::catalog_candidates(&state.pool, source_id)
            .await
            .map_err(api_err)?,
        None => Vec::new(),
    };
    let csrf = admin.user.csrf.as_str();
    let body = html! {
        (super::flash_banner(query.flash.as_deref(), "The app-library action could not be completed."))
        p { a href="/admin/webxdc" { "← Webxdc sessions and storage" } }
        (instance_library_section(&apps, csrf, limits))
        (personal_promotions_section(&personal, csrf))
        (catalog_sources_section(&admin, &sources, selected_source, csrf))
        (catalog_candidates_section(&candidates, selected_source, csrf))
    };
    Ok(admin_shell(&admin, "/admin/webxdc", "Webxdc app library", &body).into_response())
}

#[derive(Default)]
struct AppUploadForm {
    csrf: String,
    summary: String,
    category: String,
    visibility: String,
    filename: String,
    bytes: axum::body::Bytes,
    has_bundle: bool,
}

async fn parse_app_upload(
    request: Request,
    state: &AppState,
    limits: webxdc::Limits,
) -> Result<AppUploadForm, Response> {
    let mut multipart = Multipart::from_request(request, state)
        .await
        .map_err(IntoResponse::into_response)?;
    let mut form = AppUploadForm::default();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(IntoResponse::into_response)?
    {
        let name = field.name().unwrap_or_default().to_owned();
        if name == "bundle" {
            if form.has_bundle {
                return Err(
                    ApiError::Unprocessable("Upload exactly one Webxdc package.".into())
                        .into_response(),
                );
            }
            form.has_bundle = true;
            field
                .file_name()
                .unwrap_or("application.xdc")
                .clone_into(&mut form.filename);
            form.bytes = field.bytes().await.map_err(IntoResponse::into_response)?;
            if form.bytes.len() > limits.bundle_bytes() {
                return Err(ApiError::PayloadTooLargeWithMessage(format!(
                    "The Webxdc package must be at most {} MiB.",
                    limits.bundle_mb
                ))
                .into_response());
            }
            continue;
        }
        let value = field.text().await.map_err(IntoResponse::into_response)?;
        match name.as_str() {
            "csrf" => form.csrf = value,
            "summary" => form.summary = value,
            "category" => form.category = value,
            "visibility" => form.visibility = value,
            _ => {}
        }
    }
    Ok(form)
}

pub async fn create_app(
    State(state): State<AppState>,
    admin: WebAdmin,
    mut request: Request,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_WEBXDC)?;
    let limits = webxdc::limits(&state.pool).await.map_err(api_err)?;
    DefaultBodyLimit::max(limits.bundle_bytes() + 1024 * 1024).apply(&mut request);
    let form = parse_app_upload(request, &state, limits).await?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    if form.bytes.is_empty() {
        return Err(
            ApiError::Unprocessable("Choose a non-empty .xdc package.".into()).into_response(),
        );
    }
    let visibility = if form.visibility.is_empty() {
        "hidden"
    } else {
        &form.visibility
    };
    let app = crate::webxdc::save_instance_app(
        &state,
        crate::webxdc::LibraryUpload {
            actor: &admin.user.current.account,
            bundle_name: &form.filename,
            bundle_bytes: &form.bytes,
            summary: &form.summary,
            category: (!form.category.trim().is_empty()).then_some(form.category.as_str()),
        },
        visibility,
    )
    .await
    .map_err(IntoResponse::into_response)?;
    admin_log::record(
        &state.pool,
        admin.user.current.account.id,
        "create",
        &admin_log::Target::webxdc_app(&app),
    )
    .await
    .map_err(api_err)?;
    Ok(Redirect::to("/admin/webxdc/apps?flash=applied").into_response())
}

pub async fn add_app_version(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(app_id): Path<i64>,
    mut request: Request,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_WEBXDC)?;
    let limits = webxdc::limits(&state.pool).await.map_err(api_err)?;
    DefaultBodyLimit::max(limits.bundle_bytes() + 1024 * 1024).apply(&mut request);
    let form = parse_app_upload(request, &state, limits).await?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    if form.bytes.is_empty() {
        return Err(
            ApiError::Unprocessable("Choose a non-empty .xdc package.".into()).into_response(),
        );
    }
    let app = crate::webxdc::add_instance_app_version(
        &state,
        app_id,
        crate::webxdc::LibraryUpload {
            actor: &admin.user.current.account,
            bundle_name: &form.filename,
            bundle_bytes: &form.bytes,
            summary: "",
            category: None,
        },
    )
    .await
    .map_err(IntoResponse::into_response)?;
    admin_log::record(
        &state.pool,
        admin.user.current.account.id,
        "update",
        &admin_log::Target::webxdc_app(&app),
    )
    .await
    .map_err(api_err)?;
    Ok(Redirect::to("/admin/webxdc/apps?flash=applied").into_response())
}

#[derive(Deserialize)]
pub struct AppOpForm {
    csrf: String,
    op: String,
}

pub async fn app_op(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(app_id): Path<i64>,
    Form(form): Form<AppOpForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_WEBXDC)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let app = webxdc::library_app(&state.pool, app_id)
        .await
        .map_err(api_err)?
        .filter(|app| app.owner_account_id.is_none())
        .ok_or_else(|| ApiError::NotFound.into_response())?;
    match form.op.as_str() {
        "hidden" | "instance" | "public" => {
            webxdc::set_library_visibility(&state.pool, app_id, &form.op)
                .await
                .map_err(api_err)?
                .ok_or_else(|| ApiError::NotFound.into_response())?;
        }
        "delete" => {
            webxdc::delete_instance_app(&state.pool, app_id)
                .await
                .map_err(api_err)?;
        }
        _ => return Err(ApiError::BadRequest("Invalid app-library action".into()).into_response()),
    }
    admin_log::record(
        &state.pool,
        admin.user.current.account.id,
        &form.op,
        &admin_log::Target::webxdc_app(&app),
    )
    .await
    .map_err(api_err)?;
    Ok(Redirect::to("/admin/webxdc/apps?flash=applied").into_response())
}

#[derive(Deserialize)]
pub struct PromoteForm {
    csrf: String,
}

pub async fn promote_app(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(app_id): Path<i64>,
    Form(form): Form<PromoteForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_WEBXDC)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let promoted = webxdc::promote_personal_app(&state.pool, app_id, admin.user.current.account.id)
        .await
        .map_err(api_err)?;
    admin_log::record(
        &state.pool,
        admin.user.current.account.id,
        "promote",
        &admin_log::Target::webxdc_app(&promoted),
    )
    .await
    .map_err(api_err)?;
    Ok(Redirect::to("/admin/webxdc/apps?flash=applied").into_response())
}

pub async fn app_icon(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(version_id): Path<i64>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_WEBXDC)?;
    let asset = webxdc::library_icon(&state.pool, version_id, false)
        .await
        .map_err(api_err)?
        .ok_or_else(|| ApiError::NotFound.into_response())?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, asset.media_type)],
        asset.bytes,
    )
        .into_response())
}

#[derive(Deserialize)]
pub struct CatalogSourceForm {
    csrf: String,
    name: String,
    feed_url: String,
}

pub async fn create_catalog_source(
    State(state): State<AppState>,
    admin: WebAdmin,
    Form(form): Form<CatalogSourceForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_WEBXDC)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let name = form.name.trim();
    let feed_url = form.feed_url.trim();
    let safe_scheme = crate::webxdc::catalog_url(feed_url).is_some();
    if name.is_empty() || name.chars().count() > 120 || !safe_scheme {
        return Err(ApiError::Unprocessable(
            "Enter a valid catalog name and HTTPS JSON URL".into(),
        )
        .into_response());
    }
    let source = webxdc::create_catalog_source(&state.pool, name, feed_url)
        .await
        .map_err(api_err)?;
    admin_log::record(
        &state.pool,
        admin.user.current.account.id,
        "create",
        &admin_log::Target::webxdc_catalog(&source),
    )
    .await
    .map_err(api_err)?;
    Ok(Redirect::to(&format!("/admin/webxdc/apps?source={}", source.id)).into_response())
}

#[derive(Deserialize)]
pub struct CatalogCsrfForm {
    csrf: String,
}

pub async fn refresh_catalog_source(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(source_id): Path<i64>,
    Form(form): Form<CatalogCsrfForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_WEBXDC)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let source = webxdc::catalog_source(&state.pool, source_id)
        .await
        .map_err(api_err)?
        .ok_or_else(|| ApiError::NotFound.into_response())?;
    crate::webxdc::refresh_catalog_source(&state, source_id)
        .await
        .map_err(IntoResponse::into_response)?;
    admin_log::record(
        &state.pool,
        admin.user.current.account.id,
        "refresh",
        &admin_log::Target::webxdc_catalog(&source),
    )
    .await
    .map_err(api_err)?;
    Ok(Redirect::to(&format!(
        "/admin/webxdc/apps?source={source_id}&flash=applied"
    ))
    .into_response())
}

pub async fn delete_catalog_source(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(source_id): Path<i64>,
    Form(form): Form<CatalogCsrfForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_WEBXDC)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let source = webxdc::catalog_source(&state.pool, source_id)
        .await
        .map_err(api_err)?
        .ok_or_else(|| ApiError::NotFound.into_response())?;
    webxdc::delete_catalog_source(&state.pool, source_id)
        .await
        .map_err(api_err)?;
    admin_log::record(
        &state.pool,
        admin.user.current.account.id,
        "delete",
        &admin_log::Target::webxdc_catalog(&source),
    )
    .await
    .map_err(api_err)?;
    Ok(Redirect::to("/admin/webxdc/apps?flash=applied").into_response())
}

#[derive(Deserialize)]
pub struct ImportCatalogForm {
    csrf: String,
    external_app_id: String,
}

pub async fn import_catalog_app(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(source_id): Path<i64>,
    Form(form): Form<ImportCatalogForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_WEBXDC)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let app = crate::webxdc::import_catalog_candidate(
        &state,
        &admin.user.current.account,
        source_id,
        &form.external_app_id,
    )
    .await
    .map_err(IntoResponse::into_response)?;
    admin_log::record(
        &state.pool,
        admin.user.current.account.id,
        "import",
        &admin_log::Target::webxdc_app(&app),
    )
    .await
    .map_err(api_err)?;
    Ok(Redirect::to(&format!(
        "/admin/webxdc/apps?source={source_id}&flash=applied"
    ))
    .into_response())
}

pub async fn show(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Query(query): Query<IndexQuery>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_WEBXDC)?;
    let session = webxdc::find(&state.pool, id)
        .await
        .map_err(api_err)?
        .ok_or_else(|| ApiError::NotFound.into_response())?;
    let storage = webxdc::session_storage(&state.pool, id)
        .await
        .map_err(api_err)?
        .ok_or_else(|| ApiError::NotFound.into_response())?;
    let coordinator = account::find_by_id(&state.pool, session.account_id)
        .await
        .map_err(api_err)?
        .ok_or_else(|| ApiError::NotFound.into_response())?;
    let creator = match session.creator_account_id {
        Some(id) => account::find_by_id(&state.pool, id)
            .await
            .map_err(api_err)?,
        None => None,
    };
    let local = coordinator.is_local();
    let reclaim = storage.data_bytes
        + if storage.package_sessions == 1 {
            storage.package_bytes
        } else {
            0
        };
    let body = html! {
        (super::flash_banner(query.flash.as_deref(), "The session action could not be completed."))
        p { a href="/admin/webxdc" { "← All sessions" } }
        p { @if local { "Hosted here" } @else { "Remote cache" } " · " @if session.ended() { "Ended" } @else { "Active" } }
        dl.admin-detail__grid {
            dt { "Creator" } dd { @if let Some(creator) = &creator { a href=(format!("/admin/accounts/{}",creator.id)) { "@" (&creator.username) } } @else { (&session.creator_uri) } }
            dt { "Created" } dd { (admin.clock().element_date(session.published_at)) }
            dt { "App" } dd { (&session.bundle_name) }
            dt { "App storage" } dd { (size(storage.package_bytes)) @if storage.package_sessions == 1 { " · One session" } @else { " · Shared by " (storage.package_sessions) " sessions" } }
            dt { "Session data" } dd { (size(storage.data_bytes)) " · " (session.last_serial) " durable updates" }
            dt { "Participants" } dd { (storage.members) " account" @if storage.members != 1 { "s" } " · " (storage.guests) " guest" @if storage.guests != 1 { "s" } }
            dt { "Storage released by deletion" } dd { (size(reclaim)) }
        }
        p { a href=(format!("/webxdc/session/{id}")) { "View session →" } }
        @if local && !session.ended() {
            p { "Ending stops the app for everyone. Data is retained for 30 days unless you delete it sooner." }
            (action_form(&admin,id,"close","End session"))
        }
        details { summary { @if local { "Delete session" } @else { "Remove remote cache" } }
            p { @if local {
                "Permanently delete this session and its data for everyone, including federated participants."
            } @else {
                "Remove this cached session and leave it for all participants on this server. The original session stays on its host and can be opened again later."
            } " Shared app files are removed only when no session or saved app version uses them." }
            (action_form(&admin,id,if local {"delete"} else {"evict"},if local {"Delete session and data"} else {"Remove cache and leave"}))
        }
    };
    Ok(admin_shell(&admin, "/admin/webxdc", &session.name, &body).into_response())
}

fn action_form(admin: &WebAdmin, id: i64, op: &str, label: &str) -> Markup {
    html! { form method="post" action=(format!("/web/admin/webxdc/{id}/op")) {
        input type="hidden" name="csrf" value=(&admin.user.csrf);
        input type="hidden" name="op" value=(op);
        button.admin-danger type="submit" { (label) }
    } }
}

#[derive(Deserialize)]
pub struct OpForm {
    csrf: String,
    op: String,
}

pub async fn op(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<OpForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_WEBXDC)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let session = webxdc::find(&state.pool, id)
        .await
        .map_err(api_err)?
        .ok_or_else(|| ApiError::NotFound.into_response())?;
    let coordinator = account::find_by_id(&state.pool, session.account_id)
        .await
        .map_err(api_err)?
        .ok_or_else(|| ApiError::NotFound.into_response())?;
    match form.op.as_str() {
        "close" if coordinator.is_local() => {
            crate::webxdc::close_local(&state, &session)
                .await
                .map_err(IntoResponse::into_response)?;
        }
        "delete" if coordinator.is_local() => {
            crate::webxdc::delete_local(&state, &session)
                .await
                .map_err(IntoResponse::into_response)?;
        }
        "evict" if !coordinator.is_local() => {
            crate::webxdc::evict_remote(&state, &session)
                .await
                .map_err(IntoResponse::into_response)?;
        }
        _ => return Err(ApiError::BadRequest("Invalid session action".into()).into_response()),
    }
    admin_log::record(
        &state.pool,
        admin.user.current.account.id,
        &form.op,
        &admin_log::Target::webxdc(&session),
    )
    .await
    .map_err(api_err)?;
    let target = if form.op == "close" {
        format!("/admin/webxdc/{id}?flash=applied")
    } else {
        "/admin/webxdc?flash=applied".to_owned()
    };
    Ok(Redirect::to(&target).into_response())
}

fn api_err(error: plamenu_db::DbError) -> Response {
    ApiError::from(error).into_response()
}
