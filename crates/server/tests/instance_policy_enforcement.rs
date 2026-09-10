//! Instance-policy enforcement: hard federation domain decisions and
//! access-block checks on OAuth login/token paths.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use common::{
    RemoteUser, StubFederation, TEST_DOMAIN, create_local_account, test_app, test_app_with,
    test_state_with,
};
use http_body_util::BodyExt;
use plamenu::auth::hash_password;
use plamenu::{actions, delivery};
use plamenu_db::instance_policy::{self, NewDomainBlock};
use plamenu_db::{PgPool, follow, job, status, user};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use tower::ServiceExt;

const REDIRECT_URI: &str = "urn:ietf:wg:oauth:2.0:oob";

async fn create_user(pool: &PgPool, username: &str, email: &str) {
    let account = create_local_account(pool, username, username).await;
    let hash = hash_password("pw").unwrap();
    user::create(pool, account.id, Some(email), &hash)
        .await
        .unwrap();
}

async fn register_app(pool: &PgPool) -> String {
    let response = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/apps",
        None,
        Some(json!({
            "client_name": "policy-tests",
            "redirect_uris": [REDIRECT_URI],
            "scopes": "read write",
        })),
    )
    .await;
    assert_eq!(response.0, StatusCode::OK, "{:?}", response.1);
    response.1["client_id"].as_str().unwrap().to_owned()
}

async fn post_form(
    app: Router,
    uri: &str,
    fields: &[(&str, &str)],
    remote_addr: Option<SocketAddr>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(serde_urlencoded::to_string(fields).unwrap()))
        .unwrap();
    if let Some(remote_addr) = remote_addr {
        request.extensions_mut().insert(ConnectInfo(remote_addr));
    }
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, value)
}

