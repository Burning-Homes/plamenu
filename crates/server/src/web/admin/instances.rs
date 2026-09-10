//! The known-instances directory for the admin dashboard (Mastodon's
//! `admin/instances`): every remote domain this server has seen accounts
//! from, searchable and filterable by policy, each linking to a per-instance
//! detail page with stats, delivery health and the policy controls (which
//! post to the shared `instance-policy` handlers). Requires
//! `MANAGE_FEDERATION`.

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use maud::{Markup, html};
use plamenu_db::instance_policy::{self, DomainAllow, DomainBlock, KnownInstanceFilter};
use plamenu_db::role::permission;
use plamenu_db::{job, reachability};
use serde::Deserialize;

use super::super::clock::ViewerClock;
use super::{WebAdmin, admin_shell, human_secs};
use crate::AppState;

/// Page size for the directory listing.
const PAGE_LIMIT: i64 = 50;

#[derive(Debug, Default, Deserialize)]
pub struct IndexQuery {
    after: Option<String>,
    q: Option<String>,
    policy: Option<String>,
    flash: Option<String>,
}

/// The policy-filter options: query value and human label.
const POLICY_FILTERS: &[(&str, &str)] = &[
    ("suspended", "Suspended"),
    ("limited", "Limited"),
    ("blocked", "Any block"),
    ("allowed", "Allowed"),
    ("none", "No policy"),
];

/// `GET /admin/instances` — the alphabetical directory of known remote
/// domains, searchable by domain and filterable by policy verdict.
pub async fn index(
    State(state): State<AppState>,
    admin: WebAdmin,
    Query(query): Query<IndexQuery>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_FEDERATION)?;

    let after = non_empty(query.after.as_deref());
    let q = non_empty(query.q.as_deref());
    let policy = non_empty(query.policy.as_deref())
        .filter(|p| POLICY_FILTERS.iter().any(|(value, _)| value == p));
    let pattern = q
        .as_deref()
        .map(|q| format!("%{}%", q.replace('%', "\\%").replace('_', "\\_")));
    let filter = KnownInstanceFilter {
        after_domain: after.as_deref(),
        domain_query: pattern.as_deref(),
        policy: policy.as_deref(),
        limit: PAGE_LIMIT,
    };
    let instances = instance_policy::known_instances(&state.pool, &filter)
        .await
        .map_err(api_err)?;
    let next_after = (i64::try_from(instances.len()).unwrap_or(i64::MAX) == PAGE_LIMIT)
        .then(|| instances.last().map(|instance| instance.domain.clone()))
        .flatten();
    // The server-wide delivery-health roll-up (O1/O2): queue totals, the
    // circuit breaker's open hosts and the largest per-host backlogs.
    let queued = job::pending_count(&state.pool).await.map_err(api_err)?;
    let due = job::due_count(&state.pool).await.map_err(api_err)?;
    let unreachable = reachability::unreachable_hosts(&state.pool)
        .await
        .map_err(api_err)?;
    let backlogs = if queued > 0 {
        job::queue_by_host(&state.pool, 10).await.map_err(api_err)?
    } else {
        Vec::new()
    };

    let body = html! {
        (super::flash_banner(query.flash.as_deref(), "That policy change could not be saved."))
        p.admin__lead {
            "Every remote server this instance has seen accounts from. Open a "
            "domain for its stats and federation controls; server-wide access "
            "blocks live on the "
            a href="/admin/instance-policy" { "policy page" } "."
        }
        (delivery_section(queued, due, &unreachable, &backlogs, admin.clock()))
        form.admin-filter method="get" action="/admin/instances" {
            label {
                "Domain"
                input type="text" name="q" value=(q.as_deref().unwrap_or_default()) placeholder="contains";
            }
            label {
                "Policy"
                select name="policy" {
                    option value="" selected[policy.is_none()] { "Any" }
                    @for (value, label) in POLICY_FILTERS {
                        option value=(value) selected[policy.as_deref() == Some(*value)] { (label) }
                    }
                }
            }
            button type="submit" { "Filter" }
        }
        (crate::web::view::data_table(&html! {
            thead { tr { th scope="col" { "Domain" } th.is-tight scope="col" { "Known accounts" } th.is-tight scope="col" { "Policy" } } }
            tbody data-paged {
                @if instances.is_empty() {
                    tr { td colspan="3" { "No known instances match." } }
                }
                @for instance in &instances {
                    tr {
                        td {
                            a href=(detail_path(&instance.domain)) { (instance.domain) }
                        }
                        td.is-tight.admin-dimension__value { (instance.accounts_count) }
                        td.is-tight {
                            (policy_verdict_badge(instance.block_severity.as_deref(), instance.allowed))
                        }
                    }
                }
            }
        }))
        @if let Some(after) = next_after {
            p.admin-pager {
                a href=(format!("/admin/instances?after={}{}", urlencode(&after), carry(q.as_deref(), policy.as_deref()))) {
                    "More →"
                }
            }
        }
    };
    Ok(admin_shell(&admin, "/admin/instances", "Instances", &body).into_response())
}

