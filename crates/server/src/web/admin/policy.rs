//! Instance-policy CRUD for the admin dashboard. Federation policy
//! (`domain_blocks`/`domain_allows`) requires `MANAGE_FEDERATION`; sign-up and
//! access blocks (`email_domain_blocks`, `ip_blocks`, `canonical_email_blocks`)
//! require `MANAGE_BLOCKS`.

use std::net::IpAddr;

use axum::extract::{Form, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use maud::{Markup, html};
use plamenu_db::instance_policy::{
    self, CanonicalEmailBlock, DomainAllow, DomainBlock, DomainBlockUpdate, EmailDomainBlock,
    IpBlock, IpBlockUpdate, Page,
};
use plamenu_db::role::permission;
use serde::Deserialize;
use time::{Duration, OffsetDateTime};

use super::super::clock::ViewerClock;
use super::{WebAdmin, admin_shell};
use crate::instance_policy::{canonical_email_hash, normalize_domain};
use crate::web::session::csrf_rejection;
use crate::{AppState, admin_log};

const PAGE_LIMIT: i64 = 100;

#[derive(Debug, Default, Deserialize)]
pub struct IndexQuery {
    flash: Option<String>,
    /// An address to test against the canonical e-mail blocks (A4); submitted
    /// by the read-only GET form in that section.
    test_email: Option<String>,
}

/// `GET /admin/instance-policy` — all currently-supported instance-policy
/// record types, filtered by the moderator's permissions.
pub async fn index(
    State(state): State<AppState>,
    admin: WebAdmin,
    Query(query): Query<IndexQuery>,
) -> Result<Response, Response> {
    admin.require_any(&[permission::MANAGE_FEDERATION, permission::MANAGE_BLOCKS])?;

    let page = Page {
        max_id: None,
        since_id: None,
        min_id: None,
        limit: PAGE_LIMIT,
    };
    let can_federation = admin.role.can(permission::MANAGE_FEDERATION);
    let can_blocks = admin.role.can(permission::MANAGE_BLOCKS);
    let domain_blocks = if can_federation {
        instance_policy::list_domain_blocks(&state.pool, &page)
            .await
            .map_err(api_err)?
    } else {
        Vec::new()
    };
    let domain_allows = if can_federation {
        instance_policy::list_domain_allows(&state.pool, &page)
            .await
            .map_err(api_err)?
    } else {
        Vec::new()
    };
    let email_blocks = if can_blocks {
        instance_policy::list_email_domain_blocks(&state.pool, &page)
            .await
            .map_err(api_err)?
    } else {
        Vec::new()
    };
    let ip_blocks = if can_blocks {
        instance_policy::list_ip_blocks(&state.pool, &page)
            .await
            .map_err(api_err)?
    } else {
        Vec::new()
    };
    let canonical_blocks = if can_blocks {
        instance_policy::list_canonical_email_blocks(&state.pool, &page)
            .await
            .map_err(api_err)?
    } else {
        Vec::new()
    };
    // The read-only test-an-address probe: which canonical blocks would catch
    // this e-mail (the web face of `…/canonical_email_blocks/test`).
    let test_email = query
        .test_email
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty());
    let canonical_test = match test_email {
        Some(email) if can_blocks => match canonical_email_hash(email) {
            Ok(hash) => Some(CanonicalTest {
                email: email.to_owned(),
                matches: instance_policy::matching_canonical_email_blocks(&state.pool, &hash)
                    .await
                    .map_err(api_err)?,
                invalid: false,
            }),
            Err(_) => Some(CanonicalTest {
                email: email.to_owned(),
                matches: Vec::new(),
                invalid: true,
            }),
        },
        _ => None,
    };

    let csrf = admin.user.csrf.as_str();
    let body = html! {
        (super::flash_banner(query.flash.as_deref(), "That policy change could not be saved."))
        @if can_federation {
            (domain_blocks_section(csrf, &domain_blocks))
            (domain_allows_section(csrf, &domain_allows, admin.clock()))
        }
        @if can_blocks {
            (email_blocks_section(csrf, &email_blocks))
            (ip_blocks_section(csrf, &ip_blocks, admin.clock()))
            (canonical_blocks_section(csrf, &canonical_blocks, canonical_test.as_ref(), admin.clock()))
        }
    };
    Ok(admin_shell(&admin, "/admin/instance-policy", "Instance policy", &body).into_response())
}

