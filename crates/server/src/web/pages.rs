//! Read-only pages: the home and public timelines, profiles and threads.
//!
//! Every page fetches the same rows the JSON API does and renders them through
//! the shared `entities` layer, so the HTML mirrors the API exactly. Pages are
//! fully usable without JavaScript — links navigate, "older" is a plain link,
//! and the action forms POST.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Redirect, Response};
use fluent_bundle::FluentArgs;
use maud::{Markup, html};
use plamenu_db::account::{self, Account};
use plamenu_db::notification::{self, NotificationFilter};
use plamenu_db::status::{self, Status, StatusSearch};
use plamenu_db::user::{self, ReadingExpandMedia, TimelineOrder, UserSettings};
use plamenu_db::{
    bookmark, favourite, follow, instance_policy, marker, mention, pin, quote, remote_history,
    rule, tag,
};
use serde::Deserialize;
use serde_json::Value;

use super::clock::ViewerClock;
use super::collapse;
use super::i18n::Locale;
use super::session::{MaybeWebUser, WebUser};
use super::{actions, layout, reactions, view};
use crate::entities::{
    account_json, allow_direct_media, can_view, filter_viewable, media_json_hls, relationship_json,
    render_accounts, render_accounts_by_ids, render_conversations, render_notifications,
    render_status, render_status_history, render_statuses,
};
use crate::error::ApiError;
use crate::languages;
use crate::state::AppState;

/// Timeline page size. Matches the API default so paging behaves the same.
const LIMIT: i64 = 20;

async fn render_live_rows(
    state: &AppState,
    statuses: &[Status],
    viewer_id: Option<i64>,
) -> Result<Vec<Value>, ApiError> {
    let mut entities =
        render_statuses(&state.pool, &state.config.domain, statuses, viewer_id).await?;
    if crate::live_refresh::refresh_rendered_statuses(state, &entities).await {
        entities = render_statuses(&state.pool, &state.config.domain, statuses, viewer_id).await?;
    }
    Ok(entities)
}

/// The anonymous-navigation flags for the chrome, derived from the instance's
/// public-access settings. A logged-out visitor should only see links to
/// features they can actually use without signing in.
pub(crate) async fn anon_nav(state: &AppState) -> layout::AnonNav {
    layout::AnonNav {
        feeds: state.timeline_preview_federated().await || state.timeline_preview_local().await,
        trends: state.anon_trends().await,
        people: state.anon_directory().await,
        groups: state.anon_groups().await,
        search: state.public_search().await,
    }
}

pub(super) async fn settings_for(
    user: &WebUser,
    state: &AppState,
) -> Result<UserSettings, ApiError> {
    Ok(user::settings_by_user_id(&state.pool, user.current.user.id)
        .await?
        .unwrap_or_default())
}

/// The reading preferences of stored settings as the [`view::ViewPrefs`] the
/// renderers consume.
pub(super) fn prefs_of(
    settings: &UserSettings,
    translate_languages: Option<std::sync::Arc<std::collections::BTreeMap<String, Vec<String>>>>,
) -> view::ViewPrefs {
    view::ViewPrefs {
        translate_to: translate_languages
            .is_some()
            .then(|| settings.translate_language().to_owned()),
        translate_languages,
        expand_media: match settings.reading_expand_media {
            ReadingExpandMedia::Default => view::MediaDisplay::Default,
            ReadingExpandMedia::ShowAll => view::MediaDisplay::ShowAll,
            ReadingExpandMedia::HideAll => view::MediaDisplay::HideAll,
        },
        expand_spoilers: settings.reading_expand_spoilers,
        autoplay_gifs: settings.reading_autoplay_gifs,
    }
}

/// The viewer's reading preferences as the [`view::ViewPrefs`] the renderers
/// consume. A logged-out viewer gets the defaults (gate sensitive media, keep
/// content warnings collapsed).
async fn view_prefs(state: &AppState, user: Option<&WebUser>) -> Result<view::ViewPrefs, ApiError> {
    let Some(user) = user else {
        return Ok(view::ViewPrefs::default());
    };
    let settings = settings_for(user, state).await?;
    Ok(prefs_of(
        &settings,
        crate::translation::web_language_map(state).await,
    ))
}

/// How a possibly-anonymous visitor reads a timestamp. A signed-in viewer's
/// clock was resolved once by the session extractor, so this costs nothing;
/// an anonymous one reads in UTC, deliberately — public pages are cacheable
/// and varying their rendered timestamps per visitor would fragment that for
/// marginal benefit (the relative labels anon readers mostly see are
/// zone-independent anyway, and the tooltip names UTC explicitly).
pub(super) fn clock_for(user: Option<&WebUser>, locale: Locale) -> ViewerClock {
    user.map_or_else(|| ViewerClock::utc(locale), |user| user.clock.clone())
}

/// The stored settings of a possibly-anonymous visitor; `None` when logged
/// out. The timeline pages read these once for both the ordering preference
/// and the render prefs.
pub(super) async fn session_settings(
    state: &AppState,
    user: Option<&WebUser>,
) -> Result<Option<UserSettings>, ApiError> {
    match user {
        Some(user) => Ok(Some(settings_for(user, state).await?)),
        None => Ok(None),
    }
}

/// The viewer's posting defaults, owned so the borrowed [`view::ComposeDefaults`]
/// the composer takes can point into it during rendering.
struct ComposeCtx {
    csrf: String,
    visibility: String,
    sensitive: bool,
    language: String,
    languages: Vec<&'static languages::Language>,
    quote_policy: &'static str,
    content_type: &'static str,
    /// The viewer's preference time zone — what the Schedule field's
    /// wall-clock reading is interpreted in.
    time_zone: &'static str,
    limits: view::ComposeLimits,
}

impl ComposeCtx {
    /// Loads the viewer's posting defaults, enabled posting languages and the
    /// instance limits — everything the composer renders from.
    async fn load(state: &AppState, user: &WebUser) -> Result<Self, ApiError> {
        let settings = settings_for(user, state).await?;
        let enabled = user::posting_languages(&state.pool, user.current.user.id).await?;
        Ok(Self {
            csrf: user.csrf.clone(),
            visibility: settings
                .resolved_visibility(user.current.account.locked)
                .to_owned(),
            sensitive: settings.posting_default_sensitive,
            language: settings.posting_default_language.clone(),
            languages: languages::enabled(enabled.as_deref()),
            quote_policy: settings.posting_default_quote_policy.as_str(),
            content_type: settings.posting_default_content_type.as_str(),
            time_zone: user.clock.name(),
            limits: compose_limits(state).await?,
        })
    }

    fn defaults(&self) -> view::ComposeDefaults<'_> {
        view::ComposeDefaults {
            visibility: &self.visibility,
            sensitive: self.sensitive,
            language: &self.language,
            languages: &self.languages,
            quote_policy: self.quote_policy,
            content_type: self.content_type,
            time_zone: self.time_zone,
        }
    }
}

/// The live posting limits the composer is rendered against.
async fn compose_limits(state: &AppState) -> Result<view::ComposeLimits, ApiError> {
    let settings = state.settings_cache.get(&state.pool).await?;
    Ok(view::ComposeLimits {
        max_characters: settings.max_characters,
        max_characters_long_form: settings.max_characters_long_form,
        max_media_attachments: settings.max_media_attachments,
        poll_max_options: settings.poll_max_options,
    })
}

#[derive(Deserialize)]
pub struct Page {
    max_id: Option<i64>,
}

#[derive(Deserialize)]
pub struct PublicPage {
    max_id: Option<i64>,
    local: Option<String>,
}

/// A "Load older posts" link when the page came back full, keyset-paginated on
/// the last id — the no-JS equivalent of infinite scroll.
pub(super) fn older_link(base: &str, statuses: &[Status], limit: i64) -> Markup {
    older_link_localized(base, statuses, limit, Locale::default())
}

fn older_link_localized(base: &str, statuses: &[Status], limit: i64, locale: Locale) -> Markup {
    if statuses.len() < usize::try_from(limit).unwrap_or(usize::MAX) {
        return html! {};
    }
    let Some(last) = statuses.last() else {
        return html! {};
    };
    let sep = if base.contains('?') { '&' } else { '?' };
    let href = format!("{base}{sep}max_id={}", last.id);
    html! { nav.pager { a.pager__more href=(href) { (locale.text("pager-older-posts")) } } }
}

/// Flags, per home-timeline entity, the followed hashtag(s) that pulled it into
/// the feed, stashing them on the entity as `_tag_source` — a web-only render
/// hint the card turns into a banner, the same convention as `_group_mod`. The
/// provenance comes typed from a single query keyed on the page's status ids
/// (`tag::followed_tag_sources`) rather than being reconstructed from the
/// rendered JSON: it already returns only posts that reached the feed *solely*
/// because of a followed tag (an original whose author is neither the viewer nor
/// a followed account), so a boost, the viewer's own post, or a followed
/// account's post is never flagged even when it also carries a followed tag.
/// `statuses[i]` renders to `entities[i]` one-for-one (see
/// [`render_statuses`]), so the typed row supplies the id and the entity is
/// annotated in lockstep — no author ids are parsed back out of the JSON.
async fn annotate_tag_sources(
    pool: &plamenu_db::PgPool,
    viewer_id: i64,
    statuses: &[Status],
    entities: &mut [Value],
) -> Result<(), ApiError> {
    let ids: Vec<i64> = statuses.iter().map(|status| status.id).collect();
    let sources: std::collections::HashMap<i64, Vec<String>> =
        tag::followed_tag_sources(pool, viewer_id, &ids)
            .await?
            .into_iter()
            .collect();
    if sources.is_empty() {
        return Ok(());
    }
    for (status, entity) in statuses.iter().zip(entities.iter_mut()) {
        if let Some(names) = sources.get(&status.id)
            && let Some(object) = entity.as_object_mut()
        {
            let names = names
                .iter()
                .map(|name| Value::String(name.clone()))
                .collect();
            object.insert("_tag_source".to_owned(), Value::Array(names));
        }
    }
    Ok(())
}

/// `GET /` — the viewer's home timeline.
///
/// There is no inline composer here: posting lives on the dedicated `/compose`
/// page (reachable from the sidebar "New post" button and the mobile compose
/// tab). The cut-down inline box only survives where a reply is implied — the
/// thread view.
/// `GET /` — the signed-in home timeline, or the instance's public welcome
/// page for signed-out visitors (which itself falls back to the `/login`
/// redirect when the operator switched the landing page off).
pub async fn home(
    State(state): State<AppState>,
    MaybeWebUser(session): MaybeWebUser,
    request_locale: Locale,
    Query(page): Query<Page>,
) -> Result<Response, ApiError> {
    let Some(user) = session else {
        return super::landing::landing(&state, request_locale).await;
    };
    let viewer_id = user.current.account.id;
    let settings = settings_for(&user, &state).await?;
    let collapse_policy = collapse::Policy::resolve(&state, &settings).await;
    let statuses = status::home_timeline(
        &state.pool,
        viewer_id,
        settings.timeline_order,
        page.max_id,
        LIMIT,
    )
    .await?;
    let mut entities = render_live_rows(&state, &statuses, Some(viewer_id)).await?;
    // Flag posts that reached the feed via a followed hashtag so the card
    // can say so; a no-op for the common case of an account following no tags.
    // Runs before the collapse, which is what breaks the row-to-entity pairing.
    annotate_tag_sources(&state.pool, viewer_id, &statuses, &mut entities).await?;
    // Merge repeated boosts of one post into a single card. The pager
    // below reads `statuses`, the uncollapsed rows, so the cursor is unaffected
    // by cards moving.
    let entities = if collapse_policy.enabled {
        let seen = match collapse_policy.window_for(page.max_id) {
            Some((lookback, cursor)) => {
                let head = status::home_timeline(
                    &state.pool,
                    viewer_id,
                    settings.timeline_order,
                    None,
                    lookback,
                )
                .await?;
                collapse::seen_targets(&head, cursor)
            }
            None => std::collections::HashSet::new(),
        };
        collapse::collapse(entities, &seen)
    } else {
        entities
    };
    // Show a reply what it is replying *to*: a line of the parent when the
    // parent is off the page, and the exchanges the page already carries drawn
    // together, parent first. Like the collapse above, neither touches
    // `statuses`, so the pager below is unaffected.
    let mut entities = entities;
    super::thread::annotate_reply_peeks(&state, viewer_id, &mut entities).await?;
    let cards = super::thread::group(entities);
    let viewer = viewer_id.to_string();
    let ctx = view::Ctx {
        csrf: Some(&user.csrf),
        viewer_id: Some(&viewer),
        return_to: "/",
        filter_context: Some(view::FilterContext::Home),
        prefs: prefs_of(
            &settings,
            crate::translation::web_language_map(&state).await,
        ),
        locale: user.locale,
        clock: user.clock.clone(),
        admin: user.admin_capabilities(),
    };
    // Unread server announcements lead the timeline; once every one is
    // read this collapses to a one-line link (or nothing at all).
    let announcements = super::announcements::home_banner(&state, &user).await?;
    let body = html! {
        section.column {
            (announcements)
            (view::threaded_feed(&cards, &ctx))
            (older_link_localized("/", &statuses, LIMIT, user.locale))
        }
    };
    Ok(layout::shell(&user.locale.text("page-home"), Some(&user), &body).into_response())
}

/// A "Load older posts" link for the bookmark/favourite listings, which
/// keyset-paginate on the row id (bookmark/favourite id) rather than the
/// status id — so the cursor comes from the last *entry*, not the last
/// rendered status (a since-hidden post is skipped but still advances the
/// cursor). Shown only when the page came back full.
fn older_link_rows(
    base: &str,
    last_row_id: Option<i64>,
    fetched: usize,
    limit: i64,
    locale: Locale,
) -> Markup {
    if fetched < usize::try_from(limit).unwrap_or(usize::MAX) {
        return html! {};
    }
    let Some(row_id) = last_row_id else {
        return html! {};
    };
    let sep = if base.contains('?') { '&' } else { '?' };
    let href = format!("{base}{sep}max_id={row_id}");
    html! { nav.pager { a.pager__more href=(href) { (locale.text("pager-older-posts")) } } }
}

/// `GET /bookmarks` — the viewer's bookmarked posts, newest bookmark first.
pub async fn bookmarks(
    State(state): State<AppState>,
    user: WebUser,
    Query(page): Query<Page>,
) -> Result<Markup, ApiError> {
    let viewer_id = user.current.account.id;
    let settings = settings_for(&user, &state).await?;
    let entries = bookmark::list(&state.pool, viewer_id, page.max_id, None, None, LIMIT).await?;
    // A bookmark of a since-hidden post is skipped, never an error.
    let ids: Vec<i64> = entries.iter().map(|e| e.status_id).collect();
    let mut statuses = status::find_by_ids(&state.pool, &ids).await?;
    statuses.sort_by_key(|s| ids.iter().position(|id| *id == s.id));
    let entities = render_statuses(
        &state.pool,
        &state.config.domain,
        &statuses,
        Some(viewer_id),
    )
    .await?;
    let viewer = viewer_id.to_string();
    let ctx = view::Ctx {
        csrf: Some(&user.csrf),
        viewer_id: Some(&viewer),
        return_to: "/bookmarks",
        filter_context: None,
        prefs: prefs_of(
            &settings,
            crate::translation::web_language_map(&state).await,
        ),
        locale: user.locale,
        clock: user.clock.clone(),
        admin: user.admin_capabilities(),
    };
    let body = html! {
        section.column {
            h1 { (user.locale.text("page-bookmarks")) }
            @if entities.is_empty() {
                p.empty { (user.locale.text("page-bookmarks-empty")) }
            } @else {
                (view::feed(&entities, &ctx))
            }
            (older_link_rows("/bookmarks", entries.last().map(|e| e.row_id), entries.len(), LIMIT, user.locale))
        }
    };
    Ok(layout::shell(
        &user.locale.text("page-bookmarks"),
        Some(&user),
        &body,
    ))
}

/// `GET /favourites` — the viewer's favourited posts, newest favourite first.
pub async fn favourites(
    State(state): State<AppState>,
    user: WebUser,
    Query(page): Query<Page>,
) -> Result<Markup, ApiError> {
    let viewer_id = user.current.account.id;
    let settings = settings_for(&user, &state).await?;
    let entries = favourite::list(&state.pool, viewer_id, page.max_id, None, None, LIMIT).await?;
    // A favourite of a since-hidden post is skipped, never an error.
    let ids: Vec<i64> = entries.iter().map(|e| e.status_id).collect();
    let mut statuses = status::find_by_ids(&state.pool, &ids).await?;
    statuses.sort_by_key(|s| ids.iter().position(|id| *id == s.id));
    let entities = render_statuses(
        &state.pool,
        &state.config.domain,
        &statuses,
        Some(viewer_id),
    )
    .await?;
    let viewer = viewer_id.to_string();
    let ctx = view::Ctx {
        csrf: Some(&user.csrf),
        viewer_id: Some(&viewer),
        return_to: "/favourites",
        filter_context: None,
        prefs: prefs_of(
            &settings,
            crate::translation::web_language_map(&state).await,
        ),
        locale: user.locale,
        clock: user.clock.clone(),
        admin: user.admin_capabilities(),
    };
    let body = html! {
        section.column {
            h1 { (user.locale.text("page-favourites")) }
            @if entities.is_empty() {
                p.empty { (user.locale.text("page-favourites-empty")) }
            } @else {
                (view::feed(&entities, &ctx))
            }
            (older_link_rows("/favourites", entries.last().map(|e| e.row_id), entries.len(), LIMIT, user.locale))
        }
    };
    Ok(layout::shell(
        &user.locale.text("page-favourites"),
        Some(&user),
        &body,
    ))
}

