//! CORS policy tests: browser-based clients (Elk, Phanpy, ...) must be able
//! to read discovery and API responses from any origin, mirroring Mastodon's
//! CORS policy.

mod common;

use axum::body::Body;
use axum::http::header::HeaderName;
use axum::http::{Method, Request, Response, StatusCode, header};
use common::test_app;
use plamenu_db::PgPool;
use tower::ServiceExt;

const ORIGIN: &str = "https://elk.zone";

fn header_str<'a>(response: &'a Response<Body>, name: &str) -> Option<&'a str> {
    response
        .headers()
        .get(HeaderName::from_bytes(name.as_bytes()).unwrap())
        .map(|v| v.to_str().unwrap())
}

async fn get_with_origin(pool: PgPool, uri: &str) -> Response<Body> {
    let request = Request::builder()
        .uri(uri)
        .header(header::ORIGIN, ORIGIN)
        .body(Body::empty())
        .unwrap();
    test_app(pool).oneshot(request).await.unwrap()
}

async fn preflight(pool: PgPool, uri: &str, method: &str) -> Response<Body> {
    let request = Request::builder()
        .method(Method::OPTIONS)
        .uri(uri)
        .header(header::ORIGIN, ORIGIN)
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, method)
        .header(
            header::ACCESS_CONTROL_REQUEST_HEADERS,
            "authorization,content-type",
        )
        .body(Body::empty())
        .unwrap();
    test_app(pool).oneshot(request).await.unwrap()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn simple_get_requests_allow_any_origin(pool: PgPool) {
    // Discovery, the Mastodon API and media all answer with `ACAO: *` so
    // browser clients (Elk, Phanpy) can read them cross-origin. The CORS layer
    // wraps the handler, so even a 404 for a missing media file carries the
    // header — exactly the browser's read gate (Phanpy loads avatars with
    // `crossOrigin="anonymous"` to sample them on a canvas). `/api/v1/instance`
    // additionally exposes `link` for pagination.
    //
    // `None` = don't pin the status (the media proxy's failure code for a
    // missing avatar is not part of this contract; only the header is).
    for (uri, expected_status) in [
        ("/.well-known/host-meta", Some(StatusCode::OK)),
        ("/api/v1/instance", Some(StatusCode::OK)),
        (
            "/media/does-not-exist.avatar.png",
            Some(StatusCode::NOT_FOUND),
        ),
        ("/media/proxy/avatar/1", None),
    ] {
        let response = get_with_origin(pool.clone(), uri).await;
        if let Some(status) = expected_status {
            assert_eq!(response.status(), status, "status for {uri}");
        }
        assert_eq!(
            header_str(&response, "access-control-allow-origin"),
            Some("*"),
            "ACAO for {uri}"
        );
        if uri == "/api/v1/instance" {
            let exposed = header_str(&response, "access-control-expose-headers").unwrap();
            assert!(exposed.contains("link"), "exposed headers: {exposed}");
        }
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn preflight_requests_allow_post(pool: PgPool) {
    // Both the app-registration and the token endpoints answer a browser
    // preflight with `ACAO: *` and `POST` among the allowed methods; the
    // app-registration one also allows the `authorization` request header.
    for uri in ["/api/v1/apps", "/oauth/token"] {
        let response = preflight(pool.clone(), uri, "POST").await;
        assert_eq!(response.status(), StatusCode::OK, "status for {uri}");
        assert_eq!(
            header_str(&response, "access-control-allow-origin"),
            Some("*"),
            "ACAO for {uri}"
        );
        let methods = header_str(&response, "access-control-allow-methods").unwrap();
        assert!(
            methods.contains("POST"),
            "allowed methods for {uri}: {methods}"
        );
        if uri == "/api/v1/apps" {
            let headers = header_str(&response, "access-control-allow-headers").unwrap();
            assert!(
                headers.contains("authorization"),
                "allowed headers: {headers}"
            );
        }
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn federation_endpoints_get_no_cors(pool: PgPool) {
    let response = get_with_origin(pool, "/health").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_str(&response, "access-control-allow-origin"), None);
}
