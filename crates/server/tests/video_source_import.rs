//! Private native source copying: quotas, explicit intent and durable fencing.
mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use plamenu::{AppState, storage::LocalDiskStore, video_source_import as service};
use plamenu_db::{PgPool, id, video_draft as drafts, video_source_import as db};
use std::sync::Arc;
use tower::ServiceExt;

struct Fixture {
    _temp: tempfile::TempDir,
    state: AppState,
    stub: Arc<common::StubFederation>,
    owner: i64,
    channel: i64,
}
async fn fixture(pool: PgPool) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let mut config = common::test_config();
    config.media_dir = temp.path().join("media");
    let stub = common::StubFederation::with_actors([]);
    let state = AppState::new(
        pool.clone(),
        config.clone(),
        stub.clone(),
        Arc::new(LocalDiskStore::new(config.media_dir).unwrap()),
    )
    .unwrap();
    let owner = common::create_local_account(&pool, "alice", "Alice").await;
    let channel = plamenu::video::create_channel(&state, &owner, "films", "Films")
        .await
        .unwrap();
    Fixture {
        _temp: temp,
        state,
        stub,
        owner: owner.id,
        channel: channel.id,
    }
}
async fn reserve(f: &Fixture, url: &str, kind: &str, cap: i64) -> i64 {
    reserve_for(f, f.owner, url, kind, cap).await
}
async fn reserve_for(f: &Fixture, owner: i64, url: &str, kind: &str, cap: i64) -> i64 {
    let id = id::next();
    assert_eq!(
        drafts::reserve(
            &f.state.pool,
            &drafts::NewDraft {
                media_id: id,
                account_id: owner,
                channel_id: f.channel,
                title: "Imported film",
                description: "Original credit",
                language: Some("en"),
                visibility: "unlisted",
                source_file: &format!("{id}.orig"),
                source_fingerprint: None,
                poster_time_seconds: 0.0,
                media_description: Some("Visual description"),
                media_transcript: Some("Transcript"),
                caption_vtt: None,
                reserved_bytes: cap,
            },
            &drafts::AdmissionLimits {
                unfinished_per_account: 10,
                retained_source_bytes_per_account: 4096,
                pending_source_bytes_global: 4096,
                storage: plamenu_db::video_storage::Limits {
                    per_account: 4096,
                    global: 8192
                },
                disk_available_bytes: u64::MAX,
                min_free_bytes: 0,
                processing_reserved_bytes: 0
            }
        )
        .await
        .unwrap(),
        drafts::Admission::Accepted
    );
    assert!(
        db::enqueue(&f.state.pool, id, owner, url, kind)
            .await
            .unwrap()
    );
    id
}

