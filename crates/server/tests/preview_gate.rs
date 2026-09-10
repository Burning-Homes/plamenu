//! The private-by-default gate: anonymous reads of the federated/local/tag
//! timelines and of search are denied unless the matching preview flag is on,
//! while profiles stay public regardless.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{create_local_account, test_app_private};
use http_body_util::BodyExt;
use plamenu::auth::{generate_secret, hash_password, hash_secret};
use plamenu_db::account::Account;
use plamenu_db::oauth::{self, NewApp};
use plamenu_db::{PgPool, user};
use tower::ServiceExt;

const EMAIL: &str = "alice@example.com";
const PASSWORD: &str = "correct horse battery";

/// Seeds `@alice` with login credentials and returns the account.
async fn seed_alice(pool: &PgPool) -> Account {
    let account = create_local_account(pool, "alice", "Alice").await;
    let hash = hash_password(PASSWORD).unwrap();
    user::create(pool, account.id, Some(EMAIL), &hash)
        .await
        .unwrap();
    account
}

/// Mints a bearer token for a user, bypassing the OAuth dance.
async fn mint_token(pool: &PgPool, user_id: i64) -> String {
    let app = oauth::create_app(
        pool,
        NewApp {
            name: "preview-gate-test",
            website: None,
            client_id: "preview-gate-test",
            client_secret_hash: &hash_secret(&generate_secret()),
            redirect_uris: &[],
            scopes: "read write",
        },
    )
    .await
    .unwrap();
    let raw = generate_secret();
    oauth::create_token(
        pool,
        &hash_secret(&raw),
        app.id,
        Some(user_id),
        "read write",
    )
    .await
    .unwrap();
    raw
}

struct Resp {
    status: StatusCode,
    location: Option<String>,
    body: String,
}

async fn get(app: &Router, uri: &str, bearer: Option<&str>, cookie: Option<&str>) -> Resp {
    let mut request = Request::builder().uri(uri);
    if let Some(token) = bearer {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let location = response
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(ToOwned::to_owned);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Resp {
        status,
        location,
        body: String::from_utf8(bytes.to_vec()).unwrap(),
    }
}

/// Logs in through the web form and returns the session cookie pair.
async fn login(app: &Router) -> String {
    let body = serde_urlencoded::to_string([("email", EMAIL), ("password", PASSWORD)]).unwrap();
    let request = Request::builder()
        .method("POST")
        .uri("/login")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let set_cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .expect("session cookie");
    set_cookie.split(';').next().unwrap().to_owned()
}

/// The REST timelines and search reject anonymous reads with a 401 when their
/// preview flag is off.
#[sqlx::test(migrations = "../db/migrations")]
async fn api_timelines_and_search_require_auth_when_private(pool: PgPool) {
    let alice = seed_alice(&pool).await;
    let user_id = user::find_by_email(&pool, EMAIL).await.unwrap().unwrap().id;
    let token = mint_token(&pool, user_id).await;
    let app = test_app_private(pool);

    for uri in [
        "/api/v1/timelines/public",
        "/api/v1/timelines/public?local=true",
        "/api/v1/timelines/tag/rust",
        "/api/v1/tags/rust",
        "/api/v2/search?q=alice",
    ] {
        let anon = get(&app, uri, None, None).await;
        assert_eq!(anon.status, StatusCode::UNAUTHORIZED, "anon GET {uri}");
        assert_eq!(
            anon.body, r#"{"error":"This method requires an authenticated user"}"#,
            "anon GET {uri} body"
        );

        let authed = get(&app, uri, Some(&token), None).await;
        assert_eq!(authed.status, StatusCode::OK, "authed GET {uri}");
    }

    // Profiles stay public even in the private default.
    let profile = get(&app, &format!("/api/v1/accounts/{}", alice.id), None, None).await;
    assert_eq!(profile.status, StatusCode::OK);
}

/// The web timeline and search pages bounce anonymous visitors to `/login`
/// when their preview flag is off; profiles stay public.
#[sqlx::test(migrations = "../db/migrations")]
async fn web_timelines_and_search_redirect_when_private(pool: PgPool) {
    seed_alice(&pool).await;
    let app = test_app_private(pool);
    let cookie = login(&app).await;

    for uri in ["/public", "/public?local=true", "/tags/rust", "/search"] {
        let anon = get(&app, uri, None, None).await;
        assert_eq!(anon.status, StatusCode::SEE_OTHER, "anon GET {uri}");
        assert_eq!(anon.location.as_deref(), Some("/login"), "anon GET {uri}");

        let authed = get(&app, uri, None, Some(&cookie)).await;
        assert_eq!(authed.status, StatusCode::OK, "authed GET {uri}");
    }

    // The profile page renders for anonymous visitors.
    let profile = get(&app, "/@alice", None, None).await;
    assert_eq!(profile.status, StatusCode::OK);
}
