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

fn visibility_label(visibility: &str) -> &str {
    match visibility {
        "public" => "Public",
        "instance" => "Instance only",
        "hidden" => "Hidden",
        _ => visibility,
    }
}

fn source_label(source_kind: &str) -> &str {
    match source_kind {
        "external" => "External catalog",
        "upload" => "Direct upload",
        "promotion" => "Member promotion",
        _ => source_kind,
    }
}

fn owner_label(app: &webxdc::LibraryApp) -> String {
    let Some(username) = app.owner_username.as_deref() else {
        return "Unknown member".into();
    };
    let handle = match app.owner_domain.as_deref() {
        Some(domain) => format!("@{username}@{domain}"),
        None => format!("@{username}"),
    };
    match app
        .owner_display_name
        .as_deref()
        .filter(|name| !name.is_empty())
    {
        Some(name) if name != username => format!("{name} ({handle})"),
        _ => handle,
    }
}

fn app_filter_text(app: &webxdc::LibraryApp) -> String {
    format!(
        "{} {} {} {} {} {} {}",
        app.name,
        app.summary,
        app.version,
        app.category.as_deref().unwrap_or_default(),
        app.visibility,
        source_label(&app.source_kind),
        owner_label(app),
    )
}

fn candidate_filter_text(candidate: &webxdc::CatalogCandidate) -> String {
    format!(
        "{} {} {} {}",
        candidate.name,
        candidate.summary,
        candidate.version,
        candidate.category.as_deref().unwrap_or_default(),
    )
}

fn catalog_icon_url(candidate: &webxdc::CatalogCandidate) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("source_id", &candidate.source_id.to_string())
        .append_pair("external_app_id", &candidate.external_app_id)
        .finish();
    format!("/webxdc/library/catalog-icon?{query}")
}

fn admin_app_icon(app: &webxdc::LibraryApp) -> Markup {
    html! {
        div.webxdc-admin-record__icon {
            @if app.icon_path.is_some() {
                img src={ "/web/admin/webxdc/apps/version/" (app.version_id) "/icon" }
                    alt="" width="52" height="52" loading="lazy";
            } @else {
                span aria-hidden="true" { (crate::web::view::icon("apps")) }
            }
        }
    }
}

fn library_search(placeholder: &str, filter_label: &str, states: &[(&str, &str)]) -> Markup {
    html! {
        div.admin-filter.webxdc-admin-filter hidden data-library-controls {
            label.admin-filter__search {
                span { "Search" }
                span.webxdc-admin-search {
                    (crate::web::view::icon("search"))
                    input type="search" placeholder=(placeholder) autocomplete="off" data-library-search;
                }
            }
            label {
                span { (filter_label) }
                select data-library-state-filter {
                    @for (value, label) in states {
                        option value=(value) { (label) }
                    }
                }
            }
            output.webxdc-admin-filter-count aria-live="polite" data-library-visible-count { }
        }
    }
}