#[derive(Debug, Default, Deserialize)]
pub struct ShowQuery {
    flash: Option<String>,
}

/// `GET /admin/instances/{domain}` — one remote domain: content and
/// relationship stats, delivery health, and the domain-level policy controls.
/// The forms post to the shared `instance-policy` handlers with a `return_to`
/// pointing back here.
pub async fn show(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(domain): Path<String>,
    Query(query): Query<ShowQuery>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_FEDERATION)?;

    let Ok(domain) = crate::instance_policy::normalize_domain(Some(&domain)) else {
        return Ok(not_found(&admin));
    };
    let figures = instance_policy::instance_stats(&state.pool, &domain)
        .await
        .map_err(api_err)?;
    let (block, allow) = instance_policy::domain_policy(&state.pool, &domain)
        .await
        .map_err(api_err)?;
    // An unknown domain with no policy attached has nothing to show.
    if figures.accounts_count == 0 && block.is_none() && allow.is_none() {
        return Ok(not_found(&admin));
    }
    let reachability = reachability::find(&state.pool, &domain)
        .await
        .map_err(api_err)?;
    let csrf = admin.user.csrf.as_str();

    let body = html! {
        (super::flash_banner(query.flash.as_deref(), "That policy change could not be saved."))
        p.admin-back { a href="/admin/instances" { "← Back to instances" } }
        div.admin-detail {
            div.admin-detail__identity {
                div {
                    h3 { (domain) }
                }
                (policy_verdict_badge(block.as_ref().map(|b| b.severity.as_str()), allow.is_some()))
            }
            dl.admin-detail__grid {
                dt { "Known accounts" } dd { (figures.accounts_count) }
                dt { "Stored posts" } dd { (figures.statuses_count) }
                dt { "Followed by your users" } dd { (figures.follows_out) }
                dt { "Following your users" } dd { (figures.follows_in) }
                dt { "Reports against its accounts" } dd { (figures.reports_count) }
                @if let Some(last) = figures.last_status_at {
                    dt { "Latest stored post" } dd { (admin.clock().element_date(last)) }
                }
                dt { "Delivery" } dd { (delivery_health(reachability.as_ref(), admin.clock())) }
                @if let Some(block) = &block {
                    @if block.reject_media { dt { "Media" } dd { "Rejected" } }
                    @if block.reject_reports { dt { "Reports" } dd { "Rejected" } }
                }
            }
            p {
                a href=(format!("/admin/accounts?domain={}", urlencode(&domain))) {
                    "Accounts from this instance →"
                }
            }
        }

        (policy_section(&domain, csrf, block.as_ref(), allow.as_ref()))
    };
    Ok(admin_shell(&admin, "/admin/instances", &domain, &body).into_response())
}