/// `GET /public` (`?local=true` for the local-only timeline). An anonymous
/// visitor lands on whichever scope their preview flag allows: a scope whose
/// flag is off redirects to the allowed one, and only when both flags are off
/// does the page bounce to `/login`.
pub async fn public(
    State(state): State<AppState>,
    MaybeWebUser(session): MaybeWebUser,
    request_locale: Locale,
    Query(page): Query<PublicPage>,
) -> Result<Response, ApiError> {
    let local_only = page
        .local
        .as_deref()
        .is_some_and(|v| v == "true" || v == "1");
    let federated_ok = session.is_some() || state.timeline_preview_federated().await;
    let local_ok = session.is_some() || state.timeline_preview_local().await;
    if local_only && !local_ok && federated_ok {
        return Ok(Redirect::to("/public").into_response());
    }
    if !local_only && !federated_ok && local_ok {
        return Ok(Redirect::to("/public?local=true").into_response());
    }
    let preview = if local_only { local_ok } else { federated_ok };
    if let Some(redirect) = super::session::preview_redirect(&session, preview) {
        return Ok(redirect);
    }
    let viewer_id = session.as_ref().map(|u| u.current.account.id);
    let settings = session_settings(&state, session.as_ref()).await?;
    let order = settings
        .as_ref()
        .map(|s| s.timeline_order)
        .unwrap_or_default();
    let statuses = status::public_timeline(
        &state.pool,
        local_only,
        viewer_id,
        state.public_timeline_replies().await,
        order,
        page.max_id,
        LIMIT,
    )
    .await?;
    let entities = render_live_rows(&state, &statuses, viewer_id).await?;
    let base = if local_only {
        "/public?local=true"
    } else {
        "/public"
    };
    let viewer = viewer_id.map(|id| id.to_string());
    let locale = session.as_ref().map_or(request_locale, |user| user.locale);
    let ctx = view::Ctx {
        csrf: session.as_ref().map(|u| u.csrf.as_str()),
        viewer_id: viewer.as_deref(),
        return_to: base,
        filter_context: Some(view::FilterContext::Public),
        prefs: match settings.as_ref() {
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
    // Anonymous visitors only get tabs for the scopes their preview flags
    // allow; a lone allowed scope needs no selector at all.
    let federated_label = locale.text("public-tab-federated");
    let local_label = locale.text("public-tab-local");
    let mut scope_tabs = Vec::new();
    if federated_ok {
        scope_tabs.push(view::Tab::new("/public", &federated_label, !local_only));
    }
    if local_ok {
        scope_tabs.push(view::Tab::new(
            "/public?local=true",
            &local_label,
            local_only,
        ));
    }
    let body = html! {
        section.column {
            @if scope_tabs.len() > 1 {
                (view::tab_strip(&locale.text("public-scope-aria"), &scope_tabs))
            }
            (view::feed(&entities, &ctx))
            (older_link_localized(base, &statuses, LIMIT, locale))
        }
    };
    // The public timeline is a shareable instance-level entry point, so it
    // carries the instance preview card.
    let meta = super::meta::instance_page(&state, "/public", locale).await?;
    Ok(layout::shell_visitor_subject_localized(
        &locale.text("nav-live-feeds"),
        session.as_ref(),
        anon_nav(&state).await,
        &body,
        &meta,
        locale,
    )
    .into_response())
}

/// Paging plus the Activity tab's reply/boost switches for a profile page —
/// and, on group pages, the vote-ranked sort.
#[derive(Deserialize)]
pub struct ProfileQuery {
    max_id: Option<i64>,
    replies: Option<String>,
    boosts: Option<String>,
    sort: Option<String>,
    t: Option<String>,
    page: Option<i64>,
}

/// A group page's sort (honored only on group profiles): `New` is
/// the plain keyset feed every profile has; `Top` and `Hot` rank the
/// group's posts by votes and paginate by page number — rank orders don't
/// keyset.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum GroupSort {
    New,
    Top(plamenu_db::group::TopWindow, i64),
    Hot(i64),
}

impl GroupSort {
    fn parse(sort: Option<&str>, t: Option<&str>, page: Option<i64>) -> Self {
        let page = page.unwrap_or(0).max(0);
        match sort {
            Some("top") => Self::Top(
                plamenu_db::group::TopWindow::parse(t.unwrap_or("week")),
                page,
            ),
            Some("hot") => Self::Hot(page),
            _ => Self::New,
        }
    }

    fn page(self) -> i64 {
        match self {
            Self::New => 0,
            Self::Top(_, page) | Self::Hot(page) => page,
        }
    }

    /// The profile path with this sort encoded (at page 0) — the base the
    /// selector, pager and post-action redirects build on.
    fn href(self, path: &str) -> String {
        match self {
            Self::New => path.to_owned(),
            Self::Hot(_) => format!("{path}?sort=hot"),
            Self::Top(window, _) => format!("{path}?sort=top&t={}", window.as_str()),
        }
    }
}

/// A `0/1`-or-`false/true` query flag, absent meaning `default`.
fn query_flag(value: Option<&str>, default: bool) -> bool {
    value.map_or(default, |v| v == "true" || v == "1")
}

/// Which profile section a request addresses (Mastodon's Activity / Media /
/// Featured tabs plus the per-hashtag view), with the Activity tab's filter
/// switches. Mastodon's defaults: boosts shown, replies hidden.
pub(crate) enum ProfileTab {
    Activity { replies: bool, boosts: bool },
    Media,
    Featured,
    Tagged(String),
}

impl Default for ProfileTab {
    fn default() -> Self {
        Self::Activity {
            replies: false,
            boosts: true,
        }
    }
}

impl ProfileTab {
    /// The canonical path of this section under a profile at `path`, with the
    /// Activity filter state encoded as query flags (defaults omitted).
    fn base(&self, path: &str) -> String {
        match self {
            Self::Activity {
                replies: false,
                boosts: true,
            } => path.to_owned(),
            Self::Activity {
                replies: true,
                boosts: true,
            } => format!("{path}?replies=1"),
            Self::Activity {
                replies: true,
                boosts: false,
            } => format!("{path}?replies=1&boosts=0"),
            Self::Activity {
                replies: false,
                boosts: false,
            } => format!("{path}?boosts=0"),
            Self::Media => format!("{path}/media"),
            Self::Featured => format!("{path}/featured"),
            Self::Tagged(tag) => format!("{path}/tagged/{tag}"),
        }
    }
}

/// `GET /@handle` — a profile and its statuses (the Activity tab).
pub async fn profile(
    State(state): State<AppState>,
    MaybeWebUser(session): MaybeWebUser,
    request_locale: Locale,
    uri: Uri,
    Path(handle): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ProfileQuery>,
) -> Result<Response, ApiError> {
    // The Atom feed shares the profile's single-segment route (matchit has no
    // suffix wildcard): `/@name.atom` lands here as `handle = "@name.atom"`.
    if let Some(handle) = handle.strip_suffix(".atom") {
        return super::feed::account_atom(&state, handle, request_locale).await;
    }
    let tab = ProfileTab::Activity {
        replies: query_flag(query.replies.as_deref(), false),
        boosts: query_flag(query.boosts.as_deref(), true),
    };
    let sort = GroupSort::parse(query.sort.as_deref(), query.t.as_deref(), query.page);
    let account = match resolve_profile_handle(&state, &handle, &uri).await? {
        ProfileHandleResolution::Account(account) => *account,
        ProfileHandleResolution::Redirect(response) => return Ok(response),
        ProfileHandleResolution::Missing => {
            return Ok(not_found(&state, session.as_ref(), request_locale).await);
        }
    };
    // Content-negotiate like Mastodon: an ActivityPub client that dereferences
    // this shareable URL (searching by link) gets the actor document, not the
    // HTML page. Only for our own accounts — a remote actor is authoritative at
    // its origin, so its `/@user@host` mirror stays HTML. Serving in place (vs a
    // redirect to `/users/{name}`) keeps the HTTP signature valid, which
    // Pleroma/Akkoma/GoToSocial require and would otherwise reject.
    if account.is_local() && crate::routes::ap_requested(&headers) {
        return Box::pin(crate::routes::actors::get_actor(
            State(state),
            MaybeWebUser(session),
            request_locale,
            uri,
            Path(account.username.clone()),
            headers,
        ))
        .await;
    }
    profile_tab_view_sorted(
        &state,
        session,
        request_locale,
        account,
        tab,
        query.max_id,
        sort,
        !headers.contains_key("x-remote-history-fragment"),
    )
    .await
}

/// Former local handles are permanently reserved and redirect every human
/// profile sub-route (not just the root) to the current handle. The query and
/// suffix are retained, while the immutable `ActivityPub` actor URI is never
/// involved in the redirect.
pub(super) async fn former_local_handle_redirect(
    state: &AppState,
    handle: &str,
    uri: &Uri,
) -> Result<Option<Response>, ApiError> {
    let Some((prefix, local_handle)) = handle
        .strip_prefix('@')
        .map(|name| ('@', name))
        .or_else(|| handle.strip_prefix('!').map(|name| ('!', name)))
    else {
        return Ok(None);
    };
    if local_handle.contains('@') {
        return Ok(None);
    }
    let Some(current) = account::find_local_by_alias(&state.pool, local_handle).await? else {
        return Ok(None);
    };
    let old_root = format!("/{handle}");
    let suffix = uri.path().strip_prefix(&old_root).unwrap_or_default();
    let mut target = format!("/{prefix}{}{suffix}", current.username);
    if let Some(query) = uri.query() {
        target.push('?');
        target.push_str(query);
    }
    Ok(Some(Redirect::permanent(&target).into_response()))
}

/// Result of resolving a path where the handle names the resource owner.
/// Current handles stay on the one-query lookup path; the alias table is only
/// consulted after that lookup misses. Status permalinks do not use this
/// helper because their handle component is informational rather than the
/// identity used to select the status.
pub(super) enum ProfileHandleResolution {
    Account(Box<Account>),
    Redirect(Response),
    Missing,
}

pub(super) async fn resolve_profile_handle(
    state: &AppState,
    handle: &str,
    uri: &Uri,
) -> Result<ProfileHandleResolution, ApiError> {
    if let Some(account) = resolve_handle(state, handle).await? {
        return Ok(ProfileHandleResolution::Account(Box::new(account)));
    }
    Ok(
        match former_local_handle_redirect(state, handle, uri).await? {
            Some(response) => ProfileHandleResolution::Redirect(response),
            None => ProfileHandleResolution::Missing,
        },
    )
}

/// Resolves a `/@handle` segment and renders the requested profile section.
async fn profile_section(
    state: &AppState,
    session: Option<WebUser>,
    request_locale: Locale,
    handle: &str,
    uri: &Uri,
    tab: ProfileTab,
    max_id: Option<i64>,
) -> Result<Response, ApiError> {
    let account = match resolve_profile_handle(state, handle, uri).await? {
        ProfileHandleResolution::Account(account) => *account,
        ProfileHandleResolution::Redirect(response) => return Ok(response),
        ProfileHandleResolution::Missing => {
            return Ok(not_found(state, session.as_ref(), request_locale).await);
        }
    };
    profile_tab_view(state, session, request_locale, account, tab, max_id).await
}

/// Renders a resolved account's profile page (the default Activity tab).
/// Shared by the `/@handle` web route and the content-negotiated
/// `/users/{username}` `ActivityPub` route, so a browser hitting either URL
/// gets the same page (like Mastodon).
pub(crate) async fn profile_view(
    state: &AppState,
    session: Option<WebUser>,
    request_locale: Locale,
    account: Account,
) -> Result<Response, ApiError> {
    profile_tab_view(
        state,
        session,
        request_locale,
        account,
        ProfileTab::default(),
        None,
    )
    .await
}

/// Renders one section of a resolved account's profile: the shared header and
/// section tabs, then the tab's own content.
pub(crate) async fn profile_tab_view(
    state: &AppState,
    session: Option<WebUser>,
    request_locale: Locale,
    account: Account,
    tab: ProfileTab,
    max_id: Option<i64>,
) -> Result<Response, ApiError> {
    profile_tab_view_sorted(
        state,
        session,
        request_locale,
        account,
        tab,
        max_id,
        GroupSort::New,
        true,
    )
    .await
}

/// [`profile_tab_view`] with a group sort — the `/@group?sort=` views; every
/// non-group entry point passes `New`.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the profile page's one assembly point"
)]
async fn profile_tab_view_sorted(
    state: &AppState,
    session: Option<WebUser>,
    request_locale: Locale,
    account: Account,
    tab: ProfileTab,
    max_id: Option<i64>,
    sort: GroupSort,
    automatic_history: bool,
) -> Result<Response, ApiError> {
    let viewer_id = session.as_ref().map(|u| u.current.account.id);
    // The JSON actor/account representations remain dereferenceable as a
    // blank suspended stub, but Mastodon does not serve the interactive HTML
    // profile while the suspension is temporary.
    if account.suspended() {
        return Err(ApiError::Forbidden("This account is suspended".into()));
    }
    // A silenced author (own silence or a domain silence) gets a stripped-down
    // profile: anonymous visitors see only the handle and a notice; logged-in
    // viewers see the profile normally but with a notice and forced-sensitive
    // media.
    let silenced = account::effectively_silenced(&state.pool, account.id).await?;
    let locale = session.as_ref().map_or(request_locale, |user| user.locale);
    if silenced && session.is_none() {
        return silenced_stub_page(state, &account, locale).await;
    }

    // The owner's `show_media` / `show_featured` settings turn the sections
    // off; a direct link to a disabled one lands back on the profile, like
    // Mastodon's client-side redirect.
    let account_value =
        account_json(&state.pool, &state.config.domain, &account, viewer_id).await?;
    let account_entity = view::Account(&account_value);
    let path = account_entity.profile_path();
    match &tab {
        ProfileTab::Media if !account.show_media => {
            return Ok(Redirect::to(&path).into_response());
        }
        // Featured (endorsed accounts + curated collections) is a person-only
        // concept; a group has no Featured tab, so a direct link lands home.
        ProfileTab::Featured if !account.show_featured || account.is_group() => {
            return Ok(Redirect::to(&path).into_response());
        }
        _ => {}
    }

    // Ranked sorts only mean something on a group's Activity feed.
    let sort = if account.is_group() && matches!(tab, ProfileTab::Activity { .. }) {
        sort
    } else {
        GroupSort::New
    };
    // The chronological tabs honor the viewer's timeline-ordering preference,
    // like the home/list timelines; anonymous visitors read in publish order.
    let order = match &session {
        Some(user) => settings_for(user, state).await?.timeline_order,
        None => TimelineOrder::default(),
    };
    let mut feed = tab_feed(
        state, &account, &tab, max_id, viewer_id, order, silenced, sort,
    )
    .await?;

    // Opening an eligible remote profile while signed in is a local enqueue
    // only. The request never waits on federation; the worker updates this
    // durable state and the progressive enhancement refreshes in place.
    let first_page = max_id.is_none() && sort.page() == 0;
    let history_eligible = first_page
        && matches!(tab, ProfileTab::Activity { .. })
        && account.domain.is_some()
        && !account.is_portable_on(&state.config.domain)
        && !account.is_group();
    let history_panel = if history_eligible {
        let mut snapshot = remote_history::snapshot(&state.pool, account.id).await?;
        if snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.hydration_enabled)
            && let Some(user) = session.as_ref()
            && automatic_history
        {
            remote_history::touch_viewed(&state.pool, account.id).await?;
            if let Err(error) = crate::remote_history::request_automatic(
                state,
                &account,
                remote_history::JobKind::Initial,
                Some(user.current.account.id),
            )
            .await
            {
                tracing::debug!(account = account.id, error = %error.chain(), "remote profile history enqueue skipped");
            }
            // Admission is local-only, so show its durable state in this same
            // response rather than rendering a stale idle control.
            snapshot = remote_history::snapshot(&state.pool, account.id).await?;
        }
        match snapshot.filter(|snapshot| snapshot.hydration_enabled) {
            Some(snapshot) => Some(remote_history_panel(
                &snapshot,
                session.as_ref(),
                account.id,
                &path,
                &clock_for(session.as_ref(), locale),
                locale,
            )),
            None => None,
        }
    } else {
        None
    };

    let featured = match &tab {
        ProfileTab::Featured => Some(featured_content(state, &account, viewer_id, locale).await?),
        _ => None,
    };
    // The hashtags featured on this profile, shown as chips on the Activity
    // tab linking to the per-tag view (Mastodon's `FeaturedTags` strip).
    let featured_tags = if matches!(tab, ProfileTab::Activity { .. }) {
        plamenu_db::featured_tag::list(&state.pool, account.id).await?
    } else {
        Vec::new()
    };

    let (relation, sanctions, posted_languages) =
        header_state(state, session.as_ref(), &account).await?;
    // A ranked view's pager and post-action redirects stay in the sort.
    let base = match sort {
        GroupSort::New => tab.base(&path),
        ranked => ranked.href(&path),
    };
    let viewer = viewer_id.map(|id| id.to_string());
    let ctx = view::Ctx {
        csrf: session.as_ref().map(|u| u.csrf.as_str()),
        viewer_id: viewer.as_deref(),
        return_to: &base,
        filter_context: Some(view::FilterContext::Account),
        prefs: view_prefs(state, session.as_ref()).await?,
        locale,
        clock: clock_for(session.as_ref(), locale),
        admin: session
            .as_ref()
            .map(WebUser::admin_capabilities)
            .unwrap_or_default(),
    };

    // A root-URL search can discover an already-running Owncast stream even
    // when this instance never received its go-live Note. In that one case,
    // show the account-level HLS player; when a live Note is already in the
    // feed it owns the player and this stays absent to avoid duplication.
    let account_live_media = if first_page
        && matches!(tab, ProfileTab::Activity { .. })
        && account.domain.is_some()
        && account.is_bot
    {
        match crate::owncast::profile_live_media(state, account.id).await? {
            Some(item) => {
                let allow_direct = allow_direct_media(&state.pool, viewer_id).await;
                vec![media_json_hls(
                    &state.config.domain,
                    &item,
                    allow_direct,
                    None,
                )]
            }
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };

    // Loaded before the composer gate, which reads the same policy.
    let facts = community_facts(state, &account, viewer_id).await?;
    let group_post_form = group_post_form_for(
        state,
        session.as_ref(),
        &account,
        &tab,
        first_page,
        facts.as_ref(),
    )
    .await?;
    // A community its own moderators mark sensitive gates its media behind the
    // reader's media preference, the same treatment a silenced author's posts
    // get — Lemmy's NSFW community, which we published for our own groups and
    // until now dropped on the way in.
    if facts.as_ref().is_some_and(|f| f.sensitive) {
        force_sensitive(&mut feed.entities);
        force_sensitive(&mut feed.pinned_entities);
    }
    let identity_proofs = crate::identity::list(state, &account).await?;
    let manage_group = group_manage_href(state, session.as_ref(), &account).await?;
    // A moderator of this (local) group gets the per-post Moderate actions in
    // every card's overflow menu, so moderation works from the group feed, not
    // just a post's own thread page.
    if manage_group.is_some() && account.is_group() {
        apply_group_mod(state, account.id, &mut feed).await?;
    }

    let body = html! {
        section.column {
            @if silenced { (silenced_notice(locale)) }
            (profile_header(&account_entity, session.as_ref(), relation.as_ref(), &sanctions, &posted_languages, manage_group.as_deref(), &ctx.clock, locale))
            (super::identity::profile_proofs(&identity_proofs, account.id, locale))
            // The section selector and the Activity tab's filter selector sit
            // side by side on one row.
            div.profile-nav {
                (section_tabs(&account, &path, &tab, locale))
                @if let ProfileTab::Activity { replies, boosts } = &tab {
                    // Groups sort by votes instead of filtering by kind —
                    // their Activity feed is all boosts by construction.
                    @if account.is_group() {
                        (group_sort_selector(&path, sort, locale))
                    } @else {
                        (activity_filter(&path, *replies, *boosts, locale))
                    }
                }
            }
            @if !account_live_media.is_empty() {
                aside.profile-live aria-label=(locale.text("status-live-badge")) {
                    (view::standalone_media(&account_live_media, &ctx))
                }
            }
            @if let Some(facts) = &facts { (community_panel(facts)) }
            @if let Some(form) = &group_post_form { (form) }
            @if history_eligible {
                div data-remote-history-region {
                    @if let Some(panel) = &history_panel { (panel) }
                    (tab_content(&tab, &path, &base, &feed, featured.as_ref(), &featured_tags, &ctx, sort))
                }
            } @else {
                (tab_content(&tab, &path, &base, &feed, featured.as_ref(), &featured_tags, &ctx, sort))
            }
        }
    };
    // The Account entity carries the owner's `noindex` opt-out for local
    // accounts (null/absent for remote ones); mark the page accordingly,
    // like Mastodon's `user_prefers_noindex?` header tag. The sub-sections
    // and paged/filtered views are noindex outright — only the canonical
    // first profile page is worth indexing.
    let canonical_page = max_id.is_none()
        && sort == GroupSort::New
        && matches!(
            tab,
            ProfileTab::Activity {
                replies: false,
                boosts: true,
            }
        );
    let noindex = !canonical_page
        || account_value
            .get("noindex")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    let meta = super::meta::account_page(
        super::meta::site_name(state).await?,
        &state.config.domain,
        &state.config.account_domain,
        &account_entity,
        noindex,
        locale,
    );
    Ok(layout::shell_visitor_subject_localized(
        account_entity.name(),
        session.as_ref(),
        anon_nav(state).await,
        &body,
        &meta,
        locale,
    )
    .into_response())
}

fn remote_history_panel(
    snapshot: &remote_history::Snapshot,
    session: Option<&WebUser>,
    account_id: i64,
    return_to: &str,
    clock: &ViewerClock,
    locale: Locale,
) -> Markup {
    let state_key = match snapshot.state.as_str() {
        "queued" => "profile-history-queued",
        "fetching" => "profile-history-fetching",
        "partial" => "profile-history-partial",
        "complete" => "profile-history-complete",
        "unsupported" => "profile-history-unsupported",
        "backoff" => "profile-history-backoff",
        _ if snapshot.available_statuses == 0 => "profile-history-empty",
        _ => "profile-history-idle",
    };
    let busy = matches!(snapshot.state.as_str(), "queued" | "fetching");
    let can_fetch = snapshot.hydration_enabled
        && session.is_some()
        && !busy
        && !matches!(snapshot.state.as_str(), "unsupported" | "backoff");
    let mode = if snapshot.next_page_uri.is_some() {
        "older"
    } else {
        "refresh"
    };
    let button = if mode == "older" {
        locale.text("profile-history-load-older")
    } else {
        locale.text("profile-history-refresh")
    };
    html! {
        aside.remote-history data-remote-history=(account_id)
            data-state-url=(format!("/web/accounts/{account_id}/remote_history"))
            data-history-state=(snapshot.state)
            data-history-available=(snapshot.available_statuses)
            data-history-background=(locale.text("profile-history-background"))
            aria-live="polite" {
            div.remote-history__copy {
                strong { (locale.text("profile-history-title")) }
                span.remote-history__state data-remote-history-state-text { (locale.text(state_key)) }
                @if snapshot.available_statuses > 0 {
                    span.remote-history__meta {
                        (snapshot.available_statuses) " " (locale.text("profile-history-available"))
                    }
                }
                @if let Some(refreshed) = snapshot.last_success_at {
                    span.remote-history__time {
                        (locale.text("profile-history-refreshed")) " "
                        (clock.element_absolute(refreshed))
                    }
                }
            }
            @if let Some(user) = session {
                @if can_fetch {
                    form.remote-history__form method="post"
                        action=(format!("/web/accounts/{account_id}/remote_history"))
                        data-remote-history-form {
                        input type="hidden" name="csrf" value=(user.csrf);
                        input type="hidden" name="return_to" value=(return_to);
                        input type="hidden" name="mode" value=(mode);
                        button type="submit" { (button) }
                    }
                } @else if busy && snapshot.hydration_enabled {
                    button type="button" disabled { (locale.text("profile-history-working")) }
                }
            }
        }
    }
}

/// A group page's "new post" form: rendered on the first Activity
/// page only (of any sort), for viewers a local group's posting policy allows
/// or, for a remote community, viewers who follow (subscribe to) it.
async fn group_post_form_for(
    state: &AppState,
    session: Option<&WebUser>,
    account: &Account,
    tab: &ProfileTab,
    first_page: bool,
    facts: Option<&CommunityFacts>,
) -> Result<Option<Markup>, ApiError> {
    let Some(user) = session else { return Ok(None) };
    if !account.is_group() || !first_page || !matches!(tab, ProfileTab::Activity { .. }) {
        return Ok(None);
    }
    let viewer_id = user.current.account.id;
    let allowed = match facts.map(|f| &f.origin) {
        // A local group we host: only those the posting policy allows see the
        // composer; moderators reach the management console from the "Manage
        // group" button in the profile header (rendered by `group_manage_href`
        // / `profile_header`).
        Some(CommunityOrigin::Hosted(group)) => {
            crate::groups::may_submit(state, group, viewer_id, true).await?
        }
        // A remote community: its own policy decides, as far as it tells
        // us. We still don't *enforce* it — the origin does, and a post we
        // waved through would simply be dropped there — so this is about not
        // offering a composer whose result we can predict will be rejected.
        Some(CommunityOrigin::Consumed) if !account.is_local() => {
            !account.suspended() && may_submit_remotely(state, facts, account, viewer_id).await?
        }
        // A Group account with no community document either way: the posture
        // before any of this was read, i.e. the origin decides.
        None if !account.is_local() => !account.suspended(),
        _ => false,
    };
    if !allowed {
        return Ok(None);
    }
    Ok(Some(super::groups::post_form(account.id, user.locale)))
}

/// Whether `viewer_id` may start a thread in a remote community, by the policy
/// the community publishes about itself. An origin that states nothing
/// is treated as open, which is what it was before we read these facts at all.
///
/// Membership is our accepted follow of the community — the same rule a hosted
/// group uses, and the one Lemmy applies to subscriptions.
async fn may_submit_remotely(
    state: &AppState,
    facts: Option<&CommunityFacts>,
    account: &Account,
    viewer_id: i64,
) -> Result<bool, ApiError> {
    let Some(facts) = facts else {
        return Ok(true);
    };
    Ok(match facts.posting {
        plamenu_db::group::PostingPolicy::Anyone => true,
        plamenu_db::group::PostingPolicy::Members => {
            plamenu_db::group::is_member(&state.pool, account.id, viewer_id).await?
        }
        plamenu_db::group::PostingPolicy::Mods => {
            plamenu_db::remote_group::is_moderator(&state.pool, account.id, viewer_id).await?
        }
    })
}

/// What a community states about itself, for the panel under its profile
/// header. A hosted group answers from the sidecar whose policy we enforce, a
/// remote one from the facts mirrored off its actor document — the reader is
/// shown the same three things either way.
struct CommunityFacts {
    sensitive: bool,
    posting: plamenu_db::group::PostingPolicy,
    /// Rendered account entities, in the order the community lists them.
    moderators: Vec<Value>,
    /// Where the facts came from — which also decides how the posting policy
    /// is worded: ours is enforced here, a remote one is reported.
    origin: CommunityOrigin,
}

