//! A native player window keeps its media element alive while browsing elsewhere.
use super::{layout, session::WebUser, settings};
use crate::{
    AppState,
    error::ApiError,
    music_queues::{self, Queue, Source},
};
use axum::{
    Json,
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
use serde::Deserialize;
use serde_json::json;

#[derive(Default, Deserialize)]
pub struct Page {
    device: Option<String>,
    #[serde(default)]
    autoplay: bool,
    asset_id: Option<String>,
    status_id: Option<String>,
    episode_id: Option<String>,
    remote_id: Option<String>,
    collection_id: Option<String>,
    podcast_id: Option<i64>,
    offset: Option<i64>,
    #[serde(default)]
    oldest: bool,
    #[serde(skip)]
    podcast_preview: Option<Markup>,
    #[serde(skip)]
    output: Option<crate::music_outputs::Output>,
    #[serde(skip)]
    device_choices: Vec<(String, String)>,
}
fn hidden(user: &WebUser, queue: &Queue, action: &str) -> Markup {
    html! {input type="hidden" name="csrf" value=(user.csrf);input type="hidden" name="device" value=(queue.device);input type="hidden" name="revision" value=(queue.revision);input type="hidden" name="action" value=(action);}
}
pub async fn index(
    State(state): State<AppState>,
    user: WebUser,
    Query(mut page): Query<Page>,
) -> Result<Response, ApiError> {
    let device = page.device.as_deref().unwrap_or("web");
    let queue = music_queues::get(&state, user.current.account.id, device).await?;
    let addition = if page.asset_id.is_some()
        || page.status_id.is_some()
        || page.episode_id.is_some()
        || page.remote_id.is_some()
    {
        let source = Source {
            remote_id: page.remote_id.clone(),
            episode_id: page.episode_id.clone(),
            asset_id: page.asset_id.clone(),
            status_id: page.status_id.clone(),
        };
        let entry = source.entry(1, 0, 1)?;
        Some(
            crate::music_collections::sources(&state, &[entry], Some(user.current.account.id))
                .await?
                .into_iter()
                .next()
                .ok_or(ApiError::NotFound)?,
        )
    } else {
        None
    };
    let collection = if let Some(id) = &page.collection_id {
        Some(
            crate::music_collections::get(
                &state,
                id.parse().map_err(|_| ApiError::NotFound)?,
                Some(user.current.account.id),
            )
            .await?,
        )
    } else {
        None
    };
    let devices = music_queues::devices(&state, user.current.account.id).await?;
    let outputs =
        crate::music_outputs::list(&state, Some(user.current.account.id), true, None, "").await?;
    page.output = outputs
        .iter()
        .find(|output| output.device == device)
        .cloned();
    page.device_choices = vec![("web".into(), user.locale.text("music-player-title"))];
    for output in &outputs {
        page.device_choices
            .push((output.device.clone(), output.name.clone()));
    }
    for saved in &devices {
        if !page
            .device_choices
            .iter()
            .any(|(key, _)| key == &saved.device_key)
        {
            page.device_choices
                .push((saved.device_key.clone(), saved.device_key.clone()));
        }
    }
    let l = user.locale;
    let podcast = if let Some(show) = page.podcast_id {
        let show = crate::music_podcasts::get(&state, show, user.current.account.id).await?;
        let sources = crate::music_podcasts::page_sources(
            &state,
            show.id,
            user.current.account.id,
            page.oldest,
            page.offset,
        )
        .await?;
        html! {div.music-player-podcast {h2{(show.title)} p{(l.text("music-podcasts-page-queue")) " (" (sources.len()) ")"} form.music-player-command method="post" action="/web/music/player" {(hidden(&user,&queue,"podcast"))input type="hidden" name="podcast_id" value=(show.id);input type="hidden" name="offset" value=(page.offset.unwrap_or(0));input type="hidden" name="oldest" value=(page.oldest);label{input type="checkbox" name="replace" value="true";(l.text("music-podcasts-replace-queue"))}button type="submit" disabled[sources.is_empty()]{(l.text("music-player-add-collection"))}}}}
    } else {
        html! {}
    };
    page.podcast_preview = Some(podcast);
    let body = render(
        &user,
        device,
        &queue,
        addition.as_ref(),
        collection.as_ref(),
        &page,
        &devices,
    );
    Ok((
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
        ],
        layout::shell(&l.text("music-player-title"), Some(&user), &body),
    )
        .into_response())
}
fn render(
    user: &WebUser,
    device: &str,
    queue: &Queue,
    addition: Option<&crate::music_collections::Playable>,
    collection: Option<&crate::music_collections::Collection>,
    page: &Page,
    devices: &[music_queues::Device],
) -> Markup {
    let selected = queue
        .current_entry_id
        .as_ref()
        .and_then(|id| queue.entries.iter().find(|p| &p.entry_id == id));
    let l = user.locale;
    html! {div.music-player-page data-queue-label=(l.text("music-player-queue")) data-close-label=(l.text("music-player-close")) {
        // This element survives enhanced form submissions. No script is needed for playback.
        div.column.music-player-media {
            h1{(l.text("music-player-title"))}
        (super::music::workspace_navigation(l,true,"player"))
            img.music-artwork.music-player-artwork src=[selected.and_then(|p|p.image_url.as_deref())] alt=(selected.map_or("",|p|p.title.as_str())) hidden[selected.and_then(|p|p.image_url.as_ref()).is_none()] width="256" height="256";
            a.music-player-now href=[selected.map(|p|p.url.as_str())] aria-live="polite"{(selected.map_or_else(||l.text(if queue.entries.is_empty(){"music-player-empty"}else{"music-player-choose-track"}),|p|format!("{} — {}",p.title,p.artist)))}
            @if let Some(output)=&page.output {
                audio.music-output-audio controls preload="none" aria-label=(output.name) src=[(output.enabled && output.transport=="playing").then(||format!("/music/live/{}",output.mount))] {}
                p{(l.text("music-stream-window-hint"))}
                (super::music_outputs::owner_controls(user,output))
            } @else {
            audio.music-player-audio controls autoplay[page.autoplay] preload="none" aria-label=(l.text("music-player-title")) data-position=(queue.position_seconds) {
                @if let Some(item)=selected {source src=(format!("{}#t={}",item.stream_url,queue.position_seconds));}
            }
            p { a.music-player-detail href=[selected.map(|p|p.url.as_str())]  hidden[selected.is_none()] {(l.text("music-player-details"))} }
            p{(l.text("music-player-window-hint"))}p{a href="/music" {(l.text("music-player-browse"))}}
            }
            p{a href="/music/history" {(l.text("music-history-title"))}}
            p.music-player-error role="status" data-error=(l.text("music-player-error")){}
            p.music-history-feedback role="status" data-recorded=(l.text("music-history-recorded")){}
        }
        section.column.music-player data-device=(device) data-revision=(queue.revision) data-current=(queue.current_entry_id.as_deref().unwrap_or_default()) data-position=(queue.position_seconds) data-stream=(selected.map_or("",|p|p.stream_url.as_str())) data-url=(selected.map_or("",|p|p.url.as_str())) data-image=(selected.and_then(|p|p.image_url.as_deref()).unwrap_or_default()) data-title=(selected.map_or_else(||l.text(if queue.entries.is_empty(){"music-player-empty"}else{"music-player-choose-track"}),|p|format!("{} — {}",p.title,p.artist))) {
            @if addition.is_some() || collection.is_some(){(device_picker(page,device,l))}
            @if let Some(preview)=&page.podcast_preview{(preview)}
            @if let Some(item)=&addition {
                form.music-player-command method="post" action="/web/music/player" {
                    (hidden(user,queue,"add"))
                    @if let Some(id)=&page.asset_id{input type="hidden" name="asset_id" value=(id);}
                    @if let Some(id)=&page.status_id{input type="hidden" name="status_id" value=(id);}
                    @if let Some(id)=&page.remote_id{input type="hidden" name="remote_id" value=(id);}
                    @if let Some(id)=&page.episode_id{input type="hidden" name="episode_id" value=(id);}
                    p{(item.title) " — " (item.artist)}
                    button type="submit"{(l.text("music-player-add"))}
                }
            }
            @if let Some(item)=&collection {
                form.music-player-command method="post" action="/web/music/player" {
                    (hidden(user,queue,"collection")) input type="hidden" name="collection_id" value=(item.id);
                    p{(item.name)} button type="submit"{(l.text("music-player-add-collection"))}
                }
            }
            div.video-explore__links {
                @for (action,label) in [("previous","music-player-previous"),("next","music-player-next")] {
                    form.music-player-command method="post" action="/web/music/player" {(hidden(user,queue,action))button type="submit"{(l.text(label))}}
                }
            }
            details.music-advanced-playback id="music-player-advanced"{summary{(l.text("music-player-advanced"))}
            form.music-player-command.settings-form method="post" action="/web/music/player" {
                (hidden(user,queue,"settings"))
                label for="music-player-repeat"{(l.text("music-player-repeat"))}
                select id="music-player-repeat" name="repeat_mode"{
                    @for (value,label) in [("off","music-player-repeat-off"),("one","music-player-repeat-one"),("all","music-player-repeat-all")] {option value=(value) selected[queue.repeat_mode==value]{(l.text(label))}}
                }
                label {input type="checkbox" name="shuffle" value="true" checked[queue.shuffle];(l.text("music-player-shuffle"))}
                button type="submit"{(l.text("music-player-save-settings"))}
            }
                @if addition.is_none() && collection.is_none(){(device_picker(page,device,l))}
            @if let Some(item)=selected {
                form.music-player-command.settings-form method="post" action="/web/music/player" {
                    (hidden(user,queue,"seek")) input type="hidden" name="entry_id" value=(item.entry_id);
                    label for="music-player-seek"{(l.text("music-player-position"))}
                    input id="music-player-seek" name="seconds" type="number" min="0" max=(if item.duration>0.0{item.duration}else{604_800.0}) step="any" value=(queue.position_seconds);
                    button type="submit"{(l.text("music-player-go-position"))}
                }
            }
            details{summary{(l.text("music-player-devices"))}
                ul{@for saved in devices{li{a href=(format!("/music/player?device={}",saved.device_key)){(saved.device_key)}}}}
                @if queue.revision!="0"{form.music-player-command method="post" action="/web/music/player"{(hidden(user,queue,"forget"))button type="submit"{(l.text("music-player-forget"))}}}
            }
            }
            (queue_contents(user,queue))
        }
    }}
}

