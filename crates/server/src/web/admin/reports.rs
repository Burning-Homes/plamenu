//! Report moderation pages for the admin dashboard — the browser counterpart
//! to `routes::admin_reports`. Reads reuse `db::report`; the assign/resolve
//! verbs route through the same `db::report` mutators the REST API uses, so a
//! report resolved here looks identical to one resolved through the API. The
//! report-notes forms are the write surface for the schema-only
//! `report_notes` table (Mastodon's `ReportNote`). All pages require
//! `MANAGE_REPORTS`.

use axum::extract::{Form, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use maud::{Markup, PreEscaped, html};
use plamenu_db::account::{self, Account};
use plamenu_db::report::{self, AdminReportFilter, Report};
use plamenu_db::role::permission;
use plamenu_db::{report_note, rule, status};
use serde::Deserialize;

use super::super::clock::{ViewerClock, zone_chip};
use super::accounts::handle;
use super::{WebAdmin, admin_shell};
use crate::web::session::csrf_rejection;
use crate::{AppState, admin_log};

/// Page size for the report listing.
const PAGE_LIMIT: i64 = 50;

#[derive(Debug, Default, Deserialize)]
pub struct IndexQuery {
    /// `open` (default) | `resolved` | `all`.
    status: Option<String>,
    max_id: Option<i64>,
    flash: Option<String>,
}

/// `GET /admin/reports` — the report listing, scoped to open / resolved / all
/// (mirroring Mastodon's `status_scope`).
pub async fn index(
    State(state): State<AppState>,
    admin: WebAdmin,
    Query(query): Query<IndexQuery>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_REPORTS)?;

    let scope = query.status.as_deref().unwrap_or("open");
    let (resolved, unresolved) = match scope {
        "resolved" => (true, false),
        "all" => (true, true),
        _ => (false, false), // open
    };
    let filter = AdminReportFilter {
        resolved,
        unresolved,
        max_id: query.max_id,
        limit: PAGE_LIMIT,
        ..AdminReportFilter::default()
    };
    let reports = report::list_for_admin(&state.pool, &filter)
        .await
        .map_err(api_err)?;
    let next_max = (i64::try_from(reports.len()).unwrap_or(i64::MAX) == PAGE_LIMIT)
        .then(|| reports.last().map(|r| r.id))
        .flatten();

    // Both party columns of the whole page in one query — a serial reporter
    // or repeatedly-reported target used to be re-fetched once per row.
    let mut party_ids: Vec<i64> = reports
        .iter()
        .flat_map(|r| [r.account_id, r.target_account_id])
        .collect();
    party_ids.sort_unstable();
    party_ids.dedup();
    let parties: std::collections::HashMap<i64, plamenu_db::account::Account> =
        account::find_by_ids(&state.pool, &party_ids)
            .await
            .map_err(api_err)?
            .into_iter()
            .map(|a| (a.id, a))
            .collect();
    let rows: Vec<_> = reports
        .iter()
        .map(|report| {
            (
                report,
                parties.get(&report.account_id).cloned(),
                parties.get(&report.target_account_id).cloned(),
            )
        })
        .collect();

    let body = html! {
        (super::flash_banner(query.flash.as_deref(), "That action could not be completed."))
        (zone_chip(admin.clock()))
        form.admin-filter method="get" action="/admin/reports" {
            label {
                "Show"
                select name="status" {
                    @let opts = [("open", "Open"), ("resolved", "Resolved"), ("all", "All")];
                    @for (val, label) in opts {
                        option value=(val) selected[scope == val] { (label) }
                    }
                }
            }
            button type="submit" { "Filter" }
        }
        (crate::web::view::data_table(&html! {
            thead {
                tr {
                    th scope="col" { "#" } th scope="col" { "Target" } th scope="col" { "Reporter" }
                    th scope="col" { "Category" } th scope="col" { "State" }
                }
            }
            tbody data-paged {
                @if rows.is_empty() {
                    tr { td colspan="5" { "No reports match." } }
                }
                @for (report, reporter, target) in &rows {
                    tr {
                        td {
                            a href=(format!("/admin/reports/{}", report.id)) { (report.id) }
                        }
                        td { (party(target.as_ref())) }
                        td { (party(reporter.as_ref())) }
                        td { (report.category) }
                        td { (state_badge(report)) }
                    }
                }
            }
        }))
        @if let Some(max) = next_max {
            p.admin-pager {
                a href=(format!("/admin/reports?status={scope}&max_id={max}")) { "Older →" }
            }
        }
    };
    Ok(admin_shell(&admin, "/admin/reports", "Reports", &body).into_response())
}

