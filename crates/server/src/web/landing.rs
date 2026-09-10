//! The instance's public face: the welcome page a signed-out visitor gets at
//! `/`, and the always-public `/rules` and `/staff` pages the left menu (and
//! the sign-up agreement) point at.
//!
//! The landing page keeps to a compact summary — name, tagline, a stats block
//! and the operator's extended description — while rules, discoverable
//! profiles and the staff list live on their own pages. It only surfaces what
//! is already public elsewhere (the instance API's description and stats) and
//! never widens the search/timeline preview posture, which keeps its own
//! config flags.

use axum::extract::State;
use axum::response::{IntoResponse, Redirect, Response};
use fluent_bundle::FluentArgs;
use maud::{Markup, PreEscaped, html};
use plamenu_db::{account, relay, role, rule, status, terms_of_service};
use serde_json::{Value, json};

use super::i18n::Locale;
use super::pages::anon_nav;
use super::session::MaybeWebUser;
use super::{layout, view};
use crate::entities::render_accounts_by_ids;
use crate::error::ApiError;
use crate::state::AppState;

/// The instance counters shown in the stats block, cached in the shared
/// metrics TTL cache (5 minutes) — full-table aggregates have no business
/// running per anonymous page view.
#[derive(Clone, Copy, Default)]
struct LandingStats {
    local_users: u64,
    local_bots: u64,
    local_posts: u64,
    remote_users: u64,
    remote_servers: u64,
    remote_posts: u64,
    /// `(connected relays, activities received through them in the trailing
    /// week)` — `None` while no relay subscription is accepted and enabled.
    relays: Option<(u64, u64)>,
}

async fn cached_stats(state: &AppState) -> Result<LandingStats, ApiError> {
    const KEY: &str = "landing/stats";
    let unpack = |v: &Value, key: &str| v.get(key).and_then(Value::as_u64).unwrap_or(0);
    let from_json = |v: &Value| LandingStats {
        local_users: unpack(v, "local_users"),
        local_bots: unpack(v, "local_bots"),
        local_posts: unpack(v, "local_posts"),
        remote_users: unpack(v, "remote_users"),
        remote_servers: unpack(v, "remote_servers"),
        remote_posts: unpack(v, "remote_posts"),
        relays: (unpack(v, "relays") > 0).then(|| (unpack(v, "relays"), unpack(v, "relay_week"))),
    };
    if let Some(cached) = state.metrics_cache.get(KEY) {
        return Ok(from_json(&cached));
    }

    let (local_users, local_bots) = account::count_local_people_and_bots(&state.pool).await?;
    let relays: Vec<_> = relay::list(&state.pool)
        .await?
        .into_iter()
        .filter(|relay| relay.state == "accepted")
        .map(|relay| relay.id)
        .collect();
    let relay_week: i64 = if relays.is_empty() {
        0
    } else {
        relay::activity_totals(&state.pool)
            .await?
            .into_iter()
            .filter(|(id, _)| relays.contains(id))
            .map(|(_, activity)| activity.last_week)
            .sum()
    };
    let payload = json!({
        "local_users": local_users,
        "local_bots": local_bots,
        "local_posts": status::count_local(&state.pool).await?,
        "remote_users": account::count_remote(&state.pool).await?,
        "remote_servers": account::count_known_domains(&state.pool).await?,
        "remote_posts": status::count_remote(&state.pool).await?,
        "relays": relays.len(),
        "relay_week": u64::try_from(relay_week).unwrap_or(0),
    });
    let unpacked = from_json(&payload);
    state.metrics_cache.put(KEY.to_owned(), payload);
    Ok(unpacked)
}

/// Everything the welcome page shows, gathered up front so the markup stays
/// a straight render.
struct LandingPage<'a> {
    title: String,
    domain: &'a str,
    tagline: &'a str,
    signup_open: bool,
    stats: Option<LandingStats>,
    /// The extended description, already rendered from markdown.
    about: Option<String>,
}

