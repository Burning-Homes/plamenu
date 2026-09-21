//! Operator controls and diagnostics for bounded remote-history hydration.

use axum::Form;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use maud::html;
use plamenu_db::remote_history;
use plamenu_db::role::permission;
use serde::Deserialize;

use super::{WebAdmin, admin_shell};
use crate::AppState;
use crate::error::ApiError;
use crate::web::clock::zone_chip;
use crate::web::session::csrf_rejection;

#[derive(Deserialize, Default)]
pub struct PageQuery {
    flash: Option<String>,
}

#[allow(clippy::too_many_lines, reason = "single operator diagnostics page")]
pub async fn index(
    State(state): State<AppState>,
    admin: WebAdmin,
    Query(query): Query<PageQuery>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_FEDERATION)?;
    let settings = remote_history::settings(&state.pool)
        .await
        .map_err(api_err)?;
    let diagnostics = remote_history::diagnostics(&state.pool)
        .await
        .map_err(api_err)?;
    let metrics = crate::remote_history::metrics();
    let enabled_label = if settings.enabled {
        "Enabled"
    } else {
        "Disabled"
    };
    let body = html! {
        @if query.flash.as_deref() == Some("saved") {
            p.admin-flash role="status" { "Remote history settings saved." }
        }
        @if query.flash.as_deref() == Some("pruned") {
            p.admin-flash role="status" { "One bounded remote history prune batch completed." }
        }
        div.admin-history__intro {
            div {
                p.admin__lead {
                    "Demand-driven hydration fills gaps on remote profiles without "
                    "turning profile views into unbounded federation traffic."
                }
                p.settings-field__hint {
                    "Each job fetches one outbox envelope and one page of at most 20 items. "
                    "Automatic views cool down for six hours and share federation's SSRF guard and origin backoff."
                }
            }
            span.admin-badge class=(if settings.enabled { "is-pending" } else { "is-disabled" }) {
                (enabled_label)
            }
        }
        div.admin__stats aria-label="Remote history overview" {
            (stat("Pending", &diagnostics.pending_jobs.to_string()))
            (stat("In progress", &diagnostics.leased_jobs.to_string()))
            (stat("Cached posts", &diagnostics.history_statuses.to_string()))
            (stat("Stored text", &human_bytes(diagnostics.history_bytes)))
            (stat("Average job", &format!("{} ms", metrics.average_job_ms)))
            (stat("Intent to first post", &format!("{} ms", metrics.average_intent_to_first_ms)))
        }

        div.admin-history__controls {
            form.admin-history__card method="post" action="/web/admin/remote-history" {
                input type="hidden" name="csrf" value=(admin.user.csrf);
                h3 { "Hydration policy" }
                label.admin-check {
                    input type="checkbox" name="enabled" value="1" checked[settings.enabled];
                    span { strong { "Enable remote history hydration" } }
                }
                p.settings-field__hint {
                    "The kill switch stops new claims immediately; queued jobs remain durable."
                }
                label {
                    "Cold history retention"
                    span.admin-history__number {
                        input type="number" name="retention_days" min="1" max="3650"
                            value=(settings.retention_days) required;
                        span { "days" }
                    }
                    span.settings-field__hint {
                        "Posts involved in local interactions and conversations are preserved."
                    }
                }
                label.admin-check {
                    input type="checkbox" name="bare_iri_enabled" value="1"
                        checked[settings.bare_iri_enabled];
                    span { strong { "Resolve bare item IRIs" } }
                }
                p.settings-field__hint {
                    "Compatibility mode adds at most five same-origin item requests per page."
                }
                div.admin-actions { button type="submit" { "Save settings" } }
            }
            form.admin-history__card method="post" action="/web/admin/remote-history/prune" {
                input type="hidden" name="csrf" value=(admin.user.csrf);
                h3 { "Storage maintenance" }
                p {
                    "Remove one bounded batch of stale cold posts while preserving "
                    "bookmarks, favourites, pins, replies, boosts, quotes and reports."
                }
                p.settings-field__hint {
                    "At most 200 rows are removed. Associated media cleanup is queued atomically."
                }
                div.admin-actions { button type="submit" { "Prune one batch" } }
            }
        }

        section.admin-history__section {
            h3 { "Process counters" }
            dl.admin-history__metrics {
                div { dt { "Requested" } dd { (metrics.requested) } }
                div { dt { "Enqueued" } dd { (metrics.enqueued) } }
                div { dt { "Coalesced" } dd { (metrics.coalesced) } }
                div { dt { "Completed" } dd { (metrics.completed) } }
                div { dt { "Failed" } dd { (metrics.failed) } }
                div { dt { "Pages" } dd { (metrics.pages) } }
                div { dt { "Accepted posts" } dd { (metrics.accepted) } }
                div { dt { "Fetched bytes" } dd { (human_bytes(i64::try_from(metrics.bytes).unwrap_or(i64::MAX))) } }
                div { dt { "IRI requests" } dd { (metrics.iri_dereferences) } }
                div { dt { "Pruned" } dd { (metrics.pruned) } }
            }
        }
        (zone_chip(admin.clock()))
        @if !diagnostics.top_origins.is_empty() {
            section.admin-history__section {
                h3 { "Queued origins" }
                div.table-scroll tabindex="0" {
                    table.admin-table {
                        thead { tr { th { "Origin" } th { "Jobs" } th { "Oldest" } } }
                        tbody {
                            @for origin in &diagnostics.top_origins {
                                tr {
                                    td { code { (origin.origin) } }
                                    td.is-num { (origin.queued) }
                                    td.is-tight { (admin.clock().element_absolute(origin.oldest_job_at)) }
                                }
                            }
                        }
                    }
                }
            }
        }
        @if !diagnostics.top_storage_origins.is_empty() {
            section.admin-history__section {
                h3 { "Cold storage by origin" }
                div.table-scroll tabindex="0" {
                    table.admin-table {
                        thead { tr { th { "Origin" } th { "Posts" } th { "Text" } } }
                        tbody {
                            @for origin in &diagnostics.top_storage_origins {
                                tr {
                                    td { code { (origin.origin) } }
                                    td.is-num { (origin.statuses) }
                                    td.is-num { (human_bytes(origin.bytes)) }
                                }
                            }
                        }
                    }
                }
            }
        }
        @if !diagnostics.failures.is_empty() {
            section.admin-history__section {
                h3 { "Persisted actor failures" }
                ul.admin-history__reasons {
                @for failure in &diagnostics.failures {
                    li { code { (failure.class) } strong { (failure.actors) } }
                }
                }
            }
        }
        @if !metrics.reasons.is_empty() {
            section.admin-history__section {
                h3 { "Since-process-start reasons" }
                ul.admin-history__reasons {
                    @for (name, count) in &metrics.reasons {
                        li { code { (name) } strong { (count) } }
                    }
                }
            }
        }
    };
    Ok(admin_shell(&admin, "/admin/remote-history", "Remote history", &body).into_response())
}

