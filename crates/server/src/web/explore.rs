//! The Trending surface: trending posts, hashtags and links — the web
//! face of `/api/v1/trends/*`. (The People directory, once a tab here, lives
//! at `/people` — see the `people` module.)
//!
//! One page, three sections behind a tab strip. Anonymous visitors browse
//! under the operator's `anon_trends` switch; within that, the tabs explain
//! themselves when trends are off entirely.

use std::collections::HashSet;

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use fluent_bundle::FluentArgs;
use maud::{Markup, html};
use plamenu_db::{preview_card_trend, status_trend, tag, tag_trend};
use serde::Deserialize;
use serde_json::Value;

use super::i18n::Locale;
use super::pages::{anon_nav, clock_for, prefs_of, session_settings};
use super::session::{MaybeWebUser, WebUser, preview_redirect};
use super::{layout, view};
use crate::entities::{preview_card_json, render_statuses};
use crate::error::ApiError;
use crate::state::AppState;

/// Page sizes. Posts match the timeline pages; the rest are ranked shortlists
/// paged in the same stride.
const POSTS_LIMIT: i64 = 20;
const TAGS_LIMIT: i64 = 20;
const LINKS_LIMIT: i64 = 20;

#[derive(Deserialize)]
pub struct ExploreQuery {
    offset: Option<i64>,
}

/// Which Trending section a request addresses.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Posts,
    Hashtags,
    News,
}

/// The section selector, shared by all three pages.
fn section_tabs(active: Section, locale: Locale) -> Markup {
    let posts = locale.text("explore-tab-posts");
    let hashtags = locale.text("explore-tab-hashtags");
    let news = locale.text("explore-tab-links");
    view::tab_strip(
        &locale.text("explore-tabs-aria"),
        &[
            view::Tab::new("/explore", &posts, active == Section::Posts),
            view::Tab::new("/explore/hashtags", &hashtags, active == Section::Hashtags),
            view::Tab::new("/explore/links", &news, active == Section::News),
        ],
    )
}

/// Anonymous Trending follows its own operator switch (`anon_trends`). The
/// trends API itself stays anonymous either way — this only shapes the web
/// page.
async fn anon_allowed(state: &AppState) -> bool {
    state.anon_trends().await
}

/// The offset-paginated "Show more" link, rendered only when the page came
/// back full. `base` already carries any non-offset query parameters.
pub(super) fn more_link(
    base: &str,
    fetched: usize,
    limit: i64,
    offset: i64,
    locale: Locale,
) -> Markup {
    if fetched < usize::try_from(limit).unwrap_or(usize::MAX) {
        return html! {};
    }
    let sep = if base.contains('?') { '&' } else { '?' };
    html! {
        nav.pager {
            a.pager__more href=(format!("{base}{sep}offset={}", offset + limit)) {
                (locale.text("pager-show-more"))
            }
        }
    }
}

/// The explanation shown on the three trend tabs when the operator has trends
/// disabled.
fn trends_disabled(locale: Locale) -> Markup {
    html! { p.empty { (locale.text("explore-disabled")) } }
}

/// The two-day window trend blurbs count "people talking" over: today and
/// yesterday, matching the entity history the API serves.
fn talking_window() -> time::Date {
    time::OffsetDateTime::now_utc().date() - time::Duration::days(1)
}

/// "N people in the past 2 days" — the per-item usage blurb.
fn talking_blurb(people: i64, locale: Locale) -> String {
    let mut args = FluentArgs::new();
    args.set("count", people);
    locale.text_with("explore-talking", &args)
}

/// Wraps a section body in the page chrome: title, tab strip, content.
async fn shell(
    state: &AppState,
    session: Option<&WebUser>,
    active: Section,
    content: &Markup,
    locale: Locale,
) -> Markup {
    let body = html! {
        section.column {
            (section_tabs(active, locale))
            (content)
        }
    };
    layout::shell_visitor_localized(
        &locale.text("nav-trending"),
        session,
        anon_nav(state).await,
        &body,
        locale,
    )
}

/// `GET /explore` — the trending posts, highest score first.
pub async fn posts(
    State(state): State<AppState>,
    MaybeWebUser(session): MaybeWebUser,
    request_locale: Locale,
    Query(query): Query<ExploreQuery>,
) -> Result<Response, ApiError> {
    if let Some(redirect) = preview_redirect(&session, anon_allowed(&state).await) {
        return Ok(redirect);
    }
    let settings = state.settings_cache.get(&state.pool).await?;
    let viewer_id = session.as_ref().map(|u| u.current.account.id);
    let offset = query.offset.unwrap_or(0).max(0);
    let locale = session.as_ref().map_or(request_locale, |user| user.locale);

    let content = if settings.trends_enabled {
        let user_settings = session_settings(&state, session.as_ref()).await?;
        let statuses = status_trend::allowed(&state.pool, viewer_id, POSTS_LIMIT, offset).await?;
        let entities =
            render_statuses(&state.pool, &state.config.domain, &statuses, viewer_id).await?;
        let viewer = viewer_id.map(|id| id.to_string());
        let ctx = view::Ctx {
            csrf: session.as_ref().map(|u| u.csrf.as_str()),
            viewer_id: viewer.as_deref(),
            return_to: "/explore",
            filter_context: Some(view::FilterContext::Public),
            prefs: match user_settings.as_ref() {
                Some(settings) => {
                    prefs_of(settings, crate::translation::web_language_map(&state).await)
                }
                None => view::ViewPrefs::default(),
            },
            locale,
            clock: clock_for(session.as_ref(), locale),
            admin: session
                .as_ref()
                .map(WebUser::admin_capabilities)
                .unwrap_or_default(),
        };
        html! {
            @if entities.is_empty() {
                p.empty { (locale.text("explore-empty")) }
            } @else {
                (view::feed(&entities, &ctx))
                (more_link("/explore", entities.len(), POSTS_LIMIT, offset, locale))
            }
        }
    } else {
        trends_disabled(locale)
    };

    // The Trending landing is a shareable instance-level entry point, so it
    // carries the instance preview card like /public does.
    let meta = super::meta::instance_page(&state, "/explore", locale).await?;
    let body = html! {
        section.column {
            (section_tabs(Section::Posts, locale))
            (content)
        }
    };
    Ok(layout::shell_visitor_subject_localized(
        &locale.text("nav-trending"),
        session.as_ref(),
        anon_nav(&state).await,
        &body,
        &meta,
        locale,
    )
    .into_response())
}

