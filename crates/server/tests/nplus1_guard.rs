//! Regression guard: the batched account renderer must issue a number of
//! database queries independent of page size. If someone reintroduces a
//! per-row `account_json` loop behind a list endpoint, the query count grows
//! with the page and this test fails.
//!
//! **Know what these tests do not cover.** The base fixtures seed plain
//! `"hello world"` statuses and vary the *author* count, so on their own they
//! only catch per-author fan-outs; a loop hanging off an optional artifact is
//! dead code under them. Dedicated fixture shapes below now exercise quotes,
//! migrated accounts (`moved_to_uri`), tagged collections, preview cards with
//! a verified author, profile emoji, client applications, custom-emoji
//! reactions, poll-option emoji, and collection-/strike-bearing
//! notifications. `docs/NPLUS1_INVENTORY.md` lists every fan-out that still
//! exists and, for each, why it is invisible to this file; adding a fixture
//! shape to these tests is the way to shrink that list (the 2026-07 QC
//! audit documentation prescribed it).
mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::create_local_account;
use plamenu::entities;
use plamenu_db::account::RemoteAccountData;
use plamenu_db::collection::{NewCollection, NewCollectionItem};
use plamenu_db::conversation::AddStatus;
use plamenu_db::notification::NotificationFilter;
use plamenu_db::preview_card::NewPreviewCard;
use plamenu_db::status::NewLocalStatus;
use plamenu_db::{
    PgPool, account, block, collection, conversation, follow, id, mention, notification,
    preview_card, status, tagged_object,
};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;

/// Counts every `sqlx::query` tracing event — i.e. every statement the pool
/// executes — regardless of which task/thread runs it.
struct QueryCounter(Arc<AtomicUsize>);

