//! Instance groups console — the staff surface over locally hosted
//! groups, gated on `MANAGE_GROUPS`. Mastodon has no groups, so this is a
//! Plamenu-native admin section rather than a port: list/search every local
//! group (including suspended and deleted ones), then per group edit its
//! settings, transfer ownership, suspend/unsuspend or delete it — each action
//! federated by the shared `crate::groups` service and recorded in the M34
//! audit log against a `Target::group`.

use axum::extract::{Form, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use maud::{Markup, html};
use plamenu_db::account::{self, Account};
use plamenu_db::group::{self, Group, MembershipPolicy, PostingPolicy};
use plamenu_db::role::permission;
use serde::Deserialize;

use super::{WebAdmin, admin_shell};
use crate::web::pages::resolve_handle;
use crate::web::session::csrf_rejection;
use crate::{AppState, admin_log};

const PAGE_LIMIT: i64 = 40;

#[derive(Debug, Default, Deserialize)]
pub struct IndexQuery {
    #[serde(default, deserialize_with = "super::empty_as_none")]
    search: Option<String>,
    #[serde(default, deserialize_with = "super::empty_as_none")]
    max_id: Option<i64>,
    flash: Option<String>,
}

/// `GET /admin/groups` — the searchable list of local groups.
pub async fn index(
    State(state): State<AppState>,
    admin: WebAdmin,
    Query(query): Query<IndexQuery>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_GROUPS)?;

    let rows = group::admin_list(
        &state.pool,
        query.search.as_deref(),
        query.max_id,
        PAGE_LIMIT,
    )
    .await
    .map_err(api_err)?;
    let next_max = (i64::try_from(rows.len()).unwrap_or(i64::MAX) == PAGE_LIMIT)
        .then(|| rows.last().map(|r| r.account_id))
        .flatten();
    let search = query.search.clone().unwrap_or_default();
    let carry = if search.is_empty() {
        String::new()
    } else {
        format!("&search={}", urlencode(&search))
    };

    let body = html! {
        (super::flash_banner(query.flash.as_deref(), "That action could not be completed."))
        p.admin-table__sub {
            "Groups (Lemmy-style communities) hosted on this instance. Suspended and "
            "deleted groups stay listed here so they can be acted on."
        }
        form.admin-filter method="get" action="/admin/groups" {
            label {
                "Name"
                input type="text" name="search" value=(search) placeholder="name or handle";
            }
            button type="submit" { "Search" }
        }
        (crate::web::view::data_table(&html! {
            thead { tr {
                th scope="col" { "Group" }
                th scope="col" { "Members" }
                th scope="col" { "Membership" }
                th scope="col" { "Status" }
                th scope="col" { "Created" }
            } }
            tbody data-paged {
                @if rows.is_empty() {
                    tr { td colspan="5" { "No groups match." } }
                }
                @for row in &rows {
                    tr {
                        td {
                            a href=(format!("/admin/groups/{}", row.account_id)) {
                                (format!("@{}", row.username))
                            }
                            @if !row.display_name.is_empty() {
                                span.admin-table__sub { (row.display_name) }
                            }
                        }
                        td { (row.member_count) }
                        td { (policy_label(&row.membership_policy)) }
                        td { (row_status_badge(row.suspended, row.deleted)) }
                        td { (admin.clock().element_date(row.created_at)) }
                    }
                }
            }
        }))
        @if let Some(max) = next_max {
            p.admin-pager {
                a href=(format!("/admin/groups?max_id={max}{carry}")) { "Older →" }
            }
        }
    };
    Ok(admin_shell(&admin, "/admin/groups", "Groups", &body).into_response())
}

#[derive(Debug, Default, Deserialize)]
pub struct ShowQuery {
    flash: Option<String>,
}

