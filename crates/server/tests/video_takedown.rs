//! Origin revocation precedes physical deletion; current channel authority
//! gates every native playback lane. Fixtures test authority, not decoding.
mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use plamenu::{
    AppState,
    actions::{self, PostParams},
    federation::BoxFuture,
    storage::{MediaRead, MediaStore, MemoryStore},
};
use plamenu_ap::activity::PostKind;
use plamenu_db::{PgPool, id, media, media_cleanup, native_video_hls as hls};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tower::ServiceExt;

#[derive(Default)]
struct DeleteFault {
    inner: MemoryStore,
    fail: AtomicBool,
}
impl MediaStore for DeleteFault {
    fn put(&self, key: &str, data: Vec<u8>) -> BoxFuture<'_, std::io::Result<()>> {
        self.inner.put(key, data)
    }
    fn put_file(&self, key: &str, path: &Path) -> BoxFuture<'_, std::io::Result<()>> {
        self.inner.put_file(key, path)
    }
    fn get(&self, key: &str) -> BoxFuture<'_, std::io::Result<Vec<u8>>> {
        self.inner.get(key)
    }
    fn open(&self, key: &str) -> BoxFuture<'_, std::io::Result<MediaRead>> {
        self.inner.open(key)
    }
    fn list(&self) -> BoxFuture<'_, std::io::Result<Vec<String>>> {
        self.inner.list()
    }
    fn delete(&self, key: &str) -> BoxFuture<'_, std::io::Result<()>> {
        if self.fail.load(Ordering::Relaxed) {
            Box::pin(async { Err(std::io::ErrorKind::PermissionDenied.into()) })
        } else {
            self.inner.delete(key)
        }
    }
}
async fn get(state: &AppState, path: &str) -> StatusCode {
    let response = plamenu::build_router(state.clone())
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    if response.status() == StatusCode::NOT_FOUND {
        assert_eq!(
            response.headers()[axum::http::header::CACHE_CONTROL],
            "private, no-store",
            "{path}"
        );
    }
    response.status()
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn failed_cleanup_retains_revocation_and_eventually_acknowledges_repaired_storage(
    pool: PgPool,
) {
    let store = Arc::new(DeleteFault::default());
    store.fail.store(true, Ordering::Relaxed);
    let mut state = common::test_state_with(pool.clone(), Arc::default());
    state.media = store.clone();
    let key = "12345.mp4";
    store.put(key, vec![1, 2, 3]).await.unwrap();
    assert_eq!(get(&state, "/media/12345.mp4").await, StatusCode::OK);
    media_cleanup::enqueue_many(&pool, &[key.into()])
        .await
        .unwrap();
    assert_eq!(get(&state, "/media/12345.mp4").await, StatusCode::NOT_FOUND);
    assert_eq!(plamenu::media_cleanup_worker::run_due(&state).await, 1);
    assert!(
        store.get(key).await.is_ok(),
        "failed physical delete retains bytes"
    );
    sqlx::query("UPDATE media_cleanup_jobs SET attempts=10,run_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(plamenu::media_cleanup_worker::run_due(&state).await, 1);
    assert_eq!(
        media_cleanup::pending_count(&pool).await.unwrap(),
        1,
        "a failed deletion must retain its revocation tombstone"
    );
    assert_eq!(get(&state, "/media/12345.mp4").await, StatusCode::NOT_FOUND);
    let delayed: bool =
        sqlx::query_scalar("SELECT run_at>now()+interval '23 hours' FROM media_cleanup_jobs")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(delayed, "exhausted work backs off rather than spinning");
    store.fail.store(false, Ordering::Relaxed);
    sqlx::query("UPDATE media_cleanup_jobs SET run_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(plamenu::media_cleanup_worker::run_due(&state).await, 1);
    assert_eq!(media_cleanup::pending_count(&pool).await.unwrap(), 0);
    assert!(store.get(key).await.is_err());
    assert_eq!(get(&state, "/media/12345.mp4").await, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn mirror_failover_keeps_authority_and_failed_replica_delete_keeps_cleanup_debt(
    pool: PgPool,
) {
    let mut f = film(pool.clone()).await;
    let primary = tempfile::tempdir().unwrap();
    let replica = tempfile::tempdir().unwrap();
    let store = Arc::new(
        plamenu::storage_mirror::MirroredDiskStore::new(
            primary.path().into(),
            replica.path().into(),
            64 * 1024 * 1024,
            0,
        )
        .unwrap(),
    );
    for key in f.state.media.list().await.unwrap() {
        store
            .put(&key, f.state.media.get(&key).await.unwrap())
            .await
            .unwrap();
    }
    f.state.media = store;
    public_lanes(&f, true).await;
    let key = format!("{}.mp4", f.media);
    let path = format!("/media/{key}");
    tokio::fs::remove_file(primary.path().join(&key))
        .await
        .unwrap();
    public_lanes(&f, true).await;
    plamenu_db::account::suspend(&pool, f.owner, "local")
        .await
        .unwrap();
    public_lanes(&f, false).await;
    plamenu_db::account::unsuspend(&pool, f.owner)
        .await
        .unwrap();
    public_lanes(&f, true).await;

    let saved = replica.path().join("held.private");
    tokio::fs::rename(replica.path().join(&key), &saved)
        .await
        .unwrap();
    tokio::fs::create_dir(replica.path().join(&key))
        .await
        .unwrap();
    media_cleanup::enqueue_many(&pool, std::slice::from_ref(&key))
        .await
        .unwrap();
    assert_eq!(get(&f.state, &path).await, StatusCode::NOT_FOUND);
    assert_eq!(plamenu::media_cleanup_worker::run_due(&f.state).await, 1);
    assert_eq!(media_cleanup::pending_count(&pool).await.unwrap(), 1);
    assert!(saved.exists());
    tokio::fs::remove_dir(replica.path().join(&key))
        .await
        .unwrap();
    tokio::fs::rename(saved, replica.path().join(&key))
        .await
        .unwrap();
    assert_eq!(get(&f.state, &path).await, StatusCode::NOT_FOUND);
    sqlx::query("UPDATE media_cleanup_jobs SET run_at=now() WHERE file_name=$1")
        .bind(&key)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(plamenu::media_cleanup_worker::run_due(&f.state).await, 1);
    assert_eq!(media_cleanup::pending_count(&pool).await.unwrap(), 0);
    assert!(!replica.path().join(key).exists());
}

struct Film {
    state: AppState,
    media: i64,
    status: i64,
    channel: i64,
    owner: i64,
    hash: String,
    rendition: String,
}
async fn film(pool: PgPool) -> Film {
    let author = common::create_local_account(&pool, "alice", "Alice").await;
    let state = common::test_state_with(pool.clone(), Arc::default());
    let channel = plamenu::video::create_channel(&state, &author, "films", "Films")
        .await
        .unwrap();
    let mid = id::next();
    let file = format!("{mid}.mp4");
    let poster = format!("{mid}.jpg");
    media::create_local(
        &pool,
        media::NewLocalMedia {
            kind: "video",
            duration: Some(5.0),
            width: Some(320),
            height: Some(240),
            small: Some(media::SmallStyle {
                file_name: &poster,
                width: 320,
                height: 240,
            }),
            ..media::NewLocalMedia::new(author.id, mid, &file, "video/mp4")
        },
    )
    .await
    .unwrap();
    state.media.put(&file, vec![1; 128]).await.unwrap();
    let mut jpeg = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(320, 240)
        .write_to(&mut jpeg, image::ImageFormat::Jpeg)
        .unwrap();
    state.media.put(&poster, jpeg.into_inner()).await.unwrap();
    let (status, _) = actions::post_video(
        &state,
        PostParams {
            username: "alice",
            media_ids: &[mid],
            title: Some("Film"),
            text: "An authority fixture",
            visibility: "public",
            kind: PostKind::Video,
            ..Default::default()
        },
        channel.id,
    )
    .await
    .unwrap();
    sqlx::query("INSERT INTO video_drafts(media_id,account_id,channel_id,title,description,visibility,source_file,source_cleanup_job_id,source_bytes,reserved_bytes,state,generation) VALUES($1,$2,$3,'Film','','public',$4,$5,128,128,'ready',1)")
        .bind(mid).bind(author.id).bind(channel.id).bind(format!("{mid}.orig")).bind(id::next()).execute(&pool).await.unwrap();
    let rendition = format!("hls{mid}.draft.hls240.m4s");
    sqlx::query("INSERT INTO video_draft_outputs(file_name,media_id,generation,cleanup_job_id) VALUES($1,$2,1,$3)")
        .bind(&rendition).bind(mid).bind(id::next()).execute(&pool).await.unwrap();
    let hash = "abcdefghijklmnopqrst".to_owned();
    sqlx::query("INSERT INTO native_video_hls_renditions(media_id,generation,height,width,bandwidth,size_bytes,file_name,playlist,segment_sha256,p2p_infohash) VALUES($1,1,240,320,10000,128,$2,'#EXTM3U\n#EXT-X-VERSION:7\n',jsonb_build_object('0',repeat('a',64)),$3)")
        .bind(mid).bind(&rendition).bind(&hash).execute(&pool).await.unwrap();
    state.media.put(&rendition, vec![3; 128]).await.unwrap();
    sqlx::query("UPDATE media_attachments SET caption_vtt='WEBVTT\n\n00:00.000 --> 00:01.000\nHello\n' WHERE id=$1")
        .bind(mid).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO video_captions(media_id,language,vtt_body) VALUES($1,'en','WEBVTT\n\n00:00.000 --> 00:01.000\nHello\n')")
        .bind(mid).execute(&pool).await.unwrap();
    Film {
        state,
        media: mid,
        status: status.id,
        channel: channel.id,
        owner: author.id,
        hash,
        rendition,
    }
}
async fn public_lanes(f: &Film, allowed: bool) {
    let pool = &f.state.pool;
    assert_eq!(
        media::public_access_blocked(pool, f.media).await.unwrap(),
        !allowed
    );
    assert_eq!(
        !hls::public_ladder(pool, f.media).await.unwrap().is_empty(),
        allowed
    );
    assert_eq!(
        hls::public_playlist(pool, f.media, 240, None)
            .await
            .unwrap()
            .is_some(),
        allowed
    );
    assert_eq!(
        hls::public_sha256(pool, f.media, None)
            .await
            .unwrap()
            .is_some(),
        allowed
    );
    assert_eq!(
        hls::public_infohash_exists(pool, &f.hash).await.unwrap(),
        allowed
    );
    assert_eq!(
        !hls::public_peer_uuids(pool, &[f.media])
            .await
            .unwrap()
            .is_empty(),
        allowed
    );
    assert_eq!(
        media::public_caption_vtt(pool, f.media)
            .await
            .unwrap()
            .is_some(),
        allowed
    );
    assert_eq!(
        plamenu_db::video_captions::public_source(pool, f.media, "en")
            .await
            .unwrap()
            .is_some(),
        allowed
    );
    for path in [
        format!("/media/{}.mp4", f.media),
        format!("/media/{}.jpg", f.media),
        format!("/media/{}", f.rendition),
        format!("/media/hls/{}/master.m3u8", f.media),
        format!("/media/hls/{}/native/240/index.m3u8", f.media),
        format!("/media/hls/{}/sha256.json", f.media),
        format!("/media/{}/captions.vtt", f.media),
    ] {
        assert_eq!(
            get(&f.state, &path).await,
            if allowed {
                StatusCode::OK
            } else {
                StatusCode::NOT_FOUND
            },
            "{path}"
        );
    }
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn current_owner_suspension_revokes_every_native_lane_after_transfer(pool: PgPool) {
    let f = film(pool.clone()).await;
    let owner = common::create_local_account(&pool, "bob", "Bob").await;
    // Persisted post-transfer state: social authorship remains with Alice.
    sqlx::query("UPDATE video_channels SET owner_account_id=$1 WHERE account_id=$2")
        .bind(owner.id)
        .bind(f.channel)
        .execute(&pool)
        .await
        .unwrap();
    public_lanes(&f, true).await;
    plamenu_db::account::suspend(&pool, owner.id, "local")
        .await
        .unwrap();
    assert!(
        !plamenu_db::account::find_by_id(&pool, f.owner)
            .await
            .unwrap()
            .unwrap()
            .suspended()
    );
    public_lanes(&f, false).await;
    sqlx::query("UPDATE accounts SET suspended_at=NULL WHERE id=$1")
        .bind(owner.id)
        .execute(&pool)
        .await
        .unwrap();
    public_lanes(&f, true).await;
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn tombstoned_or_private_native_status_revokes_playback_and_swarm(pool: PgPool) {
    let f = film(pool.clone()).await;
    public_lanes(&f, true).await;
    for visibility in ["private", "direct"] {
        sqlx::query("UPDATE statuses SET visibility=$1 WHERE id=$2")
            .bind(visibility)
            .bind(f.status)
            .execute(&pool)
            .await
            .unwrap();
        public_lanes(&f, false).await;
    }
    sqlx::query("UPDATE statuses SET visibility='unlisted' WHERE id=$1")
        .bind(f.status)
        .execute(&pool)
        .await
        .unwrap();
    public_lanes(&f, true).await;
    sqlx::query("UPDATE statuses SET deleted_at=now() WHERE id=$1")
        .bind(f.status)
        .execute(&pool)
        .await
        .unwrap();
    public_lanes(&f, false).await;
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn live_session_dvr_and_swarm_obey_current_owner_and_status_authority(pool: PgPool) {
    async fn check(f: &Film, hash: &str, allowed: bool) {
        let pool = &f.state.pool;
        assert_eq!(
            plamenu_db::native_live::public_session(pool, f.media)
                .await
                .unwrap()
                .is_some(),
            allowed
        );
        assert_eq!(
            plamenu_db::native_live::public_dvr(pool, f.media)
                .await
                .unwrap()
                .is_some(),
            allowed
        );
        assert_eq!(
            !plamenu_db::native_live::public_peer_sessions(pool, &[f.media])
                .await
                .unwrap()
                .is_empty(),
            allowed
        );
        assert_eq!(
            plamenu_db::native_live::public_infohash_exists(pool, hash)
                .await
                .unwrap(),
            allowed
        );
    }
    let f = film(pool.clone()).await;
    let owner = common::create_local_account(&pool, "bob", "Bob").await;
    sqlx::query("UPDATE video_channels SET owner_account_id=$1 WHERE account_id=$2")
        .bind(owner.id)
        .bind(f.channel)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE media_attachments SET live_state='live' WHERE id=$1")
        .bind(f.media)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO native_live_broadcasts(media_id,status_id,owner_account_id,channel_id,ingest_key_hash,state,dvr_window_seconds) VALUES($1,$2,$3,$4,$5,'live',300)")
        .bind(f.media).bind(f.status).bind(f.owner).bind(f.channel).bind(vec![1_u8;32]).execute(&pool).await.unwrap();
    let hash = "12345678901234567890";
    sqlx::query("INSERT INTO native_live_sessions(session_id,media_id,generation,p2p_infohash) VALUES('authority-live', $1,1,$2)")
        .bind(f.media).bind(hash).execute(&pool).await.unwrap();
    check(&f, hash, true).await;
    plamenu_db::account::suspend(&pool, owner.id, "local")
        .await
        .unwrap();
    check(&f, hash, false).await;
    sqlx::query("UPDATE accounts SET suspended_at=NULL WHERE id=$1")
        .bind(owner.id)
        .execute(&pool)
        .await
        .unwrap();
    check(&f, hash, true).await;
    sqlx::query("UPDATE statuses SET deleted_at=now() WHERE id=$1")
        .bind(f.status)
        .execute(&pool)
        .await
        .unwrap();
    check(&f, hash, false).await;
}