impl<S: tracing::Subscriber> Layer<S> for QueryCounter {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        if event.metadata().target().starts_with("sqlx::query") {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
}

struct ApplicationSettingsReadCounter(Arc<AtomicUsize>);

impl<S: tracing::Subscriber> Layer<S> for ApplicationSettingsReadCounter {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        #[derive(Default)]
        struct Statement(bool);
        impl tracing::field::Visit for Statement {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if matches!(field.name(), "summary" | "db.statement") {
                    let sql = format!("{value:?}");
                    self.0 |= sql.contains("SELECT account_id, show_application");
                }
            }
        }
        if event.metadata().target().starts_with("sqlx::query") {
            let mut statement = Statement::default();
            event.record(&mut statement);
            if statement.0 {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn status_application_settings_reuse_author_snapshot_and_preserve_wire_visibility(
    pool: PgPool,
) {
    let author = create_local_account(&pool, "author", "Author").await;
    plamenu_db::user::create(&pool, author.id, None, "unused-test-password-hash")
        .await
        .unwrap();
    let booster = create_local_account(&pool, "booster", "Booster").await;
    let plain = status::create_local(
        &pool,
        NewLocalStatus::new(author.id, "plain", "public", None),
    )
    .await
    .unwrap();
    let plain_boost = status::create_local_reblog(&pool, booster.id, plain.id)
        .await
        .unwrap();
    let app = plamenu_db::oauth::create_app(
        &pool,
        plamenu_db::oauth::NewApp {
            name: "Client",
            website: Some("https://client.example"),
            client_id: "test-client",
            client_secret_hash: "unused-test-secret-hash",
            redirect_uris: &["urn:ietf:wg:oauth:2.0:oob".to_owned()],
            scopes: "read write",
        },
    )
    .await
    .unwrap();
    let attributed = status::create_local(
        &pool,
        NewLocalStatus::new(author.id, "via app", "public", None),
    )
    .await
    .unwrap();
    status::set_application(&pool, attributed.id, app.id)
        .await
        .unwrap();
    let attributed = status::find_by_id(&pool, attributed.id)
        .await
        .unwrap()
        .unwrap();
    let app_boost = status::create_local_reblog(&pool, booster.id, attributed.id)
        .await
        .unwrap();

    let reads = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(ApplicationSettingsReadCounter(reads.clone())),
    );
    let mut empty_counts = Vec::new();
    for item in [&plain, &plain_boost] {
        reads.store(0, Ordering::Relaxed);
        let page =
            entities::render_statuses(&pool, "plamenu.test", std::slice::from_ref(item), None)
                .await
                .unwrap();
        empty_counts.push(reads.load(Ordering::Relaxed));
        if item.reblog_of_id.is_some() {
            assert!(
                page[0].get("application").is_none(),
                "no user preference exists for the booster"
            );
            assert_eq!(
                page[0]["reblog"].get("application"),
                Some(&serde_json::Value::Null)
            );
        } else {
            assert_eq!(page[0].get("application"), Some(&serde_json::Value::Null));
        }
    }
    let expected_app = serde_json::json!({"name": "Client", "website": "https://client.example"});
    reads.store(0, Ordering::Relaxed);
    let public = entities::render_statuses(
        &pool,
        "plamenu.test",
        std::slice::from_ref(&app_boost),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        reads.load(Ordering::Relaxed),
        0,
        "boost targets reuse the same owner's loaded preference"
    );
    assert_eq!(public[0]["reblog"]["application"], expected_app);

    sqlx::query("UPDATE users SET show_application = FALSE WHERE account_id = $1")
        .bind(author.id)
        .execute(&pool)
        .await
        .unwrap();
    let hidden = entities::render_statuses(
        &pool,
        "plamenu.test",
        std::slice::from_ref(&app_boost),
        None,
    )
    .await
    .unwrap();
    assert!(hidden[0]["reblog"].get("application").is_none());
    let owner = entities::render_statuses(
        &pool,
        "plamenu.test",
        std::slice::from_ref(&app_boost),
        Some(author.id),
    )
    .await
    .unwrap();
    assert_eq!(owner[0]["reblog"]["application"], expected_app);
    assert_eq!(
        empty_counts,
        [0, 0],
        "plain statuses reuse the loaded owner preference"
    );
}

struct ViewerFlagsReadCounter(Arc<AtomicUsize>);

impl<S: tracing::Subscriber> Layer<S> for ViewerFlagsReadCounter {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        #[derive(Default)]
        struct Statement(bool);
        impl tracing::field::Visit for Statement {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if matches!(field.name(), "summary" | "db.statement") {
                    let sql = format!("{value:?}");
                    self.0 |= sql.contains("ANY($1)")
                        && sql.contains("ANY($2)")
                        && [
                            "FROM favourites",
                            "FROM status_dislikes",
                            "FROM bookmarks",
                            "FROM status_pins",
                            "FROM statuses -- STUBKEEP: a boost",
                            "FROM status_conversations sc",
                        ]
                        .iter()
                        .any(|table| sql.contains(table));
                }
            }
        }
        if event.metadata().target().starts_with("sqlx::query") {
            let mut statement = Statement::default();
            event.record(&mut statement);
            if statement.0 {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn status_viewer_flags_share_one_snapshot_and_stay_viewer_specific(pool: PgPool) {
    use plamenu_db::{bookmark, dislike, favourite, pin};
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let carol = create_local_account(&pool, "carol", "Carol").await;
    let original = status::create_local(
        &pool,
        NewLocalStatus::new(alice.id, "original", "public", None),
    )
    .await
    .unwrap();
    let clean = status::create_local(&pool, NewLocalStatus::new(bob.id, "clean", "public", None))
        .await
        .unwrap();
    let off_page = status::create_local(
        &pool,
        NewLocalStatus::new(carol.id, "off page", "public", None),
    )
    .await
    .unwrap();
    let boost = status::create_local_reblog(&pool, alice.id, original.id)
        .await
        .unwrap();
    favourite::create(&pool, alice.id, original.id, None)
        .await
        .unwrap();
    pin::create(&pool, alice.id, original.id).await.unwrap();
    dislike::create(&pool, bob.id, original.id, None)
        .await
        .unwrap();
    bookmark::create(&pool, bob.id, original.id).await.unwrap();
    for (owner, item) in [(alice.id, &original), (carol.id, &off_page)] {
        let conv = conversation::ensure_for_status(
            &pool,
            &conversation::EnsureConversation {
                status_id: item.id,
                account_id: owner,
                in_reply_to_id: None,
                is_reply: false,
                refs: conversation::ContextRefs::default(),
            },
        )
        .await
        .unwrap();
        let muter = if item.id == original.id {
            bob.id
        } else {
            carol.id
        };
        conversation::mute(&pool, muter, conv).await.unwrap();
    }
    favourite::create(&pool, carol.id, off_page.id, None)
        .await
        .unwrap();
    dislike::create(&pool, carol.id, off_page.id, None)
        .await
        .unwrap();
    bookmark::create(&pool, carol.id, off_page.id)
        .await
        .unwrap();
    pin::create(&pool, carol.id, off_page.id).await.unwrap();
    status::create_local_reblog(&pool, carol.id, off_page.id)
        .await
        .unwrap();

    let reads = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(ViewerFlagsReadCounter(reads.clone())),
    );
    let keys = [
        "favourited",
        "downvoted",
        "reblogged",
        "bookmarked",
        "muted",
    ];
    let mut counts = Vec::new();
    for (viewer, expected) in [
        (Some(alice.id), [true, false, true, false, false]),
        (Some(bob.id), [false, true, false, true, true]),
        (Some(carol.id), [false; 5]),
        (None, [false; 5]),
    ] {
        reads.store(0, Ordering::Relaxed);
        let page = entities::render_statuses(
            &pool,
            "plamenu.test",
            &[original.clone(), clean.clone(), boost.clone()],
            viewer,
        )
        .await
        .unwrap();
        counts.push(reads.load(Ordering::Relaxed));
        for (key, value) in keys.iter().zip(expected) {
            assert_eq!(
                page[0].get(*key),
                Some(&serde_json::Value::Bool(value)),
                "original {viewer:?} {key}"
            );
            assert_eq!(
                page[2].get(*key),
                Some(&serde_json::Value::Bool(value)),
                "boost {viewer:?} {key}"
            );
            assert_eq!(
                page[2]["reblog"].get(*key),
                Some(&serde_json::Value::Bool(value)),
                "target {viewer:?} {key}"
            );
            assert_eq!(
                page[1].get(*key),
                Some(&serde_json::Value::Bool(false)),
                "unmarked {viewer:?} {key}"
            );
        }
        if viewer == Some(alice.id) {
            assert_eq!(page[0].get("pinned"), Some(&serde_json::Value::Bool(true)));
        } else {
            assert!(page[0].get("pinned").is_none());
        }
        if viewer == Some(bob.id) {
            assert_eq!(page[1].get("pinned"), Some(&serde_json::Value::Bool(false)));
        } else {
            assert!(page[1].get("pinned").is_none());
        }
    }
    reads.store(0, Ordering::Relaxed);
    let recipients = entities::render_status_for_viewers(
        &pool,
        "plamenu.test",
        &original,
        &[alice.id, bob.id, carol.id],
    )
    .await
    .unwrap();
    counts.push(reads.load(Ordering::Relaxed));
    assert_eq!(recipients[&alice.id]["favourited"], true);
    assert_eq!(recipients[&bob.id]["bookmarked"], true);
    assert_eq!(recipients[&bob.id]["muted"], true);
    assert_eq!(recipients[&carol.id]["downvoted"], false);
    favourite::delete(&pool, alice.id, original.id)
        .await
        .unwrap();
    bookmark::delete(&pool, bob.id, original.id).await.unwrap();
    let conv = conversation::of_status(&pool, original.id)
        .await
        .unwrap()
        .unwrap();
    conversation::unmute(&pool, bob.id, conv).await.unwrap();
    let fresh =
        entities::render_status_for_viewers(&pool, "plamenu.test", &original, &[alice.id, bob.id])
            .await
            .unwrap();
    assert_eq!(fresh[&alice.id]["favourited"], false);
    assert_eq!(fresh[&alice.id]["pinned"], true);
    assert_eq!(fresh[&alice.id]["reblogged"], true);
    assert_eq!(fresh[&bob.id]["bookmarked"], false);
    assert_eq!(fresh[&bob.id]["muted"], false);
    assert_eq!(fresh[&bob.id]["downvoted"], true);
    assert_eq!(
        counts,
        [1, 1, 1, 0, 1],
        "one fresh flags snapshot per signed-in page or recipient set"
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn status_media_keys_share_one_snapshot_without_losing_cleanup_or_ownership(pool: PgPool) {
    use plamenu_db::{media, media_cleanup};
    async fn attachment(
        pool: &PgPool,
        owner: i64,
        status_id: Option<i64>,
        original: Option<&str>,
        preview: Option<&str>,
    ) -> i64 {
        let media_id = id::next();
        sqlx::query("INSERT INTO media_attachments (id, account_id, status_id, file_name, small_file_name, content_type, remote_url) VALUES ($1, $2, $3, $4, $5, 'video/mp4', $6)")
            .bind(media_id).bind(owner).bind(status_id).bind(original).bind(preview)
            .bind(original.is_none().then(|| format!("https://video.example/media/{media_id}")))
            .execute(pool).await.unwrap();
        media_id
    }
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let original = status::create_local(
        &pool,
        NewLocalStatus::new(alice.id, "original", "public", None),
    )
    .await
    .unwrap();
    let empty = status::create_local(&pool, NewLocalStatus::new(bob.id, "empty", "public", None))
        .await
        .unwrap();
    let foreign = status::create_local(
        &pool,
        NewLocalStatus::new(bob.id, "foreign", "public", None),
    )
    .await
    .unwrap();
    attachment(
        &pool,
        alice.id,
        Some(original.id),
        Some("orig.mp4"),
        Some("thumb.jpg"),
    )
    .await;
    let cached_only = attachment(&pool, alice.id, Some(original.id), None, None).await;
    attachment(&pool, alice.id, Some(original.id), Some("orig.mp4"), None).await;
    attachment(&pool, alice.id, Some(original.id), Some(""), None).await;
    attachment(&pool, alice.id, None, Some("unattached.mp4"), None).await;
    let foreign_media =
        attachment(&pool, bob.id, Some(foreign.id), Some("foreign.mp4"), None).await;
    for (media_id, url, key) in [
        (cached_only, "https://video.example/one", "one.m4s"),
        (cached_only, "https://video.example/two", "two.m4s"),
        (
            foreign_media,
            "https://video.example/foreign",
            "foreign.m4s",
        ),
    ] {
        assert!(
            media::hls_segment_record(&pool, media_id, url, 0, 50, key, 50)
                .await
                .unwrap()
        );
    }
    let expected = vec![
        "",
        "one.m4s",
        "orig.mp4",
        "orig.mp4",
        "thumb.jpg",
        "two.m4s",
    ];
    let reads = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(QueryCounter(reads.clone())),
    );
    let mut conn = pool.acquire().await.unwrap();
    let mut counts = Vec::new();
    for (status_id, keys) in [
        (original.id, expected.clone()),
        (empty.id, Vec::new()),
        (0, Vec::new()),
        (foreign.id, vec!["foreign.m4s", "foreign.mp4"]),
    ] {
        reads.store(0, Ordering::Relaxed);
        let mut actual = media_cleanup::collect_status_keys(&mut conn, status_id)
            .await
            .unwrap();
        counts.push(reads.load(Ordering::Relaxed));
        actual.sort();
        assert_eq!(
            actual, keys,
            "every key stays attached to the requested status"
        );
    }
    drop(conn);
    let mut tx = pool.begin().await.unwrap();
    assert!(
        status::delete_local_conn(&mut tx, original.id, bob.id)
            .await
            .unwrap()
            .is_none()
    );
    tx.commit().await.unwrap();
    assert_eq!(
        media_cleanup::pending_count(&pool).await.unwrap(),
        0,
        "a non-owner cannot schedule another author's files"
    );
    assert!(
        status::find_by_id(&pool, original.id)
            .await
            .unwrap()
            .is_some()
    );
    let mut tx = pool.begin().await.unwrap();
    assert!(
        status::delete_local_conn(&mut tx, original.id, alice.id)
            .await
            .unwrap()
            .is_some()
    );
    let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM media_cleanup_jobs")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(
        queued, 4,
        "shared keys deduplicate and blank names do not enqueue"
    );
    tx.rollback().await.unwrap();
    assert_eq!(
        media_cleanup::pending_count(&pool).await.unwrap(),
        0,
        "cleanup and status deletion roll back together"
    );
    assert!(
        status::find_by_id(&pool, original.id)
            .await
            .unwrap()
            .is_some()
    );
    let mut conn = pool.acquire().await.unwrap();
    let mut restored = media_cleanup::collect_status_keys(&mut conn, original.id)
        .await
        .unwrap();
    restored.sort();
    assert_eq!(
        restored, expected,
        "rollback restores cached-only segments as well as original and preview keys"
    );
    drop(conn);
    let mut tx = pool.begin().await.unwrap();
    assert!(
        status::delete_local_conn(&mut tx, original.id, alice.id)
            .await
            .unwrap()
            .is_some()
    );
    tx.commit().await.unwrap();
    let mut queued: Vec<String> = sqlx::query_scalar("SELECT file_name FROM media_cleanup_jobs")
        .fetch_all(&pool)
        .await
        .unwrap();
    queued.sort();
    assert_eq!(queued, ["one.m4s", "orig.mp4", "thumb.jpg", "two.m4s"]);
    assert!(
        status::find_by_id(&pool, original.id)
            .await
            .unwrap()
            .is_none()
    );
    let mut conn = pool.acquire().await.unwrap();
    assert!(
        media_cleanup::collect_status_keys(&mut conn, original.id)
            .await
            .unwrap()
            .is_empty()
    );
    let mut foreign_keys = media_cleanup::collect_status_keys(&mut conn, foreign.id)
        .await
        .unwrap();
    foreign_keys.sort();
    assert_eq!(
        foreign_keys,
        ["foreign.m4s", "foreign.mp4"],
        "foreign media remains referenced"
    );
    assert_eq!(
        counts,
        [1, 1, 1, 1],
        "one keys snapshot for populated, empty, missing, and foreign statuses"
    );
}

struct RemoteMediaWriteCounter(Arc<AtomicUsize>);

impl<S: tracing::Subscriber> Layer<S> for RemoteMediaWriteCounter {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        #[derive(Default)]
        struct Statement(bool);
        impl tracing::field::Visit for Statement {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if matches!(field.name(), "summary" | "db.statement") {
                    let sql = format!("{value:?}");
                    self.0 |= [
                        "instance_domain_rejects_media(domain)",
                        "INSERT INTO media_attachments",
                        "INSERT INTO media_processing_jobs",
                    ]
                    .iter()
                    .any(|part| sql.contains(part));
                }
            }
        }
        if event.metadata().target().starts_with("sqlx::query") {
            let mut statement = Statement::default();
            event.record(&mut statement);
            if statement.0 {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn remote_media_insert_and_processing_share_fresh_policy_snapshot(pool: PgPool) {
    use plamenu_db::{instance_policy, media};
    let author = remote_account(&pool, "media.example", "alice").await;
    let post = status::create_local(
        &pool,
        NewLocalStatus::new(author.id, "media", "public", None),
    )
    .await
    .unwrap();
    let reads = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(RemoteMediaWriteCounter(reads.clone())),
    );
    let mut counts = Vec::new();
    reads.store(0, Ordering::Relaxed);
    media::create_remote(
        &pool,
        media::NewRemoteMedia {
            account_id: author.id,
            status_id: post.id,
            remote_url: "https://media.example/original.jpg",
            content_type: "image/jpeg",
            description: Some("original"),
            blurhash: Some("UBL_:rOpGG-;~qRjWBay0fI]%2s:S$M{R*of"),
            focus: Some((0.1, -0.2)),
            width: Some(640),
            height: Some(480),
            thumbnail_remote_url: Some("https://media.example/preview.jpg"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    counts.push(reads.load(Ordering::Relaxed));
    let original = media::for_statuses_read_only(&pool, &[post.id])
        .await
        .unwrap()[&post.id][0]
        .clone();
    assert_eq!(original.processing, "queued");
    assert_eq!(original.description.as_deref(), Some("original"));
    assert_eq!(
        (
            original.width,
            original.height,
            original.focus_x,
            original.focus_y
        ),
        (Some(640), Some(480), Some(0.1), Some(-0.2))
    );
    assert_eq!(
        original.thumbnail_remote_url.as_deref(),
        Some("https://media.example/preview.jpg")
    );
    reads.store(0, Ordering::Relaxed);
    media::create_remote(
        &pool,
        media::NewRemoteMedia {
            account_id: author.id,
            status_id: post.id,
            remote_url: "https://media.example/original.jpg",
            content_type: "image/png",
            description: Some("duplicate must not edit"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    counts.push(reads.load(Ordering::Relaxed));
    let duplicate = media::for_statuses_read_only(&pool, &[post.id])
        .await
        .unwrap();
    assert_eq!(duplicate[&post.id].len(), 1);
    assert_eq!(duplicate[&post.id][0].id, original.id);
    assert_eq!(
        duplicate[&post.id][0].description.as_deref(),
        Some("original")
    );
    for (url, intrinsic, history) in [
        ("https://media.example/deferred.mp4", true, false),
        ("https://media.example/cold.jpg", false, true),
    ] {
        reads.store(0, Ordering::Relaxed);
        media::create_remote(
            &pool,
            media::NewRemoteMedia {
                account_id: author.id,
                status_id: post.id,
                remote_url: url,
                content_type: if intrinsic { "video/mp4" } else { "image/jpeg" },
                download_on_demand: intrinsic,
                history_deferred: history,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        counts.push(reads.load(Ordering::Relaxed));
        let rows = media::for_statuses_read_only(&pool, &[post.id])
            .await
            .unwrap();
        let row = rows[&post.id]
            .iter()
            .find(|row| row.remote_url.as_deref() == Some(url))
            .unwrap();
        assert_eq!(row.processing, "complete");
        assert_eq!(row.download_on_demand, intrinsic);
        let deferred: bool =
            sqlx::query_scalar("SELECT history_deferred FROM media_attachments WHERE id=$1")
                .bind(row.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(deferred, history);
    }
    let block = instance_policy::create_domain_block(
        &pool,
        instance_policy::NewDomainBlock {
            domain: "media.example",
            severity: "noop",
            reject_media: true,
            reject_reports: false,
            private_comment: None,
            public_comment: None,
            obfuscate: false,
        },
    )
    .await
    .unwrap();
    reads.store(0, Ordering::Relaxed);
    media::create_remote(
        &pool,
        media::NewRemoteMedia {
            account_id: author.id,
            status_id: post.id,
            remote_url: "https://media.example/rejected.jpg",
            content_type: "image/jpeg",
            ..Default::default()
        },
    )
    .await
    .unwrap();
    counts.push(reads.load(Ordering::Relaxed));
    let rows = media::for_statuses_read_only(&pool, &[post.id])
        .await
        .unwrap();
    let rejected = rows[&post.id]
        .iter()
        .find(|row| row.remote_url.as_deref() == Some("https://media.example/rejected.jpg"))
        .unwrap();
    assert_eq!(rejected.processing, "complete");
    instance_policy::update_domain_block(
        &pool,
        block.id,
        instance_policy::DomainBlockUpdate {
            reject_media: Some(false),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    reads.store(0, Ordering::Relaxed);
    media::create_remote(
        &pool,
        media::NewRemoteMedia {
            account_id: author.id,
            status_id: post.id,
            remote_url: "https://media.example/allowed-again.jpg",
            content_type: "image/jpeg",
            ..Default::default()
        },
    )
    .await
    .unwrap();
    counts.push(reads.load(Ordering::Relaxed));
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM media_processing_jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    let queued_urls: Vec<String> = sqlx::query_scalar("SELECT m.remote_url FROM media_processing_jobs j JOIN media_attachments m ON m.id=j.media_id ORDER BY m.remote_url")
        .fetch_all(&pool).await.unwrap();
    assert_eq!(
        jobs, 2,
        "only original and fresh allowed work: {queued_urls:?}"
    );
    let rows = media::for_statuses_read_only(&pool, &[post.id])
        .await
        .unwrap();
    assert_eq!(rows[&post.id].len(), 5);
    assert_eq!(rows[&post.id].iter().find(|row| row.remote_url.as_deref() == Some("https://media.example/allowed-again.jpg")).unwrap().processing, "queued");

    // A queue failure must roll back the attachment, including the combined statement.
    sqlx::raw_sql("CREATE FUNCTION reject_media_job() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected media job failure'; END; $$; CREATE TRIGGER reject_media_job BEFORE INSERT ON media_processing_jobs FOR EACH ROW EXECUTE FUNCTION reject_media_job()")
        .execute(&pool).await.unwrap();
    assert!(
        media::create_remote(
            &pool,
            media::NewRemoteMedia {
                account_id: author.id,
                status_id: post.id,
                remote_url: "https://media.example/failed-job.jpg",
                content_type: "image/jpeg",
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
    sqlx::raw_sql("DROP TRIGGER reject_media_job ON media_processing_jobs")
        .execute(&pool)
        .await
        .unwrap();
    // A later rendition failure must roll back both the first statement and its job.
    sqlx::raw_sql("CREATE FUNCTION reject_media_ladder() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected rendition failure'; END; $$; CREATE TRIGGER reject_media_ladder BEFORE INSERT ON media_renditions FOR EACH ROW EXECUTE FUNCTION reject_media_ladder()")
        .execute(&pool).await.unwrap();
    assert!(
        media::create_remote(
            &pool,
            media::NewRemoteMedia {
                account_id: author.id,
                status_id: post.id,
                remote_url: "https://media.example/failed-ladder.mp4",
                content_type: "video/mp4",
                renditions: vec![media::NewRendition {
                    height: 360,
                    width: Some(640),
                    frame_rate: Some(30),
                    size_bytes: Some(100),
                    origin_url: "https://media.example/360.mp4",
                    is_audio: false,
                }],
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
    let rows = media::for_statuses_read_only(&pool, &[post.id])
        .await
        .unwrap();
    assert_eq!(
        rows[&post.id].len(),
        5,
        "neither failed attachment survived"
    );
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM media_processing_jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(jobs, 2, "neither failed write left queued work");
    let ladders: i64 = sqlx::query_scalar("SELECT count(*) FROM media_renditions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(ladders, 0);
    assert_eq!(
        counts, [1; 6],
        "fresh policy, insert, and processing queue share one statement"
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn hashtag_upsert_and_links_share_snapshot_preserving_names_dates_and_rollback(pool: PgPool) {
    use plamenu_db::tag;
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let original =
        status::create_local(&pool, NewLocalStatus::new(alice.id, "tags", "public", None))
            .await
            .unwrap();
    let other = status::create_local(
        &pool,
        NewLocalStatus::new(alice.id, "other tags", "public", None),
    )
    .await
    .unwrap();
    let rust_id = tag::ensure(&pool, "Rust").await.unwrap();
    let reads = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(QueryCounter(reads.clone())),
    );
    let mut conn = pool.acquire().await.unwrap();
    let mut counts = Vec::new();
    reads.store(0, Ordering::Relaxed);
    tag::ensure_and_attach_many(
        &mut conn,
        original.id,
        &["rUsT", "Rust", "NewTag", "newtag", "Another"],
    )
    .await
    .unwrap();
    counts.push(reads.load(Ordering::Relaxed));
    let ids: Vec<i64> =
        sqlx::query_scalar("SELECT tag_id FROM status_tags WHERE status_id=$1 ORDER BY tag_id")
            .bind(original.id)
            .fetch_all(&mut *conn)
            .await
            .unwrap();
    assert_eq!(ids.len(), 3);
    assert!(ids.contains(&rust_id));
    let linked_at: time::OffsetDateTime =
        sqlx::query_scalar("SELECT sort_at FROM status_tags WHERE status_id=$1 LIMIT 1")
            .bind(original.id)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    let matches_status: bool = sqlx::query_scalar("SELECT bool_and(st.sort_at=s.sort_at) FROM status_tags st JOIN statuses s ON s.id=st.status_id WHERE s.id=$1")
        .bind(original.id).fetch_one(&mut *conn).await.unwrap();
    assert!(matches_status);
    sqlx::query("UPDATE statuses SET sort_at=sort_at-interval '1 day' WHERE id=$1")
        .bind(original.id)
        .execute(&mut *conn)
        .await
        .unwrap();
    reads.store(0, Ordering::Relaxed);
    tag::ensure_and_attach_many(&mut conn, original.id, &["RUST", "NEWTAG", "another"])
        .await
        .unwrap();
    counts.push(reads.load(Ordering::Relaxed));
    let again: Vec<i64> =
        sqlx::query_scalar("SELECT tag_id FROM status_tags WHERE status_id=$1 ORDER BY tag_id")
            .bind(original.id)
            .fetch_all(&mut *conn)
            .await
            .unwrap();
    assert_eq!(again, ids, "redelivery preserves existing tag and link ids");
    let dates: Vec<time::OffsetDateTime> =
        sqlx::query_scalar("SELECT sort_at FROM status_tags WHERE status_id=$1")
            .bind(original.id)
            .fetch_all(&mut *conn)
            .await
            .unwrap();
    assert!(
        dates.iter().all(|date| *date == linked_at),
        "conflict keeps link dates"
    );
    reads.store(0, Ordering::Relaxed);
    tag::ensure_and_attach_many(&mut conn, other.id, &[])
        .await
        .unwrap();
    counts.push(reads.load(Ordering::Relaxed));
    tag::ensure_and_attach_many(&mut conn, other.id, &["rust", "newTAG"])
        .await
        .unwrap();
    drop(conn);
    let names = tag::names_for_statuses(&pool, &[original.id, other.id])
        .await
        .unwrap();
    assert_eq!(
        names[&original.id],
        ["Another", "NewTag", "Rust"],
        "existing spelling and the first new spelling survive case-folded collisions"
    );
    assert_eq!(
        names[&other.id],
        ["NewTag", "Rust"],
        "links stay scoped to the requested status"
    );
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM tags")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(total, 3, "no duplicate case-folded tags");
    sqlx::raw_sql("CREATE FUNCTION reject_tag_link() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected tag link failure'; END; $$; CREATE TRIGGER reject_tag_link BEFORE INSERT ON status_tags FOR EACH ROW EXECUTE FUNCTION reject_tag_link()")
        .execute(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert!(
        tag::ensure_and_attach_many(&mut tx, original.id, &["Fault"])
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM tags")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        total, 3,
        "a failed transactional link cannot leave its tag behind"
    );
    assert_eq!(
        tag::names_for_statuses(&pool, &[original.id])
            .await
            .unwrap()[&original.id],
        ["Another", "NewTag", "Rust"]
    );
    assert_eq!(
        counts,
        [1, 1, 0],
        "one upsert/link snapshot, with no query for empty input"
    );
}

async fn seed_followers(pool: &PgPool, viewer: i64, n: usize) -> Vec<plamenu_db::account::Account> {
    let mut accounts = Vec::new();
    for i in 0..n {
        let a = create_local_account(pool, &format!("f{n}_{i}"), "Follower").await;
        // viewer follows them and they follow viewer — exercises both
        // feature-approval edge lookups and the counters.
        follow::create(pool, viewer, a.id, None).await.unwrap();
        follow::create(pool, a.id, viewer, None).await.unwrap();
        accounts.push(a);
    }
    accounts
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn render_accounts_query_count_is_flat(pool: PgPool) {
    let counter = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(QueryCounter(counter.clone())),
    );

    let viewer = create_local_account(&pool, "viewer", "Viewer").await;
    // Give the viewer a locked/undiscoverable-mix so feature-approval exercises
    // both the viewer->row and row->viewer edge lookups on every row.
    let small = seed_followers(&pool, viewer.id, 3).await;
    let big = seed_followers(&pool, viewer.id, 12).await;

    let measure = |accounts: Vec<plamenu_db::account::Account>| {
        let pool = pool.clone();
        let counter = counter.clone();
        async move {
            counter.store(0, Ordering::Relaxed);
            entities::render_accounts(&pool, "plamenu.test", &accounts, Some(viewer.id))
                .await
                .unwrap();
            counter.load(Ordering::Relaxed)
        }
    };

    let small_queries = measure(small).await;
    let big_queries = measure(big).await;

    println!(
        "render_accounts: 3 rows -> {small_queries} queries, 12 rows -> {big_queries} queries"
    );

    // Rendering 4x the accounts must not cost 4x the queries. A per-row (N+1)
    // renderer would jump from ~24 to ~96; the batched one is flat. The small
    // margin tolerates a conditional per-row lookup (custom emoji in a bio,
    // an account that has migrated) without admitting a linear blowup.
    assert!(
        big_queries <= small_queries + 2,
        "query count grew with page size: {small_queries} -> {big_queries} (N+1 regression)"
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn native_channel_account_rendering_is_batched(pool: PgPool) {
    let counter = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(QueryCounter(counter.clone())),
    );
    let owner = create_local_account(&pool, "owner", "Owner").await;
    let mut channels = Vec::new();
    for index in 0..12 {
        let mut channel = create_local_account(&pool, &format!("films{index}"), "Films").await;
        let mut transaction = pool.begin().await.unwrap();
        plamenu_db::video::create_channel(&mut transaction, channel.id, owner.id)
            .await
            .unwrap();
        transaction.commit().await.unwrap();
        channel.actor_type = Some("Group".into());
        channels.push(channel);
    }
    counter.store(0, Ordering::Relaxed);
    let small = entities::render_accounts(&pool, "plamenu.test", &channels[..3], None)
        .await
        .unwrap();
    let small_queries = counter.swap(0, Ordering::Relaxed);
    let big = entities::render_accounts(&pool, "plamenu.test", &channels, None)
        .await
        .unwrap();
    let big_queries = counter.load(Ordering::Relaxed);
    assert!(
        small
            .iter()
            .chain(big.iter())
            .all(|a| a["video_channel"] == true && a["group"] == true)
    );
    assert_eq!(
        big_queries, small_queries,
        "channel identity must not add per-account queries"
    );
    counter.store(0, Ordering::Relaxed);
    entities::render_accounts(&pool, "plamenu.test", &[owner], None)
        .await
        .unwrap();
    assert_eq!(
        big_queries,
        counter.load(Ordering::Relaxed),
        "channel identity belongs in the existing account overlay, not another query"
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn render_relationships_query_count_is_flat(pool: PgPool) {
    let counter = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(QueryCounter(counter.clone())),
    );

    let viewer = create_local_account(&pool, "viewer", "Viewer").await;
    let small = seed_followers(&pool, viewer.id, 3).await;
    let big = seed_followers(&pool, viewer.id, 12).await;

    counter.store(0, Ordering::Relaxed);
    entities::render_relationships(&pool, viewer.id, &small)
        .await
        .unwrap();
    let small_queries = counter.swap(0, Ordering::Relaxed);

    entities::render_relationships(&pool, viewer.id, &big)
        .await
        .unwrap();
    let big_queries = counter.swap(0, Ordering::Relaxed);

    println!(
        "render_relationships: 3 rows -> {small_queries} queries, 12 rows -> {big_queries} queries"
    );

    // `/api/v1/accounts/relationships` is one of the most-called endpoints; a
    // per-target renderer would cost ~9 queries per id. The batch is flat.
    assert!(
        big_queries <= small_queries + 2,
        "relationship query count grew with page size: {small_queries} -> {big_queries} (N+1)"
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn render_statuses_query_count_is_flat(pool: PgPool) {
    let counter = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(QueryCounter(counter.clone())),
    );
    let viewer = create_local_account(&pool, "viewer", "Viewer").await;

    // Each status from a *distinct* author — the worst case for a per-author
    // fan-out inside the status renderer.
    let seed = |n: usize| {
        let pool = pool.clone();
        async move {
            let mut out = Vec::new();
            for i in 0..n {
                let author = create_local_account(&pool, &format!("sa{n}_{i}"), "Author").await;
                let post = status::create_local(
                    &pool,
                    NewLocalStatus::new(author.id, "hello world", "public", None),
                )
                .await
                .unwrap();
                // Distinct invitation targets keep a per-card lookup from hiding
                // behind a shared session or an empty optional-feature fixture.
                let uri = format!("https://apps.example/sessions/{n}_{i}");
                plamenu_db::webxdc::set_remote_invitation(
                    &pool,
                    post.id,
                    Some((&uri, "Shared board")),
                )
                .await
                .unwrap();
                out.push(post);
            }
            out
        }
    };
    let small = seed(3).await;
    let big = seed(12).await;

    counter.store(0, Ordering::Relaxed);
    let small_rendered = entities::render_statuses(&pool, "plamenu.test", &small, Some(viewer.id))
        .await
        .unwrap();
    let small_queries = counter.swap(0, Ordering::Relaxed);

    let big_rendered = entities::render_statuses(&pool, "plamenu.test", &big, Some(viewer.id))
        .await
        .unwrap();
    let big_queries = counter.load(Ordering::Relaxed);

    for (posts, rendered) in [(&small, &small_rendered), (&big, &big_rendered)] {
        assert_eq!(rendered.len(), posts.len());
        for (i, card) in rendered.iter().enumerate() {
            assert_eq!(card["webxdc_invitation"]["name"], "Shared board");
            assert_eq!(
                card["webxdc_invitation"]["url"],
                format!("https://apps.example/sessions/{}_{i}", posts.len())
            );
        }
    }

    println!(
        "render_statuses: 3 authors -> {small_queries} queries, 12 authors -> {big_queries} queries"
    );

    // The status renderer is the single hottest path (every timeline,
    // notification, conversation). It must batch every per-status and
    // per-author artifact; a surviving `find_by_id`-per-author loop would jump
    // from ~28 to ~37+ here. The batch is flat regardless of author count.
    assert!(
        big_queries <= small_queries + 2,
        "status render query count grew with author count: {small_queries} -> {big_queries} (N+1)"
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn video_metadata_rendering_is_batched(pool: PgPool) {
    use plamenu_db::{media, video_metadata};
    let counter = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(QueryCounter(counter.clone())),
    );
    let author = create_local_account(&pool, "filmmaker", "Filmmaker").await;
    let mut posts = Vec::new();
    for _ in 0..12 {
        let mut input = NewLocalStatus::new(author.id, "Film", "public", None);
        input.object_type = Some("Video");
        let post = status::create_local(&pool, input).await.unwrap();
        let file = media::create_local(
            &pool,
            media::NewLocalMedia {
                kind: "video",
                ..media::NewLocalMedia::new(author.id, id::next(), "film.mp4", "video/mp4")
            },
        )
        .await
        .unwrap();
        assert!(
            media::set_attachments(&pool, post.id, author.id, &[file.id])
                .await
                .unwrap()
        );
        video_metadata::upsert(
            &pool,
            &video_metadata::Metadata {
                media_id: file.id,
                category: Some(13),
                licence: Some(2),
                licence_version_known: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        posts.push(post);
    }
    counter.store(0, Ordering::Relaxed);
    let small = entities::render_statuses(&pool, "plamenu.test", &posts[..3], Some(author.id))
        .await
        .unwrap();
    let small_queries = counter.swap(0, Ordering::Relaxed);
    let big = entities::render_statuses(&pool, "plamenu.test", &posts, Some(author.id))
        .await
        .unwrap();
    let big_queries = counter.load(Ordering::Relaxed);
    for card in small.iter().chain(&big) {
        assert_eq!(
            card["media_attachments"][0]["video_metadata"]["licence"]["id"],
            2
        );
    }
    println!("Video metadata: 3 statuses -> {small_queries} queries, 12 -> {big_queries}");
    assert!(
        big_queries <= small_queries + 2,
        "Video metadata N+1: {small_queries} -> {big_queries}"
    );
}

/// The soft-delete placeholder path: a page containing stubs must cost no more
/// than the same page fully live. The deleted-id set is one batched lookup
/// (`RenderMaps` and `NoteBatch` both), and a stub renders from data the batch
/// already holds — a per-stub query here would turn every thread with deleted
/// middles into an N+1.
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn stubs_do_not_add_per_row_queries(pool: PgPool) {
    let counter = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(QueryCounter(counter.clone())),
    );
    let viewer = create_local_account(&pool, "viewer", "Viewer").await;

    let mut statuses = Vec::new();
    let mut authors = Vec::new();
    for i in 0..12 {
        let author = create_local_account(&pool, &format!("stub{i}"), "Author").await;
        let stored = status::create_local(
            &pool,
            NewLocalStatus::new(author.id, "hello world", "public", None),
        )
        .await
        .unwrap();
        statuses.push(stored);
        authors.push(author);
    }

    counter.store(0, Ordering::Relaxed);
    entities::render_statuses(&pool, "plamenu.test", &statuses, Some(viewer.id))
        .await
        .unwrap();
    let live_queries = counter.swap(0, Ordering::Relaxed);

    // Stub half the page: each needs a live reply or the delete would wipe.
    for (stored, author) in statuses.iter().zip(&authors).take(6) {
        status::create_local(
            &pool,
            NewLocalStatus::new(viewer.id, "a reply", "public", Some(stored.id)),
        )
        .await
        .unwrap();
        status::stub_local(&pool, stored.id, author.id)
            .await
            .unwrap()
            .expect("a replied post stubs");
    }

    counter.store(0, Ordering::Relaxed);
    let rendered = entities::render_statuses(&pool, "plamenu.test", &statuses, Some(viewer.id))
        .await
        .unwrap();
    let stubbed_queries = counter.load(Ordering::Relaxed);

    println!(
        "render_statuses: live page -> {live_queries} queries, half-stubbed -> {stubbed_queries}"
    );
    assert_eq!(rendered.len(), 12, "stubs render as placeholders, not gaps");
    assert!(
        stubbed_queries <= live_queries + 2,
        "stubs added per-row queries: {live_queries} -> {stubbed_queries} (N+1)"
    );
}

/// The *quote* path of the status renderer. A page of accepted
/// quote posts must fetch the quoted statuses in one query and render them in
/// ONE recursive pass — the old per-quote `find_by_id` + full renderer call
/// multiplied the entire render cost by the page's quote count, invisible to
/// the plain fixture above.
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn render_statuses_with_quotes_query_count_is_flat(pool: PgPool) {
    let counter = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(QueryCounter(counter.clone())),
    );
    let viewer = create_local_account(&pool, "viewer", "Viewer").await;

    // Each status quotes a distinct other author's post — worst case for both
    // the per-quote fetch and the per-quoted-author render.
    let seed = |n: usize| {
        let pool = pool.clone();
        async move {
            let mut out = Vec::new();
            for i in 0..n {
                let quoted_author =
                    create_local_account(&pool, &format!("qa{n}_{i}"), "Quoted").await;
                let quoted = status::create_local(
                    &pool,
                    NewLocalStatus::new(quoted_author.id, "the original", "public", None),
                )
                .await
                .unwrap();
                let quoter = create_local_account(&pool, &format!("qq{n}_{i}"), "Quoter").await;
                let quoting = status::create_local(
                    &pool,
                    NewLocalStatus::new(quoter.id, "look at this", "public", None),
                )
                .await
                .unwrap();
                plamenu_db::quote::create(
                    &pool,
                    plamenu_db::quote::NewQuote {
                        quote_id: plamenu_db::id::next(),
                        status_id: Some(quoting.id),
                        status_uri: &format!("https://plamenu.test/statuses/{}", quoting.id),
                        account_id: quoter.id,
                        quoted_status_id: Some(quoted.id),
                        quoted_account_id: Some(quoted_author.id),
                        state: "accepted",
                        activity_uri: None,
                        approval_uri: None,
                        quoted_uri: None,
                        legacy: false,
                    },
                )
                .await
                .unwrap();
                out.push(quoting);
            }
            out
        }
    };
    let small = seed(3).await;
    let big = seed(12).await;

    counter.store(0, Ordering::Relaxed);
    entities::render_statuses(&pool, "plamenu.test", &small, Some(viewer.id))
        .await
        .unwrap();
    let small_queries = counter.swap(0, Ordering::Relaxed);

    entities::render_statuses(&pool, "plamenu.test", &big, Some(viewer.id))
        .await
        .unwrap();
    let big_queries = counter.load(Ordering::Relaxed);

    println!(
        "render_statuses with quotes: 3 quotes -> {small_queries} queries, \
         12 quotes -> {big_queries} queries"
    );

    assert!(
        big_queries <= small_queries + 2,
        "quote render query count grew with quote count: {small_queries} -> {big_queries} (N+1)"
    );
}

/// A stored remote account on `domain`, without real key generation — the
/// resolvable migration target `moved_to_uri` needs (`find_by_uri` matches the
/// `uri` column, which local accounts leave NULL).
async fn remote_account(
    pool: &PgPool,
    domain: &str,
    username: &str,
) -> plamenu_db::account::Account {
    let uri = format!("https://{domain}/users/{username}");
    account::upsert_remote(
        pool,
        RemoteAccountData {
            username,
            domain,
            uri: &uri,
            display_name: "",
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
            discoverable: true,
            feature_approval_policy: 0,
            is_bot: false,
            indexable: false,
            show_media: None,
            show_media_replies: None,
            show_featured: None,
            memorial: false,
            actor_type: None,
        },
    )
    .await
    .unwrap()
}

/// The migrated-account path of the account renderer: every row carrying a
/// `moved_to_uri` used to live-render its target (~8 queries each) inside the
/// otherwise-batched loop — invisible to the plain fixture above, which never
/// sets `moved_to_uri`. The targets must resolve and render as one batch.
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn render_accounts_with_migrations_query_count_is_flat(pool: PgPool) {
    let counter = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(QueryCounter(counter.clone())),
    );

    let viewer = create_local_account(&pool, "viewer", "Viewer").await;
    // Every follower has migrated to a distinct resolvable remote target —
    // the worst case for a per-row live render of the target.
    let seed = |n: usize| {
        let pool = pool.clone();
        async move {
            let mut out = Vec::new();
            for i in 0..n {
                let mover = create_local_account(&pool, &format!("mv{n}_{i}"), "Mover").await;
                let target = remote_account(&pool, "new-home.example", &format!("t{n}_{i}")).await;
                let mover = account::set_moved_to(&pool, mover.id, target.uri.as_deref())
                    .await
                    .unwrap()
                    .unwrap();
                out.push(mover);
            }
            out
        }
    };
    let small = seed(3).await;
    let big = seed(12).await;

    counter.store(0, Ordering::Relaxed);
    let rendered = entities::render_accounts(&pool, "plamenu.test", &small, Some(viewer.id))
        .await
        .unwrap();
    let small_queries = counter.swap(0, Ordering::Relaxed);
    assert!(
        rendered.iter().all(|entity| entity["moved"].is_object()),
        "every migrated row must carry its rendered target"
    );

    entities::render_accounts(&pool, "plamenu.test", &big, Some(viewer.id))
        .await
        .unwrap();
    let big_queries = counter.load(Ordering::Relaxed);

    println!(
        "render_accounts with migrations: 3 rows -> {small_queries} queries, \
         12 rows -> {big_queries} queries"
    );
    assert!(
        big_queries <= small_queries + 2,
        "migrated-account query count grew with page size: {small_queries} -> {big_queries} (N+1)"
    );
}

/// A `movedTo` cycle (A -> B, B -> A) used to recurse without bound — the
/// nested `moved` was stripped only *after* rendering it. The embedded target
/// must render exactly one level deep, identically on the live and batched
/// paths.
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn moved_render_is_one_level_even_on_a_cycle(pool: PgPool) {
    let viewer = create_local_account(&pool, "viewer", "Viewer").await;
    let a = remote_account(&pool, "old.example", "ouro").await;
    let b = remote_account(&pool, "new.example", "boros").await;
    let a = account::set_moved_to(&pool, a.id, b.uri.as_deref())
        .await
        .unwrap()
        .unwrap();
    account::set_moved_to(&pool, b.id, a.uri.as_deref())
        .await
        .unwrap()
        .unwrap();

    let live = entities::account_json(&pool, "plamenu.test", &a, Some(viewer.id))
        .await
        .unwrap();
    let moved = live.get("moved").expect("the redirect is shown");
    assert_eq!(moved["id"], serde_json::json!(b.id.to_string()));
    assert!(
        moved.get("moved").is_none(),
        "the embedded target must not carry its own moved (recursion guard)"
    );

    let page = std::slice::from_ref(&a);
    let batched = entities::render_accounts(&pool, "plamenu.test", page, Some(viewer.id))
        .await
        .unwrap();
    assert_eq!(
        batched[0], live,
        "batched moved render diverged from account_json"
    );
}

/// The tagged-collections path of the status renderer (FEP-7aa9): a collection
/// tagged onto many statuses used to re-fetch its owner and member items once
/// per (status, collection) pair. Owners, tags and items are fetched per page
/// and each distinct collection renders once, so the count stays flat.
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn render_statuses_with_tagged_collections_query_count_is_flat(pool: PgPool) {
    let counter = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(QueryCounter(counter.clone())),
    );
    let viewer = create_local_account(&pool, "viewer", "Viewer").await;
    let curator = create_local_account(&pool, "curator", "Curator").await;
    let featured = create_local_account(&pool, "featured", "Featured").await;

    // One shared collection tagged onto EVERY status (the pair explosion), plus
    // a distinct collection per status (the per-collection item fetch).
    let shared = collection::create(
        &pool,
        NewCollection {
            account_id: curator.id,
            name: "Shared",
            description: "",
            language: None,
            sensitive: false,
            discoverable: true,
            local: true,
            tag_id: None,
            uri: None,
            url: None,
            original_number_of_items: None,
        },
    )
    .await
    .unwrap();
    collection::add_item(
        &pool,
        NewCollectionItem {
            item_id: id::next(),
            collection_id: shared.id,
            account_id: Some(featured.id),
            state: "accepted",
            uri: None,
            object_uri: None,
            activity_uri: None,
            approval_uri: None,
        },
    )
    .await
    .unwrap()
    .unwrap();

    let seed = |n: usize| {
        let pool = pool.clone();
        let shared_id = shared.id;
        let curator_id = curator.id;
        let featured_id = featured.id;
        async move {
            let mut out = Vec::new();
            for i in 0..n {
                let author = create_local_account(&pool, &format!("tc{n}_{i}"), "Author").await;
                let stored = status::create_local(
                    &pool,
                    NewLocalStatus::new(author.id, "hello world", "public", None),
                )
                .await
                .unwrap();
                let own = collection::create(
                    &pool,
                    NewCollection {
                        account_id: curator_id,
                        name: "Own",
                        description: "",
                        language: None,
                        sensitive: false,
                        discoverable: true,
                        local: true,
                        tag_id: None,
                        uri: None,
                        url: None,
                        original_number_of_items: None,
                    },
                )
                .await
                .unwrap();
                collection::add_item(
                    &pool,
                    NewCollectionItem {
                        item_id: id::next(),
                        collection_id: own.id,
                        account_id: Some(featured_id),
                        state: "accepted",
                        uri: None,
                        object_uri: None,
                        activity_uri: None,
                        approval_uri: None,
                    },
                )
                .await
                .unwrap()
                .unwrap();
                for collection_id in [shared_id, own.id] {
                    tagged_object::add(
                        &pool,
                        stored.id,
                        collection_id,
                        &format!("https://plamenu.test/collections/{collection_id}"),
                    )
                    .await
                    .unwrap();
                }
                out.push(stored);
            }
            out
        }
    };
    let small = seed(3).await;
    let big = seed(12).await;

    counter.store(0, Ordering::Relaxed);
    let rendered = entities::render_statuses(&pool, "plamenu.test", &small, Some(viewer.id))
        .await
        .unwrap();
    let small_queries = counter.swap(0, Ordering::Relaxed);

    entities::render_statuses(&pool, "plamenu.test", &big, Some(viewer.id))
        .await
        .unwrap();
    let big_queries = counter.swap(0, Ordering::Relaxed);

    // The batched entity must match the singular `collection_json` render.
    let want =
        plamenu::collections::collection_json(&pool, "plamenu.test", &shared, Some(viewer.id))
            .await
            .unwrap();
    let got = rendered[0]["tagged_collections"]
        .as_array()
        .expect("tagged collections render")
        .iter()
        .find(|entity| entity["id"] == want["id"])
        .expect("the shared collection is on the first status")
        .clone();
    assert_eq!(got, want, "batched collection render diverged");

    println!(
        "render_statuses with tagged collections: 3 rows -> {small_queries} queries, \
         12 rows -> {big_queries} queries"
    );
    assert!(
        big_queries <= small_queries + 2,
        "tagged-collection query count grew with page size: {small_queries} -> {big_queries} (N+1)"
    );
}

/// The verified-creator path of the preview-card renderer: every card whose
/// `fediverse:creator` resolved to a known account used to live-render that
/// author (~6 queries per distinct author). The bench seed hardcodes
/// `author_account_id: None`, so only this fixture executes the branch.
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn render_statuses_with_card_authors_query_count_is_flat(pool: PgPool) {
    let counter = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(QueryCounter(counter.clone())),
    );
    let viewer = create_local_account(&pool, "viewer", "Viewer").await;

    // Each status carries a card verified to a *distinct* creator account.
    let seed = |n: usize| {
        let pool = pool.clone();
        async move {
            let mut out = Vec::new();
            for i in 0..n {
                let author = create_local_account(&pool, &format!("ca{n}_{i}"), "Author").await;
                let creator = create_local_account(&pool, &format!("cc{n}_{i}"), "Creator").await;
                let stored = status::create_local(
                    &pool,
                    NewLocalStatus::new(author.id, "hello world", "public", None),
                )
                .await
                .unwrap();
                let url = format!("https://articles.example/{n}/{i}");
                let card = preview_card::upsert(
                    &pool,
                    NewPreviewCard {
                        url: &url,
                        title: "An article",
                        kind: "link",
                        author_account_id: Some(creator.id),
                        ..NewPreviewCard::default()
                    },
                )
                .await
                .unwrap();
                preview_card::attach(&pool, stored.id, card.id, &url)
                    .await
                    .unwrap();
                out.push(stored);
            }
            out
        }
    };
    let small = seed(3).await;
    let big = seed(12).await;

    counter.store(0, Ordering::Relaxed);
    let rendered = entities::render_statuses(&pool, "plamenu.test", &small, Some(viewer.id))
        .await
        .unwrap();
    let small_queries = counter.swap(0, Ordering::Relaxed);
    assert!(
        rendered
            .iter()
            .all(|entity| entity["card"]["authors"][0]["account"].is_object()),
        "every card must carry its verified creator"
    );

    entities::render_statuses(&pool, "plamenu.test", &big, Some(viewer.id))
        .await
        .unwrap();
    let big_queries = counter.load(Ordering::Relaxed);

    println!(
        "render_statuses with card authors: 3 rows -> {small_queries} queries, \
         12 rows -> {big_queries} queries"
    );
    assert!(
        big_queries <= small_queries + 2,
        "card-author query count grew with page size: {small_queries} -> {big_queries} (N+1)"
    );
}

/// The profile-emoji path of the account renderer: every account whose bio or
/// display name carries a `:shortcode:` used to issue its own
/// `custom_emoji::lookup` inside the otherwise-batched loop — invisible to
/// the plain fixture above, whose seeded profiles carry no colons. The whole
/// page's profile emoji must resolve in one query.
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn render_accounts_with_profile_emoji_query_count_is_flat(pool: PgPool) {
    let counter = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(QueryCounter(counter.clone())),
    );
    let viewer = create_local_account(&pool, "viewer", "Viewer").await;
    plamenu_db::custom_emoji::create_local(&pool, "party", "party.png", "image/png", 0, None)
        .await
        .unwrap()
        .unwrap();

    let seed = |n: usize| {
        let pool = pool.clone();
        async move {
            let mut out = Vec::new();
            for i in 0..n {
                let account =
                    create_local_account(&pool, &format!("pe{n}_{i}"), "Loves :party: a lot").await;
                let outcome = plamenu_db::custom_emoji::create_personal_upload(
                    &pool,
                    account.id,
                    "party",
                    &format!("personal-{n}-{i}.png"),
                    "image/png",
                    1,
                    None,
                )
                .await
                .unwrap();
                assert!(matches!(
                    outcome,
                    plamenu_db::custom_emoji::PersonalCreateOutcome::Created(_)
                ));
                out.push(account);
            }
            out
        }
    };
    let small = seed(3).await;
    let big = seed(12).await;

    counter.store(0, Ordering::Relaxed);
    let rendered = entities::render_accounts(&pool, "plamenu.test", &small, Some(viewer.id))
        .await
        .unwrap();
    let small_queries = counter.swap(0, Ordering::Relaxed);
    assert!(
        rendered
            .iter()
            .all(
                |entity| entity["emojis"][0]["shortcode"] == serde_json::json!("party")
                    && entity["emojis"][0]["url"]
                        .as_str()
                        .is_some_and(|url| url.contains("personal-"))
            ),
        "every profile must carry its owner's personal emoji"
    );

    entities::render_accounts(&pool, "plamenu.test", &big, Some(viewer.id))
        .await
        .unwrap();
    let big_queries = counter.load(Ordering::Relaxed);

    println!(
        "render_accounts with profile emoji: 3 rows -> {small_queries} queries, \
         12 rows -> {big_queries} queries"
    );
    assert!(
        big_queries <= small_queries + 2,
        "profile-emoji query count grew with page size: {small_queries} -> {big_queries} (N+1)"
    );
}

/// The client-application, custom-emoji-reaction and poll-option-emoji paths
/// of the status renderer: each used to issue per-item lookups (one
/// `find_app_by_id` per distinct app, one `find_by_remote_image_url` per
/// reaction-group occurrence — with no negative memo — and one emoji lookup
/// per poll). All three artifacts are dark in the bench seed, so only this
/// fixture executes the branches.
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn render_statuses_with_apps_reactions_and_poll_emoji_is_flat(pool: PgPool) {
    let counter = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(QueryCounter(counter.clone())),
    );
    let viewer = create_local_account(&pool, "viewer", "Viewer").await;
    plamenu_db::custom_emoji::create_local(&pool, "party", "party.png", "image/png", 0, None)
        .await
        .unwrap()
        .unwrap();
    // One shared remote reaction image (the memo case) and, below, one
    // distinct unresolvable image per status (the negative-memo worst case).
    let shared_url = "https://remote.example/emoji/blob.png";
    plamenu_db::custom_emoji::upsert_remote(
        &pool,
        plamenu_db::custom_emoji::RemoteEmojiData {
            shortcode: "blob",
            domain: "remote.example",
            uri: None,
            image_remote_url: shared_url,
            updated: None,
        },
    )
    .await
    .unwrap();

    let seed = |n: usize| {
        let pool = pool.clone();
        async move {
            let mut out = Vec::new();
            for i in 0..n {
                let author = create_local_account(&pool, &format!("ar{n}_{i}"), "Author").await;
                let app = plamenu_db::oauth::create_app(
                    &pool,
                    plamenu_db::oauth::NewApp {
                        name: "Client",
                        website: None,
                        client_id: &format!("client-{n}-{i}"),
                        client_secret_hash: "hash",
                        redirect_uris: &["urn:ietf:wg:oauth:2.0:oob".to_owned()],
                        scopes: "read write",
                    },
                )
                .await
                .unwrap();
                let stored = status::create_local(
                    &pool,
                    NewLocalStatus::new(author.id, "hello :party: world", "public", None),
                )
                .await
                .unwrap();
                status::set_application(&pool, stored.id, app.id)
                    .await
                    .unwrap();
                plamenu_db::poll::create(
                    &pool,
                    plamenu_db::poll::NewPoll {
                        status_id: stored.id,
                        account_id: author.id,
                        options: &["yes :party:".to_owned(), "no".to_owned()],
                        cached_tallies: &[0, 0],
                        multiple: false,
                        hide_totals: false,
                        voters_count: Some(0),
                        expires_at: None,
                    },
                )
                .await
                .unwrap();
                for (name, url) in [
                    ("blob", shared_url.to_owned()),
                    (
                        "ghost",
                        format!("https://remote.example/emoji/gone-{n}-{i}.png"),
                    ),
                ] {
                    plamenu_db::reaction::create(
                        &pool,
                        plamenu_db::reaction::NewReaction {
                            account_id: author.id,
                            status_id: stored.id,
                            name,
                            custom_emoji_url: Some(&url),
                            uri: None,
                        },
                    )
                    .await
                    .unwrap();
                }
                out.push(stored);
            }
            out
        }
    };
    let small = seed(3).await;
    let big = seed(12).await;

    counter.store(0, Ordering::Relaxed);
    let rendered = entities::render_statuses(&pool, "plamenu.test", &small, Some(viewer.id))
        .await
        .unwrap();
    let small_queries = counter.swap(0, Ordering::Relaxed);
    // The branches must be live: the poll options resolve their emoji, and
    // the resolvable reaction group carries its proxied image URL.
    assert!(
        rendered
            .iter()
            .all(|entity| entity["poll"]["emojis"][0]["shortcode"] == serde_json::json!("party")),
        "every poll must resolve its option emoji"
    );
    assert!(
        rendered.iter().all(|entity| {
            entity["emoji_reactions"].as_array().is_some_and(|groups| {
                groups.iter().any(|g| {
                    g["name"] == serde_json::json!("blob@remote.example") && g.get("url").is_some()
                })
            })
        }),
        "every shared custom-emoji reaction must carry its image URL"
    );

    entities::render_statuses(&pool, "plamenu.test", &big, Some(viewer.id))
        .await
        .unwrap();
    let big_queries = counter.load(Ordering::Relaxed);

    println!(
        "render_statuses with apps/reactions/poll emoji: 3 rows -> {small_queries} queries, \
         12 rows -> {big_queries} queries"
    );
    assert!(
        big_queries <= small_queries + 2,
        "app/reaction/poll-emoji query count grew with page size: \
         {small_queries} -> {big_queries} (N+1)"
    );
}

/// `/api/v1/accounts/relationships` preserves duplicate ids
/// (Mastodon renders a repeated id twice), so the renderer receives the raw
/// capped target list. Rebuilding one relationship per occurrence must not cost
/// a query per occurrence — otherwise a body repeating one id amplifies queries.
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn render_relationships_is_flat_over_duplicate_ids(pool: PgPool) {
    let counter = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(QueryCounter(counter.clone())),
    );
    let viewer = create_local_account(&pool, "viewer", "Viewer").await;
    let target = create_local_account(&pool, "target", "Target").await;
    follow::create(&pool, viewer.id, target.id, None)
        .await
        .unwrap();

    let once = vec![target.clone()];
    let many = vec![target; 150];

    counter.store(0, Ordering::Relaxed);
    entities::render_relationships(&pool, viewer.id, &once)
        .await
        .unwrap();
    let one_query = counter.swap(0, Ordering::Relaxed);

    entities::render_relationships(&pool, viewer.id, &many)
        .await
        .unwrap();
    let many_queries = counter.load(Ordering::Relaxed);

    println!(
        "render_relationships: 1 target -> {one_query} queries, 150 duplicates -> {many_queries}"
    );
    assert_eq!(
        one_query, many_queries,
        "duplicate ids must not add queries (query-count amplification)"
    );
}

/// `familiar_followers` used to probe every requested id with a
/// serial `find_by_id`. The batched lookup must issue a fixed number of queries
/// regardless of how many ids (existing or not) are requested.
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn familiar_followers_lookup_is_flat_over_id_count(pool: PgPool) {
    let counter = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(QueryCounter(counter.clone())),
    );
    let viewer = create_local_account(&pool, "viewer", "Viewer").await;

    let one: Vec<i64> = vec![1_000_000];
    let many: Vec<i64> = (0..150).map(|i| 1_000_000 + i).collect();

    counter.store(0, Ordering::Relaxed);
    follow::familiar_followers(&pool, viewer.id, &one)
        .await
        .unwrap();
    let one_query = counter.swap(0, Ordering::Relaxed);

    follow::familiar_followers(&pool, viewer.id, &many)
        .await
        .unwrap();
    let many_queries = counter.load(Ordering::Relaxed);

    println!("familiar_followers lookup: 1 id -> {one_query} queries, 150 ids -> {many_queries}");
    assert_eq!(
        one_query, many_queries,
        "id count must not add per-id queries (serial find_by_id regression)"
    );
}

/// Seeds `n` distinct DM conversations for `viewer`, each with one partner who
/// sent one direct status.
async fn seed_conversations(pool: &PgPool, viewer: i64, n: usize) {
    for i in 0..n {
        let partner = create_local_account(pool, &format!("dm{n}_{i}"), "Partner").await;
        let st = status::create_local(
            pool,
            NewLocalStatus::new(partner.id, "hey there", "direct", None),
        )
        .await
        .unwrap();
        let conv = conversation::ensure_for_status(
            pool,
            &conversation::EnsureConversation {
                status_id: st.id,
                account_id: partner.id,
                in_reply_to_id: None,
                is_reply: false,
                refs: conversation::ContextRefs::default(),
            },
        )
        .await
        .unwrap();
        conversation::add_status(
            pool,
            AddStatus {
                account_id: viewer,
                conversation_id: conv,
                participant_account_ids: &[partner.id],
                status_id: st.id,
                sender_id: partner.id,
            },
        )
        .await
        .unwrap();
    }
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn render_conversations_query_count_is_flat(pool: PgPool) {
    let counter = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(QueryCounter(counter.clone())),
    );

    let viewer = create_local_account(&pool, "viewer", "Viewer").await;

    let measure = |limit: i64| {
        let pool = pool.clone();
        let counter = counter.clone();
        async move {
            let rows = conversation::list(&pool, viewer.id, None, None, None, limit)
                .await
                .unwrap();
            counter.store(0, Ordering::Relaxed);
            entities::render_conversations(&pool, "plamenu.test", viewer.id, &rows)
                .await
                .unwrap();
            (rows.len(), counter.load(Ordering::Relaxed))
        }
    };

    seed_conversations(&pool, viewer.id, 3).await;
    let (n_small, small_queries) = measure(3).await;
    seed_conversations(&pool, viewer.id, 9).await; // 12 total
    let (n_big, big_queries) = measure(12).await;

    println!(
        "render_conversations: {n_small} rows -> {small_queries} queries, \
         {n_big} rows -> {big_queries} queries"
    );

    // `GET /api/v1/conversations` is polled frequently by clients. The per-row
    // renderer cost ~38 queries per conversation (render_accounts_by_ids +
    // find_by_id + render_status each row); the batch prefetches accounts and
    // last-statuses once. A residual ~1 query per additional distinct
    // last-status author lives inside `render_statuses` (a separate, pre-
    // existing timeline-wide N+1, not specific to conversations) — the
    // page-size-proportional margin here tracks that, not the conversation
    // loop, which is flat. If the conversation loop regresses, 4x the rows
    // jumps by ~110 queries and this fails.
    assert!(
        big_queries <= small_queries + (n_big - n_small) + 4,
        "conversation query count grew with page size: {small_queries} -> {big_queries} (N+1)"
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn filter_viewable_query_count_is_flat(pool: PgPool) {
    let counter = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(QueryCounter(counter.clone())),
    );
    let viewer = create_local_account(&pool, "viewer", "Viewer").await;

    let seed = |n: usize| {
        let pool = pool.clone();
        async move {
            let mut items = Vec::new();
            for i in 0..n {
                let author = create_local_account(&pool, &format!("fv{n}_{i}"), "Author").await;
                items.push(
                    status::create_local(
                        &pool,
                        NewLocalStatus::new(author.id, "hi", "public", None),
                    )
                    .await
                    .unwrap(),
                );
            }
            items
        }
    };
    let small = seed(3).await;
    let big = seed(12).await;

    counter.store(0, Ordering::Relaxed);
    entities::filter_viewable(&pool, &small, Some(viewer.id))
        .await
        .unwrap();
    let small_queries = counter.swap(0, Ordering::Relaxed);
    entities::filter_viewable(&pool, &big, Some(viewer.id))
        .await
        .unwrap();
    let big_queries = counter.load(Ordering::Relaxed);

    println!("filter_viewable: 3 authors -> {small_queries} queries, 12 -> {big_queries} queries");
    // Behind /context, search and account-statuses. The per-item `can_view` cost
    // ~3 queries per public status; the batch is flat regardless of page size.
    assert!(
        big_queries <= small_queries + 2,
        "filter_viewable query count grew with page size: {small_queries} -> {big_queries} (N+1)"
    );
}

/// `filter_viewable` must decide each status identically to the per-item
/// `can_view` it replaces, across every visibility and relationship.
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn filter_viewable_matches_can_view(pool: PgPool) {
    let viewer = create_local_account(&pool, "viewer", "Viewer").await;
    let followed = create_local_account(&pool, "followed", "F").await;
    let stranger = create_local_account(&pool, "stranger", "S").await;
    let blocker = create_local_account(&pool, "blocker", "B").await;
    follow::create(&pool, viewer.id, followed.id, None)
        .await
        .unwrap();
    block::create(&pool, blocker.id, viewer.id, None)
        .await
        .unwrap();

    let mk = |author: i64, vis: &'static str| {
        let pool = pool.clone();
        async move {
            status::create_local(&pool, NewLocalStatus::new(author, "x", vis, None))
                .await
                .unwrap()
        }
    };
    let mut items = vec![
        mk(viewer.id, "private").await,    // own private -> visible
        mk(followed.id, "public").await,   // -> visible
        mk(followed.id, "unlisted").await, // -> visible
        mk(followed.id, "private").await,  // followed -> visible
        mk(followed.id, "local").await,    // local author -> visible
        mk(stranger.id, "private").await,  // not followed, no mention -> hidden
    ];
    let dm = mk(stranger.id, "direct").await; // mentioned -> visible
    mention::attach(&pool, dm.id, viewer.id, false)
        .await
        .unwrap();
    items.push(dm);
    items.push(mk(stranger.id, "direct").await); // no mention -> hidden
    let priv_m = mk(stranger.id, "private").await; // mentioned -> visible
    mention::attach(&pool, priv_m.id, viewer.id, false)
        .await
        .unwrap();
    items.push(priv_m);
    items.push(mk(blocker.id, "public").await); // author blocks viewer -> hidden

    for viewer_id in [Some(viewer.id), None] {
        let mut want = Vec::new();
        for it in &items {
            if entities::can_view(&pool, it, viewer_id).await.unwrap() {
                want.push(it.id);
            }
        }
        let got: Vec<i64> = entities::filter_viewable(&pool, &items, viewer_id)
            .await
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(
            got, want,
            "filter_viewable diverged from can_view (viewer={viewer_id:?})"
        );
    }
}

/// Seeds `n` notifications for `viewer`, each from a *distinct* sender and
/// (for the status-bearing kinds) pointing at that sender's own status — the
/// worst case for a per-sender/per-status fan-out inside the renderer. Every
/// fifth row carries a *distinct* collection and every fifth a *distinct*
/// moderation strike (with an appeal), so the composite `collection` /
/// `moderation_warning` embeds are live code under the flatness assertion —
/// they used to run 3-12 queries per distinct referenced entity.
async fn seed_notification_page(pool: &PgPool, viewer: i64, n: usize) {
    for i in 0..n {
        let sender = create_local_account(pool, &format!("nt{n}_{i}"), "Sender").await;
        let st = status::create_local(pool, NewLocalStatus::new(sender.id, "ping", "public", None))
            .await
            .unwrap();
        match i % 5 {
            0 => notification::create(pool, viewer, sender.id, "mention", Some(st.id))
                .await
                .unwrap(),
            1 => notification::create(pool, viewer, sender.id, "favourite", Some(st.id))
                .await
                .unwrap(),
            2 => notification::create(pool, viewer, sender.id, "follow", None)
                .await
                .unwrap(),
            3 => {
                // The sender added the viewer to a fresh collection of theirs.
                let coll = collection::create(
                    pool,
                    NewCollection {
                        account_id: sender.id,
                        name: "Faves",
                        description: "",
                        language: None,
                        sensitive: false,
                        discoverable: true,
                        local: true,
                        tag_id: None,
                        uri: None,
                        url: None,
                        original_number_of_items: None,
                    },
                )
                .await
                .unwrap();
                collection::add_item(
                    pool,
                    NewCollectionItem {
                        item_id: id::next(),
                        collection_id: coll.id,
                        account_id: Some(viewer),
                        state: "accepted",
                        uri: None,
                        object_uri: None,
                        activity_uri: None,
                        approval_uri: None,
                    },
                )
                .await
                .unwrap()
                .unwrap();
                notification::create_for_collection(
                    pool,
                    viewer,
                    sender.id,
                    "added_to_collection",
                    coll.id,
                )
                .await
                .unwrap();
            }
            _ => {
                // A distinct strike against the viewer, with an appeal filed.
                let warning = plamenu_db::account_warning::create(
                    pool,
                    plamenu_db::account_warning::NewAccountWarning {
                        account_id: Some(sender.id),
                        target_account_id: viewer,
                        action: "none",
                        text: "be nice",
                        report_id: None,
                        status_ids: &[],
                    },
                )
                .await
                .unwrap();
                plamenu_db::appeal::create(pool, viewer, warning.id, "sorry")
                    .await
                    .unwrap();
                notification::create_moderation_warning(pool, viewer, warning.id)
                    .await
                    .unwrap();
            }
        }
    }
}

const NO_FILTER: NotificationFilter<'static> = NotificationFilter {
    kinds: None,
    from_account_id: None,
    include_filtered: false,
};

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn render_notifications_query_count_is_flat(pool: PgPool) {
    let counter = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(QueryCounter(counter.clone())),
    );

    let viewer = create_local_account(&pool, "viewer", "Viewer").await;

    let measure = |limit: i64| {
        let pool = pool.clone();
        let counter = counter.clone();
        async move {
            let rows = notification::list(&pool, viewer.id, None, None, None, NO_FILTER, limit)
                .await
                .unwrap();
            counter.store(0, Ordering::Relaxed);
            entities::render_notifications(&pool, "plamenu.test", &rows, viewer.id)
                .await
                .unwrap();
            (rows.len(), counter.load(Ordering::Relaxed))
        }
    };

    // Five rows carry one of each kind (the rotation is `i % 5`), so the
    // small and big pages exercise the same set of embed branches — the
    // fixed cost of a live branch must not read as growth.
    seed_notification_page(&pool, viewer.id, 5).await;
    let (n_small, small_queries) = measure(5).await;
    seed_notification_page(&pool, viewer.id, 15).await; // 20 total
    let (n_big, big_queries) = measure(20).await;

    println!(
        "render_notifications: {n_small} rows -> {small_queries} queries, \
         {n_big} rows -> {big_queries} queries"
    );

    // `GET /api/v1/notifications` is polled by every client. The per-item
    // renderer cost a full `account_json` + `find_by_id` + singular
    // `render_status` per row (~15 queries each); the batch prefetches every
    // distinct sender and referenced status once and is flat in page size.
    assert!(
        big_queries <= small_queries + 2,
        "notification query count grew with page size: {small_queries} -> {big_queries} (N+1)"
    );
}

/// The batched notification renderer must produce each row exactly as the
/// per-item primitives (`account_json` / `render_status`) it replaced would,
/// and keep the kind-specific key rules (`status` only on status-bearing
/// kinds, `emoji` only on reactions).
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn render_notifications_matches_per_item_primitives(pool: PgPool) {
    let viewer = create_local_account(&pool, "viewer", "Viewer").await;
    let faver = create_local_account(&pool, "faver", "Faver").await;
    let follower = create_local_account(&pool, "follower", "Follower").await;
    let reactor = create_local_account(&pool, "reactor", "Reactor").await;
    let own = status::create_local(
        &pool,
        NewLocalStatus::new(viewer.id, "my post", "public", None),
    )
    .await
    .unwrap();
    notification::create(&pool, viewer.id, faver.id, "favourite", Some(own.id))
        .await
        .unwrap();
    notification::create(&pool, viewer.id, follower.id, "follow", None)
        .await
        .unwrap();
    notification::create_reaction(&pool, viewer.id, reactor.id, own.id, "🦊")
        .await
        .unwrap();

    let items = notification::list(&pool, viewer.id, None, None, None, NO_FILTER, 40)
        .await
        .unwrap();
    assert_eq!(items.len(), 3);
    let rendered = entities::render_notifications(&pool, "plamenu.test", &items, viewer.id)
        .await
        .unwrap();

    for (item, entity) in items.iter().zip(&rendered) {
        assert_eq!(entity["id"], serde_json::json!(item.id.to_string()));
        assert_eq!(entity["type"], serde_json::json!(item.kind));
        let from = plamenu_db::account::find_by_id(&pool, item.from_account_id)
            .await
            .unwrap()
            .unwrap();
        let want_account = entities::account_json(&pool, "plamenu.test", &from, Some(viewer.id))
            .await
            .unwrap();
        assert_eq!(
            entity["account"], want_account,
            "batched sender render diverged from account_json ({})",
            item.kind
        );
        if matches!(item.kind.as_str(), "favourite" | "pleroma:emoji_reaction") {
            let want_status = entities::render_status(&pool, "plamenu.test", &own, Some(viewer.id))
                .await
                .unwrap();
            assert_eq!(
                entity["status"], want_status,
                "batched status render diverged from render_status"
            );
        } else {
            // `status` is attached for status-bearing kinds only; the key
            // must be absent (not null) on follows.
            assert!(
                entity.get("status").is_none(),
                "unexpected status key on a {} notification",
                item.kind
            );
        }
        if item.kind == "pleroma:emoji_reaction" {
            assert_eq!(entity["emoji"], serde_json::json!("🦊"));
        } else {
            assert!(entity.get("emoji").is_none());
        }
    }
}

// ---------------------------------------------------------------------------
// The `ActivityPub` collection builders
// ---------------------------------------------------------------------------

/// Seeds `n` self-replies to `parent`, each carrying the artifacts the
/// renderer-guards above deliberately leave out: a mention, a hashtag and a
/// poll. Without them every optional branch of the Note builder is dead code
/// and a flatness assertion proves nothing about it.
async fn seed_note_page(
    pool: &PgPool,
    author: &plamenu_db::account::Account,
    parent: i64,
    n: usize,
) -> Vec<status::Status> {
    let mut out = Vec::new();
    for i in 0..n {
        let mentioned = create_local_account(pool, &format!("m{n}_{i}"), "Mentioned").await;
        let reply = status::create_local(
            pool,
            NewLocalStatus::new(author.id, "hello :party: world", "public", Some(parent)),
        )
        .await
        .unwrap();
        mention::attach(pool, reply.id, mentioned.id, false)
            .await
            .unwrap();
        let tag_id = plamenu_db::tag::ensure(pool, &format!("tag{i}"))
            .await
            .unwrap();
        plamenu_db::tag::attach(pool, reply.id, tag_id)
            .await
            .unwrap();
        plamenu_db::poll::create(
            pool,
            plamenu_db::poll::NewPoll {
                status_id: reply.id,
                account_id: author.id,
                options: &["yes".to_owned(), "no".to_owned()],
                cached_tallies: &[0, 0],
                multiple: false,
                hide_totals: false,
                voters_count: Some(0),
                expires_at: None,
            },
        )
        .await
        .unwrap();
        out.push(reply);
    }
    out
}

/// The `ActivityPub` collections that inline whole `Note` documents — the
/// outbox, the replies collection, the featured pins, the FEP-171b container —
/// used to call `note_for_status` once per row, and each of those re-issued the
/// single-id form of a batch helper that already existed: ~16 round trips per
/// row before any optional artifact, which is what made the benched replies
/// page cost 1,143 for 61 replies. `NoteBatch` must load the whole page once,
/// so the count does not move with the page length.
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn note_batch_query_count_is_flat(pool: PgPool) {
    let counter = Arc::new(AtomicUsize::new(0));
    let state = common::test_state_with(pool.clone(), Arc::default());
    let author = create_local_account(&pool, "author", "Author").await;
    let parent = status::create_local(
        &pool,
        NewLocalStatus::new(author.id, "the root", "public", None),
    )
    .await
    .unwrap();
    let small = seed_note_page(&pool, &author, parent.id, 3).await;
    let big = seed_note_page(&pool, &author, parent.id, 12).await;

    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(QueryCounter(counter.clone())),
    );
    let measure = |page: &[status::Status]| {
        let state = state.clone();
        let counter = counter.clone();
        let page: Vec<status::Status> = page.to_vec();
        async move {
            let refs: Vec<&status::Status> = page.iter().collect();
            counter.store(0, Ordering::Relaxed);
            plamenu::note::NoteBatch::load(&state, &refs).await.unwrap();
            counter.load(Ordering::Relaxed)
        }
    };
    let small_queries = measure(&small).await;
    let big_queries = measure(&big).await;

    println!(
        "NoteBatch::load: 3 rows -> {small_queries} queries, 12 rows -> {big_queries} queries"
    );
    assert!(
        big_queries <= small_queries,
        "Note page query count grew with page size: {small_queries} -> {big_queries} (N+1)"
    );
}

/// The batched builder must produce the byte-identical document the per-status
/// one does — the flatness assertion above is worthless if the page renders
/// something different from what a single dereference of the same status
/// returns.
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn note_batch_seeded_parent_preserves_other_parents_and_documents(pool: PgPool) {
    let state = common::test_state_with(pool.clone(), Arc::default());
    let author = create_local_account(&pool, "author", "Author").await;
    let other = create_local_account(&pool, "other", "Other root owner").await;
    let parent = status::create_local(
        &pool,
        NewLocalStatus::new(author.id, "root", "public", None),
    )
    .await
    .unwrap();
    let other_parent = status::create_local(
        &pool,
        NewLocalStatus::new(other.id, "other root", "public", None),
    )
    .await
    .unwrap();
    let mut page = Vec::new();
    for parent_id in [Some(parent.id), Some(other_parent.id), None] {
        page.push(
            status::create_local(
                &pool,
                NewLocalStatus::new(author.id, "<p>body</p>", "public", parent_id),
            )
            .await
            .unwrap(),
        );
    }
    let refs: Vec<&status::Status> = page.iter().collect();
    let normal = plamenu::note::NoteBatch::load(&state, &refs).await.unwrap();
    let seeded = plamenu::note::NoteBatch::load_with_parent(&state, &refs, &parent, &author)
        .await
        .unwrap();
    for item in &page {
        assert_eq!(
            plamenu::note::note_in_batch(&state, item, &author, &seeded).unwrap(),
            plamenu::note::note_in_batch(&state, item, &author, &normal).unwrap(),
            "only the supplied parent's identity is reused; every document stays identical",
        );
    }
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn note_batch_matches_per_status_render(pool: PgPool) {
    let state = common::test_state_with(pool.clone(), Arc::default());
    let author = create_local_account(&pool, "author", "Author").await;
    let parent = status::create_local(
        &pool,
        NewLocalStatus::new(author.id, "the root", "public", None),
    )
    .await
    .unwrap();
    let mut page = seed_note_page(&pool, &author, parent.id, 3).await;
    // A top-level post and a direct message alongside the replies, so the
    // parent arm, the visibility split behind the mention query and the
    // no-artifact row are all on the same page.
    page.push(
        status::create_local(
            &pool,
            NewLocalStatus::new(author.id, "plain and alone", "public", None),
        )
        .await
        .unwrap(),
    );
    page.push(
        status::create_local(
            &pool,
            NewLocalStatus::new(author.id, "quiet word", "direct", None),
        )
        .await
        .unwrap(),
    );

    let refs: Vec<&status::Status> = page.iter().collect();
    let batch = plamenu::note::NoteBatch::load(&state, &refs).await.unwrap();
    for item in &page {
        let batched = plamenu::note::note_in_batch(&state, item, &author, &batch).unwrap();
        let single = plamenu::note::note_for_status(&state, item, &author)
            .await
            .unwrap();
        assert_eq!(
            batched, single,
            "batched Note diverged from note_for_status for status {}",
            item.id
        );
    }
}

/// N+1 close-out: filing the mention
/// notifications for a post costs a statement count independent of how many
/// accounts are mentioned. Both widths seed recipients in the policy arms
/// that trigger every conditional query (a not-followers policy, a
/// private-mention policy), so the two measurements exercise identical
/// branches; the per-recipient pipeline this replaced cost 6-8 statements
/// *per recipient*.
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn mention_filing_query_count_is_independent_of_recipient_count(pool: PgPool) {
    use plamenu_db::notification_policy::{Disposition, Policy};

    let counter = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(QueryCounter(counter.clone())),
    );

    let author = create_local_account(&pool, "author", "Author").await;
    let seed = |n: usize| {
        let pool = pool.clone();
        async move {
            let mut ids = Vec::new();
            for i in 0..n {
                let account = create_local_account(&pool, &format!("m{n}_{i}"), "Mentioned").await;
                // The first two recipients of each set enable the two
                // conditional policy arms so both measurements take the same
                // branches.
                let policy = match i {
                    0 => Policy {
                        for_not_followers: Disposition::Filter,
                        ..Policy::default()
                    },
                    1 => Policy {
                        for_private_mentions: Disposition::Filter,
                        ..Policy::default()
                    },
                    _ => Policy::default(),
                };
                plamenu_db::notification_policy::upsert(&pool, account.id, policy)
                    .await
                    .unwrap();
                ids.push(account.id);
            }
            ids
        }
    };
    let small = seed(3).await;
    let big = seed(12).await;

    let status = plamenu_db::status::create_local(
        &pool,
        plamenu_db::status::NewLocalStatus::new(author.id, "<p>hi</p>", "public", None),
    )
    .await
    .unwrap();

    let measure = |recipients: Vec<i64>| {
        let pool = pool.clone();
        let counter = counter.clone();
        let author = author.id;
        let status = status.id;
        async move {
            counter.store(0, Ordering::Relaxed);
            notification::create_mentions_many(&pool, &recipients, author, status)
                .await
                .unwrap();
            counter.load(Ordering::Relaxed)
        }
    };

    let small_queries = measure(small).await;
    let big_queries = measure(big).await;

    println!("create_mentions_many: 3 recipients -> {small_queries} queries, 12 -> {big_queries}");
    assert_eq!(
        small_queries, big_queries,
        "mention filing must not scale with the recipient count (N+1 regression)"
    );
}

/// N+1 close-out, delivery worker: draining a
/// batch of same-host, same-signer jobs pays the host facts (reachability
/// row, signature preference) and the signer keys ONCE per batch, plus a
/// fixed three statements per job: the fresh domain-policy gate and
/// cancellation re-check, plus the combined success record and job completion.
/// Before the batch cache every job re-read the host and
/// signer facts too (~8 per job); if someone uncaches them the marginal cost
/// here roughly doubles and this fails.
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn delivery_drain_marginal_cost_per_job_is_constant(pool: PgPool) {
    use serde_json::json;

    // Two deliberately fresh reads plus one success/completion statement.
    const PER_JOB: usize = 3;

    let counter = Arc::new(AtomicUsize::new(0));
    let state = common::test_state_with(pool.clone(), Arc::default());
    let signer = create_local_account(&pool, "signer", "Signer").await;

    // Prime the one-off lazy loads (instance settings cache) outside the
    // measured drains so both widths pay identical fixed costs.
    plamenu_db::job::enqueue(
        &pool,
        signer.id,
        "https://warmup.example/inbox",
        &json!({"id": "warmup", "type": "Follow"}),
    )
    .await
    .unwrap();
    assert_eq!(plamenu::delivery::run_due(&state).await, 1);

    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(QueryCounter(counter.clone())),
    );
    let measure = |n: usize| {
        let pool = pool.clone();
        let state = state.clone();
        let counter = counter.clone();
        let signer_id = signer.id;
        async move {
            for i in 0..n {
                plamenu_db::job::enqueue(
                    &pool,
                    signer_id,
                    "https://peer.example/inbox",
                    &json!({"id": format!("{n}-{i}"), "type": "Follow"}),
                )
                .await
                .unwrap();
            }
            counter.store(0, Ordering::Relaxed);
            assert_eq!(plamenu::delivery::run_due(&state).await, n as u64);
            counter.load(Ordering::Relaxed)
        }
    };

    let small_queries = measure(3).await;
    let big_queries = measure(12).await;

    println!("delivery drain: 3 jobs -> {small_queries} queries, 12 jobs -> {big_queries}");
    assert_eq!(
        big_queries - small_queries,
        (12 - 3) * PER_JOB,
        "delivery marginal cost changed: {small_queries} -> {big_queries} \
         (cached per-host/per-signer reads leaking back to per-job?)"
    );
}

/// N+1 close-out, crawl worker: draining a claimed batch loads the per-job
/// skip context (statuses, cards, media, quotes, mentions) once per batch;
/// per job only the local-status `source_of` read and the job completion
/// remain. Before the batch context every job issued the whole set (~6 per
/// job even for a linkless post).
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn link_crawl_drain_marginal_cost_per_job_is_constant(pool: PgPool) {
    // Per job: `source_of` (the local-status link scan) + `complete_crawl`.
    const PER_JOB: usize = 2;

    let counter = Arc::new(AtomicUsize::new(0));
    let state = common::test_state_with(pool.clone(), Arc::default());
    let author = create_local_account(&pool, "author", "Author").await;

    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(QueryCounter(counter.clone())),
    );
    let measure = |n: usize| {
        let pool = pool.clone();
        let state = state.clone();
        let counter = counter.clone();
        let author_id = author.id;
        async move {
            for _ in 0..n {
                let stored = status::create_local(
                    &pool,
                    NewLocalStatus::new(author_id, "hello world", "public", None),
                )
                .await
                .unwrap();
                preview_card::enqueue_crawl(&pool, stored.id).await.unwrap();
            }
            counter.store(0, Ordering::Relaxed);
            assert_eq!(plamenu::link_preview::run_due(&state).await, n as u64);
            counter.load(Ordering::Relaxed)
        }
    };

    let small_queries = measure(3).await;
    let big_queries = measure(12).await;

    println!("link crawl drain: 3 jobs -> {small_queries} queries, 12 jobs -> {big_queries}");
    assert_eq!(
        big_queries - small_queries,
        (12 - 3) * PER_JOB,
        "crawl marginal cost changed: {small_queries} -> {big_queries} \
         (batched context reads leaking back to per-job?)"
    );
}

/// Subscribes `viewer` to `channel` straight on the hub (no websocket),
/// returning the connection id and the receiver that keeps it registered.
fn hub_subscribe(
    state: &plamenu::AppState,
    channel: plamenu::streaming::Channel,
    viewer: i64,
) -> (u64, tokio::sync::mpsc::Receiver<String>) {
    let (sender, receiver) = tokio::sync::mpsc::channel(64);
    let connection = state.streaming.connection_id();
    state.streaming.subscribe(
        connection,
        channel,
        viewer,
        true,
        sender,
        Arc::new(tokio::sync::Notify::new()),
    );
    (connection, receiver)
}

/// Streaming status fan-out (N+1 decisions): routing one status to N
/// connected home viewers plus M list streams must cost a statement count
/// independent of both N and M. Before the viewer-set renderer every
/// recipient paid a full ~25-statement render (and every list stream a
/// second one); if a per-viewer read sneaks back in, the wide fan-out here
/// jumps by that read times nine and this fails.
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn streaming_status_fanout_query_count_is_flat_over_viewers(pool: PgPool) {
    let counter = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(QueryCounter(counter.clone())),
    );
    let state = common::test_state_with(pool.clone(), Arc::default());
    let alice = create_local_account(&pool, "alice", "Alice").await;
    // Per-viewer artifacts ride along: a poll (votes dimension) and a hashtag
    // (tag-follow audience), so the flatness claim covers the viewer maps.
    let (stored, _) = plamenu::actions::post_status(
        &state,
        plamenu::actions::PostParams {
            username: "alice",
            text: "fanned #wide",
            visibility: "public",
            poll: Some(plamenu::actions::PollParams {
                options: vec!["a".to_owned(), "b".to_owned()],
                expires_in: Some(3600),
                multiple: false,
                hide_totals: false,
            }),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let route = |viewers: usize, lists: usize, tag: &'static str| {
        let pool = pool.clone();
        let state = state.clone();
        let counter = counter.clone();
        async move {
            let mut connections = Vec::new();
            let mut receivers = Vec::new();
            for i in 0..viewers {
                let fan = create_local_account(&pool, &format!("f{tag}{i}"), "Fan").await;
                follow::create(&pool, fan.id, alice.id, None).await.unwrap();
                let (connection, receiver) =
                    hub_subscribe(&state, plamenu::streaming::Channel::User(fan.id), fan.id);
                connections.push(connection);
                receivers.push(receiver);
                if i < lists {
                    let owned = plamenu_db::list::create(&pool, fan.id, "faves", "list", false)
                        .await
                        .unwrap();
                    plamenu_db::list::add_members(&pool, owned.id, fan.id, &[alice.id])
                        .await
                        .unwrap()
                        .unwrap();
                    let (connection, receiver) =
                        hub_subscribe(&state, plamenu::streaming::Channel::List(owned.id), fan.id);
                    connections.push(connection);
                    receivers.push(receiver);
                }
            }
            counter.store(0, Ordering::Relaxed);
            plamenu::streaming::handle_event(
                &state,
                &plamenu_db::streaming::Event::StatusNew {
                    status_id: stored.id,
                },
            )
            .await
            .unwrap();
            let count = counter.load(Ordering::Relaxed);
            for connection in connections {
                state.streaming.disconnect(connection);
            }
            drop(receivers);
            count
        }
    };

    let small_queries = route(2, 1, "s").await;
    let big_queries = route(12, 3, "b").await;
    println!(
        "status fan-out: 2 viewers/1 list -> {small_queries} queries, \
         12 viewers/3 lists -> {big_queries} queries"
    );
    assert!(
        big_queries <= small_queries + 2,
        "streaming fan-out query count grew with viewer count: \
         {small_queries} -> {big_queries} (per-viewer render leaking back?)"
    );
}

/// Streaming announcement fan-out (N+1 decisions): publishing to N
/// connected viewers must cost a statement count independent of N — one
/// anonymous reaction pass plus the batched per-viewer rows, one viewer-set
/// entity render (the cited status included). It used to be ≥5 statements
/// per connected viewer.
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn streaming_announcement_fanout_query_count_is_flat_over_viewers(pool: PgPool) {
    let counter = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(QueryCounter(counter.clone())),
    );
    let state = common::test_state_with(pool.clone(), Arc::default());
    create_local_account(&pool, "alice", "Alice").await;
    let (cited, _) = plamenu::actions::post_status(
        &state,
        plamenu::actions::PostParams {
            username: "alice",
            text: "the cited post",
            visibility: "public",
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let ann = plamenu_db::announcement::create(
        &pool,
        plamenu_db::announcement::NewAnnouncement {
            text: "hello everyone",
            scheduled_at: None,
            starts_at: None,
            ends_at: None,
            all_day: false,
            status_ids: Some(&[cited.id]),
        },
    )
    .await
    .unwrap();

    let route = |viewers: usize, tag: &'static str| {
        let pool = pool.clone();
        let state = state.clone();
        let counter = counter.clone();
        let ann_id = ann.id;
        async move {
            let mut connections = Vec::new();
            let mut receivers = Vec::new();
            for i in 0..viewers {
                let member = create_local_account(&pool, &format!("m{tag}{i}"), "Member").await;
                let (connection, receiver) = hub_subscribe(
                    &state,
                    plamenu::streaming::Channel::User(member.id),
                    member.id,
                );
                connections.push(connection);
                receivers.push(receiver);
            }
            counter.store(0, Ordering::Relaxed);
            plamenu::streaming::handle_event(
                &state,
                &plamenu_db::streaming::Event::Announcement {
                    announcement_id: ann_id,
                },
            )
            .await
            .unwrap();
            let count = counter.load(Ordering::Relaxed);
            for connection in connections {
                state.streaming.disconnect(connection);
            }
            drop(receivers);
            count
        }
    };

    let small_queries = route(2, "s").await;
    let big_queries = route(12, "b").await;
    println!(
        "announcement fan-out: 2 viewers -> {small_queries} queries, \
         12 viewers -> {big_queries} queries"
    );
    assert!(
        big_queries <= small_queries + 2,
        "announcement fan-out query count grew with viewer count: \
         {small_queries} -> {big_queries} (per-viewer loads leaking back?)"
    );
}

/// N+1 close-out, domain-block severance:
/// the queued job severs a domain's relationships with a statement count
/// independent of how many the user had there. Before the job, `block_domain`
/// paid ~5-6 statements per relationship on the request path; if a per-edge
/// read or delete sneaks back into the worker, the wide domain here costs 10
/// edges more and this fails.
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn domain_severance_query_count_is_flat_over_relationship_count(pool: PgPool) {
    let counter = Arc::new(AtomicUsize::new(0));
    let state = common::test_state_with(pool.clone(), Arc::default());
    let blocker = create_local_account(&pool, "blocker", "Blocker").await;

    let seed = |domain: &'static str, n: usize| {
        let pool = pool.clone();
        let blocker = blocker.id;
        async move {
            for i in 0..n {
                let other = remote_account(&pool, domain, &format!("u{i}")).await;
                // Both directions, with URIs, so every edge builds a retraction.
                follow::create_outgoing(
                    &pool,
                    blocker,
                    other.id,
                    &format!("https://plamenu.test/users/blocker#follows/{domain}-{i}"),
                )
                .await
                .unwrap();
                follow::create(
                    &pool,
                    other.id,
                    blocker,
                    Some(&format!("https://{domain}/users/u{i}#follows/blocker")),
                )
                .await
                .unwrap();
            }
        }
    };
    seed("small.example", 2).await;
    seed("big.example", 12).await;

    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(QueryCounter(counter.clone())),
    );
    let measure = |domain: &'static str| {
        let state = state.clone();
        let counter = counter.clone();
        let blocker = blocker.id;
        async move {
            plamenu_db::domain_severance_job::enqueue(&state.pool, blocker, domain)
                .await
                .unwrap();
            counter.store(0, Ordering::Relaxed);
            assert_eq!(plamenu::severance::run_due(&state).await, 1);
            counter.load(Ordering::Relaxed)
        }
    };

    let small_queries = measure("small.example").await;
    let big_queries = measure("big.example").await;

    println!("domain severance: 4 edges -> {small_queries} queries, 24 edges -> {big_queries}");
    assert_eq!(
        small_queries, big_queries,
        "severance must not scale with the relationship count (N+1 regression)"
    );
}
