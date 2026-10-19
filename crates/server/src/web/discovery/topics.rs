//! Saved topic views and attributable connections, using ordinary links/forms.
use super::{
    ApiError, AppState, Form, IntoResponse, Markup, PageQuery, Path, Query, Redirect, Response,
    State, WebUser, csrf_rejection, db, html, layout, pager, tabs,
};
use crate::discovery::topics as service;
use serde::Deserialize;

fn topic_url(topic: &str) -> String {
    format!(
        "/discover/topics?{}",
        serde_urlencoded::to_string([("topic", topic)]).unwrap_or_default()
    )
}

fn links(topics: &[String], user: &WebUser) -> Markup {
    html! {ul.discovery-topics aria-label=(user.locale.text("discovery-topics")) {
        @for topic in topics {li {a href=(topic_url(topic)) {"#" (topic)}}}
    }}
}

fn shell(user: &WebUser, body: &Markup) -> Response {
    let l = user.locale;
    let content = html! {section.column.discovery.discovery--topics data-discovery-view="topics" {
        header.column-heading {h1 {(l.text("discovery-title"))}}
        (tabs(user,"topics")) (body)
    }};
    (
        [("Cache-Control", "private, no-store"), ("Vary", "Cookie")],
        layout::shell(&l.text("discovery-title"), Some(user), &content),
    )
        .into_response()
}

async fn cards(
    state: &AppState,
    user: &WebUser,
    entries: &[db::Entry],
    back: &str,
) -> Result<Markup, ApiError> {
    let mut rendered = Vec::new();
    let images = super::images::Images::load(state, user, entries, &[]).await?;
    for entry in entries {
        rendered.push(if entry.kind == "video" {
            super::videos::card(state, entry, user, back, &images).await?
        } else if entry.kind == "post" {
            super::posts::card(state, entry, user, "", back, &images).await?
        } else {
            super::card(entry, user, back, &images)
        });
    }
    Ok(html! {div.discovery-grid data-discovery-results="topics" {@for card in rendered {(card)}}})
}

fn collection_cards(collections: &[service::PublicCollection], user: &WebUser) -> Markup {
    let l = user.locale;
    html! {@if !collections.is_empty() {
        h2.discovery-section-title {(l.text("discovery-topic-collections"))}
        div.discovery-grid {@for c in collections {
            article.discovery-card {
                h3 {a href=(format!("/discover/collections/{}",c.collection.id)) {(c.collection.name)}}
                p.discovery-card__server {(l.text("discovery-curated-by")) " " (if c.owner.display_name.is_empty() { &c.owner.username } else { &c.owner.display_name })}
                @if c.collection.sensitive {p {(l.text("discovery-sensitive"))}}
            }
        }}
    }}
}

fn save_button(user: &WebUser, topic: &str, saved: bool) -> Markup {
    html! {form method="post" action="/web/discover/topics" {
        input type="hidden" name="csrf" value=(user.csrf);
        input type="hidden" name="topic" value=(topic);
        input type="hidden" name="action" value=(if saved {"remove"} else {"save"});
        button.secondary type="submit" {(user.locale.text(if saved {"discovery-unsave-topic"} else {"discovery-save-topic"}))}
    }}
}

pub async fn index(
    State(state): State<AppState>,
    user: WebUser,
    Query(q): Query<PageQuery>,
) -> Result<Response, ApiError> {
    let l = user.locale;
    let saved = db::saved_topics(&state.pool, user.current.account.id).await?;
    let topic = q
        .topic
        .as_deref()
        .map(|t| {
            db::normalize_topic(t)
                .ok_or_else(|| ApiError::BadRequest("Invalid discovery topic".into()))
        })
        .transpose()?;
    let body = if let Some(topic) = topic {
        let page = service::page(&state, user.current.account.id, q.cursor()?, &topic).await?;
        let collections = service::collections(&state, user.current.account.id, &topic, "").await?;
        let rendered = cards(&state, &user, &page.entries, &topic_url(&topic)).await?;
        let next = page.next.map(|c| {
            format!(
                "{}&{}",
                topic_url(&topic),
                serde_urlencoded::to_string(c).unwrap_or_default()
            )
        });
        html! {
            div.discovery-topic-heading {
                div {p {a href="/discover/topics" {(l.text("discovery-all-topics"))}} h2 {"#" (topic)}}
                (save_button(&user,&topic,saved.contains(&topic)))
            }
            p.discovery-topic-help {(l.text("discovery-topic-reason"))}
            @if q.notice.as_deref()==Some("hidden") {p.discovery-state role="status" {(l.text("discovery-hidden"))}}
            @if q.notice.as_deref()==Some("unavailable") {p.discovery-state role="status" {(l.text("discovery-unavailable"))}}
            @if q.notice.as_deref()==Some("limit") {p.discovery-state role="status" {(l.text("discovery-topic-limit"))}}
            @if page.entries.is_empty() {p.empty {(l.text("discovery-topic-empty"))}}
            div data-discovery-topic=(&topic) {(rendered) (pager(&user,next.as_deref()))}
            (collection_cards(&collections,&user))
        }
    } else {
        let available = service::available(&state, user.current.account.id).await?;
        html! {
            div.discovery-topic-heading {h2 {(l.text("discovery-saved-topics"))}
                @if !saved.is_empty() {form method="post" action="/web/discover/topics" {
                    input type="hidden" name="csrf" value=(user.csrf);
                    input type="hidden" name="action" value="reset";
                    button.secondary type="submit" {(l.text("discovery-reset-topics"))}
                }}
            }
            p.discovery-topic-help {(l.text("discovery-saved-private"))}
            @if saved.is_empty() {p.empty {(l.text("discovery-saved-empty"))}} @else {(links(&saved,&user))}
            h2.discovery-section-title {(l.text("discovery-available-topics"))}
            @if available.is_empty() {p.empty {(l.text("discovery-topics-empty"))}} @else {(links(&available,&user))}
        }
    };
    Ok(shell(
        &user,
        &html! {
            @if !service::enabled(&state).await? {p.discovery-state role="status" {(l.text("discovery-topics-disabled"))}}
            (body)
        },
    ))
}