fn queue_contents(user: &WebUser, queue: &Queue) -> Markup {
    let l = user.locale;
    html! {
            details.music-player-queue id="music-player-queue" {summary{(l.text("music-player-queue"))}
            @if queue.entries.is_empty(){p{(l.text("music-player-empty"))}}
            ol.music-player-entries {@for item in &queue.entries {li data-entry-id=(item.entry_id) {
                div.music-queue-title {a href=(item.url) {(item.title)} span { (item.artist) }
                    @if queue.current_entry_id.as_ref()==Some(&item.entry_id){span aria-current="true"{" ✓"}}
                }
                form.music-player-command method="post" action="/web/music/player" {
                    (hidden(user,queue,"select")) input type="hidden" name="entry_id" value=(item.entry_id);
                    button type="submit" aria-label=(l.text("music-player-select")){(l.text("music-play-now"))}
                }
                details.music-queue-options {summary aria-label=(l.text("inbox-options")){(super::view::icon("more"))}
                    div {
                        form.music-player-command method="post" action="/web/music/player" {
                            (hidden(user,queue,"remove")) input type="hidden" name="entry_id" value=(item.entry_id);
                            button.button--quiet type="submit"{(l.text("music-player-remove"))}
                        }
                        @for (direction,label) in [("up","music-player-up"),("down","music-player-down")] {
                            form.music-player-command method="post" action="/web/music/player" {
                                (hidden(user,queue,"move")) input type="hidden" name="entry_id" value=(item.entry_id);input type="hidden" name="direction" value=(direction);
                                button.button--quiet type="submit" disabled[queue.shuffle]{(l.text(label))}
                            }
                        }
                    }
                }
            }}}
            @if !queue.entries.is_empty(){form.music-player-command method="post" action="/web/music/player"{(hidden(user,queue,"clear"))button type="submit"{(l.text("music-player-clear"))}}}
            }
    }
}

