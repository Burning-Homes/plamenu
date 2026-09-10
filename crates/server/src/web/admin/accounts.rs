//! Account moderation pages for the admin dashboard — the browser counterpart
//! to `routes::admin_accounts`. Reads reuse the same `admin_account` view the
//! REST API renders from; the action forms route through the shared
//! [`crate::moderation`] helper, so a suspension applied here federates exactly
//! as one applied through the API. All pages require `MANAGE_USERS`.

use axum::extract::{Form, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use maud::{Markup, html};
use plamenu_db::account::{self, Account};
use plamenu_db::admin_account::{self, AdminAccountFilter, AdminAccountView};
use plamenu_db::role::{self, Role, permission};
use plamenu_db::{account_moderation_note, account_warning, user, warning_preset};
use serde::Deserialize;

use super::super::clock::ViewerClock;
use super::{WebAdmin, admin_shell};
use crate::web::session::csrf_rejection;
use crate::{AppState, admin_log};

/// Page size for the account listing.
const PAGE_LIMIT: i64 = 50;

#[derive(Debug, Default, Deserialize)]
pub struct IndexQuery {
    origin: Option<String>,
    status: Option<String>,
    username: Option<String>,
    domain: Option<String>,
    max_id: Option<i64>,
    flash: Option<String>,
}

/// `GET /admin/accounts` — the moderation listing, filterable by origin,
/// status, username prefix and domain.
#[allow(clippy::too_many_lines)] // one flat template: filter form + table
pub async fn index(
    State(state): State<AppState>,
    admin: WebAdmin,
    Query(query): Query<IndexQuery>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_USERS)?;

    let origin = non_empty(query.origin.as_deref());
    let status = non_empty(query.status.as_deref());
    let domain = non_empty(query.domain.as_deref());
    let filter = AdminAccountFilter {
        origin: origin.clone(),
        status: status.clone(),
        username: non_empty(query.username.as_deref())
            .map(|u| format!("{}%", u.replace('%', "\\%").replace('_', "\\_"))),
        by_domain: domain.clone(),
        max_id: query.max_id,
        limit: PAGE_LIMIT,
        ..AdminAccountFilter::default()
    };
    let views = admin_account::list(&state.pool, &filter)
        .await
        .map_err(api_err)?;
    let next_max = (i64::try_from(views.len()).unwrap_or(i64::MAX) == PAGE_LIMIT)
        .then(|| views.last().map(|v| v.account.id))
        .flatten();

    let username = query.username.clone().unwrap_or_default();
    let body = html! {
        (super::flash_banner(query.flash.as_deref(), "That action could not be completed."))
        form.admin-filter method="get" action="/admin/accounts" {
            label {
                "Origin"
                select name="origin" {
                    option value="" selected[origin.is_none()] { "Any" }
                    option value="local" selected[origin.as_deref() == Some("local")] { "Local" }
                    option value="remote" selected[origin.as_deref() == Some("remote")] { "Remote" }
                }
            }
            label {
                "Status"
                select name="status" {
                    @let opts = ["", "active", "pending", "disabled", "silenced", "suspended", "sensitized"];
                    @for opt in opts {
                        option value=(opt) selected[status.as_deref().unwrap_or("") == opt] {
                            (if opt.is_empty() { "Any" } else { opt })
                        }
                    }
                }
            }
            label {
                "Username"
                input type="text" name="username" value=(username) placeholder="prefix";
            }
            label {
                "Domain"
                input type="text" name="domain" value=(domain.as_deref().unwrap_or_default()) placeholder="exact";
            }
            button type="submit" { "Filter" }
        }
        (crate::web::view::data_table(&html! {
            thead { tr { th scope="col" { "Account" } th scope="col" { "Origin" } th scope="col" { "Status" } th scope="col" { "Role" } } }
            tbody data-paged {
                @if views.is_empty() {
                    tr { td colspan="4" { "No accounts match." } }
                }
                @for view in &views {
                    tr {
                        td {
                            div.admin-table__account {
                                img.admin-table__avatar src=(avatar_src(&state.config.domain, &view.account)) alt="" loading="lazy" width="32" height="32";
                                div {
                                    a href=(format!("/admin/accounts/{}", view.account.id)) {
                                        (account_handle(&view.account, view.portable))
                                    }
                                    @if view.portable {
                                        " " span.admin-badge { "Portable" }
                                    }
                                    @if let Some(kind) = special_kind(&view.account) {
                                        " " span.admin-badge { (kind) }
                                    }
                                    @if !view.account.display_name.is_empty() {
                                        span.admin-table__sub { (view.account.display_name) }
                                    }
                                    span.admin-table__links {
                                        a href=(account_profile_path(&view.account, view.portable)) { "Profile" }
                                        @if let Some(url) = origin_url(&view.account) {
                                            " · "
                                            a href=(url) rel="noopener noreferrer" { "Origin" }
                                        }
                                    }
                                }
                            }
                        }
                        td.is-tight { (if view.portable { "Portable" } else if view.account.is_local() { "Local" } else { "Remote" }) }
                        td.is-tight { (status_badge(view)) }
                        td.is-tight { (view.role.as_ref().map_or("—", |r| r.name.as_str())) }
                    }
                }
            }
        }))
        @if let Some(max) = next_max {
            p.admin-pager {
                a href=(format!("/admin/accounts?max_id={max}{}", carry(origin.as_deref(), status.as_deref(), &username, domain.as_deref()))) {
                    "Older →"
                }
            }
        }
    };
    Ok(admin_shell(&admin, "/admin/accounts", "Accounts", &body).into_response())
}

