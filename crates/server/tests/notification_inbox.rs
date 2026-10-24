use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::{StubFederation, create_local_account, test_state_with};
use http_body_util::BodyExt;
use plamenu::{
    AppState,
    auth::{generate_secret, hash_password, hash_secret},
};
use plamenu_db::{PgPool, marker, notification, notification_read, oauth, user};
use plamenu_test_support as common;
use serde_json::{Value, json};
use tower::ServiceExt;
async fn fixture(pool: PgPool) -> (AppState, i64, i64, i64, Vec<i64>) {
    let owner = create_local_account(&pool, "alice", "Alice").await.id;
    let other = create_local_account(&pool, "bob", "Bob").await.id;
    let u = user::create(
        &pool,
        owner,
        Some("alice@example.test"),
        &hash_password("fixture-password").unwrap(),
    )
    .await
    .unwrap();
    let state = test_state_with(pool, StubFederation::with_actors([]));
    for kind in ["follow", "music.favourite", "webmention"] {
        notification::create(&state.pool, owner, other, kind, None)
            .await
            .unwrap();
    }
    let ids: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM notifications WHERE account_id=$1 ORDER BY id")
            .bind(owner)
            .fetch_all(&state.pool)
            .await
            .unwrap();
    (state, owner, u.id, other, ids)
}
async fn token(state: &AppState, uid: i64, scopes: &str) -> String {
    let app = oauth::create_app(
        &state.pool,
        oauth::NewApp {
            name: "Inbox tests",
            website: None,
            client_id: &generate_secret(),
            client_secret_hash: &hash_secret(&generate_secret()),
            redirect_uris: &[],
            scopes: "read write",
        },
    )
    .await
    .unwrap();
    let token = generate_secret();
    oauth::create_token(&state.pool, &hash_secret(&token), app.id, Some(uid), scopes)
        .await
        .unwrap();
    token
}
async fn json_response(response: axum::response::Response) -> Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn sparse_read_state_survives_markers_is_shared_with_lemmy_and_is_owner_scoped(pool: PgPool) {
    let (state, owner, uid, other, ids) = fixture(pool).await;
    assert!(
        notification::has_unread(&state.pool, uid, owner)
            .await
            .unwrap()
    );
    assert_eq!(
        notification_read::set_many(&state.pool, other, &ids, true)
            .await
            .unwrap(),
        0
    );
    notification_read::mark_all_read(&state.pool, uid, owner)
        .await
        .unwrap();
    assert!(
        !notification::has_unread(&state.pool, uid, owner)
            .await
            .unwrap()
    );
    notification_read::set_read(&state.pool, owner, ids[0], false)
        .await
        .unwrap();
    assert!(
        notification::has_unread(&state.pool, uid, owner)
            .await
            .unwrap()
    );
    let unread = notification_read::list(
        &state.pool,
        uid,
        owner,
        notification_read::Filter {
            unread: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        unread.iter().map(|n| n.id).collect::<Vec<_>>(),
        vec![ids[0]]
    );
    assert!(
        !plamenu_db::lemmy_inbox::read_states(&state.pool, uid, owner, &ids)
            .await
            .unwrap()[&ids[0]]
    );
    marker::advance(&state.pool, uid, "notifications", ids[2])
        .await
        .unwrap();
    assert!(
        notification::has_unread(&state.pool, uid, owner)
            .await
            .unwrap()
    );
    notification::dismiss(&state.pool, owner, ids[0])
        .await
        .unwrap();
    assert!(
        !notification::has_unread(&state.pool, uid, owner)
            .await
            .unwrap()
    );
    let remaining: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM lemmy_notification_read_overrides WHERE account_id=$1",
    )
    .bind(owner)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(remaining, 0);
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn inbox_api_supports_scoped_bulk_read_delete_filters_and_preserves_foreign_ids(
    pool: PgPool,
) {
    let (state, owner, uid, other, ids) = fixture(pool).await;
    notification::create(&state.pool, other, owner, "follow", None)
        .await
        .unwrap();
    let foreign: i64 = sqlx::query_scalar("SELECT id FROM notifications WHERE account_id=$1")
        .bind(other)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let auth = token(&state, uid, "read:notifications write:notifications").await;
    let read_only = token(&state, uid, "read:notifications").await;
    let app = plamenu::build_router(state.clone());
    let body = json!({"ids":[ids[0].to_string(),foreign.to_string()]}).to_string();
    let r = app
        .clone()
        .oneshot(
            Request::post("/api/v1/notifications/seen")
                .header("authorization", format!("Bearer {read_only}"))
                .header("content-type", "application/json")
                .body(Body::from(body.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let r = app
        .clone()
        .oneshot(
            Request::post("/api/v1/notifications/seen")
                .header("authorization", format!("Bearer {auth}"))
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(json_response(r).await["updated"], 1);
    let r = app
        .clone()
        .oneshot(
            Request::get("/api/v1/notifications/inbox?unread=true&kind=follow")
                .header("authorization", format!("Bearer {auth}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(json_response(r).await["items"].as_array().unwrap().len(), 0);
    let r = app
        .clone()
        .oneshot(
            Request::post("/api/v1/notifications/delete_bulk")
                .header("authorization", format!("Bearer {auth}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"ids":[ids[0].to_string(),foreign.to_string()]}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(json_response(r).await["deleted"], 1);
    assert!(
        notification::find(&state.pool, foreign)
            .await
            .unwrap()
            .is_some()
    );
    let r = app
        .oneshot(
            Request::get("/api/v1/notifications/inbox?kind=unknown")
                .header("authorization", format!("Bearer {auth}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn native_inbox_auto_reads_without_scripts_and_clear_requires_csrf_and_confirmation(
    pool: PgPool,
) {
    let (state, owner, uid, _, ids) = fixture(pool).await;
    let app = plamenu::build_router(state.clone());
    let login = app
        .clone()
        .oneshot(
            Request::post("/login")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(
                    "email=alice%40example.test&password=fixture-password",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::SEE_OTHER);
    let cookie = login
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("__Host-plamenu_session="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let csrf = hash_secret(&format!(
        "plamenu-csrf:{}",
        cookie.split_once('=').unwrap().1
    ));
    let r = app
        .clone()
        .oneshot(
            Request::get("/notifications/inbox")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let text =
        String::from_utf8(r.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(text.contains("/web/notifications/inbox"));
    assert!(text.contains("class=\"inbox-filter\""));
    assert!(text.contains("class=\"inbox-options\""));
    assert!(text.contains("href=\"/notifications/requests\">Filtered notifications</a>"));
    assert!(text.contains("class=\"notification\""));
    for removed in [
        "Mark all read",
        "Mark unread",
        "Mark read",
        "Opening this inbox",
        "notification-inbox-item",
        "notification-inbox-actions",
        "inbox-tabs",
        "bell__dot",
    ] {
        assert!(!text.contains(removed), "unexpected {removed}");
    }
    assert!(
        !notification::has_unread(&state.pool, uid, owner)
            .await
            .unwrap()
    );
    let shared = marker::find(&state.pool, uid, "notifications")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(shared.last_read_id, ids[2]);
    for (body, status) in [
        (
            format!("csrf=wrong&operation=seen&id={}", ids[0]),
            StatusCode::FORBIDDEN,
        ),
        (
            format!("csrf={csrf}&operation=clear"),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (format!("csrf={csrf}&operation=all"), StatusCode::SEE_OTHER),
        (
            format!("csrf={csrf}&operation=unseen&id={}", ids[0]),
            StatusCode::SEE_OTHER,
        ),
    ] {
        let r = app
            .clone()
            .oneshot(
                Request::post("/web/notifications/inbox")
                    .header("cookie", &cookie)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), status);
    }
    assert!(
        notification::has_unread(&state.pool, uid, owner)
            .await
            .unwrap()
    );
    for (path, expected) in [
        (format!("/notifications?before={}", ids[2]), StatusCode::OK),
        (
            "/notifications?kind=unknown".into(),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::get(path)
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        assert!(
            notification::has_unread(&state.pool, uid, owner)
                .await
                .unwrap()
        );
    }
    // Kind filtering stays available, without changing read-on-open behavior.
    let response = app
        .clone()
        .oneshot(
            Request::get("/notifications?kind=follow")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
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
    assert_eq!(text.matches("<article class=\"notification\"").count(), 1);
    assert!(text.contains("value=\"follow\" selected"));
    assert!(
        !notification::has_unread(&state.pool, uid, owner)
            .await
            .unwrap()
    );
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn activity_read_includes_private_notifications_is_bounded_and_never_rewinds_marker(
    pool: PgPool,
) {
    use plamenu_db::status::{self, NewLocalStatus};
    let (state, owner, uid, other, ids) = fixture(pool).await;
    let direct = status::create_local(
        &state.pool,
        NewLocalStatus::new(other, "A private message", "direct", None),
    )
    .await
    .unwrap();
    notification::create(&state.pool, owner, other, "mention", Some(direct.id))
        .await
        .unwrap();
    let message: i64 =
        sqlx::query_scalar("SELECT id FROM notifications WHERE account_id=$1 AND status_id=$2")
            .bind(owner)
            .bind(direct.id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    notification::create(&state.pool, owner, other, "music.share", None)
        .await
        .unwrap();
    let head = notification_read::activity_head(&state.pool, owner)
        .await
        .unwrap()
        .unwrap();
    assert!(head > message);
    notification_read::set_read(&state.pool, owner, ids[0], false)
        .await
        .unwrap();
    // A newer arrival must survive completion of an already-open page.
    notification::create(&state.pool, owner, other, "music.download", None)
        .await
        .unwrap();
    let newer = notification_read::activity_head(&state.pool, owner)
        .await
        .unwrap()
        .unwrap();
    notification_read::mark_activity_read(&state.pool, uid, owner, head)
        .await
        .unwrap();
    let states =
        notification_read::read_states(&state.pool, uid, owner, &[ids[0], head, message, newer])
            .await
            .unwrap();
    assert!(states[&ids[0]]);
    assert!(states[&head]);
    assert!(states[&message]);
    assert!(!states[&newer]);
    assert_eq!(
        marker::find(&state.pool, uid, "notifications")
            .await
            .unwrap()
            .unwrap()
            .last_read_id,
        head
    );

    // A concurrent API marker advance and a read message must survive an
    // older page finishing. Explicit unread activity beyond its head does too.
    marker::advance(&state.pool, uid, "notifications", newer)
        .await
        .unwrap();
    notification_read::set_read(&state.pool, owner, message, true)
        .await
        .unwrap();
    notification_read::set_read(&state.pool, owner, newer, false)
        .await
        .unwrap();
    notification_read::mark_activity_read(&state.pool, uid, owner, head)
        .await
        .unwrap();
    let states = notification_read::read_states(&state.pool, uid, owner, &[message, newer])
        .await
        .unwrap();
    assert!(states[&message]);
    assert!(!states[&newer]);
    assert_eq!(
        marker::find(&state.pool, uid, "notifications")
            .await
            .unwrap()
            .unwrap()
            .last_read_id,
        newer
    );
}
