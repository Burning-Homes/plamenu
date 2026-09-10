//! Webhook delivery: triggers serialize the event payload and enqueue it for
//! every enabled webhook subscribed to the event; the worker renders the
//! optional template, signs the body into `X-Hub-Signature` and POSTs it,
//! with Mastodon's retry/drop semantics.

mod common;

use std::sync::Arc;

use common::{StubFederation, create_local_account, test_state_with};
use plamenu::{actions, webhooks};
use plamenu_db::{PgPool, webhook};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Independent HMAC-SHA256 to verify the `X-Hub-Signature` header against.
fn hmac_sha256_hex(key: &[u8], message: &[u8]) -> String {
    let mut key_block = [0u8; 64];
    if key.len() > 64 {
        key_block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(key_block.map(|b| b ^ 0x36));
    inner.update(message);
    let mut outer = Sha256::new();
    outer.update(key_block.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().iter().fold(String::new(), |mut out, b| {
        use std::fmt::Write as _;
        write!(out, "{b:02x}").expect("writing to string cannot fail");
        out
    })
}

async fn add_webhook(
    pool: &PgPool,
    url: &str,
    events: &[&str],
    template: Option<&str>,
) -> webhook::Webhook {
    let events: Vec<String> = events.iter().map(|&e| e.to_owned()).collect();
    webhook::create(
        pool,
        webhook::NewWebhook {
            url,
            events: &events,
            secret: "s3cret",
            template,
        },
    )
    .await
    .unwrap()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn status_created_delivers_signed_payload(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let hook = add_webhook(
        &pool,
        "https://consumer.example/hook",
        &[webhook::STATUS_CREATED],
        None,
    )
    .await;
    // A disabled webhook and one subscribed to another event get nothing.
    let disabled = add_webhook(
        &pool,
        "https://consumer.example/disabled",
        &[webhook::STATUS_CREATED],
        None,
    )
    .await;
    webhook::set_enabled(&pool, disabled.id, false)
        .await
        .unwrap();
    add_webhook(
        &pool,
        "https://consumer.example/reports",
        &[webhook::REPORT_CREATED],
        None,
    )
    .await;

    let stub: Arc<StubFederation> = Arc::default();
    let state = test_state_with(pool.clone(), stub.clone());
    actions::post_status(
        &state,
        actions::PostParams {
            username: "alice",
            text: "hello webhooks",
            visibility: "public",
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(webhook::pending_deliveries(&pool).await.unwrap(), 1);
    assert_eq!(webhooks::run_due(&state).await, 1);
    assert_eq!(webhook::pending_deliveries(&pool).await.unwrap(), 0);

    let posts = stub.webhook_posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].url, "https://consumer.example/hook");

    let payload: Value = serde_json::from_str(&posts[0].body).unwrap();
    assert_eq!(payload["event"], "status.created");
    assert!(payload["created_at"].as_str().is_some());
    assert!(
        payload["object"]["content"]
            .as_str()
            .unwrap()
            .contains("hello webhooks")
    );
    assert_eq!(payload["object"]["account"]["username"], "alice");

    let expected = format!(
        "sha256={}",
        hmac_sha256_hex(hook.secret.as_bytes(), posts[0].body.as_bytes())
    );
    assert_eq!(
        posts[0].headers,
        vec![("X-Hub-Signature".to_owned(), expected)]
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn report_created_renders_template_and_failures_follow_mastodon(pool: PgPool) {
    let reporter = create_local_account(&pool, "alice", "Alice").await;
    let target = create_local_account(&pool, "bob", "Bob").await;
    add_webhook(
        &pool,
        "https://consumer.example/hook",
        &[webhook::REPORT_CREATED],
        Some(r#"{"note":"{{object.comment}} about {{object.target_account.username}}"}"#),
    )
    .await;

    let stub: Arc<StubFederation> = Arc::default();
    let state = test_state_with(pool.clone(), stub.clone());
    let file_report = || {
        actions::create_report(
            &state,
            &reporter,
            &target,
            actions::ReportParams {
                comment: "spam",
                category: None,
                forward: false,
                status_ids: &[],
                rule_ids: None,
            },
        )
    };

    file_report().await.unwrap();
    assert_eq!(webhooks::run_due(&state).await, 1);
    let posts = stub.webhook_posts();
    assert_eq!(posts[0].body, r#"{"note":"spam about bob"}"#);

    // A transient failure (5xx) leaves the job queued for a retry...
    stub.set_webhook_status(500);
    file_report().await.unwrap();
    assert_eq!(webhooks::run_due(&state).await, 1);
    assert_eq!(webhook::pending_deliveries(&pool).await.unwrap(), 1);

    // ...an unsalvageable rejection (422) drops it, like Mastodon.
    stub.set_webhook_status(422);
    webhook::make_all_deliveries_due(&pool).await.unwrap();
    assert_eq!(webhooks::run_due(&state).await, 1);
    assert_eq!(webhook::pending_deliveries(&pool).await.unwrap(), 0);
}
