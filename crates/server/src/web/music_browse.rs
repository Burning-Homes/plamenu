//! The listening hub brings readable native and remote music together.
use super::{i18n::Locale, session::WebUser};
use crate::{AppState, error::ApiError};
use maud::{Markup, html};

pub(super) async fn available(
    state: &AppState,
    user: Option<&WebUser>,
    l: Locale,
    query: &str,
    tracks_before: Option<i64>,
    remote_before: Option<i64>,
) -> Result<Markup, ApiError> {
    // Keep the landing page scannable; explicit searches retain full pages.
    let limit = if query.trim().is_empty() { 6 } else { 60 };
    let mut tracks = crate::music_assets::catalogue(
        state,
        user.map(|u| u.current.account.id),
        false,
        query,
        tracks_before,
    )
    .await?;
    let native_more = tracks.len() > limit;
    tracks.truncate(limit);
    let remote = if let Some(user) = user {
        crate::music_remote::catalogue(state, user.current.account.id, query, remote_before).await?
    } else {
        Vec::new()
    };
    let remote_more = remote.len() > limit;
    let native_next = tracks.last().map(|item| item.id);
    let remote_next = remote.iter().take(limit).next_back().map(|item| item.id);
    let mut servers = std::collections::BTreeSet::new();
    for resource in &remote {
        if resource.catalogue_kind.is_some() {
            servers.insert(crate::music_remote::catalogue::origin(&resource.uri)?);
        }
    }
    Ok(html! {
        nav.music-browse-types aria-label=(l.text("music-browse-types")) {
            a href="/music" aria-current="page" {(l.text("music-mine-all"))}
            a href="/music/albums" {(l.text("music-albums-title"))}
            a href="/music/artists" {(l.text("music-artists-title"))}
        }
        @if tracks.is_empty() && remote.is_empty() {p.music-empty-hint{(l.text("music-browse-empty"))}}
        @if !tracks.is_empty() || remote.iter().take(limit).any(|item| matches!(item.kind.as_str(),"track"|"audio") && !matches!(item.catalogue_kind.as_deref(),Some("live"|"stream"))) {
            section.music-shelf {
                h2{(l.text("music-browse-available"))}
                ul.music-listening-results {
                    @for track in &tracks {li {
                        @if let Some(art)=track.artwork_id {img.music-artwork src=(format!("/music/artwork/{art}")) alt="" width="64" height="64" loading="lazy";}
                        @else {span.music-artwork.music-artwork--placeholder aria-hidden="true" {(super::view::icon("music"))}}
                        div.music-track-info {h3{a href=(format!("/music/assets/{}",track.id)){(track.title)}} p{(track.artist)}}
                        @if let Some(user)=user {(super::music_player::play_form(user,"asset",track.id))}
                        @else {audio controls preload="none" aria-label=(track.title) src=(format!("/music/assets/{}/stream",track.id)) {}}
                    }}
                    @for item in remote.iter().take(limit).filter(|item|matches!(item.kind.as_str(),"track"|"audio") && !matches!(item.catalogue_kind.as_deref(),Some("live"|"stream"))) {li {
                        @if let Some(image)=item.artwork_url() {img.music-artwork src=(image) alt="" width="64" height="64" loading="lazy";}
                        @else {span.music-artwork.music-artwork--placeholder aria-hidden="true" {(super::view::icon("music"))}}
                        div.music-track-info {h3{a href=(format!("/music/remote/{}",item.id)){(item.name)}}p{(item.details.artist)}}
                        @if let Some(user)=user {(super::music_player::play_form(user,"remote",item.id))}
                    }}
                }
            }
        }
        @for (kind,label) in [("album","music-albums-title"),("artist","music-artists-title"),("stream","music-live-radio"),("library","music-kind-library"),("playlist","music-kind-playlist")] {
            @let items: Vec<_>=remote.iter().take(limit).filter(|item| if kind=="stream" {matches!(item.catalogue_kind.as_deref(),Some("live"|"stream")) || item.kind=="stream"} else {item.kind==kind}).collect();
            @if !items.is_empty() {
                section.music-shelf {
                    h2{(l.text(label))}
                    ul.music-cover-grid {
                        @for item in items {li {
                            a href=(format!("/music/remote/{}",item.id)) {
                                @if let Some(image)=item.artwork_url() {img src=(image) alt="" width="160" height="160" loading="lazy";}
                                @else {span.music-cover-placeholder aria-hidden="true" {(super::view::icon("music"))}}
                                span.music-cover-title {(item.name)}
                                @if !item.details.artist.is_empty(){span.music-cover-artist{(item.details.artist)}}
                            }
                            @if kind=="stream" && let Some(user)=user {(super::music_player::play_form(user,"remote",item.id))}
                        }}
                    }
                }
            }
        }
        @if !servers.is_empty(){details.music-source-sites {
            summary{(l.text("music-browse-servers"))}
            div.music-sources {@for server in servers {a href=(format!("/music/discover?{}",serde_urlencoded::to_string([("server",server.as_str())]).unwrap_or_default())){(server.trim_start_matches("https://").trim_start_matches("http://"))}}}
        }}
        @if native_more && let Some(next)=native_next {a rel="next" href=(format!("/music?tracks_before={next}&remote_before={}&{}",remote_before.unwrap_or(i64::MAX),serde_urlencoded::to_string([("q",query)]).unwrap_or_default())){(l.text("music-more-local"))}}
        @if remote_more && let Some(next)=remote_next {a rel="next" href=(format!("/music?remote_before={next}&tracks_before={}&{}",tracks_before.unwrap_or(i64::MAX),serde_urlencoded::to_string([("q",query)]).unwrap_or_default())){(l.text("music-more-remote"))}}
    })
}