/// `GET /explore/hashtags` — the trending hashtags with usage blurbs and,
/// signed in, a follow/unfollow control per row.
pub async fn hashtags(
    State(state): State<AppState>,
    MaybeWebUser(session): MaybeWebUser,
    request_locale: Locale,
    Query(query): Query<ExploreQuery>,
) -> Result<Response, ApiError> {
    if let Some(redirect) = preview_redirect(&session, anon_allowed(&state).await) {
        return Ok(redirect);
    }
    let settings = state.settings_cache.get(&state.pool).await?;
    let viewer_id = session.as_ref().map(|u| u.current.account.id);
    let offset = query.offset.unwrap_or(0).max(0);
    let locale = session.as_ref().map_or(request_locale, |user| user.locale);

    let content = if settings.trends_enabled {
        let rows = tag_trend::allowed(&state.pool, TAGS_LIMIT, offset).await?;
        let ids: Vec<i64> = rows.iter().map(|t| t.id).collect();
        let histories = tag::history_batch(&state.pool, &ids, talking_window()).await?;
        let following = match viewer_id {
            Some(viewer) => tag::followed_ids(&state.pool, viewer, &ids).await?,
            None => HashSet::new(),
        };
        let back = if offset > 0 {
            format!("/explore/hashtags?offset={offset}")
        } else {
            "/explore/hashtags".to_owned()
        };
        html! {
            @if rows.is_empty() {
                p.empty { (locale.text("explore-empty")) }
            } @else {
                ul.trend-list data-paged {
                    @for row in &rows {
                        @let people = histories
                            .get(&row.id)
                            .map_or(0, |days| days.iter().map(|d| d.accounts).sum::<i64>());
                        li.trend-row {
                            div.trend-row__main {
                                a.trend-row__name href=(format!("/tags/{}", row.name)) {
                                    "#" (row.display())
                                }
                                span.trend-row__meta { (talking_blurb(people, locale)) }
                            }
                            @if let Some(user) = session.as_ref() {
                                @let (verb, label_id) = if following.contains(&row.id) {
                                    ("unfollow", "profile-unfollow")
                                } else {
                                    ("follow", "profile-follow")
                                };
                                form.trend-row__follow method="post"
                                    action=(format!("/web/tags/{}/{verb}", row.name)) {
                                    input type="hidden" name="csrf" value=(user.csrf);
                                    input type="hidden" name="return_to" value=(back);
                                    button type="submit" { (locale.text(label_id)) }
                                }
                            }
                        }
                    }
                }
                (more_link("/explore/hashtags", rows.len(), TAGS_LIMIT, offset, locale))
            }
        }
    } else {
        trends_disabled(locale)
    };
    Ok(shell(
        &state,
        session.as_ref(),
        Section::Hashtags,
        &content,
        locale,
    )
    .await
    .into_response())
}

/// `GET /explore/links` — the trending links as preview cards with usage
/// blurbs, Mastodon's News tab.
pub async fn links(
    State(state): State<AppState>,
    MaybeWebUser(session): MaybeWebUser,
    request_locale: Locale,
    Query(query): Query<ExploreQuery>,
) -> Result<Response, ApiError> {
    if let Some(redirect) = preview_redirect(&session, anon_allowed(&state).await) {
        return Ok(redirect);
    }
    let settings = state.settings_cache.get(&state.pool).await?;
    let offset = query.offset.unwrap_or(0).max(0);
    let locale = session.as_ref().map_or(request_locale, |user| user.locale);

    let content = if settings.trends_enabled {
        let cards = preview_card_trend::allowed(&state.pool, LINKS_LIMIT, offset).await?;
        let ids: Vec<i64> = cards.iter().map(|c| c.id).collect();
        let histories =
            preview_card_trend::history_batch(&state.pool, &ids, talking_window()).await?;
        // Serialize through the shared entity builder so image URLs come out
        // proxied exactly as the API serves them.
        let mut entities: Vec<(Value, i64)> = Vec::with_capacity(cards.len());
        for card in &cards {
            let people = histories
                .get(&card.id)
                .map_or(0, |days| days.iter().map(|d| d.accounts).sum::<i64>());
            entities.push((
                preview_card_json(&state.config.domain, card, "", false)?,
                people,
            ));
        }
        html! {
            @if entities.is_empty() {
                p.empty { (locale.text("explore-empty")) }
            } @else {
                div.trend-links data-paged {
                    @for (entity, people) in &entities {
                        article.trend-link {
                            (view::link_card(entity))
                            p.trend-row__meta { (talking_blurb(*people, locale)) }
                        }
                    }
                }
                (more_link("/explore/links", entities.len(), LINKS_LIMIT, offset, locale))
            }
        }
    } else {
        trends_disabled(locale)
    };
    Ok(
        shell(&state, session.as_ref(), Section::News, &content, locale)
            .await
            .into_response(),
    )
}
