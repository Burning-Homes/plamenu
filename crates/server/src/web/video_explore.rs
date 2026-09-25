//! A lightweight, HTML-first public Video catalogue. The gallery renders
//! posters and links, never twenty eager media players; playback stays on the
//! ordinary status page with its captions, descriptions and P2P choice.

use std::collections::HashMap;

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use fluent_bundle::FluentArgs;
use maud::{Markup, html};
use plamenu_db::{account, follow, status, video};
use serde::Deserialize;
use serde_json::Value;

use super::i18n::Locale;
use super::pages::{account_local_path, anon_nav, clock_for};
use super::session::{MaybeWebUser, WebUser, preview_redirect};
use super::{layout, view};
use crate::entities::render_statuses;
use crate::error::ApiError;
use crate::state::AppState;

const LIMIT: i64 = 20;

#[derive(Deserialize)]
pub struct ExploreQuery {
    q: Option<String>,
    channel: Option<i64>,
    following: Option<String>,
    max_id: Option<i64>,
}

fn href(q: &str, channel: Option<i64>, followed: bool, max_id: Option<i64>) -> String {
    let mut params = Vec::new();
    if !q.is_empty() {
        params.push(("q", q.to_owned()));
    }
    if let Some(id) = channel {
        params.push(("channel", id.to_string()));
    }
    if followed {
        params.push(("following", "1".to_owned()));
    }
    if let Some(id) = max_id {
        params.push(("max_id", id.to_string()));
    }
    if params.is_empty() {
        "/videos/explore".to_owned()
    } else {
        format!(
            "/videos/explore?{}",
            serde_urlencoded::to_string(params).unwrap_or_default()
        )
    }
}

fn follow_form(
    channel_id: i64,
    edge: Option<&follow::FollowEdge>,
    user: &WebUser,
    back: &str,
    locale: Locale,
) -> Markup {
    if channel_id == user.current.account.id {
        return Markup::default();
    }
    let (verb, label) = match edge {
        Some(edge) if edge.pending => ("unfollow", locale.text("profile-cancel-request")),
        Some(_) => ("unfollow", locale.text("profile-unfollow")),
        None => ("follow", locale.text("videos-explore-follow-channel")),
    };
    html! {
        form.video-explore__follow method="post"
            action=(format!("/web/accounts/{channel_id}/{verb}")) {
            input type="hidden" name="csrf" value=(user.csrf);
            input type="hidden" name="return_to" value=(back);
            button type="submit" { (label) }
        }
    }
}

fn video_media(entity: &Value) -> Option<&Value> {
    entity
        .get("media_attachments")
        .and_then(Value::as_array)
        .and_then(|items| {
            items.iter().find(|item| {
                matches!(
                    item.get("type").and_then(Value::as_str),
                    Some("video" | "gifv")
                )
            })
        })
}

