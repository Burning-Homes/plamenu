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

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn video_settings_fallback_is_authorized_single_code_and_scope_safe(pool: PgPool) {
    let f = fixture(pool, false).await;
    let posted = publish(&f).await;
    let path = format!("/web/statuses/{}/video-settings", posted.id);
    let response = get(&f, path.clone()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "private, no-store");
    let body = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(body.contains("Video settings"));
    assert!(body.contains("Download playable video (MP4)"));
    assert_eq!(body.matches("data-video-embed-code").count(), 1);
    assert!(body.contains("?p2p=1"));
    assert!(!body.contains("Server-only embed code"));
    plamenu_db::video_history::update_preferences(&f.state.pool, f.owner, Some(true), None)
        .await
        .unwrap();
    let (raw, csrf) = token(&f, "read write:accounts").await;
    assert_eq!(
        form(&f, path.clone(), &raw, format!("csrf={csrf}")).await,
        StatusCode::SEE_OTHER
    );
    assert!(
        !plamenu_db::video_history::preferences(&f.state.pool, f.owner)
            .await
            .unwrap()
            .p2p_enabled
    );
    assert!(
        plamenu_db::video_history::preferences(&f.state.pool, f.owner)
            .await
            .unwrap()
            .history_enabled
    );
    assert_eq!(
        form(
            &f,
            path.clone(),
            &raw,
            format!("csrf={csrf}&enabled=invalid")
        )
        .await,
        StatusCode::UNPROCESSABLE_ENTITY
    );
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
    assert_eq!(response.status(), StatusCode::OK);
    let html = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(html.contains("?p2p=0"));
    assert_eq!(
        form(&f, path.clone(), &raw, "csrf=invalid&enabled=true".into()).await,
        StatusCode::FORBIDDEN
    );
    let (readonly, csrf) = token(&f, "read").await;
    assert_eq!(
        form(
            &f,
            path.clone(),
            &readonly,
            format!("csrf={csrf}&enabled=true")
        )
        .await,
        StatusCode::FORBIDDEN
    );
    assert!(
        !plamenu_db::video_history::preferences(&f.state.pool, f.owner)
            .await
            .unwrap()
            .p2p_enabled
    );
    sqlx::query("UPDATE statuses SET visibility='private' WHERE id=$1")
        .bind(posted.id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(get(&f, path).await.status(), StatusCode::NOT_FOUND);
    sqlx::query("UPDATE statuses SET visibility='public' WHERE id=$1")
        .bind(posted.id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE accounts SET suspended_at=now() WHERE id=$1")
        .bind(f.owner)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(
        get(&f, format!("/web/statuses/{}/video-settings", posted.id))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
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
            mirror_allowed: false,
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

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
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
        mirror_allowed: false,
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

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
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

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
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

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn initial_private_choices_survive_create_and_require_status_scope_after_publication(
    pool: PgPool,
) {
    let f = fixture(pool, true).await;
    let disabled = Policy {
        media_id: f.media,
        download_enabled: false,
        embed_enabled: false,
        mirror_allowed: false,
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

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
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

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
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

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn direct_and_download_stream_large_ranges_and_resume_identical_bytes(pool: PgPool) {
    let f = fixture(pool, false).await;
    publish(&f).await;
    let length = 2 * 1024 * 1024 + 19;
    let bytes: Vec<u8> = (0..length)
        .map(|i| u8::try_from(i % 251).unwrap())
        .collect();
    f.state
        .media
        .put(&format!("{}.mp4", f.media), bytes.clone())
        .await
        .unwrap();
    let app = plamenu::build_router(f.state.clone());
    for path in [
        format!("/media/{}.mp4", f.media),
        format!("/media/{}/download", f.media),
    ] {
        // Cancel an in-progress response, then resume across multiple chunks.
        let mut response = get(&f, path.clone()).await;
        assert_eq!(response.headers()["Content-Length"], length.to_string());
        let first = response
            .body_mut()
            .frame()
            .await
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap();
        assert_eq!(&first[..], &bytes[..first.len()]);
        let resume = first.len();
        drop(response);
        for (start, end, range) in [
            (resume, length - 1, format!("bytes={resume}-")),
            (65_530, 262_150, "bytes=65530-262150".to_owned()),
            (length - 131_075, length - 1, "bytes=-131075".to_owned()),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(&path)
                        .header("Range", range)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
            assert_eq!(
                response.headers()["Content-Range"],
                format!("bytes {start}-{end}/{length}")
            );
            assert_eq!(
                response.headers()["Content-Length"],
                (end - start + 1).to_string()
            );
            let received = response.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(&received[..], &bytes[start..=end]);
        }
    }
}

async fn progressive_fixture(f: &Fixture) -> plamenu_db::native_video_hls::Progressive {
    progressive_fixture_schema(f, false).await
}

async fn progressive_fixture_schema(
    f: &Fixture,
    legacy: bool,
) -> plamenu_db::native_video_hls::Progressive {
    use sha2::Digest;
    use std::fmt::Write as _;
    let bytes = vec![42u8; 196_731];
    let p = plamenu_db::native_video_hls::Progressive {
        file_name: format!("{}.draft.progressive240.mp4", f.media),
        size_bytes: bytes.len().try_into().unwrap(),
        sha256: sha2::Sha256::digest(&bytes).iter().fold(
            String::with_capacity(64),
            |mut output, byte| {
                write!(output, "{byte:02x}").unwrap();
                output
            },
        ),
        width: 320,
        height: 240,
        duration: 5.0,
        frame_rate: Some("30/1".into()),
        bitrate: 123_456,
        audio_codec: Some("aac".into()),
    };
    f.state.media.put(&p.file_name, bytes).await.unwrap();
    sqlx::query("UPDATE video_drafts SET generation=1 WHERE media_id=$1")
        .bind(f.media)
        .execute(&f.state.pool)
        .await
        .unwrap();
    for file in [&p.file_name, &format!("{}.draft.hls240.m4s", f.media)] {
        sqlx::query("INSERT INTO video_draft_outputs(file_name,media_id,generation,cleanup_job_id) VALUES($1,$2,1,$3)").bind(file).bind(f.media).bind(id::next()).execute(&f.state.pool).await.unwrap();
    }
    sqlx::query("INSERT INTO native_video_hls_renditions(media_id,generation,height,width,bandwidth,size_bytes,file_name,playlist) SELECT media_id,generation,240,320,123456,196731,$2,E'#EXTM3U\n#EXT-X-ENDLIST\n' FROM video_drafts WHERE media_id=$1")
        .bind(f.media).bind(format!("{}.draft.hls240.m4s",f.media)).execute(&f.state.pool).await.unwrap();
    if legacy {
        sqlx::query("UPDATE native_video_hls_renditions SET progressive=$2 WHERE media_id=$1")
            .bind(f.media)
            .bind(sqlx::types::Json(&p))
            .execute(&f.state.pool)
            .await
            .unwrap();
    } else {
        let mut conn = f.state.pool.acquire().await.unwrap();
        plamenu_db::native_video_hls::set_progressive(&mut conn, f.media, 1, 240, &p)
            .await
            .unwrap();
    }
    p
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn progressive_serving_validators_authority_and_original_identity(pool: PgPool) {
    let f = fixture(pool, true).await;
    let p = progressive_fixture(&f).await;
    let path = format!("/media/{}", p.file_name);
    assert_eq!(
        get(&f, path.clone()).await.status(),
        StatusCode::NOT_FOUND,
        "private ready draft is never exposed"
    );
    let posted = publish(&f).await;
    let etag = format!("\"{}\"", p.sha256);
    let request = |headers: &[(&str, &str)]| {
        let mut r = Request::builder().uri(&path);
        for (key, value) in headers {
            r = r.header(*key, *value);
        }
        plamenu::build_router(f.state.clone()).oneshot(r.body(Body::empty()).unwrap())
    };
    let r = request(&[("Range", "bytes=65530-131100"), ("If-Range", &etag)])
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(r.headers()["etag"], etag);
    assert_eq!(
        r.into_body().collect().await.unwrap().to_bytes().len(),
        65571
    );
    let r = request(&[
        ("Range", "bytes=65530-131100"),
        ("If-Range", "\"different\""),
    ])
    .await
    .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(
        r.into_body().collect().await.unwrap().to_bytes().len(),
        196_731
    );
    assert_eq!(
        request(&[("If-None-Match", &etag)]).await.unwrap().status(),
        StatusCode::NOT_MODIFIED
    );
    assert_eq!(
        request(&[("Range", "bytes=196731-")])
            .await
            .unwrap()
            .status(),
        StatusCode::RANGE_NOT_SATISFIABLE
    );
    let status_entity = plamenu::entities::render_status(
        &f.state.pool,
        &f.state.config.domain,
        &posted,
        Some(f.owner),
    )
    .await
    .unwrap();
    assert!(
        status_entity["media_attachments"][0]["url"]
            .as_str()
            .unwrap()
            .ends_with(&p.file_name)
    );
    assert!(
        status_entity["media_attachments"][0]["progressive"]["original_url"]
            .as_str()
            .unwrap()
            .ends_with(&format!("{}.mp4", f.media))
    );
    assert_eq!(
        status_entity["media_attachments"][0]["meta"]["original"]["bitrate"],
        123_456
    );
    let owner = plamenu_db::account::find_by_id(&f.state.pool, f.owner)
        .await
        .unwrap()
        .unwrap();
    let note = plamenu::note::note_for_status(&f.state, &posted, &owner)
        .await
        .unwrap();
    assert!(
        note["attachment"][0]["url"]
            .as_str()
            .unwrap()
            .ends_with(&format!("{}.mp4", f.media)),
        "Document identity remains original"
    );
    assert!(
        note["url"]
            .as_array()
            .unwrap()
            .iter()
            .any(|link| link["name"] == "Playable MP4" && link["size"] == p.size_bytes)
    );
    let original = get(&f, format!("/media/{}/download", f.media)).await;
    assert_eq!(
        original
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        &(0u8..128).collect::<Vec<_>>()
    );
    plamenu_db::account::suspend(&f.state.pool, f.owner, "test")
        .await
        .unwrap();
    assert_eq!(get(&f, path).await.status(), StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn progressive_missing_failed_stale_metadata_keeps_original(pool: PgPool) {
    let f = fixture(pool, true).await;
    let p = progressive_fixture(&f).await;
    let posted = publish(&f).await;
    f.state.media.delete(&p.file_name).await.unwrap();
    assert_eq!(
        get(&f, format!("/media/{}", p.file_name)).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&f, format!("/media/{}.mp4", f.media)).await.status(),
        StatusCode::OK
    );
    for query in [
        "DELETE FROM native_video_progressive_files WHERE media_id=$1",
        "UPDATE video_drafts SET generation=generation+1 WHERE media_id=$1",
        "UPDATE video_drafts SET state='failed' WHERE media_id=$1",
        "UPDATE video_draft_outputs SET retired=true WHERE media_id=$1",
    ] {
        sqlx::query("UPDATE video_drafts SET generation=1,state='ready' WHERE media_id=$1")
            .bind(f.media)
            .execute(&f.state.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE video_draft_outputs SET retired=false WHERE media_id=$1")
            .bind(f.media)
            .execute(&f.state.pool)
            .await
            .unwrap();
        let mut conn = f.state.pool.acquire().await.unwrap();
        plamenu_db::native_video_hls::set_progressive(&mut conn, f.media, 1, 240, &p)
            .await
            .unwrap();
        drop(conn);
        sqlx::query(query)
            .bind(f.media)
            .execute(&f.state.pool)
            .await
            .unwrap();
        let status_entity = plamenu::entities::render_status(
            &f.state.pool,
            &f.state.config.domain,
            &posted,
            Some(f.owner),
        )
        .await
        .unwrap();
        assert!(
            status_entity["media_attachments"][0]["url"]
                .as_str()
                .unwrap()
                .ends_with(&format!("{}.mp4", f.media))
        );
        assert!(
            status_entity["media_attachments"][0]
                .get("progressive")
                .is_none()
        );
    }
}

#[sqlx::test(migrations = false)]
async fn progressive_upgrade_preserves_metadata_journal_and_original(pool: PgPool) {
    sqlx::raw_sql("DROP SCHEMA public CASCADE; CREATE SCHEMA public")
        .execute(&pool)
        .await
        .unwrap();
    plamenu_db::MIGRATOR.run_to(132, &pool).await.unwrap();
    let f = fixture(pool.clone(), true).await;
    let expected = progressive_fixture_schema(&f, true).await;
    plamenu_db::MIGRATOR.run(&pool).await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let actual = plamenu_db::native_video_hls::progressive_for_media(&mut conn, f.media)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
    drop(conn);
    assert_eq!(
        get(&f, format!("/media/{}", expected.file_name))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    publish(&f).await;
    assert_eq!(
        get(&f, format!("/media/{}", expected.file_name))
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        get(&f, format!("/media/{}/download", f.media))
            .await
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        &(0u8..128).collect::<Vec<_>>()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM video_draft_outputs WHERE media_id=$1")
            .bind(f.media)
            .fetch_one(&pool)
            .await
            .unwrap(),
        2
    );
    plamenu_db::MIGRATOR.run(&pool).await.unwrap();
}

#[sqlx::test(migrations = false)]
async fn progressive_upgrade_failure_retains_legacy_metadata(pool: PgPool) {
    sqlx::raw_sql("DROP SCHEMA public CASCADE; CREATE SCHEMA public")
        .execute(&pool)
        .await
        .unwrap();
    plamenu_db::MIGRATOR.run_to(132, &pool).await.unwrap();
    let f = fixture(pool.clone(), true).await;
    let expected = progressive_fixture_schema(&f, true).await;
    for (field, invalid) in [
        ("sha256", serde_json::json!("invalid")),
        ("unsupported", serde_json::json!("invalid")),
        ("duration", serde_json::json!("5")),
        ("frame_rate", serde_json::json!(30)),
    ] {
        let mut malformed = serde_json::to_value(&expected).unwrap();
        malformed[field] = invalid;
        sqlx::query("UPDATE native_video_hls_renditions SET progressive=$2 WHERE media_id=$1")
            .bind(f.media)
            .bind(sqlx::types::Json(&malformed))
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            plamenu_db::MIGRATOR.run(&pool).await.is_err(),
            "reject malformed {field}"
        );
        let retained: serde_json::Value = sqlx::query_scalar(
            "SELECT progressive FROM native_video_hls_renditions WHERE media_id=$1",
        )
        .bind(f.media)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            retained, malformed,
            "failed upgrade preserves every legacy metadata field"
        );
    }
    sqlx::query("UPDATE native_video_hls_renditions SET progressive=$2 WHERE media_id=$1")
        .bind(f.media)
        .bind(sqlx::types::Json(expected))
        .execute(&pool)
        .await
        .unwrap();
    plamenu_db::MIGRATOR.run(&pool).await.unwrap();
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn progressive_typed_metadata_rejects_invalid_and_unowned_updates(pool: PgPool) {
    let f = fixture(pool.clone(), true).await;
    let expected = progressive_fixture(&f).await;
    let error =
        sqlx::query("UPDATE native_video_progressive_files SET sha256='invalid' WHERE media_id=$1")
            .bind(f.media)
            .execute(&pool)
            .await
            .unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("23514")
    );
    let mut conn = pool.acquire().await.unwrap();
    assert!(
        plamenu_db::native_video_hls::set_progressive(&mut conn, f.media, 2, 240, &expected)
            .await
            .is_err(),
        "stale generation has no matching owned rendition"
    );
    let mut unowned = expected.clone();
    unowned.file_name = "missing.draft.progressive240.mp4".into();
    assert!(
        plamenu_db::native_video_hls::set_progressive(&mut conn, f.media, 1, 240, &unowned)
            .await
            .is_err(),
        "missing journal cannot replace verified metadata"
    );
    let actual = plamenu_db::native_video_hls::progressive_for_media(&mut conn, f.media)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
}
