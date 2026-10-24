//! Fixture actors retain independent real RSA keys, including after reserve exhaustion.

use std::collections::HashSet;
use std::time::SystemTime;

use axum::http::HeaderMap;
use plamenu_ap::keys::KeyPairPem;
use plamenu_db::{PgPool, actor_key};
use plamenu_federation::{PreparedVerification, RequestSigner};
use plamenu_test_support::{
    RemoteUser, create_immutable_local_account, create_local_account, test_config,
};

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn local_and_remote_actors_keep_distinct_signing_keys_after_fixture_reserve_exhaustion(
    pool: PgPool,
) {
    let local = create_local_account(&pool, "legacy", "Legacy").await;
    let immutable = create_immutable_local_account(&pool, "immutable", "Immutable").await;
    let keyring = plamenu::crypto::FederationKeyring::from_config(&test_config()).unwrap();
    let mut actors = Vec::new();
    let mut ed25519_keys = HashSet::new();
    for account in [local, immutable] {
        let rows = actor_key::usable_for_account(&pool, account.id)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        for row in rows {
            assert!(row.encrypted_private_key.is_some());
            let decrypted = plamenu::key_store::decrypt_record(&keyring, row).unwrap();
            if decrypted.record.algorithm == "rsa" {
                actors.push((
                    KeyPairPem {
                        private_pem: decrypted.private.expose_str().unwrap().to_owned(),
                        public_pem: decrypted.record.public_key,
                    },
                    decrypted.record.key_uri,
                ));
            } else {
                assert_eq!(decrypted.record.algorithm, "ed25519");
                assert!(ed25519_keys.insert(decrypted.record.public_key));
            }
        }
    }
    assert_eq!(ed25519_keys.len(), 2);
    // Two locals plus seventy remotes exceed the 64-pair reserve. Repeated
    // handles must still get new keys; exhaustion must never wrap the pool.
    for _ in 0..70 {
        let user = RemoteUser::new("fixture.invalid", "same-handle");
        assert_eq!(user.actor.public_key.public_key_pem, user.keys.public_pem);
        actors.push((user.keys, user.actor.public_key.id.clone()));
    }
    assert_eq!(actors.len(), 72);
    let mut public_keys = HashSet::new();
    let body = br#"{"type":"Follow"}"#;
    let now = SystemTime::now();
    for (index, (keys, key_id)) in actors.iter().enumerate() {
        assert!(public_keys.insert(keys.public_pem.clone()));
        assert_eq!(
            plamenu_ap::keys::rsa_public_from_private(&keys.private_pem).unwrap(),
            keys.public_pem
        );
        let signer = RequestSigner::from_pkcs8_pem(&keys.private_pem, key_id.clone()).unwrap();
        let signed = signer.sign_post("fixture.invalid", "/inbox", body, now);
        let mut headers = HeaderMap::new();
        headers.insert("host", signed.host.parse().unwrap());
        headers.insert("date", signed.date.parse().unwrap());
        headers.insert("digest", signed.digest.parse().unwrap());
        headers.insert("signature", signed.signature.parse().unwrap());
        let verification =
            PreparedVerification::from_request("POST", "/inbox", &headers, body, now).unwrap();
        assert_eq!(verification.key_id(), key_id);
        verification.verify_pem(&keys.public_pem).unwrap();
        let different_key = &actors[(index + 1) % actors.len()].0.public_pem;
        assert!(verification.verify_pem(different_key).is_err());
    }
    assert_eq!(public_keys.len(), 72);
}
