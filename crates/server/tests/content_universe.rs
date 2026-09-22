//! Content-universe corpus tests: non-microblog objects (Lemmy `Page`,
//! `PeerTube` `Video`, forum `Article`s, `Event`s, …) driven through the signed
//! inbox path from checked-in wire fixtures. Provenance for every fixture is
//! documented in `fixtures/README.md` — the sender is always
//! `bob@remote.example`, the recipient instance `plamenu.test`.

mod common;

use std::time::SystemTime;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{RemoteUser, StubFederation, TEST_DOMAIN, create_local_account, test_app_with};
use plamenu_db::{PgPool, account, media, status, status_event};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tower::ServiceExt;

fn fixture(raw: &str) -> Value {
    serde_json::from_str(raw).expect("fixture is valid JSON")
}

/// The fixture sender every corpus file is written against.
fn sender() -> RemoteUser {
    RemoteUser::new("remote.example", "bob")
}

async fn post_signed(app: Router, body: &Value, user: &RemoteUser) -> StatusCode {
    let bytes = serde_json::to_vec(body).unwrap();
    let signed_headers = user
        .signer()
        .sign_post(TEST_DOMAIN, "/inbox", &bytes, SystemTime::now());
    let request = Request::builder()
        .method("POST")
        .uri("/inbox")
        .header("host", signed_headers.host)
        .header("date", signed_headers.date)
        .header("digest", signed_headers.digest)
        .header("signature", signed_headers.signature)
        .header("content-type", "application/activity+json")
        .body(Body::from(bytes))
        .unwrap();
    app.oneshot(request).await.unwrap().status()
}

/// The stored status a fixture's object should have produced.
async fn stored_object(pool: &PgPool, activity: &Value) -> status::Status {
    let uri = activity["object"]["id"].as_str().unwrap();
    status::find_by_uri(pool, uri)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("fixture object {uri} was not stored"))
}

async fn stored_media(pool: &PgPool, status_id: i64) -> Vec<media::Media> {
    media::for_statuses(pool, &[status_id])
        .await
        .unwrap()
        .remove(&status_id)
        .unwrap_or_default()
}

/// A Lemmy link post carries its target as `attachment: [{type: Link, href}]`
/// (`href`, not `url`): the target must land in `external_url`, the `Link`
/// must not be stored as media (the real `Image` attachment still is), and
/// the `language: {identifier}` shape must be read.
#[sqlx::test(migrations = "../db/migrations")]
async fn lemmy_link_post_stores_external_url_not_media(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let activity = fixture(include_str!(
        "fixtures/lemmy/create_page_image_and_link.json"
    ));

    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub), &activity, &bob).await,
        StatusCode::ACCEPTED
    );
    let stored = stored_object(&pool, &activity).await;
    assert_eq!(
        stored.external_url.as_deref(),
        Some("https://example.com/some-article"),
        "the Link attachment href is the post's external link"
    );
    assert_eq!(stored.language.as_deref(), Some("fr"));
    let attached = stored_media(&pool, stored.id).await;
    assert_eq!(
        attached.len(),
        1,
        "the Image attachment is media, the Link is not: {attached:?}"
    );
    assert_eq!(
        attached[0].remote_url.as_deref(),
        Some("https://remote.example/pictrs/image/eOtYb9iEiB.png")
    );
}

/// The Mastodon-API `content` folds a titled/link post's `name` and link
/// target in, the way Mastodon surfaces converted `Page`/`Article`/`Video`
/// types — a stock client reads only `content`, so a title/link confined to
/// Plamenu's extension keys would be invisible there. The extension keys stay
/// too (feature detection); the first-party UI renders the folded content.
#[sqlx::test(migrations = "../db/migrations")]
async fn titled_link_post_folds_title_and_link_into_api_content(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let activity = fixture(include_str!(
        "fixtures/lemmy/create_page_image_and_link.json"
    ));

    let app = test_app_with(pool.clone(), stub);
    assert_eq!(
        post_signed(app.clone(), &activity, &bob).await,
        StatusCode::ACCEPTED
    );
    let stored = stored_object(&pool, &activity).await;

    let request = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/statuses/{}", stored.id))
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let entity: Value = serde_json::from_slice(&bytes).unwrap();
    let content = entity["content"].as_str().unwrap();
    assert!(
        content
            .contains(r#"<p class="status__title"><strong>Post with image and link</strong></p>"#),
        "the title leads the API content as a bold line: {content}"
    );
    assert!(
        content.contains("status__external-link")
            && content.contains(r#"href="https://example.com/some-article""#),
        "the link target trails the API content: {content}"
    );
    // The extension keys still carry the raw values for feature detection.
    assert_eq!(entity["title"], "Post with image and link");
    assert_eq!(entity["external_url"], "https://example.com/some-article");
}

/// `NodeBB` sends explicit `"inReplyTo": null` / `"updated": null`; explicit
/// null must read as absent everywhere.
#[sqlx::test(migrations = "../db/migrations")]
async fn nodebb_explicit_nulls_are_tolerated(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let activity = fixture(include_str!("fixtures/nodebb/create_article.json"));

    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub), &activity, &bob).await,
        StatusCode::ACCEPTED
    );
    let stored = stored_object(&pool, &activity).await;
    assert_eq!(stored.in_reply_to_id, None, "null inReplyTo is not a reply");
    assert_eq!(stored.edited_at, None, "null updated is not an edit");
    assert_eq!(stored.title.as_deref(), Some("Welcome to your NodeBB!"));
    assert_eq!(
        stored.spoiler_text, "",
        "the excerpt summary is not a content warning"
    );
}

