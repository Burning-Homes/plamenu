//! Custom-role management (Mastodon's `Admin::RolesController`), previously
//! CLI-only. All pages require `MANAGE_ROLES`, with Mastodon's
//! `UserRolePolicy` hierarchy rules on top: a moderator may only edit or
//! delete roles positioned strictly *below* their own (so nobody edits their
//! own role or their superiors'), may not position a role above their own,
//! and may only grant permissions they themselves hold.

use axum::extract::{Form, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use maud::{Markup, html};
use plamenu_db::role::{self, Role, permission};
use serde::Deserialize;

use super::{WebAdmin, admin_shell};
use crate::web::session::csrf_rejection;
use crate::{AppState, admin_log};

/// The submitted badge colour, ready to store: empty (no tint) or a validated
/// hex literal. `None` rejects the submission — see [`crate::web::badge_color`]
/// for why an arbitrary string here is a CSS-injection vector.
fn badge_color_field(submitted: &str) -> Option<&str> {
    let trimmed = submitted.trim();
    if trimmed.is_empty() {
        return Some("");
    }
    crate::web::badge_color(trimmed)
}

/// Every grantable permission bit with its form-field name and display label,
/// in Mastodon's flag order.
const PERMISSIONS: &[(i64, &str, &str)] = &[
    (
        permission::ADMINISTRATOR,
        "administrator",
        "Administrator (grants every permission)",
    ),
    (permission::VIEW_DEVOPS, "view_devops", "View DevOps"),
    (
        permission::VIEW_AUDIT_LOG,
        "view_audit_log",
        "View audit log",
    ),
    (
        permission::VIEW_DASHBOARD,
        "view_dashboard",
        "View dashboard",
    ),
    (
        permission::MANAGE_REPORTS,
        "manage_reports",
        "Manage reports",
    ),
    (
        permission::MANAGE_FEDERATION,
        "manage_federation",
        "Manage federation",
    ),
    (
        permission::MANAGE_SETTINGS,
        "manage_settings",
        "Manage settings",
    ),
    (permission::MANAGE_BLOCKS, "manage_blocks", "Manage blocks"),
    (
        permission::MANAGE_TAXONOMIES,
        "manage_taxonomies",
        "Manage taxonomies",
    ),
    (
        permission::MANAGE_APPEALS,
        "manage_appeals",
        "Manage appeals",
    ),
    (permission::MANAGE_USERS, "manage_users", "Manage users"),
    (
        permission::MANAGE_INVITES,
        "manage_invites",
        "Manage invites",
    ),
    (permission::MANAGE_RULES, "manage_rules", "Manage rules"),
    (
        permission::MANAGE_ANNOUNCEMENTS,
        "manage_announcements",
        "Manage announcements",
    ),
    (
        permission::MANAGE_CUSTOM_EMOJIS,
        "manage_custom_emojis",
        "Manage custom emoji",
    ),
    (
        permission::MANAGE_WEBHOOKS,
        "manage_webhooks",
        "Manage webhooks",
    ),
    (permission::INVITE_USERS, "invite_users", "Invite users"),
    (permission::MANAGE_ROLES, "manage_roles", "Manage roles"),
    (
        permission::MANAGE_USER_ACCESS,
        "manage_user_access",
        "Manage user access",
    ),
    (
        permission::DELETE_USER_DATA,
        "delete_user_data",
        "Delete user data",
    ),
    (permission::MANAGE_GROUPS, "manage_groups", "Manage groups"),
    (
        permission::CREATE_WEBXDC,
        "create_webxdc",
        "Create Webxdc sessions",
    ),
    (
        permission::MANAGE_WEBXDC,
        "manage_webxdc",
        "Manage all Webxdc sessions",
    ),
    (
        permission::UPLOAD_CUSTOM_EMOJIS,
        "upload_custom_emojis",
        "Upload and borrow personal custom emoji",
    ),
];

/// Every permission the acting moderator may grant: everything for
/// administrators, otherwise their own bits (Mastodon's
/// `computed_permissions` elevation guard).
fn grantable(admin: &WebAdmin) -> i64 {
    if admin.role.can(permission::ADMINISTRATOR) {
        PERMISSIONS.iter().fold(0, |bits, (bit, ..)| bits | bit)
    } else {
        admin.role.permissions
    }
}

/// Whether the actor outranks `target` — Mastodon's `Role#overrides?`,
/// required to edit or delete it. Nobody outranks their own role.
fn overrides(admin: &WebAdmin, target: &Role) -> bool {
    admin.role.position > target.position
}

#[derive(Debug, Default, Deserialize)]
pub struct IndexQuery {
    flash: Option<String>,
}

/// `GET /admin/roles` — every role, highest first, with member counts.
pub async fn index(
    State(state): State<AppState>,
    admin: WebAdmin,
    Query(query): Query<IndexQuery>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_ROLES)?;

    let roles = role::list(&state.pool).await.map_err(api_err)?;
    let counts = role::member_counts(&state.pool).await.map_err(api_err)?;
    let members = |role_id: i64| {
        counts
            .iter()
            .find(|(id, _)| *id == role_id)
            .map_or(0, |(_, n)| *n)
    };
    let body = html! {
        (super::flash_banner(query.flash.as_deref(), "That role could not be saved."))
        p { a href="/admin/roles/new" { "Create role" } }
        section.admin-list {
            (crate::web::view::data_table(&html! {
                thead {
                    tr { th scope="col" { "Role" } th scope="col" { "Position" } th scope="col" { "Members" } th scope="col" { "Permissions" } }
                }
                tbody {
                    @for item in &roles {
                        tr {
                            td {
                                @if let Some(color) = crate::web::badge_color(&item.color) {
                                    span.role-swatch style=(format!("background:{color}")) {}
                                    " "
                                }
                                a href=(format!("/admin/roles/{}", item.id)) { (item.name) }
                                @if item.id == role::DEFAULT_ROLE_ID { " " span.admin-badge { "Default" } }
                                @if item.highlighted { " " span.admin-table__sub { "(shown on profile)" } }
                            }
                            td { (item.position) }
                            td { (members(item.id)) }
                            td { (permission_summary(item)) }
                        }
                    }
                }
            }))
        }
    };
    Ok(admin_shell(&admin, "/admin/roles", "Roles", &body).into_response())
}