/// The block/allow controls: an edit form for the existing block (or a
/// create form when there is none), plus the allow toggle. All forms post to
/// the shared `instance-policy` endpoints with `return_to` set back to this
/// page.
fn policy_section(
    domain: &str,
    csrf: &str,
    block: Option<&DomainBlock>,
    allow: Option<&DomainAllow>,
) -> Markup {
    let return_to = detail_path(domain);
    html! {
        section.admin-list {
            h3 { "Federation policy" }
            @match block {
                Some(block) => {
                    article.admin-record {
                        form.admin-form method="post"
                            action=(format!("/web/admin/instance-policy/domain-blocks/{}/update", block.id)) {
                            input type="hidden" name="csrf" value=(csrf);
                            input type="hidden" name="return_to" value=(return_to);
                            div.admin-record__head {
                                strong { "Domain block" }
                                (super::policy::policy_badge(&block.severity))
                            }
                            (super::policy::domain_block_fields(Some(block)))
                            div.admin-actions { button type="submit" { "Save" } }
                        }
                        form method="post"
                            action=(format!("/web/admin/instance-policy/domain-blocks/{}/delete", block.id)) {
                            input type="hidden" name="csrf" value=(csrf);
                            input type="hidden" name="return_to" value=(return_to);
                            button.admin-danger type="submit" { "Remove block" }
                        }
                    }
                }
                None => {
                    details.admin-form {
                        summary { "Limit or suspend this instance" }
                        form method="post" action="/web/admin/instance-policy/domain-blocks" {
                            input type="hidden" name="csrf" value=(csrf);
                            input type="hidden" name="return_to" value=(return_to);
                            input type="hidden" name="domain" value=(domain);
                            (super::policy::domain_block_fields(None))
                            button type="submit" { "Add block" }
                        }
                    }
                }
            }
            @match allow {
                Some(allow) => {
                    form method="post"
                        action=(format!("/web/admin/instance-policy/domain-allows/{}/delete", allow.id)) {
                        input type="hidden" name="csrf" value=(csrf);
                        input type="hidden" name="return_to" value=(return_to);
                        button type="submit" { "Remove from allow-list" }
                    }
                }
                None => {
                    form method="post" action="/web/admin/instance-policy/domain-allows" {
                        input type="hidden" name="csrf" value=(csrf);
                        input type="hidden" name="return_to" value=(return_to);
                        input type="hidden" name="domain" value=(domain);
                        button type="submit" { "Add to allow-list" }
                    }
                }
            }
        }
    }
}

/// The server-wide delivery-health section: the web face of `plamenu
/// federation queue inspect` and `federation reachability`. Renders the queue
/// summary line always (an empty queue is itself an answer); the unreachable
/// and backlog tables only when they have rows.
fn delivery_section(
    queued: u64,
    due: u64,
    unreachable: &[reachability::HostReachability],
    backlogs: &[job::QueueHost],
    clock: &ViewerClock,
) -> Markup {
    html! {
        section.admin-list #delivery-health {
            h3 { "Delivery health" }
            @if queued == 0 {
                p { "The outbound delivery queue is empty." }
            } @else {
                p {
                    (queued) " queued deliver" (if queued == 1 { "y" } else { "ies" })
                    ", " (due) " due now."
                }
            }
            @if !unreachable.is_empty() {
                h4 { "Unreachable hosts" }
                (crate::web::view::data_table(&html! {
                    thead {
                        tr {
                            th scope="col" { "Host" }
                            th.is-tight scope="col" { "Since" }
                            th.is-tight scope="col" { "Failures" }
                            th scope="col" { "Last error" }
                        }
                    }
                    tbody {
                        @for host in unreachable {
                            tr {
                                td {
                                    a href=(detail_path(&host.host)) { (host.host) }
                                    @if host.abandoned_at.is_some() {
                                        " " span.admin-badge.is-disabled { "Abandoned" }
                                    }
                                }
                                td.is-tight {
                                    @if let Some(since) = host.unreachable_since {
                                        (clock.element_date(since))
                                    }
                                }
                                td.is-tight.admin-dimension__value { (host.consecutive_failures) }
                                td {
                                    span.admin-table__sub {
                                        (host.last_error.as_deref().unwrap_or("—"))
                                    }
                                }
                            }
                        }
                    }
                }))
            }
            @if !backlogs.is_empty() {
                h4 { "Largest backlogs" }
                (crate::web::view::data_table(&html! {
                    thead {
                        tr {
                            th scope="col" { "Host" }
                            th.is-tight scope="col" { "Jobs" }
                            th.is-tight scope="col" { "Max attempts" }
                            th.is-tight scope="col" { "Next attempt" }
                        }
                    }
                    tbody {
                        @for host in backlogs {
                            tr {
                                td { a href=(detail_path(&host.host)) { (host.host) } }
                                td.is-tight.admin-dimension__value { (host.jobs) }
                                td.is-tight.admin-dimension__value { (host.max_attempts) }
                                td.is-tight {
                                    @match host.next_due_seconds {
                                        Some(secs) if secs <= 0.0 => "due now",
                                        Some(secs) => { "in " (human_secs(secs)) },
                                        None => "—",
                                    }
                                }
                            }
                        }
                    }
                }))
            }
        }
    }
}