/// A Lemmy `Page` ingests natively: real title, the full body (not
/// Mastodon's `<h2>`-plus-link compaction), the stored object type, and the
/// link-post target.
#[sqlx::test(migrations = "../db/migrations")]
async fn lemmy_page_ingests_natively(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let activity = fixture(include_str!("fixtures/lemmy/create_page.json"));

    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub), &activity, &bob).await,
        StatusCode::ACCEPTED
    );
    let stored = stored_object(&pool, &activity).await;
    assert_eq!(stored.title.as_deref(), Some("Post title"));
    assert_eq!(stored.object_type.as_deref(), Some("Page"));
    assert!(
        stored
            .content
            .contains("This is a post in the /c/tenforward community"),
        "the body must be stored, not discarded: {}",
        stored.content
    );
    assert!(
        !stored.content.contains("<h2>"),
        "no compacted title heading in the body: {}",
        stored.content
    );
    assert_eq!(
        stored.external_url.as_deref(),
        Some("https://remote.example/pictrs/image/eOtYb9iEiB.png")
    );
}

/// An `Update(Page)` re-hoists the edited title alongside the body.
#[sqlx::test(migrations = "../db/migrations")]
async fn lemmy_page_update_edits_title(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let create = fixture(include_str!("fixtures/lemmy/create_page.json"));
    let update = fixture(include_str!("fixtures/lemmy/update_page.json"));

    let app = test_app_with(pool.clone(), stub);
    assert_eq!(
        post_signed(app.clone(), &create, &bob).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(post_signed(app, &update, &bob).await, StatusCode::ACCEPTED);
    let stored = stored_object(&pool, &create).await;
    assert_eq!(stored.title.as_deref(), Some("Post title (edited)"));
    assert!(
        stored.content.contains("The body was edited too"),
        "edited body applies: {}",
        stored.content
    );
    assert!(stored.edited_at.is_some(), "the edit is recorded");
}

/// `Discourse` titles plain `Note`s: `name` on a top-level Note is a real
/// title (hoisted), not ignored — and the type stays a Note.
#[sqlx::test(migrations = "../db/migrations")]
async fn discourse_titled_note_hoists_title(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let activity = fixture(include_str!("fixtures/discourse/create_note_titled.json"));

    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub), &activity, &bob).await,
        StatusCode::ACCEPTED
    );
    let stored = stored_object(&pool, &activity).await;
    assert_eq!(stored.title.as_deref(), Some("Our next meeting"));
    assert_eq!(stored.object_type, None, "a titled Note is still a Note");
    assert!(stored.content.contains("Last Meeting"));
}

/// `WriteFreely` bakes a titled Note's title into `content` as `<h1>…</h1>`
/// (its Mastodon workaround) *and* sets `name`; the duplicate heading must
/// be stripped so the title doesn't render twice.
#[sqlx::test(migrations = "../db/migrations")]
async fn writefreely_titled_note_strips_duplicate_heading(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let activity = fixture(include_str!("fixtures/writefreely/create_note_titled.json"));

    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub), &activity, &bob).await,
        StatusCode::ACCEPTED
    );
    let stored = stored_object(&pool, &activity).await;
    assert_eq!(stored.title.as_deref(), Some("One-paragraph thoughts"));
    assert!(
        stored.content.contains("federates as a Note"),
        "the body paragraph stays: {}",
        stored.content
    );
    assert!(
        !stored.content.contains("One-paragraph thoughts"),
        "the duplicated title heading is stripped: {}",
        stored.content
    );
}

/// Our own long-form emission, ingested. The wire shape carries the headline
/// twice on purpose — as `name` and as a leading `<h1>` — so this pins the round
/// trip: a Plamenu instance receiving a Plamenu article stores one title and one
/// copy of the body, exactly as it does for `WriteFreely`'s identical workaround.
#[sqlx::test(migrations = "../db/migrations")]
async fn our_own_article_shape_round_trips(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    // Built by the same function that federates ours, so the shape cannot drift
    // from what we actually send.
    let object = plamenu_ap::activity::note_object(&plamenu_ap::activity::NoteParams {
        kind: plamenu_ap::activity::PostKind::Article,
        domain: "remote.example",
        username: "bob",
        actor_id: None,
        status_id: 991,
        content_html: "<p>The body, in full.</p>",
        source: None,
        published: "2026-07-26T10:00:00Z",
        updated: None,
        visibility: "public",
        summary: None,
        sensitive: false,
        language: Some("en"),
        in_reply_to_uri: None,
        attachments: &[],
        tag: &[],
        mentioned_uris: &[],
        quote: None,
        quote_approval_policy: plamenu_ap::quote_policy::AUTOMATIC_PUBLIC,
        poll: None,
        self_reply_ids: &[],
        favourites_count: 0,
        reblogs_count: 0,
        title: Some("A headline of our own"),
        external_url: None,
        group_uri: None,
        context: None,
        context_history: None,
        event: None,
    });
    let activity = json!({
        "id": "https://remote.example/users/bob/statuses/991/activity",
        "type": "Create",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": object,
    });

    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub), &activity, &bob).await,
        StatusCode::ACCEPTED
    );
    let stored = stored_object(&pool, &activity).await;
    assert_eq!(stored.object_type.as_deref(), Some("Article"));
    assert_eq!(stored.title.as_deref(), Some("A headline of our own"));
    assert!(
        stored.content.contains("The body, in full."),
        "the body survives: {}",
        stored.content
    );
    assert!(
        !stored.content.contains("A headline of our own"),
        "the baked heading is stripped rather than rendered twice: {}",
        stored.content
    );
}