#[derive(Debug, Default, Deserialize)]
pub struct ShowQuery {
    flash: Option<String>,
    /// Set when arriving from a report's "take action" link: the moderation
    /// action taken here will cite (and resolve) that report.
    #[serde(default, deserialize_with = "super::empty_as_none")]
    report_id: Option<i64>,
}

/// `GET /admin/accounts/{id}` — the full moderation view for one account:
/// identity, state, strikes and moderator notes, plus the action forms.
#[allow(clippy::too_many_lines)] // the account page's one assembly point
pub async fn show(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Query(query): Query<ShowQuery>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_USERS)?;

    let Some(view) = admin_account::show(&state.pool, id)
        .await
        .map_err(api_err)?
    else {
        return Ok(not_found(&admin));
    };
    let warnings = account_warning::for_target(&state.pool, id)
        .await
        .map_err(api_err)?;
    let notes = account_moderation_note::for_target(&state.pool, id)
        .await
        .map_err(api_err)?;
    let presets = warning_preset::list(&state.pool).await.map_err(api_err)?;
    // Only thread a cited report through the action form when it actually
    // exists and names this account (a stale link must not mis-cite).
    let cited_report = match query.report_id {
        Some(report_id) => plamenu_db::report::find_by_id(&state.pool, report_id)
            .await
            .map_err(api_err)?
            .filter(|r| r.target_account_id == id),
        None => None,
    };
    let csrf = admin.user.csrf.as_str();
    let acct = &view.account;
    let display_handle = account_handle(acct, view.portable);
    let local_user = acct.is_local() && view.has_user;
    // Whether this moderator may apply a punitive action to this target at all
    // (not oneself, and outranking the target's role). `Suspend` is a
    // representative gated verb — it shares the self/outrank rule with every
    // other punitive verb and, unlike `Destroy`, is not additionally gated on
    // `DELETE_USER_DATA`. Drives whether the action form is even offered.
    // The last administrator can never be disabled/suspended/deleted, so the
    // whole punitive surface is hidden for it too. Only queries
    // when the target actually holds the administrator permission.
    let protected_last_admin =
        crate::moderation::is_last_active_administrator(&state, acct, view.role.as_ref())
            .await
            .map_err(IntoResponse::into_response)?;
    let can_moderate = !protected_last_admin
        && crate::moderation::authorize_account_action(
            &admin.role,
            admin.user.current.account.id,
            acct.id,
            view.role.as_ref(),
            crate::moderation::ActionKind::Suspend,
        )
        .is_ok();
    let all_roles = if local_user && admin.role.can(permission::MANAGE_ROLES) {
        role::list(&state.pool).await.map_err(api_err)?
    } else {
        Vec::new()
    };

    let body = html! {
        (super::flash_banner(query.flash.as_deref(), "That action could not be completed."))
        p.admin-back { a href="/admin/accounts" { "← Back to accounts" } }
        div.admin-detail {
            div.admin-detail__identity {
                img.admin-detail__avatar src=(avatar_src(&state.config.domain, acct)) alt="" width="48" height="48";
                div {
                    h3 { (&display_handle) }
                    @if !acct.display_name.is_empty() { p.admin-detail__name { (acct.display_name) } }
                }
            }
            dl.admin-detail__grid {
                dt { "Status" } dd { (status_badge(&view)) }
                dt { "Origin" } dd { (if view.portable { "Portable (client-owned identity)" } else if acct.is_local() { "Local" } else { "Remote" }) }
                dt { "Type" } dd { (actor_kind(acct)) }
                @if acct.is_local() {
                    dt { "Approved" } dd { (yes_no(view.approved)) }
                }
                dt { "Role" } dd { (view.role.as_ref().map_or("—", |r| r.name.as_str())) }
                dt { "Created" } dd { (admin.clock().element_date(acct.created_at)) }
                @let path = account_profile_path(acct, view.portable);
                dt { "Profile" } dd { a href=(path.clone()) { (path) } }
                @if let Some(url) = origin_url(acct) {
                    dt { "Origin page" } dd { a href=(url) rel="noopener noreferrer" { (url) } }
                }
            }
        }

        @if local_user {
            section.admin-signup {
                h3 { "Registration" }
                dl.admin-detail__grid {
                    dt { "E-mail" }
                    dd { (view.email.as_deref().unwrap_or("Not provided")) }
                    dt { "Sign-up reason" }
                    dd.admin-signup__reason {
                        (view.invite_request_text.as_deref().unwrap_or("Not provided"))
                    }
                    dt { "Sign-up IP" }
                    dd { (view.sign_up_ip.as_deref().unwrap_or("Unavailable")) }
                    dt { "Application" }
                    dd { (view.sign_up_application.as_deref().unwrap_or("Unavailable")) }
                    dt { "Locale" }
                    dd { (view.locale.as_deref().unwrap_or("Server default")) }
                    dt { "Time zone" }
                    dd { (view.time_zone.as_deref().unwrap_or("Server default")) }
                    @if let Some(invite) = &view.sign_up_invite {
                        dt { "Invite" }
                        dd {
                            code { (&invite.code) }
                            @if let (Some(inviter_id), Some(inviter)) =
                                (invite.inviter_account_id, invite.inviter_username.as_deref())
                            {
                                " from "
                                a href=(format!("/admin/accounts/{inviter_id}")) {
                                    "@" (inviter)
                                }
                            }
                        }
                    }
                    @if let Some(verified_at) = view.age_verified_at {
                        dt { "Minimum age verified" }
                        dd { (admin.clock().element_date(verified_at)) }
                    }
                    dt { "Confirmed" }
                    dd { (yes_no(view.confirmed_at.is_some())) }
                    dt { "Confirmation e-mail" }
                    dd {
                        @if let Some(sent_at) = view.confirmation_sent_at {
                            "Sent " (admin.clock().element_date(sent_at))
                        } @else if view.confirmed_at.is_some() {
                            "Not required"
                        } @else {
                            "Not sent"
                        }
                    }
                    @if let Some(confirmed_at) = view.confirmed_at {
                        dt { "Confirmed at" }
                        dd { (admin.clock().element_date(confirmed_at)) }
                    }
                }
            }
        }

        @if local_user && admin.role.can(permission::MANAGE_CUSTOM_EMOJIS) {
            p.admin-actions {
                a.pill-button href=(format!("/admin/custom-emojis/users/{id}")) {
                    "Moderate this user's custom emoji"
                }
            }
        }

        // ---- Lift / approval verbs (state-dependent) --------------------
        @let lifts = available_lifts(&view, local_user);
        @if !lifts.is_empty() {
            div.admin-actions {
                @for (op, label) in lifts {
                    form method="post" action=(format!("/web/admin/accounts/{id}/op")) {
                        input type="hidden" name="csrf" value=(csrf);
                        input type="hidden" name="op" value=(op);
                        button type="submit" { (label) }
                    }
                }
            }
        }

        // ---- One-off user-access ops -------------------------------------
        @if local_user && admin.role.can(permission::MANAGE_USER_ACCESS) {
            (user_access_section(id, csrf, crate::mailer::enabled(&state)))
        }

        // ---- Role assignment (Mastodon's `UserRolePolicy`: only visible
        // when the moderator outranks the target, offering only roles below
        // the moderator's own) --------------------------------------------
        @if local_user
            && admin.role.can(permission::MANAGE_ROLES)
            && view.role.as_ref().is_none_or(|r| r.position < admin.role.position)
        {
            @let assignable: Vec<&Role> = all_roles
                .iter()
                .filter(|r| r.position < admin.role.position)
                .collect();
            (role_section(id, csrf, &assignable, view.role.as_ref()))
        }

        // ---- Apply a moderation action ----------------------------------
        // Hidden for oneself and for targets the moderator does not outrank —
        // the same accounts the `action` handler now refuses.
        @if can_moderate {
        details.admin-form open {
            summary { "Take action" }
            @if let Some(report) = &cited_report {
                p.admin-report__hint {
                    "Acting on "
                    a href=(format!("/admin/reports/{}", report.id)) { "report #" (report.id) }
                    " — the action will cite it"
                    @if report.action_taken_at.is_none() { " and mark it resolved" }
                    "."
                }
            }
            form method="post" action=(format!("/web/admin/accounts/{id}/action")) {
                input type="hidden" name="csrf" value=(csrf);
                @if let Some(report) = &cited_report {
                    input type="hidden" name="report_id" value=(report.id);
                }
                label {
                    "Action"
                    select name="type" {
                        option value="none" { "Warn (no change)" }
                        @if local_user { option value="disable" { "Disable login" } }
                        option value="sensitive" { "Force-sensitive" }
                        option value="silence" { "Silence (limit)" }
                        option value="suspend" { "Suspend" }
                    }
                }
                @if !presets.is_empty() {
                    label {
                        "Preset"
                        select name="preset" {
                            option value="" { "None" }
                            @for preset in &presets {
                                option value=(preset.id) { (preset.title) }
                            }
                        }
                    }
                }
                label {
                    "Note to record"
                    textarea name="text" rows="3" placeholder="Reason (optional; a chosen preset fills this when left empty)" {}
                }
                button type="submit" { "Apply" }
            }
        }
        }

        (notes_section(id, csrf, &notes, admin.clock()))
        (strikes_section(&warnings, admin.clock()))

        // ---- Hard delete (A2) -------------------------------------------
        // Only offered when the same policy the handler enforces would allow it:
        // never oneself, only outranked targets, only with DELETE_USER_DATA, and
        // never the last administrator.
        @if acct.suspended()
        && acct.suspension_origin.as_deref() == Some("local")
        && !protected_last_admin && crate::moderation::authorize_account_action(
            &admin.role,
            admin.user.current.account.id,
            acct.id,
            view.role.as_ref(),
            crate::moderation::ActionKind::Destroy,
        )
        .is_ok() {
            (danger_section(id, csrf, &display_handle))
        }
    };
    Ok(admin_shell(&admin, "/admin/accounts", &display_handle, &body).into_response())
}