fn poster(media: Option<&Value>, sensitive: bool, locale: Locale) -> Markup {
    if sensitive {
        return html! { span.video-explore__placeholder { (locale.text("videos-explore-sensitive")) } };
    }
    let preview = media
        .and_then(|item| item.get("preview_url"))
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty());
    match preview {
        Some(url) => html! { img src=(url) alt="" loading="lazy" decoding="async"; },
        None => {
            html! { span.video-explore__placeholder { (locale.text("videos-explore-no-poster")) } }
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one bounded, HTML-first catalogue page"
)]
pub async fn page(
    State(state): State<AppState>,
    MaybeWebUser(session): MaybeWebUser,
    request_locale: Locale,
    Query(query): Query<ExploreQuery>,
) -> Result<Response, ApiError> {
    let q = query
        .q
        .as_deref()
        .unwrap_or_default()
        .trim()
        .chars()
        .take(160)
        .collect::<String>();
    let channel_id = query.channel.filter(|id| *id > 0);
    if query.channel.is_some() && channel_id.is_none() {
        return Err(ApiError::BadRequest("Choose a valid channel".into()));
    }
    let followed = channel_id.is_none() && query.following.as_deref() == Some("1");
    let anon_allowed = if q.is_empty() {
        state.timeline_preview_federated().await
    } else {
        state.public_search().await
    };
    if let Some(redirect) = preview_redirect(&session, anon_allowed && !followed) {
        return Ok(redirect);
    }
    let viewer_id = session.as_ref().map(|user| user.current.account.id);
    let locale = session.as_ref().map_or(request_locale, |user| user.locale);
    let channel = if let Some(id) = channel_id {
        if !video::library_visible(&state.pool, id, viewer_id).await? {
            return Err(ApiError::NotFound);
        }
        Some(
            account::find_by_id(&state.pool, id)
                .await?
                .ok_or(ApiError::NotFound)?,
        )
    } else {
        None
    };
    let before = query.max_id.filter(|id| *id > 0);
    let mut hits = if let Some(id) = channel_id {
        video::discover_channel(
            &state.pool,
            id,
            (!q.is_empty()).then_some(q.as_str()),
            viewer_id,
            before,
            LIMIT + 1,
        )
        .await?
    } else {
        video::discover(
            &state.pool,
            (!q.is_empty()).then_some(q.as_str()),
            viewer_id,
            followed,
            before,
            LIMIT + 1,
        )
        .await?
    };
    let page_size = usize::try_from(LIMIT).unwrap_or(20);
    let has_more = hits.len() > page_size;
    hits.truncate(page_size);
    let status_ids = hits.iter().map(|hit| hit.status_id).collect::<Vec<_>>();
    let mut channel_ids = hits.iter().map(|hit| hit.channel_id).collect::<Vec<_>>();
    if let Some(id) = channel_id {
        channel_ids.push(id);
    }
    let statuses = status::find_by_ids(&state.pool, &status_ids).await?;
    let status_by_id = statuses
        .iter()
        .map(|item| (item.id, item))
        .collect::<HashMap<_, _>>();
    let entities = render_statuses(&state.pool, &state.config.domain, &statuses, viewer_id).await?;
    let entity_by_id = statuses
        .iter()
        .zip(entities.iter())
        .map(|(item, entity)| (item.id, entity))
        .collect::<HashMap<_, _>>();
    let channels = account::find_by_ids(&state.pool, &channel_ids).await?;
    let channel_by_id = channels
        .iter()
        .map(|item| (item.id, item))
        .collect::<HashMap<_, _>>();
    let follows = if let Some(viewer) = viewer_id {
        follow::find_out_batch(&state.pool, viewer, &channel_ids).await?
    } else {
        HashMap::new()
    };
    let back = href(&q, channel_id, followed, before);
    let clock = clock_for(session.as_ref(), locale);
    let page_title = if let Some(channel) = channel.as_ref() {
        let name = if channel.display_name.is_empty() {
            &channel.username
        } else {
            &channel.display_name
        };
        let mut args = FluentArgs::new();
        args.set("name", name.as_str());
        locale.text_with("videos-explore-channel-title", &args)
    } else {
        locale.text("videos-explore-title")
    };
    let body = html! {
        section.column.video-explore {
            h1 { (&page_title) }
            @if let Some(channel) = channel.as_ref() {
                nav.video-explore__links aria-label=(locale.text("videos-explore-navigation")) {
                    a href="/videos/explore" { (locale.text("videos-explore-all")) }
                    a href=(account_local_path(channel, &state.config.domain)) {
                        (locale.text("videos-explore-profile"))
                    }
                    @if session.is_some() {
                        a href="/videos" { (locale.text("videos-explore-publish")) }
                    }
                }
                @if let Some(user) = session.as_ref() {
                    (follow_form(channel.id, follows.get(&channel.id), user, &back, locale))
                }
            } @else {
                p { (locale.text("videos-explore-intro")) }
                @if session.is_some() {
                    p { a href="/videos" { (locale.text("videos-explore-publish")) } }
                }
            }
            @if session.is_some() && channel_id.is_none() {
                (view::tab_strip(&locale.text("videos-explore-tabs"), &[
                    view::Tab::new(&href(&q, None, false, None), &locale.text("videos-explore-latest"), !followed),
                    view::Tab::new(&href(&q, None, true, None), &locale.text("videos-explore-following"), followed),
                ]))
            }
            form.video-explore__search method="get" action="/videos/explore" role="search" {
                @if let Some(id) = channel_id { input type="hidden" name="channel" value=(id); }
                @if followed { input type="hidden" name="following" value="1"; }
                label for="video-explore-query" { (locale.text("videos-explore-search")) }
                input id="video-explore-query" type="search" name="q" value=(q) maxlength="160";
                button type="submit" { (locale.text("videos-explore-search-button")) }
            }
            @if hits.is_empty() {
                p.empty { (locale.text("videos-explore-empty")) }
            } @else {
                ul.video-explore__grid data-paged {
                    @for hit in &hits {
                        @if let (Some(item), Some(entity), Some(channel)) = (
                            status_by_id.get(&hit.status_id),
                            entity_by_id.get(&hit.status_id),
                            channel_by_id.get(&hit.channel_id),
                        ) {
                            @let sensitive = item.sensitive || !item.spoiler_text.is_empty();
                            @let title = if sensitive {
                                locale.text("videos-explore-sensitive")
                            } else {
                                item.title.clone().unwrap_or_else(|| locale.text("videos-explore-untitled"))
                            };
                            @let channel_name = if channel.display_name.is_empty() {
                                channel.username.as_str()
                            } else {
                                channel.display_name.as_str()
                            };
                            @let media = video_media(entity);
                            @let poster_description = media
                                .and_then(|item| item.get("poster_description"))
                                .and_then(Value::as_str)
                                .filter(|text| !text.is_empty());
                            li {
                                article.video-explore__card {
                                    a.video-explore__watch href=(view::Status(entity).permalink()) {
                                        span.video-explore__poster { (poster(media, sensitive, locale)) }
                                        h2 { (title) }
                                    }
                                    @if !sensitive {
                                        @if let Some(description) = poster_description {
                                            details.video-explore__description {
                                                summary { (locale.text("status-poster-description")) }
                                                p lang=[item.language.as_deref()] { (description) }
                                            }
                                        }
                                    }
                                    p.video-explore__meta {
                                        a href=(href("", Some(channel.id), false, None)) { (channel_name) }
                                        " · " (clock.element_absolute(item.created_at))
                                    }
                                    @if channel_id.is_none() {
                                        @if let Some(user) = session.as_ref() {
                                            (follow_form(channel.id, follows.get(&channel.id), user, &back, locale))
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                @if has_more {
                    @if let Some(last) = hits.last() {
                        nav.pager aria-label=(locale.text("videos-explore-pages")) {
                            a.pager__more href=(href(&q, channel_id, followed, Some(last.status_id))) {
                                (locale.text("pager-show-more"))
                            }
                        }
                    }
                }
            }
        }
    };
    Ok(layout::shell_visitor_localized(
        &page_title,
        session.as_ref(),
        anon_nav(&state).await,
        &body,
        locale,
    )
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::{href, poster, video_media};
    use crate::web::i18n::Locale;
    use serde_json::json;

    #[test]
    fn poster_uses_video_attachment_not_an_earlier_image() {
        let entity = json!({"media_attachments": [
            {"type": "image", "preview_url": "/wrong.jpg"},
            {"type": "video", "preview_url": "/film.jpg", "poster_description": "Blue poster"}
        ]});
        let media = video_media(&entity);
        assert_eq!(media.unwrap()["poster_description"], "Blue poster");
        let html = poster(media, false, Locale::negotiate(Some("en"), None)).into_string();
        assert!(html.contains("/film.jpg"));
        assert!(!html.contains("/wrong.jpg"));
    }

    #[test]
    fn sensitive_poster_never_exposes_its_preview_url() {
        let entity = json!({"media_attachments": [
            {"type": "video", "preview_url": "/sensitive.jpg"}
        ]});
        let html = poster(
            video_media(&entity),
            true,
            Locale::negotiate(Some("en"), None),
        )
        .into_string();
        assert!(!html.contains("/sensitive.jpg"));
    }

    #[test]
    fn library_links_keep_channel_search_and_cursor() {
        assert_eq!(
            href("Blue film", Some(42), false, Some(17)),
            "/videos/explore?q=Blue+film&channel=42&max_id=17"
        );
        assert_eq!(
            href("", Some(42), false, None),
            "/videos/explore?channel=42"
        );
    }
}