/// A `WordPress` `Article` arrives with the full, never-truncated body and its
/// image gallery; the `summary` excerpt must NOT become a content warning.
#[sqlx::test(migrations = "../db/migrations")]
async fn wordpress_article_keeps_full_body_and_media(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let activity = fixture(include_str!("fixtures/wordpress/create_article.json"));

    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub), &activity, &bob).await,
        StatusCode::ACCEPTED
    );
    let stored = stored_object(&pool, &activity).await;
    assert_eq!(
        stored.title.as_deref(),
        Some("Pimping my board games with a 3D printer")
    );
    assert_eq!(stored.object_type.as_deref(), Some("Article"));
    assert!(
        stored.content.contains("final paragraph proves"),
        "the whole body is stored: {}",
        stored.content
    );
    assert_eq!(
        stored.spoiler_text, "",
        "a non-sensitive excerpt is not a CW"
    );
    assert!(!stored.sensitive);
    let attached = stored_media(&pool, stored.id).await;
    assert_eq!(attached.len(), 4, "all four images stored");
}

/// AP11: `attachment` and inline `<img>` are independent media channels. A
/// shared URL stores/downloads once, an inline-only image is still exposed to
/// Mastodon API clients, alt text survives, and the built-in client renders
/// inline placements without duplicating them in its attachment gallery.
#[sqlx::test(migrations = "../db/migrations")]
async fn article_merges_inline_images_and_attachments_without_duplicates(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let activity = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://remote.example/activities/ap11",
        "type": "Create",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": "https://remote.example/articles/ap11",
            "type": "Article",
            "attributedTo": bob.actor.id,
            "name": "Mixed media",
            "content": "<p>Hero <img src=\"https://media.example/hero.jpg\" alt=\"Inline hero\"></p><p>Flow <img src=\"https://media.example/diagram.png\" alt=\"Flow from A to B\"></p>",
            "published": "2026-09-22T08:00:00Z",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "attachment": [
                {"type": "Image", "mediaType": "image/jpeg", "url": "https://media.example/hero.jpg", "name": "Hero description"},
                {"type": "Image", "mediaType": "image/jpeg", "url": "https://media.example/appendix.jpg", "name": "Appendix"}
            ]
        }
    });
    let app = test_app_with(pool.clone(), stub);
    assert_eq!(
        post_signed(app.clone(), &activity, &bob).await,
        StatusCode::ACCEPTED
    );

    let stored = stored_object(&pool, &activity).await;
    let attached = stored_media(&pool, stored.id).await;
    assert_eq!(attached.len(), 3, "two structured plus one inline-only");
    let hero = attached
        .iter()
        .find(|item| item.remote_url.as_deref() == Some("https://media.example/hero.jpg"))
        .unwrap();
    let diagram = attached
        .iter()
        .find(|item| item.remote_url.as_deref() == Some("https://media.example/diagram.png"))
        .unwrap();
    let appendix = attached
        .iter()
        .find(|item| item.remote_url.as_deref() == Some("https://media.example/appendix.jpg"))
        .unwrap();
    assert_eq!(hero.description.as_deref(), Some("Hero description"));
    assert_eq!(diagram.description.as_deref(), Some("Flow from A to B"));
    assert!(!stored.content.contains("https://media.example/"));
    assert!(
        stored
            .content
            .contains(&format!(r#"data-media-id="{}""#, hero.id))
    );
    assert!(
        stored
            .content
            .contains(&format!(r#"data-media-id="{}""#, diagram.id))
    );
    let jobs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM media_processing_jobs j JOIN media_attachments m ON m.id=j.media_id WHERE m.status_id=$1",
    )
    .bind(stored.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(jobs, 3, "the shared inline/attachment URL queues once");

    let mut unchanged = activity.clone();
    unchanged["id"] = json!("https://remote.example/activities/ap11-update");
    unchanged["type"] = json!("Update");
    unchanged["object"]["updated"] = json!("2026-09-22T09:00:00Z");
    assert_eq!(
        post_signed(app.clone(), &unchanged, &bob).await,
        StatusCode::ACCEPTED
    );
    let unchanged_stored = stored_object(&pool, &activity).await;
    assert_eq!(
        unchanged_stored.edited_at, None,
        "an identical Update is a no-op"
    );

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/statuses/{}", stored.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let entity: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(entity["media_attachments"].as_array().unwrap().len(), 3);
    assert_eq!(
        entity["media_attachments"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| {
                item["id"].as_str().and_then(|id| id.parse::<i64>().ok()) == Some(diagram.id)
            })
            .unwrap()["description"],
        "Flow from A to B"
    );

    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/@bob@remote.example/{}", stored.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = String::from_utf8(
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(body.contains(&format!(r#"data-media-id="{}""#, diagram.id)));
    assert!(body.contains(&format!("/media/proxy/attachment/{}", appendix.id)));
    assert_eq!(
        body.matches(&format!("/media/proxy/attachment/{}", diagram.id))
            .count(),
        1,
        "inline media is not repeated in the built-in gallery"
    );
}

/// `WordPress` swaps the excerpt for the CW text and flags `sensitive: true`
/// when a post carries a content warning — then (and only then) `summary`
/// is a CW on a converted type.
#[sqlx::test(migrations = "../db/migrations")]
async fn wordpress_sensitive_article_summary_is_cw(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let activity = fixture(include_str!("fixtures/wordpress/create_article_cw.json"));

    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub), &activity, &bob).await,
        StatusCode::ACCEPTED
    );
    let stored = stored_object(&pool, &activity).await;
    assert!(stored.sensitive);
    assert_eq!(stored.spoiler_text, "Money talk");
}

/// A `Mobilizon` `Event` stores the typed sidecar (start/end, location name,
/// timezone, ical status) at the DB layer; the machine-generated `summary` (a
/// date-and-place line) must be neither a CW nor the body; the banner
/// `Document` stays media. The Status *entity* then carries the extension
/// fields — `title`, `object_type`, `external_url` and the typed `event` — so
/// clients (our web UI included) can render them.
#[sqlx::test(migrations = "../db/migrations")]
async fn status_entity_carries_content_universe_fields(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let activity = fixture(include_str!("fixtures/mobilizon/create_event.json"));

    let app = test_app_with(pool.clone(), stub);
    assert_eq!(
        post_signed(app.clone(), &activity, &bob).await,
        StatusCode::ACCEPTED
    );
    let stored = stored_object(&pool, &activity).await;

    // The stored row carries the typed Event: real title/type/body, and the
    // machine-generated date/place summary is neither a CW nor the body.
    assert_eq!(
        stored.title.as_deref(),
        Some("Caledonian scottish dance class")
    );
    assert_eq!(stored.object_type.as_deref(), Some("Event"));
    assert!(stored.content.contains("posh ceilidh"));
    assert_eq!(
        stored.spoiler_text, "",
        "the generated date/place summary is not a CW"
    );

    let event = status_event::for_statuses(&pool, &[stored.id])
        .await
        .unwrap()
        .remove(&stored.id)
        .expect("event sidecar stored");
    let start = OffsetDateTime::parse("2026-12-11T19:00:00+00:00", &Rfc3339).unwrap();
    let end = OffsetDateTime::parse("2026-12-11T21:00:00+00:00", &Rfc3339).unwrap();
    assert_eq!(event.start_time, Some(start));
    assert_eq!(event.end_time, Some(end));
    assert_eq!(event.location_name.as_deref(), Some("The Kenn Centre"));
    assert_eq!(event.timezone.as_deref(), Some("Europe/London"));
    assert_eq!(event.event_status.as_deref(), Some("CONFIRMED"));
    // E1: the rest of the calendar facts, including the structured `Place` the
    // The earlier pass flattened away to just its name.
    assert_eq!(event.join_mode.as_deref(), Some("free"));
    assert_eq!(event.category.as_deref(), Some("SPORTS"));
    assert_eq!(event.is_online, Some(false));
    assert_eq!(event.comments_enabled, Some(true));
    assert_eq!(event.anonymous_participation, Some(true));
    assert_eq!(
        event.location_url.as_deref(),
        Some("https://remote.example/address/e4c95383-15ac-4cc7-adf6-723d74ee2ccc")
    );
    assert_eq!(event.location_street.as_deref(), Some("Devon Expressway"));
    assert_eq!(event.location_locality.as_deref(), Some("Teignbridge"));
    assert_eq!(event.location_region.as_deref(), Some("England"));
    assert_eq!(event.location_country.as_deref(), Some("United Kingdom"));
    assert_eq!(event.location_postal_code.as_deref(), Some("EX6 7TW"));
    // This capture names no capacity at all, so the pair stays unknown rather
    // than defaulting to zero — a zero capacity would render as "full".
    assert_eq!(event.max_attendees, None);
    assert_eq!(event.remaining_attendees, None);
    assert!(!event.is_full(), "unknown capacity is never full");
    assert_eq!(
        plamenu::events::rsvp_refusal(&event, false),
        None,
        "a free event can be joined"
    );

    let attached = stored_media(&pool, stored.id).await;
    assert_eq!(attached.len(), 1, "the banner Document is media");

    let request = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/statuses/{}", stored.id))
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let entity: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(entity["title"], "Caledonian scottish dance class");
    assert_eq!(entity["object_type"], "Event");
    assert_eq!(entity["external_url"], Value::Null);
    assert_eq!(entity["event"]["location"], "The Kenn Centre");
    assert_eq!(entity["event"]["status"], "CONFIRMED");
    assert_eq!(entity["event"]["timezone"], "Europe/London");
    assert_eq!(entity["event"]["start_time"], "2026-12-11T19:00:00Z");
    assert_eq!(entity["event"]["join_mode"], "free");
    assert_eq!(entity["event"]["category"], "SPORTS");
    assert_eq!(entity["event"]["is_online"], false);
    assert_eq!(entity["event"]["location_street"], "Devon Expressway");
    assert_eq!(entity["event"]["location_postal_code"], "EX6 7TW");
    // A fact the origin never sent is null, never a default — a client that
    // read a missing `join_mode` as `free` would offer an RSVP that no origin
    // is going to answer.
    assert_eq!(entity["event"]["max_attendees"], Value::Null);
    assert_eq!(entity["event"]["external_participation_url"], Value::Null);
}

/// The live-captured **group-attributed** Mobilizon `Event` (the only shape
/// that reaches a follower at all) ingests with the capacity/participation
/// facts the older corpus capture doesn't carry, and with the `Place` id.
#[sqlx::test(migrations = "../db/migrations")]
async fn group_event_capture_ingests_participation_facts(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let activity = fixture(include_str!("fixtures/mobilizon/create_event_group.json"));

    let app = test_app_with(pool.clone(), stub);
    assert_eq!(
        post_signed(app.clone(), &activity, &bob).await,
        StatusCode::ACCEPTED
    );
    let stored = stored_object(&pool, &activity).await;
    assert_eq!(stored.object_type.as_deref(), Some("Event"));
    assert_eq!(stored.title.as_deref(), Some("Plamenu interop meetup"));
    // The organizing Person owns the event, not the group in `attributedTo`.
    // The group is already the announcer; crediting it for the post too would
    // leave the human organizer nowhere on the card.
    let organizer = account::find_by_uri(&pool, &bob.actor.id)
        .await
        .unwrap()
        .expect("the organizing Person is stored");
    assert_eq!(
        stored.account_id, organizer.id,
        "an Event belongs to its organizer (`actor`), not to its group"
    );

    let event = status_event::find(&pool, stored.id)
        .await
        .unwrap()
        .expect("event sidecar stored");
    assert_eq!(event.join_mode.as_deref(), Some("free"));
    assert_eq!(event.participant_count, Some(0));
    assert_eq!(event.category.as_deref(), Some("MEETING"));
    assert_eq!(event.timezone.as_deref(), Some("Etc/UTC"));
    assert_eq!(event.is_online, Some(false));
    assert_eq!(event.comments_enabled, Some(false));
    // Nulls on the wire must stay unknown, not become false/zero.
    assert_eq!(event.anonymous_participation, None);
    assert_eq!(event.external_participation_url, None);
    assert_eq!(event.max_attendees, None);
    assert_eq!(event.location_name.as_deref(), Some("Community Hall"));
    assert_eq!(
        event.location_url.as_deref(),
        Some("https://remote.example/address/f53f92c7-f942-4f74-b49d-5aacfdfb4c1b")
    );
    assert_eq!(event.location_street.as_deref(), Some("Trg 1"));
    assert_eq!(event.location_locality.as_deref(), Some("Ljubljana"));
    assert_eq!(event.location_country.as_deref(), Some("Slovenia"));
    // `addressRegion` and `postalCode` are explicit nulls in this capture.
    assert_eq!(event.location_region, None);
    assert_eq!(event.location_postal_code, None);
}

/// An unpublished `Event` (`draft: true`) is never ingested — by whatever route
/// it arrives. Mobilizon does not federate drafts, but a forwarded, relayed or
/// laxer-dialect copy could still reach our inbox, and republishing an
/// organizer's unfinished draft to our own followers is a disclosure we cannot
/// take back.
#[sqlx::test(migrations = "../db/migrations")]
async fn draft_event_is_refused(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let mut activity = fixture(include_str!("fixtures/mobilizon/create_event_group.json"));
    activity["object"]["draft"] = Value::Bool(true);

    let app = test_app_with(pool.clone(), stub);
    // Accepted at the transport layer (a well-signed activity we simply decline
    // to act on), but nothing is stored.
    assert_eq!(
        post_signed(app.clone(), &activity, &bob).await,
        StatusCode::ACCEPTED
    );
    let uri = activity["object"]["id"].as_str().unwrap();
    assert!(
        status::find_by_uri(&pool, uri).await.unwrap().is_none(),
        "a draft event must not be stored"
    );
}

/// FEP-044f audit: a remote-to-remote quote carrying the `quote` property
/// and a fetchable `QuoteAuthorization` stamp (issued by the quoted author,
/// hosted on their origin) links as an *accepted, non-legacy* quote — the
/// full consent handshake, claimed by behavior.
#[sqlx::test(migrations = "../db/migrations")]
async fn fep_044f_stamped_quote_is_accepted(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let carol = RemoteUser::new("quoted.example", "carol");
    let stub = StubFederation::with_actors([bob.actor.clone(), carol.actor.clone()]);
    {
        let mut objects = stub.objects.lock().unwrap();
        objects.insert(
            "https://quoted.example/objects/original".to_owned(),
            serde_json::json!({
                "id": "https://quoted.example/objects/original",
                "type": "Note",
                "attributedTo": carol.actor.id,
                "content": "<p>carol's quotable post</p>",
                "published": "2026-07-01T12:00:00Z",
                "to": ["https://www.w3.org/ns/activitystreams#Public"],
            }),
        );
        objects.insert(
            "https://quoted.example/stamps/1".to_owned(),
            serde_json::json!({
                "id": "https://quoted.example/stamps/1",
                "type": "QuoteAuthorization",
                "attributedTo": carol.actor.id,
                "interactingObject": "https://remote.example/objects/044f-quote",
                "interactionTarget": "https://quoted.example/objects/original",
            }),
        );
    }
    let activity = fixture(include_str!("fixtures/as2/create_note_044f_quote.json"));

    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub), &activity, &bob).await,
        StatusCode::ACCEPTED
    );
    let quoting_uri = activity["object"]["id"].as_str().unwrap();
    let linked = plamenu_db::quote::find_by_status_uri(&pool, quoting_uri)
        .await
        .unwrap()
        .expect("the stamped quote must link");
    assert_eq!(linked.state, "accepted", "the stamp verifies");
    assert!(!linked.legacy, "`quote` property present => not legacy");
    assert!(linked.quoted_status_id.is_some(), "quoted post fetched");
}

