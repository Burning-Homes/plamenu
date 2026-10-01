//! Native mirror graph, durable publication and current-authority withdrawal.
//! HTTP/network compatibility is separately exercised against real peers.
mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use plamenu::{AppState, video_mirrors as service};
use plamenu_db::{PgPool, id, status, video_distribution, video_mirrors as db};
use plamenu_federation::FetchedMedia;
use std::{sync::Arc, time::Duration};
use tower::ServiceExt;

#[derive(Default)]
struct HeldWriter {
    inner: plamenu::storage::MemoryStore,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
impl plamenu::storage::MediaStore for HeldWriter {
    fn put(
        &self,
        key: &str,
        data: Vec<u8>,
    ) -> plamenu::federation::BoxFuture<'_, std::io::Result<()>> {
        self.inner.put(key, data)
    }
    fn put_file(
        &self,
        key: &str,
        path: &std::path::Path,
    ) -> plamenu::federation::BoxFuture<'_, std::io::Result<()>> {
        let key = key.to_owned();
        let path = path.to_owned();
        Box::pin(async move {
            self.entered.notify_one();
            self.release.notified().await;
            self.inner.put_file(&key, &path).await
        })
    }
    fn get(&self, key: &str) -> plamenu::federation::BoxFuture<'_, std::io::Result<Vec<u8>>> {
        self.inner.get(key)
    }
    fn open(
        &self,
        key: &str,
    ) -> plamenu::federation::BoxFuture<'_, std::io::Result<plamenu::storage::MediaRead>> {
        self.inner.open(key)
    }
    fn list(&self) -> plamenu::federation::BoxFuture<'_, std::io::Result<Vec<String>>> {
        self.inner.list()
    }
    fn delete(&self, key: &str) -> plamenu::federation::BoxFuture<'_, std::io::Result<()>> {
        self.inner.delete(key)
    }
}