/// The destructive hard-delete form — the web face of
/// `DELETE /api/v1/admin/accounts/{id}`. Guarded by re-typing the handle
/// (checked server-side, so it works without JS) and never offered on the
/// moderator's own account.
fn danger_section(id: i64, csrf: &str, handle: &str) -> Markup {
    html! {
        details.admin-form {
            summary { "Delete account permanently" }
            p.admin-report__hint {
                "This permanently purges the suspended account's data, keeps "
                "its username reserved, and cannot be undone. Local actors "
                "are notified with Delete before the purge."
            }
            form method="post" action=(format!("/web/admin/accounts/{id}/op")) {
                input type="hidden" name="csrf" value=(csrf);
                input type="hidden" name="op" value="destroy";
                label {
                    "Type " (handle) " to confirm"
                    input type="text" name="confirm" placeholder=(handle);
                }
                button.admin-danger type="submit" { "Delete permanently" }
            }
        }
    }
}

/// The one-off user-access ops: reset password, resend confirmation,
/// disable 2FA and the change-email form, all posting to `user_op`. The two
/// mail-dependent verbs only render when SMTP is configured; the generated
/// one-time password (A6) is the mail-less path and always renders.
fn user_access_section(id: i64, csrf: &str, mailer_enabled: bool) -> Markup {
    html! {
        details.admin-form {
            summary { "User access" }
            div.admin-actions {
                @let mail_ops: &[(&str, &str)] = if mailer_enabled {
                    &[
                        ("reset_password", "Reset password"),
                        ("resend_confirmation", "Resend confirmation e-mail"),
                    ]
                } else {
                    &[]
                };
                @for (op, label) in mail_ops.iter().chain(&[("disable_2fa", "Disable two-factor auth")]) {
                    form method="post" action=(format!("/web/admin/accounts/{id}/user-op")) {
                        input type="hidden" name="csrf" value=(csrf);
                        input type="hidden" name="op" value=(op);
                        button type="submit" { (label) }
                    }
                }
            }
            form method="post" action=(format!("/web/admin/accounts/{id}/user-op")) {
                input type="hidden" name="csrf" value=(csrf);
                input type="hidden" name="op" value="set_password";
                button type="submit" { "Generate a new password" }
            }
            p.admin-report__hint {
                @if !mailer_enabled {
                    "E-mail is not configured on this server, so reset links "
                    "cannot be sent. "
                }
                "Generating a password signs the user out everywhere and "
                "shows the new password once, for you to hand over."
            }
            form.admin-form method="post" action=(format!("/web/admin/accounts/{id}/user-op")) {
                input type="hidden" name="csrf" value=(csrf);
                input type="hidden" name="op" value="change_email";
                label {
                    "Change e-mail address"
                    input type="email" name="email" placeholder="new-address@example.com";
                }
                button type="submit" { "Change e-mail" }
            }
        }
    }
}