fn device_picker(page: &Page, device: &str, l: super::i18n::Locale) -> Markup {
    html! {form.settings-form method="get" action="/music/player" {
        label for="music-player-destination"{(l.text("music-player-destination"))}
        select id="music-player-destination" name="device"{@for (key,name) in &page.device_choices {option value=(key) selected[key==device]{(name)}}}
        @for (name,value) in [("asset_id",&page.asset_id),("status_id",&page.status_id),("episode_id",&page.episode_id),("remote_id",&page.remote_id),("collection_id",&page.collection_id)] {
            @if let Some(value)=value {input type="hidden" name=(name) value=(value);}
        }
        @if let Some(show)=page.podcast_id {input type="hidden" name="podcast_id" value=(show);input type="hidden" name="offset" value=(page.offset.unwrap_or(0));input type="hidden" name="oldest" value=(page.oldest);}
        button type="submit"{(l.text("music-player-choose-destination"))}
    }}
}

pub async fn command(
    State(state): State<AppState>,
    user: WebUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let pairs = settings::form_pairs(&body).map_err(ApiError::Unprocessable)?;
    if settings::field(&pairs, "csrf") != Some(user.csrf.as_str()) {
        return Ok(super::session::csrf_rejection());
    }
    let field = |key| settings::field(&pairs, key).unwrap_or_default();
    let mut input = json!({"revision":field("revision"),"action":field("action"),"entry_id":field("entry_id"),"repeat_mode":field("repeat_mode"),"shuffle":field("shuffle")=="true","ended":field("ended")=="true","collection_id":field("collection_id"),"replace":field("replace")=="true","direction":field("direction")});
    if field("action") == "podcast" {
        input["podcast_id"] = json!(field("podcast_id"));
        input["oldest"] = json!(field("oldest") == "true");
        input["offset"] = json!(
            field("offset")
                .parse::<i64>()
                .map_err(|_| ApiError::Unprocessable("Invalid podcast page".into()))?
        );
    }
    if field("action") == "seek" {
        input["seconds"] = json!(
            field("seconds")
                .parse::<f64>()
                .map_err(|_| ApiError::Unprocessable("Invalid playback position".into()))?
        );
    }
    if field("action") == "add" {
        input["sources"] = json!([{"asset_id":settings::field(&pairs,"asset_id"),"status_id":settings::field(&pairs,"status_id"),"episode_id":settings::field(&pairs,"episode_id"),"remote_id":settings::field(&pairs,"remote_id")}]);
    }
    let input = serde_json::from_value(input)
        .map_err(|_| ApiError::Unprocessable("Invalid playback command".into()))?;
    let queue =
        music_queues::change(&state, user.current.account.id, field("device"), &input).await?;
    if headers
        .get(header::ACCEPT)
        .is_some_and(|v| v == "application/json")
    {
        return Ok(([(header::CACHE_CONTROL, "private, no-store")], Json(queue)).into_response());
    }
    Ok(
        axum::response::Redirect::to(&format!("/music/player?device={}", queue.device))
            .into_response(),
    )
}
pub fn link(user: &WebUser, kind: &str, id: i64) -> Markup {
    html! {(play_form(user,kind,id)) (queue_link(user,kind,id))}
}

