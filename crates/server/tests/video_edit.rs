//! Atomic published-film adoption. Synthetic bytes isolate transaction, cache,
//! authority and lifecycle gates; decoded films are covered by browser/E2E.
mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use plamenu::{
    AppState,
    actions::{self, PostParams},
    storage::LocalDiskStore,
    video_edit,
};
use plamenu_ap::activity::PostKind;
use plamenu_db::{PgPool, id, media, video_draft as drafts, video_history};
use std::sync::Arc;
use tower::ServiceExt;

struct Fixture {
    _temp: tempfile::TempDir,
    state: AppState,
    owner: i64,
    channel: i64,
}
async fn fixture(pool: PgPool) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let mut config = common::test_config();
    config.media_dir = temp.path().join("media");
    let state = AppState::new(
        pool.clone(),
        config.clone(),
        common::StubFederation::with_actors([]),
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
        owner: owner.id,
        channel: channel.id,
    }
}
async fn ready(f: &Fixture, owner: i64) -> i64 {
    ready_with_poster(f, owner, true).await
}
async fn ready_with_poster(f: &Fixture, owner: i64, compatibility_poster: bool) -> i64 {
    let media = id::next();
    let source = format!("{media}.orig");
    assert_eq!(
        drafts::reserve(
            &f.state.pool,
            &drafts::NewDraft {
                media_id: media,
                account_id: owner,
                channel_id: f.channel,
                title: "Private candidate",
                description: "Do not replace the social description",
                language: Some("en"),
                visibility: "unlisted",
                source_file: &source,
                source_fingerprint: None,
                poster_time_seconds: 0.0,
                media_description: Some("New visual description"),
                media_transcript: Some("New transcript"),
                caption_vtt: None,
                reserved_bytes: 20
            },
            &drafts::AdmissionLimits {
                unfinished_per_account: 10,
                retained_source_bytes_per_account: 10000,
                pending_source_bytes_global: 10000,
                storage: plamenu_db::video_storage::Limits {
                    per_account: 10000,
                    global: 100_000
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
    f.state.media.put(&source, vec![1; 20]).await.unwrap();
    let mp4 = format!("{media}.draft.mp4");
    let avif = format!("{media}.draft.small.avif");
    let jpeg = format!("{media}.draft.poster.jpg");
    let hls = format!("{media}.draft.hls64.m4s");
    let board = format!("{media}.draft.storyboard.jpg");
    for key in [&mp4, &avif, &jpeg, &hls, &board] {
        f.state.media.put(key, vec![2; 20]).await.unwrap();
        sqlx::query("INSERT INTO video_storage_reservations(file_name,media_id,account_id,reserved_bytes) VALUES ($1,$2,$3,20)").bind(key).bind(media).bind(owner).execute(&f.state.pool).await.unwrap();
        sqlx::query("INSERT INTO video_draft_outputs(file_name,media_id,generation,cleanup_job_id) VALUES ($1,$2,1,$3)").bind(key).bind(media).bind(id::next()).execute(&f.state.pool).await.unwrap();
    }
    sqlx::query("UPDATE media_attachments SET file_name=$2,small_file_name=$3,processing='complete',width=64,height=64,small_width=64,small_height=64,duration=30,file_size=20,thumbnail_file_size=20 WHERE id=$1").bind(media).bind(&mp4).bind(&avif).execute(&f.state.pool).await.unwrap();
    sqlx::query(
        "UPDATE video_drafts SET source_bytes=20,state='ready',generation=1 WHERE media_id=$1",
    )
    .bind(media)
    .execute(&f.state.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO native_video_hls_renditions(media_id,generation,height,width,bandwidth,size_bytes,file_name,playlist,segment_sha256) VALUES ($1,1,64,64,10000,20,$2,'#EXTM3U\n#EXT-X-VERSION:7\n', $3)").bind(media).bind(&hls).bind(serde_json::json!({"0-19":"a".repeat(64)})).execute(&f.state.pool).await.unwrap();
    if compatibility_poster {
        sqlx::query("INSERT INTO video_compatibility_posters(media_id,file_name,width,height,file_size,cleanup_job_id) VALUES($1,$2,64,64,20,$3)").bind(media).bind(&jpeg).bind(id::next()).execute(&f.state.pool).await.unwrap();
    } else {
        let mut image = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            64,
            64,
            image::Rgb([0, 0, 255]),
        ))
        .write_to(&mut image, image::ImageFormat::Png)
        .unwrap();
        f.state.media.put(&avif, image.into_inner()).await.unwrap();
        // The real worker reserves its compatibility derivative, but does
        // not create it until publication or confirmed replacement.
        sqlx::query(
            "UPDATE video_storage_reservations SET reserved_bytes=65536 WHERE file_name=$1",
        )
        .bind(&jpeg)
        .execute(&f.state.pool)
        .await
        .unwrap();
    }
    sqlx::query("INSERT INTO video_storyboards(media_id,generation,file_name,size_bytes,columns,rows,tile_count,interval_seconds) VALUES($1,1,$2,20,1,1,1,30)").bind(media).bind(&board).execute(&f.state.pool).await.unwrap();
    sqlx::query("INSERT INTO video_captions(media_id,language,vtt_body) VALUES($1,'en','WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nnew\n')").bind(media).execute(&f.state.pool).await.unwrap();
    sqlx::query(
        "INSERT INTO video_chapters(media_id,start_seconds,title) VALUES($1,0,'New chapter')",
    )
    .bind(media)
    .execute(&f.state.pool)
    .await
    .unwrap();
    media
}

#[sqlx::test(migrations = "../db/migrations")]
async fn private_native_candidate_gets_an_interoperable_poster_before_atomic_adoption(
    pool: PgPool,
) {
    let f = fixture(pool.clone()).await;
    let old = ready(&f, f.owner).await;
    let post = publish(&f, old).await;
    let candidate = ready_with_poster(&f, f.owner, false).await;
    video_edit::replace(&f.state, old, f.owner, candidate, 0)
        .await
        .unwrap();
    let poster: String = value(
        &pool,
        "SELECT file_name FROM video_compatibility_posters WHERE media_id=$1",
        old,
    )
    .await;
    assert_eq!(poster, format!("{candidate}.draft.poster.jpg"));
    assert_eq!(
        image::guess_format(&f.state.media.get(&poster).await.unwrap()).unwrap(),
        image::ImageFormat::Jpeg
    );
    assert!(media::public_file_allowed(&pool, &poster).await.unwrap());
    let author = plamenu_db::account::find_by_id(&pool, f.owner)
        .await
        .unwrap()
        .unwrap();
    let object = plamenu::note::note_for_status(&f.state, &post, &author)
        .await
        .unwrap();
    assert_eq!(object["icon"][0]["mediaType"], "image/jpeg");
    assert_eq!(object["icon"][0]["width"], 640);
    assert_eq!(object["icon"][0]["height"], 360);
}
async fn publish(f: &Fixture, media: i64) -> plamenu_db::status::Status {
    actions::post_video(
        &f.state,
        PostParams {
            username: "alice",
            media_ids: &[media],
            title: Some("Original public title"),
            text: "Original description",
            visibility: "unlisted",
            language: Some("en"),
            kind: PostKind::Video,
            ..Default::default()
        },
        f.channel,
    )
    .await
    .unwrap()
    .0
}
#[sqlx::test(migrations = "../db/migrations")]
async fn source_edit_api_and_html_require_intent_scopes_and_private_ownership(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let original = ready(&f, f.owner).await;
    publish(&f, original).await;
    let user = plamenu_db::user::create(&pool, f.owner, Some("alice@example.test"), "unused")
        .await
        .unwrap();
    let web = plamenu::web::session::ensure_web_app(&f.state)
        .await
        .unwrap();
    let app = plamenu::build_router(f.state.clone());
    let count =
        || sqlx::query_scalar::<_, i64>("SELECT count(*) FROM video_drafts").fetch_one(&pool);
    let before = count().await.unwrap();
    for (scope, expected) in [
        ("read", StatusCode::FORBIDDEN),
        ("write:media", StatusCode::FORBIDDEN),
        ("write:statuses", StatusCode::FORBIDDEN),
        ("read write", StatusCode::ACCEPTED),
    ] {
        let raw = format!("source-{scope}");
        plamenu_db::oauth::create_token(
            &pool,
            &plamenu::auth::hash_secret(&raw),
            web.id,
            Some(user.id),
            scope,
        )
        .await
        .unwrap();
        let request = |method: &str, path: &str, body: String| {
            Request::builder()
                .method(method)
                .uri(path)
                .header("Authorization", format!("Bearer {raw}"))
                .header("Content-Type", "application/json")
                .body(Body::from(body))
                .unwrap()
        };
        let response = app
            .clone()
            .oneshot(request(
                "POST",
                &format!("/api/v1/videos/{original}/edits"),
                r#"{"operation":"reprocess","revision":0}"#.into(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        if expected != StatusCode::ACCEPTED {
            continue;
        }
        assert_eq!(response.headers()["Cache-Control"], "private, no-store");
        let value: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        let id = value["media_id"].as_str().unwrap();
        let status = app
            .clone()
            .oneshot(request(
                "GET",
                &format!("/api/v1/video_edits/{id}"),
                String::new(),
            ))
            .await
            .unwrap();
        assert_eq!(status.status(), StatusCode::OK);
        assert_eq!(status.headers()["Cache-Control"], "private, no-store");
        let text = String::from_utf8(
            status
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        assert!(text.contains("reprocess") && text.contains("queued"));
        assert!(!text.contains(".orig") && !text.contains("source_file"));
        let outsider = common::create_local_account(&pool, "outsider", "Outsider").await;
        let outsider_user =
            plamenu_db::user::create(&pool, outsider.id, Some("outsider@example.test"), "unused")
                .await
                .unwrap();
        plamenu_db::oauth::create_token(
            &pool,
            &plamenu::auth::hash_secret("outsider"),
            web.id,
            Some(outsider_user.id),
            "read write",
        )
        .await
        .unwrap();
        for method in ["GET", "DELETE"] {
            let denied = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(format!("/api/v1/video_edits/{id}"))
                        .header("Authorization", "Bearer outsider")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(denied.status(), StatusCode::NOT_FOUND);
        }
        let deleted = app
            .clone()
            .oneshot(request(
                "DELETE",
                &format!("/api/v1/video_edits/{id}"),
                String::new(),
            ))
            .await
            .unwrap();
        assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
        assert!(
            deleted
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .is_empty()
        );
        for body in [
            r#"{"operation":"trim","revision":0,"start_ms":0,"end_ms":31000}"#,
            r#"{"operation":"reprocess","revision":0,"start_ms":0}"#,
            r#"{"operation":"trim","revision":0,"start_ms":2000,"end_ms":1000}"#,
        ] {
            assert_eq!(
                app.clone()
                    .oneshot(request(
                        "POST",
                        &format!("/api/v1/videos/{original}/edits"),
                        body.into()
                    ))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::UNPROCESSABLE_ENTITY
            );
        }
        assert_eq!(
            app.clone()
                .oneshot(request(
                    "POST",
                    &format!("/api/v1/videos/{original}/edits"),
                    r#"{"operation":"reprocess","revision":8}"#.into()
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        let csrf = plamenu::auth::hash_secret(&format!("plamenu-csrf:{raw}"));
        let html_request = |csrf: &str| {
            Request::builder()
                .method("POST")
                .uri(format!("/web/videos/{original}/prepare-edit"))
                .header("Cookie", format!("__Host-plamenu_session={raw}"))
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "csrf={csrf}&revision=0&operation=trim&start=1.001&end=2.003"
                )))
                .unwrap()
        };
        assert_eq!(
            app.clone()
                .oneshot(html_request("wrong"))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        let result = app.clone().oneshot(html_request(&csrf)).await.unwrap();
        assert_eq!(result.status(), StatusCode::SEE_OTHER);
        let draft_url = result.headers()["Location"].to_str().unwrap();
        let id: i64 = draft_url.rsplit('/').next().unwrap().parse().unwrap();
        assert_eq!(
            plamenu::video_source_edit::processing(&f.state, id)
                .await
                .unwrap()
                .1,
            Some(plamenu::video_clip::Window::new(1001, 2003).unwrap())
        );
        assert!(drafts::cancel(&pool, id, f.owner).await.unwrap());
    }
    assert_eq!(count().await.unwrap(), before);
    assert_eq!(
        value::<i64>(
            &pool,
            "SELECT video_content_revision FROM media_attachments WHERE id=$1",
            original
        )
        .await,
        0
    );
}

async fn value<
    T: sqlx::Type<sqlx::Postgres> + for<'r> sqlx::Decode<'r, sqlx::Postgres> + Send + Unpin,
>(
    pool: &PgPool,
    sql: &'static str,
    id: i64,
) -> T {
    sqlx::query_scalar(sql)
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn stable_media_identity_atomic_signed_update_and_acknowledged_old_file_cleanup(
    pool: PgPool,
) {
    let f = fixture(pool.clone()).await;
    let old = ready(&f, f.owner).await;
    let post = publish(&f, old).await;
    let new = ready(&f, f.owner).await;
    let viewer = common::create_local_account(&pool, "viewer", "Viewer").await;
    plamenu_db::follow::create(&pool, viewer.id, f.channel, None)
        .await
        .unwrap();
    video_history::set_enabled(&pool, viewer.id, true)
        .await
        .unwrap();
    let peer = common::RemoteUser::new("remote.test", "subscriber");
    let subscriber = plamenu::remote::store_remote_actor(&pool, &peer.actor)
        .await
        .unwrap();
    plamenu_db::follow::create(&pool, subscriber.id, f.channel, None)
        .await
        .unwrap();
    assert!(
        video_history::save(&pool, viewer.id, old, 12, false)
            .await
            .unwrap()
    );
    let uuid: String = value(
        &pool,
        "SELECT uuid FROM video_publications WHERE media_id=$1",
        old,
    )
    .await;
    sqlx::query("DELETE FROM video_captions WHERE media_id=$1")
        .bind(new)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE video_drafts SET language='ru' WHERE media_id=$1")
        .bind(new)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE media_attachments SET caption_vtt='WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nNew Russian track\n' WHERE id=$1").bind(new).execute(&pool).await.unwrap();
    let before: i64 =
        sqlx::query_scalar("SELECT sum(reserved_bytes)::bigint FROM video_storage_reservations")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        !media::public_file_allowed(&pool, &format!("{new}.draft.mp4"))
            .await
            .unwrap()
    );
    video_edit::replace(&f.state, old, f.owner, new, 0)
        .await
        .unwrap();
    let m = media::find_by_ids(&pool, &[old])
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(m.id, old);
    assert_eq!(m.status_id, Some(post.id));
    assert_eq!(m.account_id, f.owner);
    assert_eq!(m.file_name, Some(format!("{new}.draft.mp4")));
    assert_eq!(
        value::<String>(
            &pool,
            "SELECT source_file FROM video_drafts WHERE media_id=$1",
            old
        )
        .await,
        format!("{new}.orig")
    );
    assert_eq!(
        value::<i64>(
            &pool,
            "SELECT generation FROM video_drafts WHERE media_id=$1",
            old
        )
        .await,
        2
    );
    assert_eq!(
        value::<String>(
            &pool,
            "SELECT uuid FROM video_publications WHERE media_id=$1",
            old
        )
        .await,
        uuid
    );
    assert!(media::find_by_ids(&pool, &[new]).await.unwrap().is_empty());
    let tracks = plamenu_db::video_captions::for_media(&pool, &[old])
        .await
        .unwrap();
    assert_eq!(tracks[&old].len(), 1);
    assert_eq!(tracks[&old][0].language, "ru");
    assert!(!tracks[&old][0].legacy);
    assert!(
        media::public_file_allowed(&pool, &format!("{new}.draft.mp4"))
            .await
            .unwrap()
    );
    assert!(
        !media::public_file_allowed(&pool, &format!("{old}.draft.mp4"))
            .await
            .unwrap()
    );
    assert!(
        !media::public_file_allowed(&pool, &format!("{new}.orig"))
            .await
            .unwrap()
    );
    let updated:serde_json::Value=sqlx::query_scalar("SELECT activity FROM delivery_jobs WHERE activity->>'type'='Update' ORDER BY id DESC LIMIT 1").fetch_one(&pool).await.unwrap();
    assert_eq!(
        updated["object"]["id"],
        format!("https://plamenu.test/users/alice/statuses/{}", post.id)
    );
    assert_eq!(updated["object"]["uuid"], uuid);
    assert_eq!(updated["object"]["name"], "Original public title");
    assert!(updated["object"]["url"].to_string().contains("?rev=1"));
    assert_eq!(
        plamenu_db::native_video_hls::public_peer_uuids(&pool, &[old])
            .await
            .unwrap()[&old],
        (uuid, 1)
    );
    assert!(
        video_history::recent(&pool, viewer.id)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        !video_history::save_revision(&pool, viewer.id, old, 12, false, 0)
            .await
            .unwrap()
    );
    assert!(
        video_history::save_revision(&pool, viewer.id, old, 2, false, 1)
            .await
            .unwrap()
    );
    let after: i64 =
        sqlx::query_scalar("SELECT sum(reserved_bytes)::bigint FROM video_storage_reservations")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(before, after);
    let jobs = plamenu_db::media_cleanup::claim_due(&pool, 100)
        .await
        .unwrap();
    assert_eq!(jobs.len(), 6);
    for job in &jobs {
        assert!(job.file_name.starts_with(&old.to_string()));
        f.state.media.delete(&job.file_name).await.unwrap();
    }
    plamenu_db::video_storage::complete_deleted(
        &pool,
        &jobs.iter().map(|j| j.id).collect::<Vec<_>>(),
    )
    .await
    .unwrap();
    let remaining: i64 =
        sqlx::query_scalar("SELECT sum(reserved_bytes)::bigint FROM video_storage_reservations")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(remaining, 120);
    assert!(f.state.media.get(&format!("{new}.orig")).await.is_ok());
    let app = plamenu::build_router(f.state.clone());
    for route in [
        format!("/media/hls/{old}/master.m3u8?rev=0"),
        format!("/media/hls/{old}/sha256.json?rev=0"),
        format!("/media/hls/{old}/native/64/index.m3u8?rev=0"),
    ] {
        let res = app
            .clone()
            .oneshot(Request::builder().uri(route).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }
    let res = app
        .oneshot(
            Request::builder()
                .uri(format!("/media/hls/{old}/master.m3u8?rev=1"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(
        String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec())
            .unwrap()
            .contains("index.m3u8?rev=1")
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn failures_stale_forms_concurrent_application_and_cancel_leave_old_film_intact(
    pool: PgPool,
) {
    let f = fixture(pool.clone()).await;
    let old = ready(&f, f.owner).await;
    publish(&f, old).await;
    let peer = common::RemoteUser::new("remote.test", "subscriber");
    let viewer = plamenu::remote::store_remote_actor(&pool, &peer.actor)
        .await
        .unwrap();
    plamenu_db::follow::create(&pool, viewer.id, f.channel, None)
        .await
        .unwrap();
    let new = ready(&f, f.owner).await;
    sqlx::raw_sql("CREATE FUNCTION reject_video_update() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture rejects Update'; END $$; CREATE TRIGGER reject_video_update BEFORE INSERT ON delivery_jobs FOR EACH ROW EXECUTE FUNCTION reject_video_update()").execute(&pool).await.unwrap();
    assert!(
        video_edit::replace(&f.state, old, f.owner, new, 0)
            .await
            .is_err()
    );
    assert_eq!(
        value::<String>(
            &pool,
            "SELECT source_file FROM video_drafts WHERE media_id=$1",
            old
        )
        .await,
        format!("{old}.orig")
    );
    assert!(
        drafts::preview(&pool, new, f.owner)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM media_cleanup_jobs")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    sqlx::raw_sql("DROP TRIGGER reject_video_update ON delivery_jobs")
        .execute(&pool)
        .await
        .unwrap();
    let other = ready(&f, f.owner).await;
    let (a, b) = tokio::join!(
        video_edit::replace(&f.state, old, f.owner, new, 0),
        video_edit::replace(&f.state, old, f.owner, other, 0)
    );
    assert_ne!(a.is_ok(), b.is_ok());
    assert_eq!(
        value::<i64>(
            &pool,
            "SELECT video_content_revision FROM media_attachments WHERE id=$1",
            old
        )
        .await,
        1
    );
    let remaining = if a.is_ok() { other } else { new };
    assert!(
        video_edit::replace(&f.state, old, f.owner, remaining, 0)
            .await
            .is_err()
    );
    assert!(drafts::cancel(&pool, remaining, f.owner).await.unwrap());
    assert!(
        video_edit::replace(&f.state, old, f.owner, remaining, 1)
            .await
            .is_err()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn editor_authority_quota_debt_and_revocation_are_rechecked_at_commit(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let old = ready(&f, f.owner).await;
    publish(&f, old).await;
    let editor = common::create_local_account(&pool, "editor", "Editor").await;
    sqlx::query("INSERT INTO video_channel_grants(channel_id,account_id,role,accepted) VALUES($1,$2,'editor',true)").bind(f.channel).bind(editor.id).execute(&pool).await.unwrap();
    let new = ready(&f, editor.id).await;
    sqlx::query("UPDATE video_channel_grants SET role='publisher' WHERE account_id=$1")
        .bind(editor.id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        video_edit::replace(&f.state, old, editor.id, new, 0)
            .await
            .is_err()
    );
    sqlx::query("UPDATE video_channel_grants SET role='editor' WHERE account_id=$1")
        .bind(editor.id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO video_storage_reservations(file_name,account_id,media_id,reserved_bytes) VALUES ('other.orig',$1,$2,$3)").bind(f.owner).bind(old).bind(f.state.config.native_video.storage_limits().per_account).execute(&pool).await.unwrap();
    assert!(
        video_edit::replace(&f.state, old, editor.id, new, 0)
            .await
            .is_err()
    );
    assert!(
        drafts::preview(&pool, new, editor.id)
            .await
            .unwrap()
            .is_some()
    );
    sqlx::query("DELETE FROM video_storage_reservations WHERE file_name='other.orig'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE accounts SET suspended_at=now() WHERE id=$1")
        .bind(f.owner)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        video_edit::replace(&f.state, old, editor.id, new, 0)
            .await
            .is_err()
    );
    sqlx::query("UPDATE accounts SET suspended_at=NULL WHERE id=$1")
        .bind(f.owner)
        .execute(&pool)
        .await
        .unwrap();
    video_edit::replace(&f.state, old, editor.id, new, 0)
        .await
        .unwrap();
    assert_eq!(
        value::<i64>(
            &pool,
            "SELECT account_id FROM media_attachments WHERE id=$1",
            old
        )
        .await,
        f.owner
    );
    let debts: Vec<i64> = sqlx::query_scalar(
        "SELECT DISTINCT account_id FROM video_storage_reservations WHERE file_name LIKE $1",
    )
    .bind(format!("{new}%"))
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(debts, vec![f.owner]);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn html_and_api_require_current_authority_csrf_confirmation_and_both_write_scopes(
    pool: PgPool,
) {
    let f = fixture(pool.clone()).await;
    let old = ready(&f, f.owner).await;
    publish(&f, old).await;
    let new = ready(&f, f.owner).await;
    let user = plamenu_db::user::create(&pool, f.owner, Some("alice@example.test"), "unused")
        .await
        .unwrap();
    let web = plamenu::web::session::ensure_web_app(&f.state)
        .await
        .unwrap();
    let app = plamenu::build_router(f.state.clone());
    for (scope, expected) in [
        ("read", StatusCode::FORBIDDEN),
        ("write:media", StatusCode::FORBIDDEN),
        ("write:statuses", StatusCode::FORBIDDEN),
        ("read write", StatusCode::SEE_OTHER),
    ] {
        let raw = format!("edit-{scope}");
        let csrf = plamenu::auth::hash_secret(&format!("plamenu-csrf:{raw}"));
        plamenu_db::oauth::create_token(
            &pool,
            &plamenu::auth::hash_secret(&raw),
            web.id,
            Some(user.id),
            scope,
        )
        .await
        .unwrap();
        let req = |confirm: &str, csrf: &str| {
            Request::builder()
                .method("POST")
                .uri(format!("/web/videos/{old}/edit"))
                .header("Cookie", format!("__Host-plamenu_session={raw}"))
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "csrf={csrf}&candidate={new}&revision=0&confirm={confirm}"
                )))
                .unwrap()
        };
        assert_eq!(
            app.clone()
                .oneshot(req("yes", "wrong"))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        if scope == "read write" {
            let page = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/videos/{old}/edit?candidate={new}"))
                        .header("Cookie", format!("__Host-plamenu_session={raw}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(page.status(), StatusCode::OK);
            assert_eq!(page.headers()["Cache-Control"], "private, no-store");
            let text = String::from_utf8(
                page.into_body()
                    .collect()
                    .await
                    .unwrap()
                    .to_bytes()
                    .to_vec(),
            )
            .unwrap();
            assert!(text.contains("Private replacement preview"));
            assert_eq!(
                app.clone().oneshot(req("", "")).await.unwrap().status(),
                StatusCode::FORBIDDEN
            );
            assert_eq!(
                app.clone().oneshot(req("", &csrf)).await.unwrap().status(),
                StatusCode::UNPROCESSABLE_ENTITY
            );
        }
        if scope != "read write" {
            let response=app.clone().oneshot(Request::builder().method("PUT").uri(format!("/api/v1/videos/{old}/content"))
                .header("Authorization",format!("Bearer {raw}")).header("Content-Type","application/json")
                .body(Body::from(serde_json::json!({"candidate_media_id":new.to_string(),"revision":0,"confirm":true}).to_string())).unwrap()).await.unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
        assert_eq!(
            app.clone()
                .oneshot(req("yes", &csrf))
                .await
                .unwrap()
                .status(),
            expected
        );
    }
    let next = ready(&f, f.owner).await;
    let read = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/videos/{old}/content"))
                .header("Authorization", "Bearer edit-read write")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(read.status(), StatusCode::OK);
    assert_eq!(read.headers()["Cache-Control"], "private, no-store");
    let body: serde_json::Value =
        serde_json::from_slice(&read.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["revision"], 1);
    assert_eq!(body["candidates"][0]["media_id"], next.to_string());
    let applied=app.oneshot(Request::builder().method("PUT").uri(format!("/api/v1/videos/{old}/content"))
        .header("Authorization","Bearer edit-read write").header("Content-Type","application/json")
        .body(Body::from(serde_json::json!({"candidate_media_id":next.to_string(),"revision":1,"confirm":true}).to_string())).unwrap()).await.unwrap();
    assert_eq!(applied.status(), StatusCode::OK);
}

async fn rendered_attachment(
    f: &Fixture,
    status: &plamenu_db::status::Status,
    media_id: i64,
) -> serde_json::Value {
    let entity =
        plamenu::entities::render_status(&f.state.pool, common::TEST_DOMAIN, status, Some(f.owner))
            .await
            .unwrap();
    let expected_id = media_id.to_string();
    entity["media_attachments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"].as_str() == Some(expected_id.as_str()))
        .unwrap()
        .clone()
}

#[sqlx::test(migrations = "../db/migrations")]
#[allow(
    clippy::too_many_lines,
    reason = "populated extension equivalence and readiness/visibility transitions share one publication"
)]
async fn batched_extensions_match_populated_native_sidecars_and_preserve_fences(pool: PgPool) {
    use plamenu_db::{
        video_captions, video_chapters, video_distribution, video_metadata, video_storyboards,
    };
    use serde_json::json;
    let f = fixture(pool).await;
    let media_id = ready(&f, f.owner).await;
    let status = publish(&f, media_id).await;
    sqlx::query("UPDATE video_drafts SET poster_alt='A blue pattern' WHERE media_id=$1")
        .bind(media_id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE media_attachments SET caption_vtt='WEBVTT legacy English' WHERE id=$1")
        .bind(media_id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE native_video_hls_renditions SET p2p_infohash=$2 WHERE media_id=$1")
        .bind(media_id)
        .bind("b".repeat(20))
        .execute(&f.state.pool)
        .await
        .unwrap();
    // Insert out of display order, retaining the existing modern English row.
    for language in ["fr", "de"] {
        sqlx::query("INSERT INTO video_captions(media_id,language,vtt_body,automatically_generated) VALUES($1,$2,'WEBVTT translated',true)").bind(media_id).bind(language).execute(&f.state.pool).await.unwrap();
    }
    for (start, title) in [(12, "Late"), (3, "Early")] {
        sqlx::query("INSERT INTO video_chapters(media_id,start_seconds,title) VALUES($1,$2,$3)")
            .bind(media_id)
            .bind(start)
            .bind(title)
            .execute(&f.state.pool)
            .await
            .unwrap();
    }
    let metadata = video_metadata::Metadata {
        media_id,
        category: Some(2),
        licence: Some(1),
        licence_version_known: true,
        ..Default::default()
    };
    video_metadata::upsert(&f.state.pool, &metadata)
        .await
        .unwrap();
    video_distribution::upsert(
        &f.state.pool,
        video_distribution::Policy {
            media_id,
            download_enabled: false,
            embed_enabled: true,
            mirror_allowed: false,
        },
    )
    .await
    .unwrap();

    let row = media::find_by_ids(&f.state.pool, &[media_id])
        .await
        .unwrap()
        .remove(0);
    let ladders = media::renditions_for(&f.state.pool, &[media_id])
        .await
        .unwrap();
    let original_base = plamenu::entities::media_json_hls(
        common::TEST_DOMAIN,
        &row,
        false,
        ladders.get(&media_id).map(Vec::as_slice),
    );
    let original_captions = video_captions::for_media(&f.state.pool, &[media_id])
        .await
        .unwrap();
    let original_chapters = video_chapters::for_media(&f.state.pool, &[media_id])
        .await
        .unwrap();
    let original_board = video_storyboards::for_media(&f.state.pool, &[media_id])
        .await
        .unwrap();
    let original_peers =
        plamenu_db::native_video_hls::public_peer_uuids(&f.state.pool, &[media_id])
            .await
            .unwrap();
    let original_offer = video_distribution::offers(&f.state.pool, &[media_id])
        .await
        .unwrap();
    let rendered = rendered_attachment(&f, &status, media_id).await;
    assert_eq!(
        rendered["hls"]["renditions"],
        original_base["hls"]["renditions"]
    );
    assert_eq!(rendered["poster_description"], "A blue pattern");
    assert_eq!(
        rendered["video_metadata"],
        plamenu::video_metadata::entity(&metadata)
    );
    let languages: Vec<_> = rendered["captions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|track| track["language"].as_str().unwrap())
        .collect();
    assert_eq!(
        languages,
        original_captions[&media_id]
            .iter()
            .map(|track| track.language.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(languages, ["de", "en", "fr"]);
    assert!(
        original_captions[&media_id]
            .iter()
            .all(|track| !track.legacy)
    );
    for (actual, expected) in rendered["captions"]
        .as_array()
        .unwrap()
        .iter()
        .zip(&original_captions[&media_id])
    {
        assert_eq!(
            actual["automatically_generated"],
            expected.automatically_generated
        );
    }
    assert_eq!(
        rendered["chapters"],
        json!(
            original_chapters[&media_id]
                .iter()
                .map(|chapter| json!({"start_seconds":chapter.start_seconds,"title":chapter.title}))
                .collect::<Vec<_>>()
        )
    );
    let board = &original_board[&media_id];
    assert_eq!(
        rendered["storyboard"],
        json!({"url":format!("https://{}/media/{}",common::TEST_DOMAIN,board.file_name),"tile_width":160,"tile_height":90,"columns":board.columns,"rows":board.rows,"tile_count":board.tile_count,"interval_seconds":board.interval_seconds})
    );
    let (uuid, revision) = &original_peers[&media_id];
    assert_eq!(rendered["hls"]["p2p"]["video_uuid"], *uuid);
    assert_eq!(rendered["content_revision"], *revision);
    assert_eq!(
        rendered["video_distribution"]["download_enabled"],
        original_offer[&media_id].download_enabled
    );
    assert_eq!(
        rendered["video_distribution"]["embed_enabled"],
        original_offer[&media_id].embed_enabled
    );
    assert!(rendered["video_distribution"]["download_url"].is_null());

    sqlx::query("UPDATE video_drafts SET generation=2 WHERE media_id=$1")
        .bind(media_id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    let stale = rendered_attachment(&f, &status, media_id).await;
    assert!(stale.get("storyboard").is_none());
    assert!(stale["hls"].get("p2p").is_none());
    assert!(!stale["chapters"].as_array().unwrap().is_empty());
    sqlx::query("UPDATE video_drafts SET generation=1 WHERE media_id=$1")
        .bind(media_id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE statuses SET visibility='private' WHERE id=$1")
        .bind(status.id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    let private_status = plamenu_db::status::find_by_id(&f.state.pool, status.id)
        .await
        .unwrap()
        .unwrap();
    let private = rendered_attachment(&f, &private_status, media_id).await;
    assert!(private.get("video_distribution").is_none());
    assert!(private["hls"].get("p2p").is_none());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn batched_extensions_match_remote_ladder_peer_hints_and_legacy_captions(pool: PgPool) {
    use plamenu_db::remote_hls_peers::{self, RemoteHlsPeers};
    let f = fixture(pool).await;
    let local_media = ready(&f, f.owner).await;
    let status = publish(&f, local_media).await;
    media::create_remote(
        &f.state.pool,
        media::NewRemoteMedia {
            account_id: f.owner,
            status_id: status.id,
            remote_url: "https://video.example/master.m3u8",
            content_type: "video/mp4",
            download_on_demand: true,
            hls_master_url: Some("https://video.example/master.m3u8"),
            renditions: vec![
                media::NewRendition {
                    height: 0,
                    width: None,
                    frame_rate: None,
                    size_bytes: Some(20),
                    origin_url: "https://video.example/audio.mp4",
                    is_audio: true,
                },
                media::NewRendition {
                    height: 360,
                    width: Some(640),
                    frame_rate: Some(24),
                    size_bytes: Some(200),
                    origin_url: "https://video.example/360.mp4",
                    is_audio: false,
                },
                media::NewRendition {
                    height: 720,
                    width: Some(1280),
                    frame_rate: Some(30),
                    size_bytes: Some(400),
                    origin_url: "https://video.example/720.mp4",
                    is_audio: false,
                },
            ],
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let rows = media::for_statuses_read_only(&f.state.pool, &[status.id])
        .await
        .unwrap();
    let remote = rows[&status.id]
        .iter()
        .find(|item| item.remote_url.is_some())
        .unwrap();
    sqlx::query("UPDATE media_attachments SET caption_vtt='WEBVTT legacy French' WHERE id=$1")
        .bind(remote.id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    let peers = RemoteHlsPeers {
        video_uuid: "00000000-0000-4000-8000-000000000001".into(),
        trackers: vec!["wss://video.example/tracker".into()],
        sha256_url: "https://video.example/sha256.json".into(),
    };
    remote_hls_peers::replace(&f.state.pool, remote.id, Some(&peers))
        .await
        .unwrap();
    let ladders = media::renditions_for(&f.state.pool, &[remote.id])
        .await
        .unwrap();
    let original_base = plamenu::entities::media_json_hls(
        common::TEST_DOMAIN,
        remote,
        false,
        ladders.get(&remote.id).map(Vec::as_slice),
    );
    let rendered = rendered_attachment(&f, &status, remote.id).await;
    assert_eq!(
        rendered["hls"]["renditions"],
        original_base["hls"]["renditions"]
    );
    assert_eq!(rendered["hls"]["renditions"][0]["height"], 720);
    assert_eq!(rendered["hls"]["renditions"][2]["audio_only"], true);
    assert_eq!(rendered["hls"]["p2p"]["video_uuid"], peers.video_uuid);
    assert_eq!(
        rendered["hls"]["p2p"]["trackers"],
        serde_json::json!(peers.trackers)
    );
    assert_eq!(rendered["captions"][0]["language"], "en");
    assert_eq!(rendered["captions"].as_array().unwrap().len(), 1);
    assert!(
        rendered["captions"][0]["url"]
            .as_str()
            .unwrap()
            .contains("/captions.vtt?v=")
    );
}