/// The role-assignment form: a select of the roles the moderator may grant
/// (strictly below their own position), plus "None" to clear.
fn role_section(id: i64, csrf: &str, assignable: &[&Role], current: Option<&Role>) -> Markup {
    let current_id = current.map(|r| r.id);
    html! {
        details.admin-form {
            summary { "Change role" }
            form method="post" action=(format!("/web/admin/accounts/{id}/role")) {
                input type="hidden" name="csrf" value=(csrf);
                label {
                    "Role"
                    select name="role_id" {
                        option value="" selected[current_id.is_none()] { "None" }
                        @for role in assignable {
                            option value=(role.id) selected[current_id == Some(role.id)] {
                                (role.name)
                            }
                        }
                    }
                }
                button type="submit" { "Change role" }
            }
        }
    }
}

/// The moderator-notes block: the chronological list plus the add/delete forms
/// (the write surface for `account_moderation_notes`).
fn notes_section(
    id: i64,
    csrf: &str,
    notes: &[account_moderation_note::ModerationNote],
    clock: &ViewerClock,
) -> Markup {
    html! {
        section.admin-notes {
            h3 { "Moderator notes" }
            @if notes.is_empty() {
                p.admin-notes__empty { "No notes yet." }
            }
            ul.admin-notes__list {
                @for note in notes {
                    li.admin-note {
                        p.admin-note__body { (note.content) }
                        footer.admin-note__meta {
                            span { (clock.element_date(note.created_at)) }
                            form method="post" action=(format!("/web/admin/accounts/{id}/note/{}/delete", note.id)) {
                                input type="hidden" name="csrf" value=(csrf);
                                button.admin-note__delete type="submit" { "Delete" }
                            }
                        }
                    }
                }
            }
            form.admin-form method="post" action=(format!("/web/admin/accounts/{id}/note")) {
                input type="hidden" name="csrf" value=(csrf);
                textarea name="content" rows="2" placeholder="Add a note…" required {}
                button type="submit" { "Add note" }
            }
        }
    }
}

