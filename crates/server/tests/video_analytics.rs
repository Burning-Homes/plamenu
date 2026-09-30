//! Real routed bodies, current channel authority and privacy/retention fences.
mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use plamenu::{
    AppState,
    actions::{self, PostParams},
    video_analytics as analytics,
};
use plamenu_ap::activity::PostKind;
use plamenu_db::{PgPool, id, media};
use tower::ServiceExt;

struct Fixture {
    state: AppState,
    owner: i64,
    channel: i64,
    media: i64,
    status: i64,
}
async fn fixture(pool: PgPool) -> Fixture {
    let owner = common::create_local_account(&pool, "alice", "Alice").await;
    let state = common::test_state_with(pool.clone(), common::StubFederation::with_users(&[]));
    let channel = plamenu::video::create_channel(&state, &owner, "films", "Films")
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
            ..media::NewLocalMedia::new(owner.id, media, &file, "video/mp4")
        },
    )
    .await
    .unwrap();
    state.media.put(&file, (0..128).collect()).await.unwrap();
    state.media.put(&poster, vec![1; 16]).await.unwrap();
    media::set_file_sizes(&pool, media, 128, Some(16))
        .await
        .unwrap();
    sqlx::query("UPDATE media_attachments SET hls_master_url=$2 WHERE id=$1")
        .bind(media)
        .bind(format!(
            "https://{}/media/hls/{media}/master.m3u8",
            state.config.domain
        ))
        .execute(&pool)
        .await
        .unwrap();
    let status = actions::post_video(
        &state,
        PostParams {
            username: "alice",
            media_ids: &[media],
            title: Some("Private delivery totals"),
            text: "Film",
            visibility: "public",
            kind: PostKind::Video,
            ..Default::default()
        },
        channel.id,
    )
    .await
    .unwrap()
    .0
    .id;
    Fixture {
        state,
        owner: owner.id,
        channel: channel.id,
        media,
        status,
    }
}
async fn request(
    state: &AppState,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: &str,
    range: Option<&str>,
) -> axum::response::Response {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("Content-Type", "application/json");
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    if let Some(range) = range {
        request = request.header("Range", range);
    }
    plamenu::build_router(state.clone())
        .oneshot(request.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap()
}
async fn body(response: axum::response::Response) -> Vec<u8> {
    response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}
async fn totals(f: &Fixture) -> (i64, i64) {
    analytics::flush(&f.state).await.unwrap();
    sqlx::query_as("SELECT COALESCE(SUM(requests),0)::bigint,COALESCE(SUM(body_bytes),0)::bigint FROM video_delivery_daily WHERE channel_id=$1")
        .bind(f.channel).fetch_one(&f.state.pool).await.unwrap()
}
async fn token(f: &Fixture, actor: i64, scope: &str) -> (String, String) {
    let user = plamenu_db::user::set_credentials(
        &f.state.pool,
        actor,
        Some(&format!("{actor}@example.test")),
        "hash",
    )
    .await
    .unwrap();
    let app = plamenu::web::session::ensure_web_app(&f.state)
        .await
        .unwrap();
    let raw = format!("analytics-{}", id::next());
    plamenu_db::oauth::create_token(
        &f.state.pool,
        &plamenu::auth::hash_secret(&raw),
        app.id,
        Some(user.id),
        scope,
    )
    .await
    .unwrap();
    let csrf = plamenu::auth::hash_secret(&format!("plamenu-csrf:{raw}"));
    (raw, csrf)
}

#[sqlx::test(migrations = "../db/migrations")]
async fn opt_in_counts_polled_range_bytes_not_head_errors_metadata_or_unconsumed_bodies(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let path = format!("/media/{}.mp4", f.media);
    assert_eq!(
        body(request(&f.state, "GET", &path, None, "", None).await)
            .await
            .len(),
        128
    );
    assert_eq!(totals(&f).await, (0, 0));
    analytics::change(&f.state, f.owner, f.channel, true, 30, false)
        .await
        .unwrap();
    drop(request(&f.state, "GET", &path, None, "", None).await);
    assert_eq!(
        request(&f.state, "HEAD", &path, None, "", None)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        request(&f.state, "GET", &path, None, "", Some("bytes=200-300"))
            .await
            .status(),
        StatusCode::RANGE_NOT_SATISFIABLE
    );
    assert_eq!(
        request(&f.state, "GET", "/media/missing.mp4", None, "", None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let response = request(&f.state, "GET", &path, None, "", Some("bytes=4-35")).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(body(response).await, (4..36).collect::<Vec<_>>());
    assert_eq!(
        body(
            request(
                &f.state,
                "GET",
                &format!("/media/{}/download", f.media),
                None,
                "",
                Some("bytes=0-7")
            )
            .await
        )
        .await
        .len(),
        8
    );
    assert_eq!(
        body(
            request(
                &f.state,
                "GET",
                &format!("/media/play/{}/video.mp4", f.media),
                None,
                "",
                Some("bytes=0-15")
            )
            .await
        )
        .await
        .len(),
        16
    );
    assert_eq!(totals(&f).await, (3, 56));
    let fields:Vec<String>=sqlx::query_scalar("SELECT column_name FROM information_schema.columns WHERE table_name='video_delivery_daily' ORDER BY column_name")
        .fetch_all(&f.state.pool).await.unwrap();
    assert_eq!(
        fields,
        [
            "body_bytes",
            "channel_id",
            "day",
            "lane",
            "media_id",
            "requests"
        ]
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn erase_disable_and_reenable_fence_open_bodies_and_other_collector_generations(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    let path = format!("/media/{}.mp4", f.media);
    analytics::change(&f.state, f.owner, f.channel, true, 30, false)
        .await
        .unwrap();
    let old = request(&f.state, "GET", &path, None, "", None).await;
    let mut other = f.state.clone();
    other.video_analytics = std::sync::Arc::new(analytics::Collector::default());
    analytics::flush(&other).await.unwrap();
    let stale = request(&other, "GET", &path, None, "", None).await;
    analytics::change(&f.state, f.owner, f.channel, true, 30, true)
        .await
        .unwrap();
    assert_eq!(body(old).await.len(), 128);
    assert_eq!(body(stale).await.len(), 128);
    analytics::flush(&other).await.unwrap();
    assert_eq!(totals(&f).await, (0, 0));
    assert_eq!(
        body(request(&f.state, "GET", &path, None, "", Some("bytes=0-31")).await)
            .await
            .len(),
        32
    );
    assert_eq!(totals(&f).await, (1, 32));
    analytics::change(&f.state, f.owner, f.channel, false, 30, false)
        .await
        .unwrap();
    assert_eq!(totals(&f).await, (0, 0));
    assert_eq!(
        body(request(&f.state, "GET", &path, None, "", None).await)
            .await
            .len(),
        128
    );
    analytics::change(&f.state, f.owner, f.channel, true, 30, false)
        .await
        .unwrap();
    assert_eq!(totals(&f).await, (0, 0));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn owner_editor_publisher_scopes_and_owner_transfer_remain_private(pool: PgPool) {
    let f = fixture(pool).await;
    let editor = common::create_local_account(&f.state.pool, "editor", "Editor").await;
    let publisher = common::create_local_account(&f.state.pool, "publisher", "Publisher").await;
    let next = common::create_local_account(&f.state.pool, "next", "Next").await;
    for (actor, role) in [(editor.id, "editor"), (publisher.id, "publisher")] {
        sqlx::query("INSERT INTO video_channel_grants(channel_id,account_id,role,accepted) VALUES($1,$2,$3,true)")
            .bind(f.channel).bind(actor).bind(role).execute(&f.state.pool).await.unwrap();
    }
    assert!(
        analytics::access(&f.state, f.owner, f.channel)
            .await
            .is_ok()
    );
    assert!(
        analytics::access(&f.state, editor.id, f.channel)
            .await
            .is_ok()
    );
    assert!(
        analytics::access(&f.state, publisher.id, f.channel)
            .await
            .is_err()
    );
    assert!(
        analytics::change(&f.state, editor.id, f.channel, true, 30, false)
            .await
            .is_err()
    );
    let (read, _) = token(&f, f.owner, "read:media").await;
    let (write, _) = token(&f, f.owner, "write:media").await;
    let path = format!("/api/v1/video_channels/{}/analytics", f.channel);
    let response = request(&f.state, "GET", &path, Some(&read), "", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["Cache-Control"], "private, no-store");
    assert_eq!(
        request(&f.state, "GET", &path, Some(&write), "", None)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &f.state,
            "PUT",
            &path,
            Some(&read),
            "{\"enabled\":true,\"retention_days\":30}",
            None
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &f.state,
            "PUT",
            &path,
            Some(&write),
            "{\"enabled\":true,\"retention_days\":30}",
            None
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        request(
            &f.state,
            "PUT",
            &path,
            Some(&write),
            "{\"enabled\":true,\"retention_days\":91}",
            None
        )
        .await
        .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    sqlx::query("UPDATE video_channels SET owner_account_id=$2 WHERE account_id=$1")
        .bind(f.channel)
        .bind(next.id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(
        request(&f.state, "GET", &path, Some(&read), "", None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert!(
        analytics::change(&f.state, f.owner, f.channel, false, 30, true)
            .await
            .is_err()
    );
    assert!(
        analytics::change(&f.state, next.id, f.channel, false, 30, true)
            .await
            .is_ok()
    );
    sqlx::query("UPDATE accounts SET deleted_at=now() WHERE id=$1")
        .bind(next.id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert!(
        analytics::access(&f.state, editor.id, f.channel)
            .await
            .is_err()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn retained_rows_expire_and_deleted_or_moderated_publications_do_not_gain_new_counts(
    pool: PgPool,
) {
    let f = fixture(pool).await;
    analytics::change(&f.state, f.owner, f.channel, true, 30, false)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO video_delivery_daily(channel_id,media_id,day,lane,requests,body_bytes)
        VALUES($1,$2,(now() AT TIME ZONE 'UTC')::date-2,'file',1,32)",
    )
    .bind(f.channel)
    .bind(f.media)
    .execute(&f.state.pool)
    .await
    .unwrap();
    analytics::change(&f.state, f.owner, f.channel, true, 1, false)
        .await
        .unwrap();
    assert_eq!(totals(&f).await, (0, 0));
    let path = format!("/media/{}.mp4", f.media);
    let open = request(&f.state, "GET", &path, None, "", None).await;
    sqlx::query("UPDATE accounts SET suspended_at=now() WHERE id=$1")
        .bind(f.owner)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(body(open).await.len(), 128);
    assert_eq!(totals(&f).await, (0, 0));
    assert_eq!(
        request(&f.state, "GET", &path, None, "", None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    sqlx::query("UPDATE accounts SET suspended_at=NULL WHERE id=$1")
        .bind(f.owner)
        .execute(&f.state.pool)
        .await
        .unwrap();
    let _ = body(request(&f.state, "GET", &path, None, "", None).await).await;
    assert_eq!(totals(&f).await, (1, 128));
    sqlx::query("UPDATE statuses SET deleted_at=now() WHERE id=$1")
        .bind(f.status)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert!(
        analytics::daily(&f.state, f.channel)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(totals(&f).await, (0, 0));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn operator_kill_switch_and_failed_accounting_never_break_media_delivery(pool: PgPool) {
    let mut f = fixture(pool).await;
    analytics::change(&f.state, f.owner, f.channel, true, 30, false)
        .await
        .unwrap();
    let path = format!("/media/{}.mp4", f.media);
    sqlx::query("ALTER TABLE video_analytics_policies RENAME TO unavailable_analytics_policies")
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(
        body(request(&f.state, "GET", &path, None, "", None).await)
            .await
            .len(),
        128
    );
    assert!(analytics::flush(&f.state).await.is_err());
    sqlx::query("ALTER TABLE unavailable_analytics_policies RENAME TO video_analytics_policies")
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(totals(&f).await, (0, 0));
    let _ = body(request(&f.state, "GET", &path, None, "", None).await).await;
    assert_eq!(totals(&f).await, (1, 128));
    std::sync::Arc::make_mut(&mut f.state.config).video_analytics = false;
    assert_eq!(
        body(request(&f.state, "GET", &path, None, "", None).await)
            .await
            .len(),
        128
    );
    assert_eq!(totals(&f).await, (0, 0));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn ordinary_html_forms_require_csrf_scope_and_confirmed_erasure(pool: PgPool) {
    let f = fixture(pool).await;
    let (raw, csrf) = token(&f, f.owner, "read:media write:media").await;
    let path = format!("/web/video-channels/{}/analytics", f.channel);
    let send = |body: String| {
        plamenu::build_router(f.state.clone()).oneshot(
            Request::builder()
                .method("POST")
                .uri(&path)
                .header("Cookie", format!("__Host-plamenu_session={raw}"))
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
    };
    assert_eq!(
        send("enabled=yes&retention_days=30&operation=save".into())
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(format!(
            "csrf={csrf}&enabled=yes&retention_days=30&operation=save"
        ))
        .await
        .unwrap()
        .status(),
        StatusCode::SEE_OTHER
    );
    assert!(
        analytics::policy(&f.state, f.channel)
            .await
            .unwrap()
            .enabled
    );
    assert_eq!(
        send(format!("csrf={csrf}&retention_days=30&operation=erase"))
            .await
            .unwrap()
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        send(format!(
            "csrf={csrf}&retention_days=1&confirm_erase=yes&operation=erase"
        ))
        .await
        .unwrap()
        .status(),
        StatusCode::SEE_OTHER
    );
    let policy = analytics::policy(&f.state, f.channel).await.unwrap();
    assert!(policy.enabled);
    assert_eq!(policy.retention_days, 30);
    let response = plamenu::build_router(f.state.clone())
        .oneshot(
            Request::builder()
                .uri(format!("/videos/channels/{}/analytics", f.channel))
                .header("Cookie", format!("__Host-plamenu_session={raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["Cache-Control"], "private, no-store");
    assert_eq!(response.headers()["Vary"], "Cookie");
    let html = String::from_utf8(body(response).await).unwrap();
    assert!(html.contains("Confirm permanent deletion"));
    assert!(html.contains("name=\"csrf\""));
}