#[sqlx::test(migrations = "../db/migrations")]
async fn downloads_to_private_source_then_native_queue_and_discards_url(pool: PgPool) {
    let f = fixture(pool).await;
    let url = "https://source.example/video.mp4?private-import-hint=not-public";
    f.stub
        .serve_media(url, "video/mp4", b"bounded source bytes".to_vec());
    let id = reserve(&f, url, "file", 100).await;
    assert!(service::run_one(&f.state).await.unwrap());
    let draft = drafts::find_owned(&f.state.pool, id, f.owner)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            draft.state.as_str(),
            draft.source_bytes,
            draft.reserved_bytes
        ),
        ("queued", 20, 20)
    );
    assert_eq!(
        f.state.media.get(&draft.source_file).await.unwrap(),
        b"bounded source bytes"
    );
    assert_eq!(
        db::phase(&f.state.pool, id).await.unwrap().as_deref(),
        Some("done")
    );
    let source: String =
        sqlx::query_scalar("SELECT source_url FROM video_source_imports WHERE media_id=$1")
            .bind(id)
            .fetch_one(&f.state.pool)
            .await
            .unwrap();
    assert_eq!(source, "imported");
    assert!(
        drafts::preview(&f.state.pool, id, f.owner)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses")
            .fetch_one(&f.state.pool)
            .await
            .unwrap(),
        0
    );
    assert!(
        tokio::fs::read_dir(f.state.config.media_dir.join("tmp"))
            .await
            .unwrap()
            .next_entry()
            .await
            .unwrap()
            .is_none()
    );
    let app = plamenu::build_router(f.state.clone());
    for uri in [
        format!("/media/{id}.orig"),
        format!("/videos/drafts/{id}/preview/video"),
        format!("/videos/drafts/{id}/status"),
    ] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn bounded_failure_explicit_retry_cancellation_and_fenced_crash_reclaim(pool: PgPool) {
    let f = fixture(pool).await;
    let url = "https://source.example/large.mp4";
    f.stub.serve_media(url, "video/mp4", vec![0; 101]);
    let id = reserve(&f, url, "file", 100).await;
    assert!(service::run_one(&f.state).await.is_err());
    assert_eq!(
        db::phase(&f.state.pool, id).await.unwrap().as_deref(),
        Some("failed")
    );
    assert!(!service::run_one(&f.state).await.unwrap());
    assert!(!db::retry(&f.state.pool, id, f.channel).await.unwrap());
    assert!(db::retry(&f.state.pool, id, f.owner).await.unwrap());
    let stale = db::claim(&f.state.pool).await.unwrap().unwrap();
    assert!(db::claim(&f.state.pool).await.unwrap().is_none());
    sqlx::query("UPDATE video_source_imports SET lease_until=clock_timestamp()-interval '1 second' WHERE media_id=$1").bind(id).execute(&f.state.pool).await.unwrap();
    let current = db::claim(&f.state.pool).await.unwrap().unwrap();
    assert!(current.generation > stale.generation);
    assert!(!db::renew(&f.state.pool, &stale).await.unwrap());
    assert!(!db::fail(&f.state.pool, &stale).await.unwrap());
    let mut tx = f.state.pool.begin().await.unwrap();
    assert!(!db::lock_claim(&mut tx, &stale).await.unwrap());
    tx.rollback().await.unwrap();
    assert!(drafts::cancel(&f.state.pool, id, f.owner).await.unwrap());
    assert!(!db::renew(&f.state.pool, &current).await.unwrap());
    assert!(db::phase(&f.state.pool, id).await.unwrap().is_none());
    let retained: i64 = sqlx::query_scalar(
        "SELECT reserved_bytes FROM video_storage_reservations WHERE file_name=$1",
    )
    .bind(format!("{id}.orig"))
    .fetch_one(&f.state.pool)
    .await
    .unwrap();
    assert_eq!(
        retained, 100,
        "cancellation cannot refund unacknowledged source deletion"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn public_activitypub_sources_only_and_revocation_fences_download_commit(pool: PgPool) {
    let f = fixture(pool).await;
    let url = "https://source.example/videos/watch/film";
    let file = "https://source.example/file.mp4";
    f.stub.serve_media(file, "video/mp4", vec![1; 20]);
    f.stub.objects.lock().unwrap().insert(url.into(),serde_json::json!({"type":"Video","to":["https://www.w3.org/ns/activitystreams#Public"],"url":[{"mediaType":"video/mp4","href":file,"height":480}]}));
    let id = reserve(&f, url, "activitypub", 100).await;
    assert!(service::run_one(&f.state).await.unwrap());
    assert_eq!(
        f.stub.media_fetches.lock().unwrap().as_slice(),
        &[file.to_owned()]
    );
    assert!(drafts::cancel(&f.state.pool, id, f.owner).await.unwrap());
    f.stub.objects.lock().unwrap().get_mut(url).unwrap()["isLiveBroadcast"] = true.into();
    let live = reserve(&f, url, "activitypub", 100).await;
    assert!(service::run_one(&f.state).await.is_err());
    assert_eq!(
        f.stub.media_fetches.lock().unwrap().len(),
        1,
        "live never downloads"
    );
    assert!(drafts::cancel(&f.state.pool, live, f.owner).await.unwrap());
    let collaborator = common::create_local_account(&f.state.pool, "bob", "Bob").await;
    sqlx::query("INSERT INTO video_channel_grants(channel_id,account_id,role,accepted) VALUES($1,$2,'publisher',true)").bind(f.channel).bind(collaborator.id).execute(&f.state.pool).await.unwrap();
    let candidate = reserve_for(&f, collaborator.id, file, "file", 100).await;
    sqlx::query("UPDATE accounts SET suspended_at=clock_timestamp() WHERE id=$1")
        .bind(f.owner)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert!(
        db::claim(&f.state.pool).await.unwrap().is_none(),
        "suspended current channel owner fences collaborators"
    );
    sqlx::query("UPDATE accounts SET suspended_at=NULL WHERE id=$1")
        .bind(f.owner)
        .execute(&f.state.pool)
        .await
        .unwrap();
    let claim = db::claim(&f.state.pool).await.unwrap().unwrap();
    sqlx::query("DELETE FROM video_channel_grants WHERE channel_id=$1 AND account_id=$2")
        .bind(f.channel)
        .bind(collaborator.id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert!(!db::renew(&f.state.pool, &claim).await.unwrap());
    let mut tx = f.state.pool.begin().await.unwrap();
    assert!(!db::lock_claim(&mut tx, &claim).await.unwrap());
    tx.rollback().await.unwrap();
    assert!(
        drafts::cancel(&f.state.pool, candidate, collaborator.id)
            .await
            .unwrap()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn cancelling_a_real_inflight_controller_drops_origin_work_and_scratch(pool: PgPool) {
    let f = fixture(pool).await;
    let url = "https://source.example/stalled.mp4";
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    *f.stub.media_download_gate.lock().unwrap() = Some(gate.clone());
    f.stub.serve_media(url, "video/mp4", vec![1; 20]);
    let id = reserve(&f, url, "file", 100).await;
    let state = f.state.clone();
    let worker = tokio::spawn(async move { service::run_one(&state).await });
    // Admission includes SQL preflight and scratch creation, not cancellation.
    // Await the actual polled origin signal instead of busy-spinning on a log;
    // the six-second cancellation bound below remains unchanged.
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        f.stub.media_download_started.notified(),
    )
    .await
    .unwrap();
    assert!(drafts::cancel(&f.state.pool, id, f.owner).await.unwrap());
    let result = tokio::time::timeout(std::time::Duration::from_secs(6), worker)
        .await
        .unwrap()
        .unwrap();
    assert!(
        result.is_err(),
        "network work must stop without releasing the origin gate"
    );
    assert_eq!(gate.available_permits(), 0);
    assert!(
        tokio::fs::read_dir(f.state.config.media_dir.join("tmp"))
            .await
            .unwrap()
            .next_entry()
            .await
            .unwrap()
            .is_none()
    );
    assert!(f.state.media.get(&format!("{id}.orig")).await.is_err());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn ordinary_html_requires_session_csrf_rights_and_reserves_remote_not_form_size(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let user =
        plamenu_db::user::create(&f.state.pool, f.owner, Some("alice@example.test"), "unused")
            .await
            .unwrap();
    let web = plamenu::web::session::ensure_web_app(&f.state)
        .await
        .unwrap();
    let raw = "import-test-token";
    plamenu_db::oauth::create_token(
        &f.state.pool,
        &plamenu::auth::hash_secret(raw),
        web.id,
        Some(user.id),
        "read write",
    )
    .await
    .unwrap();
    let cookie = format!("__Host-plamenu_session={raw}");
    let csrf = plamenu::auth::hash_secret(&format!("plamenu-csrf:{raw}"));
    let app = plamenu::build_router(f.state.clone());
    for scope in ["read", "write:statuses"] {
        let raw = format!("import-token-{scope}");
        plamenu_db::oauth::create_token(
            &f.state.pool,
            &plamenu::auth::hash_secret(&raw),
            web.id,
            Some(user.id),
            scope,
        )
        .await
        .unwrap();
        let csrf = plamenu::auth::hash_secret(&format!("plamenu-csrf:{raw}"));
        let denied=app.clone().oneshot(Request::builder().method("POST").uri("/web/videos/import")
            .header("Cookie",format!("__Host-plamenu_session={raw}"))
            .header("Content-Type","application/x-www-form-urlencoded")
            .body(Body::from(format!("csrf={csrf}&confirm=yes&channel={}&title=Imported+film&visibility=unlisted&source_kind=file&source_url=https%3A%2F%2Fsource.example%2Ffilm.mp4",f.channel))).unwrap()).await.unwrap();
        assert_eq!(denied.status(), StatusCode::FORBIDDEN, "{scope}");
    }
    let get = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/videos/import")
                .header("Cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(get.status(), StatusCode::OK);
    assert_eq!(get.headers()["cache-control"], "private, no-store");
    let html =
        String::from_utf8(get.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(html.contains("name=\"source_url\"") && html.contains("name=\"confirm\""));
    assert!(!html.contains("media-description"));
    assert!(f.stub.media_fetches.lock().unwrap().is_empty());
    let post = |token: &str, confirm: &str| {
        app.clone().oneshot(Request::builder().method("POST").uri("/web/videos/import")
        .header("Cookie",&cookie).header("Content-Type","application/x-www-form-urlencoded").header("Content-Length","200")
        .body(Body::from(format!("csrf={token}&confirm={confirm}&channel={}&title=Imported+film&visibility=unlisted&source_kind=file&source_url=https%3A%2F%2Fsource.example%2Ffilm.mp4",f.channel))).unwrap())
    };
    assert_eq!(
        post("wrong", "yes").await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        post(&csrf, "").await.unwrap().status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let accepted = post(&csrf, "yes").await.unwrap();
    assert_eq!(accepted.status(), StatusCode::SEE_OTHER);
    let id: i64 = accepted.headers()["location"]
        .to_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let draft = drafts::find_owned(&f.state.pool, id, f.owner)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        draft.reserved_bytes,
        i64::try_from(f.state.config.native_video.max_upload_bytes()).unwrap()
    );
    assert!(
        f.stub.media_fetches.lock().unwrap().is_empty(),
        "POST queues, does not eagerly fetch"
    );
}
