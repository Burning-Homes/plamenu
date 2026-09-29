//! Download/embed choices and scoped HTML media-management authority.
mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use plamenu::{
    AppState,
    actions::{self, PostParams},
    video_distribution as service,
};
use plamenu_ap::activity::PostKind;
use plamenu_db::{PgPool, id, media, video_distribution::Policy};
use tower::ServiceExt;

struct Fixture {
    state: AppState,
    owner: i64,
    channel: i64,
    media: i64,
}
async fn fixture(pool: PgPool, draft: bool) -> Fixture {
    let actor = common::create_local_account(&pool, "alice", "Alice").await;
    let peer = common::RemoteUser::new("remote.test", "viewer");
    let state = common::test_state_with(pool.clone(), common::StubFederation::with_users(&[&peer]));
    let channel = plamenu::video::create_channel(&state, &actor, "films", "Films")
        .await
        .unwrap();
    let viewer = plamenu::remote::store_remote_actor(&pool, &peer.actor)
        .await
        .unwrap();
    plamenu_db::follow::create(&pool, viewer.id, actor.id, None)
        .await
        .unwrap();
    let media = id::next();
    let file = format!("{media}.mp4");
    let poster = format!("{media}.jpg");
    media::create_local(
        &pool,
        media::NewLocalMedia {
            kind: "video",
            width: Some(320),
            height: Some(240),
            duration: Some(5.0),
            small: Some(media::SmallStyle {
                file_name: &poster,
                width: 320,
                height: 180,
            }),
            ..media::NewLocalMedia::new(actor.id, media, &file, "video/mp4")
        },
    )
    .await
    .unwrap();
    state.media.put(&file, (0..128).collect()).await.unwrap();
    state.media.put(&poster, vec![1; 16]).await.unwrap();
    state
        .media
        .put(&format!("{media}.orig"), vec![7; 128])
        .await
        .unwrap();
    if draft {
        sqlx::query("INSERT INTO video_drafts(media_id,account_id,channel_id,title,description,visibility,source_file,source_cleanup_job_id,source_bytes,reserved_bytes,state) VALUES($1,$2,$3,'Private film','A private source','public',$4,$5,128,128,'ready')")
            .bind(media).bind(actor.id).bind(channel.id).bind(format!("{media}.orig")).bind(id::next()).execute(&pool).await.unwrap();
    }
    Fixture {
        state,
        owner: actor.id,
        channel: channel.id,
        media,
    }
}
async fn publish(f: &Fixture) -> plamenu_db::status::Status {
    actions::post_video(
        &f.state,
        PostParams {
            username: "alice",
            media_ids: &[f.media],
            title: Some("Distribution film"),
            text: "Distribution controls",
            visibility: "public",
            kind: PostKind::Video,
            ..Default::default()
        },
        f.channel,
    )
    .await
    .unwrap()
    .0
}
async fn get(f: &Fixture, path: String) -> axum::response::Response {
    plamenu::build_router(f.state.clone())
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap()
}
async fn token(f: &Fixture, scope: &str) -> (String, String) {
    let user = plamenu_db::user::set_credentials(
        &f.state.pool,
        f.owner,
        Some("alice@example.test"),
        "hash",
    )
    .await
    .unwrap();
    let web = plamenu::web::session::ensure_web_app(&f.state)
        .await
        .unwrap();
    let raw = format!("distribution-{}", id::next());
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
    (raw, csrf)
}
async fn form(f: &Fixture, path: String, raw: &str, body: String) -> StatusCode {
    plamenu::build_router(f.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("Cookie", format!("__Host-plamenu_session={raw}"))
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn download_embed_flags_preserve_playback_and_use_current_private_authority(pool: PgPool) {
    let f = fixture(pool, false).await;
    let posted = publish(&f).await;
    let path = format!("/media/{}/download", f.media);
    let response = get(&f, path.clone()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["Cache-Control"], "private, no-store");
    assert_eq!(
        response.headers()["Content-Disposition"],
        format!("attachment; filename=\"video-{}.mp4\"", f.media)
    );
    assert_eq!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .len(),
        128
    );
    for (range, status, length) in [
        ("bytes=17-80", StatusCode::PARTIAL_CONTENT, 64),
        ("bytes=-16", StatusCode::PARTIAL_CONTENT, 16),
        ("bytes=999-1000", StatusCode::RANGE_NOT_SATISFIABLE, 0),
    ] {
        let response = plamenu::build_router(f.state.clone())
            .oneshot(
                Request::builder()
                    .uri(&path)
                    .header("Range", range)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        assert_eq!(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .len(),
            length
        );
    }
    let head = plamenu::build_router(f.state.clone())
        .oneshot(
            Request::builder()
                .method("HEAD")
                .uri(&path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(head.status(), StatusCode::OK);
    assert!(
        head.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .is_empty()
    );
    assert_eq!(
        get(&f, format!("/web/statuses/{}/embed", posted.id))
            .await
            .status(),
        StatusCode::OK
    );
    service::save(
        &f.state,
        f.owner,
        Policy {
            media_id: f.media,
            download_enabled: false,
            embed_enabled: false,
        },
        true,
    )
    .await
    .unwrap();
    assert_eq!(get(&f, path).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        get(&f, format!("/web/statuses/{}/embed?p2p=0", posted.id))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&f, format!("/media/{}.mp4", f.media)).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        get(&f, format!("/media/{}.orig", f.media)).await.status(),
        StatusCode::NOT_FOUND
    );
    let entity =
        plamenu::entities::render_status(&f.state.pool, &f.state.config.domain, &posted, None)
            .await
            .unwrap();
    assert_eq!(
        entity["media_attachments"][0]["video_distribution"]["download_enabled"],
        false
    );
    assert!(entity["media_attachments"][0]["video_distribution"]["download_url"].is_null());
    let actor = plamenu_db::account::find_by_id(&f.state.pool, f.owner)
        .await
        .unwrap()
        .unwrap();
    let object = plamenu::note::note_for_status(&f.state, &posted, &actor)
        .await
        .unwrap();
    assert_eq!(object["downloadEnabled"], false);
    service::save(&f.state, f.owner, Policy::default_for(f.media), true)
        .await
        .unwrap();
    sqlx::query("UPDATE accounts SET suspended_at=now() WHERE id=$1")
        .bind(f.channel)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(
        get(&f, format!("/media/{}/download", f.media))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&f, format!("/web/statuses/{}/embed", posted.id))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn sharing_and_signed_update_rollback_together_and_revoked_editors_cannot_change_them(
    pool: PgPool,
) {
    let f = fixture(pool, false).await;
    let posted = publish(&f).await;
    let editor = common::create_local_account(&f.state.pool, "editor", "Editor").await;
    sqlx::query("INSERT INTO video_channel_grants(channel_id,account_id,role,accepted) VALUES($1,$2,'editor',true)").bind(f.channel).bind(editor.id).execute(&f.state.pool).await.unwrap();
    sqlx::raw_sql("CREATE FUNCTION reject_distribution() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'outbox test failure'; END $$; CREATE TRIGGER reject_distribution BEFORE INSERT ON delivery_jobs FOR EACH ROW EXECUTE FUNCTION reject_distribution();").execute(&f.state.pool).await.unwrap();
    let disabled = Policy {
        media_id: f.media,
        download_enabled: false,
        embed_enabled: false,
    };
    assert!(
        service::save(&f.state, editor.id, disabled, true)
            .await
            .is_err()
    );
    assert_eq!(
        service::owned(&f.state, f.owner, f.media).await.unwrap().0,
        Policy::default_for(f.media)
    );
    assert!(
        plamenu_db::status::find_by_id(&f.state.pool, posted.id)
            .await
            .unwrap()
            .unwrap()
            .edited_at
            .is_none()
    );
    sqlx::query("DROP TRIGGER reject_distribution ON delivery_jobs")
        .execute(&f.state.pool)
        .await
        .unwrap();
    service::save(&f.state, editor.id, disabled, true)
        .await
        .unwrap();
    sqlx::query("DELETE FROM video_channel_grants WHERE account_id=$1")
        .bind(editor.id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert!(
        service::save(&f.state, editor.id, Policy::default_for(f.media), true)
            .await
            .is_err()
    );
    assert!(service::owned(&f.state, editor.id, f.media).await.is_err());
    assert_eq!(
        service::owned(&f.state, f.owner, f.media).await.unwrap().0,
        disabled
    );
    let updates: Vec<serde_json::Value> =
        sqlx::query_scalar("SELECT activity FROM delivery_jobs WHERE activity->>'type'='Update'")
            .fetch_all(&f.state.pool)
            .await
            .unwrap();
    assert!(
        updates
            .iter()
            .any(|a| a["object"]["downloadEnabled"] == false)
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn viewer_sessions_and_unlisted_embeds_never_widen_current_content_authority(pool: PgPool) {
    let f = fixture(pool, false).await;
    let posted = publish(&f).await;
    let (raw, _) = token(&f, "read write").await;
    for visibility in ["unlisted", "private", "direct", "local"] {
        sqlx::query("UPDATE statuses SET visibility=$2 WHERE id=$1")
            .bind(posted.id)
            .bind(visibility)
            .execute(&f.state.pool)
            .await
            .unwrap();
        for path in [
            format!("/media/{}/download", f.media),
            format!("/web/statuses/{}/embed?p2p=0", posted.id),
        ] {
            let response = plamenu::build_router(f.state.clone())
                .oneshot(
                    Request::builder()
                        .uri(&path)
                        .header("Cookie", format!("__Host-plamenu_session={raw}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                if visibility == "unlisted" {
                    StatusCode::OK
                } else {
                    StatusCode::NOT_FOUND
                }
            );
            assert_eq!(response.headers()["Cache-Control"], "private, no-store");
        }
    }
    sqlx::query("UPDATE statuses SET visibility='public',deleted_at=now() WHERE id=$1")
        .bind(posted.id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(
        get(&f, format!("/media/{}/download", f.media))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&f, format!("/web/statuses/{}/embed", posted.id))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert!(
        service::save(&f.state, f.owner, Policy::default_for(f.media), true)
            .await
            .is_err()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn multipart_and_resumable_draft_lanes_refuse_scoped_cookies_before_allocation(pool: PgPool) {
    let f = fixture(pool, true).await;
    for scope in ["read", "read write:statuses"] {
        let (raw, csrf) = token(&f, scope).await;
        let boundary = "distribution-fixture";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"csrf\"\r\n\r\n{csrf}\r\n--{boundary}--\r\n"
        );
        for path in [
            "/web/videos".to_owned(),
            "/web/videos/resumable".to_owned(),
            format!("/web/videos/drafts/{}/poster", f.media),
            format!("/web/videos/drafts/{}/captions", f.media),
        ] {
            let response = plamenu::build_router(f.state.clone())
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(&path)
                        .header("Cookie", format!("__Host-plamenu_session={raw}"))
                        .header(
                            "Content-Type",
                            format!("multipart/form-data; boundary={boundary}"),
                        )
                        .body(Body::from(body.clone()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{scope} {path}");
        }
        for (method, suffix) in [("PATCH", "source"), ("POST", "source/finish")] {
            let response = plamenu::build_router(f.state.clone())
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(format!("/web/videos/drafts/{}/{suffix}", f.media))
                        .header("Cookie", format!("__Host-plamenu_session={raw}"))
                        .header("X-CSRF-Token", &csrf)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM video_drafts")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(
        count, 1,
        "no private quota allocation from a read/status-only token"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn initial_private_choices_survive_create_and_require_status_scope_after_publication(
    pool: PgPool,
) {
    let f = fixture(pool, true).await;
    let disabled = Policy {
        media_id: f.media,
        download_enabled: false,
        embed_enabled: false,
    };
    service::save(&f.state, f.owner, disabled, false)
        .await
        .unwrap();
    assert_eq!(
        get(&f, format!("/media/{}/download", f.media))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let posted = publish(&f).await;
    let actor = plamenu_db::account::find_by_id(&f.state.pool, f.owner)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        plamenu::note::note_for_status(&f.state, &posted, &actor)
            .await
            .unwrap()["downloadEnabled"],
        false
    );
    let activities: Vec<serde_json::Value> =
        sqlx::query_scalar("SELECT activity FROM delivery_jobs WHERE activity->>'type'='Create'")
            .fetch_all(&f.state.pool)
            .await
            .unwrap();
    assert!(
        activities
            .iter()
            .any(|a| a["object"]["type"] == "Video" && a["object"]["downloadEnabled"] == false)
    );
    assert!(
        service::save(&f.state, f.owner, Policy::default_for(f.media), false)
            .await
            .is_err()
    );
    assert_eq!(
        service::owned(&f.state, f.owner, f.media).await.unwrap().0,
        disabled
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn all_published_html_media_mutations_keep_oauth_cookies_scoped_and_csrf_bound(pool: PgPool) {
    let f = fixture(pool, false).await;
    publish(&f).await;
    for scope in [
        "read",
        "read write:media",
        "read write:statuses",
        "read write",
    ] {
        let (raw, csrf) = token(&f, scope).await;
        let enabled = scope == "read write";
        for (suffix, fields) in [
            ("sharing", ""),
            ("metadata", "&category=12&licence=2"),
            ("chapters", "&chapters=0%3A00+Beginning"),
            ("captions/en/remove", ""),
        ] {
            let response = form(
                &f,
                format!("/web/videos/{}/{suffix}", f.media),
                &raw,
                format!("csrf={csrf}{fields}"),
            )
            .await;
            if enabled {
                assert_ne!(response, StatusCode::FORBIDDEN, "{suffix}");
            } else {
                assert_eq!(response, StatusCode::FORBIDDEN, "{scope} {suffix}");
            }
        }
        assert_eq!(
            form(
                &f,
                format!("/web/videos/{}/sharing", f.media),
                &raw,
                "csrf=wrong".into()
            )
            .await,
            StatusCode::FORBIDDEN
        );
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn draft_recovery_and_private_preview_do_not_upgrade_scoped_cookies(pool: PgPool) {
    let f = fixture(pool, true).await;
    for scope in ["read", "read write:statuses", "read write:media"] {
        let (raw, csrf) = token(&f, scope).await;
        for (suffix, fields) in [
            ("sharing", ""),
            ("drafts/placeholder/chapters", "&chapters=0%3A00+Start"),
        ] {
            let path = if suffix == "sharing" {
                format!("/web/videos/{}/sharing", f.media)
            } else {
                format!("/web/videos/drafts/{}/chapters", f.media)
            };
            let response = form(&f, path, &raw, format!("csrf={csrf}{fields}")).await;
            assert_eq!(
                response,
                if scope == "read write:media" {
                    StatusCode::SEE_OTHER
                } else {
                    StatusCode::FORBIDDEN
                }
            );
        }
        if scope != "read write:media" {
            assert_eq!(
                form(
                    &f,
                    format!("/web/videos/drafts/{}", f.media),
                    &raw,
                    format!("csrf={csrf}&op=cancel&confirm=yes")
                )
                .await,
                StatusCode::FORBIDDEN
            );
        }
    }
    let (raw, _) = token(&f, "read:statuses").await;
    let response = plamenu::build_router(f.state.clone())
        .oneshot(
            Request::builder()
                .uri(format!("/videos/drafts/{}/preview/video", f.media))
                .header("Cookie", format!("__Host-plamenu_session={raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
