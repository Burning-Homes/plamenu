//! The first-party admin dashboard — a server-rendered moderation UI over the
//! Admin data, the browser counterpart to the `/api/v1|v2/admin/*` REST
//! surface.
//!
//! Authentication reuses the ordinary cookie session ([`WebUser`]): the admin
//! is the operator sitting at their own browser, so gating is **role-based
//! only** — the `WebAdmin` extractor loads the signed-in user's moderation
//! role, and each page asserts the specific permission it needs. There is no
//! OAuth-scope check here (the web session carries `read write follow push`,
//! not `admin:*`); that mirrors Mastodon, where the admin web UI is gated by
//! `authorize_with_role` and doorkeeper scopes apply only to API apps.

mod accounts;
mod announcements;
mod appeals;
mod audit_log;
mod custom_emojis;
mod federation_debug;
mod groups;
mod instances;
mod invites;
mod policy;
mod relays;
mod remote_history;
mod reports;
mod roles;
mod rules;
mod settings;
mod tags;
mod terms;
mod trends;
mod username_blocks;
mod warning_presets;
mod webhooks;
mod webxdc;

use axum::Router;
use axum::extract::{DefaultBodyLimit, FromRequestParts, State};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use maud::{Markup, html};
use plamenu_db::role::{Role, permission};
use plamenu_db::{account, instance_settings, job, metrics, reachability, report, software_update};

use super::clock::ViewerClock;
use super::layout;
use super::session::WebUser;
use super::view::{self, icon};
use crate::AppState;

/// A signed-in browser session whose account carries a moderation role. A
/// session without a role is bounced with a 403 page; an anonymous visitor is
/// redirected to sign in (via [`WebUser`]'s own rejection).
pub struct WebAdmin {
    pub user: WebUser,
    pub role: Role,
}

impl WebAdmin {
    /// How this moderator reads a timestamp. The admin console has no settings
    /// context of its own to hang the zone on, so it reads the one the session
    /// extractor already resolved — every admin page renders in the
    /// moderator's zone without a query of its own.
    #[must_use]
    pub fn clock(&self) -> &ViewerClock {
        &self.user.clock
    }

    /// Asserts the signed-in moderator holds `permission`, rendering a 403 page
    /// otherwise. Page handlers call this with `?` before doing any work.
    // The `Err` is an axum `Response` (large by nature); the matching handlers
    // return `Result<Response, Response>` where both variants are that size, so
    // boxing here would only add noise.
    #[allow(clippy::result_large_err)]
    pub fn require(&self, permission: i64) -> Result<(), Response> {
        if self.role.can(permission) {
            Ok(())
        } else {
            Err(forbidden_page(&self.user).into_response())
        }
    }

    /// Asserts the moderator holds at least one of `permissions`.
    #[allow(clippy::result_large_err)]
    pub fn require_any(&self, permissions: &[i64]) -> Result<(), Response> {
        if permissions
            .iter()
            .any(|permission| self.role.can(*permission))
        {
            Ok(())
        } else {
            Err(forbidden_page(&self.user).into_response())
        }
    }
}

impl FromRequestParts<AppState> for WebAdmin {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let user = WebUser::from_request_parts(parts, state)
            .await
            .map_err(IntoResponse::into_response)?;
        // Like the API's `AdminUser`: only a privileged role (anything beyond
        // the everyone baseline the default "User" role carries) opens the
        // dashboard.
        let role = user
            .role
            .clone()
            .filter(Role::privileged)
            .ok_or_else(|| forbidden_page(&user).into_response())?;
        Ok(Self { user, role })
    }
}

/// The dashboard's section tabs, each gated by the permission its pages need.
struct Section {
    href: &'static str,
    label: &'static str,
    permissions: &'static [i64],
}