pub(super) fn queue_link(user: &WebUser, kind: &str, id: i64) -> Markup {
    html! {details {summary{(user.locale.text("music-player-queue-tools"))}p{a.music-player-open href=(format!("/music/player?{kind}_id={id}")) target="plamenu-music-player"{(user.locale.text("music-player-open"))}}}}
}

/// Listening is an explicit mutation; viewing a song never replaces a queue.
pub(super) fn play_form(user: &WebUser, kind: &str, id: i64) -> Markup {
    source_play_form(user.locale, &user.csrf, kind, &id.to_string())
}

pub(super) fn source_play_form(l: super::i18n::Locale, csrf: &str, kind: &str, id: &str) -> Markup {
    html! {form.music-play-now data-error=(l.text("music-player-error")) method="post" action="/web/music/play" {
        input type="hidden" name="csrf" value=(csrf);
        input type="hidden" name=(format!("{kind}_id")) value=(id);
        button type="submit"{(l.text("music-play-now"))}
    }}
}

pub async fn play(
    State(state): State<AppState>,
    user: WebUser,
    body: Bytes,
) -> Result<Response, ApiError> {
    user.current.require_scope("write:statuses")?;
    let pairs = settings::form_pairs(&body).map_err(ApiError::Unprocessable)?;
    if settings::field(&pairs, "csrf") != Some(user.csrf.as_str()) {
        return Ok(super::session::csrf_rejection());
    }
    match play_selection(&state, &user, &pairs).await {
        Ok(response) => Ok(response),
        Err(error) => play_problem(&user, error),
    }
}

