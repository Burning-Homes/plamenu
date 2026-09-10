//! Integration tests for the full account archive export —
//! Mastodon's `BackupService` / `BackupWorker`. The build produces a
//! self-contained zip (actor/outbox/likes/bookmarks JSON plus avatar and
//! attached media, with URLs rewritten to relative in-zip paths), and the web
//! flow requests one, enforces the per-account rate limit, and streams the
//! finished download.

mod common;

use std::collections::HashMap;
use std::io::{Read as _, SeekFrom};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{create_local_account, test_state_with, test_state_with_store};
use http_body_util::BodyExt;
use plamenu::AppState;
use plamenu::federation::BoxFuture;
use plamenu::storage::{MediaRead, MediaStore};
use plamenu_db::status::NewLocalStatus;
use plamenu_db::{
    PgPool, account, archive, bookmark, favourite, id, instance_settings, media, status, user,
};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncSeek, ReadBuf};
use tower::ServiceExt;

const EMAIL: &str = "alice@example.com";
const PASSWORD: &str = "correct horse battery";

// ---- Helpers -----------------------------------------------------------

fn state_and_app(pool: PgPool) -> (AppState, Router) {
    let state = test_state_with(pool, Arc::default());
    let app = plamenu::build_router(state.clone());
    (state, app)
}

async fn seed_alice(pool: &PgPool) -> account::Account {
    let account = create_local_account(pool, "alice", "Alice").await;
    let hash = plamenu::auth::hash_password(PASSWORD).unwrap();
    user::create(pool, account.id, Some(EMAIL), &hash)
        .await
        .unwrap();
    account
}

/// Unzips archive bytes into a `filename → contents` map.
fn unzip(bytes: &[u8]) -> HashMap<String, Vec<u8>> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("valid zip");
    let mut files = HashMap::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).unwrap();
        let name = entry.name().to_owned();
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf).unwrap();
        files.insert(name, buf);
    }
    files
}

fn json_of(files: &HashMap<String, Vec<u8>>, name: &str) -> Value {
    serde_json::from_slice(files.get(name).expect("stream present")).expect("valid JSON")
}

// ---- Tests -------------------------------------------------------------

#[sqlx::test(migrations = "../db/migrations")]
async fn build_bundles_streams_media_and_avatar(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let (state, _app) = state_and_app(pool.clone());

    // Alice posts, with an image attached.
    let post = status::create_local(
        &pool,
        NewLocalStatus::new(alice.id, "<p>hello world</p>", "public", None),
    )
    .await
    .unwrap();
    let media_id = id::next();
    media::create_local(
        &pool,
        media::NewLocalMedia::new(alice.id, media_id, "pic-123.png", "image/png"),
    )
    .await
    .unwrap();
    media::attach(&pool, &[media_id], post.id, alice.id)
        .await
        .unwrap();
    state
        .media
        .put("pic-123.png", b"PNGDATA".to_vec())
        .await
        .unwrap();

    // Alice favourites and bookmarks one of Bob's posts.
    let bobs = status::create_local(
        &pool,
        NewLocalStatus::new(bob.id, "<p>bob speaks</p>", "public", None),
    )
    .await
    .unwrap();
    favourite::create(&pool, alice.id, bobs.id, None)
        .await
        .unwrap();
    bookmark::create(&pool, alice.id, bobs.id).await.unwrap();

    // Alice has an avatar on file.
    sqlx::query!(
        "UPDATE accounts SET avatar_file_name = 'ava-9.jpg' WHERE id = $1",
        alice.id
    )
    .execute(&pool)
    .await
    .unwrap();
    state
        .media
        .put("ava-9.jpg", b"JPEGDATA".to_vec())
        .await
        .unwrap();

    let alice = account::find_by_id(&pool, alice.id).await.unwrap().unwrap();
    let temp = plamenu::archive::build(&state, &alice).await.unwrap();
    let files = unzip(&std::fs::read(temp.path()).unwrap());

    // Every stream and referenced file is present.
    for name in [
        "actor.json",
        "outbox.json",
        "likes.json",
        "bookmarks.json",
        "media_attachments/pic-123.png",
        "avatar.jpg",
    ] {
        assert!(files.contains_key(name), "missing {name}");
    }
    assert_eq!(files["media_attachments/pic-123.png"], b"PNGDATA");
    assert_eq!(files["avatar.jpg"], b"JPEGDATA");

    // actor.json points its collections and icon at the in-zip files.
    let actor = json_of(&files, "actor.json");
    assert_eq!(actor["outbox"], "outbox.json");
    assert_eq!(actor["likes"], "likes.json");
    assert_eq!(actor["bookmarks"], "bookmarks.json");
    assert_eq!(actor["icon"]["url"], "avatar.jpg");

    // outbox.json wraps Alice's post as a Create with a rewritten attachment.
    let outbox = json_of(&files, "outbox.json");
    assert_eq!(outbox["type"], "OrderedCollection");
    assert_eq!(outbox["totalItems"], 1);
    let item = &outbox["orderedItems"][0];
    assert_eq!(item["type"], "Create");
    assert!(
        item["object"]["content"]
            .as_str()
            .unwrap()
            .contains("hello world")
    );
    assert_eq!(
        item["object"]["attachment"][0]["url"],
        "media_attachments/pic-123.png"
    );

    // likes/bookmarks reference Bob's post by URI (derived for local statuses).
    let likes = json_of(&files, "likes.json");
    let bookmarks = json_of(&files, "bookmarks.json");
    let bob_ref = bobs.id.to_string();
    assert!(
        likes["orderedItems"][0]
            .as_str()
            .unwrap()
            .contains(&bob_ref)
    );
    assert!(
        bookmarks["orderedItems"][0]
            .as_str()
            .unwrap()
            .contains(&bob_ref)
    );
}

