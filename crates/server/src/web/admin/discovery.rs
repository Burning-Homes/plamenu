//! Small operator source view and policy controls; HTTP work stays in the worker.
use super::{WebAdmin, admin_shell};
use crate::{AppState, error::ApiError, web::session::csrf_rejection};
use axum::{
    Form,
    extract::{Path, Query, State},
    response::{IntoResponse, Redirect, Response},
};
use maud::{Markup, html};
use plamenu_db::{discovery_store as db, role::permission};
use serde::Deserialize;

mod transfers;
pub use transfers::{bootstrap, export, import};

const MECHANISMS: [&str; 13] = [
    "source_inspection",
    "peer_exploration",
    "directory",
    "community_directory",
    "outbox",
    "metadata_cache",
    "profile_listing",
    "public_post_indexing",
    "recommendations",
    "peer_sharing",
    "wider_search",
    "video_catalogue",
    "video_indexing",
];
fn error(error: plamenu_db::DbError) -> Response {
    ApiError::from(error).into_response()
}
fn back(notice: &str) -> Response {
    Redirect::to(&format!("/admin/discovery?notice={notice}")).into_response()
}
#[derive(Default, Deserialize)]
pub struct PageQuery {
    notice: Option<String>,
    admitted: Option<usize>,
    existing: Option<usize>,
    refused: Option<usize>,
}

fn checkbox(admin: &WebAdmin, name: &str, label: &str, checked: bool) -> Markup {
    html! {label.admin-check {
        input type="checkbox" name=(name) value="1" checked[checked];
        span {(admin.user.locale.text(label))}
    }}
}

fn mechanisms(admin: &WebAdmin, s: &db::Settings, names: &[&str]) -> Markup {
    html! {div.discovery-options {
        @for name in names {
            (checkbox(admin,name,&format!("discovery-mechanism-{}",name.replace('_',"-")),
                !s.disabled_mechanisms.iter().any(|v|v==name)))
        }
    }}
}

fn controls(admin: &WebAdmin, s: &db::Settings) -> Markup {
    let l = admin.user.locale;
    html! {
        form.discovery-panel.discovery-policy method="post" action="/web/admin/discovery/settings" {
            input type="hidden" name="csrf" value=(admin.user.csrf);
            input type="hidden" name="revision" value=(s.revision);
            h3 {(l.text("discovery-policy"))}
            div.discovery-options {
                (checkbox(admin,"enabled","discovery-enable",s.enabled))
                (checkbox(admin,"paused","discovery-pause",s.paused))
            }
            fieldset {legend {(l.text("discovery-missing-title"))}
                (checkbox(admin,"missing_discoverable","discovery-missing-profile",s.allow_missing_discoverable))
                (checkbox(admin,"missing_indexable","discovery-missing-post",s.allow_missing_indexable))
            }
            details.discovery-details {
                summary {(l.text("discovery-mechanisms"))}
                (mechanisms(admin,s,&["source_inspection","peer_exploration","directory","community_directory","outbox","metadata_cache","profile_listing","public_post_indexing","recommendations","wider_search","video_catalogue","video_indexing"]))
                details.discovery-details {
                    summary {(l.text("discovery-future-title"))}
                    p.settings-field__hint {(l.text("discovery-future-methods"))}
                    (mechanisms(admin,s,&["peer_sharing"]))
                }
            }
            div.discovery-options {
                label {(l.text("discovery-requests")) input type="number" name="requests_per_day" min="1" max="100000" value=(s.requests_per_day);}
                label {(l.text("discovery-retained")) input type="number" name="max_entries" min="1" max="1000000" value=(s.max_entries);}
                label {(l.text("discovery-origin-limit")) input type="number" name="entries_per_origin" min="1" max="1000000" value=(s.entries_per_origin);}
            }
            div.discovery-form-actions {button type="submit" {(l.text("discovery-save"))}}
        }
    }
}