#[derive(Debug, Default, Deserialize)]
pub struct ShowQuery {
    flash: Option<String>,
}

/// `GET /admin/reports/{id}` — the full review view: parties, category,
/// comment, cited statuses, violated rules, plus the assign/resolve verbs and
/// the report-note thread.
pub async fn show(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Query(query): Query<ShowQuery>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_REPORTS)?;

    let Some(report) = report::find_by_id(&state.pool, id).await.map_err(api_err)? else {
        return Ok(not_found(&admin));
    };
    let reporter = account::find_by_id(&state.pool, report.account_id)
        .await
        .map_err(api_err)?;
    let target = account::find_by_id(&state.pool, report.target_account_id)
        .await
        .map_err(api_err)?;
    let assigned = optional_account(&state, report.assigned_account_id)
        .await
        .map_err(api_err)?;
    let acted_by = optional_account(&state, report.action_taken_by_account_id)
        .await
        .map_err(api_err)?;
    let statuses = status::find_by_ids(&state.pool, &report.status_ids)
        .await
        .map_err(api_err)?;
    let rules = match &report.rule_ids {
        Some(ids) if !ids.is_empty() => {
            rule::find_by_ids(&state.pool, ids).await.map_err(api_err)?
        }
        _ => Vec::new(),
    };
    let all_rules = rule::list_ordered(&state.pool).await.map_err(api_err)?;
    let notes = report_note::for_report(&state.pool, id)
        .await
        .map_err(api_err)?;
    let csrf = admin.user.csrf.as_str();
    let resolved = report.action_taken_at.is_some();
    let assigned_to_me = report.assigned_account_id == Some(admin.user.current.account.id);

    let body = html! {
        (super::flash_banner(query.flash.as_deref(), "That action could not be completed."))
        p.admin-back { a href="/admin/reports" { "← Back to reports" } }
        div.admin-detail {
            h3 { "Report #" (report.id) }
            dl.admin-detail__grid {
                dt { "Target" }
                dd { (party_link(target.as_ref())) }
                dt { "Reporter" } dd { (party(reporter.as_ref())) }
                dt { "Category" } dd { (report.category) }
                dt { "State" } dd { (state_badge(&report)) }
                dt { "Forwarded" } dd { (yes_no(report.forwarded.unwrap_or(false))) }
                dt { "Assigned" } dd { (assigned.as_deref().unwrap_or("—")) }
                @if resolved {
                    dt { "Resolved by" } dd { (acted_by.as_deref().unwrap_or("—")) }
                }
                dt { "Filed" } dd { (admin.clock().element_date(report.created_at)) }
            }
            @if !report.comment.is_empty() {
                div.admin-report__comment {
                    h4 { "Comment" }
                    p { (report.comment) }
                }
            }
        }

        (cited_section(&rules, &statuses))

        (edit_section(&report, csrf, &all_rules))

        // ---- Assign / resolve verbs -------------------------------------
        div.admin-actions {
            @if !assigned_to_me {
                (op_form(id, csrf, "assign", "Assign to me"))
            } @else {
                (op_form(id, csrf, "unassign", "Unassign"))
            }
            @if resolved {
                (op_form(id, csrf, "reopen", "Reopen"))
            } @else {
                (op_form(id, csrf, "resolve", "Mark resolved"))
            }
        }
        p.admin-report__hint {
            @match target.as_ref() {
                Some(target_account) => {
                    a href=(format!("/admin/accounts/{}?report_id={}", target_account.id, report.id)) {
                        "Take action against " (handle(target_account)) " →"
                    }
                    " A moderation action applied there will cite this report and resolve it."
                }
                None => "The target account no longer exists.",
            }
        }

        (notes_section(id, csrf, &notes, admin.clock()))
    };
    Ok(admin_shell(&admin, "/admin/reports", &format!("Report #{id}"), &body).into_response())
}

/// The report categories Mastodon's `Report` model accepts.
const CATEGORIES: [&str; 4] = ["other", "spam", "legal", "violation"];