/// The build streams media into the zip through the store's
/// seekable `open` reader in fixed slices, so peak resident memory is one copy
/// buffer — not the whole file — no matter how large the attachment is, and the
/// buffering `get` path is never taken.
#[sqlx::test(migrations = "../db/migrations")]
async fn build_streams_media_without_buffering_it(pool: PgPool) {
    // Far larger than the 64 KiB copy buffer, so a buffering implementation
    // would show up as a multi-megabyte single read.
    const PER_FILE: u64 = 2 * 1024 * 1024;

    let alice = seed_alice(&pool).await;
    let post = status::create_local(
        &pool,
        NewLocalStatus::new(alice.id, "<p>hi</p>", "public", None),
    )
    .await
    .unwrap();
    let media_id = id::next();
    media::create_local(
        &pool,
        media::NewLocalMedia::new(alice.id, media_id, "big-attach.png", "image/png"),
    )
    .await
    .unwrap();
    media::attach(&pool, &[media_id], post.id, alice.id)
        .await
        .unwrap();
    sqlx::query!(
        "UPDATE accounts SET avatar_file_name = 'big-ava.jpg' WHERE id = $1",
        alice.id
    )
    .execute(&pool)
    .await
    .unwrap();
    let alice = account::find_by_id(&pool, alice.id).await.unwrap().unwrap();

    let peak_chunk = Arc::new(AtomicUsize::new(0));
    let get_calls = Arc::new(AtomicUsize::new(0));
    let store = Arc::new(SparseStore {
        len: PER_FILE,
        peak_chunk: peak_chunk.clone(),
        get_calls: get_calls.clone(),
    });
    let state = test_state_with_store(pool.clone(), Arc::default(), store);

    let temp = plamenu::archive::build(&state, &alice).await.unwrap();
    let files = unzip(&std::fs::read(temp.path()).unwrap());

    // Both media files were streamed in full (byte length matches).
    assert_eq!(
        files["media_attachments/big-attach.png"].len() as u64,
        PER_FILE
    );
    assert_eq!(files["avatar.jpg"].len() as u64, PER_FILE);

    // The largest single read never exceeded the copy buffer — no whole-file
    // allocation — and the buffering get() path was never taken.
    let peak = peak_chunk.load(Ordering::Relaxed);
    assert!(peak <= 64 * 1024, "largest single read was {peak} bytes");
    assert_eq!(
        get_calls.load(Ordering::Relaxed),
        0,
        "the build must stream via open(), never buffer via get()"
    );
}