pub(super) async fn remote_section(
    state: &AppState,
    user: Option<&WebUser>,
    l: Locale,
    query: &str,
    before: Option<i64>,
    kind: &str,
) -> Result<Markup, ApiError> {
    let path = match kind {
        "album" => "/music/albums",
        "artist" => "/music/artists",
        _ => "/music/streams",
    };
    let Some(user) = user else {
        return Ok(html! {});
    };
    let mut items =
        crate::music_remote::catalogue_section(state, user.current.account.id, query, before, kind)
            .await?;
    let more = items.len() > 60;
    items.truncate(60);
    let next = items.last().map(|item| item.id);
    Ok(html! {
        @if !items.is_empty(){h2{(l.text("music-from-other-sites"))}ul.music-listening-results{@for item in &items{li{
            div{h3{a href=(format!("/music/remote/{}",item.id)){(item.name)}}p{(kind_label(l,item.catalogue_kind.as_deref().unwrap_or(&item.kind))) @if !item.details.artist.is_empty(){" · "(item.details.artist)}}
                @if matches!(item.kind.as_str(),"track"|"audio"){(super::music_player::play_form(user,"remote",item.id))}
            }
        }}}}
        @if more && let Some(next)=next{a rel="next" href=(format!("{path}?remote_before={next}&{}",serde_urlencoded::to_string([("q",query)]).unwrap_or_default())){(l.text("music-more-remote"))}}
    })
}

pub(super) async fn source_sites(
    state: &AppState,
    user: Option<&WebUser>,
    l: Locale,
    kind: &str,
) -> Result<Markup, ApiError> {
    let Some(user) = user else {
        return Ok(html! {});
    };
    let rows = crate::music_remote::catalogue(state, user.current.account.id, "", None).await?;
    let mut sites = std::collections::BTreeSet::new();
    for row in rows {
        if row.catalogue_kind.is_some() {
            sites.insert(crate::music_remote::catalogue::origin(&row.uri)?);
        }
    }
    Ok(
        html! {@if !sites.is_empty(){h2{(l.text("music-browse-servers"))}div.music-browse-types{@for site in sites{a href=(format!("/music/discover?{}",serde_urlencoded::to_string([("server",site.as_str()),("kind",kind)]).unwrap_or_default())){(site.trim_start_matches("https://").trim_start_matches("http://"))}}}}},
    )
}

pub(super) fn kind_label(l: Locale, kind: &str) -> String {
    l.text(match kind {
        "album" => "music-album",
        "artist" => "music-artist",
        "library" => "music-kind-library",
        "playlist" => "music-kind-playlist",
        "radio" => "music-kind-radio",
        "live" | "stream" => "music-live-radio",
        _ => "music-kind-track",
    })
}