/// FEP-03c1 audit: actors without any `acct:` identity (no
/// `preferredUsername`, webfinger optional) import under their canonical id.
/// Two such actors on one domain — here both with the same trailing id
/// segment — must not conflate through acct uniqueness.
#[sqlx::test(migrations = "../db/migrations")]
async fn fep_03c1_actors_without_acct_import_distinctly(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let mut ghost_channel = RemoteUser::new("remote.example", "actor");
    ghost_channel.actor.id = "https://remote.example/channels/1/actor".to_owned();
    ghost_channel.actor.preferred_username = String::new();
    let mut ghost_board = RemoteUser::new("remote.example", "actor");
    ghost_board.actor.id = "https://remote.example/boards/1/actor".to_owned();
    ghost_board.actor.preferred_username = String::new();
    let stub = StubFederation::with_actors([
        bob.actor.clone(),
        ghost_channel.actor.clone(),
        ghost_board.actor.clone(),
    ]);
    let activity = fixture(include_str!(
        "fixtures/as2/create_note_mentions_no_acct.json"
    ));

    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub), &activity, &bob).await,
        StatusCode::ACCEPTED
    );
    let channel = account::find_by_uri(&pool, "https://remote.example/channels/1/actor")
        .await
        .unwrap()
        .expect("acct-less actor imports");
    let board = account::find_by_uri(&pool, "https://remote.example/boards/1/actor")
        .await
        .unwrap()
        .expect("second acct-less actor imports");
    assert_ne!(channel.id, board.id);
    assert_ne!(
        channel.username, board.username,
        "same id segment must not collide in the (username, domain) slot"
    );
    assert!(
        channel.username.starts_with("actor-"),
        "readable id-derived handle: {}",
        channel.username
    );
}