/// `GET /admin/groups/{id}` — one group's admin detail: identity, ownership,
/// stats, and the lifecycle action forms.
#[allow(clippy::too_many_lines)] // the group's one detail-page assembly point
pub async fn show(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Query(query): Query<ShowQuery>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_GROUPS)?;

    let Some((account, group)) = load_group(&state, id).await? else {
        return Ok(not_found(&admin));
    };
    let elevated = group::elevated(&state.pool, id).await.map_err(api_err)?;
    let owner_id = elevated
        .iter()
        .find(|e| e.affiliation == "owner")
        .map(|e| e.account_id);
    let owner = match owner_id {
        Some(oid) => account::find_by_id(&state.pool, oid)
            .await
            .map_err(api_err)?,
        None => None,
    };
    let mod_count = elevated
        .iter()
        .filter(|e| e.affiliation == "moderator")
        .count();
    let members = group::member_count(&state.pool, id)
        .await
        .map_err(api_err)?;
    let suspended = account.suspended();
    let deleted = crate::moderation::permanently_unavailable(&state.pool, &account)
        .await
        .map_err(IntoResponse::into_response)?;
    let csrf = admin.user.csrf.as_str();

    let body = html! {
        (super::flash_banner(query.flash.as_deref(), "That action could not be completed."))
        p.admin-back { a href="/admin/groups" { "← Back to groups" } }
        div.admin-detail {
            h3 { (format!("@{}", account.username)) }
            @if !account.display_name.is_empty() { p.admin-detail__name { (account.display_name) } }
            dl.admin-detail__grid {
                dt { "Status" } dd { (row_status_badge(suspended, deleted)) }
                dt { "Owner" } dd {
                    @match &owner {
                        Some(o) => a href=(format!("/admin/accounts/{}", o.id)) { (format!("@{}", o.username)) },
                        None => "—",
                    }
                }
                dt { "Moderators" } dd { (mod_count) }
                dt { "Members" } dd { (members) }
                dt { "Membership" } dd { (policy_label(&group.membership_policy)) }
                dt { "Who can post" } dd { (posting_policy_label(&group.posting_policy)) }
                dt { "Sensitive" } dd { (yes_no(group.sensitive)) }
                dt { "Created" } dd { (admin.clock().element_date(group.created_at)) }
                dt { "Profile" } dd { a href=(format!("/@{}", account.username)) { (format!("/@{}", account.username)) } }
            }
            p.admin-detail__name {
                a href="/admin/audit-log?target_type=Group" { "View group audit trail →" }
            }
        }

        @if !deleted {
            // ---- Edit settings --------------------------------------------
            details.admin-form open {
                summary { "Edit settings" }
                form method="post" action=(format!("/web/admin/groups/{id}/update")) {
                    input type="hidden" name="csrf" value=(csrf);
                    label {
                        "Display name"
                        input type="text" name="display_name" maxlength="30"
                            value=(account.display_name);
                    }
                    label {
                        "Description"
                        textarea name="note" rows="3" { (account.note_source) }
                    }
                    label {
                        "Membership"
                        select name="membership_policy" {
                            @let policy = group.membership_policy();
                            option value="open" selected[policy == MembershipPolicy::Open] { "Open (anyone can join)" }
                            option value="approval" selected[policy == MembershipPolicy::Approval] { "Approval required" }
                        }
                    }
                    label {
                        "Who can post"
                        select name="posting_policy" {
                            @let posting = group.posting_policy();
                            option value="anyone" selected[posting == PostingPolicy::Anyone] { "Anyone" }
                            option value="members" selected[posting == PostingPolicy::Members] { "Members only" }
                            option value="mods" selected[posting == PostingPolicy::Mods] { "Moderators only" }
                        }
                    }
                    label.admin-check {
                        input type="checkbox" name="sensitive" value="1" checked[group.sensitive];
                        span { "Mark all posts sensitive" }
                    }
                    label.admin-check {
                        input type="checkbox" name="discoverable" value="1"
                            checked[account.discoverable.unwrap_or(true)];
                        span { "List in the groups directory" }
                    }
                    button type="submit" { "Save settings" }
                }
            }

            // ---- Transfer ownership ---------------------------------------
            details.admin-form {
                summary { "Transfer ownership" }
                form method="post" action=(format!("/web/admin/groups/{id}/transfer")) {
                    input type="hidden" name="csrf" value=(csrf);
                    p.admin-table__sub {
                        "The new owner must be a local member. The current owner is "
                        "demoted to moderator."
                    }
                    label {
                        "New owner's handle"
                        input type="text" name="handle" placeholder="@user" autocomplete="off";
                    }
                    button type="submit" { "Transfer" }
                }
            }
        }

        // ---- Lifecycle ops ------------------------------------------------
        div.admin-actions {
            @if !deleted {
                @if suspended {
                    (op_form(id, "unsuspend", "Unsuspend", csrf, false))
                } @else {
                    (op_form(id, "suspend", "Suspend", csrf, false))
                }
                (op_form(id, "delete", "Delete group", csrf, true))
            }
        }
    };
    Ok(admin_shell(
        &admin,
        "/admin/groups",
        &format!("@{}", account.username),
        &body,
    )
    .into_response())
}