const SECTIONS: &[Section] = &[
    Section {
        href: "/admin",
        label: "Overview",
        permissions: &[],
    },
    Section {
        href: "/admin/accounts",
        label: "Accounts",
        permissions: &[permission::MANAGE_USERS],
    },
    Section {
        href: "/admin/reports",
        label: "Reports",
        permissions: &[permission::MANAGE_REPORTS],
    },
    Section {
        href: "/admin/appeals",
        label: "Appeals",
        permissions: &[permission::MANAGE_APPEALS],
    },
    Section {
        href: "/admin/webxdc",
        label: "Webxdc",
        permissions: &[permission::MANAGE_WEBXDC],
    },
    Section {
        href: "/admin/groups",
        label: "Groups",
        permissions: &[permission::MANAGE_GROUPS],
    },
    Section {
        href: "/admin/instance-policy",
        label: "Policy",
        permissions: &[permission::MANAGE_FEDERATION, permission::MANAGE_BLOCKS],
    },
    Section {
        href: "/admin/username-blocks",
        label: "Usernames",
        permissions: &[permission::MANAGE_BLOCKS],
    },
    Section {
        href: "/admin/instances",
        label: "Instances",
        permissions: &[permission::MANAGE_FEDERATION],
    },
    Section {
        href: "/admin/relays",
        label: "Relays",
        permissions: &[permission::MANAGE_FEDERATION],
    },
    Section {
        href: "/admin/federation-debug",
        label: "Debug",
        permissions: &[permission::MANAGE_FEDERATION],
    },
    Section {
        href: "/admin/remote-history",
        label: "History",
        permissions: &[permission::MANAGE_FEDERATION],
    },
    Section {
        href: "/admin/trends",
        label: "Trends",
        permissions: &[permission::MANAGE_TAXONOMIES],
    },
    Section {
        href: "/admin/tags",
        label: "Hashtags",
        permissions: &[permission::MANAGE_TAXONOMIES],
    },
    Section {
        href: "/admin/invites",
        label: "Invites",
        permissions: &[permission::MANAGE_INVITES],
    },
    Section {
        href: "/admin/rules",
        label: "Rules",
        permissions: &[permission::MANAGE_RULES],
    },
    Section {
        href: "/admin/announcements",
        label: "Announcements",
        permissions: &[permission::MANAGE_ANNOUNCEMENTS],
    },
    Section {
        href: "/admin/custom-emojis",
        label: "Custom emoji",
        permissions: &[permission::MANAGE_CUSTOM_EMOJIS],
    },
    Section {
        href: "/admin/webhooks",
        label: "Webhooks",
        permissions: &[permission::MANAGE_WEBHOOKS],
    },
    Section {
        href: "/admin/roles",
        label: "Roles",
        permissions: &[permission::MANAGE_ROLES],
    },
    Section {
        href: "/admin/warning-presets",
        label: "Warnings",
        permissions: &[permission::MANAGE_SETTINGS],
    },
    Section {
        href: "/admin/terms-of-service",
        label: "Terms",
        permissions: &[permission::MANAGE_SETTINGS],
    },
    Section {
        href: "/admin/settings",
        label: "Settings",
        permissions: &[permission::MANAGE_SETTINGS],
    },
    Section {
        href: "/admin/audit-log",
        label: "Audit log",
        permissions: &[permission::VIEW_AUDIT_LOG],
    },
];

/// Serde deserializer for query params backed by an HTML `<select>` whose
/// "Any" option submits an empty string. Without it, axum's query deserializer
/// rejects `?account_id=` with "cannot parse integer from empty string" instead
/// of treating the empty value as "no filter". An empty or whitespace-only
/// value becomes `None`; anything else is parsed as `T`.
pub(super) fn empty_as_none<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    use serde::Deserialize;
    match Option::<String>::deserialize(deserializer)?
        .as_deref()
        .map(str::trim)
    {
        None | Some("") => Ok(None),
        Some(value) => value.parse().map(Some).map_err(serde::de::Error::custom),
    }
}

fn can_section(admin: &WebAdmin, section: &Section) -> bool {
    section.permissions.is_empty()
        || section
            .permissions
            .iter()
            .any(|permission| admin.role.can(*permission))
}

/// Wraps admin page `body` in the shared web chrome plus the dashboard's own
/// section nav (mirrors `settings::settings_shell`). Sections the moderator
/// cannot reach are hidden.
pub(super) fn admin_shell(admin: &WebAdmin, current: &str, title: &str, body: &Markup) -> Markup {
    let content = html! {
        section.column.admin {
            header.admin__head {
                h1 { (icon("shield")) " Administration" }
            }
            @let tabs: Vec<view::Tab> = SECTIONS
                .iter()
                .filter(|section| can_section(admin, section))
                .map(|section| view::Tab::new(section.href, section.label, current == section.href))
                .collect();
            (view::tab_strip("Admin sections", &tabs))
            div.admin__body {
                h2.admin__title { (title) }
                (body)
            }
        }
    };
    layout::shell(title, Some(&admin.user), &content)
}

/// The standard admin flash banner shown after a redirect: `applied` renders
/// a "Done." confirmation, `error` renders the page's own `error` message.
/// Pages with extra flash codes keep a local wrapper that matches those codes
/// first and falls through to this helper.
fn flash_banner(flash: Option<&str>, error: &str) -> Markup {
    html! {
        @match flash {
            Some("applied") => p.admin-flash role="status" { "Done." }
            Some("error") => p.admin-flash.is-error role="alert" { (error) }
            _ => {}
        }
    }
}