/// The delivery-health readout from the circuit breaker's state row.
fn delivery_health(row: Option<&reachability::HostReachability>, clock: &ViewerClock) -> Markup {
    match row {
        Some(r) if r.abandoned_at.is_some() => html! {
            span.admin-badge.is-disabled { "Abandoned" }
            span.admin-table__sub {
                "Delivery permanently given up after repeated failures."
            }
        },
        Some(r) if r.unreachable_since.is_some() => html! {
            span.admin-badge.is-pending { "Unreachable" }
            @if let Some(since) = r.unreachable_since {
                span.admin-table__sub { "Since " (clock.element_date(since)) "." }
            }
        },
        Some(r) if r.consecutive_failures > 0 => html! {
            span.admin-badge { "Degraded" }
            span.admin-table__sub {
                (r.consecutive_failures) " consecutive delivery failures."
            }
        },
        _ => html! { span.admin-badge.is-active { "Reachable" } },
    }
}

/// The listing/detail badge for a domain's policy verdict.
fn policy_verdict_badge(block_severity: Option<&str>, allowed: bool) -> Markup {
    html! {
        @match block_severity {
            Some("suspend") => span.admin-badge.is-disabled { "Suspended" },
            Some("silence") => span.admin-badge.is-pending { "Limited" },
            Some(_) => span.admin-badge { "Noop block" },
            None => @if allowed {
                span.admin-badge.is-active { "Allowed" }
            } @else {
                span.admin-table__sub { "—" }
            },
        }
    }
}

/// The detail-page path for a domain, percent-encoded once.
fn detail_path(domain: &str) -> String {
    format!("/admin/instances/{}", urlencode(domain))
}

/// Percent-encodes a domain for a path/query position. Domains are almost
/// always plain ASCII; this covers the stray IDN or garbage row.
fn urlencode(value: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// Re-appends the active filters to the pager link.
fn carry(q: Option<&str>, policy: Option<&str>) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    if let Some(q) = q {
        let _ = write!(out, "&q={}", urlencode(q));
    }
    if let Some(policy) = policy {
        let _ = write!(out, "&policy={policy}");
    }
    out
}

/// Empties a blank query value to `None`.
fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned)
}

fn not_found(admin: &WebAdmin) -> Response {
    let body = html! { p { "No such instance is known here." } };
    (
        axum::http::StatusCode::NOT_FOUND,
        admin_shell(admin, "/admin/instances", "Not found", &body),
    )
        .into_response()
}

fn api_err(err: plamenu_db::DbError) -> Response {
    crate::error::ApiError::from(err).into_response()
}