async fn api(
    app: Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let request = match body {
        Some(value) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&value).unwrap()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

async fn suspend_domain(pool: &PgPool, domain: &str) {
    instance_policy::create_domain_block(
        pool,
        NewDomainBlock {
            domain,
            severity: "suspend",
            reject_media: false,
            reject_reports: false,
            private_comment: None,
            public_comment: None,
            obfuscate: false,
        },
    )
    .await
    .unwrap();
}

fn sha256_hex(value: &str) -> String {
    use std::fmt::Write;
    Sha256::digest(value.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut acc, byte| {
            let _ = write!(acc, "{byte:02x}");
            acc
        })
}

#[sqlx::test(migrations = "../db/migrations")]
async fn suspended_domain_skips_delivery_and_hides_existing_remote_content(pool: PgPool) {
    common::open_previews(&pool).await;
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let remote = plamenu::remote::store_remote_actor(&pool, &bob.actor)
        .await
        .unwrap();
    follow::create(&pool, remote.id, alice.id, None)
        .await
        .unwrap();
    status::upsert_remote(
        &pool,
        status::NewRemoteStatus {
            title: None,
            object_type: None,
            external_url: None,
            uri: "https://remote.example/users/bob/statuses/1",
            account_id: remote.id,
            content: "<p>blocked</p>",
            created_at: OffsetDateTime::now_utc(),
            visibility: "public",
            in_reply_to_id: None,
            in_reply_to_uri: None,
            spoiler_text: "",
            sensitive: false,
            language: None,
            url: None,
            quote_approval_policy: 0,
        },
    )
    .await
    .unwrap();
    suspend_domain(&pool, "remote.example").await;

    let stub: Arc<StubFederation> = Arc::default();
    let state = test_state_with(pool.clone(), stub.clone());
    let (_, enqueued) = actions::post_status(
        &state,
        actions::PostParams {
            username: "alice",
            text: "hello",
            visibility: "public",
            in_reply_to_id: None,
            media_ids: &[],
            quoted_status_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(enqueued, 1);
    assert_eq!(delivery::run_due(&state).await, 1);
    assert!(stub.deliveries().is_empty());
    assert_eq!(job::pending_count(&pool).await.unwrap(), 0);

    let (account_status, _) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/accounts/{}", remote.id),
        None,
        None,
    )
    .await;
    assert_eq!(account_status, StatusCode::NOT_FOUND);

    let (timeline_status, timeline) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/timelines/public",
        None,
        None,
    )
    .await;
    assert_eq!(timeline_status, StatusCode::OK);
    assert!(
        timeline
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["content"] != "<p>blocked</p>"),
        "{timeline}"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn allowlist_denies_unlisted_remote_resolution_without_fetch(pool: PgPool) {
    instance_policy::create_domain_allow(&pool, "friend.example")
        .await
        .unwrap();
    let remote = RemoteUser::new("stranger.example", "bob");
    let stub = StubFederation::with_users(&[&remote]);
    let state = test_state_with(pool.clone(), stub.clone());
    let acct = "bob@stranger.example".parse().unwrap();

    let resolved = plamenu::remote::resolve_remote_account(
        &state,
        &acct,
        plamenu_db::account::ActorClass::PersonLike,
    )
    .await
    .unwrap();

    assert!(resolved.is_none());
    assert!(stub.fetches().is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn oauth_login_rejects_email_canonical_and_no_access_ip_blocks(pool: PgPool) {
    let client_id = register_app(&pool).await;
    create_user(&pool, "blocked", "blocked@spam.example").await;
    instance_policy::create_email_domain_block(&pool, "spam.example", false)
        .await
        .unwrap();
    let (status, body) = post_form(
        test_app(pool.clone()),
        "/oauth/authorize",
        &[
            ("client_id", &client_id),
            ("redirect_uri", REDIRECT_URI),
            ("email", "blocked@spam.example"),
            ("password", "pw"),
        ],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    create_user(&pool, "canonical", "First.Last+tag@Example.COM").await;
    instance_policy::create_canonical_email_block(
        &pool,
        &sha256_hex("firstlast@example.com"),
        None,
    )
    .await
    .unwrap();
    let (status, body) = post_form(
        test_app(pool.clone()),
        "/oauth/authorize",
        &[
            ("client_id", &client_id),
            ("redirect_uri", REDIRECT_URI),
            ("email", "First.Last+tag@Example.COM"),
            ("password", "pw"),
        ],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    create_user(&pool, "ipblocked", "ipblocked@example.net").await;
    instance_policy::create_ip_block(&pool, "203.0.113.0/24", "no_access", "blocked net", None)
        .await
        .unwrap();
    let remote_addr: SocketAddr = "203.0.113.7:1234".parse().unwrap();
    let (status, body) = post_form(
        test_app(pool.clone()),
        "/oauth/authorize",
        &[
            ("client_id", &client_id),
            ("redirect_uri", REDIRECT_URI),
            ("email", "ipblocked@example.net"),
            ("password", "pw"),
        ],
        Some(remote_addr),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
}

/// Posts an activity to the shared inbox, HTTP-signed by `signer`.
async fn post_signed(
    app: Router,
    body: &Value,
    signer: &plamenu_federation::RequestSigner,
) -> StatusCode {
    let bytes = serde_json::to_vec(body).unwrap();
    let signed_headers =
        signer.sign_post(TEST_DOMAIN, "/inbox", &bytes, std::time::SystemTime::now());
    let request = Request::builder()
        .method("POST")
        .uri("/inbox")
        .header("host", signed_headers.host)
        .header("date", signed_headers.date)
        .header("digest", signed_headers.digest)
        .header("signature", signed_headers.signature)
        .header("content-type", "application/activity+json")
        .body(Body::from(bytes))
        .unwrap();
    app.oneshot(request).await.unwrap().status()
}

/// A non-suspending block carrying only the M31 enforcement flags.
async fn limit_domain(pool: &PgPool, domain: &str, reject_media: bool, reject_reports: bool) {
    instance_policy::create_domain_block(
        pool,
        NewDomainBlock {
            domain,
            severity: "noop",
            reject_media,
            reject_reports,
            private_comment: None,
            public_comment: None,
            obfuscate: false,
        },
    )
    .await
    .unwrap();
}

#[sqlx::test(migrations = "../db/migrations")]
async fn reject_reports_drops_inbound_flags(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    limit_domain(&pool, "reporty.example", false, true).await;

    let blocked = RemoteUser::new("reporty.example", "actor");
    let clean = RemoteUser::new("clean.example", "actor");
    let stub = StubFederation::with_actors([blocked.actor.clone(), clean.actor.clone()]);

    let flag = |user: &RemoteUser| {
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("{}/reports/1", user.actor.id),
            "type": "Flag",
            "actor": user.actor.id,
            "content": "spam",
            "object": ["https://plamenu.test/users/alice"],
        })
    };
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &flag(&blocked),
            &blocked.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    let reporter = plamenu_db::account::find_by_uri(&pool, &blocked.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        plamenu_db::report::list_by_reporter(&pool, reporter.id)
            .await
            .unwrap()
            .is_empty(),
        "a reject_reports domain files nothing"
    );

    // The same Flag from a clean domain is filed.
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &flag(&clean),
            &clean.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    let reporter = plamenu_db::account::find_by_uri(&pool, &clean.actor.id)
        .await
        .unwrap()
        .unwrap();
    let filed = plamenu_db::report::list_by_reporter(&pool, reporter.id)
        .await
        .unwrap();
    assert_eq!(filed.len(), 1);
    assert_eq!(filed[0].target_account_id, alice.id);
}

/// A Create(Note) with one image attachment and one custom-emoji tag, authored
/// by `user`.
fn create_with_media(user: &RemoteUser) -> Value {
    let note_uri = format!("{}/statuses/1", user.actor.id);
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{note_uri}/activity"),
        "type": "Create",
        "actor": user.actor.id,
        "object": {
            "id": note_uri,
            "type": "Note",
            "attributedTo": user.actor.id,
            "content": "<p>look :blob: at this</p>",
            "published": "2026-07-01T12:00:00Z",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "cc": [],
            "attachment": [{
                "type": "Document",
                "mediaType": "image/png",
                "url": format!("https://{}/media/1.png", user.actor.id.split('/').nth(2).unwrap()),
            }],
            "tag": [{
                "type": "Emoji",
                "name": ":blob:",
                "icon": {
                    "type": "Image",
                    "url": format!("https://{}/emoji/blob.png", user.actor.id.split('/').nth(2).unwrap()),
                },
            }],
        },
    })
}

#[sqlx::test(migrations = "../db/migrations")]
async fn reject_media_skips_attachment_profile_and_emoji_caching(pool: PgPool) {
    limit_domain(&pool, "flashy.example", true, false).await;

    let mut bob = RemoteUser::new("flashy.example", "bob");
    bob.actor.icon = Some(json!({
        "type": "Image",
        "url": "https://flashy.example/avatar.png",
    }));
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    // The origin would happily serve everything; nothing may be fetched.
    stub.serve_media("https://flashy.example/media/1.png", "image/png", vec![1]);
    stub.serve_media("https://flashy.example/avatar.png", "image/png", vec![1]);
    let state = test_state_with(pool.clone(), stub.clone());

    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &create_with_media(&bob),
            &bob.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );

    // The attachment reference is stored, serving only its origin URL, and no
    // download was or will be queued.
    let stored = status::find_by_uri(&pool, "https://flashy.example/users/bob/statuses/1")
        .await
        .unwrap()
        .expect("the post itself is ingested");
    let media = plamenu_db::media::for_statuses(&pool, &[stored.id])
        .await
        .unwrap()
        .remove(&stored.id)
        .unwrap();
    assert_eq!(media.len(), 1);
    assert_eq!(media[0].processing, "complete");
    assert!(media[0].file_name.is_none());
    assert_eq!(
        media[0].remote_url.as_deref(),
        Some("https://flashy.example/media/1.png")
    );

    // No avatar job either, and the emoji was not registered.
    assert!(
        plamenu_db::account_media::claim_due(&pool, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        plamenu_db::custom_emoji::lookup(&pool, &["blob".to_owned()], Some("flashy.example"))
            .await
            .unwrap()
            .is_empty()
    );

    // The workers find nothing to do and nothing is ever fetched.
    plamenu::media_worker::run_due(&state).await;
    plamenu::media_worker::run_due_account_media(&state).await;
    assert!(stub.media_fetches().is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn late_reject_media_block_abandons_queued_downloads(pool: PgPool) {
    let bob = RemoteUser::new("late.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    stub.serve_media("https://late.example/media/1.png", "image/png", vec![1]);
    let state = test_state_with(pool.clone(), stub.clone());

    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &create_with_media(&bob),
            &bob.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    let stored = status::find_by_uri(&pool, "https://late.example/users/bob/statuses/1")
        .await
        .unwrap()
        .unwrap();
    let media = plamenu_db::media::for_statuses(&pool, &[stored.id])
        .await
        .unwrap()
        .remove(&stored.id)
        .unwrap();
    assert_eq!(media[0].processing, "queued", "download starts out queued");

    // The block lands after the job was queued: the worker abandons the
    // download instead of fetching.
    limit_domain(&pool, "late.example", true, false).await;
    plamenu::media_worker::run_due(&state).await;
    assert!(stub.media_fetches().is_empty());
    let row = &plamenu_db::media::find_by_ids(&pool, &[media[0].id])
        .await
        .unwrap()[0];
    assert_eq!(row.processing, "complete");
    assert!(row.file_name.is_none());
}
