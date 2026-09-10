//! The People page: follow suggestions (signed in) over the profile
//! directory — the web face of `/api/v2/suggestions` and `/api/v1/directory`.
//! Split out of the Trending surface: it's discovery of accounts, not trends.
//!
//! Anonymous visitors browse under the operator's `anon_directory` switch and
//! only see remote profiles when `anon_directory_federated` is also on —
//! otherwise the directory is pinned to this server's accounts. The directory
//! API itself keeps Mastodon's posture (anonymous whenever the directory is
//! enabled); these switches only shape the web page.

use axum::extract::{Query, RawQuery, State};
use axum::response::{IntoResponse, Redirect, Response};
use fluent_bundle::FluentArgs;
use maud::{Markup, html};
use plamenu_db::account;
use plamenu_db::discovery::{self, DirectoryPage};
use serde::Deserialize;

use super::explore::more_link;
use super::i18n::Locale;
use super::pages::anon_nav;
use super::session::{MaybeWebUser, WebUser, preview_redirect};
use super::{layout, view};
use crate::entities::render_accounts;
use crate::error::ApiError;
use crate::state::AppState;

/// Directory page size, matching the other ranked shortlists.
const PEOPLE_LIMIT: i64 = 20;
/// How many follow suggestions lead the page.
const SUGGESTIONS_LIMIT: i64 = 8;

#[derive(Deserialize)]
pub struct PeopleQuery {
    offset: Option<i64>,
    /// `new` lists newest accounts first; default is recently active, like
    /// the API.
    order: Option<String>,
    /// `local` restricts to this server; default is everyone (where the
    /// viewer is allowed to widen at all).
    scope: Option<String>,
}

/// The directory list URL for a given control state.
fn people_href(order_new: bool, local_only: bool, offset: i64) -> String {
    let mut href = "/people".to_owned();
    let mut sep = '?';
    if order_new {
        href.push(sep);
        href.push_str("order=new");
        sep = '&';
    }
    if local_only {
        href.push(sep);
        href.push_str("scope=local");
        sep = '&';
    }
    if offset > 0 {
        use std::fmt::Write;
        let _ = write!(href, "{sep}offset={offset}");
    }
    href
}

/// The one directory selector: order and scope folded into a single control.
/// When the viewer can't widen to remote profiles the scope axis disappears
/// and only the two order choices remain.
fn directory_selector(
    order_new: bool,
    local_only: bool,
    federated_allowed: bool,
    locale: Locale,
) -> Markup {
    // Owned (href, label, active) so the borrowed `Tab` slice outlives the
    // call, like the group console's tab builder.
    let options: Vec<(String, String, bool)> = if federated_allowed {
        vec![
            (
                people_href(false, false, 0),
                locale.text("people-order-active-everywhere"),
                !order_new && !local_only,
            ),
            (
                people_href(true, false, 0),
                locale.text("people-order-new-everywhere"),
                order_new && !local_only,
            ),
            (
                people_href(false, true, 0),
                locale.text("people-order-active-local"),
                !order_new && local_only,
            ),
            (
                people_href(true, true, 0),
                locale.text("people-order-new-local"),
                order_new && local_only,
            ),
        ]
    } else {
        vec![
            (
                people_href(false, false, 0),
                locale.text("people-order-active"),
                !order_new,
            ),
            (
                people_href(true, false, 0),
                locale.text("people-order-new"),
                order_new,
            ),
        ]
    };
    let tabs: Vec<view::Tab> = options
        .iter()
        .map(|(href, label, active)| view::Tab::new(href, label, *active))
        .collect();
    view::tab_strip(&locale.text("people-selector-aria"), &tabs)
}