/// A short readout for the list table: "Administrator" or a bit count.
fn permission_summary(item: &Role) -> String {
    if item.permissions & permission::ADMINISTRATOR != 0 {
        return "Administrator".to_owned();
    }
    let granted = PERMISSIONS
        .iter()
        .filter(|(bit, ..)| item.permissions & bit == *bit)
        .count();
    format!("{granted} granted")
}

/// `GET /admin/roles/new` — the create form.
pub async fn new(admin: WebAdmin) -> Result<Response, Response> {
    admin.require(permission::MANAGE_ROLES)?;
    let body = role_form(&admin, None);
    Ok(admin_shell(&admin, "/admin/roles", "Create role", &body).into_response())
}

/// `GET /admin/roles/{id}` — the edit form (read-only summary when the actor
/// doesn't outrank the role).
pub async fn show(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_ROLES)?;
    let Some(target) = role::find_by_id(&state.pool, id).await.map_err(api_err)? else {
        return Ok(not_found(&admin));
    };
    let body = if overrides(&admin, &target) {
        role_form(&admin, Some(&target))
    } else {
        html! {
            p {
                "This role is positioned at or above your own, so you cannot "
                "change it."
            }
            (role_summary(&target))
        }
    };
    Ok(admin_shell(
        &admin,
        "/admin/roles",
        &format!("Role: {}", target.name),
        &body,
    )
    .into_response())
}

fn role_summary(target: &Role) -> Markup {
    html! {
        ul {
            li { "Position: " (target.position) }
            @for (bit, _, label) in PERMISSIONS {
                @if target.permissions & bit == *bit { li { (label) } }
            }
        }
    }
}

