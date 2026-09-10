//! Per-account Atom feed (`/@name.atom`) and its profile-page
//! autodiscovery link.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{StubFederation, create_local_account, test_app, test_state_with};
use http_body_util::BodyExt;
use plamenu::actions::{PostParams, post_status};
use plamenu_db::PgPool;
use tower::ServiceExt;

async fn get_raw(app: Router, uri: &str) -> (StatusCode, String, String) {
    let request = Request::builder()
        .method("GET")
        .uri(uri)
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
    (
        status,
        content_type,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

async fn post_public(pool: &PgPool, username: &str, text: &str) {
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    post_status(
        &state,
        PostParams {
            username,
            text,
            visibility: "public",
            ..Default::default()
        },
    )
    .await
    .unwrap();
}

#[sqlx::test(migrations = "../db/migrations")]
async fn atom_feed_lists_the_accounts_public_posts(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice Example").await;
    post_public(&pool, "alice", "hello from the feed").await;

    let (status, content_type, body) = get_raw(test_app(pool.clone()), "/@alice.atom").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        content_type.starts_with("application/atom+xml"),
        "{content_type}"
    );
    assert!(
        body.contains("<feed xmlns=\"http://www.w3.org/2005/Atom\">"),
        "{body}"
    );
    assert!(body.contains("<title>Alice Example</title>"), "{body}");
    // Exactly one entry, carrying the post's escaped HTML content.
    assert_eq!(body.matches("<entry>").count(), 1, "{body}");
    assert!(body.contains("<content type=\"html\">"), "{body}");
    assert!(body.contains("hello from the feed"), "{body}");
    // The rendered HTML is embedded escaped, so the document stays well-formed
    // (the wrapping `<p>` becomes `&lt;p&gt;`, never a raw child element).
    assert!(body.contains("&lt;p&gt;"), "{body}");
    assert!(
        !body.contains("<p>"),
        "raw HTML leaked into the feed: {body}"
    );

    // An unknown account's feed is a 404.
    let (status, _, _) = get_raw(test_app(pool.clone()), "/@nobody.atom").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn profile_page_advertises_the_feed(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;

    let (status, content_type, body) = get_raw(test_app(pool.clone()), "/@alice").await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.starts_with("text/html"), "{content_type}");
    assert!(
        body.contains(r#"<link rel="alternate" type="application/atom+xml""#),
        "autodiscovery link missing"
    );
    assert!(body.contains("/@alice.atom"), "feed href missing");
}
