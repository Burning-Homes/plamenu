mod common;
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
#[sqlx::test(migrations = "../db/migrations")]
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
#[sqlx::test(migrations = "../db/migrations")]
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
#[sqlx::test(migrations = "../db/migrations")]
async fn native_inbox_does_not_auto_read_and_forms_require_csrf_and_confirmation(pool: PgPool) {
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
    assert!(text.contains("data-seen=\"false\""));
    assert!(
        notification::has_unread(&state.pool, uid, owner)
            .await
            .unwrap()
    );
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
}