fn domain_blocks_section(csrf: &str, blocks: &[DomainBlock]) -> Markup {
    html! {
        section.admin-list {
            h3 { "Domain blocks" }
            details.admin-form open {
                summary { "Add domain block" }
                form method="post" action="/web/admin/instance-policy/domain-blocks" {
                    input type="hidden" name="csrf" value=(csrf);
                    label { "Domain" input type="text" name="domain" required; }
                    (domain_block_fields(None))
                    button type="submit" { "Add block" }
                }
            }
            @if blocks.is_empty() {
                p.empty { "No domain blocks." }
            }
            @for block in blocks {
                article.admin-record {
                    form.admin-form method="post" action=(format!("/web/admin/instance-policy/domain-blocks/{}/update", block.id)) {
                        input type="hidden" name="csrf" value=(csrf);
                        div.admin-record__head {
                            strong { (block.domain) }
                            (policy_badge(&block.severity))
                        }
                        (domain_block_fields(Some(block)))
                        div.admin-actions { button type="submit" { "Save" } }
                    }
                    (delete_form(&format!("/web/admin/instance-policy/domain-blocks/{}/delete", block.id), csrf))
                }
            }
        }
    }
}

pub(super) fn domain_block_fields(block: Option<&DomainBlock>) -> Markup {
    let severity = block.map_or("silence", |b| b.severity.as_str());
    let private_comment = block
        .and_then(|b| b.private_comment.as_deref())
        .unwrap_or("");
    let public_comment = block
        .and_then(|b| b.public_comment.as_deref())
        .unwrap_or("");
    let reject_media = block.is_some_and(|b| b.reject_media);
    let reject_reports = block.is_some_and(|b| b.reject_reports);
    let obfuscate = block.is_some_and(|b| b.obfuscate);
    html! {
        label {
            "Severity"
            select name="severity" {
                @for opt in ["silence", "suspend", "noop"] {
                    option value=(opt) selected[severity == opt] { (opt) }
                }
            }
        }
        label.admin-check {
            input type="checkbox" name="reject_media" value="1" checked[reject_media];
            span { "Reject media" }
        }
        label.admin-check {
            input type="checkbox" name="reject_reports" value="1" checked[reject_reports];
            span { "Reject reports" }
        }
        label.admin-check {
            input type="checkbox" name="obfuscate" value="1" checked[obfuscate];
            span { "Obfuscate" }
        }
        label { "Private comment" textarea name="private_comment" rows="2" { (private_comment) } }
        label { "Public comment" textarea name="public_comment" rows="2" { (public_comment) } }
    }
}

fn domain_allows_section(csrf: &str, allows: &[DomainAllow], clock: &ViewerClock) -> Markup {
    html! {
        section.admin-list {
            h3 { "Domain allows" }
            form.admin-filter method="post" action="/web/admin/instance-policy/domain-allows" {
                input type="hidden" name="csrf" value=(csrf);
                label { "Domain" input type="text" name="domain" required; }
                button type="submit" { "Allow domain" }
            }
            (crate::web::view::data_table(&html! {
                thead { tr { th scope="col" { "Domain" } th scope="col" { "Created" } th scope="col" {} } }
                tbody {
                    @if allows.is_empty() {
                        tr { td colspan="3" { "No allowed domains." } }
                    }
                    @for allow in allows {
                        tr {
                            td { (allow.domain) }
                            td { (clock.element_date(allow.created_at)) }
                            td { (delete_form(&format!("/web/admin/instance-policy/domain-allows/{}/delete", allow.id), csrf)) }
                        }
                    }
                }
            }))
        }
    }
}

