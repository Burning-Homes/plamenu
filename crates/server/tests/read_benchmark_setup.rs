#[path = "../benches/hot_paths/read_warmup.rs"]
mod read_warmup;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{Request, Response, StatusCode};
use axum::routing::get;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn request() -> Request<Body> {
    Request::builder().uri("/").body(Body::empty()).unwrap()
}

#[tokio::test]
async fn bounded_bootstrap_consumes_each_response_and_keeps_observations_separate() {
    let consumed = Arc::new(AtomicUsize::new(0));
    let counter = consumed.clone();
    let app = Router::new().route(
        "/",
        get(move || {
            let counter = counter.clone();
            async move {
                Response::new(Body::from_stream(futures_util::stream::once(async move {
                    counter.fetch_add(1, Ordering::Relaxed);
                    Ok::<_, std::io::Error>(Bytes::from_static(b"authenticated viewer: Alice"))
                })))
            }
        }),
    );
    let evidence = read_warmup::prepare(&app, request, 2, 20, &["Alice"]).await;
    assert_eq!(evidence.pool_connection_limit, 2);
    assert_eq!(evidence.requests, 16);
    assert_eq!(consumed.load(Ordering::Relaxed), 16);
    assert!(evidence.first_request_ms.is_finite() && evidence.last_request_ms.is_finite());
    let serialized = serde_json::to_value(evidence).unwrap();
    assert!(
        serialized.get("median_ms").is_none(),
        "setup is never a timed-case qualification"
    );
}

#[tokio::test]
#[should_panic(expected = "bootstrap response lost required content")]
async fn plausible_anonymous_200_cannot_warm_a_signed_in_case() {
    let app = Router::new().route(
        "/",
        get(|| async { "anonymous landing page of the same size" }),
    );
    read_warmup::prepare(&app, request, 1, 20, &["authenticated viewer: Alice"]).await;
}

#[tokio::test]
#[should_panic(expected = "empty or truncated page")]
async fn empty_200_cannot_enter_timed_phase() {
    let app = Router::new().route("/", get(|| async { "[]" }));
    read_warmup::prepare(&app, request, 1, 20, &[]).await;
}

#[tokio::test]
#[should_panic(expected = "bootstrap request failed")]
async fn http_failure_cannot_enter_timed_phase() {
    let app = Router::new().route(
        "/",
        get(|| async {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "authenticated viewer: Alice",
            )
        }),
    );
    read_warmup::prepare(&app, request, 1, 20, &["Alice"]).await;
}

#[tokio::test]
#[should_panic(expected = "bootstrap response body failed")]
async fn broken_body_must_fail_before_any_timed_request() {
    let app = Router::new().route(
        "/",
        get(|| async {
            Response::new(Body::from_stream(futures_util::stream::once(async {
                Err::<Bytes, std::io::Error>(std::io::Error::other("broken synthetic body"))
            })))
        }),
    );
    read_warmup::prepare(&app, request, 1, 20, &[]).await;
}
