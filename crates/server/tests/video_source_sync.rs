//! Native recurring feed discovery, private copies and atomic resource fencing.
mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use plamenu::{AppState, storage::LocalDiskStore, video_source_sync as service};
use plamenu_db::{PgPool, video_draft, video_source_sync as db};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

const ROOT: &str = "https://source.example/channels/films";
const OUTBOX: &str = "https://source.example/channels/films/outbox";
const PAGE: &str = "https://source.example/channels/films/outbox?page=1";

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
    config.native_video.max_upload_mib = 1;
    config.native_video.unfinished_drafts = 64;
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
fn film(uri: &str) -> Value {
    json!({"id":uri,"type":"Video","attributedTo":[ROOT],"to":[plamenu_ap::activity::PUBLIC],"name":"A source film","content":"<p>A credited film.</p>",
        "summary":"Flashing test imagery","sensitive":false,"category":{"identifier":"15"},
        "licence":{"identifier":"2","url":"https://creativecommons.org/licenses/by-sa/4.0/"},
        "url":[{"mediaType":"video/mp4","href":"https://source.example/source.mp4","height":360}]})
}
fn feed(f: &Fixture, count: usize) {
    let mut objects = f.stub.objects.lock().unwrap();
    objects.insert(
        ROOT.into(),
        json!({"id":ROOT,"type":"Group","outbox":OUTBOX}),
    );
    objects.insert(
        OUTBOX.into(),
        json!({"id":OUTBOX,"type":"OrderedCollection","first":PAGE,"totalItems":count}),
    );
    let items = (0..count)
        .map(|i| {
            let uri = format!("https://source.example/videos/{i}");
            objects.insert(uri.clone(), film(&uri));
            json!({"type":"Announce","actor":ROOT,"object":uri})
        })
        .collect::<Vec<_>>();
    objects.insert(
        PAGE.into(),
        json!({"id":PAGE,"type":"OrderedCollectionPage","partOf":OUTBOX,"orderedItems":items}),
    );
}
async fn create(f: &Fixture) -> i64 {
    db::create(&f.state.pool, f.owner, f.channel, ROOT, 15)
        .await
        .unwrap()
        .unwrap()
}
async fn due(f: &Fixture, sync: i64) {
    sqlx::query(
        "UPDATE video_source_syncs SET next_check_at=now()-interval '1 second' WHERE id=$1",
    )
    .bind(sync)
    .execute(&f.state.pool)
    .await
    .unwrap();
}
async fn count(f: &Fixture, table: &str) -> i64 {
    let statement = match table {
        "video_source_sync_items" => "SELECT count(*) FROM video_source_sync_items",
        "statuses" => "SELECT count(*) FROM statuses",
        "video_drafts" => "SELECT count(*) FROM video_drafts",
        "video_source_imports" => "SELECT count(*) FROM video_source_imports",
        "video_storage_reservations" => "SELECT count(*) FROM video_storage_reservations",
        _ => panic!("Unknown fixture table"),
    };
    sqlx::query_scalar(statement)
        .fetch_one(&f.state.pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn source_discovers_new_videos_as_private_labelled_drafts_without_publication(pool: PgPool) {
    let f = fixture(pool).await;
    feed(&f, 1);
    let sync = create(&f).await;
    f.stub.serve_media(
        "https://source.example/source.mp4",
        "video/mp4",
        b"private source bytes".to_vec(),
    );
    assert!(service::run_one(&f.state).await.unwrap());
    assert_eq!(count(&f, "video_source_sync_items").await, 1);
    assert_eq!(count(&f, "statuses").await, 0);
    assert!(f.stub.deliveries.lock().unwrap().is_empty());
    let media: i64 = sqlx::query_scalar("SELECT media_id FROM video_source_sync_items")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    let draft = video_draft::find_owned(&f.state.pool, media, f.owner)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.state, "receiving");
    assert_eq!(draft.visibility, "unlisted");
    assert!(
        draft
            .description
            .contains("Source: https://source.example/videos/0")
    );
    assert!(!draft.description.contains("<p>"));
    let labels = plamenu_db::video_metadata::for_media(&f.state.pool, &[media])
        .await
        .unwrap()
        .remove(&media)
        .unwrap();
    assert_eq!(
        (labels.category, labels.licence, labels.sensitive),
        (Some(15), Some(2), true)
    );
    assert!(labels.licence_version_known);
    assert_eq!(labels.spoiler_text, "Flashing test imagery");
    assert!(
        plamenu::video_source_import::run_one(&f.state)
            .await
            .unwrap()
    );
    assert_eq!(
        f.state.media.get(&draft.source_file).await.unwrap(),
        b"private source bytes"
    );
    assert!(!service::run_one(&f.state).await.unwrap());
    due(&f, sync).await;
    assert!(service::run_one(&f.state).await.unwrap());
    assert_eq!(count(&f, "video_source_sync_items").await, 1);
    feed(&f, 2);
    due(&f, sync).await;
    assert!(service::run_one(&f.state).await.unwrap());
    assert_eq!(count(&f, "video_source_sync_items").await, 2);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn bounded_feed_cursor_progress_and_quota_refusal_do_not_drop_or_orphan_items(pool: PgPool) {
    let mut f = fixture(pool).await;
    feed(&f, 30);
    let sync = create(&f).await;
    assert!(service::run_one(&f.state).await.unwrap());
    assert_eq!(count(&f, "video_source_sync_items").await, 20);
    assert_eq!(
        db::list(&f.state.pool, f.owner).await.unwrap()[0]
            .cursor_url
            .as_deref(),
        Some(PAGE)
    );
    due(&f, sync).await;
    assert!(service::run_one(&f.state).await.unwrap());
    assert_eq!(count(&f, "video_source_sync_items").await, 30);
    assert!(
        db::list(&f.state.pool, f.owner).await.unwrap()[0]
            .cursor_url
            .is_none()
    );
    let mut changed = (*f.state.config).clone();
    changed.native_video.unfinished_drafts = 1;
    f.state.config = Arc::new(changed);
    feed(&f, 31);
    due(&f, sync).await;
    assert!(service::run_one(&f.state).await.unwrap());
    assert_eq!(
        db::list(&f.state.pool, f.owner).await.unwrap()[0].last_result,
        "quota"
    );
    assert_eq!(count(&f, "video_drafts").await, 30);
    assert_eq!(count(&f, "video_source_imports").await, 30);
    assert_eq!(count(&f, "video_storage_reservations").await, 30);
    assert_eq!(count(&f, "video_source_sync_items").await, 30);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn pauses_restarts_ownership_and_dedup_survive_copy_deletion(pool: PgPool) {
    let f = fixture(pool).await;
    feed(&f, 1);
    let sync = create(&f).await;
    let old = db::claim(&f.state.pool).await.unwrap().unwrap();
    assert!(
        db::set_enabled(&f.state.pool, sync, f.owner, false)
            .await
            .unwrap()
    );
    assert!(!db::renew(&f.state.pool, &old).await.unwrap());
    assert!(!db::finish(&f.state.pool, &old, "ok", None).await.unwrap());
    assert!(!service::run_one(&f.state).await.unwrap());
    assert!(
        !db::set_enabled(&f.state.pool, sync, f.owner + 100, true)
            .await
            .unwrap()
    );
    assert!(
        db::set_enabled(&f.state.pool, sync, f.owner, true)
            .await
            .unwrap()
    );
    due(&f, sync).await;
    let reclaim = db::claim(&f.state.pool).await.unwrap().unwrap();
    assert!(reclaim.generation > old.generation);
    sqlx::query("UPDATE video_source_syncs SET lease_until=now()-interval '1 second' WHERE id=$1")
        .bind(sync)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert!(service::run_one(&f.state).await.unwrap());
    let media: i64 = sqlx::query_scalar("SELECT media_id FROM video_source_sync_items")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM media_attachments WHERE id=$1")
        .bind(media)
        .execute(&f.state.pool)
        .await
        .unwrap();
    // This metadata-only fixture has never written media; provenance persists.
    assert_eq!(count(&f, "video_source_sync_items").await, 1);
    due(&f, sync).await;
    assert!(service::run_one(&f.state).await.unwrap());
    assert_eq!(count(&f, "video_drafts").await, 0);
    sqlx::query("UPDATE accounts SET suspended_at=now() WHERE id=$1")
        .bind(f.owner)
        .execute(&f.state.pool)
        .await
        .unwrap();
    due(&f, sync).await;
    assert!(!service::run_one(&f.state).await.unwrap());
    assert!(!db::list(&f.state.pool, f.owner).await.unwrap()[0].enabled);
    assert!(
        !db::remove(&f.state.pool, sync, f.owner + 100)
            .await
            .unwrap()
    );
    assert!(db::remove(&f.state.pool, sync, f.owner).await.unwrap());
    assert_eq!(count(&f, "video_source_sync_items").await, 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn source_limits_concurrent_creation_and_manual_check_rate_are_bounded(pool: PgPool) {
    let f = fixture(pool).await;
    assert!(
        db::create(&f.state.pool, f.owner, f.channel, ROOT, 1)
            .await
            .unwrap()
            .is_none()
    );
    let jobs = futures_util::future::join_all((0..10).map(|i| {
        let pool = f.state.pool.clone();
        let owner = f.owner;
        let channel = f.channel;
        async move {
            let url = format!("https://source.example/{i}");
            db::create(&pool, owner, channel, &url, 15).await
        }
    }))
    .await;
    assert_eq!(
        jobs.into_iter()
            .filter(|j| j.as_ref().unwrap().is_some())
            .count(),
        5
    );
    let sources = db::list(&f.state.pool, f.owner).await.unwrap();
    assert_eq!(sources.len(), 5);
    let claimed = db::claim(&f.state.pool).await.unwrap().unwrap();
    assert!(
        !db::check_now(&f.state.pool, claimed.id, f.owner)
            .await
            .unwrap()
    );
    assert!(
        db::finish(&f.state.pool, &claimed, "ok", None)
            .await
            .unwrap()
    );
    assert!(
        !db::check_now(&f.state.pool, claimed.id, f.owner)
            .await
            .unwrap()
    );
    assert!(
        db::create(
            &f.state.pool,
            f.owner + 100,
            f.channel,
            "https://source.example/foreign",
            15
        )
        .await
        .unwrap()
        .is_none()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn hostile_pages_objects_audiences_and_untrusted_licences_are_not_adopted(pool: PgPool) {
    let f = fixture(pool).await;
    for kind in [
        "cross_page",
        "loop",
        "identity",
        "foreign_video",
        "private",
        "live",
    ] {
        feed(&f, 1);
        let sync = create(&f).await;
        {
            let mut objects = f.stub.objects.lock().unwrap();
            match kind {
                "cross_page" => {
                    objects.get_mut(OUTBOX).unwrap()["first"] = json!("https://other.example/page");
                }
                "loop" => objects.get_mut(PAGE).unwrap()["next"] = json!(PAGE),
                "identity" => {
                    objects.get_mut(PAGE).unwrap()["id"] = json!("https://source.example/wrong");
                }
                "foreign_video" => {
                    objects.get_mut("https://source.example/videos/0").unwrap()["attributedTo"] =
                        json!(["https://other.example/actor"]);
                }
                "private" => {
                    objects.get_mut("https://source.example/videos/0").unwrap()["to"] =
                        json!(["https://source.example/followers"]);
                }
                "live" => {
                    objects.get_mut("https://source.example/videos/0").unwrap()["isLiveBroadcast"] =
                        json!(true);
                }
                _ => unreachable!(),
            }
        }
        let _ = service::run_one(&f.state).await;
        assert_eq!(count(&f, "video_source_sync_items").await, 0, "{kind}");
        assert!(db::remove(&f.state.pool, sync, f.owner).await.unwrap());
    }
    assert!(
        !f.stub
            .fetches
            .lock()
            .unwrap()
            .iter()
            .any(|url| url.starts_with("https://other.example/"))
    );
    feed(&f, 1);
    create(&f).await;
    f.stub
        .objects
        .lock()
        .unwrap()
        .get_mut("https://source.example/videos/0")
        .unwrap()["licence"]["url"] = json!("https://other.example/fake-grant");
    assert!(service::run_one(&f.state).await.unwrap());
    let licence: Option<i32> = sqlx::query_scalar("SELECT licence FROM video_metadata")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert!(licence.is_none());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn pausing_during_real_admission_wait_rolls_back_all_reservations(pool: PgPool) {
    let f = fixture(pool).await;
    feed(&f, 1);
    let sync = create(&f).await;
    let mut lock = f.state.pool.begin().await.unwrap();
    plamenu_db::video_storage::lock(&mut lock).await.unwrap();
    let state = f.state.clone();
    let worker = tokio::spawn(async move { service::run_one(&state).await });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if f.stub
            .fetches
            .lock()
            .unwrap()
            .iter()
            .any(|url| url == "https://source.example/videos/0")
        {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::task::yield_now().await;
    }
    assert!(
        db::set_enabled(&f.state.pool, sync, f.owner, false)
            .await
            .unwrap()
    );
    lock.rollback().await.unwrap();
    assert!(worker.await.unwrap().is_err());
    assert_eq!(count(&f, "video_drafts").await, 0);
    assert_eq!(count(&f, "video_source_imports").await, 0);
    assert_eq!(count(&f, "video_storage_reservations").await, 0);
    assert_eq!(count(&f, "video_source_sync_items").await, 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn public_playlist_can_reference_a_guarded_foreign_video_without_copying_clip_or_actor_identity(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let playlist = "https://source.example/playlists/films";
    let page = "https://source.example/playlists/films/pages/1";
    let video = "https://other.example/videos/film";
    {
        let mut objects = f.stub.objects.lock().unwrap();
        objects.insert(playlist.into(),json!({"id":playlist,"type":"Playlist","to":[plamenu_ap::activity::PUBLIC],"first":page}));
        objects.insert(page.into(),json!({"id":page,"type":"OrderedCollectionPage","partOf":playlist,"orderedItems":[{"type":"PlaylistElement","url":video,"startTimestamp":10,"stopTimestamp":20},{"type":"PlaylistElement","url":video}]}));
        objects.insert(video.into(), film(video));
    }
    db::create(&f.state.pool, f.owner, f.channel, playlist, 15)
        .await
        .unwrap()
        .unwrap();
    assert!(service::run_one(&f.state).await.unwrap());
    assert_eq!(count(&f, "video_source_sync_items").await, 1);
    assert_eq!(count(&f, "statuses").await, 0);
    let draft: String = sqlx::query_scalar("SELECT description FROM video_drafts")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert!(draft.contains(video));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn history_limit_pauses_without_forgetting_deduplication_or_allocating_more_media(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    feed(&f, 1);
    let sync = create(&f).await;
    sqlx::query("INSERT INTO video_source_sync_items(sync_id,source_uri) SELECT $1,'https://source.example/old/'||generate_series(1,10000)::text").bind(sync).execute(&f.state.pool).await.unwrap();
    assert!(service::run_one(&f.state).await.unwrap());
    let source = db::list(&f.state.pool, f.owner).await.unwrap().remove(0);
    assert!(!source.enabled);
    assert_eq!(source.last_result, "history_limit");
    assert_eq!(count(&f, "video_drafts").await, 0);
    assert_eq!(count(&f, "video_source_sync_items").await, 10000);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn peertube_element_iris_advance_within_an_embedded_playlist_and_deduplicate_videos(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let playlist = "https://source.example/playlists/films";
    let video = "https://other.example/videos/film";
    {
        let mut objects = f.stub.objects.lock().unwrap();
        let elements: Vec<_> = (0..30)
            .map(|i| {
                let uri = format!("https://source.example/playlists/elements/{i}");
                objects.insert(
                    uri.clone(),
                    json!({"id":uri,"type":"PlaylistElement","url":video}),
                );
                uri
            })
            .collect();
        objects.insert(playlist.into(), json!({"id":playlist,"type":"Playlist","to":[plamenu_ap::activity::PUBLIC],"orderedItems":elements}));
        objects.insert(video.into(), film(video));
    }
    let sync = db::create(&f.state.pool, f.owner, f.channel, playlist, 15)
        .await
        .unwrap()
        .unwrap();
    assert!(service::run_one(&f.state).await.unwrap());
    let source = db::list(&f.state.pool, f.owner).await.unwrap().remove(0);
    assert_eq!(source.cursor_url.as_deref(), Some(playlist));
    assert_eq!(source.cursor_position, 20);
    assert_eq!(count(&f, "video_source_sync_items").await, 1);
    assert_eq!(count(&f, "video_storage_reservations").await, 1);
    due(&f, sync).await;
    assert!(service::run_one(&f.state).await.unwrap());
    assert!(
        db::list(&f.state.pool, f.owner).await.unwrap()[0]
            .cursor_url
            .is_none()
    );
    assert_eq!(count(&f, "video_source_sync_items").await, 1);
    let fetched = f.stub.fetches.lock().unwrap();
    assert!(fetched.iter().any(|uri| uri.ends_with("/elements/29")));
    assert_eq!(
        fetched.iter().filter(|uri| uri.as_str() == video).count(),
        1
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn html_sources_require_rights_csrf_scopes_and_current_authority_and_status_is_private(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    feed(&f, 1);
    let user = plamenu_db::user::set_credentials(
        &f.state.pool,
        f.owner,
        Some("alice@plamenu.test"),
        "hash",
    )
    .await
    .unwrap();
    let web = plamenu::web::session::ensure_web_app(&f.state)
        .await
        .unwrap();
    let app = plamenu::build_router(f.state.clone());
    for scope in ["read", "read write:statuses", "read write"] {
        let raw = format!("source-scope-{scope}");
        let cookie = format!("__Host-plamenu_session={raw}");
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
        let request = |path: &str, body: String| {
            Request::builder()
                .method("POST")
                .uri(path)
                .header("Cookie", &cookie)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap()
        };
        let form = format!(
            "csrf={csrf}&channel={}&source_url={ROOT}&interval=15",
            f.channel
        );
        assert_eq!(
            app.clone()
                .oneshot(request(
                    "/web/videos/sources",
                    format!("{}&confirm=yes", form.replace(&csrf, "wrong"))
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        let response = app
            .clone()
            .oneshot(request("/web/videos/sources", form.clone()))
            .await
            .unwrap();
        if scope == "read write" {
            assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(response.headers()["Referrer-Policy"], "no-referrer");
            let text = String::from_utf8(
                response
                    .into_body()
                    .collect()
                    .await
                    .unwrap()
                    .to_bytes()
                    .to_vec(),
            )
            .unwrap();
            assert!(text.contains(ROOT));
            assert!(text.contains("role=\"alert\""));
            assert_eq!(count(&f, "video_source_sync_items").await, 0);
            assert!(f.stub.fetches.lock().unwrap().is_empty());
            assert_eq!(
                app.clone()
                    .oneshot(request(
                        "/web/videos/sources",
                        format!("{form}&confirm=yes")
                    ))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::SEE_OTHER
            );
            let get = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/videos/sources/state")
                        .header("Cookie", &cookie)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(get.headers()["Cache-Control"], "private, no-store");
            assert_eq!(get.headers()["Vary"], "Cookie");
            let text =
                String::from_utf8(get.into_body().collect().await.unwrap().to_bytes().to_vec())
                    .unwrap();
            assert!(!text.contains(ROOT));
            assert!(!text.contains(&csrf));
            assert!(!text.contains("generation"));
            let sync = db::list(&f.state.pool, f.owner).await.unwrap()[0].id;
            assert_eq!(
                app.clone()
                    .oneshot(request(
                        &format!("/web/videos/sources/{sync}/remove"),
                        format!("csrf={csrf}")
                    ))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::NOT_FOUND
            );
            assert_eq!(
                app.clone()
                    .oneshot(request(
                        &format!("/web/videos/sources/{sync}/pause"),
                        format!("csrf={csrf}")
                    ))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::SEE_OTHER
            );
            assert!(!service::run_one(&f.state).await.unwrap());
            assert_eq!(
                app.clone()
                    .oneshot(request(
                        &format!("/web/videos/sources/{sync}/remove"),
                        format!("csrf={csrf}&confirm=yes")
                    ))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::SEE_OTHER
            );
        } else {
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
    }
    assert!(db::list(&f.state.pool, f.owner).await.unwrap().is_empty());
}
