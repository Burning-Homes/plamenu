//! OAuth token-endpoint admission and minting bounds: the
//! previously-unclassified `POST /oauth/token` now draws a per-IP admission
//! budget, `client_credentials` scopes are bounded through the canonical
//! parser, and app-level tokens are capped per app so an anonymous caller that
//! registered one app cannot grow the token table without limit.

mod common;

use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, Response, StatusCode, header};
use common::test_state_with;
use http_body_util::BodyExt;
use plamenu::auth::hash_secret;
use plamenu_db::{PgPool, oauth};
use serde_json::{Value, json};
use tower::ServiceExt;

/// Registers an app with a known client secret and the given granted scopes.
async fn seed_app(pool: &PgPool, scopes: &str) -> String {
    let client_id = "cid-admission";
    oauth::create_app(
        pool,
        oauth::NewApp {
            name: "admission-tests",
            website: None,
            client_id,
            client_secret_hash: &hash_secret("s3cret"),
            redirect_uris: &["urn:ietf:wg:oauth:2.0:oob".to_owned()],
            scopes,
        },
    )
    .await
    .unwrap();
    client_id.to_owned()
}

fn token_request(client_id: &str, scope: Option<&str>) -> Request<Body> {
    let mut body = json!({
        "grant_type": "client_credentials",
        "client_id": client_id,
        "client_secret": "s3cret",
    });
    if let Some(scope) = scope {
        body["scope"] = json!(scope);
    }
    Request::builder()
        .method("POST")
        .uri("/oauth/token")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

fn from_ip(mut request: Request<Body>, ip: &str) -> Request<Body> {
    let addr = SocketAddr::new(ip.parse().unwrap(), 40000);
    request.extensions_mut().insert(ConnectInfo(addr));
    request
}

async fn send(app: Router, request: Request<Body>) -> Response<Body> {
    app.oneshot(request).await.unwrap()
}

async fn body_json(response: Response<Body>) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// `POST /oauth/token` now counts against a per-IP bucket, so a flood of token
/// mints from one address is turned away with 429 — before, this endpoint drew
/// from no bucket at all.
#[sqlx::test(migrations = "../db/migrations")]
async fn token_endpoint_is_throttled_per_ip(pool: PgPool) {
    // The token endpoint shares the login-attempts budget.
    sqlx::query!("UPDATE instance_settings SET rate_limit_login_attempts = 2")
        .execute(&pool)
        .await
        .unwrap();
    let client_id = seed_app(&pool, "read").await;
    let app = plamenu::build_router(test_state_with(pool, std::sync::Arc::default()));

    for _ in 0..2 {
        let ok = send(
            app.clone(),
            from_ip(token_request(&client_id, None), "203.0.113.9"),
        )
        .await;
        assert_eq!(ok.status(), StatusCode::OK);
    }
    let throttled = send(
        app.clone(),
        from_ip(token_request(&client_id, None), "203.0.113.9"),
    )
    .await;
    assert_eq!(throttled.status(), StatusCode::TOO_MANY_REQUESTS);

    // A different address keeps its own budget.
    let other = send(
        app,
        from_ip(token_request(&client_id, None), "203.0.113.10"),
    )
    .await;
    assert_eq!(other.status(), StatusCode::OK);
}

/// A `client_credentials` request cannot mint scopes broader than the app
/// registered, and an unrecognized scope string is rejected outright — so the
/// persisted `scopes` value is bounded by the canonical parser.
#[sqlx::test(migrations = "../db/migrations")]
async fn client_credentials_scope_is_bounded(pool: PgPool) {
    let client_id = seed_app(&pool, "read").await;
    let app = plamenu::build_router(test_state_with(pool, std::sync::Arc::default()));

    // An unknown scope token fails the canonical parser.
    let unknown = send(
        app.clone(),
        from_ip(token_request(&client_id, Some("superuser")), "203.0.113.11"),
    )
    .await;
    assert_eq!(unknown.status(), StatusCode::BAD_REQUEST);

    // A real scope the app never registered for (it only has `read`) is refused
    // by the lattice subset check.
    let broader = send(
        app.clone(),
        from_ip(token_request(&client_id, Some("write")), "203.0.113.11"),
    )
    .await;
    assert_eq!(broader.status(), StatusCode::BAD_REQUEST);

    // The registered scope is accepted and echoed back normalized.
    let ok = send(
        app,
        from_ip(token_request(&client_id, Some("read read")), "203.0.113.11"),
    )
    .await;
    assert_eq!(ok.status(), StatusCode::OK);
    assert_eq!(body_json(ok).await["scope"], "read");
}

/// Minting app-level tokens through the endpoint holds one app to a bounded
/// number of live tokens, however many are requested.
#[sqlx::test(migrations = "../db/migrations")]
async fn app_level_tokens_are_capped_end_to_end(pool: PgPool) {
    // Raise the admission budget so the cap, not the throttle, is what bounds
    // the row count in this test.
    sqlx::query!("UPDATE instance_settings SET rate_limit_login_attempts = 1000")
        .execute(&pool)
        .await
        .unwrap();
    let client_id = seed_app(&pool, "read").await;
    let app_row = oauth::find_app_by_client_id(&pool, &client_id)
        .await
        .unwrap()
        .unwrap();
    let app = plamenu::build_router(test_state_with(pool.clone(), std::sync::Arc::default()));

    for _ in 0..25 {
        let ok = send(
            app.clone(),
            from_ip(token_request(&client_id, None), "203.0.113.12"),
        )
        .await;
        assert_eq!(ok.status(), StatusCode::OK);
    }

    // Whatever the request volume, the app holds at most the cap (10) live
    // app-level tokens.
    assert_eq!(
        oauth::count_live_app_tokens(&pool, app_row.id)
            .await
            .unwrap(),
        10
    );
}

/// The `authorization_code` path must bound the requested scope exactly like
/// `client_credentials` does. Before this check the `scope` query parameter was
/// carried verbatim from `/oauth/authorize` into the grant and then into the
/// minted token, so an app that registered for `read` could ask for — and be
/// issued — `write`, or `admin:write`, entirely outside its registration
/// (RFC 6749 §4.1.2.1 `invalid_scope`).
#[sqlx::test(migrations = "../db/migrations")]
async fn authorization_code_scope_cannot_exceed_the_app_registration(pool: PgPool) {
    use plamenu_db::user;

    let client_id = seed_app(&pool, "read").await;
    let account = common::create_local_account(&pool, "alice", "Alice").await;
    let hash = plamenu::auth::hash_password("correct horse battery").unwrap();
    user::create(&pool, account.id, Some("alice@example.com"), &hash)
        .await
        .unwrap();
    let app = plamenu::build_router(test_state_with(pool.clone(), std::sync::Arc::default()));

    let authorize_form = |scope: &str| {
        let query = serde_urlencoded::to_string([
            ("response_type", "code"),
            ("client_id", &client_id),
            ("redirect_uri", "urn:ietf:wg:oauth:2.0:oob"),
            ("scope", scope),
        ])
        .unwrap();
        Request::builder()
            .uri(format!("/oauth/authorize?{query}"))
            .body(Body::empty())
            .unwrap()
    };
    let authorize_submit = |scope: &str| {
        let body = serde_urlencoded::to_string([
            ("client_id", client_id.as_str()),
            ("redirect_uri", "urn:ietf:wg:oauth:2.0:oob"),
            ("scope", scope),
            ("identifier", "alice@example.com"),
            ("password", "correct horse battery"),
        ])
        .unwrap();
        Request::builder()
            .method("POST")
            .uri("/oauth/authorize")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::ORIGIN, "https://plamenu.test")
            .body(Body::from(body))
            .unwrap()
    };

    // The consent screen refuses to ask the user to approve access the app
    // never registered for, rather than showing it and granting it anyway.
    assert_eq!(
        send(app.clone(), authorize_form("read write"))
            .await
            .status(),
        StatusCode::BAD_REQUEST,
        "a scope beyond the app's registration must not reach a consent screen",
    );
    assert_eq!(
        send(app.clone(), authorize_form("admin:write"))
            .await
            .status(),
        StatusCode::BAD_REQUEST,
    );
    assert_eq!(
        send(app.clone(), authorize_form("superuser"))
            .await
            .status(),
        StatusCode::BAD_REQUEST,
        "an unknown scope token fails the canonical parser",
    );

    // The hidden fields are client-controlled, so posting past the consent
    // screen is refused too — and mints no grant.
    assert_eq!(
        send(app.clone(), authorize_submit("read write"))
            .await
            .status(),
        StatusCode::BAD_REQUEST,
        "the submit re-checks the scope it was handed",
    );
    let grants: i64 = sqlx::query_scalar("SELECT count(*) FROM oauth_grants")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(grants, 0, "a refused authorization must mint no grant");

    // The registered scope still completes the whole flow, and the token it
    // exchanges for carries exactly that scope.
    let granted = send(app.clone(), authorize_submit("read read")).await;
    assert_eq!(
        granted.status(),
        StatusCode::OK,
        "the OOB code page renders"
    );
    let page = String::from_utf8(
        granted
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    let marker = "<pre class=\"oob-code\">";
    let start = page.find(marker).expect("oob code") + marker.len();
    let code = page[start..].split('<').next().unwrap().to_owned();

    let exchange = Request::builder()
        .method("POST")
        .uri("/oauth/token")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "grant_type": "authorization_code",
                "client_id": client_id,
                "client_secret": "s3cret",
                "redirect_uri": "urn:ietf:wg:oauth:2.0:oob",
                "code": code,
            }))
            .unwrap(),
        ))
        .unwrap();
    let minted = send(app, exchange).await;
    assert_eq!(minted.status(), StatusCode::OK);
    assert_eq!(
        body_json(minted).await["scope"],
        "read",
        "the token carries the normalized, registration-bounded scope",
    );
}