/// The public web page of a non-microblog status renders the new blocks: the title
/// heading, the event facts box, and (for a link post) the external-link
/// pill.
#[sqlx::test(migrations = "../db/migrations")]
async fn web_page_renders_title_event_and_external_link(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let app = test_app_with(pool.clone(), stub);
    let event = fixture(include_str!("fixtures/mobilizon/create_event.json"));
    let page = fixture(include_str!("fixtures/lemmy/create_page.json"));
    assert_eq!(
        post_signed(app.clone(), &event, &bob).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        post_signed(app.clone(), &page, &bob).await,
        StatusCode::ACCEPTED
    );

    let fetch_page = |status_id: i64| {
        let app = app.clone();
        async move {
            let request = Request::builder()
                .method("GET")
                .uri(format!("/@bob@remote.example/{status_id}"))
                .body(Body::empty())
                .unwrap();
            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            String::from_utf8(bytes.to_vec()).unwrap()
        }
    };

    let stored_event = stored_object(&pool, &event).await;
    let html = fetch_page(stored_event.id).await;
    assert!(
        html.contains("status__title") && html.contains("Caledonian scottish dance class"),
        "event page shows the title heading"
    );
    assert!(
        html.contains("status__event") && html.contains("The Kenn Centre"),
        "event page shows the facts box"
    );

    let stored_page = stored_object(&pool, &page).await;
    let html = fetch_page(stored_page.id).await;
    assert!(
        html.contains("status__title") && html.contains("Post title"),
        "page shows the title heading"
    );
    assert!(
        html.contains("status__external-link"),
        "link post shows the external-link pill"
    );
}

