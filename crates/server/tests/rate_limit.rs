//! The Mastodon-style rate limiter: fixed-window throttles with
//! `X-RateLimit-*` headers, keyed per IP / token / user / e-mail, with the
//! trusted-proxy `X-Forwarded-For` resolution in front.

mod common;

use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, Response, StatusCode, header};
use common::{create_local_account, test_app};
use http_body_util::BodyExt;
use plamenu::auth::{generate_secret, hash_password, hash_secret};
use plamenu_db::{PgPool, oauth, user};
use serde_json::{Value, json};
use tower::ServiceExt;

/// A `POST /api/v1/apps` request — the tightest default bucket, so tests set
/// its limit low and hammer it.
fn apps_request() -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/v1/apps")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "client_name": "rate-limit-tests",
                "redirect_uris": ["urn:ietf:wg:oauth:2.0:oob"],
            }))
            .unwrap(),
        ))
        .unwrap()
}

/// Stamps the TCP peer address the router would see from a real socket.
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

fn ratelimit_headers(response: &Response<Body>) -> Option<(String, String, String)> {
    let get = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    Some((
        get("x-ratelimit-limit")?,
        get("x-ratelimit-remaining")?,
        get("x-ratelimit-reset")?,
    ))
}

#[sqlx::test(migrations = "../db/migrations")]
async fn ip_bucket_throttles_and_reports_headers(pool: PgPool) {
    sqlx::query!("UPDATE instance_settings SET rate_limit_app_registrations = 2")
        .execute(&pool)
        .await
        .unwrap();
    // One router for the whole test: counters live in its shared state, as
    // on a running server.
    let app = plamenu::build_router(common::test_state_with(pool, std::sync::Arc::default()));

    let first = send(app.clone(), from_ip(apps_request(), "203.0.113.5")).await;
    assert_eq!(first.status(), StatusCode::OK);
    let (limit, remaining, reset) = ratelimit_headers(&first).expect("headers on success");
    assert_eq!(limit, "2");
    assert_eq!(remaining, "1");
    assert!(
        reset.ends_with(".000000Z") && reset.contains('T'),
        "ruby iso8601(6) shape, got {reset}"
    );

    let second = send(app.clone(), from_ip(apps_request(), "203.0.113.5")).await;
    assert_eq!(second.status(), StatusCode::OK);
    let (_, remaining, _) = ratelimit_headers(&second).unwrap();
    assert_eq!(remaining, "0");

    let third = send(app.clone(), from_ip(apps_request(), "203.0.113.5")).await;
    assert_eq!(third.status(), StatusCode::TOO_MANY_REQUESTS);
    let (limit, remaining, _) = ratelimit_headers(&third).unwrap();
    assert_eq!(limit, "2");
    assert_eq!(remaining, "0");
    assert_eq!(
        third
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
    assert_eq!(
        third
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|value| value.to_str().ok()),
        Some("*")
    );
    assert_eq!(body_json(third).await["error"], "Too many requests");

    // Another address is an independent bucket.
    let other = send(app, from_ip(apps_request(), "203.0.113.6")).await;
    assert_eq!(other.status(), StatusCode::OK);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn ipv6_clients_share_their_64_prefix(pool: PgPool) {
    sqlx::query!("UPDATE instance_settings SET rate_limit_unauthenticated_api = 1")
        .execute(&pool)
        .await
        .unwrap();
    let state = common::test_state_with(pool, std::sync::Arc::default());
    let app = plamenu::build_router(state);

    let get_instance = || {
        Request::builder()
            .method("GET")
            .uri("/api/v1/instance")
            .body(Body::empty())
            .unwrap()
    };
    let first = send(app.clone(), from_ip(get_instance(), "2001:db8:0:1:aaaa::1")).await;
    assert_eq!(first.status(), StatusCode::OK);
    // Same /64: throttled together.
    let sibling = send(app.clone(), from_ip(get_instance(), "2001:db8:0:1:bbbb::2")).await;
    assert_eq!(sibling.status(), StatusCode::TOO_MANY_REQUESTS);
    // Different /64: fresh bucket.
    let neighbor = send(app, from_ip(get_instance(), "2001:db8:0:2::1")).await;
    assert_eq!(neighbor.status(), StatusCode::OK);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn per_token_bucket_throttles_authenticated_api(pool: PgPool) {
    sqlx::query!("UPDATE instance_settings SET rate_limit_per_token_api = 2")
        .execute(&pool)
        .await
        .unwrap();
    let account = create_local_account(&pool, "alice", "alice").await;
    let hash = hash_password("correct horse battery").unwrap();
    let row = user::create(&pool, account.id, Some("alice@example.com"), &hash)
        .await
        .unwrap();
    let app_row = oauth::create_app(
        &pool,
        oauth::NewApp {
            name: "rate-limit-tests",
            website: None,
            client_id: &generate_secret(),
            client_secret_hash: &hash_secret(&generate_secret()),
            redirect_uris: &["urn:ietf:wg:oauth:2.0:oob".to_owned()],
            scopes: "read write",
        },
    )
    .await
    .unwrap();
    let token = generate_secret();
    oauth::create_token(
        &pool,
        &hash_secret(&token),
        app_row.id,
        Some(row.id),
        "read",
    )
    .await
    .unwrap();

    let state = common::test_state_with(pool, std::sync::Arc::default());
    let app = plamenu::build_router(state);
    let verify = || {
        Request::builder()
            .method("GET")
            .uri("/api/v1/accounts/verify_credentials")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    };
    for expected_remaining in ["1", "0"] {
        let response = send(app.clone(), verify()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let (limit, remaining, _) = ratelimit_headers(&response).unwrap();
        assert_eq!(limit, "2");
        assert_eq!(remaining, expected_remaining);
    }
    let throttled = send(app, verify()).await;
    assert_eq!(throttled.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body_json(throttled).await["error"], "Too many requests");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn disabling_the_toggle_bypasses_all_buckets(pool: PgPool) {
    sqlx::query!(
        "UPDATE instance_settings
         SET rate_limiting_enabled = false, rate_limit_app_registrations = 1"
    )
    .execute(&pool)
    .await
    .unwrap();
    let state = common::test_state_with(pool, std::sync::Arc::default());
    let app = plamenu::build_router(state);
    for _ in 0..3 {
        let response = send(app.clone(), from_ip(apps_request(), "203.0.113.5")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            ratelimit_headers(&response).is_none(),
            "no throttle headers when disabled"
        );
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn forwarded_for_honored_only_from_trusted_proxies(pool: PgPool) {
    sqlx::query!("UPDATE instance_settings SET rate_limit_app_registrations = 1")
        .execute(&pool)
        .await
        .unwrap();
    let state = common::test_state_with(pool, std::sync::Arc::default());
    let app = plamenu::build_router(state);

    // Loopback is a trusted proxy in the test config: the forwarded client
    // is the bucket key.
    let via_proxy = |client: &str| {
        let mut request = from_ip(apps_request(), "127.0.0.1");
        request
            .headers_mut()
            .insert("x-forwarded-for", client.parse().unwrap());
        request
    };
    assert_eq!(
        send(app.clone(), via_proxy("203.0.113.7")).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        send(app.clone(), via_proxy("203.0.113.7")).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        send(app.clone(), via_proxy("203.0.113.8")).await.status(),
        StatusCode::OK
    );

    // From an untrusted peer the header is spoofing and is ignored: the
    // bucket keys on the peer itself.
    let spoofing = |forged: &str| {
        let mut request = from_ip(apps_request(), "203.0.113.9");
        request
            .headers_mut()
            .insert("x-forwarded-for", forged.parse().unwrap());
        request
    };
    assert_eq!(
        send(app.clone(), spoofing("198.51.100.1")).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        send(app, spoofing("198.51.100.2")).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn login_attempts_throttle_per_email(pool: PgPool) {
    sqlx::query!("UPDATE instance_settings SET rate_limit_login_attempts = 2")
        .execute(&pool)
        .await
        .unwrap();
    let state = common::test_state_with(pool.clone(), std::sync::Arc::default());
    let app = plamenu::build_router(state);
    let attempt = || {
        let body =
            serde_urlencoded::to_string([("email", "nobody@example.com"), ("password", "wrong")])
                .unwrap();
        Request::builder()
            .method("POST")
            .uri("/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap()
    };
    // No ConnectInfo on these: only the e-mail bucket applies, proving the
    // in-handler leg works without a resolvable IP.
    assert_eq!(
        send(app.clone(), attempt()).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(app.clone(), attempt()).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let throttled = send(app, attempt()).await;
    assert_eq!(throttled.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body_json(throttled).await["error"], "Too many requests");

    // The e-mail budget is durable: a freshly built app over the
    // same database — a restart — still refuses the address immediately.
    let restarted = test_app(pool);
    let still_throttled = send(restarted, attempt()).await;
    assert_eq!(still_throttled.status(), StatusCode::TOO_MANY_REQUESTS);
}

/// The security-sensitive budgets count in Postgres, so neither a process
/// restart nor a second app instance grants a fresh allowance.
#[sqlx::test(migrations = "../db/migrations")]
async fn security_budgets_survive_a_restart(pool: PgPool) {
    sqlx::query!("UPDATE instance_settings SET rate_limit_app_registrations = 2")
        .execute(&pool)
        .await
        .unwrap();
    let app = test_app(pool.clone());
    for _ in 0..2 {
        let response = send(app.clone(), from_ip(apps_request(), "203.0.113.5")).await;
        assert_eq!(response.status(), StatusCode::OK);
    }
    let throttled = send(app, from_ip(apps_request(), "203.0.113.5")).await;
    assert_eq!(throttled.status(), StatusCode::TOO_MANY_REQUESTS);

    // A fresh router over the same pool is a restart (or a sibling instance):
    // the in-process limiter state is gone, the durable counter is not.
    let restarted = test_app(pool);
    let still_throttled = send(restarted, from_ip(apps_request(), "203.0.113.5")).await;
    assert_eq!(still_throttled.status(), StatusCode::TOO_MANY_REQUESTS);
}

/// When the durable counter store is unreachable, security-sensitive routes
/// are refused (503) rather than admitted unmetered, while load-shedding
/// classes stay fail-open at the gate.
#[sqlx::test(migrations = "../db/migrations")]
async fn durable_buckets_fail_closed_when_the_store_is_unreachable(pool: PgPool) {
    let app = test_app(pool.clone());
    // Warm the settings cache with a healthy request first.
    let warm = send(app.clone(), from_ip(apps_request(), "203.0.113.5")).await;
    assert_eq!(warm.status(), StatusCode::OK);

    pool.close().await;

    // App registration draws from a durable bucket: refused outright.
    let refused = send(app.clone(), from_ip(apps_request(), "203.0.113.5")).await;
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body_json(refused).await["error"],
        "Rate limiter unavailable"
    );

    // A general API read is fail-open at the gate — it may fail later in its
    // handler for want of a database, but never with the limiter's refusal.
    let instance = Request::builder()
        .method("GET")
        .uri("/api/v1/instance")
        .body(Body::empty())
        .unwrap();
    let open = send(app, from_ip(instance, "203.0.113.5")).await;
    assert_ne!(open.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_ne!(open.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn sign_in_logging_records_the_forwarded_client_ip(pool: PgPool) {
    let account = create_local_account(&pool, "alice", "alice").await;
    let hash = hash_password("correct horse battery").unwrap();
    let row = user::create(&pool, account.id, Some("alice@example.com"), &hash)
        .await
        .unwrap();

    let body = serde_urlencoded::to_string([
        ("email", "alice@example.com"),
        ("password", "correct horse battery"),
    ])
    .unwrap();
    let mut request = Request::builder()
        .method("POST")
        .uri("/login")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    request = from_ip(request, "127.0.0.1");
    request
        .headers_mut()
        .insert("x-forwarded-for", "203.0.113.42".parse().unwrap());
    let response = send(test_app(pool.clone()), request).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);

    let recorded =
        sqlx::query_scalar!("SELECT current_sign_in_ip FROM users WHERE id = $1", row.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(recorded.as_deref(), Some("203.0.113.42"));
}