/// The `/` welcome page for signed-out visitors (signed-in ones get their
/// home timeline from `pages::home` before this is consulted). With the
/// landing page switched off, the previous behaviour — straight to the
/// sign-in form — is restored.
pub async fn landing(state: &AppState, locale: Locale) -> Result<Response, ApiError> {
    let settings = state.settings_cache.get(&state.pool).await?;
    if !settings.landing_page {
        return Ok(Redirect::to("/login").into_response());
    }
    let domain = &state.config.account_domain;
    let signup_open = crate::registration::open_for_registrations(state).await?;
    let counters = if settings.landing_show_stats {
        Some(cached_stats(state).await?)
    } else {
        None
    };

    let page = LandingPage {
        title: if settings.site_title.is_empty() {
            domain.clone()
        } else {
            settings.site_title.clone()
        },
        domain,
        tagline: &settings.site_short_description,
        signup_open,
        stats: counters,
        about: (!settings.site_extended_description.is_empty())
            .then(|| crate::routes::api::markdown_html(&settings.site_extended_description)),
    };
    let content = landing_markup(&page, locale);
    let meta = super::meta::instance_page(state, "/", locale).await?;
    Ok(layout::shell_visitor_subject_localized(
        &page.title,
        None,
        anon_nav(state).await,
        &content,
        &meta,
        locale,
    )
    .into_response())
}

fn landing_markup(page: &LandingPage, locale: Locale) -> Markup {
    html! {
        section.landing {
            header.landing__hero {
                h1.landing__title { (page.title) }
                p.landing__domain { (page.domain) }
                @if !page.tagline.is_empty() {
                    p.landing__tagline { (page.tagline) }
                }
                div.landing__cta {
                    @if page.signup_open {
                        a.pill-button.landing__cta-primary href="/signup" {
                            (locale.text("landing-create-account"))
                        }
                    }
                    a.pill-button href="/login" { (locale.text("nav-sign-in")) }
                }
            }
            @if let Some(stats) = page.stats {
                (stats_block(stats, locale))
            }
            @if let Some(about) = &page.about {
                // The operator's extended description, deliberately untitled:
                // it introduces the server in its own words.
                section.landing__section.landing__about { (PreEscaped(about)) }
            }
            footer.landing__footer {
                a href=(crate::SOURCE_URL) { "Plamenu " (crate::VERSION) }
            }
        }
    }
}

/// The counters as three compact labelled lines — this server, the wider
/// network, and (only while any is connected) the relay subscriptions.
fn stats_block(stats: LandingStats, locale: Locale) -> Markup {
    html! {
        div.landing__stats {
            (stat_row("landing-stats-server", &[
                (stats.local_users, "landing-stat-users"),
                (stats.local_bots, "landing-stat-bots"),
                (stats.local_posts, "landing-stat-posts"),
            ], locale))
            (stat_row("landing-stats-fediverse", &[
                (stats.remote_users, "landing-stat-users"),
                (stats.remote_servers, "landing-stat-servers"),
                (stats.remote_posts, "landing-stat-posts"),
            ], locale))
            @if let Some((connected, week)) = stats.relays {
                (stat_row("landing-stats-relays", &[
                    (connected, "landing-stat-connected"),
                    (week, "landing-stat-week-activities"),
                ], locale))
            }
        }
    }
}

/// One stats line: a scope label followed by dot-separated figures.
fn stat_row(scope_id: &str, figures: &[(u64, &str)], locale: Locale) -> Markup {
    html! {
        p.landing__stat-row {
            span.landing__stat-scope { (locale.text(scope_id)) }
            span.landing__stat-figures {
                @for (i, (value, unit_id)) in figures.iter().enumerate() {
                    @if i > 0 { span.landing__stat-sep aria-hidden="true" { "·" } }
                    span.landing__stat-figure { (stat_figure(*value, unit_id, locale)) }
                }
            }
        }
    }
}

/// One `value unit` figure: the message's plural form selects on `$count`
/// while the styled, digit-grouped value rides along as `$value` markup.
fn stat_figure(value: u64, unit_id: &str, locale: Locale) -> Markup {
    let styled = html! { span.landing__stat-value { (fmt_count(value, locale)) } };
    let mut args = FluentArgs::new();
    args.set("count", value);
    locale.markup_with(unit_id, &args, &[("value", styled)])
}

/// `1234567` → `1,234,567` — the digit-group separator comes from the catalog
/// (`number-group-separator`), since grouping style belongs to the locale.
fn fmt_count(n: u64, locale: Locale) -> String {
    let separator = locale.plain("number-group-separator");
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push_str(&separator);
        }
        out.push(ch);
    }
    out
}

fn rules_list(rules: &[rule::Rule]) -> Markup {
    html! {
        ol.landing__rules {
            @for rule in rules {
                li {
                    (rule.text)
                    @if !rule.hint.is_empty() {
                        span.landing__rule-hint { (rule.hint) }
                    }
                }
            }
        }
    }
}