/// Bundled media is capped; a file that would push the archive over
/// the limit is omitted (never read) and a note records it, while the metadata
/// streams complete.
#[sqlx::test(migrations = "../db/migrations")]
async fn build_omits_media_over_the_size_cap(pool: PgPool) {
    const PER_FILE: u64 = 4 * 1024 * 1024; // each media file
    const CAP: u64 = 1024 * 1024; // 1 MiB — smaller than one file

    let alice = seed_alice(&pool).await;
    let post = status::create_local(
        &pool,
        NewLocalStatus::new(alice.id, "<p>hi</p>", "public", None),
    )
    .await
    .unwrap();
    let media_id = id::next();
    media::create_local(
        &pool,
        media::NewLocalMedia::new(alice.id, media_id, "huge.png", "image/png"),
    )
    .await
    .unwrap();
    media::attach(&pool, &[media_id], post.id, alice.id)
        .await
        .unwrap();
    let alice = account::find_by_id(&pool, alice.id).await.unwrap().unwrap();

    let peak_chunk = Arc::new(AtomicUsize::new(0));
    let get_calls = Arc::new(AtomicUsize::new(0));
    let store = Arc::new(SparseStore {
        len: PER_FILE,
        peak_chunk: peak_chunk.clone(),
        get_calls: get_calls.clone(),
    });
    let state = test_state_with_store(pool.clone(), Arc::default(), store);

    let temp = plamenu::archive::build_with_media_limit(&state, &alice, CAP)
        .await
        .unwrap();
    let files = unzip(&std::fs::read(temp.path()).unwrap());

    // The oversize attachment is omitted; a note names the count.
    assert!(
        !files.contains_key("media_attachments/huge.png"),
        "media over the cap must be omitted"
    );
    let note = String::from_utf8(files["README-omitted-media.txt"].clone()).unwrap();
    assert!(
        note.contains("omitted 1 media"),
        "note records the omission: {note}"
    );

    // Metadata is still complete, and the omitted file's bytes were never read
    // (its length alone was enough to reject it) nor buffered via get().
    assert!(files.contains_key("outbox.json"));
    assert_eq!(
        peak_chunk.load(Ordering::Relaxed),
        0,
        "an omitted file is never read"
    );
    assert_eq!(get_calls.load(Ordering::Relaxed), 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn request_is_rate_limited_and_download_streams_zip(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let (state, app) = state_and_app(pool.clone());
    let cookie = login(&app).await;

    // The export page offers a request button with a CSRF token.
    let page = get(&app, "/settings/export", &cookie).await;
    assert!(page.body.contains("Request archive"));
    let csrf = csrf_of(&page.body);

    // First request is accepted and queued.
    let first = post_archive(&app, &cookie, &csrf).await;
    assert_eq!(first.status, StatusCode::SEE_OTHER);
    assert_eq!(
        first.location.as_deref(),
        Some("/settings/export?saved=archive")
    );
    let queued = archive::recent_for_account(&pool, alice.id, 5)
        .await
        .unwrap();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].state, "scheduled");

    // A second request within the window is refused by the rate gate.
    let second = post_archive(&app, &cookie, &csrf).await;
    assert_eq!(
        second.location.as_deref(),
        Some("/settings/export?error=archive_rate")
    );
    assert_eq!(
        archive::recent_for_account(&pool, alice.id, 5)
            .await
            .unwrap()
            .len(),
        1
    );

    // The worker builds it; the row goes finished with a stored file.
    assert_eq!(plamenu::archive_worker::run_due(&state).await, 1);
    let built = &archive::recent_for_account(&pool, alice.id, 5)
        .await
        .unwrap()[0];
    assert!(built.is_ready());
    let id = built.id;

    // With one archive on file the section swaps the request button for the
    // cooldown sentence, and lists the archive with its state, date and size.
    let listed = get(&app, "/settings/export", &cookie).await;
    assert!(!listed.body.contains("Request archive"));
    assert!(
        listed
            .body
            .contains("You can request a new archive once every 6 days.")
    );
    assert!(listed.body.contains("Next available "));
    assert!(listed.body.contains("Archive · Ready"));
    assert!(
        listed.body.contains(" KB") || listed.body.contains(" MB") || listed.body.contains(" B"),
        "the archive's size is shown: {}",
        listed.body
    );

    // The owner can download the zip. It is streamed from the store (finding
    // #45): a range-capable, private, attachment response — never a buffered
    // whole-file copy.
    let download = get(&app, &format!("/settings/archive/{id}/download"), &cookie).await;
    assert_eq!(download.status, StatusCode::OK);
    assert_eq!(download.content_type.as_deref(), Some("application/zip"));
    assert_eq!(download.accept_ranges.as_deref(), Some("bytes"));
    assert_eq!(download.cache_control.as_deref(), Some("no-store"));
    assert_eq!(
        download.content_disposition.as_deref(),
        Some(format!("attachment; filename=\"archive-{id}.zip\"").as_str())
    );
    assert!(download.body_bytes.starts_with(b"PK"), "a zip archive");
    let full_len = download.body_bytes.len();

    // A `Range` request is honoured with a 206 and just the requested slice, so
    // a large archive can be resumed/streamed rather than re-buffered whole.
    let ranged = get_range(
        &app,
        &format!("/settings/archive/{id}/download"),
        &cookie,
        "bytes=0-1",
    )
    .await;
    assert_eq!(ranged.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        ranged.content_range.as_deref(),
        Some(format!("bytes 0-1/{full_len}").as_str())
    );
    assert_eq!(ranged.body_bytes, b"PK", "the first two bytes of the zip");

    // A stranger cannot download it.
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let hash = plamenu::auth::hash_password(PASSWORD).unwrap();
    user::create(&pool, bob.id, Some("bob@example.com"), &hash)
        .await
        .unwrap();
    let bob_cookie = login_as(&app, "bob@example.com").await;
    let forbidden = get(
        &app,
        &format!("/settings/archive/{id}/download"),
        &bob_cookie,
    )
    .await;
    assert_eq!(forbidden.status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn concurrent_archive_requests_schedule_one(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let (_state, app) = state_and_app(pool.clone());
    let cookie = login(&app).await;
    let page = get(&app, "/settings/export", &cookie).await;
    let csrf = csrf_of(&page.body);

    // Fire several archive requests at once with a valid session + CSRF token.
    // Each hits its own pooled connection, so the account-scoped advisory lock
    // behind the gate (finding #44) is genuinely contended; exactly one must be
    // admitted and the rest refused. A racy check-then-create would let several
    // full-archive builds queue for a single account.
    let request = || post_archive(&app, &cookie, &csrf);
    let results = tokio::join!(
        request(),
        request(),
        request(),
        request(),
        request(),
        request(),
    );
    let locations = [
        results.0.location,
        results.1.location,
        results.2.location,
        results.3.location,
        results.4.location,
        results.5.location,
    ];
    let (mut saved, mut refused) = (0, 0);
    for location in locations {
        match location.as_deref() {
            Some("/settings/export?saved=archive") => saved += 1,
            Some("/settings/export?error=archive_rate") => refused += 1,
            other => panic!("unexpected archive redirect: {other:?}"),
        }
    }
    assert_eq!(saved, 1, "exactly one concurrent request is admitted");
    assert_eq!(refused, 5);
    assert_eq!(
        archive::recent_for_account(&pool, alice.id, 100)
            .await
            .unwrap()
            .len(),
        1
    );
}

/// Finding #44: the 6-day cooldown lets exactly one archive build, but a client
/// can still hammer the request route, and each attempt takes the account-scoped
/// advisory lock + a transaction before the cooldown refuses it. A per-account
/// admission budget stops the spam earlier: once it is spent the route returns
/// `429` *before* any DB work, while a single archive stays scheduled throughout.
#[sqlx::test(migrations = "../db/migrations")]
async fn archive_requests_are_admission_limited_per_account(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let (_state, app) = state_and_app(pool.clone());
    let cookie = login(&app).await;
    let page = get(&app, "/settings/export", &cookie).await;
    let csrf = csrf_of(&page.body);

    // Spend the whole budget: the first schedules the archive, the rest are
    // refused by the cooldown, but every one counts against the admission budget.
    let budget = plamenu::rate_limit::WEB_MAINTENANCE_BUDGET;
    for _ in 0..budget {
        let resp = post_archive(&app, &cookie, &csrf).await;
        assert_eq!(
            resp.status,
            StatusCode::SEE_OTHER,
            "within budget: a redirect (scheduled or cooldown-refused)",
        );
    }

    // The next request is refused by the admission budget with a 429 — no
    // redirect, so it never reached the cooldown's advisory lock/DB path.
    let over = post_archive(&app, &cookie, &csrf).await;
    assert_eq!(over.status, StatusCode::TOO_MANY_REQUESTS);

    // The cooldown held throughout: exactly one archive was ever scheduled.
    assert_eq!(
        archive::recent_for_account(&pool, alice.id, 100)
            .await
            .unwrap()
            .len(),
        1
    );
}

/// Finding #43: the self-destruct wind-down page tells users they can still
/// sign in and download an archive of their data from the export page, so the
/// two full-archive routes must survive the `410` gate — a build request and the
/// finished download — while every other settings surface, imports included,
/// stays gated. The archive worker keeps running during wind-down, so an archive
/// requested now is finished and downloadable before teardown.
#[sqlx::test(migrations = "../db/migrations")]
async fn archive_request_and_download_survive_the_self_destruct_gate(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    // Arm self-destruct before the first request, so the settings cache reads the
    // armed state on its first (lazy) miss — the wind-down gate is now up.
    instance_settings::begin_self_destruct(&pool).await.unwrap();
    let (state, app) = state_and_app(pool.clone());

    // Signing in and viewing the export page stay open (existing allowlist).
    let cookie = login(&app).await;
    let page = get(&app, "/settings/export", &cookie).await;
    assert_eq!(page.status, StatusCode::OK, "export page stays reachable");
    let csrf = csrf_of(&page.body);

    // Requesting a full archive is accepted despite the gate.
    let requested = post_archive(&app, &cookie, &csrf).await;
    assert_eq!(
        requested.location.as_deref(),
        Some("/settings/export?saved=archive"),
        "the archive request survives the wind-down gate"
    );

    // The archive worker still runs during wind-down and builds the ZIP.
    assert_eq!(plamenu::archive_worker::run_due(&state).await, 1);
    let built = &archive::recent_for_account(&pool, alice.id, 5)
        .await
        .unwrap()[0];
    assert!(built.is_ready());
    let id = built.id;

    // ...and the owner can download it through the gate.
    let download = get(&app, &format!("/settings/archive/{id}/download"), &cookie).await;
    assert_eq!(download.status, StatusCode::OK);
    assert_eq!(download.content_type.as_deref(), Some("application/zip"));
    assert!(download.body_bytes.starts_with(b"PK"), "a zip archive");

    // The allowlist opened only the two archive routes: imports and every other
    // settings mutation are still refused with the `410` wind-down notice.
    let import_read = get(&app, "/settings/import/1", &cookie).await;
    assert_eq!(
        import_read.status,
        StatusCode::GONE,
        "the import page stays gated"
    );
    let import_confirm = post_empty(&app, "/web/settings/import/1/confirm", &cookie).await;
    assert_eq!(
        import_confirm.status,
        StatusCode::GONE,
        "an import mutation stays gated"
    );
    let cleanup = post_empty(&app, "/web/settings/statuses-cleanup", &cookie).await;
    assert_eq!(
        cleanup.status,
        StatusCode::GONE,
        "an unrelated settings mutation stays gated"
    );
}

/// Finding #45: a finished archive is streamed from the store with `Range`
/// support, never buffered whole. This proves the resident-memory bound the
/// prose promises: a multi-gigabyte archive is served without the process ever
/// holding a copy of it, and several concurrent downloads each cost only a small
/// streaming chunk rather than another whole-archive allocation.
///
/// The store reports a 6 GiB archive whose bytes are generated lazily; if the
/// download path buffered the file (the old `MediaStore::get` → `Vec<u8>` shape)
/// it would try to allocate 6 GiB and OOM the test. Instead the size comes from
/// metadata and the body streams in small chunks: the test asserts the download
/// never calls the buffering `get`, that the largest single read stays kilobytes
/// (not gigabytes), and that four concurrent downloads all succeed.
#[sqlx::test(migrations = "../db/migrations")]
async fn download_streams_a_huge_archive_without_buffering_it(pool: PgPool) {
    const HUGE: u64 = 6 * 1024 * 1024 * 1024; // 6 GiB — far beyond any buffer

    let alice = seed_alice(&pool).await;
    let peak_chunk = Arc::new(AtomicUsize::new(0));
    let get_calls = Arc::new(AtomicUsize::new(0));
    let store = Arc::new(SparseStore {
        len: HUGE,
        peak_chunk: peak_chunk.clone(),
        get_calls: get_calls.clone(),
    });
    let state = test_state_with_store(pool.clone(), Arc::default(), store);
    let app = plamenu::build_router(state.clone());
    let cookie = login(&app).await;

    // A finished archive whose stored file is the 6 GiB sparse object.
    let archive = archive::create(&pool, alice.id).await.unwrap();
    archive::mark_finished(&pool, archive.id, "big.zip", i64::try_from(HUGE).unwrap())
        .await
        .unwrap();
    let uri = format!("/settings/archive/{}/download", archive.id);

    // GET the whole archive. The response headers come back immediately — its
    // `Content-Length` is the full 6 GiB, taken from the store's metadata, not
    // from reading (and allocating) the file.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(&uri)
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_LENGTH).unwrap(),
        HUGE.to_string().as_str(),
    );
    assert_eq!(
        response.headers().get(header::ACCEPT_RANGES).unwrap(),
        "bytes"
    );

    // Drain only a small prefix of the streamed body, then drop it. A buffering
    // implementation could not have produced these bytes without first holding
    // the whole file; a streaming one yields small chunks.
    let mut body = response.into_body();
    let mut drained = 0u64;
    while drained < 64 * 1024 {
        let Some(frame) = body.frame().await else {
            break;
        };
        if let Some(data) = frame.unwrap().data_ref() {
            drained += data.len() as u64;
        }
    }
    drop(body); // cancel the rest of the 6 GiB stream

    // The largest single read the store ever served is a streaming chunk —
    // kilobytes, independent of the 6 GiB file — and the buffering `get` path
    // was never taken.
    let peak = peak_chunk.load(Ordering::Relaxed);
    assert!(peak > 0, "the body was actually read");
    assert!(
        peak < 1024 * 1024,
        "a single read buffered {peak} bytes; the whole file must never be resident",
    );
    assert_eq!(
        get_calls.load(Ordering::Relaxed),
        0,
        "the download must stream via open(), never buffer via get()",
    );

    // Four concurrent downloads each seek and stream their own small slice; none
    // allocates the archive, so resident memory stays O(chunk × downloads), not
    // O(file × downloads). Each asks for two bytes at a distinct offset.
    let ranged = |offset: u64| {
        let app = app.clone();
        let cookie = cookie.clone();
        let uri = uri.clone();
        async move {
            get_range(
                &app,
                &uri,
                &cookie,
                &format!("bytes={offset}-{}", offset + 1),
            )
            .await
        }
    };
    let results = tokio::join!(ranged(0), ranged(1024), ranged(1 << 20), ranged(HUGE - 2));
    for (slice, offset) in [
        (results.0, 0),
        (results.1, 1024),
        (results.2, 1 << 20),
        (results.3, HUGE - 2),
    ] {
        assert_eq!(slice.status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            slice.content_range.as_deref(),
            Some(format!("bytes {offset}-{}/{HUGE}", offset + 1).as_str()),
        );
        assert_eq!(slice.body_bytes.len(), 2);
    }
    assert_eq!(
        get_calls.load(Ordering::Relaxed),
        0,
        "concurrent ranged downloads must stream too",
    );
}

