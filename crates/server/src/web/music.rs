//! HTML-first music discovery and publication reuse ordinary post controls.
use super::{
    i18n::Locale,
    layout, pages,
    session::{MaybeWebUser, preview_redirect},
    view,
};
use crate::{AppState, error::ApiError};
use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use maud::html;
use serde::Deserialize;

pub(super) fn total_duration(l: Locale, seconds: f64) -> maud::Markup {
    let mut args = fluent_bundle::FluentArgs::new();
    args.set("seconds", seconds);
    html! {p.music-total-duration data-seconds=(seconds){(l.text_with("music-total-duration", &args))}}
}

#[derive(Default, Deserialize)]
pub struct CatalogueQuery {
    pub q: Option<String>,
    pub mine: Option<bool>,
    pub max_id: Option<i64>,
    pub tracks_before: Option<i64>,
    pub remote_before: Option<i64>,
}

pub(super) fn term_links(
    l: Locale,
    kind: crate::music_feeds::Term,
    values: &[String],
) -> maud::Markup {
    html! {span.music-term-links{@for (index,value) in values.iter().enumerate(){@if index>0{" · "}a href=(format!("/music/{}/{}",kind.path(),crate::music_feeds::term_segment(value))){(value)}}span.visually-hidden{(l.text(if matches!(kind,crate::music_feeds::Term::Tag){"music-entity-tags"}else{"music-entity-genres"}))}}}
}
pub(super) fn workspace_navigation(l: Locale, signed_in: bool, selected: &str) -> maud::Markup {
    let library_selected = matches!(
        selected,
        "mine"
            | "favourites"
            | "history"
            | "stats"
            | "shares"
            | "folders"
            | "downloads"
            | "collections"
    );
    let radio_selected = matches!(selected, "radios" | "streams");
    let browse_selected = !library_selected
        && !radio_selected
        && !matches!(selected, "upload" | "player" | "podcasts");
    html! {
        div.music-workspace-navigation {
            nav.music-primary aria-label=(l.text("music-title")) {
                @for (href, label, active) in [
                    ("/music", "music-discovery-browse", browse_selected),
                    ("/music/mine", "music-mine-title", library_selected),
                    ("/music/streams", "music-live-radio", radio_selected),
                    ("/music/podcasts", "music-podcasts-title", selected=="podcasts"),
                ] {
                    @if signed_in || !matches!(href,"/music/mine"|"/music/podcasts") {
                        a href=(href) aria-current=[active.then_some("page")] {(l.text(label))}
                    }
                }
            }
            @if signed_in {
                details.music-tools {
                    summary {(l.text("music-more"))}
                    nav.music-tools__links aria-label=(l.text("music-tools")) {
                        a href="/music/player" {(l.text("music-now-playing"))}
                        a href="/music/assets?mine=true&upload=true" {(l.text("music-upload-audio"))}
                        a href="/music/collections?mine=true" {(l.text("music-collections-title"))}
                        a href="/music/favourites" {(l.text("music-favourites-title"))}
                        a href="/music/history" {(l.text("music-history-title"))}
                        a href="/music/shares" {(l.text("music-shares-title"))}
                        a href="/music/downloads" {(l.text("music-downloads-title"))}
                        a href="/music/radios" {(l.text("music-radios-title"))}
                        a href="/music/clients" {(l.text("music-clients-title"))}
                        a href="/music/folders" {(l.text("music-folders-title"))}
                        a href="/music/stats" {(l.text("music-stats-title"))}
                    }
                }
            }
        }
    }
}

/// Native resource pages expose their existing music subscription directly.
pub(super) async fn subscription(
    state: &AppState,
    user: Option<&super::session::WebUser>,
    target: &str,
    back: &str,
) -> Result<maud::Markup, ApiError> {
    let Some(user) = user else {
        return Ok(html! {});
    };
    if !user.current.has_scope("write:statuses") {
        return Ok(html! {});
    }
    let Some(owner) = crate::music_follows::public_resource_owner(state, target).await? else {
        return Ok(html! {});
    };
    if owner == user.current.account.id {
        return Ok(html! {});
    }
    let stored = crate::music_follows::find_target(state, user.current.account.id, target).await?;
    let l = user.locale;
    Ok(
        html! {form.music-subscription method="post" action="/web/music/follows" {
            input type="hidden" name="csrf" value=(user.csrf);input type="hidden" name="url" value=(target);input type="hidden" name="return_to" value=(back);
            input type="hidden" name="operation" value=(if stored.is_some(){"unfollow"}else{"follow"});
            @if stored.as_ref().is_some_and(|s|s.pending){p{(l.text("music-follow-pending"))}}
            button type="submit"{(l.text(if stored.is_some(){"music-unfollow"}else{"music-follow"}))}
        }},
    )
}