/// A lotide `Page` may be title-only — no `content` at all, `summary`
/// duplicating the title. It must store as a titled, empty-bodied post
/// (never a fully empty status), with `summary` not becoming a CW.
#[sqlx::test(migrations = "../db/migrations")]
async fn lotide_title_only_page_stores_titled(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let activity = fixture(include_str!("fixtures/lotide/create_page_title_only.json"));

    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub), &activity, &bob).await,
        StatusCode::ACCEPTED
    );
    let stored = stored_object(&pool, &activity).await;
    assert_eq!(
        stored.title.as_deref(),
        Some("What's Dylan Grillin'? (reupload)")
    );
    assert_eq!(stored.content, "", "no content is a valid link post");
    assert_eq!(
        stored.spoiler_text, "",
        "the duplicated summary is not a CW"
    );
    assert_eq!(
        stored.url.as_deref(),
        Some("https://www.youtube.com/watch?v=ZI4LGTXscR4"),
        "the off-site `url` is the post's link"
    );
    assert_eq!(
        stored.visibility, "unlisted",
        "Public in `cc` only is unlisted"
    );
}

/// The `PeerTube` `Video`'s playable file is mined out of the `url` Link tree
/// (nested inside the HLS playlist's `tag`): the largest mp4 becomes media
/// with the preview image as its poster.
#[sqlx::test(migrations = "../db/migrations")]
async fn peertube_video_stores_playable_media(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let activity = fixture(include_str!("fixtures/peertube/create_video.json"));

    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub), &activity, &bob).await,
        StatusCode::ACCEPTED
    );
    let stored = stored_object(&pool, &activity).await;
    // Attributed to an *array* of objects (Person + Group): ingest resolves
    // the Person entry as the author instead of bailing on the array shape.
    let author = account::find_by_uri(&pool, "https://remote.example/users/bob")
        .await
        .unwrap()
        .expect("author stored");
    assert_eq!(
        stored.account_id, author.id,
        "the Person entry is the author"
    );
    assert_eq!(stored.language.as_deref(), Some("en"));
    assert_eq!(stored.visibility, "public");
    assert_eq!(
        stored.title.as_deref(),
        Some("Mesa, Wayland & X.org in trouble: Linux & Open Source News")
    );
    assert_eq!(stored.object_type.as_deref(), Some("Video"));
    // `content` is markdown (`mediaType: text/markdown`): rendered, not
    // HTML-sanitized as-is — paragraphs split on blank lines, single
    // newlines kept as breaks (PeerTube's own `breaks: true` rendering).
    assert!(
        stored.content.starts_with("<p>Grab a brand new laptop"),
        "markdown renders paragraphs: {}",
        stored.content
    );
    assert!(
        stored.content.contains("<br"),
        "single newlines stay visible line breaks: {}",
        stored.content
    );
    // Bare URLs in the markdown body are linkified, not left as dead text.
    assert!(
        stored
            .content
            .contains(r#"<a href="https://example.com/laptops""#),
        "bare URL becomes an anchor: {}",
        stored.content
    );
    let attached = stored_media(&pool, stored.id).await;
    assert_eq!(attached.len(), 1, "exactly the primary file: {attached:?}");
    assert_eq!(
        attached[0].remote_url.as_deref(),
        Some(
            "https://remote.example/static/streaming-playlists/hls/e7946124/e7946124-480-fragmented.mp4"
        ),
        "the universally-light 480p rung is the single progressive url (not the tallest)"
    );
    assert_eq!(attached[0].content_type, "video/mp4");
    assert_eq!(attached[0].width, Some(854));
    assert_eq!(attached[0].height, Some(480));
    assert_eq!(
        attached[0].thumbnail_remote_url.as_deref(),
        Some(
            "https://remote.example/lazy-static/previews/ef6088ee-c83a-4fcf-8be2-58db95ca5135.jpg"
        ),
        "the largest icon is the poster"
    );
    // HLS video (and a deliberately-downscaled multi-rendition pick) always
    // defers: this video is play-triggered, so ingest must NOT have queued a
    // download even though the chosen 480p rung is now under the eager cap.
    assert!(
        attached[0].download_on_demand,
        "HLS / downscaled multi-rendition video defers to the on-demand lane"
    );
    assert_eq!(
        attached[0].duration,
        Some(1145.0),
        "the object's xsd:duration is stored up front"
    );
    // The whole rendition ladder + the HLS master playlist URL are retained
    // (they drive the built-in quality selector and the caching HLS proxy),
    // not collapsed to the single `remote_url` fallback.
    assert_eq!(
        media::hls_master_url(&pool, attached[0].id)
            .await
            .unwrap()
            .as_deref(),
        Some("https://remote.example/static/streaming-playlists/hls/e7946124/master.m3u8"),
    );
    let ladder = media::renditions_for(&pool, &[attached[0].id])
        .await
        .unwrap();
    let rungs = ladder.get(&attached[0].id).expect("HLS ladder stored");
    assert_eq!(rungs.len(), 2, "both renditions retained: {rungs:?}");
    assert_eq!((rungs[0].height, rungs[0].width), (1080, Some(1920)));
    assert_eq!(rungs[0].frame_rate, Some(60));
    assert!(!rungs[0].is_audio);
    assert_eq!(rungs[1].height, 480, "ordered tallest-first");
    assert!(
        media::claim_due_processing(&pool, 10)
            .await
            .unwrap()
            .is_empty()
            && media::claim_due_on_demand(&pool, 10)
                .await
                .unwrap()
                .is_empty(),
        "no eager download job for on-demand video"
    );
}