fn add_source(admin: &WebAdmin) -> Markup {
    let l = admin.user.locale;
    html! {
        form.discovery-add method="post" action="/web/admin/discovery/sources" {
            input type="hidden" name="csrf" value=(admin.user.csrf);
            label for="discovery-source-url" {(l.text("discovery-source-url"))}
            div.discovery-add__fields {
                input id="discovery-source-url" type="url" name="url" required maxlength="2048"
                    placeholder="https://example.org" aria-describedby="discovery-add-hint";
                button type="submit" {(l.text("discovery-add-source"))}
            }
            p.settings-field__hint id="discovery-add-hint" {(l.text("discovery-add-hint"))}
        }
    }
}

fn source_card(admin: &WebAdmin, source: &db::Source) -> Markup {
    let l = admin.user.locale;
    let now = time::OffsetDateTime::now_utc();
    let status = if !source.enabled {
        "discovery-source-disabled"
    } else if source.cooldown_until.is_some_and(|v| v > now) {
        "discovery-source-backoff"
    } else if source.valid_until.is_some_and(|v| v > now) {
        "discovery-source-verified"
    } else if source.verified_at.is_some() {
        "discovery-source-expired"
    } else {
        "discovery-source-pending"
    };
    html! {article.discovery-source-row {
        div.discovery-source-row__head {
            div {
                h4 {(crate::web::discovery::server_name(&source.origin))}
                p.discovery-source-row__meta {
                    (l.text(&format!("discovery-provenance-{}",source.provenance)))
                    @if source.depth > 0 {" · " (l.text("discovery-depth")) " " (source.depth)}
                }
            }
            span.discovery-source-status data-state=(status) {(l.text(status))}
        }
        div.discovery-source-row__actions {
            details.discovery-details {
                summary {(l.text("discovery-details"))}
                dl.discovery-diagnostics {
                    dt {(l.text("discovery-endpoint"))} dd {(&source.endpoint)}
                    @if let Some(checked)=source.verified_at {dt {(l.text("discovery-verified"))} dd {(admin.clock().element_absolute(checked))}}
                    @if let Some(checked)=source.peers_verified_at {dt {(l.text("discovery-peers-verified"))} dd {(admin.clock().element_absolute(checked))}}
                    @if let Some(until)=source.cooldown_until {dt {(l.text("discovery-retry"))} dd {(admin.clock().element_absolute(until))}}
                    @if let Some(reason)=&source.last_error {dt {(l.text("discovery-last-error"))} dd {code {(reason)}}}
                    @if let Some(reason)=&source.peer_error {dt {(l.text("discovery-peers-error"))} dd {code {(reason)}}}
                }
            }
            form method="post" action=(format!("/web/admin/discovery/sources/{}",source.id)) {
                input type="hidden" name="csrf" value=(admin.user.csrf);
                input type="hidden" name="enabled" value=(!source.enabled);
                button.secondary type="submit" {(l.text(if source.enabled {"discovery-disable-source"} else {"discovery-enable-source"}))}
            }
        }
    }}
}

fn overview(admin: &WebAdmin, s: &db::Settings, d: &db::Diagnostics) -> Markup {
    let l = admin.user.locale;
    html! {
        dl.discovery-metrics {
            @for (label,value) in [
                ("discovery-sources",format!("{} / {}",d.sources,s.max_sources)),
                ("discovery-cached",format!("{} / {}",d.entries,s.max_entries)),
                ("discovery-queued",d.queued.to_string()),
                ("discovery-requests",format!("{} / {}",d.requests_24h,s.requests_per_day)),
                ("discovery-bytes",format!("{} / {}",crate::entities::human_size(d.charged_bytes_24h),crate::entities::human_size(s.bytes_per_day)))] {
                div {dt {(l.text(label))} dd {(value)}}
            }
        }
        details.discovery-details.discovery-budget-details {
            summary {(l.text("discovery-budget-details"))}
            dl.discovery-diagnostics {
                dt {(l.text("discovery-origin-limit"))} dd {(s.requests_per_origin)}
                @if let Some(until)=d.next_charge_expires_at {dt {(l.text("discovery-budget-release"))} dd {(admin.clock().element_absolute(until))}}
            }
        }
    }
}

