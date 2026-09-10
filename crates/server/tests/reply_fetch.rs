//! On-demand reply fetching: opening a remote thread crawls the origin's
//! `replies` collection, ingesting replies we never received over federation
//! and recursing through the tree, bounded by the same-host anti-amplification
//! guard. Also covers the thread-open policy behind the `/context` trigger.

mod common;

use std::sync::Arc;

use common::{RemoteUser, StubFederation, create_local_account, test_state_with};
use plamenu::reply_fetch;
use plamenu_db::status::{self, NewLocalStatus, NewRemoteStatus};
use plamenu_db::{PgPool, reply_fetch as reply_fetch_db};
use serde_json::{Value, json};
use time::{Duration, OffsetDateTime};

const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";
const COOLDOWN: Duration = Duration::minutes(15);

/// A public remote Note object for the stub to serve.
fn note(id: &str, author: &str, body: &str, in_reply_to: Option<&str>, replies: &Value) -> Value {
    json!({
        "id": id,
        "type": "Note",
        "attributedTo": author,
        "content": format!("<p>{body}</p>"),
        "to": [PUBLIC],
        "published": "2026-06-25T08:00:00Z",
        "inReplyTo": in_reply_to,
        "replies": replies,
    })
}

#[sqlx::test(migrations = "../db/migrations")]
async fn crawl_ingests_and_recurses_same_host_replies(pool: PgPool) {
    // A remote thread on remote.test: alice's root, bob's reply, bob's
    // reply-to-the-reply (depth 2), plus a cross-host reply on evil.test that
    // the anti-amplification guard must skip.
    let alice = RemoteUser::new("remote.test", "alice");
    let bob = RemoteUser::new("remote.test", "bob");
    let mallory = RemoteUser::new("evil.test", "mallory");
    let stub = StubFederation::with_users(&[&alice, &bob, &mallory]);

    let root_uri = "https://remote.test/users/alice/statuses/1";
    let root_replies = "https://remote.test/users/alice/statuses/1/replies";
    let reply1_uri = "https://remote.test/users/bob/statuses/2";
    let reply2_uri = "https://remote.test/users/bob/statuses/3";
    let evil_uri = "https://evil.test/users/mallory/statuses/9";

    {
        let mut objects = stub.objects.lock().unwrap();
        // Root advertises its replies as a bare collection IRI (fetched path).
        objects.insert(
            root_uri.to_owned(),
            note(
                root_uri,
                &alice.actor.id,
                "root",
                None,
                &json!(root_replies),
            ),
        );
        // The replies collection wraps a first CollectionPage listing a
        // same-host reply and the cross-host one.
        objects.insert(
            root_replies.to_owned(),
            json!({
                "id": root_replies,
                "type": "Collection",
                "first": {
                    "type": "CollectionPage",
                    "partOf": root_replies,
                    "items": [reply1_uri, evil_uri],
                },
            }),
        );
        // reply1 advertises its own replies inline (inlined path), listing reply2.
        objects.insert(
            reply1_uri.to_owned(),
            note(
                reply1_uri,
                &bob.actor.id,
                "reply one",
                Some(root_uri),
                &json!({
                    "id": format!("{reply1_uri}/replies"),
                    "type": "Collection",
                    "first": {"type": "CollectionPage", "items": [reply2_uri]},
                }),
            ),
        );
        objects.insert(
            reply2_uri.to_owned(),
            note(
                reply2_uri,
                &bob.actor.id,
                "reply two",
                Some(reply1_uri),
                &Value::Null,
            ),
        );
        // The evil reply *would* ingest if fetched — so its absence proves the
        // same-host filter, not a missing fixture.
        objects.insert(
            evil_uri.to_owned(),
            note(
                evil_uri,
                &mallory.actor.id,
                "amplify me",
                Some(root_uri),
                &Value::Null,
            ),
        );
    }

    let state = test_state_with(pool.clone(), stub.clone());

    // Seed the root as if a local user had resolved it by URL.
    let root = plamenu::ingest::resolve_or_fetch_status(&state, root_uri)
        .await
        .unwrap()
        .expect("root ingested");

    Box::pin(reply_fetch::crawl_replies(&state, root.id))
        .await
        .unwrap();

    // Same-host replies were fetched and threaded, including the depth-2 one.
    let reply1 = status::find_by_uri(&pool, reply1_uri)
        .await
        .unwrap()
        .expect("reply1 ingested");
    assert_eq!(reply1.in_reply_to_id, Some(root.id));
    let reply2 = status::find_by_uri(&pool, reply2_uri)
        .await
        .unwrap()
        .expect("reply2 ingested via recursion");
    assert_eq!(reply2.in_reply_to_id, Some(reply1.id));

    // The cross-host reply was never fetched.
    assert!(
        status::find_by_uri(&pool, evil_uri)
            .await
            .unwrap()
            .is_none()
    );
    assert!(!stub.fetches().contains(&evil_uri.to_owned()));

    // Both appear as descendants of the root.
    let descendants: Vec<i64> = status::descendants(&pool, root.id)
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert!(descendants.contains(&reply1.id));
    assert!(descendants.contains(&reply2.id));

    // The crawl armed the per-status cooldown.
    assert!(
        !reply_fetch_db::is_due(&pool, root.id, COOLDOWN)
            .await
            .unwrap()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn thread_open_gates_on_remote_distributable_and_cooldown(pool: PgPool) {
    let account = create_local_account(&pool, "carol", "").await;
    let state = test_state_with(pool.clone(), Arc::new(StubFederation::default()));

    let remote_public = status::upsert_remote(
        &pool,
        NewRemoteStatus {
            title: None,
            object_type: None,
            external_url: None,
            uri: "https://remote.test/users/alice/statuses/50",
            account_id: account.id,
            content: "<p>remote</p>",
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

    // Opening the thread enqueues exactly one job, idempotently.
    reply_fetch::on_thread_open(&state, &remote_public)
        .await
        .unwrap();
    reply_fetch::on_thread_open(&state, &remote_public)
        .await
        .unwrap();
    assert_eq!(reply_fetch_db::pending_count(&pool).await.unwrap(), 1);

    // A local status (no AP uri) never enqueues.
    let local = status::create_local(
        &pool,
        NewLocalStatus::new(account.id, "<p>local</p>", "public", None),
    )
    .await
    .unwrap();
    reply_fetch::on_thread_open(&state, &local).await.unwrap();

    // A non-distributable remote status never enqueues.
    let remote_private = status::upsert_remote(
        &pool,
        NewRemoteStatus {
            title: None,
            object_type: None,
            external_url: None,
            uri: "https://remote.test/users/alice/statuses/51",
            account_id: account.id,
            content: "<p>secret</p>",
            created_at: OffsetDateTime::now_utc(),
            visibility: "private",
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
    reply_fetch::on_thread_open(&state, &remote_private)
        .await
        .unwrap();
    assert_eq!(reply_fetch_db::pending_count(&pool).await.unwrap(), 1);

    // Once crawled (the claim leases the job, the worker completes it, and the
    // cooldown is armed), a re-open won't re-enqueue.
    for job in reply_fetch_db::claim_due(&pool, 10).await.unwrap() {
        reply_fetch_db::complete(&pool, job.id).await.unwrap();
    }
    reply_fetch_db::mark_fetched(&pool, remote_public.id)
        .await
        .unwrap();
    reply_fetch::on_thread_open(&state, &remote_public)
        .await
        .unwrap();
    assert_eq!(reply_fetch_db::pending_count(&pool).await.unwrap(), 0);
}

/// A reply that arrived while its parent was unreachable is joined to that
/// parent the moment the parent turns up by any other route — the whole ingest
/// path, not just the DB helper. Without this the thread stayed split forever.
#[sqlx::test(migrations = "../db/migrations")]
async fn late_parent_adopts_the_reply_that_was_waiting(pool: PgPool) {
    let alice = RemoteUser::new("remote.test", "alice");
    let bob = RemoteUser::new("remote.test", "bob");
    let stub = StubFederation::with_users(&[&alice, &bob]);

    let root_uri = "https://remote.test/users/alice/statuses/70";
    let reply_uri = "https://remote.test/users/bob/statuses/71";
    // Only the reply is served: fetching its parent fails, exactly as when the
    // origin 403s our pull or is down.
    stub.objects.lock().unwrap().insert(
        reply_uri.to_owned(),
        note(
            reply_uri,
            &bob.actor.id,
            "answering something you can't see",
            Some(root_uri),
            &Value::Null,
        ),
    );
    let state = test_state_with(pool.clone(), stub.clone());

    let reply = plamenu::ingest::resolve_or_fetch_status(&state, reply_uri)
        .await
        .unwrap()
        .expect("reply ingested even with an unreachable parent");
    assert_eq!(reply.in_reply_to_id, None, "parent could not be fetched");
    assert_eq!(
        status::unresolved_reply_parents(&pool, &[reply.id])
            .await
            .unwrap()
            .len(),
        1,
    );

    // The root shows up later — its author's own delivery, a boost, someone
    // opening the thread.
    stub.objects.lock().unwrap().insert(
        root_uri.to_owned(),
        note(root_uri, &alice.actor.id, "the root", None, &Value::Null),
    );
    let root = plamenu::ingest::resolve_or_fetch_status(&state, root_uri)
        .await
        .unwrap()
        .expect("root ingested");

    // The thread is repaired: reply edge, denormalized parent author, the
    // "unfetched parent" notice, and conversation membership.
    let repaired = status::find_by_id(&pool, reply.id)
        .await
        .unwrap()
        .expect("reply still there");
    assert_eq!(repaired.in_reply_to_id, Some(root.id));
    assert!(
        status::unresolved_reply_parents(&pool, &[reply.id])
            .await
            .unwrap()
            .is_empty()
    );
    let descendants: Vec<i64> = status::descendants(&pool, root.id)
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(descendants, [reply.id]);
    assert_eq!(
        plamenu_db::conversation::of_status(&pool, reply.id)
            .await
            .unwrap(),
        plamenu_db::conversation::of_status(&pool, root.id)
            .await
            .unwrap(),
        "the placeholder conversation the orphan minted is folded into the root's",
    );
}

/// A reply that claimed a wider audience than the thread it answers escaped the
/// scope-widening clamp by arriving while its parent was unfetchable — and
/// delivery order is the sender's to choose. Adoption re-runs the clamp, over the
/// adopted reply *and* everything below it.
#[sqlx::test(migrations = "../db/migrations")]
async fn adoption_reapplies_the_scope_clamp_to_the_subtree(pool: PgPool) {
    let alice = RemoteUser::new("remote.test", "alice");
    let bob = RemoteUser::new("remote.test", "bob");
    let stub = StubFederation::with_users(&[&alice, &bob]);

    let root_uri = "https://remote.test/users/alice/statuses/80";
    let reply_uri = "https://remote.test/users/bob/statuses/81";
    let deeper_uri = "https://remote.test/users/bob/statuses/82";
    {
        let mut objects = stub.objects.lock().unwrap();
        // Bob's reply into a thread it cannot show us, claiming full public.
        objects.insert(
            reply_uri.to_owned(),
            note(
                reply_uri,
                &bob.actor.id,
                "leak it",
                Some(root_uri),
                &Value::Null,
            ),
        );
        // And a public reply under that one, which is fetched normally.
        objects.insert(
            deeper_uri.to_owned(),
            note(
                deeper_uri,
                &bob.actor.id,
                "and again",
                Some(reply_uri),
                &Value::Null,
            ),
        );
    }
    let state = test_state_with(pool.clone(), stub.clone());

    let reply = plamenu::ingest::resolve_or_fetch_status(&state, reply_uri)
        .await
        .unwrap()
        .expect("orphaned reply ingested");
    let deeper = plamenu::ingest::resolve_or_fetch_status(&state, deeper_uri)
        .await
        .unwrap()
        .expect("reply under the orphan ingested");
    // Nothing to measure against yet, so both keep the audience they claimed.
    assert_eq!(reply.visibility, "public");
    assert_eq!(deeper.visibility, "public");

    // The root turns out to be unlisted: this thread was never meant to be
    // listed publicly.
    stub.objects.lock().unwrap().insert(
        root_uri.to_owned(),
        json!({
            "id": root_uri,
            "type": "Note",
            "attributedTo": alice.actor.id,
            "content": "<p>quiet root</p>",
            "to": [format!("{}/followers", alice.actor.id)],
            "cc": [PUBLIC],
            "published": "2026-06-25T08:00:00Z",
        }),
    );
    let root = plamenu::ingest::resolve_or_fetch_status(&state, root_uri)
        .await
        .unwrap()
        .expect("root ingested");
    assert_eq!(root.visibility, "unlisted");

    for (id, label) in [(reply.id, "adopted reply"), (deeper.id, "its own reply")] {
        let stored = status::find_by_id(&pool, id)
            .await
            .unwrap()
            .expect("still stored");
        assert_eq!(
            stored.visibility, "unlisted",
            "{label} clamped to the conversation's audience"
        );
    }
    // The root keeps its own audience; the clamp only ever narrows.
    assert_eq!(
        status::find_by_id(&pool, root.id)
            .await
            .unwrap()
            .unwrap()
            .visibility,
        "unlisted"
    );
}