fn email_blocks_section(csrf: &str, blocks: &[EmailDomainBlock]) -> Markup {
    html! {
        section.admin-list {
            h3 { "E-mail domain blocks" }
            form.admin-filter method="post" action="/web/admin/instance-policy/email-domain-blocks" {
                input type="hidden" name="csrf" value=(csrf);
                label { "Domain" input type="text" name="domain" required; }
                label.admin-check {
                    input type="checkbox" name="allow_with_approval" value="1";
                    span { "Allow with approval" }
                }
                button type="submit" { "Block e-mail domain" }
            }
            (crate::web::view::data_table(&html! {
                thead { tr { th scope="col" { "Domain" } th scope="col" { "Approval" } th scope="col" {} } }
                tbody {
                    @if blocks.is_empty() {
                        tr { td colspan="3" { "No e-mail domain blocks." } }
                    }
                    @for block in blocks {
                        tr {
                            td { (block.domain) }
                            td { (if block.allow_with_approval { "Required" } else { "Blocked" }) }
                            td { (delete_form(&format!("/web/admin/instance-policy/email-domain-blocks/{}/delete", block.id), csrf)) }
                        }
                    }
                }
            }))
        }
    }
}

fn ip_blocks_section(csrf: &str, blocks: &[IpBlock], clock: &ViewerClock) -> Markup {
    html! {
        section.admin-list {
            h3 { "IP blocks" }
            details.admin-form {
                summary { "Add IP block" }
                form method="post" action="/web/admin/instance-policy/ip-blocks" {
                    input type="hidden" name="csrf" value=(csrf);
                    (ip_block_fields(None))
                    button type="submit" { "Add IP block" }
                }
            }
            @if blocks.is_empty() {
                p.empty { "No IP blocks." }
            }
            @for block in blocks {
                article.admin-record {
                    form.admin-form method="post" action=(format!("/web/admin/instance-policy/ip-blocks/{}/update", block.id)) {
                        input type="hidden" name="csrf" value=(csrf);
                        div.admin-record__head {
                            strong { (block.ip) }
                            (policy_badge(&block.severity))
                        }
                        // The form takes a *duration* ("expires in N seconds");
                        // echo the instant it lands on, in the admin's zone, so
                        // the rule's actual end is legible without arithmetic.
                        @if let Some(expires) = block.expires_at {
                            span.admin-table__sub {
                                "Expires " (clock.element(expires))
                            }
                        }
                        (ip_block_fields(Some(block)))
                        div.admin-actions { button type="submit" { "Save" } }
                    }
                    (delete_form(&format!("/web/admin/instance-policy/ip-blocks/{}/delete", block.id), csrf))
                }
            }
        }
    }
}

fn ip_block_fields(block: Option<&IpBlock>) -> Markup {
    let ip = block.map_or("", |b| b.ip.as_str());
    let severity = block.map_or("sign_up_block", |b| b.severity.as_str());
    let comment = block.map_or("", |b| b.comment.as_str());
    html! {
        label { "IP/CIDR" input type="text" name="ip" value=(ip) placeholder="192.0.2.0/24" required; }
        label {
            "Severity"
            select name="severity" {
                @for opt in ["sign_up_requires_approval", "sign_up_block", "no_access"] {
                    option value=(opt) selected[severity == opt] { (opt) }
                }
            }
        }
        label { "Comment" textarea name="comment" rows="2" { (comment) } }
        label { "Expires in seconds" input type="number" name="expires_in" min="0" placeholder="0"; }
    }
}

/// The outcome of the canonical-block test probe, rendered inline under the
/// test form.
struct CanonicalTest {
    email: String,
    matches: Vec<CanonicalEmailBlock>,
    invalid: bool,
}