/// Why an account is suggested, from the highest-priority source that matched.
fn suggestion_reason(sources: &[&'static str], locale: Locale) -> String {
    locale.text(match sources.first().copied() {
        Some("friends_of_friends") => "people-reason-friends",
        Some("most_interactions") => "people-reason-popular",
        _ => "people-reason-widely",
    })
}

/// The "Suggested for you" block leading the page: who-to-follow
/// candidates with one-click Follow and Dismiss. Dismissing suppresses the
/// account permanently, like `DELETE /api/v1/suggestions/{id}`. `None` when
/// no candidate matched — the page then opens straight on the directory.
async fn suggestions_block(state: &AppState, user: &WebUser) -> Result<Option<Markup>, ApiError> {
    let viewer = user.current.account.id;
    let page = crate::routes::suggestions::page(state, viewer, SUGGESTIONS_LIMIT, 0).await?;
    if page.is_empty() {
        return Ok(None);
    }
    let accounts: Vec<_> = page.iter().map(|(account, _)| account.clone()).collect();
    let entities =
        render_accounts(&state.pool, &state.config.domain, &accounts, Some(viewer)).await?;
    let locale = user.locale;
    Ok(Some(html! {
        section.suggestions {
            h2.explore-heading { (locale.text("people-suggested")) }
            div.suggestion-list {
                @for ((db_account, sources), entity) in page.iter().zip(&entities) {
                    div.suggestion-row {
                        (view::account_card(&view::Account(entity)))
                        div.suggestion-row__actions {
                            form method="post"
                                action=(format!("/web/accounts/{}/follow", db_account.id)) {
                                input type="hidden" name="csrf" value=(user.csrf);
                                input type="hidden" name="return_to" value="/people";
                                button type="submit" { (locale.text("profile-follow")) }
                            }
                            form method="post"
                                action=(format!("/web/suggestions/{}/dismiss", db_account.id)) {
                                input type="hidden" name="csrf" value=(user.csrf);
                                input type="hidden" name="return_to" value="/people";
                                button.suggestion-row__dismiss type="submit" {
                                    (locale.text("people-dismiss"))
                                }
                            }
                        }
                        span.suggestion-row__why { (suggestion_reason(sources, locale)) }
                    }
                }
            }
        }
    }))
}

/// `GET /people` — follow suggestions (signed in) over the profile directory.
/// Directory order and scope mirror the API's `order`/`local` parameters;
/// `discoverable` filtering happens in the query.
pub async fn people(
    State(state): State<AppState>,
    MaybeWebUser(session): MaybeWebUser,
    request_locale: Locale,
    Query(query): Query<PeopleQuery>,
) -> Result<Response, ApiError> {
    if let Some(redirect) = preview_redirect(&session, state.anon_directory().await) {
        return Ok(redirect);
    }
    let settings = state.settings_cache.get(&state.pool).await?;
    let locale = session.as_ref().map_or(request_locale, |user| user.locale);
    let viewer_id = session.as_ref().map(|u| u.current.account.id);
    let offset = query.offset.unwrap_or(0).max(0);
    let order_new = query.order.as_deref() == Some("new");
    let federated_allowed = session.is_some() || state.anon_directory_federated().await;
    let local_only = !federated_allowed || query.scope.as_deref() == Some("local");

    // Suggestions lead the first page for a signed-in viewer; deeper pages
    // skip straight to the directory list.
    let suggestions = match (session.as_ref(), offset) {
        (Some(user), 0) => suggestions_block(&state, user).await?,
        _ => None,
    };

    let directory = if settings.profile_directory {
        let page = DirectoryPage {
            order_new,
            local_only,
            viewer_id,
            limit: PEOPLE_LIMIT,
            offset,
        };
        let ids = discovery::directory_account_ids(&state.pool, &page).await?;
        let mut listed = account::find_by_ids(&state.pool, &ids).await?;
        // Preserve the ranked order; drop any id that vanished between the id
        // scan and the fetch, like the API route.
        listed.sort_by_key(|a| ids.iter().position(|id| *id == a.id).unwrap_or(usize::MAX));
        let entities =
            render_accounts(&state.pool, &state.config.domain, &listed, viewer_id).await?;
        let base = people_href(order_new, local_only && federated_allowed, 0);
        html! {
            (directory_selector(order_new, local_only, federated_allowed, locale))
            @if entities.is_empty() {
                p.empty { (locale.text("people-empty")) }
            } @else {
                div.directory-list data-paged {
                    @for entity in &entities {
                        @let account = view::Account(entity);
                        article.directory-card {
                            (view::account_card(&account))
                            @if !account.note_html().is_empty() {
                                div.directory-card__note {
                                    (account.note_markup(session.is_some()))
                                }
                            }
                            p.directory-card__stats { (card_stats(&account, locale)) }
                        }
                    }
                }
                (more_link(&base, entities.len(), PEOPLE_LIMIT, offset, locale))
            }
        }
    } else {
        html! { p.empty { (locale.text("people-disabled")) } }
    };

    let title = locale.text("nav-people");
    let content = html! {
        section.column {
            h1 { (view::icon("profile")) " " (title) }
            @if let Some(block) = &suggestions {
                (block)
                h2.explore-heading { (locale.text("people-directory")) }
            }
            (directory)
        }
    };
    // A shareable instance-level entry point, like /public and /explore.
    let meta = super::meta::instance_page(&state, "/people", locale).await?;
    Ok(layout::shell_visitor_subject_localized(
        &title,
        session.as_ref(),
        anon_nav(&state).await,
        &content,
        &meta,
        locale,
    )
    .into_response())
}

/// The `N posts · N followers` line under a directory card, one message so
/// both plural forms belong to the translator.
fn card_stats(account: &view::Account, locale: Locale) -> String {
    let mut args = FluentArgs::new();
    args.set("posts", account.statuses_count());
    args.set("followers", account.followers_count());
    locale.text_with("people-card-stats", &args)
}

/// `GET /explore/people` — permanent redirect to `/people`, which used to be
/// an Explore tab. Keeps any order/scope/offset parameters.
pub async fn legacy_redirect(RawQuery(query): RawQuery) -> Response {
    let target = match query {
        Some(query) if !query.is_empty() => format!("/people?{query}"),
        _ => "/people".to_owned(),
    };
    Redirect::permanent(&target).into_response()
}
