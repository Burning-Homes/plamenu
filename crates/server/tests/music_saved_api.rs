mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::{StubFederation, create_local_account, test_state_with};
use http_body_util::BodyExt;
use plamenu::{
    auth::{generate_secret, hash_password, hash_secret},
    music_radios,
};
use plamenu_db::{PgPool, oauth, user};
use serde_json::{Value, json};
use tower::ServiceExt;
async fn tokens(pool: &PgPool, owner: i64, email: &str) -> (String, String) {
    let user = user::create(
        pool,
        owner,
        Some(email),
        &hash_password("fixture-password").unwrap(),
    )
    .await
    .unwrap();
    let app = oauth::create_app(
        pool,
        oauth::NewApp {
            name: "saved-tests",
            website: None,
            client_id: &generate_secret(),
            client_secret_hash: &hash_secret(&generate_secret()),
            redirect_uris: &["urn:ietf:wg:oauth:2.0:oob".into()],
            scopes: "read write",
        },
    )
    .await
    .unwrap();
    let mut result = Vec::new();
    for scope in ["read:statuses", "write:statuses"] {
        let token = generate_secret();
        oauth::create_token(pool, &hash_secret(&token), app.id, Some(user.id), scope)
            .await
            .unwrap();
        result.push(token);
    }
    (result.remove(0), result.remove(0))
}
#[sqlx::test(migrations = "../db/migrations")]
async fn saved_api_enforces_scopes_owner_private_cache_and_station_config_redaction(pool: PgPool) {
    let owner = create_local_account(&pool, "alice", "Alice").await.id;
    let viewer = create_local_account(&pool, "bob", "Bob").await.id;
    let state = test_state_with(pool, StubFederation::with_actors([]));
    let radio = music_radios::create(
        &state,
        owner,
        &music_radios::New {
            name: "Radio".into(),
            description: String::new(),
            config: "Owner-only configuration".into(),
            visibility: "private".into(),
        },
    )
    .await
    .unwrap();
    let (owner_read, owner_write) = tokens(&state.pool, owner, "alice@plamenu.test").await;
    let (viewer_read, viewer_write) = tokens(&state.pool, viewer, "bob@plamenu.test").await;
    let app = plamenu::build_router(state.clone());
    for path in ["/api/v1/music/saved", "/api/v1/music/mine"] {
        for (token, status) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some(&owner_write), StatusCode::FORBIDDEN),
            (Some(&owner_read), StatusCode::OK),
        ] {
            let mut request = Request::builder().uri(path);
            if let Some(token) = token {
                request = request.header("Authorization", format!("Bearer {token}"));
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), status);
            if status == StatusCode::OK {
                assert_eq!(response.headers()["Cache-Control"], "private, no-store");
                assert_eq!(response.headers()["Vary"], "Authorization, Cookie");
            }
        }
    }
    let input = json!({"kind":"radio","id":radio.id.to_string()}).to_string();
    let mut own_id = None;
    for (token, status) in [
        (&owner_read, StatusCode::FORBIDDEN),
        (&viewer_write, StatusCode::NOT_FOUND),
        (&owner_write, StatusCode::OK),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/music/saved")
                    .header("Authorization", format!("Bearer {token}"))
                    .header("Content-Type", "application/json")
                    .body(Body::from(input.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        if status == StatusCode::OK {
            assert_eq!(response.headers()["Cache-Control"], "private, no-store");
            let data: Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            assert_eq!(data["source"]["config"], radio.config);
            own_id = data["id"].as_str().map(str::to_owned);
        }
    }
    music_radios::update(
        &state,
        owner,
        radio.id,
        radio.revision,
        &music_radios::New {
            name: radio.name,
            description: String::new(),
            config: radio.config,
            visibility: "public".into(),
        },
    )
    .await
    .unwrap();
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/music/saved")
                .header("Authorization", format!("Bearer {viewer_write}"))
                .header("Content-Type", "application/json")
                .body(Body::from(input))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let data: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert!(data["source"].get("config").is_none());
    assert!(data["source"].get("owner_id").is_none());
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/music/saved/{}", own_id.unwrap()))
                .header("Authorization", format!("Bearer {viewer_write}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/music/saved")
                .header("Authorization", format!("Bearer {owner_read}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let data: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(data["items"].as_array().unwrap().len(), 1);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/music/saved")
                .header("Authorization", format!("Bearer {viewer_read}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.headers()["Cache-Control"], "private, no-store");
    let data: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(data["items"].as_array().unwrap().len(), 1);
    assert!(data["items"][0]["source"].get("config").is_none());
}
