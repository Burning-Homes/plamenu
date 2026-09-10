//! rel="me" profile-link verification: saving a profile enqueues the
//! account, and the worker stamps the field's `verified_at` when the target
//! page links back with `rel="me"`.

mod common;

use std::sync::Arc;

use common::{StubFederation, TEST_DOMAIN, create_local_account, test_state_with};
use plamenu::profile::{ProfileChanges, update_profile};
use plamenu::{AppState, link_verify};
use plamenu_db::account::{FieldPair, RemoteAccountData};
use plamenu_db::{PgPool, account, link_verification};
use serde_json::Value;

const SITE_URL: &str = "https://alice.example";

/// Sets `alice`'s single profile field to `value` and returns the state/stub.
async fn account_with_link(
    pool: &PgPool,
    value: &str,
) -> (AppState, Arc<StubFederation>, account::Account) {
    let stub = Arc::new(StubFederation::default());
    let state = test_state_with(pool.clone(), stub.clone());
    let alice = create_local_account(pool, "alice", "Alice").await;
    let updated = update_profile(
        &state,
        &alice,
        ProfileChanges {
            fields: Some(vec![("Website".into(), value.into())]),
            ..ProfileChanges::default()
        },
    )
    .await
    .unwrap();
    (state, stub, updated)
}

