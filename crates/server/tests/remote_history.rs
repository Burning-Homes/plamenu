//! Demand-driven remote outbox hydration: bounded fetches, cold-ingest
//! isolation, promotion, and hostile cursor handling.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{
    RemoteUser, StubFederation, TEST_DOMAIN, create_local_account, test_app_with, test_state_with,
};
use plamenu::auth::{generate_secret, hash_password, hash_secret};
use plamenu::ingest::ingest_remote_note_delivery;
use plamenu::{build_router, entities, remote_history};
use plamenu_db::account::{self, Account, RemoteAccountData};
use plamenu_db::instance_policy::{self, NewDomainBlock};
use plamenu_db::remote_history::{self as history_db, EnqueueOutcome, JobKind};
use plamenu_db::{PgPool, oauth, status, user};
use serde_json::{Value, json};
use tower::ServiceExt;

const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

fn sample_png() -> Vec<u8> {
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        64,
        48,
        image::Rgb([80, 140, 220]),
    ))
    .write_to(
        &mut std::io::Cursor::new(&mut bytes),
        image::ImageFormat::Png,
    )
    .unwrap();
    bytes
}

async fn remote_account(pool: &PgPool, username: &str) -> Account {
    let uri = format!("https://remote.example/users/{username}");
    let account = account::upsert_remote(
        pool,
        RemoteAccountData {
            username,
            domain: "remote.example",
            uri: &uri,
            display_name: username,
            note: "",
            inbox_url: &format!("{uri}/inbox"),
            shared_inbox_url: "",
            public_key_pem: "pub",
            public_key_id: &format!("{uri}#main-key"),
            avatar_remote_url: None,
            header_remote_url: None,
            avatar_description: "",
            header_description: "",
            created_at: None,
            fields: Vec::new(),
            featured_collection_url: None,
            locked: false,
            also_known_as: &[],
            moved_to_uri: None,
            url: None,
            discoverable: false,
            feature_approval_policy: 0,
            is_bot: false,
            indexable: false,
            show_media: None,
            show_media_replies: None,
            show_featured: None,
            memorial: false,
            actor_type: Some("Person"),
        },
    )
    .await
    .unwrap();
    let outbox = format!("{uri}/outbox");
    account::set_collection_urls(pool, account.id, "", "", &outbox)
        .await
        .unwrap();
    history_db::set_actor_metadata(pool, account.id, Some(&outbox), None)
        .await
        .unwrap();
    account
}

async fn local_user_with_token(pool: &PgPool, username: &str, scopes: &str) -> (Account, String) {
    let account = create_local_account(pool, username, username).await;
    let user = user::create(
        pool,
        account.id,
        Some(&format!("{username}@plamenu.test")),
        &hash_password("pw").unwrap(),
    )
    .await
    .unwrap();
    let app = oauth::create_app(
        pool,
        oauth::NewApp {
            name: "remote-history-tests",
            website: None,
            client_id: &generate_secret(),
            client_secret_hash: &hash_secret(&generate_secret()),
            redirect_uris: &["urn:ietf:wg:oauth:2.0:oob".to_owned()],
            scopes,
        },
    )
    .await
    .unwrap();
    let token = generate_secret();
    oauth::create_token(pool, &hash_secret(&token), app.id, Some(user.id), scopes)
        .await
        .unwrap();
    (account, token)
}