/// Account self-deletion cancels every archive of the account and
/// durably schedules its stored ZIP for removal, so a private backup cannot
/// outlive the account it belongs to. A scheduled build never runs, a finished
/// one's row is gone, and the cleanup worker deletes the file that was
/// previously retrievable.
#[sqlx::test(migrations = "../db/migrations")]
async fn self_delete_cancels_archives_and_schedules_file_cleanup(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let (state, _app) = state_and_app(pool.clone());

    // Alice has a finished archive with a stored file, plus a still-scheduled one.
    let finished = archive::create(&pool, alice.id).await.unwrap();
    archive::mark_finished(&pool, finished.id, "alice-archive.zip", 4)
        .await
        .unwrap();
    state
        .media
        .put("alice-archive.zip", b"PK\x03\x04".to_vec())
        .await
        .unwrap();
    archive::create(&pool, alice.id).await.unwrap(); // still scheduled

    // Self-deletion cancels every archive as part of the purge transaction.
    plamenu::moderation::self_delete_account(&state, &alice)
        .await
        .unwrap();
    assert!(
        archive::recent_for_account(&pool, alice.id, 10)
            .await
            .unwrap()
            .is_empty(),
        "no archive row survives the deletion"
    );

    // The finished archive's private ZIP is present until the cleanup worker
    // drains the queue, then gone — a formerly retrievable file no longer serves.
    assert!(state.media.get("alice-archive.zip").await.is_ok());
    while plamenu::media_cleanup_worker::run_due(&state).await > 0 {}
    assert!(
        state.media.get("alice-archive.zip").await.is_err(),
        "the private ZIP is removed from the store"
    );
}