/// A single lifecycle-op button posting to `/web/admin/groups/{id}/op`.
fn op_form(id: i64, op: &str, label: &str, csrf: &str, danger: bool) -> Markup {
    html! {
        form method="post" action=(format!("/web/admin/groups/{id}/op")) {
            input type="hidden" name="csrf" value=(csrf);
            input type="hidden" name="op" value=(op);
            @if danger {
                button.admin-danger type="submit" { (label) }
            } @else {
                button type="submit" { (label) }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct UpdateForm {
    csrf: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    note: String,
    #[serde(default)]
    membership_policy: String,
    #[serde(default)]
    posting_policy: String,
    #[serde(default)]
    sensitive: Option<String>,
    #[serde(default)]
    discoverable: Option<String>,
}

/// `POST /web/admin/groups/{id}/update` — edit a group's profile & settings.
pub async fn update(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<UpdateForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_GROUPS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let Some((account, _)) = load_group(&state, id).await? else {
        return Ok(not_found(&admin));
    };
    let display_name = form.display_name.trim().to_owned();
    if display_name.chars().count() > 30 {
        return Ok(redirect_show(id, "error"));
    }
    let note = form.note.trim().to_owned();
    let rendered = crate::compose::compose(&state, &note, crate::compose::PostFormat::Plain)
        .await
        .map_err(IntoResponse::into_response)?
        .html;
    let settings = crate::groups::GroupSettings {
        display_name: &display_name,
        note_html: &rendered,
        note_source: &note,
        policy: MembershipPolicy::parse(&form.membership_policy),
        sensitive: form.sensitive.is_some(),
        posting_policy: PostingPolicy::parse(&form.posting_policy),
        discoverable: form.discoverable.is_some(),
        // Instance staff edit a group's settings, not its avatar or links —
        // those belong to the people running the community, in the group
        // console. The default leaves every one of them untouched.
        profile: crate::groups::GroupProfileEdit::default(),
    };
    crate::groups::update_settings(&state, &account, settings)
        .await
        .map_err(IntoResponse::into_response)?;
    log_group(&state, &admin, "update", &account).await?;
    Ok(redirect_show(id, "applied"))
}

#[derive(Debug, Deserialize)]
pub struct TransferForm {
    csrf: String,
    #[serde(default)]
    handle: String,
}

/// `POST /web/admin/groups/{id}/transfer` — reassign ownership to a local member.
pub async fn transfer(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<TransferForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_GROUPS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let Some((account, _)) = load_group(&state, id).await? else {
        return Ok(not_found(&admin));
    };
    let target = match resolve_handle(&state, form.handle.trim()).await {
        Ok(Some(target)) => target,
        Ok(None) => return Ok(redirect_show(id, "error")),
        Err(err) => return Err(err.into_response()),
    };
    if crate::groups::transfer_owner(&state, &account, &target)
        .await
        .is_err()
    {
        return Ok(redirect_show(id, "error"));
    }
    log_group(&state, &admin, "transfer_owner", &account).await?;
    Ok(redirect_show(id, "applied"))
}

#[derive(Debug, Deserialize)]
pub struct OpForm {
    csrf: String,
    op: String,
}

/// `POST /web/admin/groups/{id}/op` — suspend, unsuspend or delete a group.
pub async fn op(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<OpForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_GROUPS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let Some((account, _)) = load_group(&state, id).await? else {
        return Ok(not_found(&admin));
    };
    let action = match form.op.as_str() {
        "suspend" => {
            let mut tx = state
                .pool
                .begin()
                .await
                .map_err(plamenu_db::DbError::from)
                .map_err(api_err)?;
            account::suspend_conn(&mut tx, id, "local")
                .await
                .map_err(api_err)?;
            let refreshed = account::find_by_id(&mut *tx, id)
                .await
                .map_err(api_err)?
                .ok_or_else(|| not_found(&admin))?;
            crate::profile::fan_out_actor_update_conn(&state, &mut tx, &refreshed)
                .await
                .map_err(IntoResponse::into_response)?;
            tx.commit()
                .await
                .map_err(plamenu_db::DbError::from)
                .map_err(api_err)?;
            "suspend"
        }
        "unsuspend" => {
            crate::moderation::unsuspend_account(&state, &account)
                .await
                .map_err(IntoResponse::into_response)?;
            "unsuspend"
        }
        "delete" => {
            crate::groups::delete_group(&state, &account)
                .await
                .map_err(IntoResponse::into_response)?;
            "delete"
        }
        _ => return Ok(redirect_show(id, "error")),
    };
    log_group(&state, &admin, action, &account).await?;
    Ok(redirect_show(id, "applied"))
}

/// Loads a local group by account id: the account row (must be a group) plus
/// its `groups` sidecar. `None` if either is missing.
async fn load_group(state: &AppState, id: i64) -> Result<Option<(Account, Group)>, Response> {
    let Some(account) = account::find_by_id(&state.pool, id)
        .await
        .map_err(api_err)?
    else {
        return Ok(None);
    };
    if !account.is_group() {
        return Ok(None);
    }
    let Some(group) = group::find(&state.pool, id).await.map_err(api_err)? else {
        return Ok(None);
    };
    Ok(Some((account, group)))
}

async fn log_group(
    state: &AppState,
    admin: &WebAdmin,
    action: &str,
    group: &Account,
) -> Result<(), Response> {
    admin_log::record(
        &state.pool,
        admin.user.current.account.id,
        action,
        &admin_log::Target::group(group),
    )
    .await
    .map_err(api_err)
}

fn policy_label(policy: &str) -> &'static str {
    match MembershipPolicy::parse(policy) {
        MembershipPolicy::Approval => "Approval",
        MembershipPolicy::Open => "Open",
    }
}

fn posting_policy_label(policy: &str) -> &'static str {
    match PostingPolicy::parse(policy) {
        PostingPolicy::Anyone => "Anyone",
        PostingPolicy::Members => "Members only",
        PostingPolicy::Mods => "Moderators only",
    }
}

fn row_status_badge(suspended: bool, deleted: bool) -> Markup {
    // Reuse the account console's badge modifiers (`is-disabled`/`is-suspended`
    // both render danger-red; `Active` is the base pill).
    let (label, class) = if deleted {
        ("Deleted", "is-disabled")
    } else if suspended {
        ("Suspended", "is-suspended")
    } else {
        ("Active", "is-active")
    };
    html! { span class=(format!("admin-badge {class}")) { (label) } }
}

fn yes_no(value: bool) -> &'static str {
    if value { "Yes" } else { "No" }
}

/// Percent-encodes a search term for the pager query string (spaces and the few
/// reserved characters a group name or handle could contain).
fn urlencode(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push('%');
                out.push(HEX[(byte >> 4) as usize] as char);
                out.push(HEX[(byte & 0x0f) as usize] as char);
            }
        }
    }
    out
}

fn redirect_show(id: i64, flash: &str) -> Response {
    (
        StatusCode::SEE_OTHER,
        [(
            header::LOCATION,
            format!("/admin/groups/{id}?flash={flash}"),
        )],
    )
        .into_response()
}

fn not_found(admin: &WebAdmin) -> Response {
    let body = html! { p { "No such group." } };
    (
        StatusCode::NOT_FOUND,
        admin_shell(admin, "/admin/groups", "Groups", &body),
    )
        .into_response()
}

fn api_err(err: plamenu_db::DbError) -> Response {
    crate::error::ApiError::from(err).into_response()
}