fn canonical_blocks_section(
    csrf: &str,
    blocks: &[CanonicalEmailBlock],
    test: Option<&CanonicalTest>,
    clock: &ViewerClock,
) -> Markup {
    html! {
        section.admin-list {
            h3 { "Canonical e-mail blocks" }
            form.admin-filter method="post" action="/web/admin/instance-policy/canonical-email-blocks" {
                input type="hidden" name="csrf" value=(csrf);
                label { "E-mail" input type="email" name="email" placeholder="user@example.com"; }
                label { "Hash" input type="text" name="canonical_email_hash" placeholder="sha256"; }
                button type="submit" { "Block e-mail" }
            }
            form.admin-filter method="get" action="/admin/instance-policy" {
                label {
                    "Test an address"
                    input type="email" name="test_email"
                        value=(test.map(|t| t.email.as_str()).unwrap_or_default())
                        placeholder="user@example.com";
                }
                button type="submit" { "Test" }
            }
            @if let Some(test) = test {
                @if test.invalid {
                    p.admin-flash.is-error role="alert" {
                        (test.email) " is not a valid e-mail address."
                    }
                } @else if test.matches.is_empty() {
                    p.admin-flash role="status" {
                        "No canonical e-mail block matches " (test.email) "."
                    }
                } @else {
                    p.admin-flash role="status" {
                        (test.email) " is blocked by "
                        (test.matches.len())
                        @if test.matches.len() == 1 { " canonical e-mail block:" }
                        @else { " canonical e-mail blocks:" }
                        @for block in &test.matches {
                            " " code { (block.canonical_email_hash) }
                        }
                    }
                }
            }
            (crate::web::view::data_table(&html! {
                thead { tr { th scope="col" { "Canonical hash" } th scope="col" { "Created" } th scope="col" {} } }
                tbody {
                    @if blocks.is_empty() {
                        tr { td colspan="3" { "No canonical e-mail blocks." } }
                    }
                    @for block in blocks {
                        tr {
                            td { code { (block.canonical_email_hash) } }
                            td { (clock.element_date(block.created_at)) }
                            td { (delete_form(&format!("/web/admin/instance-policy/canonical-email-blocks/{}/delete", block.id), csrf)) }
                        }
                    }
                }
            }))
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct DomainBlockForm {
    csrf: String,
    /// Admin path to land on after the change; the instance detail page sets
    /// this so its embedded forms return there.
    #[serde(default)]
    return_to: String,
    #[serde(default)]
    domain: String,
    severity: String,
    reject_media: Option<String>,
    reject_reports: Option<String>,
    obfuscate: Option<String>,
    #[serde(default)]
    private_comment: String,
    #[serde(default)]
    public_comment: String,
}

pub async fn create_domain_block(
    State(state): State<AppState>,
    admin: WebAdmin,
    Form(form): Form<DomainBlockForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_FEDERATION)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let Ok(domain) = normalize_domain(Some(&form.domain)) else {
        return Ok(redirect_back(&form.return_to, "error"));
    };
    if instance_policy::find_domain_block_by_domain(&state.pool, &domain)
        .await
        .map_err(api_err)?
        .is_some()
    {
        return Ok(redirect_back(&form.return_to, "error"));
    }
    let Some(severity) = domain_severity(&form.severity) else {
        return Ok(redirect_back(&form.return_to, "error"));
    };
    let block = instance_policy::create_domain_block(
        &state.pool,
        instance_policy::NewDomainBlock {
            domain: &domain,
            severity,
            reject_media: form.reject_media.is_some(),
            reject_reports: form.reject_reports.is_some(),
            private_comment: blank_none(&form.private_comment),
            public_comment: blank_none(&form.public_comment),
            obfuscate: form.obfuscate.is_some(),
        },
    )
    .await
    .map_err(api_err)?;
    log_policy(
        &state,
        &admin,
        "create",
        &admin_log::Target::domain_block(block.id, &block.domain),
    )
    .await?;
    crate::media_worker::spawn_domain_purge(&state, &block);
    Ok(redirect_back(&form.return_to, "applied"))
}

pub async fn update_domain_block(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<DomainBlockForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_FEDERATION)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let Some(severity) = domain_severity(&form.severity) else {
        return Ok(redirect_back(&form.return_to, "error"));
    };
    let updated = instance_policy::update_domain_block(
        &state.pool,
        id,
        DomainBlockUpdate {
            severity: Some(severity),
            reject_media: Some(form.reject_media.is_some()),
            reject_reports: Some(form.reject_reports.is_some()),
            private_comment: Some(form.private_comment.trim()),
            public_comment: Some(form.public_comment.trim()),
            obfuscate: Some(form.obfuscate.is_some()),
        },
    )
    .await
    .map_err(api_err)?;
    if let Some(block) = &updated {
        log_policy(
            &state,
            &admin,
            "update",
            &admin_log::Target::domain_block(block.id, &block.domain),
        )
        .await?;
        crate::media_worker::spawn_domain_purge(&state, block);
    }
    Ok(redirect_back(
        &form.return_to,
        if updated.is_some() {
            "applied"
        } else {
            "error"
        },
    ))
}

#[derive(Debug, Deserialize)]
pub struct DomainForm {
    csrf: String,
    #[serde(default)]
    return_to: String,
    domain: String,
}

pub async fn create_domain_allow(
    State(state): State<AppState>,
    admin: WebAdmin,
    Form(form): Form<DomainForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_FEDERATION)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let Ok(domain) = normalize_domain(Some(&form.domain)) else {
        return Ok(redirect_back(&form.return_to, "error"));
    };
    let allow = instance_policy::create_domain_allow(&state.pool, &domain)
        .await
        .map_err(api_err)?;
    log_policy(
        &state,
        &admin,
        "create",
        &admin_log::Target::domain_allow(allow.id, &allow.domain),
    )
    .await?;
    Ok(redirect_back(&form.return_to, "applied"))
}