/// A `PeerTube` re-transcode regenerates the HLS master under a new filename
/// and sends an `Update`. Even though the chosen rendition (what the attachment
/// diff compares on) is unchanged, the differing master must force a re-ingest
/// — otherwise the caching proxy keeps serving a stale, now-404 master forever.
#[sqlx::test(migrations = "../db/migrations")]
async fn peertube_retranscode_update_refreshes_the_hls_master(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let create = fixture(include_str!("fixtures/peertube/create_video.json"));

    let app = test_app_with(pool.clone(), stub);
    assert_eq!(
        post_signed(app.clone(), &create, &bob).await,
        StatusCode::ACCEPTED
    );
    let stored = stored_object(&pool, &create).await;
    let before = stored_media(&pool, stored.id).await;
    assert!(
        media::hls_master_url(&pool, before[0].id)
            .await
            .unwrap()
            .unwrap()
            .ends_with("/e7946124/master.m3u8")
    );

    // Same object, but the HLS master href now points at a regenerated file
    // (new UUID) — the only change, as a real re-transcode produces.
    let mut object = create["object"].clone();
    for entry in object["url"].as_array_mut().unwrap() {
        if entry.get("mediaType").and_then(serde_json::Value::as_str)
            == Some("application/x-mpegURL")
        {
            let fresh = entry["href"]
                .as_str()
                .unwrap()
                .replace("master.m3u8", "v2-master.m3u8");
            entry["href"] = serde_json::json!(fresh);
        }
    }
    object["updated"] = serde_json::json!("2026-07-13T12:00:00Z");
    let update = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://remote.example/videos/watch/e7946124-7b72-4ad7-9d22-844a84bb2de1#update-1",
        "type": "Update",
        "actor": "https://remote.example/users/bob",
        "object": object,
    });
    assert_eq!(post_signed(app, &update, &bob).await, StatusCode::ACCEPTED);

    let after = stored_media(&pool, stored.id).await;
    assert_eq!(
        media::hls_master_url(&pool, after[0].id)
            .await
            .unwrap()
            .as_deref(),
        Some("https://remote.example/static/streaming-playlists/hls/e7946124/v2-master.m3u8"),
        "the Update refreshed the stale HLS master instead of skipping the edit",
    );
}