/// The category/rules edit form (the web face of `PUT
/// /api/v1/admin/reports/{id}`). Collapsed by default — most reports are
/// worked, not reclassified.
fn edit_section(report: &Report, csrf: &str, all_rules: &[rule::Rule]) -> Markup {
    let cited: &[i64] = report.rule_ids.as_deref().unwrap_or(&[]);
    html! {
        details.admin-form {
            summary { "Edit category" }
            form method="post" action=(format!("/web/admin/reports/{}/update", report.id)) {
                input type="hidden" name="csrf" value=(csrf);
                label {
                    "Category"
                    select name="category" {
                        @for category in CATEGORIES {
                            option value=(category) selected[report.category == category] {
                                (category)
                            }
                        }
                    }
                }
                @if !all_rules.is_empty() {
                    fieldset.admin-form__group {
                        legend { "Violated rules" }
                        div.admin-form__checks {
                            @for rule in all_rules {
                                label.admin-check {
                                    input type="checkbox" name="rule_ids" value=(rule.id)
                                        checked[cited.contains(&rule.id)];
                                    span { (rule.text) }
                                }
                            }
                        }
                    }
                }
                button type="submit" { "Save" }
            }
        }
    }
}

/// The cited rules and reported posts, the report's evidence. Each is omitted
/// when empty (inbound reports often carry neither).
fn cited_section(rules: &[rule::Rule], statuses: &[status::Status]) -> Markup {
    html! {
        @if !rules.is_empty() {
            section.admin-report__rules {
                h3 { "Cited rules" }
                ul {
                    @for r in rules {
                        li { (r.text) }
                    }
                }
            }
        }
        @if !statuses.is_empty() {
            section.admin-report__statuses {
                h3 { "Reported posts" }
                @for s in statuses {
                    article.admin-report__status {
                        @if !s.spoiler_text.is_empty() { p.admin-report__cw { (s.spoiler_text) } }
                        div.status__content { (PreEscaped(s.content.clone())) }
                        @if let Some(url) = &s.url {
                            p.admin-report__statuslink { a href=(url) { "View source" } }
                        }
                    }
                }
            }
        }
    }
}

/// The report-note thread plus the add/delete forms (the write surface
/// for the schema-only `report_notes` table).
fn notes_section(
    id: i64,
    csrf: &str,
    notes: &[report_note::ReportNote],
    clock: &ViewerClock,
) -> Markup {
    html! {
        section.admin-notes {
            h3 { "Notes" }
            @if notes.is_empty() {
                p.admin-notes__empty { "No notes yet." }
            }
            ul.admin-notes__list {
                @for note in notes {
                    li.admin-note {
                        p.admin-note__body { (note.content) }
                        footer.admin-note__meta {
                            span { (clock.element_date(note.created_at)) }
                            form method="post" action=(format!("/web/admin/reports/{id}/note/{}/delete", note.id)) {
                                input type="hidden" name="csrf" value=(csrf);
                                button.admin-note__delete type="submit" { "Delete" }
                            }
                        }
                    }
                }
            }
            form.admin-form method="post" action=(format!("/web/admin/reports/{id}/note")) {
                input type="hidden" name="csrf" value=(csrf);
                textarea name="content" rows="2" placeholder="Add a note…" required {}
                button type="submit" { "Add note" }
            }
        }
    }
}

/// `POST /web/admin/reports/{id}/update` — the category/rules edit form.
/// Deserialized as raw pairs because the rule checkboxes repeat the
/// `rule_ids` key a dynamic number of times; the form always renders the full
/// checkbox set, so it is authoritative (no boxes checked clears the list).
pub async fn update(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_REPORTS)?;
    let mut csrf = String::new();
    let mut category = None;
    let mut rule_ids: Vec<i64> = Vec::new();
    for (key, value) in pairs {
        match key.as_str() {
            "csrf" => csrf = value,
            "category" => category = Some(value),
            "rule_ids" => {
                if let Ok(rule_id) = value.parse() {
                    rule_ids.push(rule_id);
                }
            }
            _ => {}
        }
    }
    if !admin.user.csrf_ok(&csrf) {
        return Err(csrf_rejection());
    }
    if !category.as_deref().is_some_and(|c| CATEGORIES.contains(&c)) {
        return Ok(redirect_show(id, "error"));
    }
    match report::update_category(&state.pool, id, category.as_deref(), Some(&rule_ids))
        .await
        .map_err(api_err)?
    {
        Some(report) => {
            crate::webhooks::report_event(&state, plamenu_db::webhook::REPORT_UPDATED, &report)
                .await;
            admin_log::record(
                &state.pool,
                admin.user.current.account.id,
                "update",
                &admin_log::Target::report(id),
            )
            .await
            .map_err(api_err)?;
            Ok(redirect_show(id, "applied"))
        }
        None => Ok(not_found(&admin)),
    }
}

#[derive(Debug, Deserialize)]
pub struct OpForm {
    csrf: String,
    op: String,
}