/// A community we host (carrying the sidecar whose policy we enforce) or one
/// we consume. Loaded once per group page and shared by the panel and the
/// posting gate, so neither re-reads the other's rows.
enum CommunityOrigin {
    Hosted(plamenu_db::group::Group),
    Consumed,
}

impl CommunityFacts {
    fn hosted(&self) -> bool {
        matches!(self.origin, CommunityOrigin::Hosted(_))
    }
}

async fn community_facts(
    state: &AppState,
    account: &Account,
    viewer_id: Option<i64>,
) -> Result<Option<CommunityFacts>, ApiError> {
    if !account.is_group() {
        return Ok(None);
    }
    let (sensitive, posting, moderator_ids, origin) =
        if let Some(group) = plamenu_db::group::find(&state.pool, account.id).await? {
            let ids = plamenu_db::group::elevated(&state.pool, account.id)
                .await?
                .into_iter()
                .map(|entry| entry.account_id)
                .collect();
            let posting = group.posting_policy();
            (
                group.sensitive,
                posting,
                ids,
                CommunityOrigin::Hosted(group),
            )
        } else if let Some(facts) = plamenu_db::remote_group::find(&state.pool, account.id).await? {
            let ids = plamenu_db::remote_group::moderator_ids(&state.pool, account.id).await?;
            (
                facts.sensitive,
                facts.posting_policy(),
                ids,
                CommunityOrigin::Consumed,
            )
        } else {
            // A Group account we have never seen a community document for
            // (an actor fetched before this shipped, say): nothing to state.
            return Ok(None);
        };
    let moderators =
        render_accounts_by_ids(&state.pool, &state.config.domain, &moderator_ids, viewer_id)
            .await?;
    Ok(Some(CommunityFacts {
        sensitive,
        posting,
        moderators,
        origin,
    }))
}