pub async fn index(
    State(state): State<AppState>,
    MaybeWebUser(user): MaybeWebUser,
    Query(q): Query<CatalogueQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let l = user
        .as_ref()
        .map_or_else(|| Locale::from_headers(&headers), |u| u.locale);
    if let Some(response) = preview_redirect(
        &user,
        state
            .settings_cache
            .get(&state.pool)
            .await?
            .timeline_preview_federated,
    ) {
        return Ok(response);
    }
    let viewer = user.as_ref().map(|u| u.current.account.id);
    let mine = q.mine.unwrap_or(false);
    let query = q.q.as_deref().unwrap_or_default();
    if (query.trim().starts_with("https://") || query.trim().starts_with("http://"))
        && let Some(user) = user
    {
        return super::music_discovery::open(
            State(state),
            user,
            Query(super::music_discovery::Request {
                url: Some(query.trim().into()),
                ..Default::default()
            }),
        )
        .await;
    }
    let subscriptions = if let Some(viewer) = viewer {
        crate::music_follows::list(&state, viewer).await?
    } else {
        Vec::new()
    };
    let page = crate::audio::catalogue(&state, viewer, mine, query, q.max_id).await?;
    let entities =
        crate::entities::render_statuses(&state.pool, &state.config.domain, &page.items, viewer)
            .await?;
    let viewer_id = viewer.map(|id| id.to_string());
    let ctx = view::Ctx {
        csrf: user.as_ref().map(|u| u.csrf.as_str()),
        viewer_id: viewer_id.as_deref(),
        return_to: "/music",
        filter_context: Some(view::FilterContext::Public),
        prefs: pages::view_prefs(&state, user.as_ref()).await?,
        locale: l,
        clock: pages::clock_for(user.as_ref(), l),
        admin: user
            .as_ref()
            .map_or_default(super::session::WebUser::admin_capabilities),
    };
    let available = super::music_browse::available(
        &state,
        user.as_ref(),
        l,
        query,
        q.tracks_before,
        q.remote_before,
    )
    .await?;
    let body = html! {section.column.music-hub {
        h1 {(l.text("music-title"))}
        (workspace_navigation(l,user.is_some(),"public"))

        form.settings-form.music-search method="get" action="/music" {
            @if mine {input type="hidden" name="mine" value="true";}
            label.visually-hidden for="music-search" {(l.text("music-hub-search"))}
            input id="music-search" type="search" name="q" value=(query) maxlength="2048" placeholder=(l.text("music-hub-placeholder"));
            button type="submit" {(l.text("music-search-submit"))}
        }
        @if query.is_empty() {p.music-search-hint{(l.text("music-hub-search-hint"))}}
        (available)
        @if !entities.is_empty(){h2{(l.text("music-audio-posts"))}(view::feed(&entities,&ctx))}
        (subscriptions_view(l,user.as_ref(),&subscriptions))

        @if let Some(before)=page.before {a href=(format!("/music?mine={mine}&max_id={before}&{}",serde_urlencoded::to_string([("q",query)]).unwrap_or_default())) {(l.text("music-older"))}}
    }};
    let mut response = layout::shell(&l.text("music-title"), user.as_ref(), &body).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "private, no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::VARY, "Cookie".parse().unwrap());
    Ok(response)
}

fn subscriptions_view(
    l: Locale,
    user: Option<&super::session::WebUser>,
    subscriptions: &[crate::music_follows::Subscription],
) -> maud::Markup {
    html! {
        @if let Some(user)=&user {
            details {
                summary {(l.text("music-subscriptions"))}
                p {(l.text("music-subscription-hint"))}
                form.settings-form method="post" action="/web/music/follows" {
                    input type="hidden" name="csrf" value=(user.csrf);
                    input type="hidden" name="operation" value="follow";
                    label for="music-follow-url" {(l.text("music-follow-url"))}
                    input id="music-follow-url" type="url" name="url" required maxlength="2048";
                    button type="submit" {(l.text("music-follow"))}
                }
                ul {@for subscription in subscriptions {li {
                    a href=(format!("/music/open?{}",serde_urlencoded::to_string([("url",subscription.target_uri.as_str())]).unwrap_or_default())) {(subscription.target_name)}
                    @if subscription.pending {" — " (l.text("music-follow-pending"))}
                    form method="post" action="/web/music/follows" {
                        input type="hidden" name="csrf" value=(user.csrf);
                        input type="hidden" name="operation" value="unfollow";
                        input type="hidden" name="url" value=(subscription.target_uri);
                        button type="submit" {(l.text("music-unfollow"))}
                    }
                }}}
            }
        }

    }
}

pub async fn follow_action(
    State(state): State<AppState>,
    user: super::session::WebUser,
    body: Bytes,
) -> Result<Response, ApiError> {
    user.current.require_scope("write:statuses")?;
    let pairs = super::settings::form_pairs(&body).map_err(ApiError::Unprocessable)?;
    if super::settings::field(&pairs, "csrf") != Some(user.csrf.as_str()) {
        return Ok(super::session::csrf_rejection());
    }
    let target = super::settings::field(&pairs, "url").unwrap_or_default();
    let result = match super::settings::field(&pairs, "operation") {
        Some("follow") => crate::music_follows::follow(&state, &user.current.account, target).await,
        Some("unfollow") => {
            crate::music_follows::unfollow(&state, &user.current.account, target).await
        }
        _ => return Err(ApiError::Unprocessable("Choose follow or unfollow".into())),
    };
    if let Err(error) = result {
        let message = match error {
            ApiError::Unprocessable(message) | ApiError::BadRequest(message) => message,
            ApiError::NotFound => user.locale.text("music-follow-unavailable"),
            other => return Err(other),
        };
        let body = html! {section.column {
            h1 {(user.locale.text("music-subscriptions"))}
            p.settings__error role="alert" {(message)}
            form.settings-form method="post" action="/web/music/follows" {
                input type="hidden" name="csrf" value=(user.csrf);
                input type="hidden" name="operation" value="follow";
                label for="music-follow-url" {(user.locale.text("music-follow-url"))}
                input id="music-follow-url" type="url" name="url" required maxlength="2048" value=(target);
                button type="submit" {(user.locale.text("music-follow"))}
            }
            a href="/music" {(user.locale.text("music-title"))}
        }};
        return Ok((
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            layout::shell(&user.locale.text("music-subscriptions"), Some(&user), &body),
        )
            .into_response());
    }
    let back = super::actions::safe_return(super::settings::field(&pairs, "return_to"), "/music");
    Ok(super::settings::redirect_to(&back))
}