/// The read-only strike-history audit list.
fn strikes_section(warnings: &[account_warning::AccountWarning], clock: &ViewerClock) -> Markup {
    html! {
        section.admin-strikes {
            h3 { "Strike history" }
            @if warnings.is_empty() {
                p { "No recorded actions." }
            }
            ul.admin-strikes__list {
                @for w in warnings {
                    li {
                        strong { (w.action) }
                        " — " (clock.element_date(w.created_at))
                        @if !w.text.is_empty() { p.admin-note__body { (w.text) } }
                    }
                }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ActionForm {
    csrf: String,
    #[serde(rename = "type")]
    action_type: String,
    #[serde(default)]
    text: String,
    /// Warning-preset id; fills `text` when that is left empty.
    #[serde(default)]
    preset: String,
    /// The report this action was reached from (A3); cited on the strike and
    /// resolved by the shared moderation helper.
    #[serde(default)]
    report_id: String,
}

/// `POST /web/admin/accounts/{id}/action` — apply a moderation action through
/// the shared helper (state change + strike + federation).
pub async fn action(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_USERS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    if !crate::moderation::ACTION_TYPES.contains(&form.action_type.as_str()) {
        return Ok(redirect_show(id, "error"));
    }
    let Some(target) = account::find_by_id(&state.pool, id)
        .await
        .map_err(api_err)?
    else {
        return Ok(not_found(&admin));
    };
    // A cited report must exist and target this account, like the REST verb's
    // `Report.find` (a mangled hidden field must not mis-cite).
    let report_id = match form.report_id.trim() {
        "" => None,
        raw => {
            let cited = raw.parse::<i64>().ok();
            let report = match cited {
                Some(report_id) => plamenu_db::report::find_by_id(&state.pool, report_id)
                    .await
                    .map_err(api_err)?
                    .filter(|r| r.target_account_id == id),
                None => None,
            };
            match report {
                Some(report) => Some(report.id),
                None => return Ok(redirect_show(id, "error")),
            }
        }
    };
    // A chosen preset supplies the strike text unless the moderator wrote
    // their own (Mastodon pre-fills the textarea client-side; we resolve at
    // submit time instead).
    let mut text = form.text;
    if text.trim().is_empty()
        && let Ok(preset_id) = form.preset.parse::<i64>()
        && let Some(preset) = warning_preset::find_by_id(&state.pool, preset_id)
            .await
            .map_err(api_err)?
    {
        text = preset.text;
    }
    crate::moderation::apply_account_action(
        &state,
        &admin.role,
        admin.user.current.account.id,
        &target,
        &form.action_type,
        &text,
        report_id,
    )
    .await
    .map_err(IntoResponse::into_response)?;
    // An action reached from a report returns to that report, now resolved.
    match report_id {
        Some(report_id) => Ok(redirect_to(&format!(
            "/admin/reports/{report_id}?flash=applied"
        ))),
        None => Ok(redirect_show(id, "applied")),
    }
}

#[derive(Debug, Deserialize)]
pub struct OpForm {
    csrf: String,
    op: String,
    /// The typed-handle confirmation for `destroy`.
    #[serde(default)]
    confirm: String,
}

/// Shared outrank/self/`DELETE_USER_DATA` gate for the web `op` destructive
/// verbs (`reject`, `destroy`), mirroring the REST endpoints. Returns a `403`
/// response on denial. The non-destructive lifts do not use it.
async fn authorize_op(
    state: &AppState,
    admin: &WebAdmin,
    target: &Account,
    kind: crate::moderation::ActionKind,
) -> Result<(), Response> {
    let role = crate::moderation::target_role(state, target)
        .await
        .map_err(IntoResponse::into_response)?;
    crate::moderation::authorize_account_action(
        &admin.role,
        admin.user.current.account.id,
        target.id,
        role.as_ref(),
        kind,
    )
    .map_err(IntoResponse::into_response)?;
    // Never remove the last administrator.
    crate::moderation::guard_last_administrator(state, target, role.as_ref(), kind)
        .await
        .map_err(IntoResponse::into_response)
}

/// `POST /web/admin/accounts/{id}/op` — the state-lift and approval verbs
/// (unsuspend/unsilence/unsensitive/enable/approve/reject), mirroring the REST
/// action verbs, plus the confirm-guarded `destroy` (A2).
#[allow(clippy::too_many_lines)] // one flat dispatch over the account verbs
pub async fn op(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<OpForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_USERS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let Some(target) = account::find_by_id(&state.pool, id)
        .await
        .map_err(api_err)?
    else {
        return Ok(not_found(&admin));
    };
    let local = target.is_local();
    let moderator = admin.user.current.account.id;

    match form.op.as_str() {
        "unsuspend" => {
            crate::moderation::unsuspend_account(&state, &target)
                .await
                .map_err(IntoResponse::into_response)?;
        }
        "unsilence" => {
            account::unsilence(&state.pool, id).await.map_err(api_err)?;
        }
        "unsensitive" => {
            account::unsensitize(&state.pool, id)
                .await
                .map_err(api_err)?;
        }
        "enable" if local => {
            user::set_disabled(&state.pool, id, false)
                .await
                .map_err(api_err)?;
        }
        "approve" if local => {
            if user::approve(&state.pool, id).await.map_err(api_err)?
                && let Some(user) = user::find_by_account_id(&state.pool, id)
                    .await
                    .map_err(api_err)?
                && user.confirmed()
            {
                crate::registration::user_became_functional(&state, &user)
                    .await
                    .map_err(IntoResponse::into_response)?;
            }
        }
        "reject" if local => {
            authorize_op(
                &state,
                &admin,
                &target,
                crate::moderation::ActionKind::Reject,
            )
            .await?;
            // Deletion and audit line commit together (finding #49).
            admin_log::record_account_deletion(
                &state.pool,
                id,
                moderator,
                "reject",
                &admin_log::Target::user(&target),
            )
            .await
            .map_err(api_err)?;
            // The account is gone — return to the listing rather than its page.
            return Ok(redirect_to("/admin/accounts?flash=applied"));
        }
        // The web face of `DELETE /api/v1/admin/accounts/{id}`: permanent,
        // any account, guarded by re-typing the handle. The shared policy
        // enforces "never yourself", outranking the target, and
        // `DELETE_USER_DATA`.
        "destroy" => {
            if form.confirm.trim() != handle(&target) {
                return Ok(redirect_show(id, "error"));
            }
            authorize_op(
                &state,
                &admin,
                &target,
                crate::moderation::ActionKind::Destroy,
            )
            .await?;
            crate::moderation::destroy_suspended_account(&state, moderator, &target)
                .await
                .map_err(IntoResponse::into_response)?;
            return Ok(redirect_to("/admin/accounts?flash=applied"));
        }
        _ => return Ok(redirect_show(id, "error")),
    }
    let log_target = match form.op.as_str() {
        "enable" | "approve" => admin_log::Target::user(&target),
        _ => admin_log::Target::account(&target),
    };
    admin_log::record(&state.pool, moderator, &form.op, &log_target)
        .await
        .map_err(api_err)?;
    Ok(redirect_show(id, "applied"))
}

#[derive(Debug, Deserialize)]
pub struct RoleForm {
    csrf: String,
    #[serde(default)]
    role_id: String,
}

/// `POST /web/admin/accounts/{id}/role` — assign (or clear) the target user's
/// role. Mastodon's `UserRolePolicy`: requires `MANAGE_ROLES`, the moderator
/// must outrank the target's current role, and may only grant roles strictly
/// below their own position.
pub async fn set_role(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<RoleForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_ROLES)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let Some(target) = account::find_by_id(&state.pool, id)
        .await
        .map_err(api_err)?
    else {
        return Ok(not_found(&admin));
    };
    let Some(target_user) = user::find_by_account_id(&state.pool, id)
        .await
        .map_err(api_err)?
    else {
        return Ok(redirect_show(id, "error"));
    };
    if let Some(current) = role::for_user(&state.pool, target_user.id)
        .await
        .map_err(api_err)?
        && current.position >= admin.role.position
    {
        return Ok(redirect_show(id, "error"));
    }
    let new_role_id = match form.role_id.trim() {
        "" => None,
        raw => {
            let Ok(role_id) = raw.parse::<i64>() else {
                return Ok(redirect_show(id, "error"));
            };
            let Some(role) = role::find_by_id(&state.pool, role_id)
                .await
                .map_err(api_err)?
            else {
                return Ok(redirect_show(id, "error"));
            };
            if role.position >= admin.role.position {
                return Ok(redirect_show(id, "error"));
            }
            Some(role.id)
        }
    };
    if !role::assign_to_account(&state.pool, id, new_role_id)
        .await
        .map_err(api_err)?
    {
        return Ok(redirect_show(id, "error"));
    }
    admin_log::record(
        &state.pool,
        admin.user.current.account.id,
        "change_role",
        &admin_log::Target::user(&target),
    )
    .await
    .map_err(api_err)?;
    Ok(redirect_show(id, "applied"))
}

#[derive(Debug, Deserialize)]
pub struct UserOpForm {
    csrf: String,
    op: String,
    #[serde(default)]
    email: String,
}

/// `POST /web/admin/accounts/{id}/user-op` — the one-off user-access verbs
/// (Mastodon's `Admin::ResetsController`, `ConfirmationsController#resend`,
/// `Users::TwoFactorAuthenticationsController` and
/// `ChangeEmailsController`). Gated on `MANAGE_USER_ACCESS` plus the role
/// hierarchy: nobody operates on a login whose role is at or above their own.
pub async fn user_op(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<UserOpForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_USER_ACCESS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let Some(target) = account::find_by_id(&state.pool, id)
        .await
        .map_err(api_err)?
    else {
        return Ok(not_found(&admin));
    };
    let Some(target_user) = user::find_by_account_id(&state.pool, id)
        .await
        .map_err(api_err)?
    else {
        return Ok(redirect_show(id, "error"));
    };
    // Mastodon's `role.overrides?(record.role)`.
    if let Some(target_role) = plamenu_db::role::for_user(&state.pool, target_user.id)
        .await
        .map_err(api_err)?
        && target_role.position >= admin.role.position
    {
        return Ok(redirect_show(id, "error"));
    }
    let moderator = admin.user.current.account.id;

    let verb = match form.op.as_str() {
        // Mastodon's `User#reset_password!` (see [`op_reset_password`]).
        "reset_password" => {
            if !op_reset_password(&state, &target_user).await? {
                return Ok(redirect_show(id, "error"));
            }
            "reset_password"
        }
        // Mastodon's `ConfirmationsController#resend` (log verb `resend`).
        "resend_confirmation" => {
            if !op_resend_confirmation(&state, &target, &target_user).await? {
                return Ok(redirect_show(id, "error"));
            }
            "resend"
        }
        // Clears TOTP, backup codes and security keys in one go.
        "disable_2fa" => {
            user::disable_otp(&state.pool, target_user.id)
                .await
                .map_err(api_err)?;
            "disable_2fa"
        }
        // The SMTP-less credential path (A6): set a generated password,
        // revoke every session/token, and show the password once. Unlike
        // `reset_password` this needs no deliverable address.
        "set_password" => {
            let password = generate_one_time_password();
            let hashed = crate::auth::hash_password_gated(password.clone())
                .await
                .map_err(IntoResponse::into_response)?;
            // Set the credential, revoke every session/token, and record the
            // action in one transaction, then show the generated
            // password. A split sequence could change the password while leaving
            // a stolen token live, or reveal (or fail to reveal) the one-time
            // secret out of step with the change; here the password is displayed
            // only after the whole transition commits.
            let log_target = admin_log::Target::user(&target);
            user::set_credentials_and_revoke(
                &state.pool,
                id,
                None,
                &hashed,
                Some(admin_log::new_line(moderator, "set_password", &log_target)),
            )
            .await
            .map_err(api_err)?;
            // Render (not redirect): the password may never appear in a URL.
            return Ok(one_time_password_page(
                &admin,
                id,
                &handle(&target),
                &password,
            ));
        }
        // Immediate address change (Plamenu's e-mail handling is
        // confirmation-less for changes, like the self-service page); an
        // empty field removes the address.
        "change_email" => {
            let email = form.email.trim();
            let email = (!email.is_empty()).then_some(email);
            match user::update_email(&state.pool, target_user.id, email).await {
                Ok(Some(_)) => {}
                Ok(None) => return Ok(redirect_show(id, "error")),
                Err(plamenu_db::DbError::EmailTaken) => {
                    return Ok(redirect_show(id, "error"));
                }
                Err(other) => return Err(api_err(other)),
            }
            "change_email"
        }
        _ => return Ok(redirect_show(id, "error")),
    };
    admin_log::record(
        &state.pool,
        moderator,
        verb,
        &admin_log::Target::user(&target),
    )
    .await
    .map_err(api_err)?;
    Ok(redirect_show(id, "applied"))
}

/// A generated one-time password for the mail-less credential path: 20
/// URL-safe base64 characters (~119 bits), short enough to read out.
fn generate_one_time_password() -> String {
    let mut secret = crate::auth::generate_secret();
    secret.truncate(20);
    secret
}

/// The page showing a freshly generated password exactly once.
fn one_time_password_page(admin: &WebAdmin, id: i64, handle: &str, password: &str) -> Response {
    let body = html! {
        p.admin-back {
            a href=(format!("/admin/accounts/{id}")) { "← Back to " (handle) }
        }
        p {
            "A new password has been set for " strong { (handle) } ". Every "
            "session and app token has been signed out. Copy it now — it is "
            "shown only this once:"
        }
        p { code { (password) } }
    };
    admin_shell(admin, "/admin/accounts", "New password", &body).into_response()
}

/// Scrambles the password (revoking every session and token), then sends
/// reset instructions — Mastodon's `User#reset_password!`. Refused (`false`)
/// without a deliverable address, so the user isn't locked out for good.
async fn op_reset_password(state: &AppState, target_user: &user::User) -> Result<bool, Response> {
    if !crate::mailer::enabled(state) {
        return Ok(false);
    }
    let Some(recipient) = target_user.email.clone() else {
        return Ok(false);
    };
    let scrambled = crate::auth::hash_password_gated(crate::auth::generate_secret())
        .await
        .map_err(IntoResponse::into_response)?;
    let token = crate::auth::generate_secret();
    // Render the mail before the transaction so no work happens across the
    // commit, then scramble the password, revoke every session/token, store the
    // fresh reset token, and durably queue the mail as one unit.
    // A split four-step sequence could leave the target locked out — old password
    // gone, tokens dead — while the mail delivering the way back in never
    // commits; the atomic transition makes that impossible.
    // The mail is read by the account owner, so it is written in *their*
    // stored locale, not the moderator's.
    let locale = crate::web::i18n::Locale::for_user(&state.pool, target_user.id)
        .await
        .map_err(api_err)?;
    let (subject, body) = crate::web::password::render_reset_email(state, &token, locale)
        .await
        .map_err(IntoResponse::into_response)?;
    user::admin_reset_password(
        &state.pool,
        target_user.id,
        &scrambled,
        &crate::auth::hash_secret(&token),
        &plamenu_db::email::OutgoingEmail {
            recipient: &recipient,
            subject: &subject,
            body: &body,
        },
    )
    .await
    .map_err(api_err)?;
    Ok(true)
}

/// Mints a fresh confirmation token and re-sends the instructions. Refused
/// (`false`) for already-confirmed or address-less logins.
async fn op_resend_confirmation(
    state: &AppState,
    target: &Account,
    target_user: &user::User,
) -> Result<bool, Response> {
    if target_user.confirmed() || !crate::mailer::enabled(state) {
        return Ok(false);
    }
    let Some(recipient) = target_user.email.as_deref() else {
        return Ok(false);
    };
    let token = crate::auth::generate_secret();
    let settings = plamenu_db::instance_settings::get(&state.pool)
        .await
        .map_err(api_err)?;
    // Read by the applicant, so it is written in their locale, not the
    // moderator's.
    let locale = crate::web::i18n::Locale::for_user(&state.pool, target_user.id)
        .await
        .map_err(api_err)?;
    let (subject, body) = crate::registration::render_confirmation_email(
        state,
        &settings.site_title,
        target,
        &token,
        locale,
    );
    // Rotate the confirmation token and enqueue the mail atomically: a failed
    // enqueue leaves the previous link valid.
    let refreshed = user::refresh_confirmation_token_with_mail(
        &state.pool,
        target_user.id,
        &crate::auth::hash_secret(&token),
        None,
        &plamenu_db::email::OutgoingEmail {
            recipient,
            subject: &subject,
            body: &body,
        },
    )
    .await
    .map_err(api_err)?;
    Ok(refreshed.is_some())
}

#[derive(Debug, Deserialize)]
pub struct NoteForm {
    csrf: String,
    content: String,
}

/// `POST /web/admin/accounts/{id}/note` — record a moderator note (the
/// write surface for the schema-only `account_moderation_notes` table).
pub async fn add_note(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<NoteForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_USERS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let content = form.content.trim();
    if content.is_empty() {
        return Ok(redirect_show(id, "error"));
    }
    if account::find_by_id(&state.pool, id)
        .await
        .map_err(api_err)?
        .is_none()
    {
        return Ok(not_found(&admin));
    }
    account_moderation_note::create(&state.pool, admin.user.current.account.id, id, content)
        .await
        .map_err(api_err)?;
    Ok(redirect_show(id, "applied"))
}

#[derive(Debug, Deserialize)]
pub struct CsrfForm {
    csrf: String,
}

/// `POST /web/admin/accounts/{id}/note/{note_id}/delete` — remove a note.
pub async fn delete_note(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path((id, note_id)): Path<(i64, i64)>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_USERS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    account_moderation_note::delete(&state.pool, note_id)
        .await
        .map_err(api_err)?;
    Ok(redirect_show(id, "applied"))
}

// ---- Helpers -------------------------------------------------------------

/// The lift/approval verbs applicable to an account's current state, as
/// `(op, button label)` pairs.
fn available_lifts(view: &AdminAccountView, local_user: bool) -> Vec<(&'static str, &'static str)> {
    let mut lifts = Vec::new();
    if view.account.suspended() {
        lifts.push(("unsuspend", "Unsuspend"));
    }
    if view.account.silenced() {
        lifts.push(("unsilence", "Unsilence"));
    }
    if view.account.sensitized() {
        lifts.push(("unsensitive", "Remove sensitive"));
    }
    if local_user && view.disabled {
        lifts.push(("enable", "Enable login"));
    }
    if local_user && !view.approved {
        lifts.push(("approve", "Approve"));
        lifts.push(("reject", "Reject"));
    }
    lifts
}

/// The display handle: `@user` for local accounts, `@user@domain` for remote.
pub(super) fn handle(account: &Account) -> String {
    account_handle(account, false)
}

fn account_handle(account: &Account, portable: bool) -> String {
    if portable {
        return format!("@{}", account.username);
    }
    match &account.domain {
        Some(domain) => format!("@{}@{}", account.username, domain),
        None => format!("@{}", account.username),
    }
}

/// The account's avatar for the moderation views: the same cached-or-proxied
/// URL the client entities emit, or the placeholder when the account has none.
pub(super) fn avatar_src(domain: &str, account: &Account) -> String {
    crate::entities::avatar_url(domain, account, false)
        .unwrap_or_else(|| format!("https://{domain}/static/missing.png"))
}

/// The in-app profile path, including the portable account's local handle.
fn account_profile_path(account: &Account, portable: bool) -> String {
    let sigil = if account.is_group() { "!" } else { "@" };
    if portable {
        return format!("/{sigil}{}", account.username);
    }
    match &account.domain {
        Some(domain) => format!("/{sigil}{}@{}", account.username, domain),
        None => format!("/{sigil}{}", account.username),
    }
}

/// A remote account's own profile page at its origin; `None` for local
/// accounts.
pub(super) fn origin_url(account: &Account) -> Option<&str> {
    if account.is_local() {
        return None;
    }
    account.url.as_deref().or(account.uri.as_deref())
}

/// The human label for what kind of actor this is (Mastodon's entity flags
/// spelled out): `Group` actors, automated accounts (`Service`/`Application`
/// or the local bot flag), `Organization`, else `Person`.
pub(super) fn actor_kind(account: &Account) -> &'static str {
    if account.is_group() {
        "Group"
    } else if account.is_bot
        || matches!(
            account.actor_type.as_deref(),
            Some("Service" | "Application")
        )
    {
        "Bot"
    } else if account.actor_type.as_deref() == Some("Organization") {
        "Organization"
    } else {
        "Person"
    }
}

/// The listing badge for non-person actors; `None` keeps plain people
/// unadorned.
pub(super) fn special_kind(account: &Account) -> Option<&'static str> {
    match actor_kind(account) {
        "Person" => None,
        kind => Some(kind),
    }
}

/// A short status label mirroring Mastodon's account-state precedence.
fn status_badge(view: &AdminAccountView) -> Markup {
    let (label, class) = if view.account.suspended() {
        ("Suspended", "is-suspended")
    } else if view.disabled {
        ("Disabled", "is-disabled")
    } else if view.account.is_local() && view.has_user && !view.approved {
        ("Pending", "is-pending")
    } else if view.account.silenced() {
        ("Limited", "is-silenced")
    } else {
        ("Active", "is-active")
    };
    html! { span.admin-badge class=(format!("admin-badge {class}")) { (label) } }
}

fn yes_no(value: bool) -> &'static str {
    if value { "Yes" } else { "No" }
}