/// The community panel: NSFW marking, who may post, and the moderator roster.
/// Rendered for every group profile, so a consumed community reads the way a
/// hosted one does.
fn community_panel(facts: &CommunityFacts) -> Markup {
    let posting_note = match (facts.posting, facts.hosted()) {
        (plamenu_db::group::PostingPolicy::Mods, _) => Some("Only moderators can start threads."),
        (plamenu_db::group::PostingPolicy::Members, true) => {
            Some("Only members can start threads.")
        }
        // A remote members-only policy is the origin's to enforce, and only a
        // peer running this software states it at all.
        (plamenu_db::group::PostingPolicy::Members, false) => {
            Some("This community accepts threads from its members.")
        }
        (plamenu_db::group::PostingPolicy::Anyone, _) => None,
    };
    if !facts.sensitive && posting_note.is_none() && facts.moderators.is_empty() {
        return html! {};
    }
    html! {
        section.community-facts {
            @if facts.sensitive {
                p.community-facts__flag { (view::icon("alert")) " Marked sensitive by its moderators." }
            }
            @if let Some(note) = posting_note {
                p.community-facts__note { (note) }
            }
            @if !facts.moderators.is_empty() {
                details.community-facts__mods {
                    summary { "Moderators (" (facts.moderators.len()) ")" }
                    ul.relationships-list {
                        @for value in &facts.moderators {
                            li.relationships-list__item {
                                (view::account_card(&view::Account(value)))
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The attendee panel under an event we host (E3): who is coming, who is
/// waiting, and the approve/reject buttons for the latter.
///
/// Organizer-only (or a moderator of a local group the event belongs to) — a
/// guest list is not public information. Absent entirely for a remote event: we
/// see only the participation activities addressed to us there, so a list built
/// from our own rows would be a misleading fraction of the real one.
async fn event_attendees_for(
    state: &AppState,
    session: Option<&WebUser>,
    item: &plamenu_db::status::Status,
    csrf: &str,
) -> Result<Option<Markup>, ApiError> {
    let Some(user) = session else { return Ok(None) };
    let Some(event) = plamenu_db::status_event::find(&state.pool, item.id).await? else {
        return Ok(None);
    };
    let organizer = plamenu_db::account::find_by_id(&state.pool, item.account_id).await?;
    if !organizer.is_some_and(|a| a.is_local()) {
        return Ok(None);
    }
    if !crate::events::may_moderate_event(state, &user.current.account, item).await? {
        return Ok(None);
    }
    let rows = plamenu_db::status_participation::for_status(&state.pool, item.id).await?;
    let account_ids: Vec<i64> = rows.iter().map(|row| row.account_id).collect();
    let accounts = crate::entities::render_accounts_by_ids(
        &state.pool,
        &state.config.domain,
        &account_ids,
        Some(user.current.account.id),
    )
    .await?;
    let entries: Vec<(plamenu_db::status_participation::Participation, Value)> =
        rows.into_iter().zip(accounts).collect();
    Ok(Some(super::view::event_attendees(
        item.id,
        &entries,
        event.is_cancelled(),
        csrf,
        user.locale,
    )))
}

/// The management-console link (`/groups/{id}/manage`) for a local group the
/// signed-in viewer owns or moderates, else `None`. Drives the "Manage group"
/// button in the profile header — the counterpart to the moderation routes'
/// own `moderated_group` gate.
async fn group_manage_href(
    state: &AppState,
    session: Option<&WebUser>,
    account: &Account,
) -> Result<Option<String>, ApiError> {
    let Some(user) = session else { return Ok(None) };
    if !account.is_group() || !account.is_local() {
        return Ok(None);
    }
    let manages = matches!(
        plamenu_db::group::affiliation_of(&state.pool, account.id, user.current.account.id).await?,
        Some(plamenu_db::group::Affiliation::Owner | plamenu_db::group::Affiliation::Moderator)
    );
    Ok(manages.then(|| format!("/groups/{}/manage", account.id)))
}

/// The content below a profile's header and selector row: the tab's feed
/// (as a media wall on the Media tab), pager and extras.
#[allow(clippy::too_many_arguments)] // one call site, mirrors the page's slots
fn tab_content(
    tab: &ProfileTab,
    path: &str,
    base: &str,
    feed: &TabFeed,
    featured: Option<&Markup>,
    featured_tags: &[plamenu_db::featured_tag::FeaturedTag],
    ctx: &view::Ctx,
    sort: GroupSort,
) -> Markup {
    html! {
        @match tab {
            ProfileTab::Activity { .. } => {
                (featured_tags_strip(path, featured_tags, ctx.locale))
                (view::pinned_feed(&feed.pinned_entities, ctx))
                // With pins shown above, an empty chronological feed
                // shouldn't claim "nothing here yet".
                @if feed.pinned_entities.is_empty() || !feed.entities.is_empty() {
                    (view::feed(&feed.entities, ctx))
                }
                @match sort {
                    GroupSort::New => {
                        (older_link_localized(base, &feed.statuses, LIMIT, ctx.locale))
                    }
                    ranked => (next_page_link(base, ranked, feed.statuses.len(), ctx.locale)),
                }
            }
            ProfileTab::Media => {
                (view::media_wall(&feed.entities, ctx))
                (older_link_localized(base, &feed.statuses, LIMIT, ctx.locale))
            }
            ProfileTab::Tagged(tag) => {
                h2.tag-title { "#" (tag) }
                (view::feed(&feed.entities, ctx))
                (older_link_localized(base, &feed.statuses, LIMIT, ctx.locale))
            }
            ProfileTab::Featured => {
                @if let Some(featured) = featured { (featured) }
            }
        }
    }
}

/// The Featured tab's contents: the accounts this profile features
/// (Mastodon's endorsements, under a "Profiles" heading) and its FEP-7aa9
/// collections, each a curated set of accounts. An empty state when the
/// profile features nothing.
async fn featured_content(
    state: &AppState,
    account: &Account,
    viewer_id: Option<i64>,
    locale: Locale,
) -> Result<Markup, ApiError> {
    let featured = featured_accounts(state, account.id, viewer_id).await?;
    let collections =
        super::collections::profile_section(state, account, viewer_id, locale).await?;
    if featured.is_empty() && collections.0.is_empty() {
        return Ok(html! { p.empty { (locale.text("profile-nothing-featured")) } });
    }
    Ok(html! {
        (featured_section(&featured, locale))
        (collections)
    })
}

/// The viewer-dependent state the profile header renders from: the
/// viewer→account relationship for the follow control (never for one's own
/// profile), the admin sanction state for the badge row (logged-in viewers
/// only — an anonymous visitor to a silenced profile already gets the stub
/// page), and the languages this account posts in, which are all the
/// follow-settings language filter offers (the full inventory is noise
/// there).
async fn header_state(
    state: &AppState,
    session: Option<&WebUser>,
    account: &Account,
) -> Result<(Option<Value>, Sanctions, Vec<String>), ApiError> {
    let relation = match session {
        Some(user) if user.current.account.id != account.id => {
            Some(relationship_json(&state.pool, user.current.account.id, account).await?)
        }
        _ => None,
    };
    let sanctions = match session {
        Some(_) => sanctions_of(state, account).await?,
        None => Sanctions::default(),
    };
    let posted_languages = if relation.is_some() {
        status::languages_by_account(&state.pool, account.id).await?
    } else {
        Vec::new()
    };
    Ok((relation, sanctions, posted_languages))
}

/// A profile tab's rendered feed: the chronological statuses (for the pager)
/// and their entities, plus the pinned posts leading the Activity tab.
struct TabFeed {
    statuses: Vec<Status>,
    entities: Vec<Value>,
    pinned_entities: Vec<Value>,
}

/// Injects group-moderation context onto every card of a local group's feed so
/// its moderators reach the overflow-menu Moderate actions (Remove / Lock /
/// Pin) from the feed, not only a post's thread page. Pinned state is the
/// group's featured set (already surfaced as the pinned leaders); locked state
/// comes from the group's thread locks. The context rides the boosted object,
/// which is what the card shows and the actions target.
async fn apply_group_mod(
    state: &AppState,
    group_id: i64,
    feed: &mut TabFeed,
) -> Result<(), ApiError> {
    use std::collections::HashSet;
    let pinned: HashSet<i64> = feed
        .pinned_entities
        .iter()
        .filter_map(view::displayed_status_id)
        .collect();
    let ids: Vec<i64> = feed
        .entities
        .iter()
        .chain(feed.pinned_entities.iter())
        .filter_map(view::displayed_status_id)
        .collect();
    let locked: HashSet<i64> = plamenu_db::group::locked_of(&state.pool, &ids)
        .await?
        .into_iter()
        .collect();
    for entity in feed
        .pinned_entities
        .iter_mut()
        .chain(feed.entities.iter_mut())
    {
        if let Some(id) = view::displayed_status_id(entity) {
            view::inject_group_mod(entity, group_id, pinned.contains(&id), locked.contains(&id));
        }
    }
    Ok(())
}

/// Loads and renders the statuses a profile tab shows; the Featured tab has
/// none. The Media tab's reply inclusion is the owner's `show_media_replies`
/// setting, like Mastodon's gallery.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "one profile tab's feed keeps its account, group-sort, pin, live-refresh and rendering paths together"
)]
async fn tab_feed(
    state: &AppState,
    account: &Account,
    tab: &ProfileTab,
    max_id: Option<i64>,
    viewer_id: Option<i64>,
    order: TimelineOrder,
    silenced: bool,
    sort: GroupSort,
) -> Result<TabFeed, ApiError> {
    // The vote-ranked group sorts read the group's boost rows in
    // score order — offset pages, no keyset, no pins.
    match sort {
        GroupSort::New => {}
        GroupSort::Top(window, page) => {
            let statuses = plamenu_db::group::timeline_top(
                &state.pool,
                account.id,
                window,
                LIMIT,
                page * LIMIT,
            )
            .await?;
            let entities = render_live_rows(state, &statuses, viewer_id).await?;
            return Ok(TabFeed {
                statuses,
                entities,
                pinned_entities: Vec::new(),
            });
        }
        GroupSort::Hot(page) => {
            let statuses =
                plamenu_db::group::timeline_hot(&state.pool, account.id, LIMIT, page * LIMIT)
                    .await?;
            let entities = render_live_rows(state, &statuses, viewer_id).await?;
            return Ok(TabFeed {
                statuses,
                entities,
                pinned_entities: Vec::new(),
            });
        }
    }
    let filter = match tab {
        ProfileTab::Activity { replies, boosts } => Some(status::AccountStatusesFilter {
            exclude_replies: !replies,
            exclude_reblogs: !boosts,
            only_media: false,
            media_through_reblog: false,
            tagged: None,
            max_id,
            since_id: None,
        }),
        ProfileTab::Media => Some(status::AccountStatusesFilter {
            exclude_replies: !account.show_media_replies,
            exclude_reblogs: false,
            only_media: true,
            // A group's feed is all announces, its media on the boosted post;
            // look through the reblog so the group's Media wall isn't empty. A
            // person's Media tab keeps its own-media-only semantics (boosts,
            // having no attachments of their own, never match).
            media_through_reblog: account.is_group(),
            tagged: None,
            max_id,
            since_id: None,
        }),
        ProfileTab::Tagged(tag) => Some(status::AccountStatusesFilter {
            exclude_replies: false,
            exclude_reblogs: false,
            only_media: false,
            media_through_reblog: false,
            tagged: Some(tag.clone()),
            max_id,
            since_id: None,
        }),
        ProfileTab::Featured => None,
    };
    let statuses = match &filter {
        Some(filter) => {
            status::by_account(&state.pool, account.id, viewer_id, filter, order, LIMIT).await?
        }
        None => Vec::new(),
    };
    // Pinned posts lead the Activity tab's first page, Mastodon-style,
    // each visibility-checked (a followers-only pin stays hidden from
    // strangers) and dropped from the chronological feed below so the page
    // doesn't show them twice.
    let show_pins = matches!(tab, ProfileTab::Activity { .. }) && max_id.is_none();
    let pinned = if show_pins {
        visible(
            state,
            pin::pinned_statuses(&state.pool, account.id).await?,
            viewer_id,
        )
        .await?
    } else {
        Vec::new()
    };
    let statuses: Vec<_> = statuses
        .into_iter()
        .filter(|s| !pinned.iter().any(|p| p.id == s.id))
        .collect();
    let mut pinned_entities = render_live_rows(state, &pinned, viewer_id).await?;
    let mut entities = render_live_rows(state, &statuses, viewer_id).await?;
    // For a logged-in viewer, a silenced author's media is forced sensitive.
    // The entity carries only the `sensitive` flag; the viewer's own
    // media-display preference still decides whether it's revealed or gated.
    if silenced {
        force_sensitive(&mut pinned_entities);
        force_sensitive(&mut entities);
    }
    Ok(TabFeed {
        statuses,
        entities,
        pinned_entities,
    })
}

/// The profile section tabs under the header: Activity, plus Media and
/// Featured where the owner's settings allow them. With both extras off
/// there's only one section, so no strip at all (Mastodon renders a bare
/// rule there).
fn section_tabs(account: &Account, path: &str, tab: &ProfileTab, locale: Locale) -> Markup {
    // Featured is a person-only concept (endorsed accounts + collections), so a
    // group only ever offers Activity and Media.
    let show_featured = account.show_featured && !account.is_group();
    if !account.show_media && !show_featured {
        return html! {};
    }
    let media = format!("{path}/media");
    let featured = format!("{path}/featured");
    let activity_label = locale.text("profile-tab-activity");
    let media_label = locale.text("profile-tab-media");
    let featured_label = locale.text("profile-tab-featured");
    let mut tabs = vec![view::Tab::new(
        path,
        &activity_label,
        matches!(tab, ProfileTab::Activity { .. } | ProfileTab::Tagged(_)),
    )];
    if account.show_media {
        tabs.push(view::Tab::new(
            &media,
            &media_label,
            matches!(tab, ProfileTab::Media),
        ));
    }
    if show_featured {
        tabs.push(view::Tab::new(
            &featured,
            &featured_label,
            matches!(tab, ProfileTab::Featured),
        ));
    }
    view::tab_strip(&locale.text("profile-sections"), &tabs)
}

/// The Activity tab's reply/boost filter as a second selector — the four
/// states of Mastodon's "Show replies" / "Show boosts" toggles, as plain
/// links carrying the query flags.
fn activity_filter(path: &str, replies: bool, boosts: bool, locale: Locale) -> Markup {
    let all = format!("{path}?replies=1");
    let posts_replies = format!("{path}?replies=1&boosts=0");
    let posts = format!("{path}?boosts=0");
    view::tab_strip(
        &locale.text("profile-activity-filter"),
        &[
            view::Tab::new(
                path,
                &locale.text("profile-filter-posts-boosts"),
                !replies && boosts,
            ),
            view::Tab::new(&all, &locale.text("profile-filter-all"), replies && boosts),
            view::Tab::new(
                &posts_replies,
                &locale.text("profile-filter-posts-replies"),
                replies && !boosts,
            ),
            view::Tab::new(&posts, &locale.text("profile-posts"), !replies && !boosts),
        ],
    )
}

/// The group page's sort selector: New / Hot / Top, with Top's time
/// window as a second strip once Top is active — Lemmy's sorts on the same
/// `tab_strip` control every profile selector uses.
fn group_sort_selector(path: &str, sort: GroupSort, locale: Locale) -> Markup {
    use plamenu_db::group::TopWindow;
    let hot = GroupSort::Hot(0).href(path);
    let top = GroupSort::Top(TopWindow::Week, 0).href(path);
    let strip = view::tab_strip(
        &locale.text("profile-sort"),
        &[
            view::Tab::new(
                path,
                &locale.text("profile-sort-new"),
                sort == GroupSort::New,
            ),
            view::Tab::new(
                &hot,
                &locale.text("profile-sort-hot"),
                matches!(sort, GroupSort::Hot(_)),
            ),
            view::Tab::new(
                &top,
                &locale.text("profile-sort-top"),
                matches!(sort, GroupSort::Top(..)),
            ),
        ],
    );
    html! {
        (strip)
        @if let GroupSort::Top(window, _) = sort {
            @let day = GroupSort::Top(TopWindow::Day, 0).href(path);
            @let week = GroupSort::Top(TopWindow::Week, 0).href(path);
            @let month = GroupSort::Top(TopWindow::Month, 0).href(path);
            @let all = GroupSort::Top(TopWindow::All, 0).href(path);
            (view::tab_strip(
                &locale.text("profile-top-window"),
                &[
                    view::Tab::new(
                        &day,
                        &locale.text("profile-window-today"),
                        window == TopWindow::Day,
                    ),
                    view::Tab::new(
                        &week,
                        &locale.text("profile-window-week"),
                        window == TopWindow::Week,
                    ),
                    view::Tab::new(
                        &month,
                        &locale.text("profile-window-month"),
                        window == TopWindow::Month,
                    ),
                    view::Tab::new(
                        &all,
                        &locale.text("profile-window-all"),
                        window == TopWindow::All,
                    ),
                ],
            ))
        }
    }
}

/// The ranked sorts' pager: rank orders don't keyset, so Top/Hot page by
/// number — a full page implies more may follow.
fn next_page_link(base: &str, sort: GroupSort, fetched: usize, locale: Locale) -> Markup {
    if fetched < usize::try_from(LIMIT).unwrap_or(usize::MAX) {
        return html! {};
    }
    let sep = if base.contains('?') { '&' } else { '?' };
    let href = format!("{base}{sep}page={}", sort.page() + 1);
    html! { nav.pager { a.pager__more href=(href) { (locale.text("profile-load-more-posts")) } } }
}

/// The hashtags featured on this profile as a chip row, each linking to the
/// account's posts under that tag (Mastodon's featured-hashtags strip).
fn featured_tags_strip(
    path: &str,
    tags: &[plamenu_db::featured_tag::FeaturedTag],
    locale: Locale,
) -> Markup {
    html! {
        @if !tags.is_empty() {
            nav aria-label=(locale.text("profile-featured-hashtags")) {
                ul.hashtag-list {
                    @for tag in tags {
                        li {
                            a.hashtag href=(format!("{path}/tagged/{}", tag.name)) {
                                "#" (tag.name)
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The profile a silenced account shows to an anonymous visitor: just the
/// handle and a notice. No avatar, banner, bio, fields, counts or posts. A
/// remote account also offers a link to its original page, since the real
/// profile still lives on its home server.
async fn silenced_stub_page(
    state: &AppState,
    account: &Account,
    locale: Locale,
) -> Result<Response, ApiError> {
    let account_value = account_json(&state.pool, &state.config.domain, account, None).await?;
    let account_entity = view::Account(&account_value);
    let handle = account_entity.acct();
    let url = account_entity.url();
    let can_view_original = account_entity.is_remote() && !url.is_empty();
    let body = html! {
        section.column {
            div.profile-silenced {
                h1.profile-silenced__handle { (account_entity.handle_prefix()) (handle) }
                p.profile-silenced__notice {
                    (locale.text("profile-silenced-public"))
                }
                @if can_view_original {
                    a.profile-silenced__link href=(url)
                        target="_blank" rel="noopener noreferrer" {
                        (locale.text("status-open-original-page"))
                    }
                }
            }
        }
    };
    // Bare noindex metadata only: a hidden profile shouldn't leak a preview
    // card either.
    let meta = layout::PageMeta {
        noindex: true,
        ..layout::PageMeta::default()
    };
    Ok(layout::shell_visitor_subject_localized(
        handle,
        None,
        anon_nav(state).await,
        &body,
        &meta,
        locale,
    )
    .into_response())
}

/// The banner shown above a silenced account's profile to a logged-in viewer,
/// who otherwise sees the profile in full.
fn silenced_notice(locale: Locale) -> Markup {
    html! {
        div.profile-silenced-banner role="note" {
            (locale.text("profile-silenced-member"))
        }
    }
}

/// Forces every rendered status entity (and any boosted status it wraps) to
/// `sensitive`, so a silenced author's media is gated behind the viewer's
/// media-display preference.
fn force_sensitive(entities: &mut [Value]) {
    for entity in entities {
        entity["sensitive"] = Value::Bool(true);
        if entity.get("reblog").is_some_and(Value::is_object) {
            entity["reblog"]["sensitive"] = Value::Bool(true);
        }
    }
}

/// The accounts a profile features on itself (endorsements), most recently
/// pinned first, as rendered entities ready for [`featured_section`].
async fn featured_accounts(
    state: &AppState,
    account_id: i64,
    viewer_id: Option<i64>,
) -> Result<Vec<Value>, ApiError> {
    const FEATURED_LIMIT: i64 = 12;
    let ids: Vec<i64> =
        plamenu_db::endorsement::list(&state.pool, account_id, None, None, FEATURED_LIMIT)
            .await?
            .into_iter()
            .map(|e| e.target_account_id)
            .collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut accounts = account::find_by_ids(&state.pool, &ids).await?;
    accounts.sort_by_key(|a| ids.iter().position(|id| *id == a.id).unwrap_or(usize::MAX));
    render_accounts(&state.pool, &state.config.domain, &accounts, viewer_id).await
}

/// The endorsed accounts as compact cards on the Featured tab — Mastodon's
/// "Profiles" heading there. Renders nothing when the profile features
/// nobody.
fn featured_section(featured: &[Value], locale: Locale) -> Markup {
    html! {
        @if !featured.is_empty() {
            section.profile-featured {
                h2.profile-featured__title { (locale.text("profile-featured-profiles")) }
                div.profile-featured__grid {
                    @for value in featured {
                        (view::account_card(&view::Account(value)))
                    }
                }
            }
        }
    }
}

/// Admin moderation state of the profiled account, feeding the badge row.
/// `silenced` is the account's own `silenced_at` mark; a domain-level silence
/// shows separately via `domain_severity`, so the two aren't conflated.
#[derive(Default)]
struct Sanctions {
    suspended: bool,
    silenced: bool,
    /// The admin domain block's severity (`silence`/`suspend`/`noop`) on the
    /// account's home server, when one exists.
    domain_severity: Option<String>,
}

/// Reads an account's admin sanction state for the profile badge row.
async fn sanctions_of(state: &AppState, account: &Account) -> Result<Sanctions, ApiError> {
    let domain_severity = match account
        .domain
        .as_ref()
        .filter(|_| !account.is_portable_on(&state.config.domain))
    {
        Some(domain) => instance_policy::find_domain_block_by_domain(&state.pool, domain)
            .await?
            .map(|block| block.severity),
        None => None,
    };
    Ok(Sanctions {
        suspended: account.suspended(),
        silenced: account.silenced_at.is_some(),
        domain_severity,
    })
}

/// The moderation-state badges under the handle: how the viewer has
/// sanctioned this account (mute / block / domain block, plus "blocks you")
/// and what admin sanctions apply to the account or its server. Renders
/// nothing when no state applies.
fn moderation_badges(
    account: &view::Account,
    relation: Option<&Value>,
    sanctions: &Sanctions,
    locale: Locale,
) -> Markup {
    let flag = |key: &str| relation.and_then(|r| r.get(key)).and_then(Value::as_bool) == Some(true);
    let muting = flag("muting");
    let blocking = flag("blocking");
    let blocked_by = flag("blocked_by");
    let domain_blocking = flag("domain_blocking");
    let server_limited = sanctions.domain_severity.as_deref() == Some("silence");
    let server_suspended = sanctions.domain_severity.as_deref() == Some("suspend");
    let any = muting
        || blocking
        || blocked_by
        || domain_blocking
        || sanctions.suspended
        || sanctions.silenced
        || server_limited
        || server_suspended;
    if !any {
        return html! {};
    }
    let domain = account.remote_domain().unwrap_or_default();
    let mut domain_args = FluentArgs::new();
    domain_args.set("domain", domain);
    html! {
        div.profile__moderation {
            @if blocking {
                span.profile__badge.profile__badge--danger
                    title=(locale.text("profile-badge-blocked-title")) {
                    (locale.text("profile-badge-blocked"))
                }
            }
            @if blocked_by {
                span.profile__badge.profile__badge--danger
                    title=(locale.text("profile-badge-blocks-you-title")) {
                    (locale.text("profile-badge-blocks-you"))
                }
            }
            @if muting {
                span.profile__badge.profile__badge--warn
                    title=(locale.text("profile-badge-muted-title")) {
                    (locale.text("profile-badge-muted"))
                }
            }
            @if domain_blocking {
                span.profile__badge.profile__badge--danger
                    title=(locale.text_with("profile-badge-domain-blocked-title", &domain_args)) {
                    (locale.text("profile-badge-domain-blocked"))
                }
            }
            @if sanctions.suspended {
                span.profile__badge.profile__badge--danger
                    title=(locale.text("profile-badge-suspended-title")) {
                    (locale.text("profile-badge-suspended"))
                }
            }
            @if sanctions.silenced {
                span.profile__badge.profile__badge--warn
                    title=(locale.text("profile-badge-limited-title")) {
                    (locale.text("profile-badge-limited"))
                }
            }
            @if server_suspended {
                span.profile__badge.profile__badge--danger
                    title=(locale.text_with("profile-badge-server-suspended-title", &domain_args)) {
                    (locale.text("profile-badge-server-suspended"))
                }
            }
            @if server_limited {
                span.profile__badge.profile__badge--warn
                    title=(locale.text_with("profile-badge-server-limited-title", &domain_args)) {
                    (locale.text("profile-badge-server-limited"))
                }
            }
        }
    }
}

/// The badges beside the display name: kind (Group/Bot), locked, and the
/// account's publicly highlighted server roles — the same `roles` array
/// 3rd-party clients render, coloured by each role's configured colour.
fn identity_badges(account: &view::Account, locale: Locale) -> Markup {
    html! {
        @if account.group() {
            span.profile__badge.profile__badge--bot title=(locale.text("profile-group")) {
                (locale.text("profile-group"))
            }
        }
        @if account.bot() {
            span.profile__badge.profile__badge--bot
                title=(locale.text("profile-bot-title")) { (locale.text("profile-bot")) }
        }
        @if account.locked() {
            span.profile__badge.profile__badge--locked
                title=(locale.text("profile-manual-approval-title")) {
                    "🔒 " (locale.text("profile-manual-approval"))
                }
        }
        @for (name, color) in account.roles() {
            span.profile__badge.profile__badge--role
                style=[super::badge_color(color).map(|color| format!("--role-color: {color}"))]
                title=(locale.text("profile-server-role")) { (name) }
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the profile header assembles independent account, viewer, moderation, follow and locale state"
)]
fn profile_header(
    account: &view::Account,
    user: Option<&WebUser>,
    relation: Option<&Value>,
    sanctions: &Sanctions,
    posted_languages: &[String],
    manage_group_href: Option<&str>,
    clock: &ViewerClock,
    locale: Locale,
) -> Markup {
    // Signed-in viewers get external links in the bio/fields routed through the
    // in-app resolver (remote group descriptions link to sibling communities).
    let resolve_links = user.is_some();
    let fields = account.fields(resolve_links);
    // A group or bot didn't "join" — it was created. Persons joined.
    let created_verb = if account.group() || account.bot() {
        locale.text("profile-created")
    } else {
        locale.text("profile-joined")
    };
    let privileged_menu = view::account_privileged_menu(
        account,
        user.map(WebUser::admin_capabilities).unwrap_or_default(),
        manage_group_href,
        locale,
    );
    html! {
        header.profile.has-header[account.header().is_some()] {
            (profile_banner(account))
            // Avatar beside the identity block instead of above it, and the
            // handle and join date on one line: the header stays shallow.
            div.profile__top {
                img.profile__avatar src=(account.avatar()) alt=(account.avatar_description())
                    width="80" height="80";
                div.profile__id {
                    div.profile__name-row {
                        h1.profile__name { (account.name_markup()) }
                        (identity_badges(account, locale))
                        (profile_menu(account, user, relation, locale))
                    }
                    p.profile__meta {
                        span.profile__acct { (account.handle_prefix()) (account.acct()) }
                        @if !account.created_at().is_empty() {
                            @let joined = clock.tooltip_iso(account.created_at());
                            span.profile__joined title=(joined) {
                                " · " (created_verb) " "
                                (clock.month_year_iso(account.created_at()))
                            }
                        }
                    }
                }
            }
            (moderation_badges(account, relation, sanctions, locale))
            @if !account.note_html().is_empty() {
                div.profile__note { (account.note_markup(resolve_links)) }
            }
            @if !fields.is_empty() {
                dl.profile__fields {
                    @for (name, value, verified) in fields {
                        div.profile__field.is-verified[verified] {
                            dt.profile__field-name { (name) }
                            dd.profile__field-value {
                                (value)
                                @if verified {
                                    " " span.profile__field-verified
                                        title=(locale.text("profile-verified-title")) {
                                        (view::icon("check")) " " (locale.text("profile-verified"))
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // Stats on the left; the action buttons (Manage group, Follow)
            // cluster on the right, with the follow-settings disclosure wrapping
            // onto its own full-width line below.
            div.profile__footer {
                div.profile__stats {
                    div.profile__stat {
                        span.profile__stat-value { (account.statuses_count()) }
                        span.profile__stat-label { (locale.text("profile-posts")) }
                    }
                    a.profile__stat.profile__stat--link
                        href=(format!("{}/following", account.profile_path())) {
                        span.profile__stat-value { (account.following_count()) }
                        span.profile__stat-label { (locale.text("profile-following")) }
                    }
                    a.profile__stat.profile__stat--link
                        href=(format!("{}/followers", account.profile_path())) {
                        span.profile__stat-value { (account.followers_count()) }
                        span.profile__stat-label { (locale.text("profile-followers")) }
                    }
                }
                @if privileged_menu.is_some() || (user.is_some() && relation.is_some())
                    || (user.is_none() && !account.uri().is_empty()) {
                    div.profile__actions {
                        @if let (Some(user), Some(relation)) = (user, relation) {
                            (follow_button(account, &user.csrf, relation, locale))
                        } @else if user.is_none() && !account.uri().is_empty() {
                            // A logged-out visitor follows from their own
                            // server via the interstitial.
                            (super::interact::follow_via_interact(account.uri(), locale))
                        }
                        // Keep the popup trigger at the trailing edge. Its
                        // right-aligned panel then stays inside narrow mobile
                        // viewports even when a wide Follow button precedes it.
                        @if let Some(menu) = &privileged_menu {
                            (menu)
                        }
                    }
                }
                @if let (Some(user), Some(relation)) = (user, relation) {
                    @let following = relation.get("following").and_then(Value::as_bool) == Some(true);
                    @let requested = relation.get("requested").and_then(Value::as_bool) == Some(true);
                    @if following || requested {
                        (follow_settings_form(account, &user.csrf, relation, posted_languages, locale))
                    }
                    (account_note_form(account, &user.csrf, relation, locale))
                }
            }
        }
    }
}

fn profile_banner(account: &view::Account<'_>) -> Markup {
    html! {
        @if let Some(header) = account.header() {
            img.profile__header src=(header) alt=(account.header_description());
        }
    }
}

/// The profile overflow menu ("···"), reusing the status menu's markup so its
/// JS (single-open, outside-click close, viewport-aware placement) and styling
/// apply unchanged. It carries the whole-account counterparts of the status
/// menu's author actions: add/remove from lists, feature/unfeature on the
/// viewer's profile (endorsements), view original page, mute/unmute,
/// block/unblock, report, and the remote server's domain block.
#[allow(
    clippy::too_many_lines,
    reason = "one declarative menu keeps all mutually exclusive account actions together"
)]
fn profile_menu(
    account: &view::Account,
    user: Option<&WebUser>,
    relation: Option<&Value>,
    locale: Locale,
) -> Markup {
    let url = account.url();
    let acct = account.acct();
    let return_to = account.profile_path();
    let has_actions = user.is_some() && relation.is_some();
    let can_view_original = account.is_remote() && !url.is_empty();
    // Local accounts serve an Atom feed at `/@name.atom` — the one menu
    // entry a logged-out visitor gets on a local profile.
    let feed_href = (!account.is_remote()).then(|| format!("{return_to}.atom"));
    if !has_actions && !can_view_original && feed_href.is_none() {
        return html! {};
    }
    html! {
        details.status__menu.profile__menu data-status-menu {
            summary.action title=(locale.text("status-more-options")) { (view::icon("more")) }
            div.status__menu-pop role="menu" {
                @if !url.is_empty() {
                    button.status__menu-item.status__menu-item--js type="button"
                        data-copy-link=(url) { (locale.text("profile-copy-link")) }
                }
                @if can_view_original {
                    a.status__menu-item href=(url) target="_blank" rel="noopener noreferrer" {
                        (locale.text("profile-view-original"))
                    }
                }
                @if let Some(feed) = &feed_href {
                    a.status__menu-item href=(feed) { (locale.text("profile-atom-feed")) }
                }
                @if let (Some(user), Some(relation)) = (user, relation) {
                    @if user.can(plamenu_db::role::permission::UPLOAD_CUSTOM_EMOJIS)
                        && account.has_emojis() {
                        a.status__menu-item href=(format!("/settings/custom-emojis/borrow/account/{}", account.id())) {
                            (locale.text("custom-emojis-borrow-personal"))
                        }
                    }
                    @let flag = |key: &str|
                        relation.get(key).and_then(Value::as_bool) == Some(true);
                    @let dm_text = format!("@{acct} ");
                    @let dm_query = serde_urlencoded::to_string(
                        [("visibility", "direct"), ("text", dm_text.as_str())])
                        .unwrap_or_default();
                    a.status__menu-item href=(format!("/compose?{dm_query}")) {
                        (locale.text("profile-send-private-mention"))
                    }
                    @let list_query = serde_urlencoded::to_string(
                        [("return_to", return_to.as_str())]).unwrap_or_default();
                    a.status__menu-item
                        href=(format!("/web/accounts/{}/lists?{list_query}", account.id())) {
                        (locale.text("profile-manage-lists"))
                    }
                    a.status__menu-item
                        href=(format!("/web/accounts/{}/collections?{list_query}", account.id())) {
                        (locale.text("profile-feature-collections"))
                    }
                    @if flag("endorsed") {
                        (view::menu_form(&format!("/web/accounts/{}/unendorse", account.id()),
                            &locale.text("profile-unfeature"), false, None, &user.csrf, &return_to))
                    } @else {
                        (view::menu_form(&format!("/web/accounts/{}/endorse", account.id()),
                            &locale.text("profile-feature"), false, None, &user.csrf, &return_to))
                    }
                    @let mut account_args = FluentArgs::new();
                    @let () = account_args.set("account", acct);
                    @if flag("muting") {
                        (view::menu_form(&format!("/web/accounts/{}/unmute", account.id()),
                            &locale.text_with("status-unmute-account", &account_args),
                            false, None, &user.csrf, &return_to))
                    } @else {
                        (view::menu_form(&format!("/web/accounts/{}/mute", account.id()),
                            &locale.text_with("status-mute-account", &account_args),
                            false, None, &user.csrf, &return_to))
                    }
                    @if flag("blocking") {
                        (view::menu_form(&format!("/web/accounts/{}/unblock", account.id()),
                            &locale.text_with("status-unblock-account", &account_args),
                            false, None, &user.csrf, &return_to))
                    } @else {
                        (view::menu_form(&format!("/web/accounts/{}/block", account.id()),
                            &locale.text_with("status-block-account", &account_args), true,
                            Some(&locale.text_with("status-block-account-confirm", &account_args)),
                            &user.csrf, &return_to))
                    }
                    @let report_query = serde_urlencoded::to_string(
                        [("return_to", return_to.as_str())]).unwrap_or_default();
                    a.status__menu-item.is-danger
                        href=(format!("/web/accounts/{}/report?{report_query}", account.id())) {
                        (locale.text_with("status-report-account", &account_args))
                    }
                    @if let Some(domain) = account.remote_domain() {
                        @if flag("domain_blocking") {
                            @let mut domain_args = FluentArgs::new();
                            @let () = domain_args.set("domain", domain);
                            (profile_domain_form("/web/domains/unblock",
                                &locale.text_with("profile-unblock-server", &domain_args),
                                domain, None,
                                &user.csrf, &return_to))
                        } @else {
                            @let mut domain_args = FluentArgs::new();
                            @let () = domain_args.set("domain", domain);
                            (profile_domain_form("/web/domains/block",
                                &locale.text_with("profile-block-server", &domain_args), domain,
                                Some(&locale.text_with(
                                    "status-block-domain-confirm", &domain_args)),
                                &user.csrf, &return_to))
                        }
                    }
                }
            }
        }
    }
}

/// A domain (un)block verb as a menu POST form. Like `view::menu_form` but
/// carries the extra hidden `domain` field the domain-block routes expect; a
/// `confirm` prompt and danger styling mark the destructive block direction.
fn profile_domain_form(
    action: &str,
    label: &str,
    domain: &str,
    confirm: Option<&str>,
    csrf: &str,
    return_to: &str,
) -> Markup {
    html! {
        form.status__menu-form method="post" action=(action) data-confirm=[confirm] {
            input type="hidden" name="csrf" value=(csrf);
            input type="hidden" name="return_to" value=(return_to);
            input type="hidden" name="domain" value=(domain);
            button.status__menu-item.is-danger[confirm.is_some()] type="submit" { (label) }
        }
    }
}

/// The follow / unfollow / cancel-request button, driven by the relationship
/// entity. The per-follow settings (M32) render separately below the action
/// cluster (`follow_settings_form`), so this stays a single button.
fn follow_button(account: &view::Account, csrf: &str, relation: &Value, locale: Locale) -> Markup {
    let following = relation.get("following").and_then(Value::as_bool) == Some(true);
    let requested = relation.get("requested").and_then(Value::as_bool) == Some(true);
    let (verb, label) = if following {
        ("unfollow", locale.text("profile-unfollow"))
    } else if requested {
        ("unfollow", locale.text("profile-cancel-request"))
    } else {
        ("follow", locale.text("profile-follow"))
    };
    let path = format!("/web/accounts/{}/{verb}", account.id());
    html! {
        form.follow method="post" action=(path) {
            input type="hidden" name="csrf" value=(csrf);
            input type="hidden" name="return_to" value=(account.profile_path());
            button type="submit" { (label) }
        }
    }
}

/// The per-follow settings as a collapsed disclosure under the follow
/// button: notify on new posts, show boosts, show replies, and the
/// home-timeline language filter. All controls submit absent-when-off, so
/// saving replaces all four settings.
///
/// The replies box is the only place a user finds out that a community or bot
/// follow arrived with replies already off (the default depends on
/// the target), hence the hint under it — an unchecked box there is intended,
/// not a bug.
///
/// The language filter offers only the languages this account has actually
/// posted in (plus any already-filtered codes, so a stale selection stays
/// editable). Untagged posts always pass the filter, so when that list is
/// empty the control would do nothing and is omitted.
fn follow_settings_form(
    account: &view::Account,
    csrf: &str,
    relation: &Value,
    posted_languages: &[String],
    locale: Locale,
) -> Markup {
    let notifying = relation.get("notifying").and_then(Value::as_bool) == Some(true);
    let showing_reblogs = relation.get("showing_reblogs").and_then(Value::as_bool) == Some(true);
    let showing_replies = relation.get("showing_replies").and_then(Value::as_bool) == Some(true);
    let selected: Vec<&str> = relation
        .get("languages")
        .and_then(Value::as_array)
        .map(|langs| langs.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut codes: Vec<&str> = posted_languages.iter().map(String::as_str).collect();
    for code in &selected {
        if !codes.contains(code) {
            codes.push(code);
        }
    }
    codes.sort_by_key(|code| {
        languages::find(code).map_or_else(|| (*code).to_string(), languages::Language::label)
    });
    let path = format!("/web/accounts/{}/follow_settings", account.id());
    html! {
        details.follow-settings {
            summary { (locale.text("profile-follow-settings")) }
            form.follow-settings__form method="post" action=(path) {
                input type="hidden" name="csrf" value=(csrf);
                input type="hidden" name="return_to" value=(account.profile_path());
                label.follow-settings__toggle {
                    input type="checkbox" name="notify" checked[notifying];
                    " " (locale.text("profile-notify-posts"))
                }
                label.follow-settings__toggle {
                    input type="checkbox" name="show_reblogs" checked[showing_reblogs];
                    " " (locale.text("profile-show-boosts"))
                }
                div.follow-settings__field {
                    label.follow-settings__toggle {
                        input type="checkbox" name="with_replies" checked[showing_replies];
                        " " (locale.text("profile-show-replies"))
                    }
                    span.follow-settings__hint { (locale.text("profile-show-replies-hint")) }
                }
                @if !codes.is_empty() {
                    div.follow-settings__languages {
                        span.follow-settings__hint {
                            (locale.text("profile-language-filter"))
                        }
                        @for code in &codes {
                            label.follow-settings__toggle {
                                input type="checkbox" name="languages" value=(code)
                                    checked[selected.contains(code)];
                                " "
                                (languages::find(code)
                                    .map_or_else(|| (*code).to_string(), languages::Language::label))
                            }
                        }
                    }
                }
                button type="submit" { (locale.text("common-save")) }
            }
        }
    }
}

/// The viewer's private note about this account, as a disclosure in the
/// profile footer beside the follow settings — open when a note exists, so a
/// saved note is visible at a glance. Whitespace-only input clears the note
/// (the API's semantics); only the viewer ever sees it.
fn account_note_form(
    account: &view::Account,
    csrf: &str,
    relation: &Value,
    locale: Locale,
) -> Markup {
    let note = relation
        .get("note")
        .and_then(Value::as_str)
        .unwrap_or_default();
    html! {
        details.follow-settings.account-note open[!note.is_empty()] {
            summary { (locale.text("profile-private-note")) }
            form.follow-settings__form method="post"
                action=(format!("/web/accounts/{}/note", account.id())) {
                input type="hidden" name="csrf" value=(csrf);
                input type="hidden" name="return_to" value=(account.profile_path());
                textarea.account-note__text name="comment" rows="3"
                    placeholder=(locale.text("profile-private-note-placeholder")) { (note) }
                button type="submit" { (locale.text("common-save")) }
            }
        }
    }
}

/// Paging for the `/{handle}/{segment}` dispatch: `offset` pages the
/// followers/following lists, `max_id` the profile's media section.
#[derive(Deserialize)]
pub struct ListPage {
    offset: Option<i64>,
    max_id: Option<i64>,
    /// `?translate=1` on a thread permalink renders the focus post translated
    /// into the viewer's language.
    translate: Option<String>,
}

/// Which side of a follow relationship a list page shows.
enum FollowList {
    Followers,
    Following,
}

/// `GET /@handle/followers` and `/@handle/following` — the account's follow
/// lists as a paged grid of account cards. Shares the `/{handle}/{segment}`
/// route with the thread view, dispatched on the segment.
async fn account_list(
    state: &AppState,
    session: Option<WebUser>,
    request_locale: Locale,
    handle: &str,
    uri: &Uri,
    kind: FollowList,
    offset: Option<i64>,
) -> Result<Response, ApiError> {
    let account = match resolve_profile_handle(state, handle, uri).await? {
        ProfileHandleResolution::Account(account) => *account,
        ProfileHandleResolution::Redirect(response) => return Ok(response),
        ProfileHandleResolution::Missing => {
            return Ok(not_found(state, session.as_ref(), request_locale).await);
        }
    };
    let offset = offset.unwrap_or(0).max(0);
    // `hide_collections` keeps the lists to the owner, same rule as
    // `GET /api/v1/accounts/{id}/followers|following`.
    let is_owner = session
        .as_ref()
        .is_some_and(|u| u.current.account.id == account.id);
    let viewer_id = session.as_ref().map(|u| u.current.account.id);
    let hidden = account.hide_collections && !is_owner;
    let ids = if hidden {
        Vec::new()
    } else {
        // `viewer_id` lets members who hid their own social graph stay visible
        // to themselves and to this list's owner, while dropping out for anyone
        // else browsing the list.
        match kind {
            FollowList::Followers => {
                follow::followers_page(&state.pool, account.id, offset, LIMIT, viewer_id).await?
            }
            FollowList::Following => {
                follow::following_page(&state.pool, account.id, offset, LIMIT, viewer_id).await?
            }
        }
    };
    let accounts =
        render_accounts_by_ids(&state.pool, &state.config.domain, &ids, viewer_id).await?;

    let account_value =
        account_json(&state.pool, &state.config.domain, &account, viewer_id).await?;
    let profile = view::Account(&account_value);
    let acct = profile.acct().to_owned();
    let locale = session.as_ref().map_or(request_locale, |user| user.locale);
    let mut name_args = FluentArgs::new();
    name_args.set("name", profile.name());
    let (title, active_followers) = match kind {
        FollowList::Followers => (locale.text_with("profile-followers-of", &name_args), true),
        FollowList::Following => (locale.text_with("profile-follows", &name_args), false),
    };
    let full = ids.len() >= usize::try_from(LIMIT).unwrap_or(usize::MAX);
    let base = if active_followers {
        format!("/@{acct}/followers")
    } else {
        format!("/@{acct}/following")
    };
    let body = html! {
        section.column {
            header.profile__back {
                a href=(profile.profile_path()) { "← " (profile.name()) }
            }
            (view::tab_strip(&locale.text("profile-follow-lists"), &[
                view::Tab::new(&format!("/@{acct}/followers"),
                    &locale.text("profile-followers"), active_followers),
                view::Tab::new(&format!("/@{acct}/following"),
                    &locale.text("profile-following"), !active_followers),
            ]))
            @if hidden {
                p.empty { (locale.text("profile-follow-list-hidden")) }
            } @else if accounts.is_empty() {
                p.empty { (locale.text("profile-nobody-yet")) }
            } @else {
                div.account-list data-paged {
                    @for found in &accounts {
                        (view::account_card(&view::Account(found)))
                    }
                }
            }
            @if full {
                nav.pager {
                    a.pager__more href=(format!("{base}?offset={}", offset + LIMIT)) {
                        (locale.text("page-load-more"))
                    }
                }
            }
        }
    };
    Ok(layout::shell_visitor_localized(
        &title,
        session.as_ref(),
        anon_nav(state).await,
        &body,
        locale,
    )
    .into_response())
}

/// `GET /@handle/{segment}` — a status in thread context (ancestors above,
/// replies below) when `segment` is a status id, the account's
/// followers/following list when it is `followers`/`following`, or the
/// profile's Media/Featured section. The handle is canonical for the list
/// views and informational for a thread.
pub async fn thread(
    State(state): State<AppState>,
    MaybeWebUser(session): MaybeWebUser,
    request_locale: Locale,
    uri: Uri,
    Path((handle, status_id)): Path<(String, String)>,
    headers: HeaderMap,
    Query(page): Query<ListPage>,
) -> Result<Response, ApiError> {
    match status_id.as_str() {
        "followers" => {
            return account_list(
                &state,
                session,
                request_locale,
                &handle,
                &uri,
                FollowList::Followers,
                page.offset,
            )
            .await;
        }
        "following" => {
            return account_list(
                &state,
                session,
                request_locale,
                &handle,
                &uri,
                FollowList::Following,
                page.offset,
            )
            .await;
        }
        "media" => {
            return profile_section(
                &state,
                session,
                request_locale,
                &handle,
                &uri,
                ProfileTab::Media,
                page.max_id,
            )
            .await;
        }
        "featured" => {
            return profile_section(
                &state,
                session,
                request_locale,
                &handle,
                &uri,
                ProfileTab::Featured,
                None,
            )
            .await;
        }
        // Mastodon's URL for the replies-included activity view; ours is a
        // query flag on the profile itself.
        "with_replies" => {
            return Ok(Redirect::to(&format!("/{handle}?replies=1")).into_response());
        }
        _ => {}
    }
    let Ok(status_id) = status_id.parse::<i64>() else {
        return Ok(not_found(&state, session.as_ref(), request_locale).await);
    };
    // Content-negotiate like Mastodon: an ActivityPub client dereferencing this
    // shareable permalink (searching by link) gets the Note document off the
    // same URL. Only our own statuses — a remote post is authoritative at its
    // origin. `get_status` is normally behind the `signed_fetch` middleware, so
    // apply the same secure-mode gate here before delegating. Served in place,
    // not redirected, so the caller's signature stays valid for peers that
    // require it (Pleroma/Akkoma/GoToSocial).
    if crate::routes::ap_requested(&headers)
        && let Some(author) = resolve_handle(&state, &handle).await?
        && author.is_local()
    {
        crate::signed_fetch::enforce(&state, &uri, &headers).await?;
        return crate::routes::statuses::get_status(
            State(state),
            MaybeWebUser(session),
            Path((author.username, status_id)),
            uri,
            headers,
        )
        .await;
    }
    let viewer_id = session.as_ref().map(|u| u.current.account.id);
    let Some(focus) = status::find_by_id(&state.pool, status_id).await? else {
        return Ok(not_found(&state, session.as_ref(), request_locale).await);
    };
    if !can_view(&state.pool, &focus, viewer_id).await? {
        return Ok(not_found(&state, session.as_ref(), request_locale).await);
    }
    thread_view(
        &state,
        session,
        request_locale,
        focus,
        page.translate.is_some(),
    )
    .await
}

#[derive(Deserialize)]
pub struct FragmentQuery {
    pub(super) return_to: Option<String>,
}

/// Loads one visible status and the signed-in rendering context shared by its
/// small enhancement fragments. Keeping the admission check and context in
/// one place prevents the poll, RSVP and reaction endpoints drifting apart.
async fn status_region_fragment(
    state: &AppState,
    user: &WebUser,
    id: i64,
    return_to: Option<&str>,
    render: impl for<'a> FnOnce(&view::Status<'a>, &view::Ctx<'a>) -> Option<Markup>,
) -> Result<Response, ApiError> {
    let viewer_id = user.current.account.id;
    let Some(status) = status::find_by_id(&state.pool, id).await? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    if !can_view(&state.pool, &status, Some(viewer_id)).await? {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let entity = render_status(&state.pool, &state.config.domain, &status, Some(viewer_id)).await?;
    let status = view::Status(&entity);
    let viewer = viewer_id.to_string();
    let return_to = actions::safe_return(return_to, "/");
    let ctx = view::Ctx {
        csrf: Some(&user.csrf),
        viewer_id: Some(&viewer),
        return_to: &return_to,
        filter_context: None,
        prefs: view_prefs(state, Some(user)).await?,
        locale: user.locale,
        clock: user.clock.clone(),
        admin: user.admin_capabilities(),
    };
    match render(&status, &ctx) {
        Some(markup) => Ok(markup.into_response()),
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

/// `GET /web/statuses/{id}/reactions` — the reaction-chip row on its own,
/// fetched by the script to swap the row in place after a fetch-based
/// react/unreact. The no-JS path never hits this: its plain form POST
/// re-renders the whole page via PRG instead.
pub async fn reactions_fragment(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Query(query): Query<FragmentQuery>,
) -> Result<Response, ApiError> {
    status_region_fragment(
        &state,
        &user,
        id,
        query.return_to.as_deref(),
        |status, ctx| Some(view::reactions_row(status, ctx)),
    )
    .await
}

/// `GET /web/statuses/{id}/reaction` — the complete categorized reaction
/// catalog. This is the link target when JavaScript is unavailable and is
/// deliberately separate from status pages so the large Unicode inventory and
/// custom-emoji query are paid for only when someone asks to react.
pub async fn reaction_picker_page(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Query(query): Query<FragmentQuery>,
) -> Result<Response, ApiError> {
    let Some(item) = status::find_by_id(&state.pool, id).await? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    if !can_view(&state.pool, &item, Some(user.current.account.id)).await? {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    reactions::picker_page(
        &state,
        &user,
        &format!("/web/statuses/{id}"),
        query.return_to.as_deref(),
        "/",
    )
    .await
}

/// `GET /web/statuses/{id}/poll` — the poll form/results region on its own.
/// The enhancement fetches this after a successful vote so percentages, the
/// total and the viewer's selected option all come from the authoritative
/// server state. Plain form submissions continue to use the PRG response.
pub async fn poll_fragment(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Query(query): Query<FragmentQuery>,
) -> Result<Response, ApiError> {
    status_region_fragment(
        &state,
        &user,
        id,
        query.return_to.as_deref(),
        |status, ctx| status.poll().map(|_| view::poll_view(status, ctx)),
    )
    .await
}

/// `GET /web/statuses/{id}/rsvp` — the viewer's current attendance controls
/// for an event. RSVP POSTs can resolve to accepted, pending or rejected, so
/// the enhancement refreshes this server-rendered state instead of guessing.
pub async fn rsvp_fragment(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Query(query): Query<FragmentQuery>,
) -> Result<Response, ApiError> {
    status_region_fragment(
        &state,
        &user,
        id,
        query.return_to.as_deref(),
        |status, ctx| {
            status
                .event()
                .map(|event| view::rsvp_cluster(status, event, ctx))
        },
    )
    .await
}

/// Which of a status' engagement lists a page shows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EngagementList {
    Boosts,
    Quotes,
    Favourites,
}

/// A page of engagement entries (favers, boosters) as account cards.
async fn engagement_accounts(
    state: &AppState,
    ids: &[i64],
    viewer: Option<i64>,
) -> Result<Markup, ApiError> {
    let accounts = render_accounts_by_ids(&state.pool, &state.config.domain, ids, viewer).await?;
    Ok(html! {
        @if accounts.is_empty() {
            p.empty { "Nobody here yet." }
        } @else {
            div.account-list data-paged {
                @for found in &accounts {
                    (view::account_card(&view::Account(found)))
                }
            }
        }
    })
}

/// `GET /@handle/{id}/reblogs|quotes|favourites` — a status' engagement
/// lists, linked from the thread detail counters and the "…" menu. Boosts and
/// favourites are account-card pages; quotes render the quoting posts
/// themselves, with a per-quote revoke control for the quoted author. Same
/// visibility gate as the thread view, keyset-paginated like the API lists.
#[allow(clippy::too_many_lines)]
pub async fn engagement(
    State(state): State<AppState>,
    MaybeWebUser(session): MaybeWebUser,
    request_locale: Locale,
    uri: Uri,
    Path((handle, status_id, list)): Path<(String, String, String)>,
    Query(page): Query<Page>,
) -> Result<Response, ApiError> {
    // `/@handle/tagged/{tag}` shares this route shape: the profile's
    // activity narrowed to one featured hashtag.
    if status_id == "tagged" {
        return profile_section(
            &state,
            session,
            request_locale,
            &handle,
            &uri,
            ProfileTab::Tagged(list),
            page.max_id,
        )
        .await;
    }
    let kind = match list.as_str() {
        "reblogs" => Some(EngagementList::Boosts),
        "quotes" => Some(EngagementList::Quotes),
        "favourites" => Some(EngagementList::Favourites),
        // The edit-history page shares this route shape and gate.
        "history" => None,
        _ => return Ok(not_found(&state, session.as_ref(), request_locale).await),
    };
    let Ok(status_id) = status_id.parse::<i64>() else {
        return Ok(not_found(&state, session.as_ref(), request_locale).await);
    };
    let viewer_id = session.as_ref().map(|u| u.current.account.id);
    let Some(focus) = status::find_by_id(&state.pool, status_id).await? else {
        return Ok(not_found(&state, session.as_ref(), request_locale).await);
    };
    if !can_view(&state.pool, &focus, viewer_id).await? {
        return Ok(not_found(&state, session.as_ref(), request_locale).await);
    }
    let entity = render_status(&state.pool, &state.config.domain, &focus, viewer_id).await?;
    let permalink = view::Status(&entity).permalink();
    let Some(kind) = kind else {
        return history_page(&state, session.as_ref(), request_locale, &focus, &permalink).await;
    };
    let base = format!("{permalink}/{list}");
    let full = |len: usize| len >= usize::try_from(LIMIT).unwrap_or(usize::MAX);
    let locale = session.as_ref().map_or(request_locale, |user| user.locale);

    let (title, content, next) = match kind {
        EngagementList::Favourites => {
            let entries =
                favourite::favers_of(&state.pool, status_id, viewer_id, page.max_id, None, LIMIT)
                    .await?;
            let ids: Vec<i64> = entries.iter().map(|e| e.account_id).collect();
            let next = full(entries.len())
                .then(|| entries.last().map(|e| e.row_id))
                .flatten();
            (
                locale.text("engagement-favourited-by"),
                engagement_accounts(&state, &ids, viewer_id).await?,
                next,
            )
        }
        EngagementList::Boosts => {
            let entries =
                status::rebloggers_of(&state.pool, status_id, viewer_id, page.max_id, None, LIMIT)
                    .await?;
            let ids: Vec<i64> = entries.iter().map(|e| e.account_id).collect();
            let next = full(entries.len())
                .then(|| entries.last().map(|e| e.row_id))
                .flatten();
            (
                locale.text("engagement-boosted-by"),
                engagement_accounts(&state, &ids, viewer_id).await?,
                next,
            )
        }
        EngagementList::Quotes => {
            let entries =
                quote::accepted_quotes_of(&state.pool, status_id, page.max_id, None, LIMIT).await?;
            let next = full(entries.len())
                .then(|| entries.last().map(|e| e.quote_id))
                .flatten();
            // Quoting posts the viewer can't see (visibility, blocks) are
            // dropped from the page, never an error — same as the API.
            let ids: Vec<i64> = entries.iter().map(|e| e.status_id).collect();
            let fetched = status::find_by_ids(&state.pool, &ids).await?;
            let mut items = filter_viewable(&state.pool, &fetched, viewer_id).await?;
            items.sort_by_key(|s| ids.iter().position(|id| *id == s.id));
            let quoting =
                render_statuses(&state.pool, &state.config.domain, &items, viewer_id).await?;
            let viewer = viewer_id.map(|id| id.to_string());
            let ctx = view::Ctx {
                csrf: session.as_ref().map(|u| u.csrf.as_str()),
                viewer_id: viewer.as_deref(),
                return_to: &base,
                filter_context: Some(view::FilterContext::Public),
                prefs: view_prefs(&state, session.as_ref()).await?,
                locale,
                clock: clock_for(session.as_ref(), locale),
                admin: session
                    .as_ref()
                    .map(WebUser::admin_capabilities)
                    .unwrap_or_default(),
            };
            // Only the quoted post's author may withdraw a quote.
            let owner_csrf = session
                .as_ref()
                .filter(|u| u.current.account.id == focus.account_id)
                .map(|u| u.csrf.as_str());
            let content = html! {
                @if quoting.is_empty() {
                    p.empty { (locale.text("page-nothing-here")) }
                } @else {
                    div.feed data-paged {
                        @for value in &quoting {
                            (view::status_card(&view::Status(value), &ctx))
                            @if let Some(csrf) = owner_csrf {
                                form.quote-revoke method="post"
                                    action=(format!("/web/statuses/{status_id}/quotes/{}/revoke",
                                        view::Status(value).id()))
                                    data-confirm=(locale.plain("engagement-revoke-confirm")) {
                                    input type="hidden" name="csrf" value=(csrf);
                                    input type="hidden" name="return_to" value=(base);
                                    button.quote-revoke__btn type="submit" {
                                        (locale.text("engagement-revoke"))
                                    }
                                }
                            }
                        }
                    }
                }
            };
            (locale.text("status-quotes"), content, next)
        }
    };

    let boosts_label = locale.text("status-boosts");
    let quotes_label = locale.text("status-quotes");
    let favourites_label = locale.text("engagement-favourites");
    let body = html! {
        section.column {
            header.profile__back {
                a href=(permalink) { (locale.text("history-back")) }
            }
            (view::tab_strip(&locale.text("engagement-tabs-aria"), &[
                view::Tab::new(&format!("{permalink}/reblogs"), &boosts_label,
                    kind == EngagementList::Boosts),
                view::Tab::new(&format!("{permalink}/quotes"), &quotes_label,
                    kind == EngagementList::Quotes),
                view::Tab::new(&format!("{permalink}/favourites"), &favourites_label,
                    kind == EngagementList::Favourites),
            ]))
            (content)
            @if let Some(cursor) = next {
                nav.pager {
                    a.pager__more href=(format!("{base}?max_id={cursor}")) {
                        (locale.text("page-load-more"))
                    }
                }
            }
        }
    };
    Ok(layout::shell_visitor_localized(
        &title,
        session.as_ref(),
        anon_nav(&state).await,
        &body,
        locale,
    )
    .into_response())
}

/// `GET /@handle/{id}/history` — the post's edit history, linked from
/// the "edited" markers on cards and the detail view. Same visibility gate as
/// the thread view (applied by the caller); the versions come from the same
/// renderer as `GET /api/v1/statuses/{id}/history`, newest shown first.
async fn history_page(
    state: &AppState,
    session: Option<&WebUser>,
    request_locale: Locale,
    focus: &Status,
    permalink: &str,
) -> Result<Response, ApiError> {
    let locale = session.map_or(request_locale, |user| user.locale);
    let viewer_id = session.map(|s| s.current.account.id);
    let versions =
        render_status_history(&state.pool, &state.config.domain, focus, viewer_id).await?;
    let clock = clock_for(session, locale);
    let title = locale.text("history-title");
    let body = html! {
        section.column {
            header.profile__back {
                a href=(permalink) { (locale.text("history-back")) }
            }
            h1 { (title) }
            (view::edit_history(&versions, &clock, locale))
        }
    };
    Ok(
        layout::shell_visitor_localized(&title, session, anon_nav(state).await, &body, locale)
            .into_response(),
    )
}

/// Submitted edit-composer state echoed across a no-JS preview or validation
/// error. `media: None` uses the status' current attachments (the GET path).
pub(crate) struct EditComposerEcho<'a> {
    pub text: &'a str,
    pub spoiler_text: &'a str,
    pub sensitive: bool,
    pub language: Option<&'a str>,
    pub content_type: &'a str,
    pub quote_policy: Option<&'a str>,
    pub media: Option<&'a [Value]>,
    pub preview: Option<Markup>,
    pub error: Option<&'a str>,
}

/// Renders the edit page from either stored state (GET) or submitted state
/// (the no-JS preview/error path).
pub(crate) async fn render_edit_composer(
    state: &AppState,
    user: &WebUser,
    stored: &Status,
    echo: EditComposerEcho<'_>,
) -> Result<Markup, ApiError> {
    let entity = render_status(
        &state.pool,
        &state.config.domain,
        stored,
        Some(user.current.account.id),
    )
    .await?;
    let compose = ComposeCtx::load(state, user).await?;
    let status = view::Status(&entity);
    let language = echo
        .language
        .or_else(|| status.language())
        .unwrap_or(&compose.language);
    let quote_policy = echo
        .quote_policy
        .or_else(|| status.quote_policy_value())
        .unwrap_or("nobody");
    let media = echo.media.unwrap_or_else(|| status.media());
    let prefill = view::EditComposePrefill {
        text: echo.text,
        spoiler_text: echo.spoiler_text,
        sensitive: echo.sensitive,
        content_type: echo.content_type,
        language,
        quote_policy,
        media,
        preview: echo.preview,
        error: echo.error,
    };
    let body = html! {
        section.column {
            h1 { (user.locale.text("compose-edit")) }
            (view::edit_compose_form(&compose.csrf, &status, &compose.languages,
                compose.limits, &prefill, user.locale))
        }
    };
    Ok(layout::shell(
        &user.locale.text("compose-edit"),
        Some(user),
        &body,
    ))
}

/// `GET /web/statuses/{id}/edit` — the composer in edit mode, owner
/// only: the raw source text prefilled, attachments kept/removed with
/// editable alt text. Others' posts and boost wrappers 404.
pub async fn edit_page(
    State(state): State<AppState>,
    user: WebUser,
    Path(status_id): Path<i64>,
) -> Result<Response, ApiError> {
    let viewer_id = user.current.account.id;
    let stored = status::find_local(&state.pool, viewer_id, status_id)
        .await?
        .filter(|s| s.reblog_of_id.is_none());
    let Some(stored) = stored else {
        return Ok(not_found(&state, Some(&user), user.locale).await);
    };
    let source = status::source_of(&state.pool, stored.id)
        .await?
        .unwrap_or_default();
    Ok(render_edit_composer(
        &state,
        &user,
        &stored,
        EditComposerEcho {
            text: &source.text,
            spoiler_text: &stored.spoiler_text,
            sensitive: stored.sensitive,
            language: stored.language.as_deref(),
            content_type: &source.content_type,
            quote_policy: None,
            media: None,
            preview: None,
            error: None,
        },
    )
    .await?
    .into_response())
}

#[derive(Deserialize)]
pub struct ReportQuery {
    done: Option<String>,
    return_to: Option<String>,
}

/// How many of the target's recent posts the report form offers as
/// supporting evidence, besides the reported one.
const REPORT_STATUS_CHOICES: i64 = 20;

/// A short plain-text excerpt for the report form's post picker: the content
/// warning when there is one (the gated text stays gated), otherwise the
/// stripped content.
fn report_excerpt(status: &Status) -> String {
    let text = if status.spoiler_text.is_empty() {
        crate::filters::plain_text(&status.content)
    } else {
        format!("CW: {}", status.spoiler_text)
    };
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return "(no text)".to_owned();
    }
    if text.chars().count() > 160 {
        let mut cut: String = text.chars().take(160).collect();
        cut.push('…');
        cut
    } else {
        text
    }
}

/// `GET /web/statuses/{id}/report` — Mastodon's report stepper as one plain
/// multi-field form: category, instance-rule picker, additional
/// statuses, comment, and the forward toggle for remote targets, posting to
/// the same path (the web face of `POST /api/v1/reports`). `?done=1` renders
/// the post-submission confirmation with mute/block shortcuts instead. Boost
/// wrappers, own posts and posts the viewer can't see 404.
pub async fn report_page(
    State(state): State<AppState>,
    user: WebUser,
    Path(status_id): Path<i64>,
    Query(query): Query<ReportQuery>,
) -> Result<Response, ApiError> {
    let viewer_id = user.current.account.id;
    let stored = status::find_by_id(&state.pool, status_id)
        .await?
        .filter(|s| s.reblog_of_id.is_none() && s.account_id != viewer_id);
    let Some(stored) = stored else {
        return Ok(not_found(&state, Some(&user), user.locale).await);
    };
    if !can_view(&state.pool, &stored, Some(viewer_id)).await? {
        return Ok(not_found(&state, Some(&user), user.locale).await);
    }
    let Some(target) = account::find_by_id(&state.pool, stored.account_id).await? else {
        return Ok(not_found(&state, Some(&user), user.locale).await);
    };
    let acct = crate::entities::account_acct(&state.config.domain, &target);
    let back = actions::safe_return(query.return_to.as_deref(), "/");

    if query.done.is_some() {
        let body = report_done(&user, &target, &acct, &back);
        let title = format!("Report @{acct}");
        return Ok(layout::shell(&title, Some(&user), &body).into_response());
    }

    // The reported post leads the evidence picker pre-checked; the target's
    // other recent posts (as visible to the viewer) follow.
    let filter = status::AccountStatusesFilter {
        exclude_reblogs: true,
        ..Default::default()
    };
    let mut candidates = vec![stored.clone()];
    for other in status::by_account(
        &state.pool,
        target.id,
        Some(viewer_id),
        &filter,
        TimelineOrder::default(),
        REPORT_STATUS_CHOICES,
    )
    .await?
    {
        if other.id != stored.id {
            candidates.push(other);
        }
    }

    let rules = rule::list_ordered(&state.pool).await?;
    let body = html! {
        section.column {
            h1 { "Report @" (acct) }
            (report_form(&user, &format!("/web/statuses/{}/report", stored.id),
                &target, &back, &rules, &candidates, Some(stored.id), &state.config.domain))
        }
    };
    let title = format!("Report @{acct}");
    Ok(layout::shell(&title, Some(&user), &body).into_response())
}

/// `GET /web/accounts/{id}/report` — the same report form reached from a
/// profile rather than a single post: no post leads the evidence picker, but
/// the account's recent visible posts are still offered as optional backing
/// evidence. `?done=1` renders the same confirmation. Reporting oneself 404s.
pub async fn report_account_page(
    State(state): State<AppState>,
    user: WebUser,
    Path(account_id): Path<i64>,
    Query(query): Query<ReportQuery>,
) -> Result<Response, ApiError> {
    let viewer_id = user.current.account.id;
    let target = account::find_by_id(&state.pool, account_id)
        .await?
        .filter(|a| a.id != viewer_id);
    let Some(target) = target else {
        return Ok(not_found(&state, Some(&user), user.locale).await);
    };
    let acct = crate::entities::account_acct(&state.config.domain, &target);
    let back = actions::safe_return(query.return_to.as_deref(), "/");

    if query.done.is_some() {
        let body = report_done(&user, &target, &acct, &back);
        let title = format!("Report @{acct}");
        return Ok(layout::shell(&title, Some(&user), &body).into_response());
    }

    let filter = status::AccountStatusesFilter {
        exclude_reblogs: true,
        ..Default::default()
    };
    let candidates = status::by_account(
        &state.pool,
        target.id,
        Some(viewer_id),
        &filter,
        TimelineOrder::default(),
        REPORT_STATUS_CHOICES,
    )
    .await?;

    let rules = rule::list_ordered(&state.pool).await?;
    let body = html! {
        section.column {
            h1 { "Report @" (acct) }
            (report_form(&user, &format!("/web/accounts/{}/report", target.id),
                &target, &back, &rules, &candidates, None, &state.config.domain))
        }
    };
    let title = format!("Report @{acct}");
    Ok(layout::shell(&title, Some(&user), &body).into_response())
}

/// Mastodon's category step as `(value, label, hint)` rows; "it violates
/// server rules" only joins the list when the instance has rules to cite.
fn report_categories(has_rules: bool) -> Vec<(&'static str, &'static str, &'static str)> {
    let mut categories = vec![
        (
            "spam",
            "It's spam",
            "Malicious links, fake engagement or repetitive replies",
        ),
        (
            "legal",
            "It's illegal",
            "You believe it violates the law of your or the server's country",
        ),
    ];
    if has_rules {
        categories.push((
            "violation",
            "It violates server rules",
            "You are aware that it breaks specific rules",
        ));
    }
    categories.push((
        "other",
        "It's something else",
        "The issue does not fit into other categories",
    ));
    categories
}

/// The report form's fields, Mastodon's stepper flattened into one page.
#[allow(
    clippy::too_many_arguments,
    reason = "server-rendered report form receives independent view fields"
)]
fn report_form(
    user: &WebUser,
    action: &str,
    target: &Account,
    back: &str,
    rules: &[rule::Rule],
    candidates: &[Status],
    lead_id: Option<i64>,
    local_domain: &str,
) -> Markup {
    let categories = report_categories(!rules.is_empty());
    // From a post the report leads with that post pre-checked; from a profile
    // there is no lead, so the copy addresses the account instead.
    let intro = if lead_id.is_some() {
        "Tell us what's going on with this post"
    } else {
        "Tell us what's going on with this account"
    };
    html! {
        form.settings-form.report-form method="post" action=(action) {
            input type="hidden" name="csrf" value=(user.csrf);
            input type="hidden" name="return_to" value=(back);
            fieldset.settings-form__group {
                legend { (intro) }
                @for (value, label, hint) in &categories {
                    label.settings-toggle {
                        input type="radio" name="category" value=(value) required;
                        span.settings-toggle__text {
                            span.settings-toggle__label { (label) }
                            span.settings-field__hint { (hint) }
                        }
                    }
                }
            }
            @if !rules.is_empty() {
                fieldset.settings-form__group {
                    legend { "Which rules are being violated?" }
                    p.settings-field__hint {
                        "Citing a rule files the report under \"It violates server rules\"."
                    }
                    @for rule in rules {
                        label.settings-toggle {
                            input type="checkbox" name="rule_ids[]" value=(rule.id);
                            span.settings-toggle__text {
                                span.settings-toggle__label { (rule.text) }
                                @if !rule.hint.is_empty() {
                                    span.settings-field__hint { (rule.hint) }
                                }
                            }
                        }
                    }
                }
            }
            @if !candidates.is_empty() {
                fieldset.settings-form__group {
                    legend { "Are there any posts that back up this report?" }
                    @for candidate in candidates {
                        label.settings-toggle {
                            input type="checkbox" name="status_ids[]" value=(candidate.id)
                                checked[Some(candidate.id) == lead_id];
                            span.settings-toggle__text {
                                span.report-form__status-time { (user.clock.element_date(candidate.created_at)) }
                                span.report-form__status-excerpt { (report_excerpt(candidate)) }
                            }
                        }
                    }
                }
            }
            fieldset.settings-form__group {
                legend { "Is there anything else we should know?" }
                label.settings-field {
                    span.settings-field__label { "Additional comments" }
                    textarea name="comment" rows="4" maxlength="1000" {}
                    span.settings-field__hint { "Up to 1000 characters." }
                }
            }
            @if let Some(domain) = target.domain.as_deref()
                .filter(|_| !target.is_portable_on(local_domain)) {
                fieldset.settings-form__group {
                    legend { "This account is from another server" }
                    label.settings-toggle {
                        input type="checkbox" name="forward" value="1";
                        span.settings-toggle__text {
                            span.settings-toggle__label {
                                "Also forward this report to " (domain)
                            }
                            span.settings-field__hint {
                                "An anonymized copy is sent to the account's own server; \
                                 your report stays with our moderators either way."
                            }
                        }
                    }
                }
            }
            div.settings-form__actions.report-form__actions {
                a.settings-button--plain href=(back) { "Cancel" }
                button type="submit" { "Submit report" }
            }
        }
    }
}

/// The post-submission view: Mastodon's "thanks for reporting" step, with its
/// take-action-while-we-review mute/block shortcuts.
fn report_done(user: &WebUser, target: &Account, acct: &str, back: &str) -> Markup {
    html! {
        section.column {
            h1 { "Thanks for reporting" }
            p.muted { "Your report was sent to the moderators for review." }
            h2 { "Don't want to see this?" }
            p.muted {
                "While the report is reviewed, you can take action against @" (acct) "."
            }
            div.report-done__actions {
                form method="post" action=(format!("/web/accounts/{}/mute", target.id)) {
                    input type="hidden" name="csrf" value=(user.csrf);
                    input type="hidden" name="return_to" value=(back);
                    button type="submit" { "Mute @" (acct) }
                }
                form method="post" action=(format!("/web/accounts/{}/block", target.id))
                    data-confirm=(format!("Block @{acct}?")) {
                    input type="hidden" name="csrf" value=(user.csrf);
                    input type="hidden" name="return_to" value=(back);
                    button.settings-button--danger type="submit" { "Block @" (acct) }
                }
            }
            p { a href=(back) { "Back" } }
        }
    }
}

/// `GET /web/statuses/{id}` — resolves a bare status id to its thread
/// permalink (`/@acct/{id}`). Renderers link here when they know a status
/// only by id (a reply parent, a shallow quote); the redirect target applies
/// the usual visibility gate, but the resolver checks it too so an
/// unauthorized viewer learns neither the author nor whether the id exists.
pub async fn status_redirect(
    State(state): State<AppState>,
    MaybeWebUser(session): MaybeWebUser,
    request_locale: Locale,
    Path(status_id): Path<String>,
) -> Result<Response, ApiError> {
    let viewer_id = session.as_ref().map(|u| u.current.account.id);
    let Ok(status_id) = status_id.parse::<i64>() else {
        return Ok(not_found(&state, session.as_ref(), request_locale).await);
    };
    let Some(status) = status::find_by_id(&state.pool, status_id).await? else {
        return Ok(not_found(&state, session.as_ref(), request_locale).await);
    };
    if !can_view(&state.pool, &status, viewer_id).await? {
        return Ok(not_found(&state, session.as_ref(), request_locale).await);
    }
    let Some(author) = account::find_by_id(&state.pool, status.account_id).await? else {
        return Ok(not_found(&state, session.as_ref(), request_locale).await);
    };
    let acct = crate::entities::account_acct(&state.config.domain, &author);
    // Carry a `#post-id` fragment so the resolved post scrolls into view on
    // arrival (e.g. the "Replying to …" link, which only knows the parent id).
    // Put it on the redirect target explicitly rather than relying on the
    // browser to re-apply the request's fragment across the 302.
    Ok(
        axum::response::Redirect::to(&format!("/@{acct}/{status_id}#post-{status_id}"))
            .into_response(),
    )
}

/// Renders a resolved (and already viewer-authorized) status in thread context.
/// Shared by the `/@handle/{id}` web route and the content-negotiated
/// `/users/{username}/statuses/{id}` `ActivityPub` route.
/// The group `viewer` moderates that this status belongs to, if any — gates the
/// thread page's moderation panel.
async fn focus_mod_group(
    state: &AppState,
    viewer: i64,
    focus: &Status,
) -> Result<Option<i64>, ApiError> {
    use plamenu_db::group::{self, Affiliation};
    let group_ids: Vec<i64> = group::groups_of_status(&state.pool, focus.id)
        .await?
        .iter()
        .map(|g| g.account_id)
        .collect();
    if group_ids.is_empty() {
        return Ok(None);
    }
    // One batched affiliation lookup; the first moderated group in
    // attribution order wins, like the per-group short-circuit this replaced.
    let affiliations = group::affiliations_of(&state.pool, &group_ids, viewer).await?;
    Ok(group_ids.into_iter().find(|group_id| {
        matches!(
            affiliations.get(group_id),
            Some(Affiliation::Owner | Affiliation::Moderator)
        )
    }))
}

/// Whether the focus post's thread is locked in any community that still
/// attributes it — the lock lives on the thread root, so this holds for every
/// comment in the thread. Scoped through `communities_of_status` (local mention
/// rows + remote boost rows) rather than the group-agnostic `locked_of`, so
/// a stale lock row left after a post is removed from a group stops reporting
/// the thread locked and the web page stays consistent with the reply gate.
async fn focus_thread_locked(state: &AppState, focus: &Status) -> Result<bool, ApiError> {
    use plamenu_db::group;
    let root = status::thread_root(&state.pool, focus.id).await?;
    let Some(root_status) = status::find_by_id(&state.pool, root).await? else {
        return Ok(false);
    };
    let community_ids: Vec<i64> = crate::groups::communities_of_status(state, &root_status)
        .await?
        .iter()
        .map(|community| community.id)
        .collect();
    Ok(group::thread_locked_in_any(&state.pool, &community_ids, root).await?)
}

/// The visibility a reply composer opens with: the stricter of the viewer's
/// posting default and the parent post's own level (Mastodon's
/// `privacyPreference`), so replying to a followers-only post doesn't default
/// to public. `local` (P2) sits between unlisted and private: it reaches this
/// whole instance but never federates, and it must not widen a followers-only
/// thread. An unknown parent level ranks lowest and never displaces the
/// viewer's default.
fn reply_default_visibility<'a>(default: &'a str, parent: &'a str) -> &'a str {
    fn rank(visibility: &str) -> u8 {
        match visibility {
            "unlisted" => 1,
            "local" => 2,
            "private" => 3,
            "direct" => 4,
            _ => 0, // public, or unknown
        }
    }
    if rank(parent) > rank(default) {
        parent
    } else {
        default
    }
}

/// The `@handles` a reply composer opens with, as literal textarea text —
/// Mastodon's `statusToTextMentions`: the parent's author first, then everyone
/// the parent actively mentions, deduped, minus the viewer. Visible and
/// prunable — deleting a handle really does un-ping that person, which is the
/// only dogpile escape a long thread has. The compose pass parses these back
/// into Mention tags, which is what makes the reply notify its recipients on
/// Mastodon-lineage receivers (they have no reply notification of their own).
fn mention_prefill_text<'a>(
    viewer_id: i64,
    accounts: impl IntoIterator<Item = &'a Account>,
    local_domain: &str,
) -> String {
    let mut seen = Vec::new();
    let mut text = String::new();
    for account in accounts {
        if account.id == viewer_id || seen.contains(&account.id) {
            continue;
        }
        seen.push(account.id);
        text.push('@');
        text.push_str(&account.username);
        if let Some(domain) = account
            .domain
            .as_ref()
            .filter(|_| !account.is_portable_on(local_domain))
        {
            text.push('@');
            text.push_str(domain);
        }
        text.push(' ');
    }
    text
}

/// [`mention_prefill_text`] for a reply to `parent`, from storage. Open
/// parents only: a private/direct reply inherits its audience server-side as
/// silent mentions, where pruning a prefilled handle would *not*
/// un-address anyone — prefilling there would promise control the closed
/// thread deliberately doesn't offer.
async fn reply_mention_prefill(
    state: &AppState,
    parent: &Status,
    viewer_id: i64,
) -> Result<String, ApiError> {
    if !matches!(parent.visibility.as_str(), "public" | "unlisted" | "local") {
        return Ok(String::new());
    }
    let mut accounts = Vec::new();
    if let Some(author) = account::find_by_id(&state.pool, parent.account_id).await? {
        accounts.push(author);
    }
    if let Some(mentioned) = mention::for_statuses(&state.pool, &[parent.id], true)
        .await?
        .remove(&parent.id)
    {
        accounts.extend(mentioned);
    }
    Ok(mention_prefill_text(
        viewer_id,
        &accounts,
        &state.config.domain,
    ))
}

/// The primary language subtag ("pt" of "pt-BR"), for the translate-offer
/// language-mismatch gate — the backend narrows regions the same way.
fn primary_language(tag: &str) -> &str {
    tag.split(['-', '_']).next().unwrap_or(tag)
}

/// The web Translate control on a thread's focus post: offered when a
/// translation backend is configured, the viewer is signed in, and the post
/// (public/unlisted, with text) isn't already in the viewer's language — the
/// API's own gates re-check on request. The target is the viewer's dedicated
/// translate-to preference, falling back to their default posting language.
/// With `translate` set the translation is applied onto the rendered entity.
/// Returns the user-facing error line when a requested translation failed
/// (the page shows the original with that notice).
async fn apply_focus_translation(
    state: &AppState,
    settings: Option<&UserSettings>,
    viewer_account_id: Option<i64>,
    focus: &Status,
    focus_entity: &mut Value,
    translate: bool,
) -> Option<String> {
    let (target, viewer) = match (settings, viewer_account_id) {
        (Some(settings), Some(viewer)) if state.config.translation.is_some() => {
            (settings.translate_language(), viewer)
        }
        _ => return None,
    };
    // A title-only Page has empty `content` but a translatable title.
    let has_text =
        !focus.content.is_empty() || focus.title.as_deref().is_some_and(|t| !t.trim().is_empty());
    let translatable = matches!(focus.visibility.as_str(), "public" | "unlisted")
        && has_text
        && focus
            .language
            .as_deref()
            .is_none_or(|lang| primary_language(lang) != primary_language(target));
    if !(translate && translatable) {
        return None;
    }
    match crate::translation::translate_status(state, focus, target, viewer).await {
        Ok(translation) => {
            crate::translation::apply_to_entity(focus_entity, &translation);
            None
        }
        Err(error) => Some(crate::translation::error_message(&error)),
    }
}

#[allow(clippy::too_many_lines)] // the thread page's one assembly point
pub(crate) async fn thread_view(
    state: &AppState,
    session: Option<WebUser>,
    request_locale: Locale,
    focus: Status,
    translate: bool,
) -> Result<Response, ApiError> {
    let viewer_id = session.as_ref().map(|u| u.current.account.id);
    let status_id = focus.id;

    // A signed-in viewer opening a remote thread fills either missing direction:
    // the orphaned parent and replies we never received over federation. Both
    // run in the background and show up on a later view.
    if viewer_id.is_some() {
        crate::reply_fetch::on_thread_open(state, &focus).await?;
    }

    // Opening a live broadcast's page is someone asking to watch it, so this
    // is where its state is made exact — a stream that started since the post
    // was last seen renders as on air rather than as still announced. No-op
    // for everything that is not a live, and throttled per broadcast. Boxed:
    // a re-ingest is a deep future, and inlining it here would push every
    // thread-view request's future (and so `get_status`'s) far past the size
    // clippy flags, for a branch almost no request takes.
    Box::pin(crate::live_refresh::refresh_statuses(state, &[status_id])).await;

    // Opening a direct thread reads it: every one of the viewer's rows for
    // the conversation clears (participant-set forks share it), the same act
    // as the /notifications marker advance. Idempotent, session-gated, and
    // only participants can reach the page — safe as a GET side effect.
    // Deliberate limit: entering a mixed thread through a non-direct
    // ancestor's permalink clears nothing.
    if focus.visibility == "direct"
        && let Some(viewer) = viewer_id
        && let Some(conversation_id) =
            plamenu_db::conversation::of_status(&state.pool, focus.id).await?
    {
        plamenu_db::conversation::mark_read_conversation(&state.pool, viewer, conversation_id)
            .await?;
    }

    // Load settings once for both the thread-order preference and the render
    // prefs; anonymous visitors get the defaults (tree order).
    let settings = session_settings(state, session.as_ref()).await?;
    let thread_order = settings
        .as_ref()
        .map(|s| s.thread_order)
        .unwrap_or_default();
    let (raw_ancestors, raw_descendants) = match thread_order {
        user::ThreadOrder::Tree => (
            status::ancestors(&state.pool, status_id).await?,
            status::descendants(&state.pool, status_id).await?,
        ),
        user::ThreadOrder::Flat => status::thread_flat(&state.pool, status_id).await?,
    };
    // Same shaping as the API: self-reply promotion is computed on the
    // unfiltered tree, then applied after visibility filtering; flat mode
    // (Pleroma) never reorders.
    let self_replies = match thread_order {
        user::ThreadOrder::Tree => {
            status::self_reply_ids(status_id, focus.account_id, &raw_descendants)
        }
        user::ThreadOrder::Flat => std::collections::HashSet::new(),
    };
    let ancestors = visible(state, raw_ancestors, viewer_id).await?;
    let mut descendants = visible(state, raw_descendants, viewer_id).await?;
    status::promote_self_replies(&mut descendants, &self_replies);

    let mut ancestor_entities =
        render_statuses(&state.pool, &state.config.domain, &ancestors, viewer_id).await?;
    let mut focus_entity =
        render_status(&state.pool, &state.config.domain, &focus, viewer_id).await?;
    let mut descendant_entities =
        render_statuses(&state.pool, &state.config.domain, &descendants, viewer_id).await?;

    let translation_error = apply_focus_translation(
        state,
        settings.as_ref(),
        viewer_id,
        &focus,
        &mut focus_entity,
        translate,
    )
    .await;

    let permalink = view::Status(&focus_entity).permalink();
    let viewer = viewer_id.map(|id| id.to_string());
    let locale = session.as_ref().map_or(request_locale, |user| user.locale);
    let ctx = view::Ctx {
        csrf: session.as_ref().map(|u| u.csrf.as_str()),
        viewer_id: viewer.as_deref(),
        return_to: &permalink,
        filter_context: Some(view::FilterContext::Thread),
        prefs: match settings.as_ref() {
            Some(settings) => prefs_of(settings, crate::translation::web_language_map(state).await),
            None => view::ViewPrefs::default(),
        },
        locale,
        clock: clock_for(session.as_ref(), locale),
        admin: session
            .as_ref()
            .map(WebUser::admin_capabilities)
            .unwrap_or_default(),
    };
    // The reply composer is the same full editor as `/compose`, so a reply has
    // the content warning, media+alt, poll and visibility controls a top-level
    // post does — driven by the viewer's posting defaults.
    let compose = match &session {
        Some(user) => Some(ComposeCtx::load(state, user).await?),
        None => None,
    };
    // A reply opens at the parent's visibility when that's stricter than the
    // viewer's own default (Mastodon's behaviour), so a followers-only thread
    // isn't answered in public by accident. A declared parent language also
    // replaces the viewer's posting default, keeping the reply in the language
    // of the conversation.
    let compose_defaults = compose.as_ref().map(|compose| {
        let mut defaults = compose.defaults();
        defaults.visibility = reply_default_visibility(defaults.visibility, &focus.visibility);
        if let Some(language) = focus.language.as_deref() {
            defaults.language = language;
        }
        defaults
    });
    // Replying inside a direct thread locks the inline composer to a private
    // mention and shows who will receive it (see `DirectCompose`).
    let direct = match viewer_id {
        Some(viewer) if focus.visibility == "direct" => Some(view::DirectCompose {
            participants: direct_participants(state, &focus, viewer).await?,
        }),
        _ => None,
    };
    let focus_id = view::Status(&focus_entity).id().to_owned();
    // The inline reply opens with the thread's handles pasted in (Mastodon's
    // convention) — without them a reply carries no Mention tags and
    // Mastodon-lineage recipients are never notified. The direct-locked
    // composer instead lists its (fixed) participants.
    let reply_prefill = match (&session, &direct) {
        (Some(user), None) => reply_mention_prefill(state, &focus, user.current.account.id).await?,
        _ => String::new(),
    };

    // The group a moderator viewing this thread may moderate, and the
    // thread's lock state. Moderation now rides each post's overflow menu (so
    // it works from the group feed too); inject its context onto the focus,
    // its ancestors and its replies. The lock lives on the thread root, so a
    // locked comment page is locked throughout.
    let mod_group = match session.as_ref() {
        Some(user) => focus_mod_group(state, user.current.account.id, &focus).await?,
        None => None,
    };
    let thread_locked = focus_thread_locked(state, &focus).await?;
    // The organizer's attendee panel, for an event we host (E3).
    let attendees = match session.as_ref() {
        Some(user) => event_attendees_for(state, Some(user), &focus, &user.csrf).await?,
        None => None,
    };
    if let Some(group_id) = mod_group {
        let pinned: std::collections::HashSet<i64> =
            plamenu_db::pin::pinned_statuses(&state.pool, group_id)
                .await?
                .into_iter()
                .map(|status| status.id)
                .collect();
        for entity in ancestor_entities
            .iter_mut()
            .chain(std::iter::once(&mut focus_entity))
            .chain(descendant_entities.iter_mut())
        {
            if let Some(id) = view::displayed_status_id(entity) {
                view::inject_group_mod(entity, group_id, pinned.contains(&id), thread_locked);
            }
        }
    }

    let body = html! {
        section.column {
            @if !ancestor_entities.is_empty() {
                div.thread__ancestors { (view::feed(&ancestor_entities, &ctx)) }
            } @else if let Some(uri) = view::Status(&focus_entity).in_reply_to_uri() {
                // The focus is a reply whose parent was never fetched, so there
                // are no ancestors to walk. Stand a placeholder card in their
                // place so the thread reads as a reply and links to the original.
                div.thread__ancestors {
                    (view::unfetched_parent_card(
                        uri,
                        session.as_ref().map_or_else(Locale::default, |user| user.locale),
                    ))
                }
            }
            div.thread__focus {
                @if let Some(message) = &translation_error {
                    p.thread__translate-error { (message) }
                }
                (view::status_detail(&view::Status(&focus_entity), &ctx))
            }
            @if let Some(panel) = &attendees { (panel) }
            @if thread_locked {
                p.thread__locked-notice { (view::icon("lock")) " " (locale.text("thread-locked-notice")) }
            } @else if let (Some(compose), Some(defaults)) = (&compose, &compose_defaults) {
                (view::full_compose_form(&compose.csrf, Some(&focus_id), None, None,
                    direct.as_ref(), defaults, compose.limits,
                    &view::ComposePrefill {
                        text: &reply_prefill,
                        ..view::ComposePrefill::default()
                    },
                    session.as_ref().map_or_else(Locale::default, |user| user.locale)))
            }
            @if !descendant_entities.is_empty() {
                (view::feed(&descendant_entities, &ctx))
            }
        }
    };
    // Mastodon marks a status page `noindex` when its *author* opted out
    // (`@account.user_prefers_noindex?`); the focus author's Account entity
    // carries the flag for local accounts.
    let noindex = focus_entity
        .get("account")
        .and_then(|a| a.get("noindex"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let focus_view = view::Status(&focus_entity);
    let meta = super::meta::status_page(
        super::meta::site_name(state).await?,
        &state.config.account_domain,
        &focus_view,
        noindex,
        locale,
    );
    Ok(layout::shell_visitor_subject_localized(
        &super::meta::status_page_title(&focus_view, locale),
        session.as_ref(),
        anon_nav(state).await,
        &body,
        &meta,
        locale,
    )
    .into_response())
}

/// `GET /notifications` — the viewer's notifications as a flat list. Opening
/// the page reads it all: the first page advances the shared `notifications`
/// marker (the same one `/api/v1/markers` serves) to the newest notification,
/// clearing the bell's unread dot here and in API clients alike.
pub async fn notifications(
    State(state): State<AppState>,
    mut user: WebUser,
    Query(page): Query<Page>,
) -> Result<Markup, ApiError> {
    let viewer_id = user.current.account.id;
    let items = notification::list(
        &state.pool,
        viewer_id,
        page.max_id,
        None,
        None,
        NotificationFilter {
            kinds: None,
            from_account_id: None,
            ..Default::default()
        },
        LIMIT,
    )
    .await?;
    // Advance-only, so loading older pages — or a marker an API client
    // already pushed further — never rewinds the read position.
    if page.max_id.is_none()
        && let Some(newest) = items.first()
    {
        marker::advance(
            &state.pool,
            user.current.user.id,
            "notifications",
            newest.id,
        )
        .await?;
        // The dot was computed before the marker moved; this render is the
        // read that clears it.
        user.unread_notifications = false;
    }
    let entities =
        render_notifications(&state.pool, &state.config.domain, &items, viewer_id).await?;
    let next = items
        .last()
        .filter(|_| items.len() >= usize::try_from(LIMIT).unwrap_or(usize::MAX))
        .map(|n| format!("/notifications?max_id={}", n.id));
    let viewer = viewer_id.to_string();
    let ctx = view::Ctx {
        csrf: Some(&user.csrf),
        viewer_id: Some(&viewer),
        return_to: "/notifications",
        filter_context: Some(view::FilterContext::Notifications),
        prefs: view_prefs(&state, Some(&user)).await?,
        locale: user.locale,
        clock: user.clock.clone(),
        admin: user.admin_capabilities(),
    };
    let body = html! {
        section.column {
            h1 { (user.locale.text("page-notifications")) }
            @if entities.is_empty() {
                p.empty { (user.locale.text("page-notifications-empty")) }
            } @else {
                div.notifications data-paged {
                    @for note in &entities {
                        (view::notification_item(note, &ctx))
                    }
                }
            }
            @if let Some(href) = next {
                nav.pager { a.pager__more href=(href) { (user.locale.text("pager-older")) } }
            }
        }
    };
    Ok(layout::shell(
        &user.locale.text("page-notifications"),
        Some(&user),
        &body,
    ))
}

/// The handles a direct reply will reach: the parent's author plus everyone
/// it addressed (silent recipients included — the audience the reply inherits
/// server-side), minus the viewer. Feeds the composer's "will be seen by"
/// line.
async fn direct_participants(
    state: &AppState,
    parent: &Status,
    viewer_id: i64,
) -> Result<Vec<String>, ApiError> {
    let mut audience: Vec<Account> = Vec::new();
    if let Some(author) = account::find_by_id(&state.pool, parent.account_id).await? {
        audience.push(author);
    }
    if let Some(mentioned) = mention::for_statuses(&state.pool, &[parent.id], false)
        .await?
        .remove(&parent.id)
    {
        audience.extend(mentioned);
    }
    let mut handles = Vec::new();
    for account in audience {
        if account.id == viewer_id {
            continue;
        }
        let handle = format!(
            "@{}",
            crate::entities::account_acct(&state.config.domain, &account)
        );
        if !handles.contains(&handle) {
            handles.push(handle);
        }
    }
    Ok(handles)
}

/// `GET /conversations` — the Private-mentions inbox: one row per direct
/// thread, newest activity first, keyset-paginated on the last message id
/// exactly like `/api/v1/conversations`. Opening the page deliberately marks
/// nothing read (matching Mastodon) — a thread is read by opening it.
pub async fn conversations(
    State(state): State<AppState>,
    user: WebUser,
    Query(page): Query<Page>,
) -> Result<Markup, ApiError> {
    let viewer_id = user.current.account.id;
    let rows =
        plamenu_db::conversation::list(&state.pool, viewer_id, page.max_id, None, None, LIMIT)
            .await?;
    let entities =
        render_conversations(&state.pool, &state.config.domain, viewer_id, &rows).await?;
    let next = rows
        .last()
        .filter(|_| rows.len() >= usize::try_from(LIMIT).unwrap_or(usize::MAX))
        .and_then(|row| row.last_status_id)
        .map(|id| format!("/conversations?max_id={id}"));
    let viewer = viewer_id.to_string();
    let ctx = view::Ctx {
        csrf: Some(&user.csrf),
        viewer_id: Some(&viewer),
        return_to: "/conversations",
        filter_context: None,
        prefs: view_prefs(&state, Some(&user)).await?,
        locale: user.locale,
        clock: user.clock.clone(),
        admin: user.admin_capabilities(),
    };
    let body = html! {
        section.column {
            h1 { (user.locale.text("page-private-mentions")) }
            p.conversations__hint {
                (user.locale.text("page-private-mentions-help"))
            }
            p.conversations__new {
                a.conversation__reply href="/compose?visibility=direct" {
                    (user.locale.text("page-private-mentions-new"))
                }
            }
            @if entities.is_empty() {
                p.empty { (user.locale.text("page-private-mentions-empty")) }
            } @else {
                div.conversations data-paged {
                    @for row in &entities {
                        (view::conversation_row(row, &ctx))
                    }
                }
            }
            @if let Some(href) = next {
                nav.pager { a.pager__more href=(href) { (user.locale.text("pager-older")) } }
            }
        }
    };
    Ok(layout::shell(
        &user.locale.text("page-private-mentions"),
        Some(&user),
        &body,
    ))
}

#[derive(Deserialize)]
pub struct SearchQuery {
    q: Option<String>,
}

/// The single account or status a URL query dereferences to, rendered for
/// the search page — same `ResolveURLService` path as `/api/v2/search`.
async fn search_by_url(
    state: &AppState,
    q: &str,
    viewer_id: Option<i64>,
) -> Result<(Vec<serde_json::Value>, Vec<serde_json::Value>), ApiError> {
    let (mut accounts, mut statuses) = (Vec::new(), Vec::new());
    match Box::pin(crate::routes::search::resolve_url(state, q, viewer_id)).await? {
        Some(crate::routes::search::UrlResource::Account(found)) => {
            accounts = render_accounts(
                &state.pool,
                &state.config.domain,
                std::slice::from_ref(&*found),
                viewer_id,
            )
            .await?;
        }
        Some(crate::routes::search::UrlResource::Status(found)) => {
            statuses = render_statuses(
                &state.pool,
                &state.config.domain,
                std::slice::from_ref(&*found),
                viewer_id,
            )
            .await?;
        }
        None => {}
    }
    Ok((accounts, statuses))
}

/// `GET /search` — accounts, hashtags, and (for signed-in viewers) statuses.
/// Backed by the same Postgres search the API uses. For signed-in viewers a
/// URL query dereferences that one resource instead of searching, exactly
/// like `/api/v2/search` with `resolve`.
#[allow(clippy::too_many_lines, reason = "one linear page assembly")]
pub async fn search(
    State(state): State<AppState>,
    MaybeWebUser(session): MaybeWebUser,
    request_locale: Locale,
    Query(query): Query<SearchQuery>,
) -> Result<Response, ApiError> {
    let public_search = state.public_search().await;
    if let Some(redirect) = super::session::preview_redirect(&session, public_search) {
        return Ok(redirect);
    }
    let q = query.q.as_deref().unwrap_or_default().trim().to_owned();
    let locale = session.as_ref().map_or(request_locale, |user| user.locale);
    let viewer_id = session.as_ref().map(|u| u.current.account.id);
    let csrf = session.as_ref().map(|u| u.csrf.as_str());

    let mut accounts = Vec::new();
    let mut statuses = Vec::new();
    let mut hashtags = Vec::new();
    let is_url = q.starts_with("https://") || q.starts_with("http://");
    if !q.is_empty() && is_url && viewer_id.is_some() {
        (accounts, statuses) = search_by_url(&state, &q, viewer_id).await?;
    } else if !q.is_empty() {
        // Same account search the API (`/api/v2/search`) uses: an exact
        // `user@domain` match first — webfinger-resolved for signed-in
        // viewers — then ranked full-text. Calling `account::search`
        // directly would miss handles entirely, because Postgres lexes
        // `nova@lunar.place` as a single email token that never matches the
        // separately-indexed username and domain lexemes.
        let viewer_account = session.as_ref().map(|u| &u.current.account);
        let found = crate::routes::search::search_accounts(
            &state,
            viewer_account,
            &q,
            crate::routes::search::AccountSearchOpts {
                limit: LIMIT,
                offset: 0,
                resolve: viewer_account.is_some(),
                following: false,
            },
        )
        .await?;
        let viewer_id = viewer_account.map(|a| a.id);
        accounts
            .extend(render_accounts(&state.pool, &state.config.domain, &found, viewer_id).await?);

        // Status full-text search is viewer-scoped, so it is offered to
        // signed-in viewers only, exactly like the API.
        if let Some(viewer_id) = viewer_id {
            let found = status::search(
                &state.pool,
                &q,
                &StatusSearch {
                    viewer: viewer_id,
                    account_id: None,
                    max_id: None,
                    min_id: None,
                    limit: LIMIT,
                    offset: 0,
                },
            )
            .await?;
            statuses =
                render_statuses(&state.pool, &state.config.domain, &found, Some(viewer_id)).await?;
        }

        let tag_term = q.strip_prefix('#').unwrap_or(&q);
        hashtags = tag::search(&state.pool, tag_term, LIMIT, 0)
            .await?
            .into_iter()
            .map(|t| t.name)
            .collect();
    }

    let has_results = !accounts.is_empty() || !statuses.is_empty() || !hashtags.is_empty();
    let empty_result_message = if q.is_empty() || has_results {
        None
    } else {
        let mut args = FluentArgs::new();
        args.set("query", q.as_str());
        Some(locale.text_with("page-search-empty", &args))
    };
    let viewer = viewer_id.map(|id| id.to_string());
    let ctx = view::Ctx {
        csrf,
        viewer_id: viewer.as_deref(),
        return_to: "/search",
        filter_context: Some(view::FilterContext::Search),
        prefs: view_prefs(&state, session.as_ref()).await?,
        locale,
        clock: clock_for(session.as_ref(), locale),
        admin: session
            .as_ref()
            .map(WebUser::admin_capabilities)
            .unwrap_or_default(),
    };
    let body = html! {
        section.column {
            (view::search_form(&q, locale))
            @if q.is_empty() {
                p.muted { (locale.text("page-search-help")) }
            } @else if !has_results {
                p.empty { (empty_result_message.as_deref().unwrap_or_default()) }
            } @else {
                @if !accounts.is_empty() {
                    section.results {
                        h2 { (locale.text("page-search-people")) }
                        div.account-list {
                            @for account in &accounts {
                                (view::account_card(&view::Account(account)))
                            }
                        }
                    }
                }
                @if !hashtags.is_empty() {
                    section.results {
                        h2 { (locale.text("page-search-hashtags")) }
                        (view::hashtag_list(&hashtags))
                    }
                }
                @if !statuses.is_empty() {
                    section.results {
                        h2 { (locale.text("page-search-posts")) }
                        (view::feed(&statuses, &ctx))
                    }
                }
            }
        }
    };
    Ok(layout::shell_visitor_localized(
        &locale.text("page-search"),
        session.as_ref(),
        anon_nav(&state).await,
        &body,
        locale,
    )
    .into_response())
}

#[derive(Deserialize)]
pub struct GoQuery {
    url: String,
}

/// The in-app path an account lives at — its `/@acct` profile, or the
/// disambiguating `/!acct` for a *remote* group (a same-named person may share
/// the handle). Mirrors `view::Account::profile_path` for a stored account.
pub(super) fn account_local_path(account: &Account, domain: &str) -> String {
    let acct = crate::entities::account_acct(domain, account);
    if account.is_group() && !account.has_local_account_on(domain) {
        format!("/!{acct}")
    } else {
        format!("/@{acct}")
    }
}

/// The in-app thread path for a status: `/@authoracct/{id}`. Falls back to the
/// snowflake-only redirect route if the author has since vanished.
pub(super) async fn status_local_path(
    state: &AppState,
    status: &Status,
) -> Result<String, ApiError> {
    match account::find_by_id(&state.pool, status.account_id).await? {
        Some(author) => {
            let acct = crate::entities::account_acct(&state.config.domain, &author);
            Ok(format!("/@{acct}/{}", status.id))
        }
        None => Ok(format!("/statuses/{}", status.id)),
    }
}

/// `GET /web/go?url=…` — the in-app link resolver. A signed-in viewer's click
/// on an external content link lands here; we dereference the URL (the same
/// `ResolveURLService` path the search box uses) and redirect to our local copy
/// of the actor or post it names, so links inside remote content — a Lemmy
/// group's sibling communities, users and posts — open here instead of on their
/// origin server. An anonymous viewer gets the storage-only lookup: a
/// known object still opens locally, but no fetch is made on their behalf.
/// Anything else falls through to the original page. Only http(s) URLs are
/// followed.
pub async fn go(
    State(state): State<AppState>,
    MaybeWebUser(session): MaybeWebUser,
    request_locale: Locale,
    Query(query): Query<GoQuery>,
) -> Result<Response, ApiError> {
    use crate::routes::search::{KnownUrl, UrlResource, resolve_url, resolve_url_known};
    let url = query.url.trim();
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Ok(not_found(&state, session.as_ref(), request_locale).await);
    }
    let viewer_id = session.as_ref().map(|u| u.current.account.id);
    let found = if viewer_id.is_some() {
        Box::pin(resolve_url(&state, url, viewer_id)).await?
    } else {
        match resolve_url_known(&state, url, None).await? {
            KnownUrl::Found(found) => Some(found),
            KnownUrl::Refused | KnownUrl::Unknown => None,
        }
    };
    if let Some(found) = found {
        let path = match found {
            UrlResource::Account(account) => account_local_path(&account, &state.config.domain),
            UrlResource::Status(status) => status_local_path(&state, &status).await?,
        };
        return Ok(Redirect::to(&path).into_response());
    }
    Ok(Redirect::to(url).into_response())
}

#[derive(Deserialize)]
pub struct SuggestQuery {
    #[serde(rename = "type")]
    kind: Option<String>,
    q: Option<String>,
}

/// `GET /web/compose/suggestions` — the composer's as-you-type autocomplete
/// session-authenticated JSON for `@mention` and `#hashtag` tokens.
/// Four results like Mastodon's composer and, like it, no webfinger
/// resolution (`resolve: false` there): a never-seen remote account resolves
/// when the post is submitted, not on every keystroke. `:emoji:` never hits
/// this — the client filters the `/api/v1/custom_emojis` catalog it already
/// holds for the picker.
pub async fn compose_suggestions(
    State(state): State<AppState>,
    user: WebUser,
    Query(query): Query<SuggestQuery>,
) -> Result<axum::Json<Value>, ApiError> {
    const SUGGESTIONS: i64 = 4;
    let q = query.q.as_deref().unwrap_or_default().trim();
    let mut items = Vec::new();
    if q.is_empty() {
        return Ok(axum::Json(Value::Array(items)));
    }
    match query.kind.as_deref() {
        Some("accounts") => {
            let found = crate::routes::search::search_accounts(
                &state,
                Some(&user.current.account),
                q,
                crate::routes::search::AccountSearchOpts {
                    limit: SUGGESTIONS,
                    offset: 0,
                    resolve: false,
                    following: false,
                },
            )
            .await?;
            for account in &found {
                // The webfinger-style handle the client inserts: bare username
                // for locals, `user@domain` for remotes — same as the API's
                // `acct` field.
                let acct = crate::entities::account_acct(&state.config.domain, account);
                items.push(serde_json::json!({
                    "acct": acct,
                    "display_name": account.display_name,
                    "avatar": crate::entities::avatar_url(&state.config.domain, account, false),
                }));
            }
        }
        Some("hashtags") => {
            let term = q.strip_prefix('#').unwrap_or(q);
            for tag in tag::search(&state.pool, term, SUGGESTIONS, 0).await? {
                items.push(serde_json::json!({ "name": tag.name }));
            }
        }
        _ => {}
    }
    Ok(axum::Json(Value::Array(items)))
}

/// `GET /tags/{tag}` — the timeline of a hashtag. Anonymous visitors are
/// bounced to `/login` unless tag preview is on.
pub async fn tag(
    State(state): State<AppState>,
    MaybeWebUser(session): MaybeWebUser,
    request_locale: Locale,
    Path(name): Path<String>,
    Query(page): Query<Page>,
) -> Result<Response, ApiError> {
    if let Some(redirect) =
        super::session::preview_redirect(&session, state.timeline_preview_tag().await)
    {
        return Ok(redirect);
    }
    let viewer_id = session.as_ref().map(|u| u.current.account.id);
    let settings = session_settings(&state, session.as_ref()).await?;
    let order = settings
        .as_ref()
        .map(|s| s.timeline_order)
        .unwrap_or_default();
    let statuses = tag::timeline(&state.pool, &name, viewer_id, order, page.max_id, LIMIT).await?;
    let entities = render_statuses(&state.pool, &state.config.domain, &statuses, viewer_id).await?;
    let base = format!("/tags/{name}");
    let viewer = viewer_id.map(|id| id.to_string());
    let locale = session.as_ref().map_or(request_locale, |user| user.locale);
    let ctx = view::Ctx {
        csrf: session.as_ref().map(|u| u.csrf.as_str()),
        viewer_id: viewer.as_deref(),
        return_to: &base,
        filter_context: Some(view::FilterContext::Public),
        prefs: match settings.as_ref() {
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
    // The follow-this-hashtag control: following injects the tag's
    // public posts into the home timeline. State keys off the stored tag; a
    // never-used tag simply isn't followed yet.
    let following = match viewer_id {
        Some(viewer) => match tag::find_by_name(&state.pool, &name).await? {
            Some(found) => tag::is_following(&state.pool, viewer, found.id).await?,
            None => false,
        },
        None => false,
    };
    let body = html! {
        section.column {
            div.tag-head {
                h1.tag-title { "#" (name) }
                @if let Some(user) = session.as_ref() {
                    @let (verb, label_id) = if following {
                        ("unfollow", "profile-unfollow")
                    } else {
                        ("follow", "profile-follow")
                    };
                    form.follow method="post" action=(format!("/web/tags/{name}/{verb}")) {
                        input type="hidden" name="csrf" value=(user.csrf);
                        input type="hidden" name="return_to" value=(base);
                        button type="submit" { (locale.text(label_id)) }
                    }
                }
            }
            (view::feed(&entities, &ctx))
            (older_link_localized(&base, &statuses, LIMIT, locale))
        }
    };
    Ok(layout::shell_visitor_localized(
        &format!("#{name}"),
        session.as_ref(),
        anon_nav(&state).await,
        &body,
        locale,
    )
    .into_response())
}

#[derive(Deserialize)]
pub struct ComposeQuery {
    webxdc: Option<i64>,
    reply: Option<i64>,
    quote: Option<i64>,
    /// A local group to post into: the composer grows Title/Link fields
    /// and posts to the group. Reached from the group page's "New post" button.
    group: Option<i64>,
    /// Prefills for the delete-and-redraft flow (`/web/statuses/{id}/redraft`).
    text: Option<String>,
    cw: Option<String>,
    visibility: Option<String>,
    /// The deleted post's text format (P4), so a markdown redraft reopens as
    /// markdown.
    format: Option<String>,
}

/// `GET /compose` — the full composer (content warning, poll, quote/reply).
/// `?reply=` and `?quote=` thread the post and show the target for context;
/// `?text=`/`?cw=`/`?visibility=` prefill it (delete-and-redraft).
#[allow(clippy::too_many_lines)] // the composer page's one assembly point
pub async fn compose_page(
    State(state): State<AppState>,
    user: WebUser,
    Query(query): Query<ComposeQuery>,
) -> Result<Markup, ApiError> {
    let viewer_id = user.current.account.id;
    // Resolve the reply/quote target so the composer can show what it's
    // attached to; an unviewable or missing target just drops the context.
    // A direct reply parent is kept — it locks the composer below.
    let mut direct_parent: Option<Status> = None;
    let mut reply_parent: Option<Status> = None;
    let context = match query.reply.or(query.quote) {
        Some(id) => match status::find_by_id(&state.pool, id).await? {
            Some(target) if can_view(&state.pool, &target, Some(viewer_id)).await? => {
                let rendered =
                    render_status(&state.pool, &state.config.domain, &target, Some(viewer_id))
                        .await?;
                if query.reply.is_some() {
                    if target.visibility == "direct" {
                        direct_parent = Some(target.clone());
                    }
                    reply_parent = Some(target);
                }
                Some(rendered)
            }
            _ => None,
        },
        None => None,
    };
    if query.quote.is_some()
        && context
            .as_ref()
            .is_some_and(|target| !view::Status(target).quotable())
    {
        return Err(ApiError::Unprocessable(
            "Validation failed: Quoting is not allowed for this post".into(),
        ));
    }
    // Resolve an optional group target: a local group the viewer is
    // allowed to post into, or a remote community the viewer follows. A
    // missing or off-limits group falls back to the ordinary composer rather
    // than erroring — the button that links here only shows for members, so
    // this is a stale-link / edge case.
    let group_account = match query.group {
        Some(group_id) => resolve_group_target(&state, viewer_id, group_id).await?,
        None => None,
    };
    let group_handle = group_account.as_ref().map(|a| format!("!{}", a.username));
    let reply = query.reply.map(|id| id.to_string());
    let quote = query.quote.map(|id| id.to_string());
    let viewer = viewer_id.to_string();
    let compose = ComposeCtx::load(&state, &user).await?;
    let mut defaults = compose.defaults();
    // A group post is always public (the group only relays public posts).
    if group_account.is_some() {
        defaults.visibility = "public";
    }
    // A reply opens at the parent's visibility when that's stricter than the
    // viewer's default, and at the parent's declared language — the same rules
    // as the thread page's inline composer. An explicit `?visibility=`
    // (redraft) still wins below.
    if let Some(parent) = &reply_parent {
        defaults.visibility = reply_default_visibility(defaults.visibility, &parent.visibility);
        if let Some(language) = parent.language.as_deref() {
            defaults.language = language;
        }
    }
    // A redraft carries the deleted post's visibility; only known levels are
    // honoured so a crafted URL can't inject an arbitrary form value.
    if let Some(visibility) = query
        .visibility
        .as_deref()
        .filter(|v| matches!(*v, "public" | "unlisted" | "private" | "direct" | "local"))
    {
        defaults.visibility = visibility;
    }
    // Same treatment for the redraft's text format: known values only.
    if let Some(format) = query
        .format
        .as_deref()
        .filter(|f| crate::compose::PostFormat::ADVERTISED.contains(f))
    {
        defaults.content_type = format;
    }
    // A direct parent locks the composer to a private mention (the server
    // clamps the reply regardless; the form just stops pretending otherwise)
    // and surfaces who will receive it.
    let direct = match &direct_parent {
        Some(parent) if group_account.is_none() => {
            defaults.visibility = "direct";
            Some(view::DirectCompose {
                participants: direct_participants(&state, parent, viewer_id).await?,
            })
        }
        _ => None,
    };
    // A reply opens with the thread's handles pasted in (Mastodon's
    // convention), unless a redraft brought its own text or the direct-locked
    // composer took over (it lists its fixed participants instead).
    let reply_prefill = match (&reply_parent, &direct, &query.text) {
        (Some(parent), None, None) => reply_mention_prefill(&state, parent, viewer_id).await?,
        _ => String::new(),
    };
    let invited_session = match query.webxdc {
        Some(id) => Some(
            plamenu_db::webxdc::find(&state.pool, id)
                .await?
                .filter(|s| !s.ended())
                .ok_or(ApiError::NotFound)?,
        ),
        None => None,
    };
    let invitation_text = invited_session
        .as_ref()
        .map(|s| format!("Join me in {}\n\n{}", s.name, s.coordinator_uri));
    let text = query
        .text
        .as_deref()
        .or(invitation_text.as_deref())
        .unwrap_or(&reply_prefill);
    let invitation = crate::webxdc::invitation_in_text(&state.pool, text)
        .await?
        .map(|(id, uri, name)| {
            view::webxdc_invitation_card(Some(&crate::webxdc::invitation_entity(
                Some(id),
                &uri,
                &name,
            )))
        });
    let prefill = view::ComposePrefill {
        invitation,
        text,
        spoiler_text: query.cw.as_deref().unwrap_or_default(),
        ..view::ComposePrefill::default()
    };
    // The context card is fully live — date, favourite/bookmark/react and the
    // "…" menu all work, exactly like the thread page a reply is written on.
    // Actions return to this composer (JS intercepts them in place anyway).
    let return_to = match (query.reply, query.quote) {
        (Some(id), _) => format!("/compose?reply={id}"),
        (_, Some(id)) => format!("/compose?quote={id}"),
        _ => "/compose".to_owned(),
    };
    let ctx = view::Ctx {
        csrf: Some(user.csrf.as_str()),
        viewer_id: Some(&viewer),
        return_to: &return_to,
        // The reply/quote target the viewer deliberately picked: no filtering.
        filter_context: None,
        prefs: view_prefs(&state, Some(&user)).await?,
        locale: user.locale,
        clock: user.clock.clone(),
        admin: user.admin_capabilities(),
    };
    let group_id_str = group_account.as_ref().map(|a| a.id.to_string());
    let group_compose = group_id_str.as_deref().map(|id| view::GroupCompose {
        id,
        title: "",
        external_url: "",
    });
    let group_note = group_handle.as_deref().map(|handle| {
        let mut args = FluentArgs::new();
        args.set("group", handle);
        user.locale.text_with("compose-posting-to", &args)
    });
    let body = html! {
        section.column {
            h1 {
                @if quote.is_some() { (user.locale.text("compose-quote")) }
                @else if reply.is_some() { (user.locale.text("compose-reply")) }
                @else { (user.locale.text("compose-new")) }
            }
            @if let Some(note) = &group_note {
                p.compose__group-note { (note) }
            }
            @if let Some(target) = &context {
                div.compose__context { (view::status_card(&view::Status(target), &ctx)) }
            }
            (view::full_compose_form(&compose.csrf, reply.as_deref(), quote.as_deref(),
                group_compose.as_ref(), direct.as_ref(), &defaults, compose.limits,
                &prefill, user.locale))
        }
    };
    Ok(layout::shell(
        &user.locale.text("compose-page-title"),
        Some(&user),
        &body,
    ))
}

/// Resolves a `?group=` compose target to a group account the viewer may post
/// into — a local group per its posting policy, or a remote community the
/// viewer follows — or `None` (unknown, non-group or off-limits) so the
/// composer degrades to an ordinary post rather than erroring.
async fn resolve_group_target(
    state: &AppState,
    viewer_id: i64,
    group_id: i64,
) -> Result<Option<Account>, ApiError> {
    let Some(account) = account::find_by_id(&state.pool, group_id).await? else {
        return Ok(None);
    };
    if !account.is_group() {
        return Ok(None);
    }
    let allowed = match plamenu_db::group::find(&state.pool, group_id).await? {
        Some(group) => crate::groups::may_submit(state, &group, viewer_id, true).await?,
        // A remote community: open unless suspended locally; no follow
        // required (the remote enforces its own policy on delivery).
        None if !account.is_local() => !account.suspended(),
        None => false,
    };
    if allowed { Ok(Some(account)) } else { Ok(None) }
}

/// The submitted composer state echoed back when `/web/compose` re-renders on a
/// preview or a validation error, so the form comes back exactly as the
/// user left it and (on a preview) shows the rendered post below.
pub(crate) struct ComposerEcho<'a> {
    pub kind: &'a str,
    pub event: view::EventCompose<'a>,
    pub text: &'a str,
    pub spoiler_text: &'a str,
    pub visibility: &'a str,
    pub sensitive: bool,
    pub language: Option<&'a str>,
    pub content_type: &'a str,
    pub quote_policy: &'a str,
    pub poll_options: &'a [String],
    pub poll_expires_in: Option<i64>,
    pub poll_multiple: bool,
    pub reply: Option<&'a str>,
    pub quote: Option<&'a str>,
    /// A group submission: the target group id and the Title/Link the
    /// user entered, echoed back so a failed post keeps its group context.
    pub group_id: Option<&'a str>,
    pub title: &'a str,
    pub external_url: &'a str,
    /// The Schedule field's `datetime-local` value, echoed so a preview
    /// or failed submit keeps the chosen time.
    pub scheduled_at: &'a str,
    /// Already-uploaded attachments as `MediaAttachment` entities.
    pub media: &'a [Value],
    /// The rendered preview card (a `Preview` submit); `None` on an error-only
    /// re-render.
    pub preview: Option<Markup>,
    pub error: Option<&'a str>,
}

/// Renders a draft preview status value as the same card the timeline uses, so
/// the composer's preview matches exactly what the post will look like. Shown
/// read-only (`csrf: None` ⇒ no action buttons), as the author.
pub(crate) async fn render_preview_card(
    state: &AppState,
    user: &WebUser,
    preview: &Value,
) -> Result<Markup, ApiError> {
    let viewer = user.current.account.id.to_string();
    let ctx = view::Ctx {
        csrf: None,
        viewer_id: Some(&viewer),
        return_to: "/compose",
        filter_context: None,
        prefs: view_prefs(state, Some(user)).await?,
        locale: user.locale,
        clock: user.clock.clone(),
        admin: user.admin_capabilities(),
    };
    Ok(view::status_card(&view::Status(preview), &ctx))
}

/// Re-renders the full compose page from an [`ComposerEcho`] — the no-JS
/// preview and validation-error path. Mirrors [`compose_page`]'s assembly but
/// from submitted values rather than the viewer's stored defaults.
#[allow(clippy::too_many_lines)] // one echo re-assembly mirroring compose_page
pub(crate) async fn render_composer(
    state: &AppState,
    user: &WebUser,
    echo: ComposerEcho<'_>,
) -> Result<Markup, ApiError> {
    let enabled = user::posting_languages(&state.pool, user.current.user.id).await?;
    let languages = languages::enabled(enabled.as_deref());
    let limits = compose_limits(state).await?;
    let defaults = view::ComposeDefaults {
        visibility: echo.visibility,
        sensitive: echo.sensitive,
        language: echo.language.unwrap_or_default(),
        languages: &languages,
        quote_policy: echo.quote_policy,
        content_type: echo.content_type,
        time_zone: user.clock.name(),
    };
    // A group re-render keeps its "Posting to …" note and Title/Link fields.
    let group_handle = match echo.group_id.and_then(|id| id.parse::<i64>().ok()) {
        Some(group_id) => account::find_by_id(&state.pool, group_id)
            .await?
            .filter(Account::is_group)
            .map(|a| format!("!{}", a.username)),
        None => None,
    };
    // Only re-render as a group compose when the group actually resolved.
    let group_compose = match (echo.group_id, group_handle.as_deref()) {
        (Some(id), Some(_)) => Some(view::GroupCompose {
            id,
            title: echo.title,
            external_url: echo.external_url,
        }),
        _ => None,
    };
    let heading = if echo.quote.is_some() {
        user.locale.text("compose-quote")
    } else if echo.reply.is_some() {
        user.locale.text("compose-reply")
    } else {
        user.locale.text("compose-new")
    };
    let viewer_id = user.current.account.id;
    // The reply/quote target survives a preview or validation re-render — the
    // same live context card `compose_page` opens with.
    let context = match echo
        .reply
        .or(echo.quote)
        .and_then(|id| id.parse::<i64>().ok())
    {
        Some(id) => match status::find_by_id(&state.pool, id).await? {
            Some(target) if can_view(&state.pool, &target, Some(viewer_id)).await? => Some(
                render_status(&state.pool, &state.config.domain, &target, Some(viewer_id)).await?,
            ),
            _ => None,
        },
        None => None,
    };
    let return_to = match (echo.reply, echo.quote) {
        (Some(id), _) => format!("/compose?reply={id}"),
        (_, Some(id)) => format!("/compose?quote={id}"),
        _ => "/compose".to_owned(),
    };
    let viewer = viewer_id.to_string();
    let ctx = view::Ctx {
        csrf: Some(&user.csrf),
        viewer_id: Some(&viewer),
        return_to: &return_to,
        filter_context: None,
        prefs: view_prefs(state, Some(user)).await?,
        locale: user.locale,
        clock: user.clock.clone(),
        admin: user.admin_capabilities(),
    };
    // A direct-thread reply stays locked across a preview or a validation
    // error — the re-render must not hand the visibility selector back.
    // Gated on can_view: the reply id is client-submitted, and the lock
    // banner lists a DM's participants.
    let direct = match echo.reply.and_then(|id| id.parse::<i64>().ok()) {
        Some(parent_id) => match status::find_by_id(&state.pool, parent_id).await? {
            Some(parent)
                if parent.visibility == "direct"
                    && group_compose.is_none()
                    && can_view(&state.pool, &parent, Some(viewer_id)).await? =>
            {
                Some(view::DirectCompose {
                    participants: direct_participants(state, &parent, viewer_id).await?,
                })
            }
            _ => None,
        },
        None => None,
    };
    let invitation = crate::webxdc::invitation_in_text(&state.pool, echo.text)
        .await?
        .map(|(id, uri, name)| {
            view::webxdc_invitation_card(Some(&crate::webxdc::invitation_entity(
                Some(id),
                &uri,
                &name,
            )))
        });
    let prefill = view::ComposePrefill {
        invitation,
        kind: echo.kind,
        title: echo.title,
        event: echo.event,
        text: echo.text,
        spoiler_text: echo.spoiler_text,
        poll_options: echo.poll_options,
        poll_expires_in: echo.poll_expires_in,
        poll_multiple: echo.poll_multiple,
        scheduled_at: echo.scheduled_at,
        media: echo.media,
        preview: echo.preview,
        error: echo.error,
    };
    let group_note = group_handle.as_deref().map(|handle| {
        let mut args = FluentArgs::new();
        args.set("group", handle);
        user.locale.text_with("compose-posting-to", &args)
    });
    let body = html! {
        section.column {
            h1 { (heading) }
            @if let Some(note) = &group_note {
                p.compose__group-note { (note) }
            }
            @if let Some(target) = &context {
                div.compose__context { (view::status_card(&view::Status(target), &ctx)) }
            }
            (view::full_compose_form(&user.csrf, echo.reply, echo.quote,
                group_compose.as_ref(), direct.as_ref(), &defaults, limits, &prefill,
                user.locale))
        }
    };
    Ok(layout::shell(
        &user.locale.text("compose-page-title"),
        Some(user),
        &body,
    ))
}

/// Keeps only the thread members the viewer is allowed to see.
async fn visible(
    state: &AppState,
    items: Vec<Status>,
    viewer_id: Option<i64>,
) -> Result<Vec<Status>, ApiError> {
    filter_viewable(&state.pool, &items, viewer_id).await
}

/// Resolves a `/@handle` or `/!handle` path segment to an account. `/@name`
/// selects a person-like actor — falling back to a same-named Group so existing
/// links to remote communities keep working — while `/!name` selects the Group
/// (Lemmy-style community syntax). Local handles are unambiguous either way.
/// `None` when unknown or the segment carries no `@`/`!` prefix.
pub(super) async fn resolve_handle(
    state: &AppState,
    handle: &str,
) -> Result<Option<Account>, ApiError> {
    let (group_wanted, acct) = if let Some(rest) = handle.strip_prefix('!') {
        (true, rest)
    } else if let Some(rest) = handle.strip_prefix('@') {
        (false, rest)
    } else {
        return Ok(None);
    };
    let account = match acct.split_once('@') {
        None => account::find_public_local_account_by_username(&state.pool, acct).await?,
        Some((username, domain)) if state.config.is_local_domain(domain) => {
            account::find_public_local_account_by_username(&state.pool, username).await?
        }
        Some((username, domain)) if group_wanted => {
            account::find_remote_group_by_acct(&state.pool, username, domain).await?
        }
        Some((username, domain)) => {
            match account::find_remote_person_by_acct(&state.pool, username, domain).await? {
                Some(person) => Some(person),
                None => account::find_remote_group_by_acct(&state.pool, username, domain).await?,
            }
        }
    };
    let Some(account) = account else {
        return Ok(None);
    };
    // Local-namespace lookups above already folded activation into their one
    // account query. Portable actors continue through suspension/domain policy.
    if account.is_local() {
        return Ok(Some(account));
    }
    if !crate::instance_policy::account_visible(&state.pool, &state.config.domain, &account).await?
    {
        return Ok(None);
    }
    Ok(Some(account))
}

/// `GET /terms-of-service` — the current published Terms of Service
/// editor), publicly visible like Mastodon's terms page.
pub async fn terms_of_service(
    State(state): State<AppState>,
    MaybeWebUser(session): MaybeWebUser,
    request_locale: Locale,
) -> Result<Response, ApiError> {
    let locale = session.as_ref().map_or(request_locale, |user| user.locale);
    let tos = plamenu_db::terms_of_service::current(&state.pool).await?;
    let effective = tos.as_ref().and_then(|tos| {
        tos.effective_date.map(|date| {
            let mut args = FluentArgs::new();
            args.set("date", date.to_string());
            locale.text_with("page-terms-effective", &args)
        })
    });
    let body = html! {
        section.column {
            h1 { (locale.text("page-terms")) }
            @match &tos {
                Some(tos) => {
                    @if let Some(effective) = &effective {
                        p.muted { (effective) }
                    }
                    div.static-page__text {
                        (maud::PreEscaped(crate::routes::api::markdown_html(
                            &tos.text.replace("%{domain}", &state.config.account_domain),
                        )))
                    }
                }
                None => { p.muted { (locale.text("page-terms-empty")) } }
            }
        }
    };
    Ok(layout::shell_visitor_localized(
        &locale.text("page-terms"),
        session.as_ref(),
        anon_nav(&state).await,
        &body,
        locale,
    )
    .into_response())
}

/// A styled 404 page (the web UI never returns the API's JSON error body).
/// `locale` is the request-negotiated fallback for anonymous readers; a
/// signed-in viewer's stored preference wins.
pub(super) async fn not_found(
    state: &AppState,
    user: Option<&WebUser>,
    locale: Locale,
) -> Response {
    let locale = user.map_or(locale, |user| user.locale);
    let body = html! {
        section.column {
            h1 { (locale.text("page-not-found")) }
            p.muted { (locale.text("page-not-found-body")) }
            p { a href="/" { (locale.text("page-not-found-home")) } }
        }
    };
    (
        StatusCode::NOT_FOUND,
        layout::shell_visitor_localized(
            &locale.text("page-not-found"),
            user,
            anon_nav(state).await,
            &body,
            locale,
        ),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn russian_profile_header_localizes_public_identity_and_stats() {
        let value = serde_json::json!({
            "id": "42",
            "username": "robot",
            "acct": "robot",
            "display_name": "Робот",
            "url": "https://example.test/@robot",
            "uri": "",
            "avatar": "",
            "note": "",
            "fields": [{
                "name": "Сайт",
                "value": "<a href=\"https://example.test\">example.test</a>",
                "verified_at": "2026-01-01T00:00:00Z"
            }],
            "emojis": [],
            "roles": [],
            "bot": true,
            "locked": true,
            "group": false,
            "created_at": "2024-01-15T12:00:00Z",
            "statuses_count": 12,
            "following_count": 3,
            "followers_count": 7
        });
        let locale = Locale::negotiate(Some("ru"), None);
        let rendered = profile_header(
            &view::Account(&value),
            None,
            None,
            &Sanctions::default(),
            &[],
            None,
            &crate::web::clock::ViewerClock::utc(Locale::default()),
            locale,
        )
        .into_string()
        .replace(['\u{2068}', '\u{2069}'], "");

        for expected in [
            "Автоматизированный аккаунт",
            "Подписка по запросу",
            "Создан",
            "подтверждено",
            "Публикации",
            "Подписки",
            "Подписчики",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?} in {rendered}"
            );
        }
        assert!(!rendered.contains(">Posts<"), "{rendered}");
        assert!(!rendered.contains(">Following<"), "{rendered}");
        assert!(!rendered.contains(">Followers<"), "{rendered}");
    }
}
