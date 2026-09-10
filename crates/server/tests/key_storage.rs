//! Encrypted normalized-key migration, audit, contract and rotation.

mod common;

use common::{
    StubFederation, TEST_DOMAIN, create_immutable_local_account, test_config, test_state_with,
};
use plamenu::key_store::LegacyPrivateDisposition;
use plamenu_db::{PgPool, actor_key};

#[sqlx::test(migrations = "../db/migrations")]
async fn legacy_plaintext_backfill_is_verified_resumable_and_contractible(pool: PgPool) {
    let rsa = plamenu_ap::keys::generate_keypair().unwrap();
    let ed = plamenu_ap::keys::generate_ed25519_keypair();
    sqlx::query(
        "INSERT INTO accounts
            (id, username, private_key, public_key, uri,
             ed25519_private_key, ed25519_public_key)
         VALUES (101, 'legacy', $1, $2, $3, $4, $5)",
    )
    .bind(&rsa.private_pem)
    .bind(&rsa.public_pem)
    .bind("https://plamenu.test/users/legacy")
    .bind(&ed.private_multibase)
    .bind(&ed.public_multibase)
    .execute(&pool)
    .await
    .unwrap();

    let instance_rsa = plamenu_ap::keys::generate_keypair().unwrap();
    let instance_ed = plamenu_ap::keys::generate_ed25519_keypair();
    sqlx::query(
        "INSERT INTO instance_actor_keys
            (id, private_key, public_key, ed25519_private_key, ed25519_public_key)
         VALUES (1, $1, $2, $3, $4)",
    )
    .bind(&instance_rsa.private_pem)
    .bind(&instance_rsa.public_pem)
    .bind(&instance_ed.private_multibase)
    .bind(&instance_ed.public_multibase)
    .execute(&pool)
    .await
    .unwrap();

    let config = test_config();
    assert_eq!(
        plamenu::key_store::backfill_and_preflight(
            &pool,
            &config,
            LegacyPrivateDisposition::PreserveForRollback,
        )
        .await
        .unwrap(),
        2
    );
    assert_eq!(
        actor_key::plaintext_private_count(&pool).await.unwrap(),
        2,
        "the expand pass keeps verified sources readable by the old binary"
    );
    let rows = actor_key::private_rows(&pool).await.unwrap();
    assert_eq!(rows.len(), 4);
    assert!(rows.iter().all(|row| {
        row.encrypted_private_key
            .as_deref()
            .is_some_and(|value| value.starts_with("fsk1.1.") && !value.contains("PRIVATE KEY"))
    }));
    let keyring = plamenu::crypto::FederationKeyring::from_config(&config).unwrap();
    for row in rows {
        plamenu::key_store::decrypt_record(&keyring, row).unwrap();
    }
    assert_eq!(
        plamenu::key_store::backfill_and_preflight(
            &pool,
            &config,
            LegacyPrivateDisposition::PreserveForRollback,
        )
        .await
        .unwrap(),
        2,
        "a preserved batch can be reverified without duplicating keys"
    );
    assert_eq!(actor_key::private_rows(&pool).await.unwrap().len(), 4);

    let mut wrong = test_config();
    wrong.encryption_secret = Some("definitely-the-wrong-root-but-long-enough".into());
    assert!(
        plamenu::key_store::backfill_and_preflight(
            &pool,
            &wrong,
            LegacyPrivateDisposition::PreserveForRollback,
        )
        .await
        .is_err(),
        "startup refuses ciphertext that the configured root cannot open"
    );

    let state = test_state_with(pool.clone(), std::sync::Arc::new(StubFederation::default()));
    assert!(
        plamenu::key_store::audit_private_storage(&state)
            .await
            .is_err(),
        "strict release audit stays red while rollback plaintext is retained"
    );
    assert!(
        plamenu::key_store::contract_legacy_private_storage(&state)
            .await
            .unwrap()
    );
    assert_eq!(
        actor_key::legacy_private_columns(&pool).await.unwrap(),
        actor_key::LegacyPrivateColumns {
            accounts: false,
            instance: false,
        }
    );
    assert_eq!(
        plamenu::key_store::audit_private_storage(&state)
            .await
            .unwrap(),
        4
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn mismatched_legacy_public_half_aborts_without_clearing_source(pool: PgPool) {
    let rsa = plamenu_ap::keys::generate_keypair().unwrap();
    sqlx::query(
        "INSERT INTO accounts (id, username, private_key, public_key, uri)
         VALUES (102, 'broken', $1, 'not-the-derived-public-key', $2)",
    )
    .bind(&rsa.private_pem)
    .bind("https://plamenu.test/users/broken")
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        plamenu::key_store::backfill_and_preflight(
            &pool,
            &test_config(),
            LegacyPrivateDisposition::PreserveForRollback,
        )
        .await
        .is_err()
    );
    assert_eq!(actor_key::plaintext_private_count(&pool).await.unwrap(), 1);
    assert!(actor_key::private_rows(&pool).await.unwrap().is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn signing_rotation_overlaps_then_rewraps_to_new_envelope_root(pool: PgPool) {
    let account = create_immutable_local_account(&pool, "alice", "Alice").await;
    let state = test_state_with(pool.clone(), std::sync::Arc::new(StubFederation::default()));
    let original = actor_key::usable_for_account(&pool, account.id)
        .await
        .unwrap();
    let old_ed = original
        .iter()
        .find(|key| key.algorithm == "ed25519")
        .unwrap()
        .key_uri
        .clone();
    let rotated = plamenu::key_store::rotate_account_key(
        &state,
        &account,
        "ed25519",
        time::Duration::hours(24),
        time::Duration::ZERO,
    )
    .await
    .unwrap();
    assert_ne!(rotated.key_uri, old_ed);
    let overlap = actor_key::usable_for_account(&pool, account.id)
        .await
        .unwrap();
    assert_eq!(
        overlap
            .iter()
            .filter(|key| key.algorithm == "ed25519")
            .count(),
        2
    );
    assert!(
        overlap
            .iter()
            .find(|key| key.key_uri == old_ed)
            .unwrap()
            .expires_at
            .is_some()
    );
    assert_eq!(
        plamenu::key_store::account_signing_key(&state, account.id, "ed25519")
            .await
            .unwrap()
            .record
            .key_uri,
        rotated.key_uri,
        "new deliveries deterministically select the newest active key"
    );
    let actor = plamenu::profile::local_actor(&state, &account)
        .await
        .unwrap();
    assert!(
        actor
            .assertion_method
            .iter()
            .filter(|method| method.kind == "Multikey" && method.id.contains("ed25519"))
            .count()
            >= 2,
        "the actor publishes both keys throughout overlap"
    );

    let ring = state.federation_keyring.as_deref().unwrap();
    plamenu::key_store::ensure_instance(&pool, ring, TEST_DOMAIN)
        .await
        .unwrap();
    plamenu::key_store::rotate_instance_key(
        &state,
        "rsa",
        time::Duration::hours(24),
        time::Duration::ZERO,
    )
    .await
    .unwrap();

    let mut next_config = test_config();
    let old_secret = next_config.encryption_secret.take().unwrap();
    next_config.encryption_secret = Some("new-primary-envelope-root-at-least-32-bytes".into());
    next_config.encryption_secret_version = 2;
    next_config.encryption_previous_secrets = vec![(1, old_secret)];
    let next_state = plamenu::AppState::new(
        pool.clone(),
        next_config,
        std::sync::Arc::new(StubFederation::default()),
        std::sync::Arc::new(plamenu::storage::MemoryStore::default()),
    )
    .unwrap();
    let count = actor_key::private_rows(&pool).await.unwrap().len();
    assert_eq!(
        plamenu::key_store::rewrap_batch(&next_state, 1)
            .await
            .unwrap(),
        1,
        "a deliberately interrupted rehearsal commits a bounded prefix"
    );
    assert!(
        actor_key::private_rows(&pool)
            .await
            .unwrap()
            .iter()
            .any(|key| key.encryption_key_version == Some(1)),
        "old-version rows remain for the resume pass"
    );
    assert_eq!(
        plamenu::key_store::rewrap_all(&next_state).await.unwrap(),
        count - 1,
        "the unbounded pass resumes the remaining rows"
    );
    assert!(
        actor_key::private_rows(&pool)
            .await
            .unwrap()
            .iter()
            .all(|key| key.encryption_key_version == Some(2))
    );
    assert_eq!(
        plamenu::key_store::rewrap_all(&next_state).await.unwrap(),
        0
    );
    assert_eq!(
        plamenu::key_store::audit_private_storage(&next_state)
            .await
            .unwrap(),
        count
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn pending_replacements_publish_before_they_become_signers(pool: PgPool) {
    let account = create_immutable_local_account(&pool, "staged", "Staged").await;
    let state = test_state_with(pool.clone(), std::sync::Arc::new(StubFederation::default()));
    let old_account_rsa = plamenu::key_store::account_signing_key(&state, account.id, "rsa")
        .await
        .unwrap()
        .record
        .key_uri;
    let pending_account = plamenu::key_store::rotate_account_key(
        &state,
        &account,
        "rsa",
        time::Duration::hours(24),
        time::Duration::hours(1),
    )
    .await
    .unwrap();
    assert_eq!(
        plamenu::key_store::account_signing_key(&state, account.id, "rsa")
            .await
            .unwrap()
            .record
            .key_uri,
        old_account_rsa,
        "the old key signs propagation during the activation delay"
    );
    assert!(
        actor_key::published_for_account(&pool, account.id)
            .await
            .unwrap()
            .iter()
            .any(|key| key.key_uri == pending_account.key_uri)
    );
    assert_eq!(
        plamenu::profile::local_actor(&state, &account)
            .await
            .unwrap()
            .public_key
            .id,
        pending_account.key_uri,
        "the pending replacement is already advertised to peers"
    );

    let ring = state.federation_keyring.as_deref().unwrap();
    plamenu::key_store::ensure_instance(&pool, ring, TEST_DOMAIN)
        .await
        .unwrap();
    let old_instance_rsa = plamenu::key_store::instance_signing_key(&state, "rsa")
        .await
        .unwrap()
        .record
        .key_uri;
    let pending_instance = plamenu::key_store::rotate_instance_key(
        &state,
        "rsa",
        time::Duration::hours(24),
        time::Duration::hours(1),
    )
    .await
    .unwrap();
    assert_eq!(
        plamenu::key_store::instance_signing_key(&state, "rsa")
            .await
            .unwrap()
            .record
            .key_uri,
        old_instance_rsa
    );
    assert!(
        actor_key::published_for_instance(&pool)
            .await
            .unwrap()
            .iter()
            .any(|key| key.key_uri == pending_instance.key_uri)
    );
    assert_eq!(
        plamenu::instance_actor::document(&state)
            .await
            .unwrap()
            .public_key
            .id,
        pending_instance.key_uri
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn concurrent_rotations_keep_exactly_one_unbounded_current_key(pool: PgPool) {
    let account = create_immutable_local_account(&pool, "racer", "Racer").await;
    let state = test_state_with(pool.clone(), std::sync::Arc::new(StubFederation::default()));
    let (first, second) = tokio::join!(
        plamenu::key_store::rotate_account_key(
            &state,
            &account,
            "ed25519",
            time::Duration::hours(24),
            time::Duration::ZERO,
        ),
        plamenu::key_store::rotate_account_key(
            &state,
            &account,
            "ed25519",
            time::Duration::hours(24),
            time::Duration::ZERO,
        ),
    );
    first.unwrap();
    second.unwrap();
    let account_keys = actor_key::usable_for_account(&pool, account.id)
        .await
        .unwrap();
    let current: Vec<_> = account_keys
        .iter()
        .filter(|key| key.algorithm == "ed25519" && key.expires_at.is_none())
        .collect();
    assert_eq!(current.len(), 1);
    assert_eq!(
        plamenu::key_store::account_signing_key(&state, account.id, "ed25519")
            .await
            .unwrap()
            .record
            .key_uri,
        current[0].key_uri
    );

    let ring = state.federation_keyring.as_deref().unwrap();
    plamenu::key_store::ensure_instance(&pool, ring, TEST_DOMAIN)
        .await
        .unwrap();
    let (first, second) = tokio::join!(
        plamenu::key_store::rotate_instance_key(
            &state,
            "rsa",
            time::Duration::hours(24),
            time::Duration::ZERO,
        ),
        plamenu::key_store::rotate_instance_key(
            &state,
            "rsa",
            time::Duration::hours(24),
            time::Duration::ZERO,
        ),
    );
    first.unwrap();
    second.unwrap();
    let instance_keys = actor_key::usable_for_instance(&pool).await.unwrap();
    let current: Vec<_> = instance_keys
        .iter()
        .filter(|key| key.algorithm == "rsa" && key.expires_at.is_none())
        .collect();
    assert_eq!(current.len(), 1);
    assert_eq!(
        plamenu::key_store::instance_signing_key(&state, "rsa")
            .await
            .unwrap()
            .record
            .key_uri,
        current[0].key_uri
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn concurrent_instance_provisioning_is_atomic_and_idempotent(pool: PgPool) {
    let config = test_config();
    let ring = plamenu::crypto::FederationKeyring::from_config(&config).unwrap();
    let (first, second) = tokio::join!(
        plamenu::key_store::ensure_instance(&pool, &ring, TEST_DOMAIN),
        plamenu::key_store::ensure_instance(&pool, &ring, TEST_DOMAIN),
    );
    first.unwrap();
    second.unwrap();
    let rows = actor_key::usable_for_instance(&pool).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|key| key.algorithm == "rsa"));
    assert!(rows.iter().any(|key| key.algorithm == "ed25519"));
    for row in rows {
        plamenu::key_store::decrypt_record(&ring, row).unwrap();
    }
}