fn note(author: &Account, n: usize, local_mention: Option<&str>) -> Value {
    let actor = author.uri.as_deref().unwrap();
    let mut value = json!({
        "id": format!("{actor}/statuses/{n}"),
        "type": "Note",
        "attributedTo": actor,
        "content": format!("<p>history item {n}</p>"),
        "published": "2026-08-01T12:00:00Z",
        "to": [PUBLIC],
    });
    if n == 0 {
        value["attachment"] = json!([{
            "type": "Image",
            "mediaType": "image/jpeg",
            "url": "https://remote.example/media/history-zero.jpg",
        }]);
    }
    if let Some(href) = local_mention {
        value["content"] = json!(format!("<p>history item {n} :historycat:</p>"));
        value["tag"] = json!([
            {"type": "Mention", "href": href, "name": "@alice"},
            {"type": "Hashtag", "name": "#hydrated"},
            {
                "id": "https://remote.example/emojis/historycat",
                "type": "Emoji",
                "name": ":historycat:",
                "icon": {
                    "type": "Image",
                    "mediaType": "image/png",
                    "url": "https://remote.example/media/historycat.png"
                }
            },
        ]);
    }
    value
}

#[sqlx::test(migrations = "../db/migrations")]
async fn hydration_caps_a_page_and_promotes_once_without_cold_side_effects(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = remote_account(&pool, "bob").await;
    let bob_uri = bob.uri.as_deref().unwrap();
    let alice_uri = format!("https://{TEST_DOMAIN}/users/alice");
    let outbox = format!("{bob_uri}/outbox");
    let mut items: Vec<Value> = (0..25)
        .map(|n| note(&bob, n, (n == 0).then_some(alice_uri.as_str())))
        .collect();
    // A private object consumes one of the bounded collection slots but is not
    // ingested and does not fail the rest of the page.
    items[1]["to"] = json!([format!("{bob_uri}/followers")]);
    let stub = Arc::new(StubFederation::default());
    stub.objects.lock().unwrap().insert(
        outbox.clone(),
        json!({
            "id": outbox,
            "type": "OrderedCollection",
            "totalItems": 25,
            "first": {
                "id": format!("{bob_uri}/outbox?page=true"),
                "type": "OrderedCollectionPage",
                "partOf": format!("{bob_uri}/outbox"),
                "next": format!("{bob_uri}/outbox?max_id=older"),
                "orderedItems": items,
            },
        }),
    );
    let state = test_state_with(pool.clone(), stub.clone());
    history_db::save_settings(&pool, true, 90, false)
        .await
        .unwrap();

    assert_eq!(
        remote_history::request(&state, &bob, JobKind::Initial, Some(alice.id))
            .await
            .unwrap(),
        EnqueueOutcome::Enqueued
    );
    assert_eq!(remote_history::run_due(&state).await, 1);

    let snapshot = history_db::snapshot(&pool, bob.id).await.unwrap().unwrap();
    assert_eq!(snapshot.state, "partial");
    assert_eq!(snapshot.items_seen, 20, "one page is capped at 20 items");
    assert_eq!(snapshot.items_accepted, 19, "private history is skipped");
    assert_eq!(snapshot.reported_total_items, Some(25));
    assert_eq!(snapshot.available_statuses, 19);
    assert!(
        snapshot
            .next_page_uri
            .as_deref()
            .unwrap()
            .contains("max_id")
    );
    assert_eq!(stub.fetches(), [format!("{bob_uri}/outbox")]);

    let cold = status::find_by_uri(&pool, &format!("{bob_uri}/statuses/0"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        status::ingest_provenance(&pool, cold.id)
            .await
            .unwrap()
            .as_deref(),
        Some("history")
    );
    let (media_id, on_demand, history_deferred): (i64, bool, bool) = sqlx::query_as(
        "SELECT id, download_on_demand, history_deferred
         FROM media_attachments WHERE status_id = $1",
    )
    .bind(cold.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!on_demand, "a cold image uses the image proxy lane");
    assert!(
        history_deferred,
        "cold media remains metadata-only at ingest"
    );
    let media_jobs: i64 =
        sqlx::query_scalar("SELECT count(*) FROM media_processing_jobs WHERE media_id = $1")
            .bind(media_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(media_jobs, 0);
    let notifications: i64 =
        sqlx::query_scalar("SELECT count(*) FROM notifications WHERE status_id = $1")
            .bind(cold.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(notifications, 0, "cold mentions never notify");
    let rendered = entities::render_status(&pool, TEST_DOMAIN, &cold, None)
        .await
        .unwrap();
    assert_eq!(rendered["mentions"][0]["acct"], "alice");
    assert_eq!(rendered["emojis"][0]["shortcode"], "historycat");
    let crawls: i64 =
        sqlx::query_scalar("SELECT count(*) FROM link_crawl_jobs WHERE status_id = $1")
            .bind(cold.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(crawls, 0, "cold content never starts preview crawling");
    let prohibited_jobs: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM delivery_jobs)
              + (SELECT count(*) FROM tag_usages)
              + (SELECT count(*) FROM reply_fetch_jobs)
              + (SELECT count(*) FROM quote_verify_jobs)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(prohibited_jobs, 0, "cold hydration created prohibited work");

    let live_object = note(&bob, 0, Some(&alice_uri));
    let first = ingest_remote_note_delivery(&state, &bob, &live_object)
        .await
        .unwrap();
    let replay = ingest_remote_note_delivery(&state, &bob, &live_object)
        .await
        .unwrap();
    assert!(first.delivery_effects);
    assert!(!replay.delivery_effects);
    assert_eq!(first.status.id, cold.id);
    assert_eq!(
        status::ingest_provenance(&pool, cold.id)
            .await
            .unwrap()
            .as_deref(),
        Some("delivery")
    );
    let on_demand: bool =
        sqlx::query_scalar("SELECT download_on_demand FROM media_attachments WHERE id = $1")
            .bind(media_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        !on_demand,
        "live delivery promotes media into normal processing"
    );
    let media_jobs: i64 =
        sqlx::query_scalar("SELECT count(*) FROM media_processing_jobs WHERE media_id = $1")
            .bind(media_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(media_jobs, 1, "promotion queues media once");
    let notifications: i64 =
        sqlx::query_scalar("SELECT count(*) FROM notifications WHERE status_id = $1")
            .bind(cold.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(notifications, 1, "promotion notifies exactly once");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn history_quotes_resolve_and_render_without_delivery_effects(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = remote_account(&pool, "bob").await;
    let bob_uri = bob.uri.as_deref().unwrap();
    let outbox = format!("{bob_uri}/outbox");
    let self_target_uri = format!("{bob_uri}/statuses/10");
    let self_quote_uri = format!("{bob_uri}/statuses/11");
    let remote_quote_uri = format!("{bob_uri}/statuses/12");
    let carol = RemoteUser::new("quoted.example", "carol");
    let quoted_uri = format!("{}/statuses/20", carol.actor.id);
    let stamp_uri = format!("{}/quote-authorizations/12", carol.actor.id);

    let self_target = note(&bob, 10, None);
    let mut self_quote = note(&bob, 11, None);
    self_quote["content"] = json!("<p>self quote</p>");
    self_quote["quote"] = json!(self_target_uri);
    let mut remote_quote = note(&bob, 12, None);
    remote_quote["content"] = json!("<p>authorized quote</p>");
    remote_quote["quote"] = json!(quoted_uri);
    remote_quote["quoteAuthorization"] = json!(stamp_uri);

    let stub = Arc::new(StubFederation::default());
    stub.actors
        .lock()
        .unwrap()
        .insert(carol.actor.id.clone(), carol.actor.clone());
    {
        let mut objects = stub.objects.lock().unwrap();
        objects.insert(
            outbox.clone(),
            json!({
                "id": outbox,
                "type": "OrderedCollection",
                "orderedItems": [self_target, self_quote, remote_quote],
            }),
        );
        objects.insert(
            quoted_uri.clone(),
            json!({
                "id": quoted_uri,
                "type": "Note",
                "attributedTo": carol.actor.id,
                "content": "<p>remote quoted target</p>",
                "published": "2026-08-01T11:00:00Z",
                "to": [PUBLIC],
            }),
        );
        objects.insert(
            stamp_uri.clone(),
            json!({
                "id": stamp_uri,
                "type": "QuoteAuthorization",
                "attributedTo": carol.actor.id,
                "interactingObject": remote_quote_uri,
                "interactionTarget": quoted_uri,
            }),
        );
    }
    let state = test_state_with(pool.clone(), stub.clone());
    history_db::save_settings(&pool, true, 90, false)
        .await
        .unwrap();

    remote_history::request(&state, &bob, JobKind::Initial, Some(alice.id))
        .await
        .unwrap();
    assert_eq!(remote_history::run_due(&state).await, 1);

    for (uri, expected_content) in [
        (&self_quote_uri, "<p>history item 10</p>"),
        (&remote_quote_uri, "<p>remote quoted target</p>"),
    ] {
        let stored = status::find_by_uri(&pool, uri)
            .await
            .unwrap()
            .expect("history quote should be stored");
        let rendered = entities::render_status(&pool, TEST_DOMAIN, &stored, None)
            .await
            .unwrap();
        assert_eq!(rendered["quote"]["state"], "accepted");
        assert_eq!(
            rendered["quote"]["quoted_status"]["content"],
            expected_content
        );
    }
    let notifications: i64 = sqlx::query_scalar("SELECT count(*) FROM notifications")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(notifications, 0);
    let quote_jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM quote_verify_jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(quote_jobs, 0, "both quote decisions settled synchronously");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn history_announce_fetches_uncached_target_and_stores_visible_boost(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = remote_account(&pool, "bob").await;
    let bob_uri = bob.uri.as_deref().unwrap();
    let outbox = format!("{bob_uri}/outbox");
    let announce_uri = format!("{bob_uri}/activities/announce-30");
    let carol = RemoteUser::new("elsewhere.example", "carol");
    let target_uri = format!("{}/statuses/30", carol.actor.id);
    let stub = Arc::new(StubFederation::default());
    stub.actors
        .lock()
        .unwrap()
        .insert(carol.actor.id.clone(), carol.actor.clone());
    {
        let mut objects = stub.objects.lock().unwrap();
        objects.insert(
            outbox.clone(),
            json!({
                "id": outbox,
                "type": "OrderedCollection",
                "orderedItems": [{
                    "id": announce_uri,
                    "type": "Announce",
                    "actor": bob_uri,
                    "object": target_uri,
                    "published": "2026-08-01T12:30:00Z"
                }],
            }),
        );
        objects.insert(
            target_uri.clone(),
            json!({
                "id": target_uri,
                "type": "Note",
                "attributedTo": carol.actor.id,
                "content": "<p>uncached boosted target</p>",
                "published": "2026-08-01T12:00:00Z",
                "to": [PUBLIC],
            }),
        );
    }
    let state = test_state_with(pool.clone(), stub.clone());
    history_db::save_settings(&pool, true, 90, false)
        .await
        .unwrap();

    remote_history::request(&state, &bob, JobKind::Initial, Some(alice.id))
        .await
        .unwrap();
    assert_eq!(remote_history::run_due(&state).await, 1);

    let target = status::find_by_uri(&pool, &target_uri)
        .await
        .unwrap()
        .expect("uncached boost target should be resolved");
    let boost = status::find_by_uri(&pool, &announce_uri)
        .await
        .unwrap()
        .expect("historical boost should be stored");
    assert_eq!(boost.reblog_of_id, Some(target.id));
    assert_eq!(
        status::ingest_provenance(&pool, boost.id)
            .await
            .unwrap()
            .as_deref(),
        Some("history")
    );
    let rendered = entities::render_status(&pool, TEST_DOMAIN, &boost, None)
        .await
        .unwrap();
    assert_eq!(
        rendered["reblog"]["content"],
        "<p>uncached boosted target</p>"
    );
    let side_effects: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM notifications)
              + (SELECT count(*) FROM delivery_jobs)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(side_effects, 0);

    let (promoted, first_effects) =
        status::upsert_remote_reblog_delivery(&pool, &announce_uri, bob.id, target.id, None)
            .await
            .unwrap();
    let (_, replay_effects) =
        status::upsert_remote_reblog_delivery(&pool, &announce_uri, bob.id, target.id, None)
            .await
            .unwrap();
    assert_eq!(promoted.id, boost.id);
    assert!(first_effects);
    assert!(!replay_effects);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn cold_history_image_caches_from_its_small_proxy_without_a_thumbnail(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = remote_account(&pool, "bob").await;
    let bob_uri = bob.uri.as_deref().unwrap();
    let outbox = format!("{bob_uri}/outbox");
    let media_url = "https://remote.example/media/history-zero.jpg";
    let stub = Arc::new(StubFederation::default());
    stub.objects.lock().unwrap().insert(
        outbox.clone(),
        json!({
            "id": outbox,
            "type": "OrderedCollection",
            "orderedItems": [note(&bob, 0, None)],
        }),
    );
    stub.serve_media(media_url, "image/png", sample_png());
    let state = test_state_with(pool.clone(), stub.clone());
    history_db::save_settings(&pool, true, 90, false)
        .await
        .unwrap();

    remote_history::request(&state, &bob, JobKind::Initial, Some(alice.id))
        .await
        .unwrap();
    assert_eq!(remote_history::run_due(&state).await, 1);
    let cold = status::find_by_uri(&pool, &format!("{bob_uri}/statuses/0"))
        .await
        .unwrap()
        .unwrap();
    let (media_id, on_demand, deferred, thumbnail): (i64, bool, bool, Option<String>) =
        sqlx::query_as(
            "SELECT id, download_on_demand, history_deferred, thumbnail_remote_url
             FROM media_attachments WHERE status_id = $1",
        )
        .bind(cold.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!on_demand);
    assert!(deferred);
    assert!(thumbnail.is_none(), "the regression requires no poster URL");
    assert_eq!(stub.media_fetches(), Vec::<String>::new());

    let response = build_router(state.clone())
        .oneshot(
            Request::builder()
                .uri(format!("/media/proxy/attachment/{media_id}/small"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .unwrap();
    assert!(location.starts_with(&format!("https://{TEST_DOMAIN}/media/")));
    assert_eq!(stub.media_fetches(), vec![media_url.to_owned()]);
    let (file, small): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT file_name, small_file_name FROM media_attachments WHERE id = $1")
            .bind(media_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(file.is_some());
    assert!(small.is_some());
    let media_jobs: i64 =
        sqlx::query_scalar("SELECT count(*) FROM media_processing_jobs WHERE media_id = $1")
            .bind(media_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(media_jobs, 0, "the successful proxy consumes its retry job");
    assert_eq!(
        status::ingest_provenance(&pool, cold.id)
            .await
            .unwrap()
            .as_deref(),
        Some("history"),
        "media access does not manufacture a live delivery"
    );

    let promoted = ingest_remote_note_delivery(&state, &bob, &note(&bob, 0, None))
        .await
        .unwrap();
    assert!(promoted.delivery_effects);
    let (deferred, media_jobs): (bool, i64) = sqlx::query_as(
        "SELECT m.history_deferred,
                (SELECT count(*) FROM media_processing_jobs j WHERE j.media_id = m.id)
         FROM media_attachments m WHERE m.id = $1",
    )
    .bind(media_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!deferred);
    assert_eq!(
        media_jobs, 0,
        "promotion does not requeue an image already cached on demand"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn iri_first_items_wrappers_updates_announces_and_one_older_page_interoperate(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = remote_account(&pool, "bob").await;
    let actor = bob.uri.as_deref().unwrap();
    let outbox = format!("{actor}/outbox");
    let first = format!("{actor}/outbox?page=first");
    let older = format!("{actor}/outbox?page=older");
    let mut original = note(&bob, 0, None);
    original["content"] = json!("<p>original snapshot</p>");
    let mut edited = note(&bob, 0, None);
    edited["content"] = json!("<p>edited snapshot</p>");
    edited["updated"] = json!("2026-08-02T12:00:00Z");
    let mut unlisted = note(&bob, 1, None);
    unlisted["to"] = json!([format!("{actor}/followers")]);
    unlisted["cc"] = json!([PUBLIC]);
    let stub = Arc::new(StubFederation::default());
    {
        let mut objects = stub.objects.lock().unwrap();
        objects.insert(
            outbox.clone(),
            json!({
                "id": outbox,
                "type": "Collection",
                "totalItems": 3,
                "first": first,
            }),
        );
        objects.insert(
            first.clone(),
            json!({
                "id": first,
                "type": "CollectionPage",
                "partOf": outbox,
                "next": older,
                "items": [
                    {"id": format!("{actor}/activities/create-0"), "type": "Create", "actor": actor, "object": original},
                    {"id": format!("{actor}/activities/update-0"), "type": "Update", "actor": actor, "object": edited},
                    unlisted,
                    {"id": format!("{actor}/activities/like"), "type": "Like", "actor": actor, "object": "https://elsewhere.example/status"},
                    {"id": format!("{actor}/activities/announce-0"), "type": "Announce", "actor": actor, "object": format!("{actor}/statuses/0")},
                ],
            }),
        );
        objects.insert(
            older.clone(),
            json!({
                "id": older,
                "type": "OrderedCollectionPage",
                "partOf": format!("{actor}/outbox"),
                "next": format!("{actor}/outbox?page=first"),
                "orderedItems": [note(&bob, 2, None)],
            }),
        );
    }
    let state = test_state_with(pool.clone(), stub.clone());
    history_db::save_settings(&pool, true, 90, false)
        .await
        .unwrap();

    remote_history::request(&state, &bob, JobKind::Initial, Some(alice.id))
        .await
        .unwrap();
    assert_eq!(remote_history::run_due(&state).await, 1);
    let snapshot = history_db::snapshot(&pool, bob.id).await.unwrap().unwrap();
    assert_eq!(snapshot.state, "partial");
    assert_eq!(snapshot.items_seen, 5);
    assert_eq!(snapshot.items_accepted, 4);
    assert_eq!(
        snapshot.available_statuses, 3,
        "update reuses one canonical row"
    );
    let updated = status::find_by_uri(&pool, &format!("{actor}/statuses/0"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.content, "<p>edited snapshot</p>");
    assert_eq!(stub.fetches(), [outbox.clone(), first.clone()]);

    assert_eq!(
        remote_history::request(&state, &bob, JobKind::Older, Some(alice.id))
            .await
            .unwrap(),
        EnqueueOutcome::Enqueued
    );
    assert_eq!(remote_history::run_due(&state).await, 1);
    let complete = history_db::snapshot(&pool, bob.id).await.unwrap().unwrap();
    assert_eq!(complete.state, "complete");
    assert!(complete.next_page_uri.is_none(), "cycle is not persisted");
    assert_eq!(complete.available_statuses, 4);
    assert_eq!(stub.fetches(), [outbox, first, older]);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn stale_refresh_sends_etag_and_handles_not_modified_without_reingest(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = remote_account(&pool, "bob").await;
    let outbox = format!("{}/outbox", bob.uri.as_deref().unwrap());
    let stub = Arc::new(StubFederation::default());
    stub.objects.lock().unwrap().insert(
        outbox.clone(),
        json!({
            "id": outbox,
            "type": "OrderedCollection",
            "orderedItems": [note(&bob, 0, None)],
        }),
    );
    stub.serve_activitypub_etag(&outbox, "\"history-v1\"");
    let state = test_state_with(pool.clone(), stub.clone());
    history_db::save_settings(&pool, true, 90, false)
        .await
        .unwrap();
    remote_history::request(&state, &bob, JobKind::Initial, Some(alice.id))
        .await
        .unwrap();
    remote_history::run_due(&state).await;
    sqlx::query(
        "UPDATE remote_history_states
         SET last_success_at = now() - interval '2 hours'
         WHERE account_id = $1",
    )
    .bind(bob.id)
    .execute(&pool)
    .await
    .unwrap();

    assert_eq!(
        remote_history::request(&state, &bob, JobKind::Refresh, Some(alice.id))
            .await
            .unwrap(),
        EnqueueOutcome::Enqueued
    );
    remote_history::run_due(&state).await;
    assert_eq!(
        stub.conditional_fetches(),
        [
            (outbox.clone(), None),
            (outbox, Some("\"history-v1\"".to_owned())),
        ]
    );
    let snapshot = history_db::snapshot(&pool, bob.id).await.unwrap().unwrap();
    assert_eq!(snapshot.pages_fetched, 1, "304 performs no page ingest");
    assert_eq!(snapshot.available_statuses, 1);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn cross_origin_first_cursor_is_rejected_without_a_second_fetch(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = remote_account(&pool, "bob").await;
    let outbox = format!("{}/outbox", bob.uri.as_deref().unwrap());
    let stub = Arc::new(StubFederation::default());
    stub.objects.lock().unwrap().insert(
        outbox.clone(),
        json!({
            "id": outbox,
            "type": "OrderedCollection",
            "first": "https://evil.example/exfiltrate",
        }),
    );
    let state = test_state_with(pool.clone(), stub.clone());
    history_db::save_settings(&pool, true, 90, false)
        .await
        .unwrap();
    assert_eq!(
        remote_history::request(&state, &bob, JobKind::Initial, Some(alice.id))
            .await
            .unwrap(),
        EnqueueOutcome::Enqueued
    );
    assert_eq!(remote_history::run_due(&state).await, 1);
    let snapshot = history_db::snapshot(&pool, bob.id).await.unwrap().unwrap();
    assert_eq!(snapshot.state, "unsupported");
    assert_eq!(
        snapshot.last_error_class.as_deref(),
        Some("cross_origin_first")
    );
    assert_eq!(stub.fetches(), [outbox]);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn policy_change_after_enqueue_cancels_before_network(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = remote_account(&pool, "bob").await;
    let stub = Arc::new(StubFederation::default());
    let state = test_state_with(pool.clone(), stub.clone());
    history_db::save_settings(&pool, true, 90, false)
        .await
        .unwrap();
    assert_eq!(
        remote_history::request(&state, &bob, JobKind::Initial, Some(alice.id))
            .await
            .unwrap(),
        EnqueueOutcome::Enqueued
    );
    instance_policy::create_domain_block(
        &pool,
        NewDomainBlock {
            domain: "remote.example",
            severity: "suspend",
            reject_media: false,
            reject_reports: false,
            private_comment: None,
            public_comment: None,
            obfuscate: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(remote_history::run_due(&state).await, 1);
    assert!(stub.fetches().is_empty());
    let snapshot = history_db::snapshot(&pool, bob.id).await.unwrap().unwrap();
    assert_eq!(snapshot.state, "unsupported");
    assert_eq!(
        snapshot.last_error_class.as_deref(),
        Some("instance_policy")
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn anonymous_and_read_only_get_routes_never_enqueue(pool: PgPool) {
    let bob = remote_account(&pool, "bob").await;
    history_db::save_settings(&pool, true, 90, false)
        .await
        .unwrap();
    let app = test_app_with(pool.clone(), Arc::default());
    let profile = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/@bob@remote.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(profile.status(), StatusCode::OK);
    let _ = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/accounts/{}/statuses", bob.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let state_read = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/accounts/{}/remote_history", bob.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(state_read.status(), StatusCode::UNAUTHORIZED);
    let fetch = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/accounts/{}/remote_history/fetch", bob.id))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"mode":"initial"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(fetch.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(history_db::pending_count(&pool).await.unwrap(), 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn authenticated_first_profile_status_page_enqueues_without_waiting_on_federation(
    pool: PgPool,
) {
    let (_alice, token) = local_user_with_token(&pool, "alice", "read").await;
    let bob = remote_account(&pool, "bob").await;
    history_db::save_settings(&pool, true, 90, false)
        .await
        .unwrap();
    let stub = Arc::new(StubFederation::default());
    let app = test_app_with(pool.clone(), stub.clone());

    // Account metadata, pagination and pinned-only reads are ordinary cache
    // reads. Third-party clients express profile-view intent only through an
    // authenticated first page of the account-status timeline.
    for uri in [
        format!("/api/v1/accounts/{}", bob.id),
        format!("/api/v1/accounts/{}/statuses?max_id=1", bob.id),
        format!("/api/v1/accounts/{}/statuses?pinned=true", bob.id),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    assert_eq!(history_db::pending_count(&pool).await.unwrap(), 0);

    for _ in 0..2 {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/accounts/{}/statuses", bob.id))
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if history_db::pending_count(&pool).await.unwrap() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detached hydration intent should become a durable job");
    assert!(
        stub.fetches().is_empty(),
        "the foreground GET must not fetch"
    );
    let viewed = sqlx::query_scalar::<_, bool>(
        "SELECT last_viewed_at IS NOT NULL FROM remote_history_states WHERE account_id = $1",
    )
    .bind(bob.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(viewed);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn bare_item_dereferencing_is_off_by_default_and_hard_capped_when_enabled(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let stub = Arc::new(StubFederation::default());
    history_db::save_settings(&pool, true, 90, false)
        .await
        .unwrap();

    let bob = remote_account(&pool, "bob").await;
    let bob_outbox = format!("{}/outbox", bob.uri.as_deref().unwrap());
    let bob_items: Vec<String> = (0..7)
        .map(|n| format!("{}/statuses/{n}", bob.uri.as_deref().unwrap()))
        .collect();
    stub.objects.lock().unwrap().insert(
        bob_outbox.clone(),
        json!({"id": bob_outbox, "type": "OrderedCollection", "orderedItems": bob_items}),
    );
    let state = test_state_with(pool.clone(), stub.clone());
    remote_history::request(&state, &bob, JobKind::Initial, Some(alice.id))
        .await
        .unwrap();
    remote_history::run_due(&state).await;
    assert_eq!(
        history_db::snapshot(&pool, bob.id)
            .await
            .unwrap()
            .unwrap()
            .items_accepted,
        0
    );
    assert_eq!(stub.fetches(), [bob_outbox]);

    history_db::save_settings(&pool, true, 90, true)
        .await
        .unwrap();
    let carol = remote_account(&pool, "carol").await;
    let carol_outbox = format!("{}/outbox", carol.uri.as_deref().unwrap());
    let carol_items: Vec<String> = (0..7)
        .map(|n| format!("{}/statuses/{n}", carol.uri.as_deref().unwrap()))
        .collect();
    {
        let mut objects = stub.objects.lock().unwrap();
        objects.insert(
            carol_outbox.clone(),
            json!({"id": carol_outbox, "type": "OrderedCollection", "orderedItems": carol_items}),
        );
        for n in 0..7 {
            let object = note(&carol, n, None);
            objects.insert(
                format!("{}/statuses/{n}", carol.uri.as_deref().unwrap()),
                object,
            );
        }
    }
    remote_history::request(&state, &carol, JobKind::Initial, Some(alice.id))
        .await
        .unwrap();
    remote_history::run_due(&state).await;
    assert_eq!(
        history_db::snapshot(&pool, carol.id)
            .await
            .unwrap()
            .unwrap()
            .items_accepted,
        5
    );
    let carol_fetches = stub
        .fetches()
        .into_iter()
        .filter(|uri| uri.contains("/users/carol/"))
        .collect::<Vec<_>>();
    assert_eq!(carol_fetches.len(), 6, "one outbox plus at most five IRIs");
}
