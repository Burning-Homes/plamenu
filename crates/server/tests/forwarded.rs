//! Forwarded activity copies (M31): a delivery validly HTTP-signed by a
//! server other than the activity's actor. There are no Linked Data
//! Signatures, so nothing in the payload is trusted — a forwarded Create or
//! Update is replaced by fetching the object from its origin, and a forwarded
//! Delete only applies once the origin confirms the object is gone.

mod common;

use std::time::SystemTime;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{RemoteUser, StubFederation, TEST_DOMAIN, create_local_account, test_app_with};
use plamenu_db::{PgPool, favourite, status};
use plamenu_federation::RequestSigner;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn post_signed(app: Router, body: &Value, signer: &RequestSigner) -> StatusCode {
    let bytes = serde_json::to_vec(body).unwrap();
    let signed_headers = signer.sign_post(TEST_DOMAIN, "/inbox", &bytes, SystemTime::now());
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

/// The note `bob` serves at his own host, and its uri.
fn origin_note(bob: &RemoteUser, content: &str) -> (String, Value) {
    let uri = format!("{}/statuses/77", bob.actor.id);
    let note = json!({
        "id": uri,
        "type": "Note",
        "attributedTo": bob.actor.id,
        "content": content,
        "published": "2026-07-01T12:00:00Z",
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "cc": [],
    });
    (uri, note)
}

#[sqlx::test(migrations = "../db/migrations")]
async fn forwarded_create_ingests_the_origin_copy_not_the_payload(pool: PgPool) {
    let bob = RemoteUser::new("origin.example", "bob");
    let fred = RemoteUser::new("relay.example", "fred");
    let stub = StubFederation::with_actors([bob.actor.clone(), fred.actor.clone()]);
    let (note_uri, note) = origin_note(&bob, "<p>the real thing</p>");
    stub.objects.lock().unwrap().insert(note_uri.clone(), note);

    // Fred forwards bob's Create — with tampered content. Only the origin's
    // copy may be stored.
    let mut tampered = json!({
        "id": format!("{note_uri}/activity"),
        "type": "Create",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": note_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>a forgery</p>",
            "published": "2026-07-01T12:00:00Z",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
        },
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &tampered,
            &fred.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    let stored = status::find_by_uri(&pool, &note_uri)
        .await
        .unwrap()
        .expect("the forwarded post was ingested from its origin");
    assert_eq!(stored.content, "<p>the real thing</p>");
    assert!(
        stub.fetches().contains(&note_uri),
        "the origin was consulted: {:?}",
        stub.fetches()
    );

    // A forwarded Update is likewise refreshed from the origin.
    stub.objects.lock().unwrap().insert(
        note_uri.clone(),
        json!({
            "id": note_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>the real edit</p>",
            "published": "2026-07-01T12:00:00Z",
            "updated": "2026-07-02T12:00:00Z",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "cc": [],
        }),
    );
    tampered["type"] = json!("Update");
    tampered["object"]["content"] = json!("<p>a forged edit</p>");
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &tampered,
            &fred.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    let refreshed = status::find_by_uri(&pool, &note_uri)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(refreshed.content, "<p>the real edit</p>");
}

/// A Create forwarded by a subscribed relay (LitePub-style: the relay signs,
/// the origin author stays the actor) counts toward that relay's activity
/// stats; the same delivery signed by a non-relay forwarder counts nothing.
#[sqlx::test(migrations = "../db/migrations")]
async fn forwarded_delivery_counts_toward_the_signing_relay(pool: PgPool) {
    let bob = RemoteUser::new("origin.example", "bob");
    let fred = RemoteUser::new("relay.example", "fred");
    let stub = StubFederation::with_actors([bob.actor.clone(), fred.actor.clone()]);
    let (note_uri, note) = origin_note(&bob, "<p>relayed</p>");
    stub.objects.lock().unwrap().insert(note_uri.clone(), note);

    let relay = plamenu_db::relay::create(&pool, &fred.actor.inbox, Some(&fred.actor.id))
        .await
        .unwrap()
        .unwrap();
    let follow_id = format!("https://{TEST_DOMAIN}/payloads/1");
    plamenu_db::relay::mark_pending(&pool, relay.id, &follow_id)
        .await
        .unwrap();

    let forwarded = json!({
        "id": format!("{note_uri}/activity"),
        "type": "Create",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": note_uri,
    });

    // Not yet accepted: fred is just a forwarder, nothing is counted.
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &forwarded,
            &fred.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert!(
        plamenu_db::relay::activity_totals(&pool)
            .await
            .unwrap()
            .is_empty()
    );

    // Accepted: the forwarded Create is attributed to the relay.
    plamenu_db::relay::resolve_follow_response(&pool, &follow_id, true)
        .await
        .unwrap();
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &forwarded,
            &fred.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert!(
        status::find_by_uri(&pool, &note_uri)
            .await
            .unwrap()
            .is_some()
    );
    let totals = plamenu_db::relay::activity_totals(&pool).await.unwrap();
    assert_eq!(totals.len(), 1);
    let (relay_id, activity) = totals[0];
    assert_eq!(relay_id, relay.id);
    assert_eq!(activity.total, 1);
    assert_eq!(activity.last_week, 1);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn forwarded_delete_needs_the_origin_to_confirm(pool: PgPool) {
    let bob = RemoteUser::new("origin.example", "bob");
    let fred = RemoteUser::new("relay.example", "fred");
    let stub = StubFederation::with_actors([bob.actor.clone(), fred.actor.clone()]);
    let (note_uri, note) = origin_note(&bob, "<p>soon deleted</p>");
    stub.objects
        .lock()
        .unwrap()
        .insert(note_uri.clone(), note.clone());

    // Bob's post arrives directly, signed by bob himself.
    let create = json!({
        "id": format!("{note_uri}/activity"),
        "type": "Create",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": note,
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &create,
            &bob.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert!(
        status::find_by_uri(&pool, &note_uri)
            .await
            .unwrap()
            .is_some()
    );

    // Fred forwards a Delete while the origin still serves the post: ignored.
    let delete = json!({
        "id": format!("{note_uri}#delete"),
        "type": "Delete",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": note_uri,
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &delete,
            &fred.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert!(
        status::find_by_uri(&pool, &note_uri)
            .await
            .unwrap()
            .is_some(),
        "the origin still serves the post; the copy is hearsay"
    );

    // Once the origin answers 404, the same forwarded Delete applies.
    stub.objects.lock().unwrap().remove(&note_uri);
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &delete,
            &fred.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert!(
        status::find_by_uri(&pool, &note_uri)
            .await
            .unwrap()
            .is_none(),
        "the origin confirmed the deletion (reply-less: hard-removed, no stub)"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn forwarded_copies_of_other_activities_are_ignored(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let local_status = status::create_local(
        &pool,
        status::NewLocalStatus::new(alice.id, "<p>hi</p>", "public", None),
    )
    .await
    .unwrap();

    let bob = RemoteUser::new("origin.example", "bob");
    let fred = RemoteUser::new("relay.example", "fred");
    let stub = StubFederation::with_actors([bob.actor.clone(), fred.actor.clone()]);

    // A forwarded Like cannot be confirmed at any origin: dropped.
    let like = json!({
        "id": format!("{}/likes/1", bob.actor.id),
        "type": "Like",
        "actor": bob.actor.id,
        "object": format!("https://plamenu.test/users/alice/statuses/{}", local_status.id),
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &like,
            &fred.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    let bob_row = plamenu_db::account::find_by_uri(&pool, &bob.actor.id)
        .await
        .unwrap();
    if let Some(bob_row) = bob_row {
        let favourited = favourite::favourited_of(&pool, bob_row.id, &[local_status.id])
            .await
            .unwrap();
        assert!(
            !favourited.contains(&local_status.id),
            "a forwarded Like must not favourite"
        );
    }

    // A forwarded Create naming an object on a third host is never fetched.
    let cross_host = json!({
        "id": "https://origin.example/activities/9",
        "type": "Create",
        "actor": bob.actor.id,
        "object": {
            "id": "https://elsewhere.example/statuses/1",
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>x</p>",
        },
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &cross_host,
            &fred.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert!(
        !stub
            .fetches()
            .contains(&"https://elsewhere.example/statuses/1".to_owned()),
        "a server may not speak for another host's objects"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn direct_deliveries_still_verify_the_actor_key(pool: PgPool) {
    // A Create claiming bob as actor but signed by fred, where the object is
    // on bob's host and the origin does NOT serve it: nothing may be stored.
    let bob = RemoteUser::new("origin.example", "bob");
    let fred = RemoteUser::new("relay.example", "fred");
    let stub = StubFederation::with_actors([bob.actor.clone(), fred.actor.clone()]);
    let (note_uri, note) = origin_note(&bob, "<p>never served</p>");
    let create = json!({
        "id": format!("{note_uri}/activity"),
        "type": "Create",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": note,
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &create,
            &fred.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert!(
        status::find_by_uri(&pool, &note_uri)
            .await
            .unwrap()
            .is_none(),
        "the payload alone proves nothing"
    );
}

/// FEP-8b32: a forwarded copy carrying a valid integrity proof bound to the
/// actor's own Ed25519 key is trusted wholesale — no origin fetch, tampered
/// relays impossible (the proof covers the payload).
#[sqlx::test(migrations = "../db/migrations")]
async fn forwarded_create_with_valid_proof_skips_the_origin_fetch(pool: PgPool) {
    let bob = RemoteUser::new("origin.example", "bob").with_ed25519();
    let fred = RemoteUser::new("relay.example", "fred");
    let stub = StubFederation::with_actors([bob.actor.clone(), fred.actor.clone()]);
    // The origin serves *nothing* — a fallback fetch would come back empty.
    let (note_uri, note) = origin_note(&bob, "<p>proof-carried</p>");
    let create = json!({
        "id": format!("{note_uri}/activity"),
        "type": "Create",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": note,
    });
    let proven = bob.proof_signed(&create);

    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &proven,
            &fred.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    let stored = status::find_by_uri(&pool, &note_uri)
        .await
        .unwrap()
        .expect("the proven payload was ingested directly");
    assert_eq!(stored.content, "<p>proof-carried</p>");
    assert!(
        !stub.fetches().contains(&note_uri),
        "the origin must not be consulted for a proven copy: {:?}",
        stub.fetches()
    );
}

/// Mastodon 4.7 parity: a real `mldsa44-jcs-2024` proof resolves the exact
/// FEP-521a verification method, is persisted as ML-DSA-44 material, and can
/// authenticate a forwarded payload without consulting its origin object.
#[sqlx::test(migrations = "../db/migrations")]
async fn forwarded_create_with_valid_ml_dsa_44_proof_skips_origin_fetch(pool: PgPool) {
    use ml_dsa::{Generate, Keypair};
    use time::format_description::well_known::Rfc3339;

    let mut bob = RemoteUser::new("origin.example", "bob");
    let signing = ml_dsa::SigningKey::<ml_dsa::MlDsa44>::generate();
    let encoded = signing.verifying_key().encode();
    let public: [u8; plamenu_ap::multikey::ML_DSA_44_PUBLIC_LEN] =
        encoded.as_slice().try_into().unwrap();
    let public = plamenu_ap::multikey::encode_ml_dsa_44_public(&public);
    let method = format!("{}#mldsa-44", bob.actor.id);
    bob.actor.assertion_method = vec![json!({
        "id": method,
        "type": "Multikey",
        "controller": bob.actor.id,
        "publicKeyMultibase": public,
    })];
    let fred = RemoteUser::new("relay.example", "fred");
    let stub = StubFederation::with_actors([bob.actor.clone(), fred.actor.clone()]);
    let (note_uri, note) = origin_note(&bob, "<p>post-quantum proof</p>");
    let create = json!({
        "@context": [
            "https://www.w3.org/ns/activitystreams",
            "https://w3id.org/security/data-integrity/v2"
        ],
        "id": format!("{note_uri}/activity"),
        "type": "Create",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": note,
    });
    let created = time::OffsetDateTime::now_utc().format(&Rfc3339).unwrap();
    let proven = plamenu_ap::proof::sign_document_ml_dsa_44(
        &create,
        &signing,
        &format!("{}#mldsa-44", bob.actor.id),
        &created,
    )
    .unwrap();

    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &proven,
            &fred.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    let stored = status::find_by_uri(&pool, &note_uri)
        .await
        .unwrap()
        .expect("the ML-DSA-proven payload was ingested directly");
    assert_eq!(stored.content, "<p>post-quantum proof</p>");
    assert!(!stub.fetches().contains(&note_uri));
    let bob_account = plamenu_db::account::find_by_uri(&pool, &bob.actor.id)
        .await
        .unwrap()
        .unwrap();
    let exact = plamenu_db::actor_key::usable_by_uri(&pool, &format!("{}#mldsa-44", bob.actor.id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(exact.account_id, Some(bob_account.id));
    assert_eq!(exact.algorithm, "ml-dsa-44");
}

/// A tampered payload invalidates the proof; the delivery degrades to the
/// origin-confirmation path (which, with the origin serving nothing, stores
/// nothing).
#[sqlx::test(migrations = "../db/migrations")]
async fn forwarded_create_with_broken_proof_falls_back_to_the_origin(pool: PgPool) {
    let bob = RemoteUser::new("origin.example", "bob").with_ed25519();
    let fred = RemoteUser::new("relay.example", "fred");
    let stub = StubFederation::with_actors([bob.actor.clone(), fred.actor.clone()]);
    let (note_uri, note) = origin_note(&bob, "<p>original</p>");
    let create = json!({
        "id": format!("{note_uri}/activity"),
        "type": "Create",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": note,
    });
    let mut proven = bob.proof_signed(&create);
    proven["object"]["content"] = json!("<p>tampered by the relay</p>");

    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &proven,
            &fred.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert!(
        status::find_by_uri(&pool, &note_uri)
            .await
            .unwrap()
            .is_none(),
        "a broken proof must not authenticate the payload"
    );
    assert!(
        stub.fetches().contains(&note_uri),
        "the origin fallback ran: {:?}",
        stub.fetches()
    );
}

/// A proof signed by someone other than the activity's actor (the relay
/// vouching for itself) authenticates nothing.
#[sqlx::test(migrations = "../db/migrations")]
async fn forwarded_proof_by_the_wrong_actor_is_ignored(pool: PgPool) {
    let bob = RemoteUser::new("origin.example", "bob");
    let fred = RemoteUser::new("relay.example", "fred").with_ed25519();
    let stub = StubFederation::with_actors([bob.actor.clone(), fred.actor.clone()]);
    let (note_uri, note) = origin_note(&bob, "<p>vouched by fred</p>");
    let create = json!({
        "id": format!("{note_uri}/activity"),
        "type": "Create",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": note,
    });
    // Fred signs bob's activity with fred's key.
    let proven = fred.proof_signed(&create);

    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &proven,
            &fred.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert!(
        status::find_by_uri(&pool, &note_uri)
            .await
            .unwrap()
            .is_none(),
        "a third-party proof must not authenticate someone else's activity"
    );
}

/// A proven forwarded interaction (a Like, which the origin-confirmation
/// path always ignores) goes through the full direct-delivery dispatch.
#[sqlx::test(migrations = "../db/migrations")]
async fn forwarded_like_with_valid_proof_is_processed(pool: PgPool) {
    let bob = RemoteUser::new("origin.example", "bob").with_ed25519();
    let fred = RemoteUser::new("relay.example", "fred");
    let stub = StubFederation::with_actors([bob.actor.clone(), fred.actor.clone()]);
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let target = plamenu_db::status::create_local(
        &pool,
        plamenu_db::status::NewLocalStatus::new(alice.id, "<p>hello</p>", "public", None),
    )
    .await
    .unwrap();
    let target_uri = format!("https://{TEST_DOMAIN}/users/alice/statuses/{}", target.id);

    let like = json!({
        "id": format!("{}/likes/1", bob.actor.id),
        "type": "Like",
        "actor": bob.actor.id,
        "object": target_uri,
    });
    let proven = bob.proof_signed(&like);
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &proven,
            &fred.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    let bob_account = plamenu_db::account::find_by_uri(&pool, &bob.actor.id)
        .await
        .unwrap()
        .expect("bob was dereferenced for his key");
    let favourited = favourite::favourited_of(&pool, bob_account.id, &[target.id])
        .await
        .unwrap();
    assert!(
        favourited.contains(&target.id),
        "the proven Like was stored, though forwarded"
    );
}