#[derive(Debug, Deserialize)]
pub struct EmailBlockForm {
    csrf: String,
    domain: String,
    allow_with_approval: Option<String>,
}

pub async fn create_email_block(
    State(state): State<AppState>,
    admin: WebAdmin,
    Form(form): Form<EmailBlockForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_BLOCKS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let Ok(domain) = normalize_domain(Some(&form.domain)) else {
        return Ok(redirect_policy("error"));
    };
    if instance_policy::find_email_domain_block_by_domain(&state.pool, &domain)
        .await
        .map_err(api_err)?
        .is_some()
    {
        return Ok(redirect_policy("error"));
    }
    let block = instance_policy::create_email_domain_block(
        &state.pool,
        &domain,
        form.allow_with_approval.is_some(),
    )
    .await
    .map_err(api_err)?;
    log_policy(
        &state,
        &admin,
        "create",
        &admin_log::Target::email_domain_block(block.id, &block.domain),
    )
    .await?;
    Ok(redirect_policy("applied"))
}

#[derive(Debug, Deserialize)]
pub struct IpBlockForm {
    csrf: String,
    ip: String,
    severity: String,
    #[serde(default)]
    comment: String,
    #[serde(default)]
    expires_in: String,
}

pub async fn create_ip_block(
    State(state): State<AppState>,
    admin: WebAdmin,
    Form(form): Form<IpBlockForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_BLOCKS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let Some(ip) = normalize_ip(&form.ip) else {
        return Ok(redirect_policy("error"));
    };
    if instance_policy::find_ip_block_by_ip(&state.pool, &ip)
        .await
        .map_err(api_err)?
        .is_some()
    {
        return Ok(redirect_policy("error"));
    }
    let Some(severity) = ip_severity(&form.severity) else {
        return Ok(redirect_policy("error"));
    };
    let Ok(expires_at) = expires_at(&form.expires_in) else {
        return Ok(redirect_policy("error"));
    };
    let block = instance_policy::create_ip_block(
        &state.pool,
        &ip,
        severity,
        form.comment.trim(),
        expires_at,
    )
    .await
    .map_err(api_err)?;
    log_policy(
        &state,
        &admin,
        "create",
        &admin_log::Target::ip_block(block.id, &block.ip),
    )
    .await?;
    Ok(redirect_policy("applied"))
}