pub async fn index(
    State(state): State<AppState>,
    admin: WebAdmin,
    Query(q): Query<PageQuery>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_FEDERATION)?;
    let s = db::settings(&state.pool).await.map_err(error)?;
    let d = db::diagnostics(&state.pool).await.map_err(error)?;
    let sources = db::source_diagnostics(&state.pool).await.map_err(error)?;
    let queue = db::queue_diagnostics(&state.pool).await.map_err(error)?;
    let families = db::family_diagnostics(&state.pool).await.map_err(error)?;
    let inventory = db::recovery_inventory(&state.pool).await.map_err(error)?;
    let metrics = db::recovery_metrics(&state.pool).await.map_err(error)?;
    let l = admin.user.locale;
    let body = html! {
        div.discovery-admin {
        @if let Some(notice)=q.notice.as_deref().filter(|n|["saved","queued","disabled","capacity","invalid","conflict"].contains(n)) {
            p.admin-flash role="status" {(l.text(&format!("discovery-notice-{notice}")))}
        }
        p role="status" {(l.text(crate::discovery::browse::status(&s,&d,&queue)))}
        (overview(&admin,&s,&d))
        @if q.notice.as_deref()==Some("imported") {
            p.admin-flash role="status" {
                (l.text("discovery-import-admitted")) " " (q.admitted.unwrap_or(0)) "; "
                (l.text("discovery-import-existing")) " " (q.existing.unwrap_or(0)) "; "
                (l.text("discovery-import-refused")) " " (q.refused.unwrap_or(0))
            }
        }
        @if !families.is_empty() {
            section.discovery-panel {
                h3 {(l.text("discovery-costs"))}
                div.table-scroll tabindex="0" role="region" aria-label=(l.text("discovery-costs")) { table {
                    thead {tr {th scope="col" {(l.text("discovery-cost-family"))} th scope="col" {(l.text("discovery-requests"))} th scope="col" {(l.text("discovery-cost-bytes"))}}}
                    tbody {@for family in &families {tr {
                        td {(l.text(&format!("discovery-family-{}",family.family))) " · " code {(family.operation)} }
                        td {(family.requests)} td {(family.charged_bytes)}
                    }}}
                }}
            }
        }
        section.discovery-panel {
            h3 {(l.text("discovery-retained"))}
            p {(l.text("discovery-inventory"))}
            @for item in &inventory {p {(item.kind) ": " (item.retained) " / " (item.active)}}
            details.discovery-details {summary {(l.text("discovery-outcomes"))}
                div.table-scroll tabindex="0" {table {
                    thead {tr {th {"UTC"} th {(l.text("discovery-servers"))} th {(l.text("discovery-cost-family"))} th {(l.text("discovery-outcome-purpose"))} th {(l.text("discovery-outcome-event"))} th {"#"}}}
                    tbody {@for metric in &metrics {tr {td {(metric.day)} td {(metric.origin)} td {(metric.kind)} td {(metric.purpose)} td {(metric.event)} td {(metric.total)}}}}
                }}
            }
        }
        (controls(&admin,&s))
        section.discovery-panel {
            h3 {(l.text("discovery-sources"))}
            (add_source(&admin))
            (transfers::controls(&admin))
            div.discovery-source-list {@for source in &sources {(source_card(&admin,source))}}
            @if sources.is_empty() {p.empty {(l.text("discovery-admin-empty"))}}
            @if sources.len() == 200 {p.settings-field__hint {(l.text("discovery-source-limit"))}}
        }
        }
    };
    Ok((
        [("Cache-Control", "private, no-store"), ("Vary", "Cookie")],
        admin_shell(
            &admin,
            "/admin/discovery",
            &l.text("discovery-title"),
            &body,
        ),
    )
        .into_response())
}

