//! Remote actor URI/handle binding and rename security.

mod common;

use common::{RemoteUser, StubFederation, test_config, test_state_with};
use plamenu::remote::{refresh_remote_actor, store_remote_actor_from_resolution};
use plamenu_db::{PgPool, account};
use plamenu_federation::{ResolvedAcct, WebfingerCandidate};

fn resolution(acct: &str, actors: &[(&str, Option<&str>)]) -> ResolvedAcct {
    let acct = acct.parse().unwrap();
    let candidates: Vec<_> = actors
        .iter()
        .map(|(uri, kind)| WebfingerCandidate {
            actor_uri: (*uri).to_owned(),
            advertised_type: kind.map(str::to_owned),
        })
        .collect();
    ResolvedAcct {
        acct,
        actor_uri: candidates[0].actor_uri.clone(),
        candidates,
        subscribe_template: None,
        hls_stream_url: None,
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn handle_change_requires_loopback_and_preserves_profile_refresh(pool: PgPool) {
    let mut alice = RemoteUser::new("old.example", "alice");
    alice.actor.webfinger = Some("alice@old.example".into());
    let federation = StubFederation::with_users(&[&alice]);
    let state = test_state_with(pool.clone(), federation.clone());

    let original = refresh_remote_actor(&state, &alice.actor).await.unwrap();
    assert_eq!(original.username, "alice");
    assert_eq!(original.domain.as_deref(), Some("old.example"));
    assert!(
        !account::webfinger_is_stale(&pool, original.id)
            .await
            .unwrap()
    );

    let mut claim = alice.actor.clone();
    claim.webfinger = Some("alice@new.example".into());
    claim.preferred_username = "alice".into();
    claim.name = Some("profile updated while WebFinger is offline".into());

    // Offline/unavailable WebFinger: non-identity profile data refreshes, but
    // the last verified handle and its timestamp do not move.
    let offline = refresh_remote_actor(&state, &claim).await.unwrap();
    assert_eq!(offline.id, original.id);
    assert_eq!(offline.domain.as_deref(), Some("old.example"));
    assert_eq!(
        offline.display_name,
        "profile updated while WebFinger is offline"
    );

    // A live WebFinger document naming somebody else's actor is a takeover,
    // not a rename.
    federation.webfinger.lock().unwrap().insert(
        "alice@new.example".into(),
        vec![WebfingerCandidate {
            actor_uri: "https://new.example/users/mallory".into(),
            advertised_type: Some("Person".into()),
        }],
    );
    let takeover = refresh_remote_actor(&state, &claim).await.unwrap();
    assert_eq!(takeover.domain.as_deref(), Some("old.example"));

    // Once the claimed acct loops back to the exact stable actor URI the
    // rename is accepted without changing row identity or relationships.
    federation.webfinger.lock().unwrap().insert(
        "alice@new.example".into(),
        vec![WebfingerCandidate {
            actor_uri: claim.id.clone(),
            advertised_type: Some("Person".into()),
        }],
    );
    let renamed = refresh_remote_actor(&state, &claim).await.unwrap();
    assert_eq!(renamed.id, original.id);
    assert_eq!(renamed.username, "alice");
    assert_eq!(renamed.domain.as_deref(), Some("new.example"));
    assert!(
        !account::webfinger_is_stale(&pool, renamed.id)
            .await
            .unwrap()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn reused_and_shared_handle_never_merges_actor_uris(pool: PgPool) {
    let first = RemoteUser::new("shared.example", "sam");
    let mut second = RemoteUser::new("shared.example", "other");
    second.actor.id = "https://shared.example/groups/sam".into();
    second.actor.kind = "Group".into();
    second.actor.preferred_username = "sam".into();
    second.actor.webfinger = Some("sam@shared.example".into());
    second.actor.public_key.owner = second.actor.id.clone();
    second.actor.public_key.id = format!("{}#main-key", second.actor.id);

    let federation = StubFederation::default();
    let federation = std::sync::Arc::new(federation);
    let state = test_state_with(pool.clone(), federation);
    let resolved = resolution(
        "sam@shared.example",
        &[
            (&first.actor.id, Some("Person")),
            (&second.actor.id, Some("Group")),
        ],
    );
    let a = store_remote_actor_from_resolution(&state, &first.actor, &resolved)
        .await
        .unwrap();
    let b = store_remote_actor_from_resolution(&state, &second.actor, &resolved)
        .await
        .unwrap();

    assert_ne!(a.id, b.id);
    assert_eq!(a.username, b.username);
    assert_eq!(a.domain, b.domain);
    assert_eq!(
        account::find_by_uri(&pool, &first.actor.id)
            .await
            .unwrap()
            .unwrap()
            .id,
        a.id
    );
    assert_eq!(
        account::find_by_uri(&pool, &second.actor.id)
            .await
            .unwrap()
            .unwrap()
            .id,
        b.id
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn external_fep_521a_method_is_guarded_and_exactly_controller_bound(pool: PgPool) {
    let pair = plamenu_ap::keys::generate_ed25519_keypair();
    let mut alice = RemoteUser::new("remote.example", "alice");
    let method = "https://keys.remote.example/alice/ed25519";
    alice.actor.assertion_method = vec![serde_json::Value::String(method.into())];
    let federation = StubFederation::with_actors([alice.actor.clone()]);
    federation.objects.lock().unwrap().insert(
        method.into(),
        serde_json::json!({
            "id": method,
            "type": "Multikey",
            "controller": alice.actor.id,
            "publicKeyMultibase": pair.public_multibase,
            "expires": "2030-01-01T00:00:00Z",
        }),
    );
    let state = test_state_with(pool.clone(), federation.clone());
    let stored = refresh_remote_actor(&state, &alice.actor).await.unwrap();
    let key = plamenu_db::actor_key::usable_by_uri(&pool, method)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(key.account_id, Some(stored.id));
    assert_eq!(key.controller_uri, alice.actor.id);
    assert_eq!(key.algorithm, "ed25519");
    assert_eq!(key.source, "multikey-external");
    assert!(federation.fetches().contains(&method.to_owned()));

    let mut mallory = RemoteUser::new("evil.example", "mallory");
    let bad_method = "https://keys.evil.example/mallory/key";
    mallory.actor.assertion_method = vec![serde_json::Value::String(bad_method.into())];
    federation.objects.lock().unwrap().insert(
        bad_method.into(),
        serde_json::json!({
            "id": bad_method,
            "type": "Multikey",
            "controller": alice.actor.id,
            "publicKeyMultibase": pair.public_multibase,
        }),
    );
    assert!(refresh_remote_actor(&state, &mallory.actor).await.is_err());
    assert!(
        account::find_by_uri(&pool, &mallory.actor.id)
            .await
            .unwrap()
            .is_none(),
        "key validation completes before actor state is mutated"
    );

    let mut unsafe_actor = RemoteUser::new("unsafe.example", "unsafe");
    unsafe_actor.actor.assertion_method = vec![serde_json::Value::String(
        "http://127.0.0.1/private-key".into(),
    )];
    let mut strict_config = test_config();
    strict_config.allow_private_fetch = false;
    let strict_state = plamenu::AppState::new(
        pool.clone(),
        strict_config,
        federation.clone(),
        std::sync::Arc::new(plamenu::storage::MemoryStore::default()),
    )
    .unwrap();
    assert!(
        refresh_remote_actor(&strict_state, &unsafe_actor.actor)
            .await
            .is_err()
    );
    // The production fetcher performs DNS/IP/redirect SSRF enforcement; the
    // in-memory stub deliberately records the attempted URL and answers 404.
    // Either way, no actor/key mutation is permitted on fetch failure.
    assert!(
        account::find_by_uri(&pool, &unsafe_actor.actor.id)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn external_classic_key_document_is_guarded_and_owner_bound(pool: PgPool) {
    let pair = plamenu_ap::keys::generate_keypair().unwrap();
    let actor_id = "https://remote.example/users/classic";
    let method = "https://keys.remote.example/classic/main-key";
    let actor: plamenu_ap::actor::RemoteActor = serde_json::from_value(serde_json::json!({
        "id": actor_id,
        "type": "Person",
        "preferredUsername": "classic",
        "inbox": format!("{actor_id}/inbox"),
        "publicKey": method,
    }))
    .unwrap();
    let federation = StubFederation::default();
    federation.objects.lock().unwrap().insert(
        method.into(),
        serde_json::json!({
            "id": method,
            "owner": actor_id,
            "publicKeyPem": pair.public_pem,
        }),
    );
    let state = test_state_with(pool.clone(), std::sync::Arc::new(federation));
    let resolved = resolution("classic@remote.example", &[(actor_id, Some("Person"))]);
    let stored = store_remote_actor_from_resolution(&state, &actor, &resolved)
        .await
        .unwrap();
    let key = plamenu_db::actor_key::usable_by_uri(&pool, method)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(key.account_id, Some(stored.id));
    assert_eq!(key.controller_uri, actor_id);
    assert_eq!(key.algorithm, "rsa");
    assert_eq!(key.source, "classic-external");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn locally_suspended_actor_refresh_fetches_no_claimed_identity_or_key(pool: PgPool) {
    let mut alice = RemoteUser::new("remote.example", "alice");
    let federation = StubFederation::with_actors([alice.actor.clone()]);
    let state = test_state_with(pool.clone(), federation.clone());
    let stored = refresh_remote_actor(&state, &alice.actor).await.unwrap();
    plamenu_db::account::suspend(&pool, stored.id, "local")
        .await
        .unwrap();
    federation.fetches.lock().unwrap().clear();

    alice.actor.webfinger = Some("acct:stolen@evil.example".into());
    alice.actor.assertion_method = vec![serde_json::Value::String(
        "https://keys.evil.example/attacker".into(),
    )];
    let frozen = refresh_remote_actor(&state, &alice.actor).await.unwrap();
    assert_eq!(frozen.id, stored.id);
    assert_eq!(frozen.username, stored.username);
    assert!(federation.fetches().is_empty());
}