/// If account deletion removes an archive's row while the worker
/// is mid-build, the conditional finish loses the race and the worker deletes
/// the object it just wrote — no untracked private ZIP is left in the store.
#[sqlx::test(migrations = "../db/migrations")]
async fn worker_deletes_its_object_when_it_loses_the_finish_race(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    // A post so the build has real content to serialize.
    status::create_local(
        &pool,
        NewLocalStatus::new(alice.id, "<p>hello</p>", "public", None),
    )
    .await
    .unwrap();

    let store = Arc::new(RaceOnPutStore {
        inner: plamenu::storage::MemoryStore::default(),
        pool: pool.clone(),
        account_id: alice.id,
        fired: AtomicBool::new(false),
    });
    let state = test_state_with_store(pool.clone(), Arc::default(), store.clone());

    // Schedule an archive and run the worker. The store lands a racing deletion
    // during the object put, so the finish sees a missing row and loses.
    archive::create(&pool, alice.id).await.unwrap();
    plamenu::archive_worker::run_due(&state).await;

    // No archive row survived, and the object the worker wrote was deleted rather
    // than orphaned as an untracked private ZIP.
    assert!(
        archive::recent_for_account(&pool, alice.id, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store.inner.list().await.unwrap().is_empty(),
        "the worker cleaned up the object it wrote for the lost race"
    );
}

/// A media store that lands a racing account deletion the instant the archive
/// worker writes its ZIP: the first `put_file` deletes the account's archive
/// rows — exactly the "row gone after the store put, before finalization" race
/// the worker must survive (finding #56) — then delegates to an inner store so
/// the object is genuinely written and must be cleaned up when the finish loses.
struct RaceOnPutStore {
    inner: plamenu::storage::MemoryStore,
    pool: PgPool,
    account_id: i64,
    fired: AtomicBool,
}

impl MediaStore for RaceOnPutStore {
    fn put(&self, file_name: &str, bytes: Vec<u8>) -> BoxFuture<'_, std::io::Result<()>> {
        self.inner.put(file_name, bytes)
    }
    fn put_file(
        &self,
        file_name: &str,
        src: &std::path::Path,
    ) -> BoxFuture<'_, std::io::Result<()>> {
        let file_name = file_name.to_owned();
        let src = src.to_path_buf();
        Box::pin(async move {
            if !self.fired.swap(true, Ordering::SeqCst) {
                let mut conn = self.pool.acquire().await.map_err(std::io::Error::other)?;
                archive::delete_for_account(&mut conn, self.account_id)
                    .await
                    .map_err(std::io::Error::other)?;
            }
            self.inner.put_file(&file_name, &src).await
        })
    }
    fn get(&self, file_name: &str) -> BoxFuture<'_, std::io::Result<Vec<u8>>> {
        self.inner.get(file_name)
    }
    fn open(&self, file_name: &str) -> BoxFuture<'_, std::io::Result<MediaRead>> {
        self.inner.open(file_name)
    }
    fn delete(&self, file_name: &str) -> BoxFuture<'_, std::io::Result<()>> {
        self.inner.delete(file_name)
    }
    fn list(&self) -> BoxFuture<'_, std::io::Result<Vec<String>>> {
        self.inner.list()
    }
}