async fn play_selection(
    state: &AppState,
    user: &WebUser,
    pairs: &[(String, String)],
) -> Result<Response, ApiError> {
    let mut source = Source {
        asset_id: settings::field(pairs, "asset_id").map(str::to_owned),
        status_id: settings::field(pairs, "status_id").map(str::to_owned),
        episode_id: settings::field(pairs, "episode_id").map(str::to_owned),
        remote_id: settings::field(pairs, "remote_id").map(str::to_owned),
    };
    if let Some(url) = settings::field(pairs, "url") {
        if [
            &source.asset_id,
            &source.status_id,
            &source.episode_id,
            &source.remote_id,
        ]
        .iter()
        .any(|id| id.is_some())
        {
            return Err(ApiError::Unprocessable("Choose one song".into()));
        }
        let resource = crate::music_remote::import(state, user.current.account.id, url).await?;
        source.remote_id = Some(resource.id.to_string());
    }
    if let Some(id) = &source.remote_id {
        crate::music_remote::stream::source(
            state,
            id.parse().map_err(|_| ApiError::NotFound)?,
            user.current.account.id,
        )
        .await?;
    }
    let sources = selection_sources(state, user, pairs, source).await?;
    let queue = music_queues::get(state, user.current.account.id, "web").await?;
    music_queues::change(
        state,
        user.current.account.id,
        "web",
        &music_queues::Change {
            revision: queue.revision,
            action: music_queues::Action::Play { sources },
        },
    )
    .await?;
    Ok(settings::redirect_to("/music/player?autoplay=true"))
}

pub(super) fn url_play_form(user: &WebUser, url: &str) -> Markup {
    html! {form.music-play-now data-error=(user.locale.text("music-player-error")) method="post" action="/web/music/play"{
        input type="hidden" name="csrf" value=(user.csrf);input type="hidden" name="url" value=(url);
        button type="submit"{(user.locale.text("music-play-now"))}
    }}
}

fn play_problem(user: &WebUser, error: ApiError) -> Result<Response, ApiError> {
    use axum::http::StatusCode;
    let (status, key) = match error {
        ApiError::NotFound | ApiError::Forbidden(_) => {
            (StatusCode::NOT_FOUND, "music-play-unavailable")
        }
        ApiError::Conflict(_) => (StatusCode::CONFLICT, "music-play-changed"),
        ApiError::BadRequest(_) | ApiError::Unprocessable(_) | ApiError::ServiceUnavailable(_) => {
            (StatusCode::UNPROCESSABLE_ENTITY, "music-play-unavailable")
        }
        other => return Err(other),
    };
    let l = user.locale;
    let body = html! {section.column{h1{(l.text("music-play-problem"))}p role="alert"{(l.text(key))}a href="/music" {(l.text("music-discovery-browse"))}}};
    Ok((
        status,
        [(header::CACHE_CONTROL, "private, no-store")],
        layout::shell(&l.text("music-play-problem"), Some(user), &body),
    )
        .into_response())
}

async fn selection_sources(
    state: &AppState,
    user: &WebUser,
    pairs: &[(String, String)],
    source: Source,
) -> Result<Vec<Source>, ApiError> {
    let groups = [
        ("collection", settings::field(pairs, "collection_id")),
        ("album", settings::field(pairs, "album_id")),
        ("artist", settings::field(pairs, "artist_id")),
    ]
    .into_iter()
    .filter_map(|(kind, id)| id.map(|id| (kind, id)))
    .collect::<Vec<_>>();
    if groups.is_empty() {
        return Ok(vec![source]);
    }
    if groups.len() != 1
        || [
            &source.asset_id,
            &source.status_id,
            &source.episode_id,
            &source.remote_id,
        ]
        .iter()
        .any(|id| id.is_some())
    {
        return Err(ApiError::Unprocessable(
            "Choose one song or collection".into(),
        ));
    }
    let (kind, id) = groups[0];
    if kind == "collection" {
        return music_queues::collection_sources(state, user.current.account.id, id).await;
    }
    let kind = if kind == "album" {
        crate::music_entities::Kind::Album
    } else {
        crate::music_entities::Kind::Artist
    };
    let page = crate::music_entities::tracks(
        state,
        kind,
        id.parse().map_err(|_| ApiError::NotFound)?,
        Some(user.current.account.id),
        settings::field(pairs, "after")
            .map(str::parse::<i64>)
            .transpose()
            .map_err(|_| ApiError::NotFound)?,
    )
    .await?;
    Ok(page
        .items
        .into_iter()
        .map(|track| Source::asset(track.id))
        .collect())
}

pub(super) fn group_play_form(user: &WebUser, kind: &str, id: i64, after: Option<i64>) -> Markup {
    html! {form.music-play-now data-error=(user.locale.text("music-player-error")) method="post" action="/web/music/play"{
        input type="hidden" name="csrf" value=(user.csrf);input type="hidden" name=(format!("{kind}_id")) value=(id);
        @if let Some(after)=after{input type="hidden" name="after" value=(after);}
        button type="submit"{(user.locale.text("music-play-all"))}
    }}
}