/// `PeerTube` 6.x separated-audio HLS: every video rendition is silent and a
/// `height: 0` file Link carries the audio. Ingest must pair the chosen
/// rendition with that audio stream (muxed at cache time) and defer the
/// download to first play.
#[sqlx::test(migrations = "../db/migrations")]
async fn peertube_separated_audio_pairs_streams_and_defers(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let activity = fixture(include_str!(
        "fixtures/peertube/create_video_separated_audio.json"
    ));

    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub), &activity, &bob).await,
        StatusCode::ACCEPTED
    );
    let stored = stored_object(&pool, &activity).await;
    let attached = stored_media(&pool, stored.id).await;
    assert_eq!(attached.len(), 1, "exactly the primary file: {attached:?}");
    assert_eq!(
        attached[0].remote_url.as_deref(),
        Some(
            "https://remote.example/static/streaming-playlists/hls/a1b2c3d4/a1b2c3d4-480-fragmented.mp4"
        ),
        "the 480p sweet-spot rung wins for the progressive url; the height-0 audio stream is not a rendition"
    );
    assert_eq!(
        attached[0].remote_audio_url.as_deref(),
        Some(
            "https://remote.example/static/streaming-playlists/hls/a1b2c3d4/a1b2c3d4-0-fragmented.mp4"
        ),
        "the audio-only companion is remembered for the cache-time mux"
    );
    assert!(attached[0].download_on_demand);
    assert_eq!(attached[0].processing, "complete", "born serving origin");
    assert!(attached[0].file_name.is_none());
    assert_eq!(attached[0].duration, Some(10185.0));
    // The ladder retains both video renditions AND the separated audio track;
    // the master playlist URL is kept for the HLS proxy.
    assert_eq!(
        media::hls_master_url(&pool, attached[0].id)
            .await
            .unwrap()
            .as_deref(),
        Some("https://remote.example/static/streaming-playlists/hls/a1b2c3d4/master.m3u8"),
    );
    let ladder = media::renditions_for(&pool, &[attached[0].id])
        .await
        .unwrap();
    let rungs = ladder.get(&attached[0].id).expect("HLS ladder stored");
    assert_eq!(
        rungs.len(),
        3,
        "two video renditions + the audio track: {rungs:?}"
    );
    assert_eq!((rungs[0].height, rungs[1].height), (720, 480));
    assert!(!rungs[0].is_audio && !rungs[1].is_audio);
    assert!(
        rungs[2].is_audio && rungs[2].height == 0,
        "the height-0 stream is flagged audio-only and sorts last: {:?}",
        rungs[2]
    );
    assert!(
        media::claim_due_processing(&pool, 10)
            .await
            .unwrap()
            .is_empty()
            && media::claim_due_on_demand(&pool, 10)
                .await
                .unwrap()
                .is_empty(),
        "nothing downloads until someone presses play"
    );
    // Even a legacy play-trigger call cannot queue HLS into the whole-file
    // A/V lane; playback is handled by the sparse/virtual gateway.
    media::enqueue_on_demand_download(&pool, attached[0].id)
        .await
        .unwrap();
    assert!(
        media::claim_due_processing(&pool, 10)
            .await
            .unwrap()
            .is_empty(),
        "the general lane never claims on-demand A/V"
    );
    assert!(
        media::claim_due_on_demand(&pool, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        media::find_by_ids(&pool, &[attached[0].id]).await.unwrap()[0].processing,
        "complete"
    );
}