const MASTER: &str = "https://source.test/hls/master.m3u8";
const CHILD: &str = "https://source.test/hls/240/index.m3u8";
const FILE: &str = "https://source.test/hls/240/film.mp4";
struct Fixture {
    state: AppState,
    post: i64,
    operator: i64,
    media: i64,
    stub: Arc<common::StubFederation>,
    _root: tempfile::TempDir,
}
async fn fixture(pool: PgPool) -> Fixture {
    let operator = common::create_local_account(&pool, "operator", "Operator").await;
    let peer = common::RemoteUser::new("source.test", "creator");
    let author = plamenu::remote::store_remote_actor(&pool, &peer.actor)
        .await
        .unwrap();
    let stub = common::StubFederation::with_users(&[&peer]);
    let root = tempfile::tempdir().unwrap();
    let mut config = common::test_config();
    config.media_dir = root.path().into();
    config.video_redundancy = Some(plamenu::config::VideoRedundancyConfig::default());
    let state = AppState::new(
        pool.clone(),
        config,
        stub.clone(),
        Arc::new(plamenu::storage::MemoryStore::default()),
    )
    .unwrap();
    let post = status::create_local(
        &pool,
        status::NewLocalStatus::new(author.id, "<p>Film</p>", "public", None),
    )
    .await
    .unwrap();
    sqlx::query(
        "UPDATE statuses SET uri='https://source.test/videos/1',object_type='Video' WHERE id=$1",
    )
    .bind(post.id)
    .execute(&pool)
    .await
    .unwrap();
    let media = id::next();
    sqlx::query("INSERT INTO media_attachments(id,account_id,status_id,content_type,processing,remote_url,hls_master_url,download_on_demand) VALUES($1,$2,$3,'video/mp4','complete',$4,$5,true)")
        .bind(media).bind(author.id).bind(post.id).bind(FILE).bind(MASTER).execute(&pool).await.unwrap();
    video_distribution::upsert(
        &pool,
        video_distribution::Policy {
            media_id: media,
            download_enabled: true,
            embed_enabled: true,
            mirror_allowed: true,
        },
    )
    .await
    .unwrap();
    service::approve(&state, operator.id, "source.test", true, false)
        .await
        .unwrap();
    for (url, bytes) in [
        (MASTER,b"#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1000\n240/index.m3u8\n".to_vec()),
        (CHILD,b"#EXTM3U\n#EXT-X-MAP:URI=\"film.mp4\",BYTERANGE=\"4@0\"\n#EXTINF:2,\n#EXT-X-BYTERANGE:8@4\nfilm.mp4\n#EXT-X-ENDLIST\n".to_vec()),
        (FILE,(0..12).collect()),
    ] {
        stub.media.lock().unwrap().insert(url.into(),FetchedMedia {final_url:url.into(),content_type:"application/octet-stream".into(),bytes});
    }
    Fixture {
        state,
        post: post.id,
        operator: operator.id,
        media,
        stub,
        _root: root,
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn mirror_copy_and_publication_do_not_require_a_second_pool_connection(pool: PgPool) {
    let one = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(3))
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let f = fixture(one).await;
    let item = service::request(&f.state, f.operator, f.post, "")
        .await
        .unwrap();
    finish(&f, item.id, "ready").await;
    let ready = db::find(&f.state.pool, item.id).await.unwrap().unwrap();
    assert!(service::authorized(&f.state, &ready).await.unwrap());
    service::withdraw(&f.state, item.id).await.unwrap();
}
async fn finish(f: &Fixture, mirror: i64, expected: &str) {
    let worker = service::worker::spawn(f.state.clone());
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if db::find(&f.state.pool, mirror)
                .await
                .unwrap()
                .unwrap()
                .state
                == expected
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("finite mirror processing");
    worker.abort();
    let _ = worker.await;
}
async fn get(f: &Fixture, path: &str, range: Option<&str>) -> (StatusCode, Vec<u8>) {
    let mut req = Request::builder().uri(path);
    if let Some(range) = range {
        req = req.header("Range", range);
    }
    let response = plamenu::build_router(f.state.clone())
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    (
        status,
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
}

#[sqlx::test(migrations = "../db/migrations")]
async fn complete_graph_is_durable_private_until_published_and_withdrawal_keeps_debt(pool: PgPool) {
    let f = fixture(pool).await;
    let item = service::request(&f.state, f.operator, f.post, "")
        .await
        .unwrap();
    let path = format!("/media/mirrors/{}/master.m3u8", item.id);
    assert_eq!(get(&f, &path, None).await.0, StatusCode::NOT_FOUND);
    finish(&f, item.id, "ready").await;
    let assets = db::assets(&f.state.pool, item.id, 1).await.unwrap();
    assert_eq!(assets.len(), 3);
    let ready = db::find(&f.state.pool, item.id).await.unwrap().unwrap();
    assert_eq!(
        ready.reserved_bytes,
        assets.iter().map(|a| a.bytes).sum::<i64>()
    );
    let (code, body) = get(&f, &path, None).await;
    assert_eq!(code, StatusCode::OK);
    assert!(!String::from_utf8(body).unwrap().contains("source.test"));
    let (code, body) = get(&f, &format!("{path}?canonical=true"), None).await;
    assert_eq!(code, StatusCode::OK);
    assert!(String::from_utf8(body).unwrap().contains("240/index.m3u8"));
    let (code, body) = get(
        &f,
        &format!("/media/mirrors/{}/film.mp4", item.id),
        Some("bytes=4-7"),
    )
    .await;
    assert_eq!(code, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, vec![4, 5, 6, 7]);
    for asset in &assets {
        assert_eq!(
            get(&f, &format!("/media/{}", asset.file_name), None)
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }
    let report = plamenu::media_cleanup_worker::reconcile(&f.state, false)
        .await
        .unwrap();
    assert_eq!(
        report.orphans, 0,
        "private mirror assets are legitimate references"
    );
    let create: serde_json::Value = sqlx::query_scalar(
        "SELECT activity FROM delivery_jobs WHERE activity->'object'->>'type'='CacheFile'",
    )
    .fetch_one(&f.state.pool)
    .await
    .unwrap();
    assert_eq!(create["actor"], "https://plamenu.test/actor");
    assert_eq!(
        create["object"]["url"]["href"],
        format!("https://plamenu.test/media/mirrors/{}/", item.id),
        "PeerTube appends the original media basename to a delivery base, not a master URL"
    );
    // Normal playback remains canonical and needs no living source server.
    f.stub.media.lock().unwrap().clear();
    let (code, master) = get(&f, &format!("/media/hls/{}/master.m3u8", f.media), None).await;
    assert_eq!(code, StatusCode::OK);
    assert!(String::from_utf8(master).unwrap().contains("/pl?"));
    let (code, bytes) = get(
        &f,
        &format!("/media/play/{}/video.mp4", f.media),
        Some("bytes=4-7"),
    )
    .await;
    assert_eq!(code, StatusCode::PARTIAL_CONTENT);
    assert_eq!(bytes, vec![4, 5, 6, 7]);
    service::withdraw(&f.state, item.id).await.unwrap();
    let queued_create: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM delivery_jobs WHERE activity->'object'->>'type'='CacheFile')",
    )
    .fetch_one(&f.state.pool)
    .await
    .unwrap();
    assert!(!queued_create, "unsent Create must not outlive its Undo");
    assert_eq!(get(&f, &path, None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        db::charged(&f.state.pool).await.unwrap(),
        ready.reserved_bytes
    );
    let undo: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM delivery_jobs WHERE activity->>'type'='Undo')",
    )
    .fetch_one(&f.state.pool)
    .await
    .unwrap();
    assert!(undo);
    assert_eq!(plamenu::media_cleanup_worker::run_due(&f.state).await, 3);
    assert_eq!(db::charged(&f.state.pool).await.unwrap(), 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn accepted_offer_is_actor_bound_and_undo_is_not_resurrected(pool: PgPool) {
    let one = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(3))
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let f = fixture(one).await;
    let mut peer = common::RemoteUser::new("mirror.test", "server");
    peer.actor.kind = "Application".into();
    let sender = plamenu::remote::store_remote_actor(&pool, &peer.actor)
        .await
        .unwrap();
    service::approve(&f.state, f.operator, "mirror.test", false, true)
        .await
        .unwrap();
    let offer = plamenu_ap::cache_file::CacheFile::new(
        "https://mirror.test/cachefiles/1".into(),
        "https://source.test/videos/1".into(),
        "https://mirror.test/media/mirrors/1/master.m3u8".into(),
        time::OffsetDateTime::now_utc() + time::Duration::hours(1),
    )
    .unwrap();
    let create = serde_json::json!({"type":"Create","id":"https://mirror.test/cachefiles/1/create","actor":peer.actor.id,"object":offer});
    let activity = serde_json::from_value(create.clone()).unwrap();
    assert!(
        service::incoming::handle(&f.state, &sender, &activity)
            .await
            .unwrap()
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM remote_video_mirrors WHERE withdrawn_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
    let mut forged = create.clone();
    forged["object"]["id"] = serde_json::json!("https://elsewhere.test/cachefiles/1");
    assert!(
        service::incoming::handle(&f.state, &sender, &serde_json::from_value(forged).unwrap())
            .await
            .is_err()
    );
    let undo = serde_json::from_value(
        serde_json::json!({"type":"Undo","actor":peer.actor.id,"object":create}),
    )
    .unwrap();
    assert!(
        service::incoming::handle(&f.state, &sender, &undo)
            .await
            .unwrap()
    );
    assert!(
        service::incoming::handle(&f.state, &sender, &activity)
            .await
            .unwrap()
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM remote_video_mirrors WHERE withdrawn_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        count, 0,
        "a replayed Create cannot resurrect an authenticated Undo"
    );
    // Source deletion and out-of-order delivery must not erase Undo knowledge.
    let mut before_create = create.clone();
    before_create["object"]["id"] = serde_json::json!("https://mirror.test/cachefiles/2");
    let first_undo = serde_json::from_value(
        serde_json::json!({"type":"Undo","actor":peer.actor.id,"object":before_create}),
    )
    .unwrap();
    service::incoming::handle(&f.state, &sender, &first_undo)
        .await
        .unwrap();
    service::incoming::handle(
        &f.state,
        &sender,
        &serde_json::from_value(before_create).unwrap(),
    )
    .await
    .unwrap();
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM remote_video_mirrors WHERE withdrawn_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        count, 0,
        "Undo arriving before Create still refuses resurrection"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn consent_update_revokes_warm_assets_and_public_download_does_not_grant_mirroring(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    video_distribution::upsert(
        &f.state.pool,
        video_distribution::Policy {
            media_id: f.media,
            download_enabled: true,
            embed_enabled: true,
            mirror_allowed: false,
        },
    )
    .await
    .unwrap();
    assert!(
        service::request(&f.state, f.operator, f.post, "")
            .await
            .is_err()
    );
    let item = service::request(
        &f.state,
        f.operator,
        f.post,
        "Creator explicitly authorized this test mirror.",
    )
    .await
    .unwrap();
    finish(&f, item.id, "ready").await;
    let path = format!("/media/mirrors/{}/film.mp4", item.id);
    assert_eq!(get(&f, &path, None).await.0, StatusCode::OK);
    sqlx::query("UPDATE statuses SET updated_at=clock_timestamp() WHERE id=$1")
        .bind(f.post)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(get(&f, &path, None).await.0, StatusCode::NOT_FOUND);
    finish(&f, item.id, "withdrawn").await;
    assert!(db::charged(&f.state.pool).await.unwrap() > 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn unfinished_live_dependency_fails_without_public_offer(pool: PgPool) {
    let f = fixture(pool).await;
    f.stub.media.lock().unwrap().get_mut(CHILD).unwrap().bytes =
        b"#EXTM3U\n#EXTINF:2,\nfilm.mp4\n".to_vec();
    let item = service::request(&f.state, f.operator, f.post, "")
        .await
        .unwrap();
    finish(&f, item.id, "failed").await;
    assert!(
        db::assets(&f.state.pool, item.id, 1)
            .await
            .unwrap()
            .is_empty()
    );
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM delivery_jobs WHERE activity->'object'->>'type'='CacheFile')",
    )
    .fetch_one(&f.state.pool)
    .await
    .unwrap();
    assert!(!exists);
    assert!(
        db::charged(&f.state.pool).await.unwrap() > 0,
        "already-written private files stay charged"
    );
    plamenu::media_cleanup_worker::run_due(&f.state).await;
    assert_eq!(db::charged(&f.state.pool).await.unwrap(), 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn cancelled_worker_cannot_refund_or_remove_a_still_running_owned_writer(pool: PgPool) {
    use plamenu::storage::MediaStore as _;
    let mut f = fixture(pool).await;
    let store = Arc::new(HeldWriter::default());
    f.state.media = store.clone();
    let item = service::request(&f.state, f.operator, f.post, "")
        .await
        .unwrap();
    let worker = service::worker::spawn(f.state.clone());
    tokio::time::timeout(Duration::from_secs(10), store.entered.notified())
        .await
        .unwrap();
    worker.abort();
    let _ = worker.await;
    let state = f.state.clone();
    let withdrawal = tokio::spawn(async move { service::withdraw(&state, item.id).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !withdrawal.is_finished(),
        "withdrawal must wait for the owned filesystem acknowledgement"
    );
    assert_eq!(
        db::charged(&f.state.pool).await.unwrap(),
        item.reserved_bytes
    );
    assert_eq!(plamenu::media_cleanup_worker::run_due(&f.state).await, 0);
    store.release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), withdrawal)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        store.list().await.unwrap().len(),
        1,
        "owned scratch survived caller cancellation"
    );
    let debt = db::charged(&f.state.pool).await.unwrap();
    assert!(debt > 0 && debt < item.reserved_bytes);
    assert_eq!(plamenu::media_cleanup_worker::run_due(&f.state).await, 1);
    assert!(store.list().await.unwrap().is_empty());
    assert_eq!(db::charged(&f.state.pool).await.unwrap(), 0);
}
