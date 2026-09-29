mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::{StubFederation, create_local_account, test_state_with};
use http_body_util::BodyExt;
use plamenu::{
    actions::{self, PostParams},
    error::ApiError,
    video_history as service,
};
use plamenu_db::{PgPool, media, video_history as db};
use tower::ServiceExt;

async fn token(pool: &PgPool, account: i64, scope: &str) -> String {
    let user = plamenu_db::user::create(pool, account, Some("bob@example.test"), "unused")
        .await
        .unwrap();
    let app = plamenu_db::oauth::create_app(
        pool,
        plamenu_db::oauth::NewApp {
            name: "history test",
            website: None,
            client_id: "history-test",
            client_secret_hash: "unused",
            redirect_uris: &[],
            scopes: "read write",
        },
    )
    .await
    .unwrap();
    let raw = "private-history-test-token";
    plamenu_db::oauth::create_token(
        pool,
        &plamenu::auth::hash_secret(raw),
        app.id,
        Some(user.id),
        scope,
    )
    .await
    .unwrap();
    raw.into()
}
async fn request(
    app: &axum::Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: serde_json::Value,
) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("Content-Type", "application/json");
    if let Some(token) = token {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let code = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        code,
        headers,
        serde_json::from_slice(&bytes).unwrap_or_default(),
    )
}