/// Empties a blank query value to `None`.
fn non_empty(value: Option<&str>) -> Option<String> {
    value.filter(|v| !v.is_empty()).map(ToOwned::to_owned)
}

/// Re-appends the active list filters to a pager link.
fn carry(
    origin: Option<&str>,
    status: Option<&str>,
    username: &str,
    domain: Option<&str>,
) -> String {
    use std::fmt::Write;
    let mut q = String::new();
    if let Some(o) = origin {
        let _ = write!(q, "&origin={o}");
    }
    if let Some(s) = status {
        let _ = write!(q, "&status={s}");
    }
    if !username.is_empty() {
        let _ = write!(q, "&username={username}");
    }
    if let Some(d) = domain {
        let _ = write!(q, "&domain={d}");
    }
    q
}

fn redirect_show(id: i64, flash: &str) -> Response {
    redirect_to(&format!("/admin/accounts/{id}?flash={flash}"))
}

fn redirect_to(path: &str) -> Response {
    (StatusCode::SEE_OTHER, [(header::LOCATION, path.to_owned())]).into_response()
}

fn not_found(admin: &WebAdmin) -> Response {
    let body = html! { p { "That account no longer exists." } };
    (
        StatusCode::NOT_FOUND,
        admin_shell(admin, "/admin/accounts", "Not found", &body),
    )
        .into_response()
}

/// Renders a db error as the API's JSON error response.
fn api_err(err: plamenu_db::DbError) -> Response {
    crate::error::ApiError::from(err).into_response()
}