fn role_form(admin: &WebAdmin, target: Option<&Role>) -> Markup {
    let csrf = admin.user.csrf.as_str();
    let grantable = grantable(admin);
    let action = target.map_or_else(
        || "/web/admin/roles".to_owned(),
        |t| format!("/web/admin/roles/{}/update", t.id),
    );
    html! {
        form.admin-form method="post" action=(action) {
            input type="hidden" name="csrf" value=(csrf);
            label {
                "Name"
                input type="text" name="name" value=(target.map_or("", |t| t.name.as_str())) required;
            }
            label {
                "Badge color"
                input type="text" name="color" value=(target.map_or("", |t| t.color.as_str())) placeholder="#ff5050";
            }
            label {
                "Position"
                input type="number" name="position" value=(target.map_or(0, |t| t.position))
                    max=(admin.role.position);
            }
            label.admin-check {
                input type="checkbox" name="highlighted" value="1"
                    checked[target.is_some_and(|t| t.highlighted)];
                span { "Show role badge on profile" }
            }
            fieldset.admin-form__group {
                legend { "Permissions" }
                div.admin-form__checks {
                    @for (bit, field, label) in PERMISSIONS {
                        @let checked = target.is_some_and(|t| t.permissions & bit == *bit);
                        @let allowed = grantable & bit == *bit;
                        label.admin-check {
                            input type="checkbox" name=(field) value="1" checked[checked] disabled[!allowed];
                            span { (label) }
                        }
                    }
                }
            }
            button type="submit" { (if target.is_some() { "Save" } else { "Create role" }) }
        }
        @if let Some(t) = target {
            @if t.id == role::DEFAULT_ROLE_ID {
                p.admin__lead { "This is the default role for new users; it cannot be deleted." }
            } @else {
                form method="post" action=(format!("/web/admin/roles/{}/delete", t.id)) {
                    input type="hidden" name="csrf" value=(csrf);
                    button.admin-danger type="submit" { "Delete role" }
                }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RoleForm {
    csrf: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    color: String,
    #[serde(default)]
    position: i32,
    highlighted: Option<String>,
    administrator: Option<String>,
    view_devops: Option<String>,
    view_audit_log: Option<String>,
    view_dashboard: Option<String>,
    manage_reports: Option<String>,
    manage_federation: Option<String>,
    manage_settings: Option<String>,
    manage_blocks: Option<String>,
    manage_taxonomies: Option<String>,
    manage_appeals: Option<String>,
    manage_users: Option<String>,
    manage_invites: Option<String>,
    manage_rules: Option<String>,
    manage_announcements: Option<String>,
    manage_custom_emojis: Option<String>,
    manage_webhooks: Option<String>,
    invite_users: Option<String>,
    manage_roles: Option<String>,
    manage_user_access: Option<String>,
    delete_user_data: Option<String>,
    manage_groups: Option<String>,
    upload_custom_emojis: Option<String>,
    create_webxdc: Option<String>,
    manage_webxdc: Option<String>,
}

impl RoleForm {
    /// The submitted checkboxes as a bitmask. Bits the actor may not grant
    /// are masked off — a well-behaved browser never submits them (the boxes
    /// render disabled), so this only stops forged requests.
    fn permissions(&self, admin: &WebAdmin) -> i64 {
        let fields = [
            (permission::ADMINISTRATOR, &self.administrator),
            (permission::VIEW_DEVOPS, &self.view_devops),
            (permission::VIEW_AUDIT_LOG, &self.view_audit_log),
            (permission::VIEW_DASHBOARD, &self.view_dashboard),
            (permission::MANAGE_REPORTS, &self.manage_reports),
            (permission::MANAGE_FEDERATION, &self.manage_federation),
            (permission::MANAGE_SETTINGS, &self.manage_settings),
            (permission::MANAGE_BLOCKS, &self.manage_blocks),
            (permission::MANAGE_TAXONOMIES, &self.manage_taxonomies),
            (permission::MANAGE_APPEALS, &self.manage_appeals),
            (permission::MANAGE_USERS, &self.manage_users),
            (permission::MANAGE_INVITES, &self.manage_invites),
            (permission::MANAGE_RULES, &self.manage_rules),
            (permission::MANAGE_ANNOUNCEMENTS, &self.manage_announcements),
            (permission::MANAGE_CUSTOM_EMOJIS, &self.manage_custom_emojis),
            (permission::MANAGE_WEBHOOKS, &self.manage_webhooks),
            (permission::INVITE_USERS, &self.invite_users),
            (permission::MANAGE_ROLES, &self.manage_roles),
            (permission::MANAGE_USER_ACCESS, &self.manage_user_access),
            (permission::DELETE_USER_DATA, &self.delete_user_data),
            (permission::MANAGE_GROUPS, &self.manage_groups),
            (permission::CREATE_WEBXDC, &self.create_webxdc),
            (permission::MANAGE_WEBXDC, &self.manage_webxdc),
            (permission::UPLOAD_CUSTOM_EMOJIS, &self.upload_custom_emojis),
        ];
        let requested = fields
            .iter()
            .filter(|(_, value)| value.is_some())
            .fold(0, |bits, (bit, _)| bits | bit);
        requested & grantable(admin)
    }
}

/// `POST /web/admin/roles` — create a role.
pub async fn create(
    State(state): State<AppState>,
    admin: WebAdmin,
    Form(form): Form<RoleForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_ROLES)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let name = form.name.trim();
    // A blank colour means "no badge tint"; anything else must be a hex
    // literal, since the value is interpolated into a `style` attribute on a
    // public profile.
    let Some(color) = badge_color_field(&form.color) else {
        return Ok(redirect_roles("error"));
    };
    if name.is_empty() || form.position > admin.role.position {
        return Ok(redirect_roles("error"));
    }
    let created = role::create(
        &state.pool,
        name,
        color,
        form.position,
        form.permissions(&admin),
        form.highlighted.is_some(),
    )
    .await
    .map_err(api_err)?;
    log_role(&state, &admin, "create", &created).await?;
    Ok(redirect_roles("applied"))
}

/// `POST /web/admin/roles/{id}/update` — rewrite a role the actor outranks.
pub async fn update(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<RoleForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_ROLES)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let Some(target) = role::find_by_id(&state.pool, id).await.map_err(api_err)? else {
        return Ok(redirect_roles("error"));
    };
    let name = form.name.trim();
    let Some(color) = badge_color_field(&form.color) else {
        return Ok(redirect_roles("error"));
    };
    if !overrides(&admin, &target) || name.is_empty() || form.position > admin.role.position {
        return Ok(redirect_roles("error"));
    }
    let updated = role::update(
        &state.pool,
        id,
        name,
        color,
        form.position,
        form.permissions(&admin),
        form.highlighted.is_some(),
    )
    .await
    .map_err(api_err)?;
    if let Some(role) = &updated {
        log_role(&state, &admin, "update", role).await?;
    }
    Ok(redirect_roles(if updated.is_some() {
        "applied"
    } else {
        "error"
    }))
}

#[derive(Debug, Deserialize)]
pub struct CsrfForm {
    csrf: String,
}

/// `POST /web/admin/roles/{id}/delete` — delete a role the actor outranks;
/// members fall back to no role.
pub async fn delete(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_ROLES)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let Some(target) = role::find_by_id(&state.pool, id).await.map_err(api_err)? else {
        return Ok(redirect_roles("error"));
    };
    if !overrides(&admin, &target) {
        return Ok(redirect_roles("error"));
    }
    // The default role every new user receives is not deletable — removing it
    // would silently degrade sign-ups to no role at all.
    if target.id == role::DEFAULT_ROLE_ID {
        return Ok(redirect_roles("error"));
    }
    let deleted = role::delete(&state.pool, id).await.map_err(api_err)?;
    if deleted {
        log_role(&state, &admin, "destroy", &target).await?;
    }
    Ok(redirect_roles(if deleted { "applied" } else { "error" }))
}

async fn log_role(
    state: &AppState,
    admin: &WebAdmin,
    action: &str,
    role: &Role,
) -> Result<(), Response> {
    admin_log::record(
        &state.pool,
        admin.user.current.account.id,
        action,
        &admin_log::Target::role(role.id, &role.name),
    )
    .await
    .map_err(api_err)
}

fn not_found(admin: &WebAdmin) -> Response {
    let body = html! { p { "That role no longer exists." } };
    (
        StatusCode::NOT_FOUND,
        admin_shell(admin, "/admin/roles", "Not found", &body),
    )
        .into_response()
}

fn redirect_roles(flash: &str) -> Response {
    (
        StatusCode::SEE_OTHER,
        [(header::LOCATION, format!("/admin/roles?flash={flash}"))],
    )
        .into_response()
}

fn api_err(err: plamenu_db::DbError) -> Response {
    crate::error::ApiError::from(err).into_response()
}