#[sqlx::test(migrations = "../db/migrations")]
async fn api_auth_scopes_cache_owner_isolation_and_clear(pool: PgPool) {
    use serde_json::json;
    let (state, _, bob, film, _) = setup(&pool).await;
    let token = token(&pool, bob, "read write").await;
    let app = plamenu::build_router(state);
    assert_eq!(
        request(&app, "GET", "/api/v1/video_history", None, json!({}))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let (code, headers, prefs) = request(
        &app,
        "GET",
        "/api/v1/video_viewer_preferences",
        Some(&token),
        json!({}),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(headers["cache-control"], "private, no-store");
    assert_eq!(prefs["history_enabled"], false);
    assert_eq!(prefs["p2p_enabled"], true);
    assert_eq!(prefs["p2p_configured"], false);
    assert_eq!(
        request(
            &app,
            "PUT",
            "/api/v1/video_viewer_preferences",
            Some(&token),
            json!({"history_enabled":true})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &app,
            "PUT",
            &format!("/api/v1/video_history/{film}"),
            Some(&token),
            json!({"position_seconds":35})
        )
        .await
        .2["saved"],
        true
    );
    let (_, _, history) = request(
        &app,
        "GET",
        "/api/v1/video_history",
        Some(&token),
        json!({}),
    )
    .await;
    assert_eq!(history["data"][0]["media_id"], film.to_string());
    let (_, _, prefs) = request(
        &app,
        "PUT",
        "/api/v1/video_viewer_preferences",
        Some(&token),
        json!({"p2p_enabled":false}),
    )
    .await;
    assert_eq!(prefs["history_enabled"], true);
    assert_eq!(prefs["p2p_enabled"], false);
    assert_eq!(prefs["p2p_configured"], true);
    assert_eq!(db::recent(&pool, bob).await.unwrap().len(), 1);
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/v1/video_history?before_media=1",
            Some(&token),
            json!({})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(
            &app,
            "PUT",
            &format!("/api/v1/video_history/{film}"),
            Some(&token),
            json!({"position_seconds":1.5})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    sqlx::query("UPDATE oauth_tokens SET scopes='read:statuses' WHERE token_hash=$1")
        .bind(plamenu::auth::hash_secret(&token))
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/v1/video_history",
            Some(&token),
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/v1/video_viewer_preferences",
            Some(&token),
            json!({})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "PUT",
            &format!("/api/v1/video_history/{film}"),
            Some(&token),
            json!({"position_seconds":2})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "DELETE",
            "/api/v1/video_history",
            Some(&token),
            json!({})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    sqlx::query("UPDATE oauth_tokens SET scopes='write:statuses' WHERE token_hash=$1")
        .bind(plamenu::auth::hash_secret(&token))
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        request(
            &app,
            "DELETE",
            "/api/v1/video_history",
            Some(&token),
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(db::recent(&pool, bob).await.unwrap().is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn html_settings_save_and_forget_require_csrf_and_clear_confirmation(pool: PgPool) {
    let (state, _, bob, film, _) = setup(&pool).await;
    plamenu_db::user::create(
        &pool,
        bob,
        Some("bob@example.test"),
        &plamenu::auth::hash_password("history-test-password").unwrap(),
    )
    .await
    .unwrap();
    let app = plamenu::build_router(state);
    let login = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(Body::from("identifier=bob&password=history-test-password"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::SEE_OTHER);
    let cookie = login.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let page = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/videos/settings")
                .header("Cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    assert_eq!(page.headers()["cache-control"], "private, no-store");
    let body = String::from_utf8(
        page.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    let csrf = body
        .split("name=\"csrf\" value=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let post = |path: &str, body: String| {
        app.clone().oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("Cookie", &cookie)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
    };
    for path in [
        "/web/videos/settings",
        "/web/videos/history/forget",
        "/web/videos/p2p",
        &format!("/web/videos/{film}/position"),
    ] {
        assert_eq!(
            post(
                path,
                "csrf=wrong&history_enabled=1&position_seconds=1&confirm=1".into()
            )
            .await
            .unwrap()
            .status(),
            StatusCode::FORBIDDEN
        );
    }
    assert!(!db::enabled(&pool, bob).await.unwrap());
    assert_eq!(
        post(
            "/web/videos/settings",
            format!("csrf={csrf}&history_enabled=1")
        )
        .await
        .unwrap()
        .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        post(
            &format!("/web/videos/{film}/position"),
            format!("csrf={csrf}&position_seconds=10")
        )
        .await
        .unwrap()
        .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(db::recent(&pool, bob).await.unwrap().len(), 1);
    assert_eq!(
        post("/web/videos/history/forget", format!("csrf={csrf}"))
            .await
            .unwrap()
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(db::recent(&pool, bob).await.unwrap().len(), 1);
    assert_eq!(
        post(
            "/web/videos/history/forget",
            format!("csrf={csrf}&confirm=1")
        )
        .await
        .unwrap()
        .status(),
        StatusCode::SEE_OTHER
    );
    assert!(db::recent(&pool, bob).await.unwrap().is_empty());

    let sharing = post("/web/videos/p2p", format!("csrf={csrf}&enabled=false"))
        .await
        .unwrap();
    assert_eq!(sharing.status(), StatusCode::OK);
    assert!(!db::preferences(&pool, bob).await.unwrap().p2p_enabled);
    assert!(db::enabled(&pool, bob).await.unwrap());
    let view = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/videos/explore")
                .header("Cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = String::from_utf8(
        view.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(body.contains("data-p2p-default=\"false\""), "{body}");
    let post_id: i64 = sqlx::query_scalar("SELECT status_id FROM media_attachments WHERE id=$1")
        .bind(film)
        .fetch_one(&pool)
        .await
        .unwrap();
    for (path, with_cookie) in [
        (format!("/web/statuses/{post_id}/embed"), true),
        (format!("/web/statuses/{post_id}/embed?p2p=0"), false),
    ] {
        let mut req = Request::builder().uri(path);
        if with_cookie {
            req = req.header("Cookie", &cookie);
        }
        let view = app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(view.status(), StatusCode::OK);
        assert_eq!(view.headers()["cache-control"], "private, no-store");
        let body = String::from_utf8(
            view.into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        assert!(body.contains("data-p2p-default=\"false\""));
        assert!(
            !body.contains("data-p2p-csrf="),
            "public read-only frame exposes no mutation capability"
        );
        assert!(!body.contains("viewer_history"));
    }
    sqlx::query("UPDATE statuses SET visibility='private' WHERE id=$1")
        .bind(post_id)
        .execute(&pool)
        .await
        .unwrap();
    let private = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/web/statuses/{post_id}/embed"))
                .header("Cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        private.status(),
        StatusCode::NOT_FOUND,
        "session preferences never widen embed content authority"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn viewer_preferences_partial_update_keeps_history_and_rolls_back_together(pool: PgPool) {
    let (_, _, bob, film, _) = setup(&pool).await;
    assert!(db::preferences(&pool, bob).await.unwrap().p2p_enabled);
    assert!(!db::preferences(&pool, bob).await.unwrap().p2p_configured);
    db::update_preferences(&pool, bob, Some(true), None)
        .await
        .unwrap();
    assert!(!db::preferences(&pool, bob).await.unwrap().p2p_configured);
    db::save(&pool, bob, film, 1, false).await.unwrap();
    let prefs = db::update_preferences(&pool, bob, None, Some(false))
        .await
        .unwrap();
    assert!(prefs.history_enabled && !prefs.p2p_enabled);
    assert!(prefs.p2p_configured);
    assert_eq!(db::recent(&pool, bob).await.unwrap().len(), 1);
    sqlx::query("CREATE FUNCTION fail_p2p_history_delete() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected preference deletion failure'; END $$").execute(&pool).await.unwrap();
    sqlx::query("CREATE TRIGGER fail_p2p_history_delete BEFORE DELETE ON video_watch_history FOR EACH ROW EXECUTE FUNCTION fail_p2p_history_delete()").execute(&pool).await.unwrap();
    assert!(
        db::update_preferences(&pool, bob, Some(false), Some(true))
            .await
            .is_err()
    );
    let prefs = db::preferences(&pool, bob).await.unwrap();
    assert!(prefs.history_enabled && !prefs.p2p_enabled);
    assert_eq!(db::recent(&pool, bob).await.unwrap().len(), 1);
}

async fn setup(pool: &PgPool) -> (plamenu::AppState, i64, i64, i64, i64) {
    let alice = create_local_account(pool, "alice", "Alice").await;
    let bob = create_local_account(pool, "bob", "Bob").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let channel = plamenu::video::create_channel(&state, &alice, "films", "Films")
        .await
        .unwrap();
    let film = media::create_local(
        pool,
        media::NewLocalMedia {
            kind: "video",
            duration: Some(120.0),
            small: Some(media::SmallStyle {
                file_name: "film.jpg",
                width: 320,
                height: 180,
            }),
            ..media::NewLocalMedia::new(alice.id, plamenu_db::id::next(), "film.mp4", "video/mp4")
        },
    )
    .await
    .unwrap();
    let (s, _) = actions::post_video(
        &state,
        PostParams {
            username: "alice",
            media_ids: &[film.id],
            title: Some("Private history title <&>"),
            text: "A film",
            visibility: "public",
            kind: plamenu_ap::activity::PostKind::Video,
            ..Default::default()
        },
        channel.id,
    )
    .await
    .unwrap();
    (state, alice.id, bob.id, film.id, s.id)
}

#[sqlx::test(migrations = "../db/migrations")]
async fn history_is_opt_in_not_federated_and_disable_deletes(pool: PgPool) {
    let (state, alice, bob, film, post) = setup(&pool).await;
    assert!(!db::enabled(&pool, bob).await.unwrap());
    assert!(!service::save(&state, bob, film, 25, false).await.unwrap());
    assert!(db::recent(&pool, bob).await.unwrap().is_empty());
    db::set_enabled(&pool, bob, true).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM delivery_jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(service::save(&state, bob, film, 25, false).await.unwrap());
    let (entries, _) = service::list(&state, bob, None, 0).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["position_seconds"], 25);
    assert!(
        entries[0]["resume_url"]
            .as_str()
            .unwrap()
            .ends_with("#t=25")
    );
    assert!(
        service::list(&state, alice, None, 0)
            .await
            .unwrap()
            .0
            .is_empty()
    );
    let s = plamenu_db::status::find_by_id(&pool, post)
        .await
        .unwrap()
        .unwrap();
    let entity = plamenu::entities::render_status(&pool, "plamenu.test", &s, Some(bob))
        .await
        .unwrap();
    assert!(
        entity["media_attachments"][0]
            .get("viewer_history")
            .is_none()
    );
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM delivery_jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        count, after,
        "a watch position creates no federation traffic"
    );
    db::set_enabled(&pool, bob, false).await.unwrap();
    assert!(db::recent(&pool, bob).await.unwrap().is_empty());
    assert!(!service::save(&state, bob, film, 26, false).await.unwrap());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn history_rechecks_author_channel_owner_blocks_and_private_access(pool: PgPool) {
    let (state, alice, bob, film, post) = setup(&pool).await;
    db::set_enabled(&pool, bob, true).await.unwrap();
    service::save(&state, bob, film, 25, false).await.unwrap();
    sqlx::query("UPDATE statuses SET visibility='private' WHERE id=$1")
        .bind(post)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        service::list(&state, bob, None, 0)
            .await
            .unwrap()
            .0
            .is_empty()
    );
    assert!(matches!(
        service::save(&state, bob, film, 26, false).await,
        Err(ApiError::NotFound)
    ));
    sqlx::query("UPDATE statuses SET visibility='public' WHERE id=$1")
        .bind(post)
        .execute(&pool)
        .await
        .unwrap();
    for (from, to) in [(alice, bob), (bob, alice)] {
        plamenu_db::block::create(&pool, from, to, None)
            .await
            .unwrap();
        assert!(
            service::list(&state, bob, None, 0)
                .await
                .unwrap()
                .0
                .is_empty()
        );
        assert!(matches!(
            service::save(&state, bob, film, 26, false).await,
            Err(ApiError::NotFound)
        ));
        plamenu_db::block::delete(&pool, from, to).await.unwrap();
    }
    let channel: i64 =
        sqlx::query_scalar("SELECT channel_id FROM video_publications WHERE status_id=$1")
            .bind(post)
            .fetch_one(&pool)
            .await
            .unwrap();
    plamenu_db::block::create(&pool, channel, bob, None)
        .await
        .unwrap();
    assert!(
        service::list(&state, bob, None, 0)
            .await
            .unwrap()
            .0
            .is_empty()
    );
    plamenu_db::block::delete(&pool, channel, bob)
        .await
        .unwrap();
    sqlx::query("UPDATE accounts SET suspended_at=now() WHERE id=$1")
        .bind(channel)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        service::list(&state, bob, None, 0)
            .await
            .unwrap()
            .0
            .is_empty()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn positions_live_and_unpublished_validation(pool: PgPool) {
    let (state, alice, bob, film, _) = setup(&pool).await;
    db::set_enabled(&pool, bob, true).await.unwrap();
    for seconds in [-1, 121, 604_801] {
        assert!(matches!(
            service::save(&state, bob, film, seconds, false).await,
            Err(ApiError::Unprocessable(_))
        ));
    }
    assert!(matches!(
        service::save(&state, bob, film, 20, true).await,
        Err(ApiError::Unprocessable(_))
    ));
    assert!(service::save(&state, bob, film, 120, true).await.unwrap());
    let unposted = media::create_local(
        &pool,
        media::NewLocalMedia {
            kind: "video",
            ..media::NewLocalMedia::new(alice, plamenu_db::id::next(), "draft.mp4", "video/mp4")
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        service::save(&state, alice, unposted.id, 0, false).await,
        Err(ApiError::NotFound)
    ));
    sqlx::query("UPDATE media_attachments SET live_state='live' WHERE id=$1")
        .bind(film)
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        service::save(&state, bob, film, 0, false).await,
        Err(ApiError::NotFound)
    ));
    assert!(
        service::list(&state, bob, None, 0)
            .await
            .unwrap()
            .0
            .is_empty()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn bounded_retention_paging_and_media_cascade(pool: PgPool) {
    let (state, alice, bob, film, post) = setup(&pool).await;
    db::set_enabled(&pool, bob, true).await.unwrap();
    sqlx::query("INSERT INTO media_attachments(id,account_id,status_id,file_name,content_type,kind,processing) SELECT 900000+n,$1,$2,'film.mp4','video/mp4','video','complete' FROM generate_series(1,1001)n")
        .bind(alice).bind(post).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO video_watch_history(account_id,media_id,position_seconds,updated_at) SELECT $1,900000+n,0,now()-interval '1 day' FROM generate_series(1,1001)n")
        .bind(bob).execute(&pool).await.unwrap();
    service::save(&state, bob, film, 1, false).await.unwrap();
    assert_eq!(db::recent(&pool, bob).await.unwrap().len(), 1000);
    let (entries, cursor) = service::list(&state, bob, None, 0).await.unwrap();
    assert_eq!(entries.len(), 20);
    let cursor = cursor.unwrap();
    let (next, _) = service::list(&state, bob, Some(cursor.updated_at), cursor.media_id)
        .await
        .unwrap();
    assert_eq!(next.len(), 20);
    assert!(
        next.iter()
            .all(|n| entries.iter().all(|e| e["media_id"] != n["media_id"]))
    );
    sqlx::query(
        "UPDATE video_watch_history SET updated_at=now()-interval '91 days' WHERE account_id=$1",
    )
    .bind(bob)
    .execute(&pool)
    .await
    .unwrap();
    assert!(db::recent(&pool, bob).await.unwrap().is_empty());
    assert_eq!(db::expire(&pool).await.unwrap(), 1000);
    service::save(&state, bob, film, 1, false).await.unwrap();
    sqlx::query("DELETE FROM media_attachments WHERE id=$1")
        .bind(film)
        .execute(&pool)
        .await
        .unwrap();
    assert!(db::recent(&pool, bob).await.unwrap().is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn opt_out_fences_concurrent_writes_and_failure_rolls_back(pool: PgPool) {
    let (_, _, bob, film, _) = setup(&pool).await;
    db::set_enabled(&pool, bob, true).await.unwrap();
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("SELECT account_id FROM video_viewer_preferences WHERE account_id=$1 FOR UPDATE")
        .bind(bob)
        .execute(&mut *lock)
        .await
        .unwrap();
    let writer_pool = pool.clone();
    let writer =
        tokio::spawn(async move { db::save(&writer_pool, bob, film, 2, false).await.unwrap() });
    sqlx::query("UPDATE video_viewer_preferences SET history_enabled=false WHERE account_id=$1")
        .bind(bob)
        .execute(&mut *lock)
        .await
        .unwrap();
    lock.commit().await.unwrap();
    assert!(!writer.await.unwrap());
    db::set_enabled(&pool, bob, true).await.unwrap();
    db::save(&pool, bob, film, 2, false).await.unwrap();
    sqlx::query("CREATE FUNCTION fail_history_delete() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected deletion failure'; END $$").execute(&pool).await.unwrap();
    sqlx::query("CREATE TRIGGER fail_history_delete BEFORE DELETE ON video_watch_history FOR EACH ROW EXECUTE FUNCTION fail_history_delete()").execute(&pool).await.unwrap();
    assert!(db::set_enabled(&pool, bob, false).await.is_err());
    assert!(db::enabled(&pool, bob).await.unwrap());
    assert_eq!(db::recent(&pool, bob).await.unwrap().len(), 1);
}