pub async fn update_ip_block(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<IpBlockForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_BLOCKS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let Some(ip) = normalize_ip(&form.ip) else {
        return Ok(redirect_policy("error"));
    };
    let Some(severity) = ip_severity(&form.severity) else {
        return Ok(redirect_policy("error"));
    };
    let Ok(expires_at) = expires_at(&form.expires_in) else {
        return Ok(redirect_policy("error"));
    };
    let updated = instance_policy::update_ip_block(
        &state.pool,
        id,
        IpBlockUpdate {
            ip: Some(&ip),
            severity: Some(severity),
            comment: Some(form.comment.trim()),
            update_expires_at: true,
            expires_at,
        },
    )
    .await
    .map_err(api_err)?;
    if let Some(block) = &updated {
        log_policy(
            &state,
            &admin,
            "update",
            &admin_log::Target::ip_block(block.id, &block.ip),
        )
        .await?;
    }
    Ok(redirect_policy(if updated.is_some() {
        "applied"
    } else {
        "error"
    }))
}

#[derive(Debug, Deserialize)]
pub struct CanonicalEmailForm {
    csrf: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    canonical_email_hash: String,
}

pub async fn create_canonical_email_block(
    State(state): State<AppState>,
    admin: WebAdmin,
    Form(form): Form<CanonicalEmailForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_BLOCKS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let Some(hash) = canonical_hash(&form) else {
        return Ok(redirect_policy("error"));
    };
    if instance_policy::find_canonical_email_block_by_hash(&state.pool, &hash)
        .await
        .map_err(api_err)?
        .is_some()
    {
        return Ok(redirect_policy("error"));
    }
    let block = instance_policy::create_canonical_email_block(&state.pool, &hash, None)
        .await
        .map_err(api_err)?;
    log_policy(
        &state,
        &admin,
        "create",
        &admin_log::Target::canonical_email_block(block.id),
    )
    .await?;
    Ok(redirect_policy("applied"))
}

#[derive(Debug, Deserialize)]
pub struct CsrfForm {
    csrf: String,
    #[serde(default)]
    return_to: String,
}

pub async fn delete_domain_block(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_FEDERATION)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let target = instance_policy::find_domain_block(&state.pool, id)
        .await
        .map_err(api_err)?;
    let deleted = instance_policy::delete_domain_block(&state.pool, id)
        .await
        .map_err(api_err)?;
    if deleted && let Some(block) = target {
        log_policy(
            &state,
            &admin,
            "destroy",
            &admin_log::Target::domain_block(block.id, &block.domain),
        )
        .await?;
    }
    Ok(redirect_back(
        &form.return_to,
        if deleted { "applied" } else { "error" },
    ))
}

pub async fn delete_domain_allow(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_FEDERATION)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let target = instance_policy::find_domain_allow(&state.pool, id)
        .await
        .map_err(api_err)?;
    let deleted = instance_policy::delete_domain_allow(&state.pool, id)
        .await
        .map_err(api_err)?;
    if deleted && let Some(allow) = target {
        log_policy(
            &state,
            &admin,
            "destroy",
            &admin_log::Target::domain_allow(allow.id, &allow.domain),
        )
        .await?;
    }
    Ok(redirect_back(
        &form.return_to,
        if deleted { "applied" } else { "error" },
    ))
}

pub async fn delete_email_block(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_BLOCKS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let target = instance_policy::find_email_domain_block(&state.pool, id)
        .await
        .map_err(api_err)?;
    let deleted = instance_policy::delete_email_domain_block(&state.pool, id)
        .await
        .map_err(api_err)?;
    if deleted && let Some(block) = target {
        log_policy(
            &state,
            &admin,
            "destroy",
            &admin_log::Target::email_domain_block(block.id, &block.domain),
        )
        .await?;
    }
    Ok(redirect_policy(if deleted { "applied" } else { "error" }))
}

pub async fn delete_ip_block(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_BLOCKS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let target = instance_policy::find_ip_block(&state.pool, id)
        .await
        .map_err(api_err)?;
    let deleted = instance_policy::delete_ip_block(&state.pool, id)
        .await
        .map_err(api_err)?;
    if deleted && let Some(block) = target {
        log_policy(
            &state,
            &admin,
            "destroy",
            &admin_log::Target::ip_block(block.id, &block.ip),
        )
        .await?;
    }
    Ok(redirect_policy(if deleted { "applied" } else { "error" }))
}