#[derive(Deserialize)]
pub struct TopicForm {
    csrf: String,
    action: String,
    topic: Option<String>,
}

pub async fn save(
    State(state): State<AppState>,
    user: WebUser,
    Form(form): Form<TopicForm>,
) -> Result<Response, ApiError> {
    if !user.csrf_ok(&form.csrf) {
        return Ok(csrf_rejection());
    }
    if form.action == "reset" {
        db::remove_topics(&state.pool, user.current.account.id, None).await?;
        return Ok(Redirect::to("/discover/topics").into_response());
    }
    let topic = form
        .topic
        .as_deref()
        .and_then(db::normalize_topic)
        .ok_or_else(|| ApiError::BadRequest("Invalid discovery topic".into()))?;
    let mut destination = topic_url(&topic);
    match form.action.as_str() {
        "save" => {
            if !db::save_topic(&state.pool, user.current.account.id, &topic).await? {
                destination.push_str("&notice=limit");
            }
        }
        "remove" => db::remove_topics(&state.pool, user.current.account.id, Some(&topic)).await?,
        _ => return Err(ApiError::BadRequest("Invalid discovery action".into())),
    }
    Ok(Redirect::to(&destination).into_response())
}

pub async fn trail(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    let l = user.locale;
    let Some(entry) = service::eligible(&state, user.current.account.id, id).await? else {
        return Ok(shell(
            &user,
            &html! {p.empty {(l.text("discovery-unavailable"))}},
        ));
    };
    let topics = db::entry_topics(&state.pool, id).await?;
    let mut related = Vec::new();
    if entry.kind == "post" {
        if let Some(uri) = &entry.actor_uri
            && let Some(actor) = db::entry(&state.pool, uri).await?
            && let Some(actor) =
                service::eligible(&state, user.current.account.id, actor.id).await?
        {
            related.push(actor);
        }
    } else if entry.kind == "profile" {
        related = service::authored(&state, user.current.account.id, &entry.canonical_uri).await?;
    }
    let collections =
        service::collections(&state, user.current.account.id, "", &entry.canonical_uri).await?;
    let focus = cards(
        &state,
        &user,
        std::slice::from_ref(&entry),
        "/discover/topics",
    )
    .await?;
    let related_cards = cards(&state, &user, &related, "/discover/topics").await?;
    Ok(shell(
        &user,
        &html! {
            div.discovery-topic-heading {h2 {(l.text("discovery-connections"))}}
            div.discovery-trail-focus {(focus)}
            @if !topics.is_empty() {
                h2.discovery-section-title {(l.text("discovery-tagged-topics"))}
                @if entry.sensitive {details.discovery-topic-warning {summary {(l.text("discovery-sensitive"))} (links(&topics,&user))}}
                @else {(links(&topics,&user))}
            }
            @if !related.is_empty() {
                h2.discovery-section-title {(l.text(if entry.kind=="post" {"discovery-by-author"} else {"discovery-author-posts"}))}
                (related_cards)
            }
            (collection_cards(&collections,&user))
            @if topics.is_empty() && related.is_empty() && collections.is_empty() {p.empty {(l.text("discovery-connections-empty"))}}
        },
    ))
}

pub async fn collection(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    let l = user.locale;
    let Some(c) = service::public_collection(&state, user.current.account.id, id).await? else {
        return Ok(shell(
            &user,
            &html! {p.empty {(l.text("discovery-unavailable"))}},
        ));
    };
    let rendered = cards(
        &state,
        &user,
        &c.entries,
        &format!("/discover/collections/{id}"),
    )
    .await?;
    let topic = match c.collection.tag_id {
        Some(id) => plamenu_db::tag::name_of(&state.pool, id)
            .await?
            .as_deref()
            .and_then(db::normalize_topic),
        None => None,
    };
    let body = html! {
        @if let Some(topic)=topic {(links(&[topic],&user))}
        p.discovery-topic-help {(c.collection.description)}
        (rendered)
    };
    Ok(shell(
        &user,
        &html! {
            div.discovery-topic-heading {div {h2 {(c.collection.name)}
                p {(l.text("discovery-curated-by")) " "
                    a href=(super::super::pages::account_local_path(&c.owner,&state.config.domain)) {
                        (if c.owner.display_name.is_empty() { &c.owner.username } else { &c.owner.display_name })
                    }
                }
            }}
            @if c.collection.sensitive {details.discovery-topic-warning {summary {(l.text("discovery-sensitive"))} (body)}} @else {(body)}
        },
    ))
}
