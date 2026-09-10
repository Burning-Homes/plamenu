//! Transactional-outbox atomicity: a status creation or a
//! relationship mutation and the delivery jobs it produces commit as one unit.
//! A failure while enqueueing a delivery must roll the whole domain write back —
//! never leave a half-assembled status, a post queued to only part of its
//! audience, or a committed post/edge with no delivery that a client retry would
//! duplicate.

mod common;

use std::sync::Arc;

use common::{RemoteUser, create_local_account, test_state_with};
use plamenu::actions;
use plamenu_db::{PgPool, account, follow, job};

/// A delivery inbox the injected trigger rejects, so the `INSERT` into
/// `delivery_jobs` fails mid-transaction.
const SENTINEL_INBOX: &str = "https://fail.invalid/inbox";

/// Installs a `BEFORE INSERT` trigger that aborts any `delivery_jobs` insert
/// aimed at [`SENTINEL_INBOX`] — a controllable mid-transaction failure at the
/// outbox boundary. Each `sqlx::test` runs against its own database, so the
/// trigger is isolated to the one test.
async fn install_delivery_failure_trigger(pool: &PgPool) {
    sqlx::query(
        "CREATE OR REPLACE FUNCTION qc18_fail_sentinel() RETURNS trigger AS $$
         BEGIN
           IF NEW.inbox_url = 'https://fail.invalid/inbox' THEN
             RAISE EXCEPTION 'qc18 injected delivery failure';
           END IF;
           RETURN NEW;
         END;
         $$ LANGUAGE plpgsql;",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER qc18_fail_sentinel_trigger BEFORE INSERT ON delivery_jobs
         FOR EACH ROW EXECUTE FUNCTION qc18_fail_sentinel();",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn drop_delivery_failure_trigger(pool: &PgPool) {
    sqlx::query("DROP TRIGGER IF EXISTS qc18_fail_sentinel_trigger ON delivery_jobs;")
        .execute(pool)
        .await
        .unwrap();
}

/// Points a stored remote account's delivery inbox at `inbox` — both the shared
/// inbox (which the follower fan-out prefers) and the per-actor inbox (which a
/// direct `Follow` addresses).
async fn set_inbox(pool: &PgPool, account_id: i64, inbox: &str) {
    sqlx::query("UPDATE accounts SET inbox_url = $1, shared_inbox_url = $1 WHERE id = $2")
        .bind(inbox)
        .bind(account_id)
        .execute(pool)
        .await
        .unwrap();
}

async fn status_count(pool: &PgPool, account_id: i64) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses WHERE account_id = $1")
        .bind(account_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn status_tag_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM status_tags")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Stores a remote follower of alice and returns its account.
async fn add_remote_follower(pool: &PgPool, user: &RemoteUser) -> account::Account {
    let remote = plamenu::remote::store_remote_actor(pool, &user.actor)
        .await
        .unwrap();
    let alice = account::find_local_by_username(pool, "alice")
        .await
        .unwrap()
        .unwrap();
    follow::create(pool, remote.id, alice.id, None)
        .await
        .unwrap();
    remote
}

fn public_post(text: &str) -> actions::PostParams<'_> {
    actions::PostParams {
        username: "alice",
        text,
        visibility: "public",
        ..Default::default()
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn status_and_its_outbox_commit_together(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    add_remote_follower(&pool, &bob).await;

    let state = test_state_with(pool.clone(), Arc::default());
    let (_status, deliveries) = actions::post_status(&state, public_post("hello world"))
        .await
        .unwrap();

    // The post exists AND its single delivery job exists together — the
    // transactional-outbox guarantee (nothing is queued to only part of the
    // audience, and no post commits with an empty outbox).
    assert_eq!(deliveries, 1);
    assert_eq!(status_count(&pool, alice.id).await, 1);
    assert_eq!(job::pending_count(&pool).await.unwrap(), 1);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn a_failed_delivery_enqueue_rolls_back_the_whole_post(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let bob_account = add_remote_follower(&pool, &bob).await;
    // Bob's inbox is the sentinel the trigger rejects: alice's public fan-out
    // enqueue will fail *after* the status row and its child rows are written in
    // the same transaction.
    set_inbox(&pool, bob_account.id, SENTINEL_INBOX).await;
    install_delivery_failure_trigger(&pool).await;

    let state = test_state_with(pool.clone(), Arc::default());
    let result = actions::post_status(&state, public_post("hello #atomic")).await;
    assert!(
        result.is_err(),
        "a failed fan-out enqueue must fail the whole post"
    );

    // The entire post rolled back: no status row, no hashtag child rows, no
    // delivery jobs — not the half-assembled status the pre-#18 code would have
    // left behind after committing the status but failing the fan-out.
    assert_eq!(
        status_count(&pool, alice.id).await,
        0,
        "the status row rolled back with its failed outbox"
    );
    assert_eq!(
        status_tag_count(&pool).await,
        0,
        "the hashtag child rows rolled back too"
    );
    assert_eq!(
        job::pending_count(&pool).await.unwrap(),
        0,
        "no orphaned delivery jobs survive the rollback"
    );

    // Recovery: with the fault cleared, retrying commits exactly one post — the
    // failed attempt left nothing behind to duplicate.
    drop_delivery_failure_trigger(&pool).await;
    set_inbox(&pool, bob_account.id, "https://remote.example/inbox").await;
    actions::post_status(&state, public_post("hello #atomic"))
        .await
        .unwrap();
    assert_eq!(
        status_count(&pool, alice.id).await,
        1,
        "exactly one post after recovery — no duplicate from the failed attempt"
    );
    assert_eq!(job::pending_count(&pool).await.unwrap(), 1);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn a_remote_follow_and_its_follow_job_commit_together(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let bob_account = plamenu::remote::store_remote_actor(&pool, &bob.actor)
        .await
        .unwrap();

    let state = test_state_with(pool.clone(), Arc::default());
    actions::follow_account(&state, &alice, &bob_account)
        .await
        .unwrap();

    // The outgoing follow edge and its single `Follow` delivery exist together.
    assert!(
        follow::find(&pool, alice.id, bob_account.id)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(job::pending_count(&pool).await.unwrap(), 1);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn a_failed_follow_enqueue_rolls_back_the_follow_edge(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stored = plamenu::remote::store_remote_actor(&pool, &bob.actor)
        .await
        .unwrap();
    set_inbox(&pool, stored.id, SENTINEL_INBOX).await;
    install_delivery_failure_trigger(&pool).await;
    // Reload so the target carries the sentinel inbox the follow will address.
    let bob_account = account::find_by_id(&pool, stored.id)
        .await
        .unwrap()
        .unwrap();

    let state = test_state_with(pool.clone(), Arc::default());
    let result = actions::follow_account(&state, &alice, &bob_account).await;
    assert!(
        result.is_err(),
        "a failed Follow enqueue must fail the whole follow"
    );

    // The pending follow edge rolled back with its failed outbox — no divergence
    // between local state and what the remote was told.
    assert!(
        follow::find(&pool, alice.id, bob_account.id)
            .await
            .unwrap()
            .is_none(),
        "the follow edge rolled back"
    );
    assert_eq!(
        job::pending_count(&pool).await.unwrap(),
        0,
        "no orphaned Follow job survives the rollback"
    );
}