fn instance_library_section(
    apps: &[webxdc::LibraryApp],
    csrf: &str,
    limits: webxdc::Limits,
) -> Markup {
    html! {
        details.admin-form.webxdc-admin-add {
            summary { (crate::web::view::icon("upload")) " Add an instance app" }
            div.webxdc-admin-add__body {
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
        }
        section.admin-list.webxdc-admin-section id="instance-library" data-library-filter {
            div.webxdc-admin-section__head {
                div { h3 { "Instance library" } p { "Apps controlled by this instance." } }
                span.webxdc-admin-count { (apps.len()) }
            }
            (library_search("Search instance apps", "Visibility", &[("", "All visibility"), ("hidden", "Hidden"), ("instance", "Instance only"), ("public", "Public")]))
            @if apps.is_empty() { p.empty { "No instance apps." } }
            p.empty.webxdc-admin-filter-empty hidden { "No instance apps match." }
            div.webxdc-admin-records {
            @for app in apps {
                article.admin-record.webxdc-admin-record data-library-record data-library-state=(&app.visibility)
                    data-library-text=(app_filter_text(app)) {
                    (admin_app_icon(app))
                    div.webxdc-admin-record__body {
                        div.admin-record__head {
                            h4 { (&app.name) }
                            span.webxdc-badge data-visibility=(&app.visibility) { (visibility_label(&app.visibility)) }
                        }
                        @if !app.summary.is_empty() { p.webxdc-admin-record__summary title=(&app.summary) { (&app.summary) } }
                        p.webxdc-admin-record__meta {
                            @if !app.version.is_empty() { "Version " (&app.version) " · " }
                            (size(app.package_bytes))
                            @if let Some(category) = &app.category { " · " (category) }
                            " · " (source_label(&app.source_kind))
                        }
                        @if app.update_available { p.webxdc-admin-notice { "New personal version available for review" } }
                        div.webxdc-admin-record__actions {
                            @for (value,label) in [("hidden","Hide"),("instance","Instance only"),("public","Publish publicly")] {
                                @if app.visibility != value {
                                    form method="post" action={ "/web/admin/webxdc/apps/" (app.id) "/op" } {
                                        input type="hidden" name="csrf" value=(csrf);
                                        input type="hidden" name="op" value=(value);
                                        button.settings-button--plain type="submit" { (label) }
                                    }
                                }
                            }
                        }
                        details.webxdc-admin-manage {
                            summary { "Manage package" }
                            dl.webxdc-admin-facts {
                                div { dt { "File" } dd { (&app.filename) } }
                                div { dt { "Digest" } dd { code { (&app.digest_multibase) } } }
                                @if let Some(source) = &app.source_code_url { div { dt { "Source" } dd { a href=(source) rel="noopener noreferrer" { "Source code ↗" } } } }
                            }
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
    }
}

fn personal_promotions_section(apps: &[webxdc::LibraryApp], csrf: &str) -> Markup {
    html! {
        section.admin-list.webxdc-admin-section id="personal-apps" data-library-filter {
            div.webxdc-admin-section__head {
                div { h3 { "Personal apps" } p { "Review member apps for optional instance promotion." } }
                span.webxdc-admin-count { (apps.len()) }
            }
            (library_search("Search personal apps or owners", "Status", &[("", "All status"), ("available", "Available"), ("promoted", "Promoted"), ("update", "Update available")]))
            @if apps.is_empty() { p.empty { "No personal apps." } }
            p.empty.webxdc-admin-filter-empty hidden { "No personal apps match." }
            div.webxdc-admin-records {
            @for app in apps {
                @let state = if app.update_available { "update" } else if app.promoted_instance_app_id.is_some() { "promoted" } else { "available" };
                article.admin-record.webxdc-admin-record data-library-record data-library-state=(state)
                    data-library-text=(app_filter_text(app)) {
                    (admin_app_icon(app))
                    div.webxdc-admin-record__body {
                        div.admin-record__head {
                            h4 { (&app.name) }
                            @if app.update_available { span.webxdc-badge { "Update available" } }
                            @else if app.promoted_instance_app_id.is_some() { span.webxdc-badge { "Promoted" } }
                            @else { span.webxdc-badge.webxdc-badge--ended { "Personal" } }
                        }
                        @if !app.summary.is_empty() { p.webxdc-admin-record__summary title=(&app.summary) { (&app.summary) } }
                        p.webxdc-admin-record__meta {
                            @if let Some(owner_id) = app.owner_account_id {
                                a href={ "/admin/accounts/" (owner_id) } { (owner_label(app)) }
                                " · "
                            }
                            @if !app.version.is_empty() { "Version " (&app.version) " · " }
                            (size(app.package_bytes))
                            @if let Some(category) = &app.category { " · " (category) }
                        }
                        @if app.promoted_instance_app_id.is_none() || app.update_available {
                            form method="post" action={ "/web/admin/webxdc/apps/" (app.id) "/promote" } {
                                input type="hidden" name="csrf" value=(csrf);
                                button type="submit" { @if app.promoted_instance_app_id.is_some() { "Promote new version" } @else { "Promote to instance library" } }
                            }
                        }
                    }
                }
            }
            }
            p.settings-field__hint { "Promotion adds an instance-library reference. The member's copy and existing sessions stay unchanged." }
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
        section.admin-list.webxdc-admin-section {
            div.webxdc-admin-section__head {
                div { h3 { "Catalog sources" } p { "External catalogs available to members and moderators." } }
                span.webxdc-admin-count { (sources.len()) }
            }
            div.webxdc-admin-sources {
                @for source in sources {
                    article.admin-record.webxdc-admin-source {
                        div.webxdc-admin-source__icon aria-hidden="true" { (crate::web::view::icon("link")) }
                        div.webxdc-admin-source__body {
                            div.admin-record__head {
                                h4 { (&source.name) }
                                @if selected_source == Some(source.id) { span.webxdc-badge { "Viewing" } }
                            }
                            p.webxdc-admin-record__meta title=(&source.feed_url) { (&source.feed_url) }
                            p.webxdc-admin-record__meta {
                                @if let Some(at) = source.last_fetched_at { "Refreshed " (admin.clock().element(at)) }
                                @else { "Not refreshed yet" }
                            }
                            @if let Some(error) = &source.last_error { p.webxdc-admin-error { (error) } }
                            div.webxdc-admin-record__actions {
                                @if selected_source != Some(source.id) { a.pill-button href={ "/admin/webxdc/apps?source=" (source.id) "#catalog-apps" } { "Browse apps" } }
                                form method="post" action={ "/web/admin/webxdc/catalog-sources/" (source.id) "/refresh" } {
                                    input type="hidden" name="csrf" value=(csrf);
                                    button.settings-button--plain type="submit" { "Refresh" }
                                }
                                form method="post" action={ "/web/admin/webxdc/catalog-sources/" (source.id) "/delete" }
                                    data-confirm="Remove this catalog source? Imported apps remain in the instance library." {
                                    input type="hidden" name="csrf" value=(csrf);
                                    button.settings-button--plain.webxdc-admin-remove type="submit" { "Remove source" }
                                }
                            }
                        }
                    }
                }
            }
            details.admin-form.webxdc-admin-add {
                summary { "Add a catalog source" }
                div.webxdc-admin-add__body {
                    p { "Catalogs refresh only when a moderator asks. Feed metadata is advisory; imported packages still pass guarded download and ZIP validation." }
                    form method="post" action="/web/admin/webxdc/catalog-sources" {
                        input type="hidden" name="csrf" value=(csrf);
                        div.admin-form__grid {
                            label { "Name" input required name="name" maxlength="120" placeholder="Webxdc apps"; }
                            label { "JSON feed URL" input required type="url" name="feed_url" maxlength="2048" placeholder="https://example.org/catalog.json"; }
                        }
                        button type="submit" { "Add source" }
                    }
                }
            }
        }
    }
}

fn catalog_candidates_section(
    candidates: &[webxdc::CatalogCandidate],
    source_id: Option<i64>,
    source_name: Option<&str>,
    csrf: &str,
) -> Markup {
    html! {
        @if let Some(source_id) = source_id {
            section.admin-list.webxdc-admin-section id="catalog-apps" data-library-filter {
                div.webxdc-admin-section__head {
                    div { h3 { "Catalog apps" } p { "Review " @if let Some(name) = source_name { (name) " " } "feed entries before importing them." } }
                    span.webxdc-admin-count { (candidates.len()) }
                }
                (library_search("Search catalog apps", "Status", &[("", "All status"), ("available", "Not imported"), ("imported", "Imported")]))
                @if candidates.is_empty() { p.empty { "Refresh this source to browse its apps." } }
                p.empty.webxdc-admin-filter-empty hidden { "No catalog apps match." }
                div.webxdc-admin-records {
                @for candidate in candidates {
                    @let state = if candidate.imported_app_id.is_some() { "imported" } else { "available" };
                    article.admin-record.webxdc-admin-record data-library-record data-library-state=(state)
                        data-library-text=(candidate_filter_text(candidate)) {
                        div.webxdc-admin-record__icon {
                            @if candidate.icon_url.is_some() {
                                img src=(catalog_icon_url(candidate)) alt="" width="52" height="52" loading="lazy";
                            } @else {
                                span aria-hidden="true" { (crate::web::view::icon("apps")) }
                            }
                        }
                        div.webxdc-admin-record__body {
                            div.admin-record__head {
                                h4 { (&candidate.name) }
                                @if candidate.imported_app_id.is_some() { span.webxdc-badge { "Imported" } }
                                @else { span.webxdc-badge.webxdc-badge--ended { "Unreviewed" } }
                            }
                            @if !candidate.summary.is_empty() { p.webxdc-admin-record__summary title=(&candidate.summary) { (&candidate.summary) } }
                            p.webxdc-admin-record__meta {
                                @if !candidate.version.is_empty() { "Version " (&candidate.version) }
                                @if let Some(candidate_size) = candidate.advertised_size { " · About " (size(candidate_size)) }
                                @if let Some(category) = &candidate.category { " · " (category) }
                            }
                            div.webxdc-admin-record__actions {
                                form method="post" action={ "/web/admin/webxdc/catalog-sources/" (source_id) "/import" } {
                                    input type="hidden" name="csrf" value=(csrf);
                                    input type="hidden" name="external_app_id" value=(&candidate.external_app_id);
                                    button type="submit" { @if candidate.imported_app_id.is_some() { "Revalidate / check update" } @else { "Import for review" } }
                                }
                                @if let Some(source) = &candidate.source_code_url { a href=(source) rel="noopener noreferrer" { "Source ↗" } }
                            }
                        }
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
    let selected_source_name = sources
        .iter()
        .find(|source| Some(source.id) == selected_source)
        .map(|source| source.name.as_str());
    let csrf = admin.user.csrf.as_str();
    let body = html! {
        (super::flash_banner(query.flash.as_deref(), "The app-library action could not be completed."))
        nav.webxdc-admin-nav aria-label="Webxdc administration" {
            a href="/admin/webxdc" { "← Sessions and storage" }
            a href="#instance-library" { "Instance" }
            a href="#personal-apps" { "Personal" }
            a href="#catalog-apps" { "Catalog" }
        }
        p.admin__lead {
            (apps.len()) @if apps.len() == 1 { " instance app" } @else { " instance apps" }
            " · " (personal.len()) @if personal.len() == 1 { " personal app" } @else { " personal apps" }
            " · " (candidates.len()) @if candidates.len() == 1 { " catalog entry" } @else { " catalog entries" }
        }
        (instance_library_section(&apps, csrf, limits))
        (personal_promotions_section(&personal, csrf))
        (catalog_sources_section(&admin, &sources, selected_source, csrf))
        (catalog_candidates_section(&candidates, selected_source, selected_source_name, csrf))
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
