//! Bounded setup for steady-state read benchmarks. No timed request is skipped.

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use serde::Serialize;
use tower::ServiceExt;

const PASSES_PER_CONNECTION: u32 = 8;

#[derive(Serialize)]
pub struct BootstrapEvidence {
    pub requests: u32,
    pub pool_connection_limit: u32,
    pub first_request_ms: f64,
    pub last_request_ms: f64,
    pub total_setup_ms: f64,
}

/// Give connection-local prepared plans a bounded setup before Criterion.
/// The request count does not prove every connection selected a generic plan.
/// This neither forces a plan nor changes the timed request or pool settings.
/// Every setup response must pass the same shape checks as its read case.
pub async fn prepare(
    router: &Router,
    make_request: impl Fn() -> Request<Body>,
    pool_connection_limit: u32,
    min_len: usize,
    must_contain: &[&str],
) -> BootstrapEvidence {
    assert!(
        pool_connection_limit > 0,
        "benchmark pool must have connections"
    );
    let requests = pool_connection_limit
        .checked_mul(PASSES_PER_CONNECTION)
        .expect("bounded benchmark bootstrap count");
    let began = std::time::Instant::now();
    let mut first_request_ms = 0.0;
    let mut last_request_ms = 0.0;
    for index in 0..requests {
        let start = std::time::Instant::now();
        let response = router.clone().oneshot(make_request()).await.unwrap();
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("bootstrap response body failed")
            .to_bytes();
        assert!(status.is_success(), "bootstrap request failed: {status}");
        assert!(
            body.len() >= min_len,
            "bootstrap returned an empty or truncated page"
        );
        let text = String::from_utf8_lossy(&body);
        for needle in must_contain {
            assert!(
                text.contains(needle),
                "bootstrap response lost required content: {needle}"
            );
        }
        last_request_ms = start.elapsed().as_secs_f64() * 1000.0;
        if index == 0 {
            first_request_ms = last_request_ms;
        }
    }
    BootstrapEvidence {
        requests,
        pool_connection_limit,
        first_request_ms,
        last_request_ms,
        total_setup_ms: began.elapsed().as_secs_f64() * 1000.0,
    }
}