fn stat(label: &str, value: &str) -> maud::Markup {
    html! {
        div.admin-stat {
            span.admin-stat__value { (value) }
            span.admin-stat__label { (label) }
        }
    }
}

fn human_bytes(bytes: i64) -> String {
    let bytes = u64::try_from(bytes).unwrap_or(0);
    for (unit, size) in [
        ("GB", 1_073_741_824_u64),
        ("MB", 1_048_576_u64),
        ("KB", 1024_u64),
    ] {
        if bytes >= size {
            let whole = bytes / size;
            let decimal = (bytes % size).saturating_mul(10) / size;
            return format!("{whole}.{decimal} {unit}");
        }
    }
    format!("{bytes} B")
}

#[derive(Deserialize)]
pub struct SettingsForm {
    csrf: String,
    enabled: Option<String>,
    retention_days: i32,
    bare_iri_enabled: Option<String>,
}

pub async fn save(
    State(state): State<AppState>,
    admin: WebAdmin,
    Form(form): Form<SettingsForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_FEDERATION)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    remote_history::save_settings(
        &state.pool,
        form.enabled.is_some(),
        form.retention_days,
        form.bare_iri_enabled.is_some(),
    )
    .await
    .map_err(api_err)?;
    Ok((
        StatusCode::SEE_OTHER,
        [(header::LOCATION, "/admin/remote-history?flash=saved")],
    )
        .into_response())
}

fn api_err(error: plamenu_db::DbError) -> Response {
    ApiError::from(error).into_response()
}

#[derive(Deserialize)]
pub struct PruneForm {
    csrf: String,
}

pub async fn prune(
    State(state): State<AppState>,
    admin: WebAdmin,
    Form(form): Form<PruneForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_FEDERATION)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let settings = remote_history::settings(&state.pool)
        .await
        .map_err(api_err)?;
    let pruned = remote_history::prune(&state.pool, settings.retention_days, 200)
        .await
        .map_err(api_err)?;
    crate::remote_history::record_pruned(pruned);
    Ok((
        StatusCode::SEE_OTHER,
        [(header::LOCATION, "/admin/remote-history?flash=pruned")],
    )
        .into_response())
}