#[derive(Deserialize)]
pub struct SettingsForm {
    requests_per_day: Option<i64>,
    max_entries: Option<i64>,
    entries_per_origin: Option<i64>,
    csrf: String,
    revision: i64,
    enabled: Option<String>,
    paused: Option<String>,
    missing_discoverable: Option<String>,
    missing_indexable: Option<String>,
    source_inspection: Option<String>,
    peer_exploration: Option<String>,
    directory: Option<String>,
    community_directory: Option<String>,
    outbox: Option<String>,
    metadata_cache: Option<String>,
    profile_listing: Option<String>,
    public_post_indexing: Option<String>,
    recommendations: Option<String>,
    peer_sharing: Option<String>,
    wider_search: Option<String>,
    video_catalogue: Option<String>,
    video_indexing: Option<String>,
}
pub async fn save(
    State(state): State<AppState>,
    admin: WebAdmin,
    Form(f): Form<SettingsForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_FEDERATION)?;
    if !admin.user.csrf_ok(&f.csrf) {
        return Err(csrf_rejection());
    }
    let mut s = db::settings(&state.pool).await.map_err(error)?;
    if s.revision != f.revision {
        return Ok(back("conflict"));
    }
    s.enabled = f.enabled.is_some();
    s.paused = f.paused.is_some();
    s.allow_missing_discoverable = f.missing_discoverable.is_some();
    s.allow_missing_indexable = f.missing_indexable.is_some();
    if let Some(value) = f.requests_per_day {
        s.requests_per_day = value;
    }
    if let Some(value) = f.max_entries {
        s.max_entries = value;
    }
    if let Some(value) = f.entries_per_origin {
        s.entries_per_origin = value;
    }
    s.disabled_mechanisms = MECHANISMS
        .into_iter()
        .zip([
            f.source_inspection,
            f.peer_exploration,
            f.directory,
            f.community_directory,
            f.outbox,
            f.metadata_cache,
            f.profile_listing,
            f.public_post_indexing,
            f.recommendations,
            f.peer_sharing,
            f.wider_search,
            f.video_catalogue,
            f.video_indexing,
        ])
        .filter(|(_, value)| value.is_none())
        .map(|(name, _)| name.to_owned())
        .collect();
    Ok(back(
        if db::save_settings(&state.pool, &s).await.map_err(error)? {
            "saved"
        } else {
            "conflict"
        },
    ))
}
#[derive(Deserialize)]
pub struct SourceForm {
    csrf: String,
    url: String,
}
pub async fn add(
    State(state): State<AppState>,
    admin: WebAdmin,
    Form(f): Form<SourceForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_FEDERATION)?;
    if !admin.user.csrf_ok(&f.csrf) {
        return Err(csrf_rejection());
    }
    let Ok(endpoint) = crate::discovery::videos::endpoint(&f.url) else {
        return Ok(back("invalid"));
    };
    let admission = db::add_source(&state.pool, &endpoint, None)
        .await
        .map_err(error)?;
    let admission = match admission {
        db::Admission::Added(id) | db::Admission::Existing(id) => db::enqueue(
            &state.pool,
            id,
            &endpoint,
            db::Operation::InspectSource,
            None,
        )
        .await
        .map_err(error)?,
        other => other,
    };
    Ok(back(match admission {
        db::Admission::Added(_) | db::Admission::Existing(_) => "queued",
        db::Admission::Capacity => "capacity",
        _ => "disabled",
    }))
}
#[derive(Deserialize)]
pub struct ToggleForm {
    csrf: String,
    enabled: bool,
}
pub async fn toggle(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(f): Form<ToggleForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_FEDERATION)?;
    if !admin.user.csrf_ok(&f.csrf) {
        return Err(csrf_rejection());
    }
    db::set_source_enabled(&state.pool, id, f.enabled)
        .await
        .map_err(error)?;
    Ok(back("saved"))
}