/// `POST /web/admin/reports/{id}/op` — the assign/unassign/resolve/reopen
/// verbs, mirroring `routes::admin_reports`.
pub async fn op(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<OpForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_REPORTS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let me = admin.user.current.account.id;
    let result = match form.op.as_str() {
        "assign" => report::assign(&state.pool, id, Some(me)).await,
        "unassign" => report::assign(&state.pool, id, None).await,
        "resolve" => report::resolve(&state.pool, id, me).await,
        "reopen" => report::unresolve(&state.pool, id).await,
        _ => return Ok(redirect_show(id, "error")),
    };
    match result.map_err(api_err)? {
        Some(report) => {
            crate::webhooks::report_event(&state, plamenu_db::webhook::REPORT_UPDATED, &report)
                .await;
            // Mastodon's vocabulary: assignment logs as
            // `assigned_to_self`/`unassigned`, the state flips by their verb.
            let verb = match form.op.as_str() {
                "assign" => "assigned_to_self",
                "unassign" => "unassigned",
                other => other,
            };
            admin_log::record(&state.pool, me, verb, &admin_log::Target::report(id))
                .await
                .map_err(api_err)?;
            Ok(redirect_show(id, "applied"))
        }
        None => Ok(not_found(&admin)),
    }
}

#[derive(Debug, Deserialize)]
pub struct NoteForm {
    csrf: String,
    content: String,
}

/// `POST /web/admin/reports/{id}/note` — record a report note.
pub async fn add_note(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<NoteForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_REPORTS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let content = form.content.trim();
    if content.is_empty() {
        return Ok(redirect_show(id, "error"));
    }
    if report::find_by_id(&state.pool, id)
        .await
        .map_err(api_err)?
        .is_none()
    {
        return Ok(not_found(&admin));
    }
    report_note::create(&state.pool, admin.user.current.account.id, id, content)
        .await
        .map_err(api_err)?;
    Ok(redirect_show(id, "applied"))
}

#[derive(Debug, Deserialize)]
pub struct CsrfForm {
    csrf: String,
}

/// `POST /web/admin/reports/{id}/note/{note_id}/delete` — remove a note.
pub async fn delete_note(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path((id, note_id)): Path<(i64, i64)>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_REPORTS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    report_note::delete(&state.pool, note_id)
        .await
        .map_err(api_err)?;
    Ok(redirect_show(id, "applied"))
}

// ---- Helpers -------------------------------------------------------------

/// Resolves an optional account reference to its display handle.
async fn optional_account(
    state: &AppState,
    account_id: Option<i64>,
) -> Result<Option<String>, plamenu_db::DbError> {
    let Some(id) = account_id else {
        return Ok(None);
    };
    Ok(account::find_by_id(&state.pool, id)
        .await?
        .map(|a| handle(&a)))
}

/// A small assign/resolve verb form posting to `…/op`.
fn op_form(id: i64, csrf: &str, op: &str, label: &str) -> Markup {
    html! {
        form method="post" action=(format!("/web/admin/reports/{id}/op")) {
            input type="hidden" name="csrf" value=(csrf);
            input type="hidden" name="op" value=(op);
            button type="submit" { (label) }
        }
    }
}

/// A party's plain handle, or a dash when the account is gone.
fn party(account: Option<&Account>) -> Markup {
    html! { (account.map_or_else(|| "—".to_owned(), handle)) }
}

/// A party's handle linked to its account moderation page (target column).
fn party_link(account: Option<&Account>) -> Markup {
    html! {
        @match account {
            Some(a) => a href=(format!("/admin/accounts/{}", a.id)) { (handle(a)) },
            None => "—",
        }
    }
}

/// Open vs resolved badge.
fn state_badge(report: &Report) -> Markup {
    let (label, class) = if report.action_taken_at.is_some() {
        ("Resolved", "is-active")
    } else {
        ("Open", "is-pending")
    };
    html! { span.admin-badge class=(format!("admin-badge {class}")) { (label) } }
}

fn yes_no(value: bool) -> &'static str {
    if value { "Yes" } else { "No" }
}

fn redirect_show(id: i64, flash: &str) -> Response {
    redirect_to(&format!("/admin/reports/{id}?flash={flash}"))
}

fn redirect_to(path: &str) -> Response {
    (StatusCode::SEE_OTHER, [(header::LOCATION, path.to_owned())]).into_response()
}

fn not_found(admin: &WebAdmin) -> Response {
    let body = html! { p { "That report no longer exists." } };
    (
        StatusCode::NOT_FOUND,
        admin_shell(admin, "/admin/reports", "Not found", &body),
    )
        .into_response()
}

/// Renders a db error as the API's JSON error response.
fn api_err(err: plamenu_db::DbError) -> Response {
    crate::error::ApiError::from(err).into_response()
}