/// `GET /rules` — the server rules on their own page, always public: it is
/// the document the sign-up agreement checkbox points at (Mastodon's
/// `/about/more` role), so it stays reachable even with the landing page
/// switched off.
pub async fn rules_page(
    State(state): State<AppState>,
    MaybeWebUser(session): MaybeWebUser,
    request_locale: Locale,
) -> Result<Response, ApiError> {
    let locale = session.as_ref().map_or(request_locale, |user| user.locale);
    let rules = rule::list_ordered(&state.pool).await?;
    let has_terms = terms_of_service::current(&state.pool)
        .await
        .map_err(ApiError::from)?
        .is_some();
    let title = locale.text("landing-rules-title");
    let content = html! {
        section.landing__section.landing--page {
            h1 { (title) }
            @if rules.is_empty() {
                p.empty { (locale.text("landing-rules-empty")) }
            } @else {
                (rules_list(&rules))
            }
            @if has_terms {
                p { a href="/terms-of-service" { (locale.text("page-terms")) } }
            }
        }
    };
    Ok(layout::shell_visitor_localized(
        &title,
        session.as_ref(),
        anon_nav(&state).await,
        &content,
        locale,
    )
    .into_response())
}

/// `GET /staff` — who runs the server: the contact account and every local
/// user holding a privileged role, plus the contact e-mail. Always public,
/// like `/rules` — it is the moderator list the landing page used to inline.
pub async fn staff_page(
    State(state): State<AppState>,
    MaybeWebUser(session): MaybeWebUser,
    request_locale: Locale,
) -> Result<Response, ApiError> {
    let locale = session.as_ref().map_or(request_locale, |user| user.locale);
    let settings = state.settings_cache.get(&state.pool).await?;
    let domain = &state.config.domain;

    let contact_id = if settings.site_contact_username.is_empty() {
        None
    } else {
        let contact =
            account::find_local_by_username(&state.pool, &settings.site_contact_username).await?;
        if let Some(found) = contact
            && crate::instance_policy::public_account_visible(&state.pool, domain, &found).await?
        {
            Some(found.id)
        } else {
            None
        }
    };
    // The contact account leads the page; keep it out of the role sections
    // so holding a staff role doesn't show it twice.
    let roster: Vec<role::StaffRosterRow> = role::staff_roster(&state.pool)
        .await?
        .into_iter()
        .filter(|row| Some(row.account_id) != contact_id)
        .collect();
    // One section per role, in roster order (highest role first). Rows arrive
    // sorted by role position, so equal names are always adjacent.
    let mut sections: Vec<(String, Vec<i64>)> = Vec::new();
    for row in roster {
        match sections.last_mut() {
            Some((name, ids)) if *name == row.role_name => ids.push(row.account_id),
            _ => sections.push((row.role_name, vec![row.account_id])),
        }
    }
    // One render over the contact and every section's members — the batched
    // renderer's fixed cost used to be repaid in full per role section.
    let all_ids: Vec<i64> = contact_id
        .into_iter()
        .chain(sections.iter().flat_map(|(_, ids)| ids.iter().copied()))
        .collect();
    let rendered: std::collections::HashMap<i64, serde_json::Value> =
        render_accounts_by_ids(&state.pool, domain, &all_ids, None)
            .await?
            .into_iter()
            .filter_map(|value| {
                let id = value.get("id")?.as_str()?.parse::<i64>().ok()?;
                Some((id, value))
            })
            .collect();
    let contact = contact_id.and_then(|id| rendered.get(&id).cloned());
    let rendered_sections: Vec<(&String, Vec<serde_json::Value>)> = sections
        .iter()
        .map(|(name, ids)| {
            let entities = ids
                .iter()
                .filter_map(|id| rendered.get(id).cloned())
                .collect();
            (name, entities)
        })
        .collect();

    let title = locale.text("nav-staff");
    let content = html! {
        section.landing__section.landing--page {
            h1 { (title) }
            @if contact.is_none() && rendered_sections.is_empty() {
                p.empty { (locale.text("landing-staff-empty")) }
            }
            @if let Some(entity) = &contact {
                h2 { (locale.text("landing-staff-admin")) }
                div.account-list {
                    (view::account_card(&view::Account(entity)))
                }
            }
            @for (name, entities) in &rendered_sections {
                h2 { (name) }
                div.account-list {
                    @for entity in entities {
                        (view::account_card(&view::Account(entity)))
                    }
                }
            }
            @if !settings.site_contact_email.is_empty() {
                @let email = html! {
                    a href=(format!("mailto:{}", settings.site_contact_email)) {
                        (settings.site_contact_email)
                    }
                };
                p.settings-field__hint {
                    (locale.markup("landing-staff-contact", &[("email", email)]))
                }
            }
        }
    };
    Ok(layout::shell_visitor_localized(
        &title,
        session.as_ref(),
        anon_nav(&state).await,
        &content,
        locale,
    )
    .into_response())
}