/// A media store standing in for a multi-gigabyte archive: [`open`](MediaStore::open)
/// reports a huge length and streams zero bytes lazily (nothing is ever fully
/// allocated), recording the largest single read served. The buffering
/// [`get`](MediaStore::get) path counts its calls so a test can prove the
/// streaming download never takes it.
struct SparseStore {
    len: u64,
    peak_chunk: Arc<AtomicUsize>,
    get_calls: Arc<AtomicUsize>,
}

impl MediaStore for SparseStore {
    fn put(&self, _file_name: &str, _bytes: Vec<u8>) -> BoxFuture<'_, std::io::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn put_file(
        &self,
        _file_name: &str,
        _src: &std::path::Path,
    ) -> BoxFuture<'_, std::io::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn get(&self, _file_name: &str) -> BoxFuture<'_, std::io::Result<Vec<u8>>> {
        self.get_calls.fetch_add(1, Ordering::Relaxed);
        Box::pin(async {
            Err(std::io::Error::other(
                "the streaming download path must not buffer via get()",
            ))
        })
    }

    fn open(&self, _file_name: &str) -> BoxFuture<'_, std::io::Result<MediaRead>> {
        let read = MediaRead {
            len: self.len,
            reader: Box::new(SparseReader {
                len: self.len,
                pos: 0,
                peak_chunk: self.peak_chunk.clone(),
            }),
        };
        Box::pin(async move { Ok(read) })
    }

    fn delete(&self, _file_name: &str) -> BoxFuture<'_, std::io::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn list(&self) -> BoxFuture<'_, std::io::Result<Vec<String>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

