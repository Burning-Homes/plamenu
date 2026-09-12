//! Link preview cards: the crawl queue, OpenGraph/oEmbed extraction through
//! the stubbed page fetcher, card sharing/refresh, edit resets and the
//! `card` attribute on Status entities.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{RemoteUser, StubFederation, create_local_account, test_app_with, test_state_with};
use http_body_util::BodyExt;
use plamenu::actions::{self, EditParams, PostParams};
use plamenu::{AppState, ingest, link_preview, remote};
use plamenu_db::{PgPool, instance_settings, media, preview_card, status, user};
use serde_json::{Value, json};
use tower::ServiceExt;

const NEWS_URL: &str = "https://news.example/articles/42";
const NEWS_PAGE: &str = concat!(
    "<!doctype html><html lang=\"en\"><head>",
    "<title>html title</title>",
    r#"<meta property="og:title" content="OG Title">"#,
    r#"<meta property="og:description" content="OG description">"#,
    r#"<meta property="og:image" content="/cover.jpg">"#,
    r#"<meta property="og:image:alt" content="a cover">"#,
    r#"<meta property="og:site_name" content="News">"#,
    "</head><body>article</body></html>",
);

fn state_with_news_page(pool: &PgPool) -> (AppState, Arc<StubFederation>) {
    let stub = Arc::new(StubFederation::default());
    stub.serve_page(NEWS_URL, NEWS_PAGE);
    (test_state_with(pool.clone(), stub.clone()), stub)
}

/// Posts a public status as a fresh local user and runs the crawl worker.
async fn post_and_crawl(state: &AppState, username: &str, text: &str) -> status::Status {
    create_local_account(&state.pool, username, username).await;
    let (stored, _) = actions::post_status(
        state,
        PostParams {
            username,
            text,
            visibility: "public",
            ..PostParams::default()
        },
    )
    .await
    .unwrap();
    link_preview::run_due(state).await;
    stored
}