/// The `verified_at` stamp on `alice`'s first profile field, if any.
async fn verified_at(pool: &PgPool, account_id: i64) -> Option<String> {
    let account = account::find_by_id(pool, account_id)
        .await
        .unwrap()
        .unwrap();
    account.fields.as_array().unwrap()[0]
        .get("verified_at")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

#[sqlx::test(migrations = "../db/migrations")]
async fn backlink_gets_verified(pool: PgPool) {
    let (state, stub, alice) = account_with_link(&pool, SITE_URL).await;
    // The linked site links back to alice's `/@handle` URL with rel="me".
    stub.serve_page(
        SITE_URL,
        &format!(
            r#"<html><head><link rel="me" href="https://{TEST_DOMAIN}/@alice"></head><body>hi</body></html>"#
        ),
    );

    // Saving the profile enqueued alice; the worker claims and verifies.
    assert_eq!(link_verification::pending_count(&pool).await.unwrap(), 1);
    assert_eq!(link_verify::run_due(&state).await, 1);

    assert!(verified_at(&pool, alice.id).await.is_some());
    // The URL was actually fetched.
    assert!(
        stub.page_fetches
            .lock()
            .unwrap()
            .contains(&SITE_URL.to_owned())
    );
    // The queue is drained.
    assert_eq!(link_verification::pending_count(&pool).await.unwrap(), 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn anchor_backlink_to_actor_id_verifies(pool: PgPool) {
    let (state, stub, alice) = account_with_link(&pool, SITE_URL).await;
    // An <a rel="me"> pointing at the ActivityPub id also counts.
    stub.serve_page(
        SITE_URL,
        &format!(
            r#"<html><body><a rel="me" href="https://{TEST_DOMAIN}/users/alice">me</a></body></html>"#
        ),
    );
    link_verify::run_due(&state).await;
    assert!(verified_at(&pool, alice.id).await.is_some());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn no_backlink_is_not_verified(pool: PgPool) {
    let (state, stub, alice) = account_with_link(&pool, SITE_URL).await;
    // The page exists but never links back.
    stub.serve_page(SITE_URL, "<html><body>nothing here</body></html>");
    link_verify::run_due(&state).await;
    assert!(verified_at(&pool, alice.id).await.is_none());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn editing_the_link_clears_verification(pool: PgPool) {
    let (state, stub, alice) = account_with_link(&pool, SITE_URL).await;
    stub.serve_page(
        SITE_URL,
        &format!(
            r#"<html><head><link rel="me" href="https://{TEST_DOMAIN}/@alice"></head></html>"#
        ),
    );
    link_verify::run_due(&state).await;
    let verified = account::find_by_id(&pool, alice.id).await.unwrap().unwrap();
    assert!(verified_at(&pool, alice.id).await.is_some());

    // Editing the field's value drops the stamp until re-verified. The new URL
    // is not served, so the re-run cannot re-verify it.
    update_profile(
        &state,
        &verified,
        ProfileChanges {
            fields: Some(vec![("Website".into(), "https://elsewhere.example".into())]),
            ..ProfileChanges::default()
        },
    )
    .await
    .unwrap();
    assert!(verified_at(&pool, alice.id).await.is_none());
    link_verify::run_due(&state).await;
    assert!(verified_at(&pool, alice.id).await.is_none());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn unchanged_link_keeps_verification_across_save(pool: PgPool) {
    let (state, stub, alice) = account_with_link(&pool, SITE_URL).await;
    stub.serve_page(
        SITE_URL,
        &format!(
            r#"<html><head><link rel="me" href="https://{TEST_DOMAIN}/@alice"></head></html>"#
        ),
    );
    link_verify::run_due(&state).await;
    let verified = account::find_by_id(&pool, alice.id).await.unwrap().unwrap();
    assert!(verified_at(&pool, alice.id).await.is_some());

    // Saving an unrelated change (display name) with the same field keeps the
    // stamp — before the worker even re-runs.
    update_profile(
        &state,
        &verified,
        ProfileChanges {
            display_name: Some("Alice Renamed".into()),
            fields: Some(vec![("Website".into(), SITE_URL.into())]),
            ..ProfileChanges::default()
        },
    )
    .await
    .unwrap();
    assert!(verified_at(&pool, alice.id).await.is_some());
}

/// A stored remote account whose profile field carries the sanitized-HTML
/// anchor form remote ingest produces.
async fn remote_with_link(pool: &PgPool, field_value: &str) -> account::Account {
    let uri = "https://remote.example/users/bob";
    account::upsert_remote(
        pool,
        RemoteAccountData {
            username: "bob",
            domain: "remote.example",
            uri,
            display_name: "",
            note: "",
            inbox_url: "https://remote.example/users/bob/inbox",
            shared_inbox_url: "",
            public_key_pem: "pub",
            public_key_id: "https://remote.example/users/bob#main-key",
            avatar_remote_url: None,
            header_remote_url: None,
            avatar_description: "",
            header_description: "",
            created_at: None,
            fields: vec![FieldPair {
                name: "Website".to_owned(),
                value: field_value.to_owned(),
            }],
            featured_collection_url: None,
            locked: false,
            also_known_as: &[],
            moved_to_uri: None,
            url: Some("https://remote.example/@bob"),
            discoverable: false,
            feature_approval_policy: 0,
            is_bot: false,
            indexable: false,
            show_media: None,
            show_media_replies: None,
            show_featured: None,
            memorial: false,
            actor_type: None,
        },
    )
    .await
    .unwrap()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn remote_account_anchor_field_gets_verified(pool: PgPool) {
    let stub = Arc::new(StubFederation::default());
    let state = test_state_with(pool.clone(), stub.clone());
    let bob = remote_with_link(
        &pool,
        &format!(r#"<a href="{SITE_URL}" rel="nofollow">{SITE_URL}</a>"#),
    )
    .await;
    // The site links back to the actor's published web URL.
    stub.serve_page(
        SITE_URL,
        r#"<html><head><link rel="me" href="https://remote.example/@bob"></head></html>"#,
    );
    link_verification::enqueue(&pool, bob.id).await.unwrap();
    assert_eq!(link_verify::run_due(&state).await, 1);
    assert!(verified_at(&pool, bob.id).await.is_some());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn remote_deceptive_anchor_is_not_verified(pool: PgPool) {
    let stub = Arc::new(StubFederation::default());
    let state = test_state_with(pool.clone(), stub.clone());
    // The anchor's text names a different URL than its href: even though the
    // href's page confirms the backlink, the field must not earn a badge.
    let bob = remote_with_link(
        &pool,
        &format!(r#"<a href="{SITE_URL}">https://honest-looking.example</a>"#),
    )
    .await;
    stub.serve_page(
        SITE_URL,
        r#"<html><head><link rel="me" href="https://remote.example/@bob"></head></html>"#,
    );
    link_verification::enqueue(&pool, bob.id).await.unwrap();
    link_verify::run_due(&state).await;
    assert!(verified_at(&pool, bob.id).await.is_none());
    // The candidate URL was never even fetched.
    assert!(stub.page_fetches.lock().unwrap().is_empty());
}