/// A seekable reader over `len` zero bytes that never allocates them all at
/// once — it fills only what each `poll_read` asks for and records the largest
/// such fill in `peak_chunk`.
struct SparseReader {
    len: u64,
    pos: u64,
    peak_chunk: Arc<AtomicUsize>,
}

impl AsyncRead for SparseReader {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let file_left = this.len - this.pos;
        if file_left == 0 {
            return Poll::Ready(Ok(())); // EOF
        }
        let want = usize::try_from((buf.remaining() as u64).min(file_left)).unwrap();
        this.peak_chunk.fetch_max(want, Ordering::Relaxed);
        buf.initialize_unfilled_to(want);
        buf.advance(want);
        this.pos += want as u64;
        Poll::Ready(Ok(()))
    }
}

impl AsyncSeek for SparseReader {
    fn start_seek(self: Pin<&mut Self>, position: SeekFrom) -> std::io::Result<()> {
        let this = self.get_mut();
        let target = match position {
            SeekFrom::Start(n) => n,
            SeekFrom::End(n) => this.len.saturating_add_signed(n),
            SeekFrom::Current(n) => this.pos.saturating_add_signed(n),
        };
        this.pos = target.min(this.len);
        Ok(())
    }

    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<u64>> {
        Poll::Ready(Ok(self.pos))
    }
}