/// The rendered Status entity, fetched anonymously through the router.
async fn status_json(state: &AppState, stub: &Arc<StubFederation>, status_id: i64) -> Value {
    let app = test_app_with(state.pool.clone(), stub.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/statuses/{status_id}"))
                .header(header::ACCEPT, "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn response_text(state: &AppState, stub: &Arc<StubFederation>, uri: &str) -> String {
    let response = test_app_with(state.pool.clone(), stub.clone())
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn local_status_link_gets_an_opengraph_card(pool: PgPool) {
    let (state, stub) = state_with_news_page(&pool);
    let stored = post_and_crawl(&state, "alice", &format!("read this: {NEWS_URL} now")).await;

    let entity = status_json(&state, &stub, stored.id).await;
    // The URL was linkified in the stored HTML (Mastodon's anchor shape).
    let content = entity["content"].as_str().unwrap();
    assert!(
        content.contains(r#"<a href="https://news.example/articles/42""#),
        "{content}"
    );
    assert!(
        content.contains(r#"<span class="invisible">https://</span>"#),
        "{content}"
    );

    let card = &entity["card"];
    assert_eq!(card["url"], NEWS_URL);
    assert_eq!(card["title"], "OG Title");
    assert_eq!(card["description"], "OG description");
    assert_eq!(card["type"], "link");
    // The card image is proxied through the instance, never the origin.
    let image = card["image"].as_str().unwrap();
    assert!(
        image.starts_with("https://plamenu.test/media/proxy/card/"),
        "{image}"
    );
    assert!(!image.contains("news.example"), "{image}");
    assert_eq!(card["image_description"], "a cover");
    assert_eq!(card["provider_name"], "News");
    assert_eq!(card["language"], "en");
    assert_eq!(card["blurhash"], Value::Null);
    assert_eq!(card["authors"], json!([]));
    assert_eq!(card["width"], 0);
    assert_eq!(card["height"], 0);
}

/// A Lemmy-style link post keeps its MAIN link (the Page's `Link` attachment,
/// stored as `external_url`) for the card — not a URL that happens to sit in the
/// body text (a crosspost backlink). The body link is never even fetched.
#[sqlx::test(migrations = "../db/migrations")]
async fn link_post_cards_its_main_link_not_a_body_url(pool: PgPool) {
    const MAIN_URL: &str = "https://main.example/topic";
    let (state, stub) = state_with_news_page(&pool); // NEWS_URL is the body red herring
    stub.serve_page(
        MAIN_URL,
        concat!(
            "<!doctype html><html lang=\"en\"><head><title>t</title>",
            r#"<meta property="og:title" content="Main Topic">"#,
            "</head><body>x</body></html>",
        ),
    );
    create_local_account(&pool, "alice", "alice").await;
    let sender = RemoteUser::new("remote.example", "bob");
    let author = remote::store_remote_actor(&pool, &sender.actor)
        .await
        .unwrap();

    let page = json!({
        "id": "https://remote.example/post/7",
        "type": "Page",
        "attributedTo": sender.actor.id,
        "name": "A link post",
        "content": format!(r#"<p>see also <a href="{NEWS_URL}">the backlink</a></p>"#),
        "attachment": [{ "type": "Link", "href": MAIN_URL }],
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
    });
    let stored = ingest::ingest_remote_note(&state, &author, &page)
        .await
        .unwrap();
    assert_eq!(
        stored.external_url.as_deref(),
        Some(MAIN_URL),
        "the Link attachment is stored as the main link"
    );
    link_preview::run_due(&state).await;

    // Only the main link was crawled; the body backlink was never fetched.
    assert_eq!(stub.page_fetches(), vec![MAIN_URL.to_owned()]);
    let card = status_json(&state, &stub, stored.id).await["card"].clone();
    assert_eq!(card["url"], MAIN_URL);
    assert_eq!(card["title"], "Main Topic");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn statuses_without_eligible_links_get_no_card(pool: PgPool) {
    let (state, stub) = state_with_news_page(&pool);
    // No link at all, and a link to our own instance: neither is crawled.
    let plain = post_and_crawl(&state, "alice", "no links here").await;
    let local_link =
        post_and_crawl(&state, "bob", "see https://plamenu.test/users/alice please").await;
    assert_eq!(
        status_json(&state, &stub, plain.id).await["card"],
        Value::Null
    );
    assert_eq!(
        status_json(&state, &stub, local_link.id).await["card"],
        Value::Null
    );
    assert!(stub.page_fetches().is_empty());
    // The queue is drained either way.
    assert_eq!(preview_card::pending_crawl_count(&pool).await.unwrap(), 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn statuses_with_media_or_quotes_get_no_card(pool: PgPool) {
    let (state, stub) = state_with_news_page(&pool);
    let alice = create_local_account(&pool, "alice", "alice").await;
    let upload = media::create_local(
        &pool,
        media::NewLocalMedia {
            width: Some(1),
            height: Some(1),
            ..media::NewLocalMedia::new(alice.id, plamenu_db::id::next(), "x.png", "image/png")
        },
    )
    .await
    .unwrap();
    let (with_media, _) = actions::post_status(
        &state,
        PostParams {
            username: "alice",
            text: &format!("look {NEWS_URL}"),
            visibility: "public",
            media_ids: &[upload.id],
            ..PostParams::default()
        },
    )
    .await
    .unwrap();

    let (quotable, _) = actions::post_status(
        &state,
        PostParams {
            username: "alice",
            text: "quote me",
            visibility: "public",
            ..PostParams::default()
        },
    )
    .await
    .unwrap();
    let (with_quote, _) = actions::post_status(
        &state,
        PostParams {
            username: "alice",
            text: &format!("quoting {NEWS_URL}"),
            visibility: "public",
            quoted_status_id: Some(quotable.id),
            ..PostParams::default()
        },
    )
    .await
    .unwrap();

    link_preview::run_due(&state).await;
    assert_eq!(
        status_json(&state, &stub, with_media.id).await["card"],
        Value::Null
    );
    assert_eq!(
        status_json(&state, &stub, with_quote.id).await["card"],
        Value::Null
    );
    assert!(stub.page_fetches().is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn cards_are_shared_and_refreshed_after_two_weeks(pool: PgPool) {
    let (state, stub) = state_with_news_page(&pool);
    let first = post_and_crawl(&state, "alice", &format!("a {NEWS_URL}")).await;
    let second = post_and_crawl(&state, "bob", &format!("b {NEWS_URL}")).await;

    // One fetch served both statuses: the second crawl reused the card.
    assert_eq!(stub.page_fetches(), vec![NEWS_URL.to_owned()]);
    let first_card = status_json(&state, &stub, first.id).await["card"].clone();
    let second_card = status_json(&state, &stub, second.id).await["card"].clone();
    assert_eq!(first_card["title"], "OG Title");
    assert_eq!(second_card["title"], "OG Title");

    // Once the card is stale, the next status re-fetches it.
    let card = preview_card::find_by_url(&pool, NEWS_URL)
        .await
        .unwrap()
        .unwrap();
    preview_card::backdate_updated_at(
        &pool,
        card.id,
        time::OffsetDateTime::now_utc() - time::Duration::days(15),
    )
    .await
    .unwrap();
    post_and_crawl(&state, "carol", &format!("c {NEWS_URL}")).await;
    assert_eq!(stub.page_fetches().len(), 2);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn editing_the_text_resets_and_recrawls_the_card(pool: PgPool) {
    let (state, stub) = state_with_news_page(&pool);
    let other_url = "https://blog.example/post";
    stub.serve_page(
        other_url,
        "<html><head><title>Blog post</title></head></html>",
    );
    let stored = post_and_crawl(&state, "alice", &format!("read {NEWS_URL}")).await;
    assert_eq!(
        status_json(&state, &stub, stored.id).await["card"]["title"],
        "OG Title"
    );

    let alice = plamenu_db::account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    actions::edit_status(
        &state,
        &alice,
        stored.id,
        EditParams {
            text: Some(&format!("read {other_url} instead")),
            ..EditParams::default()
        },
    )
    .await
    .unwrap();
    // The old card is gone immediately; the new one appears after the crawl.
    assert_eq!(
        status_json(&state, &stub, stored.id).await["card"],
        Value::Null
    );
    link_preview::run_due(&state).await;
    let card = status_json(&state, &stub, stored.id).await["card"].clone();
    assert_eq!(card["title"], "Blog post");
    assert_eq!(card["url"], other_url);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn editing_a_link_post_removes_derived_audio_before_recrawl(pool: PgPool) {
    let audio_page = "https://pod.example/episodes/one";
    let audio_url = "https://cdn.pod.example/one.mp3";
    let (state, stub) = state_with_news_page(&pool);
    stub.serve_page(
        audio_page,
        &format!(
            r#"<html><head><title>Episode</title><meta property="og:audio" content="{audio_url}"><meta property="og:audio:type" content="audio/mpeg"></head></html>"#,
        ),
    );
    let stored = post_and_crawl(&state, "alice", &format!("listen {audio_page}")).await;
    assert_eq!(
        media::for_statuses(&pool, &[stored.id]).await.unwrap()[&stored.id].len(),
        1
    );

    let alice = plamenu_db::account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    actions::edit_status(
        &state,
        &alice,
        stored.id,
        EditParams {
            text: Some(&format!("read {NEWS_URL} instead")),
            ..EditParams::default()
        },
    )
    .await
    .unwrap();
    assert!(
        media::for_statuses(&pool, &[stored.id])
            .await
            .unwrap()
            .is_empty()
    );

    link_preview::run_due(&state).await;
    let entity = status_json(&state, &stub, stored.id).await;
    assert!(entity["media_attachments"].as_array().unwrap().is_empty());
    assert_eq!(entity["card"]["url"], NEWS_URL);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn remote_status_anchors_get_cards_skipping_tags_and_mentions(pool: PgPool) {
    let (state, stub) = state_with_news_page(&pool);
    create_local_account(&pool, "alice", "alice").await;
    let sender = RemoteUser::new("remote.example", "bob");
    let author = remote::store_remote_actor(&pool, &sender.actor)
        .await
        .unwrap();

    let note = json!({
        "id": "https://remote.example/notes/1",
        "type": "Note",
        "attributedTo": sender.actor.id,
        "content": concat!(
            r#"<p><span class="h-card"><a href="https://plamenu.test/users/alice" class="u-url mention">@alice</a></span> "#,
            r#"<a href="https://remote.example/tags/news" class="mention hashtag" rel="tag">#news</a> "#,
            r#"<a href="https://news.example/articles/42">story</a></p>"#,
        ),
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "tag": [
            { "type": "Mention", "href": "https://plamenu.test/users/alice", "name": "@alice" },
        ],
    });
    let stored = ingest::ingest_remote_note(&state, &author, &note)
        .await
        .unwrap();
    link_preview::run_due(&state).await;

    // Only the real link was considered — and fetched exactly once.
    assert_eq!(stub.page_fetches(), vec![NEWS_URL.to_owned()]);
    let card = status_json(&state, &stub, stored.id).await["card"].clone();
    assert_eq!(card["url"], NEWS_URL);
    assert_eq!(card["title"], "OG Title");
}

/// Regression for #5: Castopod episode Notes carry only a page link, while the
/// page advertises the actual episode through Open Graph. Promote that stream
/// into the regular attachment pipeline so the web UI and Mastodon API both
/// expose a player, with the existing cache/direct-fallback policy intact.
#[sqlx::test(migrations = "../db/migrations")]
async fn castopod_episode_link_becomes_a_playable_audio_attachment(pool: PgPool) {
    const EPISODE: &str = "https://solarcast.cc/@solarcast/episodes/invisible-internet";
    const AUDIO: &str = "https://op3.dev/solarcast.cc/audio/invisible-internet.mp3?from=og";
    const OEMBED: &str = "https://solarcast.cc/@solarcast/episodes/invisible-internet/oembed.json";
    let stub = Arc::new(StubFederation::default());
    stub.serve_page(
        EPISODE,
        concat!(
            "<!doctype html><html lang=\"en\"><head>",
            r#"<title>The Invisible Internet Project</title>"#,
            r#"<meta property="og:title" content="The Invisible Internet Project">"#,
            r#"<meta property="og:image" content="https://solarcast.cc/media/episode.png">"#,
            r#"<meta property="og:audio" content="https://op3.dev/solarcast.cc/audio/invisible-internet.mp3?from=og">"#,
            r#"<meta property="og:audio:type" content="audio/mpeg">"#,
            r#"<meta name="twitter:card" content="player">"#,
            r#"<meta name="twitter:player" content="/embed/light">"#,
            r#"<link rel="alternate" type="application/json+oembed" href="/@solarcast/episodes/invisible-internet/oembed.json">"#,
            "</head></html>",
        ),
    );
    // Castopod's oEmbed is `rich`; Plamenu deliberately rejects script-based
    // rich embeds and must still fall back to the page's safe OG audio URL.
    stub.serve_page_as(
        OEMBED,
        OEMBED,
        "application/json",
        r#"{"type":"rich","html":"<iframe src=\"/embed/light\"></iframe>"}"#,
    );
    let state = test_state_with(pool.clone(), stub.clone());
    let sender = RemoteUser::new("solarcast.cc", "solarcast");
    let author = remote::store_remote_actor(&pool, &sender.actor)
        .await
        .unwrap();
    let note = json!({
        "id": "https://solarcast.cc/@solarcast/posts/episode-announcement",
        "type": "Note",
        "attributedTo": sender.actor.id,
        "content": format!(r#"<p><a href="{EPISODE}">new episode</a></p>"#),
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
    });
    let stored = ingest::ingest_remote_note(&state, &author, &note)
        .await
        .unwrap();
    assert_eq!(link_preview::run_due(&state).await, 1);

    let attachments = media::for_statuses(&pool, &[stored.id]).await.unwrap();
    let audio = &attachments[&stored.id][0];
    assert_eq!(audio.kind_or_derived(), "audio");
    assert_eq!(audio.remote_url.as_deref(), Some(AUDIO));
    assert_eq!(audio.content_type, "audio/mpeg");
    assert!(
        audio.download_on_demand,
        "podcasts must not download on render"
    );
    assert_eq!(audio.processing, "complete");
    let source_card: Option<i64> =
        sqlx::query_scalar("SELECT preview_card_id FROM media_attachments WHERE id = $1")
            .bind(audio.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        source_card.is_some(),
        "derived-media provenance is retained"
    );

    // API clients receive a standard Mastodon audio attachment through the
    // privacy-preserving sparse cache, not a static card or origin hotlink.
    let entity = status_json(&state, &stub, stored.id).await;
    assert_eq!(entity["card"], Value::Null);
    let api_audio = &entity["media_attachments"][0];
    assert_eq!(api_audio["type"], "audio");
    let playback_url = api_audio["url"].as_str().unwrap();
    assert_eq!(
        playback_url,
        format!("https://plamenu.test/media/play/{}/audio", audio.id)
    );
    assert!(!playback_url.contains("op3.dev"), "{playback_url}");

    // The same entity drives the first-party renderer, which must emit the
    // native playable control rather than the old static link card.
    let page = response_text(
        &state,
        &stub,
        &format!("/@solarcast@solarcast.cc/{}", stored.id),
    )
    .await;
    assert!(page.contains("media--audio"), "{page}");
    assert!(page.contains("<audio"), "{page}");
    assert!(page.contains(" controls"), "{page}");
    assert!(page.contains("preload=\"none\""), "{page}");
    assert!(page.contains("/media/play/"), "{page}");
    assert!(page.contains("/audio"), "{page}");

    // A signed-in reader's default direct-remote preference is encoded into
    // the API URL. When long-form caching is disabled/over budget, that marker
    // is what lets the proxy fall back to the episode origin.
    let viewer = create_local_account(&pool, "listener", "Listener").await;
    user::create(
        &pool,
        viewer.id,
        Some("listener@example.test"),
        "$argon2id$x",
    )
    .await
    .unwrap();
    let rendered = plamenu::entities::render_statuses(
        &pool,
        "plamenu.test",
        std::slice::from_ref(&stored),
        Some(viewer.id),
    )
    .await
    .unwrap();
    assert!(
        rendered[0]["media_attachments"][0]["url"]
            .as_str()
            .unwrap()
            .ends_with("?d=1")
    );
    let current = instance_settings::get(&pool).await.unwrap();
    instance_settings::save(
        &pool,
        instance_settings::SettingsUpdate {
            remote_video_max_mb: 0,
            ..current.as_update()
        },
    )
    .await
    .unwrap();
    state.settings_cache.invalidate();
    let fallback = test_app_with(pool.clone(), stub.clone())
        .oneshot(
            Request::builder()
                .uri(format!("/media/play/{}/audio?d=1", audio.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(fallback.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(fallback.headers()[header::LOCATION], AUDIO);

    // The audio is derived rather than present on the signed Note. A remote
    // Update that changes another field must therefore preserve it instead of
    // mistaking it for a removed wire attachment.
    let mut updated_note = note;
    updated_note["summary"] = json!("Episode");
    updated_note["updated"] = json!("2026-09-12T12:00:00Z");
    ingest::update_remote_note(&state, &author, &stored, &updated_note)
        .await
        .unwrap();
    let after_update = media::for_statuses(&pool, &[stored.id]).await.unwrap();
    assert_eq!(after_update[&stored.id].len(), 1);
    assert_eq!(
        after_update[&stored.id][0].remote_url.as_deref(),
        Some(AUDIO)
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn oembed_endpoints_are_preferred_over_opengraph(pool: PgPool) {
    let stub = Arc::new(StubFederation::default());
    let page_url = "https://tube.example/v/9";
    stub.serve_page(
        page_url,
        concat!(
            "<html><head><title>ignored by oembed</title>",
            r#"<link rel="alternate" type="application/json+oembed" href="/oembed?v=9">"#,
            "</head></html>",
        ),
    );
    stub.serve_page_as(
        "https://tube.example/oembed?v=9",
        "https://tube.example/oembed?v=9",
        "application/json",
        &json!({
            "type": "video",
            "title": "A video",
            "author_name": "Creator",
            "author_url": "/c/creator",
            "provider_name": "Tube",
            "html": "<iframe src=\"https://tube.example/e/9\"></iframe>",
            "width": 640,
            "height": 360,
            "thumbnail_url": "/thumb/9.jpg",
        })
        .to_string(),
    );
    let state = test_state_with(pool.clone(), stub.clone());
    let stored = post_and_crawl(&state, "alice", &format!("watch {page_url}")).await;

    let card = status_json(&state, &stub, stored.id).await["card"].clone();
    assert_eq!(card["type"], "video");
    assert_eq!(card["title"], "A video");
    assert_eq!(card["width"], 640);
    assert_eq!(card["height"], 360);
    assert_eq!(card["provider_name"], "Tube");
    // Relative oEmbed URLs resolve against the endpoint.
    assert_eq!(card["author_url"], "https://tube.example/c/creator");
    let image = card["image"].as_str().unwrap();
    assert!(
        image.starts_with("https://plamenu.test/media/proxy/card/"),
        "{image}"
    );
    assert!(!image.contains("tube.example"), "{image}");
    assert_eq!(
        card["authors"],
        json!([{
            "name": "Creator",
            "url": "https://tube.example/c/creator",
            "account": null,
        }])
    );
    let html = card["html"].as_str().unwrap();
    assert!(html.contains(r#"src="https://tube.example/e/9""#), "{html}");
    assert!(html.contains("sandbox=\"allow-scripts"), "{html}");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn redirects_store_the_card_under_the_canonical_url(pool: PgPool) {
    let stub = Arc::new(StubFederation::default());
    let short = "https://sho.example/abc";
    // The shortener redirects (final_url differs) and the page canonicalizes
    // away its query string.
    stub.serve_page_as(
        short,
        "https://news.example/articles/42?utm=tracker",
        "text/html",
        concat!(
            "<html><head><title>Article</title>",
            r#"<link rel="canonical" href="https://news.example/articles/42">"#,
            "</head></html>",
        ),
    );
    let state = test_state_with(pool.clone(), stub.clone());
    let stored = post_and_crawl(&state, "alice", &format!("via {short}")).await;

    // The card row is keyed by the canonical URL…
    assert!(
        preview_card::find_by_url(&pool, "https://news.example/articles/42")
            .await
            .unwrap()
            .is_some()
    );
    // …but the API serves the link as it appeared in the status.
    let card = status_json(&state, &stub, stored.id).await["card"].clone();
    assert_eq!(card["url"], short);
    assert_eq!(card["title"], "Article");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn failed_and_non_html_fetches_leave_no_card(pool: PgPool) {
    let stub = Arc::new(StubFederation::default());
    stub.serve_page_as(
        "https://api.example/data",
        "https://api.example/data",
        "application/json",
        "{}",
    );
    let state = test_state_with(pool.clone(), stub.clone());
    // 404 (unserved URL) and a non-HTML content type: both no-ops.
    let gone = post_and_crawl(&state, "alice", "x https://gone.example/x").await;
    let json_link = post_and_crawl(&state, "bob", "x https://api.example/data").await;
    assert_eq!(
        status_json(&state, &stub, gone.id).await["card"],
        Value::Null
    );
    assert_eq!(
        status_json(&state, &stub, json_link.id).await["card"],
        Value::Null
    );
    assert_eq!(preview_card::pending_crawl_count(&pool).await.unwrap(), 0);
}

/// `fediverse:creator` author attribution (Mastodon's `attributionDomains`
/// enforcement): the named account becomes the card's verified author only
/// when its attribution domains authorize the page's domain — including via
/// a parent suffix — and the Status entity's `authors[].account` carries the
/// full Account entity.
#[sqlx::test(migrations = "../db/migrations")]
async fn fediverse_creator_attribution_respects_attribution_domains(pool: PgPool) {
    let stub = Arc::new(StubFederation::default());
    let creator_page = concat!(
        "<!doctype html><html><head>",
        r#"<meta property="og:title" content="By our author">"#,
        r#"<meta name="fediverse:creator" content="@author@plamenu.test">"#,
        "</head><body>article</body></html>",
    );
    stub.serve_page("https://blog.news.example/post", creator_page);
    let state = test_state_with(pool.clone(), stub.clone());

    let author = create_local_account(&pool, "author", "The Author").await;

    // No attribution domains yet: the creator tag is ignored.
    let unverified =
        post_and_crawl(&state, "reader1", "see https://blog.news.example/post now").await;
    let card = preview_card::find_by_url(&pool, "https://blog.news.example/post")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(card.author_account_id, None);
    let entity = status_json(&state, &stub, unverified.id).await;
    assert_eq!(entity["card"]["authors"], json!([]));

    // The apex domain authorizes the blog subdomain (parent-suffix match).
    plamenu_db::account::set_attribution_domains(&pool, author.id, &["news.example".to_owned()])
        .await
        .unwrap();
    // Force a re-crawl of the same URL.
    plamenu_db::preview_card::backdate_updated_at(
        &pool,
        card.id,
        time::OffsetDateTime::now_utc() - time::Duration::days(30),
    )
    .await
    .unwrap();
    let verified = post_and_crawl(
        &state,
        "reader2",
        "see https://blog.news.example/post again",
    )
    .await;
    let card = preview_card::find_by_url(&pool, "https://blog.news.example/post")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(card.author_account_id, Some(author.id));
    let entity = status_json(&state, &stub, verified.id).await;
    assert_eq!(entity["card"]["authors"][0]["account"]["acct"], "author");
    assert_eq!(
        entity["card"]["authors"][0]["account"]["display_name"],
        "The Author"
    );
}

/// N+1 close-out: the worker's batch-context
/// path must decide every job exactly as the per-job path it falls back to.
/// Two identical fixture sets cover the decision arms — a real fetch, a fresh
/// stored card reused without fetching, an already-carded status, media and
/// quote suppression, and a remote status whose mention anchor must be
/// filtered via the batched mention map — set `a` crawled through
/// `crawl_status` (the per-job fallback), set `b` drained through `run_due`
/// (the batch path), then compared arm by arm.
#[sqlx::test(migrations = "../db/migrations")]
async fn crawl_batch_matches_the_per_job_path(pool: PgPool) {
    use plamenu_db::media::NewRemoteMedia;
    use plamenu_db::preview_card::NewPreviewCard;
    use plamenu_db::status::{NewLocalStatus, NewRemoteStatus};
    use plamenu_db::{mention, quote};

    const PAGE: &str = concat!(
        "<!doctype html><html lang=\"en\"><head><title>t</title>",
        r#"<meta property="og:title" content="Fetched Title">"#,
        "</head><body>x</body></html>",
    );

    /// One local plain-text status whose raw source is `text`.
    async fn text_status(pool: &PgPool, account_id: i64, text: &str) -> status::Status {
        status::create_local(
            pool,
            NewLocalStatus {
                text,
                ..NewLocalStatus::new(account_id, text, "public", None)
            },
        )
        .await
        .unwrap()
    }

    async fn crawl_fixtures(
        state: &AppState,
        stub: &Arc<StubFederation>,
        tag: &str,
    ) -> Vec<(&'static str, i64)> {
        let pool = &state.pool;
        let author = create_local_account(pool, &format!("author_{tag}"), "Author").await;
        let mut out = Vec::new();

        // A link that must be fetched and carded.
        let fetch_url = format!("https://news.example/{tag}/fetch");
        stub.serve_page(&fetch_url, PAGE);
        let s = text_status(pool, author.id, &format!("read {fetch_url} now")).await;
        out.push(("fetch", s.id));

        // A fresh stored card for the URL: reused, never fetched.
        let fresh_url = format!("https://news.example/{tag}/fresh");
        preview_card::upsert(
            pool,
            NewPreviewCard {
                url: &fresh_url,
                title: "Fresh card",
                kind: "link",
                ..NewPreviewCard::default()
            },
        )
        .await
        .unwrap();
        let s = text_status(pool, author.id, &format!("see {fresh_url}")).await;
        out.push(("fresh", s.id));

        // A status already carrying a card: the crawl is skipped.
        let carded_url = format!("https://news.example/{tag}/carded");
        let card = preview_card::upsert(
            pool,
            NewPreviewCard {
                url: &carded_url,
                title: "Old card",
                kind: "link",
                ..NewPreviewCard::default()
            },
        )
        .await
        .unwrap();
        let s = text_status(pool, author.id, &format!("again {carded_url}")).await;
        preview_card::attach(pool, s.id, card.id, &carded_url)
            .await
            .unwrap();
        out.push(("carded", s.id));

        // Attachments suppress the card.
        let s = text_status(
            pool,
            author.id,
            &format!("pic https://news.example/{tag}/media"),
        )
        .await;
        media::create_remote(
            pool,
            NewRemoteMedia {
                account_id: author.id,
                status_id: s.id,
                remote_url: "https://files.example/x.png",
                content_type: "image/png",
                ..NewRemoteMedia::default()
            },
        )
        .await
        .unwrap();
        out.push(("media", s.id));

        // Quote posts never card.
        let quoted = text_status(pool, author.id, "the original").await;
        let s = text_status(
            pool,
            author.id,
            &format!("quoting https://news.example/{tag}/quoted"),
        )
        .await;
        quote::create(
            pool,
            quote::NewQuote {
                quote_id: plamenu_db::id::next(),
                status_id: Some(s.id),
                status_uri: &format!("https://plamenu.test/statuses/{}", s.id),
                account_id: author.id,
                quoted_status_id: Some(quoted.id),
                quoted_account_id: Some(author.id),
                state: "accepted",
                activity_uri: None,
                approval_uri: None,
                quoted_uri: None,
                legacy: false,
            },
        )
        .await
        .unwrap();
        out.push(("quoted", s.id));

        out.push(("mention", mention_fixture(state, stub, tag).await));

        out
    }

    /// A remote status whose first anchor is a mention: it must be filtered
    /// (via the batch's mention map on the `b` set) and the second anchor
    /// carded.
    async fn mention_fixture(state: &AppState, stub: &Arc<StubFederation>, tag: &str) -> i64 {
        let pool = &state.pool;
        let bob = remote::store_remote_actor(
            pool,
            &RemoteUser::new("remote.example", &format!("bob_{tag}")).actor,
        )
        .await
        .unwrap();
        let carol = remote::store_remote_actor(
            pool,
            &RemoteUser::new("remote.example", &format!("carol_{tag}")).actor,
        )
        .await
        .unwrap();
        let mention_url = format!("https://news.example/{tag}/mentioned");
        stub.serve_page(&mention_url, PAGE);
        let content = format!(
            r#"<p><a href="{}">@bob</a> look <a href="{mention_url}">news</a></p>"#,
            bob.uri.as_deref().unwrap(),
        );
        let s = status::upsert_remote(
            pool,
            NewRemoteStatus {
                uri: &format!("https://remote.example/notes/{tag}-mention"),
                account_id: carol.id,
                content: &content,
                created_at: time::OffsetDateTime::now_utc(),
                visibility: "public",
                in_reply_to_id: None,
                in_reply_to_uri: None,
                spoiler_text: "",
                sensitive: false,
                language: None,
                url: None,
                quote_approval_policy: 0,
                title: None,
                object_type: None,
                external_url: None,
            },
        )
        .await
        .unwrap();
        mention::attach(pool, s.id, bob.id, false).await.unwrap();
        s.id
    }

    let stub = Arc::new(StubFederation::default());
    let state = test_state_with(pool.clone(), stub.clone());

    // Set `a`: the per-job fallback path, one status at a time.
    let a = crawl_fixtures(&state, &stub, "a").await;
    for (_, id) in &a {
        link_preview::crawl_status(&state, *id).await.unwrap();
    }

    // Set `b`: the batch path, drained through the worker.
    let b = crawl_fixtures(&state, &stub, "b").await;
    for (_, id) in &b {
        preview_card::enqueue_crawl(&pool, *id).await.unwrap();
    }
    link_preview::run_due(&state).await;

    for ((name, a_id), (_, b_id)) in a.iter().zip(&b) {
        let a_card = preview_card::for_statuses(&pool, &[*a_id])
            .await
            .unwrap()
            .remove(a_id);
        let b_card = preview_card::for_statuses(&pool, &[*b_id])
            .await
            .unwrap()
            .remove(b_id);
        match (&a_card, &b_card) {
            (Some((a_card, a_url)), Some((b_card, b_url))) => {
                assert_eq!(a_card.title, b_card.title, "{name}: card title diverged");
                assert_eq!(a_card.kind, b_card.kind, "{name}: card kind diverged");
                assert_eq!(
                    a_url.replace("/a/", "/_/"),
                    b_url.replace("/b/", "/_/"),
                    "{name}: original link diverged"
                );
            }
            (None, None) => {}
            _ => panic!("{name}: per-job {a_card:?} vs batch {b_card:?}"),
        }
    }

    // The arms must have done what they claim, not just agree while broken.
    let fetched = stub.page_fetches();
    for tag in ["a", "b"] {
        assert!(
            fetched.contains(&format!("https://news.example/{tag}/fetch")),
            "{tag}: the fetch arm must hit the page: {fetched:?}"
        );
        assert!(
            fetched.contains(&format!("https://news.example/{tag}/mentioned")),
            "{tag}: the mention arm must card its second anchor: {fetched:?}"
        );
    }
    assert!(
        !fetched.iter().any(|u| u.contains("/fresh")),
        "a fresh stored card must be reused without fetching: {fetched:?}"
    );
    assert!(
        !fetched.iter().any(|u| u.contains("/users/bob")),
        "mention anchors must never be crawled: {fetched:?}"
    );
    for (name, id) in a.iter().chain(&b) {
        let has_card = preview_card::exists_for_status(&pool, *id).await.unwrap();
        let want = matches!(*name, "fetch" | "fresh" | "carded" | "mention");
        assert_eq!(has_card, want, "{name}: unexpected card presence");
    }
}