/// The 403 page shown when a signed-in user lacks the permission a section
/// needs (or holds no moderation role at all).
fn forbidden_page(user: &WebUser) -> impl IntoResponse {
    let content = html! {
        section.column.admin {
            div.admin__body {
                h1.admin__title { "Access denied" }
                p { "Your account does not have permission to view this page." }
                p { a href="/" { "Return home" } }
            }
        }
    };
    (
        axum::http::StatusCode::FORBIDDEN,
        layout::shell("Access denied", Some(user), &content),
    )
}

/// `GET /admin` — the dashboard overview: at-a-glance figures plus, for
/// holders of `VIEW_DASHBOARD`, the trailing-30-day measures and sign-up
/// dimensions from the metrics queries (the web face of
/// `/api/v1/admin/{measures,dimensions}`). Section navigation lives in the
/// shared tab strip; the stat cards themselves link into the moderation queue
/// they summarise (local accounts, pending review, open reports).
#[allow(clippy::too_many_lines, reason = "one linear dashboard assembly")]
async fn overview(State(state): State<AppState>, admin: WebAdmin) -> Markup {
    let accounts = account::count_local(&state.pool).await.unwrap_or(0);
    // The accounts list this card links into requires `manage_users`; gate both
    // the link and the pending-review count on it so no card leads to a 403.
    let can_manage_users = admin.role.can(permission::MANAGE_USERS);
    let pending = if can_manage_users {
        account::count_local_pending(&state.pool).await.unwrap_or(0)
    } else {
        0
    };
    // Only surface the moderation queue to those who can work it.
    let open_reports = if admin.role.can(permission::MANAGE_REPORTS) {
        report::count_unresolved(&state.pool).await.ok()
    } else {
        None
    };
    let dashboard = if admin.role.can(permission::VIEW_DASHBOARD) {
        dashboard_metrics(&state).await
    } else {
        None
    };
    // The software-update banner mirrors Mastodon's, gated by `view_devops`.
    let updates = if admin.role.can(permission::VIEW_DEVOPS) {
        software_update::pending(&state.pool)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    // The delivery-queue + circuit-breaker roll-up (O1/O2), for whoever can
    // work the instances pages the card links into.
    let federation = if admin.role.can(permission::MANAGE_FEDERATION) {
        federation_summary(&state).await
    } else {
        None
    };
    // The auto-close-registrations notice (O3), for whoever can change the
    // mode back; saving the settings form clears the stamp.
    let auto_closed = if admin.role.can(permission::MANAGE_SETTINGS) {
        instance_settings::get(&state.pool)
            .await
            .ok()
            .and_then(|settings| settings.registrations_auto_closed_at)
    } else {
        None
    };
    // A7: the self-destruct wind-down report. The trigger is CLI-only by
    // design (Mastodon parity — no button anywhere); once armed, every staff
    // member sees how far along the notice fan-out is.
    let clock = &admin.user.clock;
    // Retention-sweep observability, for the ops audience.
    let retention = if admin.role.can(permission::VIEW_DEVOPS) {
        retention_summary(&state, clock).await
    } else {
        None
    };
    let self_destruct = match state.settings_cache.get(&state.pool).await {
        Ok(settings) if settings.is_self_destructing() => crate::self_destruct::progress(&state)
            .await
            .ok()
            .map(|progress| (settings.self_destruct_initiated_at, progress)),
        _ => None,
    };
    let body = html! {
        @if let Some((initiated_at, progress)) = self_destruct {
            div.admin-update.admin-update--urgent {
                span.admin-update__label {
                    "This server is winding down (self-destruct armed"
                    @if let Some(at) = initiated_at {
                        " on " (clock.element(at))
                    }
                    "): "
                    (progress.pending_accounts) " account deletion notice(s) and "
                    (progress.pending_deliveries) " delivery(ies) still pending. "
                    "When both reach zero the remaining data can be dropped."
                }
            }
        }
        (software_update_banner(&updates))
        @if let Some(closed_at) = auto_closed {
            div.admin-update {
                span.admin-update__label {
                    "Open registration was switched to approval mode on "
                    (clock.element(closed_at))
                    " because no moderator had been active for a week."
                }
                a.admin-update__link href="/admin/settings?open=registrations" {
                    "Registration settings"
                }
            }
        }
        p.admin__lead { "Moderation and server administration for this instance." }
        div.admin__stats {
            @if can_manage_users {
                a.admin-stat.admin-stat--link href="/admin/accounts?origin=local" {
                    span.admin-stat__value { (accounts) }
                    span.admin-stat__label { "Local accounts" }
                }
            } @else {
                div.admin-stat {
                    span.admin-stat__value { (accounts) }
                    span.admin-stat__label { "Local accounts" }
                }
            }
            // Only when something is actually waiting — an empty queue (the
            // norm outside approval-required mode) shows no card.
            @if pending > 0 {
                a.admin-stat.admin-stat--link href="/admin/accounts?origin=local&status=pending" {
                    span.admin-stat__value { (pending) }
                    span.admin-stat__label { "Pending review" }
                }
            }
            @if let Some(open) = open_reports {
                a.admin-stat.admin-stat--link href="/admin/reports" {
                    span.admin-stat__value { (open) }
                    span.admin-stat__label { "Open reports" }
                }
            }
        }
        @if let Some(federation) = federation {
            (federation)
        }
        @if let Some(retention) = retention {
            (retention)
        }
        @if let Some(dashboard) = dashboard {
            (dashboard)
        }
    };
    admin_shell(&admin, "/admin", "Overview", &body)
}

/// The retention-sweep observability card: when each retention
/// sweep last completed and what it did — read from the durable
/// `retention_sweeps` table, so the answer survives log rotation — plus the
/// age of the oldest object each sweep is still retaining. Best-effort like
/// the other cards: a query failure drops the section, and a sweep that has
/// never completed is simply absent (itself a signal on a long-running
/// instance).
async fn retention_summary(state: &AppState, clock: &super::clock::ViewerClock) -> Option<Markup> {
    let statuses = plamenu_db::retention_sweep::statuses(&state.pool)
        .await
        .ok()?;
    if statuses.is_empty() {
        return None;
    }
    let archive_oldest = plamenu_db::archive::oldest_age_seconds(&state.pool)
        .await
        .ok()
        .flatten();
    let import_oldest = plamenu_db::bulk_import::oldest_age_seconds(&state.pool)
        .await
        .ok()
        .flatten();
    let oldest_for = |name: &str| match name {
        "account archives" => archive_oldest,
        "bulk imports" => import_oldest,
        _ => None,
    };
    Some(html! {
        h3.admin-dashboard__heading { "Retention sweeps" }
        div.admin__stats {
            @for sweep in &statuses {
                div.admin-stat {
                    span.admin-stat__value { (sweep.last_swept) }
                    span.admin-stat__label {
                        (sweep.name) " swept — last ran " (clock.element(sweep.last_success_at))
                    }
                    @if sweep.last_retained > 0 {
                        span.admin-stat__delta {
                            (sweep.last_retained) " kept to retry a failed file delete"
                        }
                    }
                    @if let Some(age) = oldest_for(&sweep.name) {
                        span.admin-stat__delta {
                            "oldest retained: "
                            // Clamped through u32 (~136 years in seconds), so
                            // the f64 conversion is lossless.
                            (human_secs(f64::from(
                                u32::try_from(age.max(0)).unwrap_or(u32::MAX)
                            )))
                        }
                    }
                }
            }
        }
    })
}

/// The at-a-glance federation-delivery card: outbound queue totals and the
/// circuit breaker's unreachable-host count (the web face of `plamenu
/// federation queue inspect` / `federation reachability`). The detail tables
/// live on the instances page the card links to. Best-effort like the
/// metrics: a query failure drops the section.
async fn federation_summary(state: &AppState) -> Option<Markup> {
    let queued = job::pending_count(&state.pool).await.ok()?;
    let due = job::due_count(&state.pool).await.ok()?;
    let next = job::next_due_delay(&state.pool).await.ok()?;
    let unreachable = reachability::unreachable_hosts(&state.pool).await.ok()?;
    Some(html! {
        h3.admin-dashboard__heading { "Federation delivery" }
        div.admin__stats {
            a.admin-stat.admin-stat--link href="/admin/instances#delivery-health" {
                span.admin-stat__value { (queued) }
                span.admin-stat__label { "Queued deliveries" }
                @match next {
                    Some(delay) if delay.is_zero() => {
                        span.admin-stat__delta { "next due now" }
                    }
                    Some(delay) => {
                        span.admin-stat__delta { "next due in " (human_secs(delay.as_secs_f64())) }
                    }
                    None => {}
                }
            }
            div.admin-stat {
                span.admin-stat__value { (due) }
                span.admin-stat__label { "Due now" }
            }
            a.admin-stat.admin-stat--link href="/admin/instances#delivery-health" {
                span.admin-stat__value { (unreachable.len()) }
                span.admin-stat__label { "Unreachable hosts" }
            }
        }
    })
}

/// A compact human duration for the delivery readouts (mirrors the debug
/// CLI's formatting).
pub(super) fn human_secs(secs: f64) -> String {
    let secs = secs.max(0.0);
    if secs < 60.0 {
        format!("{secs:.0}s")
    } else if secs < 3600.0 {
        format!("{:.0}m", secs / 60.0)
    } else {
        format!("{:.1}h", secs / 3600.0)
    }
}

/// The software-update notice: nothing when the install is current or
/// the check is disabled, else a banner naming the newest available version. An
/// `urgent` (security) release styles the banner as a warning, matching
/// Mastodon's dashboard update notice.
fn software_update_banner(updates: &[software_update::SoftwareUpdate]) -> Markup {
    let Some(newest) = updates.first() else {
        return html! {};
    };
    let urgent = updates.iter().any(|u| u.urgent);
    let class = if urgent {
        "admin-update admin-update--urgent"
    } else {
        "admin-update"
    };
    html! {
        div.(class) {
            span.admin-update__label {
                @if urgent { "Security update available: " } @else { "Update available: " }
                "version " (newest.version)
            }
            @if !newest.release_notes.is_empty() {
                a.admin-update__link href=(newest.release_notes) rel="noopener" { "Release notes" }
            }
        }
    }
}

/// A measure tile: headline total for the window plus the change against the
/// previous window, in plain ink (a rise in "reports opened" is not "good").
fn measure_tile(label: &str, measure: &metrics::Measurement) -> Markup {
    let delta = measure.previous_total.map(|previous| {
        let diff = measure.total - previous;
        match (diff, previous) {
            (0, _) => "no change".to_owned(),
            (d, 0) => format!("{d:+} vs previous 30 days"),
            (d, p) => {
                #[allow(clippy::cast_precision_loss)]
                let pct = (d as f64 / p as f64) * 100.0;
                format!("{d:+} ({pct:+.0}%) vs previous 30 days")
            }
        }
    });
    html! {
        div.admin-stat {
            span.admin-stat__value { (measure.total) }
            span.admin-stat__label { (label) }
            @if let Some(delta) = delta {
                span.admin-stat__delta { (delta) }
            }
        }
    }
}

/// A dimension table: top rows for the window (sign-up sources, languages).
fn dimension_table(title: &str, rows: &[(String, i64)]) -> Markup {
    html! {
        section.admin-dimension {
            h3 { (title) }
            @if rows.is_empty() {
                p.empty { "No data for this period." }
            } @else {
                (crate::web::view::data_table(&html! {
                    tbody {
                        @for (name, value) in rows {
                            tr {
                                td { (name) }
                                td.admin-dimension__value { (value) }
                            }
                        }
                    }
                }))
            }
        }
    }
}

/// The trailing-30-day measure tiles and dimension tables. Metric queries are
/// best-effort: a failure drops the section rather than failing the page.
async fn dashboard_metrics(state: &AppState) -> Option<Markup> {
    let end = time::OffsetDateTime::now_utc();
    let start = end - time::Duration::days(30);

    let new_users = metrics::new_users(&state.pool, start, end).await.ok()?;
    let active_users = metrics::active_users(&state.pool, start, end).await.ok()?;
    let interactions = metrics::interactions(&state.pool, start, end).await.ok()?;
    let opened = metrics::opened_reports(&state.pool, start, end)
        .await
        .ok()?;
    let resolved = metrics::resolved_reports(&state.pool, start, end)
        .await
        .ok()?;
    let sources = metrics::dim_sources(&state.pool, start, end, Some(8))
        .await
        .ok()?;
    let languages = metrics::dim_languages(&state.pool, start, end, Some(8))
        .await
        .ok()?;
    let (earliest, latest) = crate::routes::admin_metrics::snowflake_range(start, end);
    let servers = metrics::dim_servers(&state.pool, earliest, latest, Some(8))
        .await
        .ok()?;
    // Six monthly sign-up cohorts ending this month (the API's `retention`
    // endpoint with `frequency=month`).
    let retention = metrics::retention(
        &state.pool,
        (end - time::Duration::days(150)).date(),
        end.date(),
        "month",
    )
    .await
    .ok()?;
    let server = server_section(state).await?;

    let source_rows: Vec<(String, i64)> = sources
        .iter()
        .map(|row| {
            let name = match row.key.as_deref() {
                Some(name) if !name.is_empty() => name.to_owned(),
                _ => "Website".to_owned(),
            };
            (name, row.value)
        })
        .collect();
    let language_rows: Vec<(String, i64)> = languages
        .iter()
        .map(|row| {
            let code = row.key.as_deref().unwrap_or("und");
            (crate::entities::locale_name(code), row.value)
        })
        .collect();
    // A NULL domain is this server itself (local statuses).
    let server_rows: Vec<(String, i64)> = servers
        .iter()
        .map(|row| {
            let domain = row
                .key
                .clone()
                .unwrap_or_else(|| state.config.domain.clone());
            (domain, row.value)
        })
        .collect();
    Some(html! {
        h3.admin-dashboard__heading { "Last 30 days" }
        // Deliberately *not* the admin's zone (TZ §4.4): UTC days keep the
        // series identical for every moderator and match what
        // `/api/v1/admin/measures` reports. Saying so beats silently
        // disagreeing with the timestamps elsewhere on the page.
        p.admin__lead { "Daily figures are counted in UTC days." }
        div.admin__stats {
            (measure_tile("New users", &new_users))
            (measure_tile("Active users", &active_users))
            (measure_tile("Interactions", &interactions))
            (measure_tile("Reports opened", &opened))
            (measure_tile("Reports resolved", &resolved))
        }
        div.admin-dashboard__dimensions {
            (dimension_table("Sign-up sources", &source_rows))
            (dimension_table("Top languages", &language_rows))
            (dimension_table("Most active servers", &server_rows))
        }
        (retention_table(&retention))
        (server)
    })
}

/// The storage and software-version cards (the API's `space_usage` and
/// `software_versions` dimensions).
async fn server_section(state: &AppState) -> Option<Markup> {
    let db_size = metrics::pg_database_size(&state.pool).await.ok()?;
    let media_bytes = metrics::media_storage_bytes(&state.pool).await.ok()?;
    let pg_version = metrics::pg_version(&state.pool).await.ok();
    let ffmpeg_version = crate::routes::admin_metrics::ffmpeg_version(state).await;
    let build = crate::BUILD_INFO;
    let versions: Vec<(&str, String)> = [
        ("Plamenu", Some(build.version.to_owned())),
        (
            "Build",
            Some(build.build_number.unwrap_or("local").to_owned()),
        ),
        ("Revision", Some(build.revision.to_owned())),
        ("Channel", Some(build.channel.as_str().to_owned())),
        ("Profile", Some(build.profile.to_owned())),
        ("Target", Some(build.target.to_owned())),
        ("Architecture", Some(build.architecture.to_owned())),
        (
            "Debug assertions",
            Some(
                if build.debug_assertions {
                    "enabled"
                } else {
                    "disabled"
                }
                .to_owned(),
            ),
        ),
        (
            "Dirty source",
            Some(if build.dirty { "yes" } else { "no" }.to_owned()),
        ),
        ("PostgreSQL", pg_version),
        ("FFmpeg", ffmpeg_version),
    ]
    .into_iter()
    .filter_map(|(name, version)| version.map(|v| (name, v)))
    .collect();
    // Build/version metadata reads as key–value text, not headline stat tiles:
    // a git revision, a target triple ("x86_64-unknown-linux-musl") or a bare
    // "yes"/"no" looks broken rendered as a big number.
    Some(html! {
        h3.admin-dashboard__heading { "Server" }
        dl.admin-meta {
            div.admin-meta__row {
                dt { "Database size" }
                dd { (crate::entities::human_size(db_size)) }
            }
            div.admin-meta__row {
                dt { "Media storage" }
                dd { (crate::entities::human_size(media_bytes)) }
            }
            @for (name, version) in &versions {
                div.admin-meta__row {
                    dt { (name) }
                    dd { (version) }
                }
            }
        }
    })
}

/// The monthly retention cohorts as a table: one row per sign-up month, one
/// column per month since sign-up, each cell the share of that cohort still
/// signing in (raw user count in the cell's tooltip).
fn retention_table(cells: &[metrics::RetentionCell]) -> Markup {
    // Cells arrive ordered by cohort then period; group into rows.
    let mut cohorts: Vec<(time::Date, Vec<&metrics::RetentionCell>)> = Vec::new();
    for cell in cells {
        match cohorts.last_mut() {
            Some((period, bucket)) if *period == cell.cohort_period => bucket.push(cell),
            _ => cohorts.push((cell.cohort_period, vec![cell])),
        }
    }
    let max_periods = cohorts.iter().map(|(_, b)| b.len()).max().unwrap_or(0);
    html! {
        section.admin-dimension {
            h3 { "Retention by sign-up month" }
            @if cohorts.is_empty() {
                p.empty { "No data for this period." }
            } @else {
                // Seven nowrap columns can't fit a phone: the table scrolls
                // inside its own box instead of crushing the cohort labels.
                (crate::web::view::data_table(&html! {
                    thead {
                        tr {
                            th.is-tight scope="col" { "Signed up" }
                            @for offset in 0..max_periods {
                                th.is-tight scope="col" { "+" (offset) " mo" }
                            }
                        }
                    }
                    tbody {
                        @for (period, bucket) in &cohorts {
                            tr {
                                td.is-tight { (format!("{}-{:02}", period.year(), u8::from(period.month()))) }
                                @for cell in bucket {
                                    td.is-tight.admin-dimension__value
                                        title=(format!("{} user(s)", cell.value)) {
                                        (format!("{:.0}%", cell.rate * 100.0))
                                    }
                                }
                                @for _ in bucket.len()..max_periods {
                                    td.is-tight {}
                                }
                            }
                        }
                    }
                }))
            }
        }
    }
}

/// The admin dashboard routes, merged into the web router. Page reads live
/// under `/admin/*`; state-changing posts under `/web/admin/*`, matching the
/// rest of the web UI's convention.
pub fn router() -> Router<AppState> {
    Router::new()
        .merge(moderation_router())
        .merge(server_ops_router())
}

/// Moderation surfaces: accounts, reports and instance policy.
#[allow(clippy::too_many_lines)] // a flat route table, one line per endpoint
fn moderation_router() -> Router<AppState> {
    Router::new()
        .route("/admin", get(overview))
        .route("/admin/accounts", get(accounts::index))
        .route("/admin/accounts/{id}", get(accounts::show))
        .route(
            "/web/admin/accounts/bulk/confirm",
            post(accounts::bulk_confirm),
        )
        .route(
            "/web/admin/accounts/bulk/reject",
            post(accounts::bulk_reject),
        )
        .route("/web/admin/accounts/{id}/action", post(accounts::action))
        .route("/web/admin/accounts/{id}/op", post(accounts::op))
        .route("/web/admin/accounts/{id}/user-op", post(accounts::user_op))
        .route("/web/admin/accounts/{id}/role", post(accounts::set_role))
        .route("/web/admin/accounts/{id}/note", post(accounts::add_note))
        .route(
            "/web/admin/accounts/{id}/note/{note_id}/delete",
            post(accounts::delete_note),
        )
        .route("/admin/webxdc", get(webxdc::index))
        .route("/admin/webxdc/{id}", get(webxdc::show))
        .route("/web/admin/webxdc/{id}/op", post(webxdc::op))
        .route("/admin/groups", get(groups::index))
        .route("/admin/groups/{id}", get(groups::show))
        .route("/web/admin/groups/{id}/update", post(groups::update))
        .route("/web/admin/groups/{id}/transfer", post(groups::transfer))
        .route("/web/admin/groups/{id}/op", post(groups::op))
        .route("/admin/reports", get(reports::index))
        .route("/admin/reports/{id}", get(reports::show))
        .route("/web/admin/reports/{id}/update", post(reports::update))
        .route("/web/admin/reports/{id}/op", post(reports::op))
        .route("/web/admin/reports/{id}/note", post(reports::add_note))
        .route(
            "/web/admin/reports/{id}/note/{note_id}/delete",
            post(reports::delete_note),
        )
        .route("/admin/instance-policy", get(policy::index))
        .route(
            "/web/admin/instance-policy/domain-blocks",
            post(policy::create_domain_block),
        )
        .route(
            "/web/admin/instance-policy/domain-blocks/{id}/update",
            post(policy::update_domain_block),
        )
        .route(
            "/web/admin/instance-policy/domain-blocks/{id}/delete",
            post(policy::delete_domain_block),
        )
        .route(
            "/web/admin/instance-policy/domain-allows",
            post(policy::create_domain_allow),
        )
        .route(
            "/web/admin/instance-policy/domain-allows/{id}/delete",
            post(policy::delete_domain_allow),
        )
        .route(
            "/web/admin/instance-policy/email-domain-blocks",
            post(policy::create_email_block),
        )
        .route(
            "/web/admin/instance-policy/email-domain-blocks/{id}/delete",
            post(policy::delete_email_block),
        )
        .route(
            "/web/admin/instance-policy/ip-blocks",
            post(policy::create_ip_block),
        )
        .route(
            "/web/admin/instance-policy/ip-blocks/{id}/update",
            post(policy::update_ip_block),
        )
        .route(
            "/web/admin/instance-policy/ip-blocks/{id}/delete",
            post(policy::delete_ip_block),
        )
        .route(
            "/web/admin/instance-policy/canonical-email-blocks",
            post(policy::create_canonical_email_block),
        )
        .route(
            "/web/admin/instance-policy/canonical-email-blocks/{id}/delete",
            post(policy::delete_canonical_email_block),
        )
        .route("/admin/appeals", get(appeals::index))
        .route("/web/admin/appeals/{id}/approve", post(appeals::approve))
        .route("/web/admin/appeals/{id}/reject", post(appeals::reject))
        .route("/admin/audit-log", get(audit_log::index))
        .route("/admin/instances", get(instances::index))
        .route("/admin/instances/{domain}", get(instances::show))
        .route("/admin/username-blocks", get(username_blocks::index))
        .route("/web/admin/username-blocks", post(username_blocks::create))
        .route(
            "/web/admin/username-blocks/{id}/update",
            post(username_blocks::update),
        )
        .route(
            "/web/admin/username-blocks/{id}/delete",
            post(username_blocks::delete),
        )
        .route("/admin/warning-presets", get(warning_presets::index))
        .route("/web/admin/warning-presets", post(warning_presets::create))
        .route(
            "/web/admin/warning-presets/{id}/update",
            post(warning_presets::update),
        )
        .route(
            "/web/admin/warning-presets/{id}/delete",
            post(warning_presets::delete),
        )
        .route("/admin/relays", get(relays::index))
        .route("/admin/federation-debug", get(federation_debug::index))
        .route("/admin/remote-history", get(remote_history::index))
        .route("/web/admin/remote-history", post(remote_history::save))
        .route(
            "/web/admin/remote-history/prune",
            post(remote_history::prune),
        )
        .route("/web/admin/relays", post(relays::create))
        .route("/web/admin/relays/{id}/op", post(relays::op))
        .route("/web/admin/relays/{id}/delete", post(relays::delete))
}

/// Server-ops surfaces: rules, announcements, emoji, webhooks and settings.
#[allow(clippy::too_many_lines, reason = "flat route registry")]
fn server_ops_router() -> Router<AppState> {
    Router::new()
        .route("/admin/trends", get(trends::index))
        .route("/web/admin/trends/{kind}/{id}/review", post(trends::review))
        .route("/admin/tags", get(tags::index))
        .route("/web/admin/tags/{id}/update", post(tags::update))
        .route("/admin/invites", get(invites::index))
        .route("/web/admin/invites/{id}/expire", post(invites::expire))
        .route("/admin/roles", get(roles::index))
        .route("/admin/roles/new", get(roles::new))
        .route("/admin/roles/{id}", get(roles::show))
        .route("/web/admin/roles", post(roles::create))
        .route("/web/admin/roles/{id}/update", post(roles::update))
        .route("/web/admin/roles/{id}/delete", post(roles::delete))
        .route("/admin/terms-of-service", get(terms::index))
        .route("/web/admin/terms-of-service", post(terms::save))
        .route("/admin/rules", get(rules::index))
        .route("/web/admin/rules", post(rules::create))
        .route("/web/admin/rules/{id}/update", post(rules::update))
        .route("/web/admin/rules/{id}/delete", post(rules::delete))
        .route("/admin/announcements", get(announcements::index))
        .route("/web/admin/announcements", post(announcements::create))
        .route(
            "/web/admin/announcements/{id}/update",
            post(announcements::update),
        )
        .route("/web/admin/announcements/{id}/op", post(announcements::op))
        .route(
            "/web/admin/announcements/{id}/delete",
            post(announcements::delete),
        )
        .route("/admin/custom-emojis", get(custom_emojis::index))
        .route(
            "/admin/custom-emojis/trending",
            get(custom_emojis::trending),
        )
        .route(
            "/admin/custom-emojis/users/{id}",
            get(custom_emojis::user_emojis),
        )
        .route(
            "/admin/custom-emojis/borrow/account/{id}",
            get(custom_emojis::borrow_account_index),
        )
        .route(
            "/admin/custom-emojis/borrow/status/{id}",
            get(custom_emojis::borrow_status_index),
        )
        .route(
            "/web/admin/custom-emojis",
            post(custom_emojis::create).layer(DefaultBodyLimit::max(
                crate::media_processing::HARD_MAX_EMOJI_BYTES + 64 * 1024,
            )),
        )
        .route(
            "/web/admin/custom-emojis/{id}/moderate",
            post(custom_emojis::moderate_personal),
        )
        .route(
            "/web/admin/custom-emojis/{id}/retire",
            post(custom_emojis::retire_personal),
        )
        .route(
            "/web/admin/custom-emojis/{id}/promote",
            post(custom_emojis::promote_personal),
        )
        .route(
            "/web/admin/custom-emojis/borrow",
            post(custom_emojis::borrow),
        )
        .route(
            "/web/admin/custom-emojis/{id}/update",
            post(custom_emojis::update),
        )
        .route(
            "/web/admin/custom-emojis/{id}/delete",
            post(custom_emojis::delete),
        )
        .route("/admin/webhooks", get(webhooks::index))
        .route("/web/admin/webhooks", post(webhooks::create))
        .route("/web/admin/webhooks/{id}/update", post(webhooks::update))
        .route("/web/admin/webhooks/{id}/op", post(webhooks::op))
        .route("/web/admin/webhooks/{id}/delete", post(webhooks::delete))
        .route("/admin/settings", get(settings::index))
        .route("/web/admin/settings", post(settings::save))
        .route(
            "/web/admin/settings/test-email",
            post(settings::send_test_email),
        )
        .route(
            "/web/admin/settings/backfill-sizes",
            post(settings::backfill_sizes),
        )
        .route(
            "/web/admin/site-uploads/{var}",
            post(settings::upload_site_image),
        )
        .route(
            "/web/admin/site-uploads/{var}/description",
            post(settings::describe_site_image),
        )
        .route(
            "/web/admin/site-uploads/{var}/delete",
            post(settings::delete_site_image),
        )
}
