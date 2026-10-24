mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use common::{StubFederation, create_local_account, test_state_with_store};
use plamenu::{
    AppState,
    music_renditions::{self, Selection},
    storage::{LocalDiskStore, MediaStore},
};
use plamenu_db::{PgPool, id};
use std::sync::Arc;
use tower::ServiceExt;
struct Fixture {
    state: AppState,
    owner: i64,
    reader: i64,
    asset: i64,
    root: tempfile::TempDir,
}
async fn fixture(pool: PgPool) -> Fixture {
    let owner = create_local_account(&pool, "owner", "Owner").await.id;
    let reader = create_local_account(&pool, "reader", "Reader").await.id;
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(LocalDiskStore::new(root.path().join("store")).unwrap());
    let mut state = test_state_with_store(pool, StubFederation::with_actors([]), store.clone());
    let cfg = Arc::make_mut(&mut state.config);
    cfg.media_dir = root.path().join("store");
    cfg.music.min_free_mib = 0;
    cfg.music.max_rendition_mib = 1;
    cfg.music.rendition_scratch_mib = 4;
    cfg.music.rendition_jobs = 2;
    let asset = id::next();
    let artist = id::next();
    let source = root.path().join("source.wav");
    assert!(
        std::process::Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-loglevel",
                "fatal",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=443:duration=0.3",
                "-c:a",
                "pcm_s16le"
            ])
            .arg(&source)
            .status()
            .unwrap()
            .success()
    );
    let original = format!("music{asset}.orig");
    let stream = format!("music{asset}.mp3");
    let bytes = std::fs::metadata(&source).unwrap().len();
    store.put_file(&original, &source).await.unwrap();
    store
        .put(&stream, b"seed original stream".to_vec())
        .await
        .unwrap();
    sqlx::query("INSERT INTO music_artists(id,owner_id,name) VALUES($1,$2,'Artist')")
        .bind(artist)
        .bind(owner)
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO music_assets(id,owner_id,title,artist_id,original_file,stream_file,original_bytes,stream_bytes,audio_hash,duration,original_format) VALUES($1,$2,'Track',$3,$4,$5,$6,20,$7,0.3,'wav')")
        .bind(asset).bind(owner).bind(artist).bind(original).bind(stream).bind(i64::try_from(bytes).unwrap()).bind(format!("{asset:064x}")).execute(&state.pool).await.unwrap();
    Fixture {
        state,
        owner,
        reader,
        asset,
        root,
    }
}
fn selection(f: &Fixture) -> Selection {
    music_renditions::selection(f.state.config.music, "ogg", Some(192), None).unwrap()
}
async fn create(f: &Fixture) -> music_renditions::Rendition {
    music_renditions::request(&f.state, f.asset, Some(f.owner), selection(f))
        .await
        .unwrap()
}
fn launcher() -> std::path::PathBuf {
    env!("CARGO_BIN_EXE_plamenu").into()
}
async fn bytes(f: &Fixture, job: i64, key: Option<&str>) -> axum::response::Response {
    let mut request = Request::builder()
        .uri(format!("/music/assets/{}/renditions/{job}/stream", f.asset))
        .header(header::RANGE, "bytes=0-3");
    if let Some(key) = key {
        request = request.header(header::IF_NONE_MATCH, key);
    }
    plamenu::build_router(f.state.clone())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn durable_singleflight_encodes_real_audio_and_rechecks_acl_before_every_range(pool: PgPool) {
    let f = fixture(pool).await;
    let job = create(&f).await;
    assert_eq!(job.state, "queued");
    assert_eq!(create(&f).await.id, job.id);
    assert!(
        music_renditions::get(&f.state, f.asset, job.id, Some(f.reader))
            .await
            .is_err()
    );
    let (first, second) = tokio::join!(
        music_renditions::run_one(&f.state, job.id, launcher()),
        music_renditions::run_one(&f.state, job.id, launcher())
    );
    assert_eq!(u8::from(first.unwrap()) + u8::from(second.unwrap()), 1);
    let ready = music_renditions::get(&f.state, f.asset, job.id, Some(f.owner))
        .await
        .unwrap();
    assert_eq!(ready.state, "ready");
    assert_eq!(ready.actual_bitrate, Some(192));
    assert!(ready.size_bytes > 0);
    assert_eq!(create(&f).await.id, job.id);
    assert_eq!(
        bytes(&f, job.id, None).await.status(),
        StatusCode::NOT_FOUND
    );
    sqlx::query("UPDATE music_assets SET visibility='public' WHERE id=$1")
        .bind(f.asset)
        .execute(&f.state.pool)
        .await
        .unwrap();
    let range = bytes(&f, job.id, None).await;
    assert_eq!(range.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(range.headers()[header::CONTENT_TYPE], "audio/ogg");
    assert_eq!(
        axum::body::to_bytes(range.into_body(), 100).await.unwrap(),
        &b"OggS"[..]
    );
    let key = ready.file_name.unwrap();
    assert!(
        plamenu_db::media_cleanup::referenced_keys(&f.state.pool)
            .await
            .unwrap()
            .contains(&key)
    );
    let direct = plamenu::build_router(f.state.clone())
        .oneshot(
            Request::builder()
                .uri(format!("/media/{key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(direct.status(), StatusCode::NOT_FOUND);
    sqlx::query("UPDATE music_assets SET visibility='private' WHERE id=$1")
        .bind(f.asset)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(
        bytes(&f, job.id, Some("*")).await.status(),
        StatusCode::NOT_FOUND
    );
    let debt: i64 = sqlx::query_scalar("SELECT count(*) FROM music_storage_debt")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(debt, 0);
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn cache_retirement_preserves_quota_debt_until_physical_cleanup(pool: PgPool) {
    let mut f = fixture(pool).await;
    let job = create(&f).await;
    music_renditions::run_one(&f.state, job.id, launcher())
        .await
        .unwrap();
    let ready = music_renditions::get(&f.state, f.asset, job.id, Some(f.owner))
        .await
        .unwrap();
    let key = ready.file_name.clone().unwrap();
    music_renditions::cancel(&f.state, f.asset, job.id, f.owner)
        .await
        .unwrap();
    assert!(f.state.media.open(&key).await.is_ok());
    let debt: i64 =
        sqlx::query_scalar("SELECT size_bytes FROM music_storage_debt WHERE file_name=$1")
            .bind(&key)
            .fetch_one(&f.state.pool)
            .await
            .unwrap();
    assert_eq!(debt, ready.size_bytes);
    assert!(
        music_renditions::get(&f.state, f.asset, job.id, Some(f.owner))
            .await
            .is_err()
    );
    Arc::make_mut(&mut f.state.config).music.per_account_mib = 1;
    let retry = create(&f).await;
    assert!(
        music_renditions::run_one(&f.state, retry.id, launcher())
            .await
            .is_err()
    );
    assert_eq!(
        music_renditions::get(&f.state, f.asset, retry.id, Some(f.owner))
            .await
            .unwrap()
            .state,
        "failed"
    );
    assert_eq!(f.state.media.list().await.unwrap().len(), 3);
    plamenu::media_cleanup_worker::run_due(&f.state).await;
    assert!(f.state.media.open(&key).await.is_err());
    let debt: i64 =
        sqlx::query_scalar("SELECT count(*) FROM music_storage_debt WHERE file_name=$1")
            .bind(key)
            .fetch_one(&f.state.pool)
            .await
            .unwrap();
    assert_eq!(debt, 0);
}
fn blocked(f: &Fixture) -> (AppState, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let mut state = f.state.clone();
    let script = f.root.path().join("encoder.py");
    let marker = f.root.path().join("encoder-started");
    std::fs::write(&script,format!("#!/usr/bin/python3\nimport os,sys,time\nopen({marker:?},'w').close()\ntime.sleep(20)\nos.execv('/usr/bin/ffmpeg',['ffmpeg']+sys.argv[1:])\n",marker=marker.to_str().unwrap())).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    Arc::make_mut(&mut state.config).ffmpeg_path = script.to_str().unwrap().into();
    (state, marker)
}
async fn started(marker: &std::path::Path) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !marker.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn cancellation_kills_inflight_encoder_and_keeps_durable_cleanup_charge(pool: PgPool) {
    let f = fixture(pool).await;
    let job = create(&f).await;
    let (state, marker) = blocked(&f);
    let task =
        tokio::spawn(async move { music_renditions::run_one(&state, job.id, launcher()).await });
    started(&marker).await;
    let running = music_renditions::get(&f.state, f.asset, job.id, Some(f.owner))
        .await
        .unwrap();
    assert_eq!(running.state, "running");
    music_renditions::cancel(&f.state, f.asset, job.id, f.owner)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert_eq!(
        std::fs::read_dir(f.state.config.media_dir.join("tmp"))
            .unwrap()
            .count(),
        0
    );
    let debt: i64 =
        sqlx::query_scalar("SELECT size_bytes FROM music_storage_debt WHERE file_name=$1")
            .bind(&running.file_name)
            .fetch_one(&f.state.pool)
            .await
            .unwrap();
    assert_eq!(debt, 1024 * 1024);
    plamenu::media_cleanup_worker::run_due(&f.state).await;
    let debt: i64 = sqlx::query_scalar("SELECT count(*) FROM music_storage_debt")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(debt, 0);
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn expired_lease_uses_new_key_and_stale_encoder_cannot_retire_successor(pool: PgPool) {
    let f = fixture(pool).await;
    let job = create(&f).await;
    let (state, marker) = blocked(&f);
    let task =
        tokio::spawn(async move { music_renditions::run_one(&state, job.id, launcher()).await });
    started(&marker).await;
    let old = music_renditions::get(&f.state, f.asset, job.id, Some(f.owner))
        .await
        .unwrap();
    sqlx::query("UPDATE music_renditions SET lease_until=now()-interval '1 second' WHERE id=$1")
        .bind(job.id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert!(
        music_renditions::run_one(&f.state, job.id, launcher())
            .await
            .unwrap()
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    let ready = music_renditions::get(&f.state, f.asset, job.id, Some(f.owner))
        .await
        .unwrap();
    assert_eq!(ready.state, "ready");
    assert_ne!(ready.file_name, old.file_name);
    plamenu::media_cleanup_worker::run_due(&f.state).await;
    assert!(
        f.state
            .media
            .open(ready.file_name.as_ref().unwrap())
            .await
            .is_ok()
    );
    assert!(
        f.state
            .media
            .open(old.file_name.as_ref().unwrap())
            .await
            .is_err()
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn revoked_grant_aborts_requester_job_and_asset_deletion_retires_ready_cache(pool: PgPool) {
    let f = fixture(pool).await;
    plamenu::music_assets::grant(&f.state, f.owner, f.asset, f.reader, true)
        .await
        .unwrap();
    let job = music_renditions::request(&f.state, f.asset, Some(f.reader), selection(&f))
        .await
        .unwrap();
    let (state, marker) = blocked(&f);
    let task =
        tokio::spawn(async move { music_renditions::run_one(&state, job.id, launcher()).await });
    started(&marker).await;
    plamenu::music_assets::grant(&f.state, f.owner, f.asset, f.reader, false)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert_eq!(
        music_renditions::get(&f.state, f.asset, job.id, Some(f.owner))
            .await
            .unwrap()
            .state,
        "failed"
    );
    let retry = create(&f).await;
    assert_ne!(retry.id, job.id);
    music_renditions::run_one(&f.state, retry.id, launcher())
        .await
        .unwrap();
    let ready = music_renditions::get(&f.state, f.asset, retry.id, Some(f.owner))
        .await
        .unwrap();
    plamenu::music_assets::delete(&f.state, f.owner, f.asset)
        .await
        .unwrap();
    let charged: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM music_storage_debt WHERE file_name=$1)")
            .bind(&ready.file_name)
            .fetch_one(&f.state.pool)
            .await
            .unwrap();
    assert!(charged);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM music_renditions")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn queue_and_active_scratch_admission_remain_bounded(pool: PgPool) {
    let mut f = fixture(pool).await;
    Arc::make_mut(&mut f.state.config).music.rendition_jobs = 1;
    let job = create(&f).await;
    let (state, marker) = blocked(&f);
    let task =
        tokio::spawn(async move { music_renditions::run_one(&state, job.id, launcher()).await });
    started(&marker).await;
    let next = music_renditions::request(
        &f.state,
        f.asset,
        Some(f.owner),
        music_renditions::selection(f.state.config.music, "flac", None, None).unwrap(),
    )
    .await
    .unwrap();
    assert!(
        !music_renditions::run_one(&f.state, next.id, launcher())
            .await
            .unwrap()
    );
    assert_eq!(
        music_renditions::get(&f.state, f.asset, next.id, Some(f.owner))
            .await
            .unwrap()
            .state,
        "queued"
    );
    music_renditions::cancel(&f.state, f.asset, job.id, f.owner)
        .await
        .unwrap();
    assert!(task.await.unwrap().is_err());
    assert!(
        music_renditions::run_one(&f.state, next.id, launcher())
            .await
            .unwrap()
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn ready_renditions_count_against_new_upload_budget_and_retirement_journals(pool: PgPool) {
    let mut f = fixture(pool).await;
    let job = create(&f).await;
    music_renditions::run_one(&f.state, job.id, launcher())
        .await
        .unwrap();
    let ready = music_renditions::get(&f.state, f.asset, job.id, Some(f.owner))
        .await
        .unwrap();
    // Exactly 1 MiB usage with the cached rendition, versus under budget if
    // admission forgets it. The next encoder reserves a 1 MiB output.
    sqlx::query("UPDATE music_assets SET original_bytes=$2,stream_bytes=1 WHERE id=$1")
        .bind(f.asset)
        .bind(1024 * 1024 - ready.size_bytes - 1)
        .execute(&f.state.pool)
        .await
        .unwrap();
    Arc::make_mut(&mut f.state.config).music.per_account_mib = 2;
    let next = music_renditions::request(
        &f.state,
        f.asset,
        Some(f.owner),
        music_renditions::selection(f.state.config.music, "flac", None, None).unwrap(),
    )
    .await
    .unwrap();
    assert!(
        music_renditions::run_one(&f.state, next.id, launcher())
            .await
            .unwrap()
    );
    let third = music_renditions::request(
        &f.state,
        f.asset,
        Some(f.owner),
        music_renditions::selection(f.state.config.music, "opus", None, None).unwrap(),
    )
    .await
    .unwrap();
    assert!(
        music_renditions::run_one(&f.state, third.id, launcher())
            .await
            .is_err()
    );
    sqlx::query("UPDATE accounts SET deleted_at=now() WHERE id=$1")
        .bind(f.owner)
        .execute(&f.state.pool)
        .await
        .unwrap();
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM music_renditions WHERE owner_id=$1")
        .bind(f.owner)
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
    let debt: i64 =
        sqlx::query_scalar("SELECT size_bytes FROM music_storage_debt WHERE file_name=$1")
            .bind(&ready.file_name)
            .fetch_one(&f.state.pool)
            .await
            .unwrap();
    assert_eq!(debt, ready.size_bytes);
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn public_cached_bitrate_cannot_bypass_current_member_ceiling(pool: PgPool) {
    let mut f = fixture(pool).await;
    Arc::make_mut(&mut f.state.config).music.user_max_bitrate = 320;
    sqlx::query("UPDATE music_assets SET visibility='public' WHERE id=$1")
        .bind(f.asset)
        .execute(&f.state.pool)
        .await
        .unwrap();
    let selection =
        music_renditions::selection(f.state.config.music, "ogg", Some(320), None).unwrap();
    let job = music_renditions::request(&f.state, f.asset, Some(f.owner), selection)
        .await
        .unwrap();
    music_renditions::run_one(&f.state, job.id, launcher())
        .await
        .unwrap();
    assert_eq!(
        bytes(&f, job.id, None).await.status(),
        StatusCode::PARTIAL_CONTENT
    );
    Arc::make_mut(&mut f.state.config).music.user_max_bitrate = 192;
    assert_eq!(
        bytes(&f, job.id, Some("*")).await.status(),
        StatusCode::FORBIDDEN
    );
    assert!(
        music_renditions::list(&f.state, f.asset, None)
            .await
            .unwrap()
            .is_empty()
    );
    let page = plamenu::build_router(f.state.clone())
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/music/assets/{}/renditions?job={}",
                    f.asset, job.id
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(page.status(), StatusCode::FORBIDDEN);
}
async fn share(f: &Fixture) -> plamenu::music_shares::Created {
    plamenu::music_shares::create(
        &f.state,
        f.owner,
        &plamenu::music_shares::New {
            reference: plamenu::music_saved::Reference {
                kind: plamenu::music_saved::Kind::Track,
                id: f.asset.to_string(),
            },
            expires_at: None,
        },
    )
    .await
    .unwrap()
}
async fn shared_request(
    f: &Fixture,
    method: &str,
    path: &str,
    body: &str,
) -> axum::response::Response {
    plamenu::build_router(f.state.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::RANGE, "bytes=0-3")
                .header(header::IF_NONE_MATCH, "*")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap()
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn share_format_forms_prepare_private_audio_and_revoke_all_cached_byte_aliases(pool: PgPool) {
    let f = fixture(pool).await;
    let link = share(&f).await;
    let response = shared_request(
        &f,
        "POST",
        &format!("/web/music/assets/{}/renditions", f.asset),
        &serde_urlencoded::to_string([
            ("token", link.token.as_str()),
            ("format", "flac"),
            ("bitrate", "320"),
        ])
        .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let location = response.headers()[header::LOCATION].to_str().unwrap();
    let job = location
        .split("job=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap()
        .parse::<i64>()
        .unwrap();
    let scope = plamenu::music_shares::resolve(&f.state, &link.token, None)
        .await
        .unwrap();
    let queued = music_renditions::get_shared(&f.state, f.asset, job, None, &scope)
        .await
        .unwrap();
    assert_eq!(queued.principal_kind, "share");
    assert_eq!(queued.requester_id, None);
    assert_eq!(queued.share_link_id, Some(scope.id));
    assert_eq!(queued.entity()["actual_bitrate"], serde_json::Value::Null);
    assert_eq!(queued.entity()["lossless"], true);
    assert!(
        music_renditions::run_one(&f.state, job, launcher())
            .await
            .unwrap()
    );
    let page = shared_request(&f, "GET", location, "").await;
    assert_eq!(page.status(), StatusCode::OK);
    assert_eq!(page.headers()[header::REFERRER_POLICY], "no-referrer");
    let html = String::from_utf8(
        axum::body::to_bytes(page.into_body(), 1024 * 1024)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(html.contains("data-actual-bitrate=\"lossless\""));
    assert!(!html.contains("FLAC · 192 kbps"));
    let stream = format!(
        "/music/share/assets/{}/renditions/{job}/stream?token={}",
        f.asset, link.token
    );
    let range = shared_request(&f, "GET", &stream, "").await;
    assert_eq!(range.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(range.headers()[header::REFERRER_POLICY], "no-referrer");
    assert_eq!(
        axum::body::to_bytes(range.into_body(), 100).await.unwrap(),
        &b"fLaC"[..]
    );
    assert_eq!(bytes(&f, job, None).await.status(), StatusCode::NOT_FOUND);
    plamenu::music_shares::revoke(&f.state, f.owner, scope.id)
        .await
        .unwrap();
    assert_eq!(
        shared_request(&f, "GET", &stream, "").await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        shared_request(&f, "GET", location, "").await.status(),
        StatusCode::NOT_FOUND
    );
    assert!(
        music_renditions::get_shared(&f.state, f.asset, job, None, &scope)
            .await
            .is_err()
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn share_principal_retirement_cannot_downgrade_an_inflight_job_to_anonymous(pool: PgPool) {
    let f = fixture(pool).await;
    let link = share(&f).await;
    let scope = plamenu::music_shares::resolve(&f.state, &link.token, None)
        .await
        .unwrap();
    let job = music_renditions::request_shared(&f.state, f.asset, None, selection(&f), &scope)
        .await
        .unwrap();
    let (state, marker) = blocked(&f);
    let task =
        tokio::spawn(async move { music_renditions::run_one(&state, job.id, launcher()).await });
    started(&marker).await;
    // Keep source independently public: loss of the original proof still
    // cancels this job instead of silently replacing its admitted principal.
    sqlx::query("UPDATE music_assets SET visibility='public' WHERE id=$1")
        .bind(f.asset)
        .execute(&f.state.pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM music_share_links WHERE id=$1")
        .bind(scope.id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    let failed = music_renditions::get(&f.state, f.asset, job.id, None)
        .await
        .unwrap();
    assert_eq!(failed.principal_kind, "share");
    assert_eq!(failed.share_link_id, None);
    assert_eq!(failed.state, "failed");
    let account_job = music_renditions::request(&f.state, f.asset, Some(f.reader), selection(&f))
        .await
        .unwrap();
    sqlx::query("DELETE FROM accounts WHERE id=$1")
        .bind(f.reader)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert!(
        music_renditions::run_one(&f.state, account_job.id, launcher())
            .await
            .is_err()
    );
    assert_eq!(
        music_renditions::get(&f.state, f.asset, account_job.id, None)
            .await
            .unwrap()
            .principal_kind,
        "account"
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn share_collection_renditions_preserve_exact_membership_and_foreign_asset_acl(pool: PgPool) {
    let f = fixture(pool).await;
    let collection = plamenu::music_collections::create(
        &f.state,
        f.owner,
        &plamenu::music_collections::New {
            name: "Scoped library".into(),
            description: String::new(),
            kind: "library".into(),
            visibility: "private".into(),
        },
    )
    .await
    .unwrap();
    sqlx::query("UPDATE music_collections SET share_tracks=true WHERE id=$1")
        .bind(collection.id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    let foreign = id::next();
    let artist = id::next();
    sqlx::query("INSERT INTO music_artists(id,owner_id,name) VALUES($1,$2,'Foreign artist')")
        .bind(artist)
        .bind(f.reader)
        .execute(&f.state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO music_assets(id,owner_id,title,artist_id,original_file,stream_file,original_bytes,stream_bytes,audio_hash,duration,original_format) VALUES($1,$2,'Foreign private',$3,$4,$5,10,10,$6,1,'wav')")
        .bind(foreign).bind(f.reader).bind(artist).bind(format!("music{foreign}.orig")).bind(format!("music{foreign}.mp3")).bind(format!("{foreign:064x}")).execute(&f.state.pool).await.unwrap();
    for (position, asset) in [(1, f.asset), (2, foreign)] {
        sqlx::query("INSERT INTO music_collection_entries(id,collection_id,asset_id,position) VALUES($1,$2,$3,$4)").bind(id::next()).bind(collection.id).bind(asset).bind(position).execute(&f.state.pool).await.unwrap();
    }
    let link = plamenu::music_shares::create(
        &f.state,
        f.owner,
        &plamenu::music_shares::New {
            reference: plamenu::music_saved::Reference {
                kind: plamenu::music_saved::Kind::Library,
                id: collection.id.to_string(),
            },
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let scope = plamenu::music_shares::resolve(&f.state, &link.token, None)
        .await
        .unwrap();
    let job = music_renditions::request_shared(&f.state, f.asset, None, selection(&f), &scope)
        .await
        .unwrap();
    assert!(
        music_renditions::request_shared(&f.state, foreign, None, selection(&f), &scope)
            .await
            .is_err()
    );
    assert!(
        music_renditions::run_one(&f.state, job.id, launcher())
            .await
            .unwrap()
    );
    sqlx::query("DELETE FROM music_collection_entries WHERE collection_id=$1 AND asset_id=$2")
        .bind(collection.id)
        .bind(f.asset)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert!(
        music_renditions::get_shared(&f.state, f.asset, job.id, None, &scope)
            .await
            .is_err()
    );
    let stream = format!(
        "/music/share/assets/{}/renditions/{}/stream?token={}",
        f.asset, job.id, link.token
    );
    assert_eq!(
        shared_request(&f, "GET", &stream, "").await.status(),
        StatusCode::NOT_FOUND
    );
}
fn real_slow_encoder(f: &Fixture) -> AppState {
    use std::os::unix::fs::PermissionsExt;
    let source = f.root.path().join("long.wav");
    assert!(
        std::process::Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-loglevel",
                "fatal",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=443:duration=8",
                "-c:a",
                "pcm_s16le"
            ])
            .arg(&source)
            .status()
            .unwrap()
            .success()
    );
    std::fs::copy(
        source,
        f.state
            .config
            .media_dir
            .join(format!("music{}.orig", f.asset)),
    )
    .unwrap();
    let script = f.root.path().join("real-slow-encoder.py");
    std::fs::write(&script,"#!/usr/bin/python3\nimport os,sys\nargs=['/usr/bin/ffmpeg']+sys.argv[1:]\nargs.insert(args.index('-i'),'-re')\nos.execv(args[0],args)\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut state = f.state.clone();
    Arc::make_mut(&mut state.config).ffmpeg_path = script.to_str().unwrap().into();
    state
}
fn due_worker(state: AppState) -> tokio::task::JoinHandle<Result<bool, plamenu::error::ApiError>> {
    tokio::spawn(async move {
        let job = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let job: Option<i64> = sqlx::query_scalar(
                    "SELECT id FROM music_renditions WHERE state='queued' ORDER BY id LIMIT 1",
                )
                .fetch_optional(&state.pool)
                .await
                .unwrap();
                if let Some(job) = job {
                    break job;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        music_renditions::run_one(&state, job, launcher()).await
    })
}
async fn cold_format(pool: PgPool, format: &str, magic: &[u8]) {
    let f = fixture(pool).await;
    let state = real_slow_encoder(&f);
    sqlx::query("UPDATE music_assets SET visibility='public',duration=8 WHERE id=$1")
        .bind(f.asset)
        .execute(&state.pool)
        .await
        .unwrap();
    let worker = due_worker(state.clone());
    let response = plamenu::build_router(state.clone())
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/music/assets/{}/stream?format={format}&bitrate=320",
                    f.asset
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::ACCEPT_RANGES], "none");
    assert!(response.headers().get(header::CONTENT_LENGTH).is_none());
    let job: music_renditions::Rendition = sqlx::query_as("SELECT * FROM music_renditions")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(job.state, "running");
    assert!(!job.keep_running);
    assert_eq!(job.actual_bitrate, Some(192));
    assert!(job.progress_dir.is_some());
    assert!(job.entity().get("progress_dir").is_none());
    let bytes = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        axum::body::to_bytes(response.into_body(), 1024 * 1024),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(worker.await.unwrap().unwrap());
    if format == "aac" {
        assert_eq!(&bytes[4..8], magic);
    } else {
        assert!(bytes.starts_with(magic));
    }
    let file = f.root.path().join(format!(
        "received.{}",
        music_renditions::Format::parse(format).unwrap().extension()
    ));
    std::fs::write(&file, bytes).unwrap();
    let decoded = std::process::Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-i"])
        .arg(file)
        .args(["-f", "null", "-"])
        .output()
        .unwrap();
    assert!(
        decoded.status.success(),
        "{}",
        String::from_utf8_lossy(&decoded.stderr)
    );
    let ready = music_renditions::get(&state, f.asset, job.id, None)
        .await
        .unwrap();
    assert_eq!(ready.state, "ready");
    assert_eq!(ready.progress_dir, None);
    let range = plamenu::build_router(state)
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/music/assets/{}/stream?format={format}&bitrate=320",
                    f.asset
                ))
                .header(header::RANGE, "bytes=0-3")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(range.status(), StatusCode::PARTIAL_CONTENT);
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn cold_mp3_delivers_decodable_audio_before_encoder_finishes(pool: PgPool) {
    cold_format(pool, "mp3", b"ID3").await;
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn cold_vorbis_delivers_decodable_audio_before_encoder_finishes(pool: PgPool) {
    cold_format(pool, "ogg", b"OggS").await;
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn cold_flac_delivers_decodable_audio_before_encoder_finishes(pool: PgPool) {
    cold_format(pool, "flac", b"fLaC").await;
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn cold_aac_delivers_fragmented_mp4_before_encoder_finishes(pool: PgPool) {
    cold_format(pool, "aac", b"ftyp").await;
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn cold_opus_delivers_decodable_audio_before_encoder_finishes(pool: PgPool) {
    cold_format(pool, "opus", b"OggS").await;
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn cold_listener_drop_cancels_running_work_but_explicit_prebuild_survives(pool: PgPool) {
    let f = fixture(pool).await;
    let (state, marker) = blocked(&f);
    let (job, listener) =
        music_renditions::request_cold(&state, f.asset, Some(f.owner), selection(&f), None)
            .await
            .unwrap();
    let worker =
        tokio::spawn(async move { music_renditions::run_one(&state, job.id, launcher()).await });
    started(&marker).await;
    drop(listener);
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), worker)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    let (next, listener) =
        music_renditions::request_cold(&f.state, f.asset, Some(f.owner), selection(&f), None)
            .await
            .unwrap();
    assert_eq!(create(&f).await.id, next.id);
    drop(listener);
    assert!(
        music_renditions::run_one(&f.state, next.id, launcher())
            .await
            .unwrap()
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn cold_head_does_not_admit_jobs_and_listener_limits_precede_encoding(pool: PgPool) {
    let f = fixture(pool).await;
    sqlx::query("UPDATE music_assets SET visibility='public' WHERE id=$1")
        .bind(f.asset)
        .execute(&f.state.pool)
        .await
        .unwrap();
    let response = plamenu::build_router(f.state.clone())
        .oneshot(
            Request::builder()
                .method("HEAD")
                .uri(format!("/music/assets/{}/stream?format=mp3", f.asset))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM music_renditions")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
    let mut listeners = Vec::new();
    let mut job = None;
    for _ in 0..16 {
        let (row, listener) =
            music_renditions::request_cold(&f.state, f.asset, None, selection(&f), None)
                .await
                .unwrap();
        job = Some(row.id);
        listeners.push(listener);
    }
    assert!(matches!(
        music_renditions::request_cold(&f.state, f.asset, None, selection(&f), None).await,
        Err(plamenu::error::ApiError::TooManyRequests)
    ));
    drop(listeners);
    sqlx::query("DELETE FROM music_rendition_listeners")
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert!(
        !music_renditions::run_one(&f.state, job.unwrap(), launcher())
            .await
            .unwrap()
    );
    assert_eq!(
        music_renditions::get(&f.state, f.asset, job.unwrap(), None)
            .await
            .unwrap()
            .state,
        "cancelled"
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn cold_share_revocation_stops_body_and_encoder_before_remaining_private_audio(pool: PgPool) {
    use http_body_util::BodyExt;
    let f = fixture(pool).await;
    let state = real_slow_encoder(&f);
    let link = share(&f).await;
    let worker = due_worker(state.clone());
    let response = plamenu::build_router(state)
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/music/share/assets/{}/stream?token={}&format=mp3",
                    f.asset, link.token
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::REFERRER_POLICY], "no-referrer");
    let mut body = response.into_body();
    assert!(
        body.frame()
            .await
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap()
            .starts_with(b"ID3")
    );
    plamenu::music_shares::revoke(&f.state, f.owner, link.id.parse().unwrap())
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    assert!(body.frame().await.unwrap().is_err());
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), worker)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert_eq!(
        std::fs::read_dir(f.state.config.media_dir.join("tmp"))
            .unwrap()
            .count(),
        0
    );
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn cold_visibility_revocation_stops_public_stream_and_retains_cleanup_debt(pool: PgPool) {
    use http_body_util::BodyExt;
    let f = fixture(pool).await;
    let state = real_slow_encoder(&f);
    sqlx::query("UPDATE music_assets SET visibility='public' WHERE id=$1")
        .bind(f.asset)
        .execute(&f.state.pool)
        .await
        .unwrap();
    let worker = due_worker(state.clone());
    let response = plamenu::build_router(state)
        .oneshot(
            Request::builder()
                .uri(format!("/music/assets/{}/stream?format=mp3", f.asset))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    assert!(body.frame().await.unwrap().is_ok());
    sqlx::query("UPDATE music_assets SET visibility='private' WHERE id=$1")
        .bind(f.asset)
        .execute(&f.state.pool)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    assert!(body.frame().await.unwrap().is_err());
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), worker)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM music_storage_debt")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn cold_embed_codec_gate_ignores_credentials_and_capabilities_before_admission(pool: PgPool) {
    let f = fixture(pool).await;
    let link = share(&f).await;
    let path = format!(
        "/music/embed/assets/{}/audio?format=mp3&bitrate=320&token={}",
        f.asset, link.token
    );
    let app = plamenu::build_router(f.state.clone());
    let denied = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(&path)
                .header(header::AUTHORIZATION, "Bearer owner-cookie-cannot-elevate")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);
    assert_eq!(denied.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM music_renditions")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    sqlx::query("UPDATE music_assets SET visibility='public' WHERE id=$1")
        .bind(f.asset)
        .execute(&f.state.pool)
        .await
        .unwrap();
    let worker = due_worker(f.state.clone());
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(&path)
                .header(header::ORIGIN, "https://publisher.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
    assert!(
        response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
            .is_none()
    );
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    assert!(bytes.starts_with(b"ID3"));
    assert!(worker.await.unwrap().unwrap());
    let row: music_renditions::Rendition = sqlx::query_as("SELECT * FROM music_renditions")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(row.bitrate, 192);
    assert_eq!(row.principal_kind, "anonymous");
    assert_eq!(row.requester_id, None);
    assert_eq!(row.share_link_id, None);
    sqlx::query("UPDATE music_assets SET visibility='private' WHERE id=$1")
        .bind(f.asset)
        .execute(&f.state.pool)
        .await
        .unwrap();
    let denied = app
        .oneshot(
            Request::builder()
                .uri(path)
                .header(header::RANGE, "bytes=0-3")
                .header(header::IF_NONE_MATCH, "*")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);
}