// ---- HTTP harness ------------------------------------------------------

struct Resp {
    status: StatusCode,
    location: Option<String>,
    content_type: Option<String>,
    content_disposition: Option<String>,
    content_range: Option<String>,
    accept_ranges: Option<String>,
    cache_control: Option<String>,
    set_cookie: Option<String>,
    body: String,
    body_bytes: Vec<u8>,
}

async fn send(app: &Router, request: Request<Body>) -> Resp {
    let response = app.clone().oneshot(request).await.unwrap();
    let header = |name: header::HeaderName| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(ToOwned::to_owned)
    };
    let status = response.status();
    let location = header(header::LOCATION);
    let content_type = header(header::CONTENT_TYPE);
    let content_disposition = header(header::CONTENT_DISPOSITION);
    let content_range = header(header::CONTENT_RANGE);
    let accept_ranges = header(header::ACCEPT_RANGES);
    let cache_control = header(header::CACHE_CONTROL);
    let set_cookie = header(header::SET_COOKIE);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Resp {
        status,
        location,
        content_type,
        content_disposition,
        content_range,
        accept_ranges,
        cache_control,
        set_cookie,
        // Fluent brackets interpolated values in directional isolates
        // (U+2068 FSI / U+2069 PDI); drop them so `body` reads like the page.
        // `body_bytes` keeps the response verbatim for the zip assertions.
        body: String::from_utf8_lossy(&bytes).replace(['\u{2068}', '\u{2069}'], ""),
        body_bytes: bytes.to_vec(),
    }
}

async fn get(app: &Router, uri: &str, cookie: &str) -> Resp {
    send(
        app,
        Request::builder()
            .uri(uri)
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

/// A `GET` carrying a single-range `Range` header, for the archive
/// resume/streaming assertions.
async fn get_range(app: &Router, uri: &str, cookie: &str, range: &str) -> Resp {
    send(
        app,
        Request::builder()
            .uri(uri)
            .header(header::COOKIE, cookie)
            .header(header::RANGE, range)
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

async fn post_archive(app: &Router, cookie: &str, csrf: &str) -> Resp {
    let body = serde_urlencoded::to_string([("csrf", csrf)]).unwrap();
    send(
        app,
        Request::builder()
            .method("POST")
            .uri("/web/settings/archive")
            .header(header::COOKIE, cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap(),
    )
    .await
}

/// A bodyless authenticated `POST`, for asserting the wind-down gate refuses a
/// route before its handler ever runs (so no CSRF/body is required).
async fn post_empty(app: &Router, uri: &str, cookie: &str) -> Resp {
    send(
        app,
        Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::COOKIE, cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

async fn login(app: &Router) -> String {
    login_as(app, EMAIL).await
}

async fn login_as(app: &Router, email: &str) -> String {
    let body = serde_urlencoded::to_string([("email", email), ("password", PASSWORD)]).unwrap();
    let resp = send(
        app,
        Request::builder()
            .method("POST")
            .uri("/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap(),
    )
    .await;
    resp.set_cookie
        .as_ref()
        .expect("login sets a cookie")
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

fn csrf_of(body: &str) -> String {
    let marker = "name=\"csrf\" value=\"";
    let start = body.find(marker).expect("a csrf field") + marker.len();
    body[start..].split('"').next().unwrap().to_owned()
}
