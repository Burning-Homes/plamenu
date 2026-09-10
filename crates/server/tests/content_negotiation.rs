//! Content-negotiation audit: the exact media types Plamenu serves and
//! accepts are an interop contract with strict peers (Mastodon rejects
//! non-AP content types on fetch, Mitra requests the profiled `ld+json`
//! form). These tests pin the served `Content-Type` strings and the Accept
//! forms that must select the `ActivityPub` representation.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{create_local_account, test_app};
use http_body_util::BodyExt;
use plamenu_db::PgPool;
use serde_json::Value;
use tower::ServiceExt;

const AP_CONTENT_TYPE: &str = "application/activity+json; charset=utf-8";
const LD_JSON_PROFILED: &str =
    "application/ld+json; profile=\"https://www.w3.org/ns/activitystreams\"";

async fn get(app: Router, path: &str, accept: &str) -> (StatusCode, String, Value) {
    let request = Request::builder()
        .method("GET")
        .uri(path)
        .header(header::ACCEPT, accept)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, content_type, body)
}

async fn create_public_status(pool: &PgPool) -> i64 {
    let alice = plamenu_db::account::find_local_by_username(pool, "alice")
        .await
        .unwrap()
        .unwrap();
    plamenu_db::status::create_local(
        pool,
        plamenu_db::status::NewLocalStatus::new(alice.id, "<p>hello</p>", "public", None),
    )
    .await
    .unwrap()
    .id
}

#[sqlx::test(migrations = "../db/migrations")]
async fn ap_documents_are_served_with_the_exact_activitypub_content_type(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let status_id = create_public_status(&pool).await;
    let status_path = format!("/users/alice/statuses/{status_id}");

    for path in [
        "/users/alice",
        "/users/alice/outbox",
        "/users/alice/followers",
        "/users/alice/following",
        "/users/alice/collections/featured",
        status_path.as_str(),
        "/actor",
        "/actor/outbox",
    ] {
        let (status, content_type, _) =
            get(test_app(pool.clone()), path, "application/activity+json").await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(content_type, AP_CONTENT_TYPE, "{path}");
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn profiled_ld_json_accept_selects_the_ap_representation(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let status_id = create_public_status(&pool).await;
    let status_path = format!("/users/alice/statuses/{status_id}");

    // Every Accept form a strict peer sends must reach the AP document, on
    // both content-negotiated endpoints (actor and status).
    for accept in [
        "application/activity+json",
        LD_JSON_PROFILED,
        "application/ld+json",
        "application/json",
        "application/activity+json, text/html;q=0.1",
    ] {
        for path in ["/users/alice", status_path.as_str()] {
            let (status, content_type, body) = get(test_app(pool.clone()), path, accept).await;
            assert_eq!(status, StatusCode::OK, "{path} with {accept}");
            assert_eq!(content_type, AP_CONTENT_TYPE, "{path} with {accept}");
            assert!(body["id"].is_string(), "{path} with {accept}");
        }
    }

    // A browser Accept gets HTML from the same URLs.
    for path in ["/users/alice", status_path.as_str()] {
        let (status, content_type, _) = get(
            test_app(pool.clone()),
            path,
            "text/html, application/xhtml+xml",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(
            content_type.starts_with("text/html"),
            "{path}: {content_type}"
        );
    }

    // Federation-only collections hard-require an AP Accept.
    let (status, _, _) = get(test_app(pool), "/users/alice/outbox", "text/html").await;
    assert_eq!(status, StatusCode::NOT_ACCEPTABLE);
}

async fn get_with_cache_control(app: Router, path: &str) -> (String, Value) {
    let request = Request::builder()
        .method("GET")
        .uri(path)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{path}");
    let cache_control = response
        .headers()
        .get(header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (cache_control, serde_json::from_slice(&bytes).unwrap())
}

#[sqlx::test(migrations = "../db/migrations")]
async fn nodeinfo_counts_are_live_and_responses_are_cacheable(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    create_public_status(&pool).await;
    create_public_status(&pool).await;

    // Mastodon's cache windows: 3 days for discovery, 30 minutes for docs.
    let (cache, _) = get_with_cache_control(test_app(pool.clone()), "/.well-known/nodeinfo").await;
    assert_eq!(cache, "public, max-age=259200");
    let (cache, doc) = get_with_cache_control(test_app(pool.clone()), "/nodeinfo/2.1").await;
    assert_eq!(cache, "public, max-age=1800");
    assert_eq!(doc["usage"]["localPosts"], 2);
    assert_eq!(doc["usage"]["users"]["total"], 1);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn instance_stats_count_local_posts_and_known_domains(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    create_public_status(&pool).await;
    let (status, _, body) = get(test_app(pool), "/api/v1/instance", "application/json").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["stats"]["user_count"], 1);
    assert_eq!(body["stats"]["status_count"], 1);
    assert_eq!(body["stats"]["domain_count"], 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn webfinger_serves_jrd_with_the_full_mastodon_link_set(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let (status, content_type, jrd) = get(
        test_app(pool),
        "/.well-known/webfinger?resource=acct:alice@plamenu.test",
        "application/jrd+json",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type, "application/jrd+json; charset=utf-8");
    assert_eq!(jrd["subject"], "acct:alice@plamenu.test");
    assert_eq!(jrd["aliases"][0], "https://plamenu.test/@alice");
    assert_eq!(jrd["aliases"][1], "https://plamenu.test/users/alice");

    let links = jrd["links"].as_array().unwrap();
    let link = |rel: &str| {
        links
            .iter()
            .find(|l| l["rel"] == rel)
            .unwrap_or_else(|| panic!("missing link {rel}"))
    };
    let self_link = link("self");
    assert_eq!(self_link["type"], "application/activity+json");
    assert_eq!(self_link["href"], "https://plamenu.test/users/alice");
    let profile = link("http://webfinger.net/rel/profile-page");
    assert_eq!(profile["type"], "text/html");
    assert_eq!(profile["href"], "https://plamenu.test/@alice");
    // Remote-follow buttons on Mastodon/Pleroma resolve this template.
    assert_eq!(
        link("http://ostatus.org/schema/1.0/subscribe")["template"],
        "https://plamenu.test/interact?uri={uri}"
    );
    assert_eq!(
        link("https://w3id.org/fep/3b86/Create")["template"],
        "https://plamenu.test/compose?text={content}"
    );
    assert_eq!(
        link("https://w3id.org/fep/3b86/Object")["template"],
        "https://plamenu.test/search?q={object}"
    );
}