pub async fn delete_canonical_email_block(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_BLOCKS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let deleted = instance_policy::delete_canonical_email_block(&state.pool, id)
        .await
        .map_err(api_err)?;
    if deleted {
        log_policy(
            &state,
            &admin,
            "destroy",
            &admin_log::Target::canonical_email_block(id),
        )
        .await?;
    }
    Ok(redirect_policy(if deleted { "applied" } else { "error" }))
}

/// Appends a policy verb to the audit log.
async fn log_policy(
    state: &AppState,
    admin: &WebAdmin,
    verb: &str,
    target: &admin_log::Target,
) -> Result<(), Response> {
    admin_log::record(&state.pool, admin.user.current.account.id, verb, target)
        .await
        .map_err(api_err)?;
    Ok(())
}

fn delete_form(action: &str, csrf: &str) -> Markup {
    html! {
        form method="post" action=(action) {
            input type="hidden" name="csrf" value=(csrf);
            button.admin-danger type="submit" { "Delete" }
        }
    }
}

fn domain_severity(raw: &str) -> Option<&'static str> {
    match raw.trim() {
        "silence" => Some("silence"),
        "suspend" => Some("suspend"),
        "noop" => Some("noop"),
        _ => None,
    }
}

fn ip_severity(raw: &str) -> Option<&'static str> {
    match raw.trim() {
        "sign_up_requires_approval" => Some("sign_up_requires_approval"),
        "sign_up_block" => Some("sign_up_block"),
        "no_access" => Some("no_access"),
        _ => None,
    }
}

fn normalize_ip(raw: &str) -> Option<String> {
    let value = raw.trim();
    if value.is_empty() {
        return None;
    }
    let (addr, prefix) = if let Some((addr, prefix)) = value.split_once('/') {
        let parsed: IpAddr = addr.parse().ok()?;
        let prefix: u8 = prefix.parse().ok()?;
        let max = if parsed.is_ipv4() { 32 } else { 128 };
        if prefix > max {
            return None;
        }
        (parsed, prefix)
    } else {
        let parsed: IpAddr = value.parse().ok()?;
        let prefix = if parsed.is_ipv4() { 32 } else { 128 };
        (parsed, prefix)
    };
    Some(format!("{addr}/{prefix}"))
}

fn expires_at(raw: &str) -> Result<Option<OffsetDateTime>, ()> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let seconds: i64 = raw.parse().map_err(|_| ())?;
    if seconds <= 0 {
        return Ok(None);
    }
    Ok(Some(OffsetDateTime::now_utc() + Duration::seconds(seconds)))
}

fn canonical_hash(form: &CanonicalEmailForm) -> Option<String> {
    let hash = form.canonical_email_hash.trim();
    if !hash.is_empty() {
        return Some(hash.to_owned());
    }
    canonical_email_hash(form.email.trim()).ok()
}

fn blank_none(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

pub(super) fn policy_badge(label: &str) -> Markup {
    html! { span.admin-badge { (label) } }
}

fn redirect_policy(flash: &str) -> Response {
    (
        StatusCode::SEE_OTHER,
        [(
            header::LOCATION,
            format!("/admin/instance-policy?flash={flash}"),
        )],
    )
        .into_response()
}

/// Redirects to the form's `return_to` when it names another admin page (the
/// per-instance detail page embeds these forms), else to the policy page.
/// The prefix check keeps the redirect on-dashboard — never an open redirect.
fn redirect_back(return_to: &str, flash: &str) -> Response {
    if return_to.starts_with("/admin/") {
        (
            StatusCode::SEE_OTHER,
            [(header::LOCATION, format!("{return_to}?flash={flash}"))],
        )
            .into_response()
    } else {
        redirect_policy(flash)
    }
}

fn api_err(err: plamenu_db::DbError) -> Response {
    crate::error::ApiError::from(err).into_response()
}
