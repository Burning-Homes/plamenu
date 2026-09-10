//! Application boundary for normalized `ActivityPub` keys.
//!
//! Database code sees only public material and opaque ciphertext. This module
//! alone turns configured external secrets into short-lived plaintext signing
//! keys, validates public/private correspondence, provisions new keys, and
//! performs the idempotent legacy backfill.

use plamenu_ap::urls::{InstanceActorUrls, LocalUserUrls};
use plamenu_db::account::Account;
use plamenu_db::actor_key::{self, ActorKey, NewPublicKey, PrivateEnvelope};
use plamenu_db::{DbError, PgConnection, PgPool};
use thiserror::Error;
use zeroize::Zeroizing;

use crate::config::Config;
use crate::crypto::{FederationKeyring, KeyEncryptionError, PrivateSigningKey};

#[derive(Debug, Error)]
pub enum KeyStoreError {
    #[error("key publication failed: {0}")]
    Publication(#[source] Box<crate::error::ApiError>),
    #[error(transparent)]
    Database(#[from] DbError),
    #[error(transparent)]
    Encryption(#[from] KeyEncryptionError),
    #[error("key {0} has no encrypted private material")]
    MissingPrivate(String),
    #[error("key {0} has no encryption-key version")]
    MissingVersion(String),
    #[error("key {0} has invalid or mismatched public/private material")]
    MismatchedMaterial(String),
    #[error("published key URI {0} is already bound to different material")]
    ConflictingKeyUri(String),
    #[error("local account {0} has no persisted ActivityPub actor URI")]
    MissingActorUri(i64),
}

pub struct DecryptedActorKey {
    pub record: ActorKey,
    pub private: PrivateSigningKey,
}

fn new_key(
    key_uri: String,
    controller_uri: String,
    algorithm: &str,
    public_key: String,
    source: &str,
) -> NewPublicKey {
    NewPublicKey {
        key_uri,
        controller_uri,
        algorithm: algorithm.to_owned(),
        public_key,
        source: source.to_owned(),
        expires_at: None,
    }
}

fn envelope(encrypted: &crate::crypto::EncryptedPrivateKey) -> PrivateEnvelope<'_> {
    PrivateEnvelope {
        ciphertext: &encrypted.ciphertext,
        key_version: encrypted.key_version,
    }
}

fn validate_plaintext(record: &ActorKey, plaintext: &str) -> Result<(), KeyStoreError> {
    let derived = match record.algorithm.as_str() {
        "rsa" => plamenu_ap::keys::rsa_public_from_private(plaintext)
            .map_err(|_| KeyStoreError::MismatchedMaterial(record.key_uri.clone()))?,
        "ed25519" => plamenu_ap::keys::ed25519_public_from_private(plaintext)
            .map_err(|_| KeyStoreError::MismatchedMaterial(record.key_uri.clone()))?,
        _ => return Err(KeyStoreError::MismatchedMaterial(record.key_uri.clone())),
    };
    if derived == record.public_key {
        Ok(())
    } else {
        Err(KeyStoreError::MismatchedMaterial(record.key_uri.clone()))
    }
}

pub fn decrypt_record(
    keyring: &FederationKeyring,
    record: ActorKey,
) -> Result<DecryptedActorKey, KeyStoreError> {
    let ciphertext = record
        .encrypted_private_key
        .as_deref()
        .ok_or_else(|| KeyStoreError::MissingPrivate(record.key_uri.clone()))?;
    let version = record
        .encryption_key_version
        .ok_or_else(|| KeyStoreError::MissingVersion(record.key_uri.clone()))?;
    let private = keyring.decrypt(&record.key_uri, version, ciphertext)?;
    let text = private
        .expose_str()
        .map_err(|_| KeyStoreError::MismatchedMaterial(record.key_uri.clone()))?;
    validate_plaintext(&record, text)?;
    Ok(DecryptedActorKey { record, private })
}

/// Stores a freshly-generated RSA+Ed25519 pair for a new local actor. The
/// account row itself receives public material only.
pub async fn provision_account_tx(
    conn: &mut PgConnection,
    keyring: &FederationKeyring,
    domain: &str,
    account: &Account,
    rsa: &plamenu_ap::keys::KeyPairPem,
    ed25519: &plamenu_ap::keys::Ed25519KeyPairMultibase,
) -> Result<(), KeyStoreError> {
    let urls = LocalUserUrls::for_account(domain, &account.username, account.uri.as_deref());
    let rsa_key = new_key(
        urls.key_id,
        urls.id.clone(),
        "rsa",
        rsa.public_pem.clone(),
        "generated",
    );
    let rsa_private = keyring.encrypt(&rsa_key.key_uri, rsa.private_pem.as_bytes());
    let stored = actor_key::put_local(&mut *conn, account.id, &rsa_key, envelope(&rsa_private))
        .await?
        .ok_or_else(|| KeyStoreError::ConflictingKeyUri(rsa_key.key_uri.clone()))?;
    decrypt_record(keyring, stored)?;

    let ed_key = new_key(
        urls.ed25519_key_id,
        urls.id,
        "ed25519",
        ed25519.public_multibase.clone(),
        "generated",
    );
    let ed_private = keyring.encrypt(&ed_key.key_uri, ed25519.private_multibase.as_bytes());
    let stored = actor_key::put_local(&mut *conn, account.id, &ed_key, envelope(&ed_private))
        .await?
        .ok_or_else(|| KeyStoreError::ConflictingKeyUri(ed_key.key_uri.clone()))?;
    decrypt_record(keyring, stored)?;
    Ok(())
}

/// Stores the RSA key a FEP-ae97 gateway generated for a portable actor. The
/// client publishes this key as `#gateway-rsa`; unlike a normal local account,
/// its actor URI and key ID are supplied by the portable identity itself.
pub async fn provision_gateway_rsa_tx(
    conn: &mut PgConnection,
    keyring: &FederationKeyring,
    account: &Account,
    actor_uri: &str,
    rsa: &plamenu_ap::keys::KeyPairPem,
) -> Result<(), KeyStoreError> {
    let key = new_key(
        format!("{actor_uri}#gateway-rsa"),
        actor_uri.to_owned(),
        "rsa",
        rsa.public_pem.clone(),
        "fep-ae97-gateway",
    );
    let encrypted = keyring.encrypt(&key.key_uri, rsa.private_pem.as_bytes());
    let stored = actor_key::put_local(&mut *conn, account.id, &key, envelope(&encrypted))
        .await?
        .ok_or_else(|| KeyStoreError::ConflictingKeyUri(key.key_uri.clone()))?;
    decrypt_record(keyring, stored)?;
    Ok(())
}

pub async fn provision_account(
    pool: &PgPool,
    keyring: &FederationKeyring,
    domain: &str,
    account: &Account,
    rsa: &plamenu_ap::keys::KeyPairPem,
    ed25519: &plamenu_ap::keys::Ed25519KeyPairMultibase,
) -> Result<(), KeyStoreError> {
    let mut tx = pool.begin().await.map_err(DbError::from)?;
    provision_account_tx(&mut tx, keyring, domain, account, rsa, ed25519).await?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}

async fn put_instance_pair_tx(
    conn: &mut PgConnection,
    keyring: &FederationKeyring,
    domain: &str,
    rsa: &plamenu_ap::keys::KeyPairPem,
    ed25519: &plamenu_ap::keys::Ed25519KeyPairMultibase,
    source: &str,
) -> Result<(), KeyStoreError> {
    let urls = InstanceActorUrls::new(domain);
    for (key, plaintext) in [
        (
            new_key(
                urls.key_id,
                urls.id.clone(),
                "rsa",
                rsa.public_pem.clone(),
                source,
            ),
            rsa.private_pem.as_str(),
        ),
        (
            new_key(
                urls.ed25519_key_id,
                urls.id,
                "ed25519",
                ed25519.public_multibase.clone(),
                source,
            ),
            ed25519.private_multibase.as_str(),
        ),
    ] {
        let encrypted = keyring.encrypt(&key.key_uri, plaintext.as_bytes());
        let record = actor_key::put_instance(&mut *conn, &key, envelope(&encrypted))
            .await?
            .ok_or_else(|| KeyStoreError::ConflictingKeyUri(key.key_uri.clone()))?;
        decrypt_record(keyring, record)?;
    }
    Ok(())
}

async fn put_instance_pair(
    pool: &PgPool,
    keyring: &FederationKeyring,
    domain: &str,
    rsa: &plamenu_ap::keys::KeyPairPem,
    ed25519: &plamenu_ap::keys::Ed25519KeyPairMultibase,
    source: &str,
) -> Result<(), KeyStoreError> {
    let mut tx = pool.begin().await.map_err(DbError::from)?;
    actor_key::lock_instance_owner(&mut tx).await?;
    put_instance_pair_tx(&mut tx, keyring, domain, rsa, ed25519, source).await?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}

/// Whether a verified legacy plaintext source is retained for old binaries in
/// a rolling deployment or compare-and-cleared at the final contract gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyPrivateDisposition {
    PreserveForRollback,
    ClearVerified,
}

/// Encrypts every legacy plaintext source and verifies it by reconstructing
/// the matching public key. During an expand/rolling deployment the source is
/// retained so old web/worker binaries remain usable; the operator-triggered
/// contract pass compare-and-clears each verified source before dropping the
/// columns. Both modes are safe to resume after interruption.
#[allow(
    clippy::too_many_lines,
    reason = "the transactional expand/preflight sequence must remain visibly linear"
)]
pub async fn backfill_and_preflight(
    pool: &PgPool,
    config: &Config,
    disposition: LegacyPrivateDisposition,
) -> Result<usize, KeyStoreError> {
    const BATCH: i64 = 100;
    let legacy_columns = actor_key::legacy_private_columns(pool).await?;
    let plaintext_count = actor_key::plaintext_private_count(pool).await?;
    let private_rows = actor_key::private_rows(pool).await?;
    let mut migrated = 0usize;
    if plaintext_count > 0 || !private_rows.is_empty() {
        let keyring = FederationKeyring::from_config(config)?;
        let mut after_id = i64::MIN;
        loop {
            let legacy_accounts = if legacy_columns.accounts {
                actor_key::legacy_account_rows_after(pool, after_id, BATCH).await?
            } else {
                Vec::new()
            };
            if legacy_accounts.is_empty() {
                break;
            }
            let mut batch_migrated = 0usize;
            for legacy in legacy_accounts {
                after_id = legacy.id;
                let actor_id = legacy
                    .uri
                    .clone()
                    .ok_or(KeyStoreError::MissingActorUri(legacy.id))?;
                if let Some(private) = &legacy.private_key {
                    let public = plamenu_ap::keys::rsa_public_from_private(private)
                        .map_err(|_| KeyStoreError::MismatchedMaterial(actor_id.clone()))?;
                    if public != legacy.public_key {
                        return Err(KeyStoreError::MismatchedMaterial(actor_id));
                    }
                    let key = new_key(
                        format!("{}#main-key", legacy.uri.as_deref().unwrap_or_default()),
                        legacy.uri.clone().unwrap_or_default(),
                        "rsa",
                        public,
                        "legacy-backfill",
                    );
                    let encrypted = keyring.encrypt(&key.key_uri, private.as_bytes());
                    let stored = actor_key::put_local(pool, legacy.id, &key, envelope(&encrypted))
                        .await?
                        .ok_or_else(|| KeyStoreError::ConflictingKeyUri(key.key_uri.clone()))?;
                    decrypt_record(&keyring, stored)?;
                }
                if let (Some(private), Some(public)) = (
                    legacy.ed25519_private_key.as_deref(),
                    legacy.ed25519_public_key.as_deref(),
                ) {
                    if plamenu_ap::keys::ed25519_public_from_private(private)
                        .map_err(|_| KeyStoreError::MismatchedMaterial(actor_id.clone()))?
                        != public
                    {
                        return Err(KeyStoreError::MismatchedMaterial(actor_id));
                    }
                    let key = new_key(
                        format!("{}#ed25519-key", legacy.uri.as_deref().unwrap_or_default()),
                        legacy.uri.clone().unwrap_or_default(),
                        "ed25519",
                        public.to_owned(),
                        "legacy-backfill",
                    );
                    let encrypted = keyring.encrypt(&key.key_uri, private.as_bytes());
                    let stored = actor_key::put_local(pool, legacy.id, &key, envelope(&encrypted))
                        .await?
                        .ok_or_else(|| KeyStoreError::ConflictingKeyUri(key.key_uri.clone()))?;
                    decrypt_record(&keyring, stored)?;
                }
                if disposition == LegacyPrivateDisposition::ClearVerified
                    && !actor_key::clear_legacy_account_private(
                        pool,
                        legacy.id,
                        legacy.private_key.as_deref(),
                        legacy.ed25519_private_key.as_deref(),
                    )
                    .await?
                {
                    return Err(KeyStoreError::MismatchedMaterial(actor_id));
                }
                migrated += 1;
                batch_migrated += 1;
            }
            tracing::info!(
                processed = batch_migrated,
                last_account_id = after_id,
                "federation private-key backfill batch committed"
            );
        }

        let legacy_instance = if legacy_columns.instance {
            actor_key::legacy_instance_row(pool).await?
        } else {
            None
        };
        if let Some(legacy) = legacy_instance {
            let public = plamenu_ap::keys::rsa_public_from_private(&legacy.private_key)
                .map_err(|_| KeyStoreError::MismatchedMaterial("instance actor".into()))?;
            if public != legacy.public_key {
                return Err(KeyStoreError::MismatchedMaterial("instance actor".into()));
            }
            let ed_private = legacy
                .ed25519_private_key
                .as_deref()
                .ok_or_else(|| KeyStoreError::MissingPrivate("instance Ed25519".into()))?;
            let ed_public = legacy
                .ed25519_public_key
                .as_deref()
                .ok_or_else(|| KeyStoreError::MissingPrivate("instance Ed25519".into()))?;
            if plamenu_ap::keys::ed25519_public_from_private(ed_private)
                .map_err(|_| KeyStoreError::MismatchedMaterial("instance actor".into()))?
                != ed_public
            {
                return Err(KeyStoreError::MismatchedMaterial("instance actor".into()));
            }
            let rsa = plamenu_ap::keys::KeyPairPem {
                private_pem: legacy.private_key.clone(),
                public_pem: legacy.public_key,
            };
            let ed = plamenu_ap::keys::Ed25519KeyPairMultibase {
                private_multibase: ed_private.to_owned(),
                public_multibase: ed_public.to_owned(),
            };
            put_instance_pair(pool, &keyring, &config.domain, &rsa, &ed, "legacy-backfill").await?;
            if disposition == LegacyPrivateDisposition::ClearVerified
                && !actor_key::clear_legacy_instance_private(
                    pool,
                    &legacy.private_key,
                    legacy.ed25519_private_key.as_deref(),
                )
                .await?
            {
                return Err(KeyStoreError::MismatchedMaterial("instance actor".into()));
            }
            migrated += 1;
        }

        for record in actor_key::private_rows(pool).await? {
            decrypt_record(&keyring, record)?;
        }
        if disposition == LegacyPrivateDisposition::ClearVerified
            && actor_key::plaintext_private_count(pool).await? != 0
        {
            return Err(KeyStoreError::MismatchedMaterial("plaintext audit".into()));
        }
    }

    // Public-only remote backfill is independent of encryption config. The
    // legacy columns did not retain the original Ed25519 key URI, so that
    // one compatibility row uses the convention the old verifier used.
    let mut after_id = i64::MIN;
    loop {
        let rows = actor_key::legacy_remote_rows_after(pool, after_id, BATCH).await?;
        if rows.is_empty() {
            break;
        }
        let mut batch_migrated = 0usize;
        for legacy in rows {
            after_id = legacy.id;
            let mut keys = Vec::with_capacity(2);
            if !legacy.public_key.is_empty() && !legacy.public_key_id.is_empty() {
                keys.push(new_key(
                    legacy.public_key_id,
                    legacy.uri.clone(),
                    "rsa",
                    legacy.public_key,
                    "legacy-remote-backfill",
                ));
            }
            if let Some(public) = legacy.ed25519_public_key.filter(|key| !key.is_empty()) {
                keys.push(new_key(
                    format!("{}#ed25519-key", legacy.uri),
                    legacy.uri.clone(),
                    "ed25519",
                    public,
                    "legacy-remote-backfill",
                ));
            }
            actor_key::replace_remote(pool, legacy.id, &legacy.uri, &keys).await?;
            migrated += 1;
            batch_migrated += 1;
        }
        tracing::info!(
            processed = batch_migrated,
            last_account_id = after_id,
            "remote public-key backfill batch committed"
        );
    }
    Ok(migrated)
}

pub async fn ensure_instance(
    pool: &PgPool,
    keyring: &FederationKeyring,
    domain: &str,
) -> Result<(), KeyStoreError> {
    let mut tx = pool.begin().await.map_err(DbError::from)?;
    actor_key::lock_instance_owner(&mut tx).await?;
    let existing = actor_key::usable_for_instance(&mut *tx).await?;
    let urls = InstanceActorUrls::new(domain);
    if !existing.iter().any(|key| {
        key.algorithm == "rsa"
            && key.controller_uri == urls.id
            && key.encrypted_private_key.is_some()
    }) {
        let rsa = plamenu_ap::keys::generate_keypair()
            .map_err(|_| KeyStoreError::MismatchedMaterial("generated RSA".into()))?;
        let key = new_key(
            urls.key_id.clone(),
            urls.id.clone(),
            "rsa",
            rsa.public_pem.clone(),
            "generated",
        );
        let encrypted = keyring.encrypt(&key.key_uri, rsa.private_pem.as_bytes());
        let stored = actor_key::put_instance(&mut *tx, &key, envelope(&encrypted))
            .await?
            .ok_or_else(|| KeyStoreError::ConflictingKeyUri(key.key_uri.clone()))?;
        decrypt_record(keyring, stored)?;
    }
    if !existing.iter().any(|key| {
        key.algorithm == "ed25519"
            && key.controller_uri == urls.id
            && key.encrypted_private_key.is_some()
    }) {
        let ed = plamenu_ap::keys::generate_ed25519_keypair();
        let key = new_key(
            urls.ed25519_key_id,
            urls.id,
            "ed25519",
            ed.public_multibase.clone(),
            "generated",
        );
        let encrypted = keyring.encrypt(&key.key_uri, ed.private_multibase.as_bytes());
        let stored = actor_key::put_instance(&mut *tx, &key, envelope(&encrypted))
            .await?
            .ok_or_else(|| KeyStoreError::ConflictingKeyUri(key.key_uri.clone()))?;
        decrypt_record(keyring, stored)?;
    }
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}

pub async fn account_signing_key(
    state: &crate::AppState,
    account_id: i64,
    algorithm: &str,
) -> Result<DecryptedActorKey, KeyStoreError> {
    let record = actor_key::usable_for_account(&state.pool, account_id)
        .await?
        .into_iter()
        .find(|key| key.algorithm == algorithm && key.encrypted_private_key.is_some())
        .ok_or_else(|| {
            KeyStoreError::MissingPrivate(format!("account {account_id} {algorithm}"))
        })?;
    let keyring = state
        .federation_keyring
        .as_deref()
        .ok_or(KeyEncryptionError::MissingConfiguration)?;
    decrypt_record(keyring, record)
}

pub async fn instance_signing_key(
    state: &crate::AppState,
    algorithm: &str,
) -> Result<DecryptedActorKey, KeyStoreError> {
    let record = actor_key::usable_for_instance(&state.pool)
        .await?
        .into_iter()
        .find(|key| key.algorithm == algorithm && key.encrypted_private_key.is_some())
        .ok_or_else(|| KeyStoreError::MissingPrivate(format!("instance {algorithm}")))?;
    let keyring = state
        .federation_keyring
        .as_deref()
        .ok_or(KeyEncryptionError::MissingConfiguration)?;
    decrypt_record(keyring, record)
}

/// Creates a fresh local signing key and leaves the previous key published
/// for a bounded overlap. Signing selection immediately prefers this newest
/// row; the repository stops publishing/verifying the old row at `expires_at`.
pub async fn rotate_account_key(
    state: &crate::AppState,
    account: &Account,
    algorithm: &str,
    overlap: time::Duration,
    activation_delay: time::Duration,
) -> Result<ActorKey, KeyStoreError> {
    let actor_id = account
        .uri
        .clone()
        .ok_or(KeyStoreError::MissingActorUri(account.id))?;
    let keyring = state
        .federation_keyring
        .as_deref()
        .ok_or(KeyEncryptionError::MissingConfiguration)?;
    let marker = plamenu_db::id::next();
    let (key_uri, public, private) = match algorithm {
        "rsa" => {
            let pair = plamenu_ap::keys::generate_keypair()
                .map_err(|_| KeyStoreError::MismatchedMaterial("generated RSA".into()))?;
            (
                format!("{actor_id}#main-key-{marker}"),
                pair.public_pem.clone(),
                pair.private_pem.clone(),
            )
        }
        "ed25519" => {
            let pair = plamenu_ap::keys::generate_ed25519_keypair();
            (
                format!("{actor_id}#ed25519-key-{marker}"),
                pair.public_multibase.clone(),
                pair.private_multibase.clone(),
            )
        }
        _ => return Err(KeyStoreError::MismatchedMaterial(algorithm.to_owned())),
    };
    let private = Zeroizing::new(private);
    let key = new_key(key_uri, actor_id, algorithm, public, "rotation");
    let encrypted = keyring.encrypt(&key.key_uri, private.as_bytes());
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    if !actor_key::lock_account_owner(&mut tx, account.id).await? {
        return Err(KeyStoreError::MissingActorUri(account.id));
    }
    let stored = actor_key::put_local(&mut *tx, account.id, &key, envelope(&encrypted))
        .await?
        .ok_or_else(|| KeyStoreError::ConflictingKeyUri(key.key_uri.clone()))?;
    let stored = decrypt_record(keyring, stored)?.record;
    let activates_at = time::OffsetDateTime::now_utc() + activation_delay;
    let stored = actor_key::schedule_activation(&mut *tx, stored.id, activates_at).await?;
    actor_key::expire_other_private_keys(
        &mut *tx,
        "account",
        Some(account.id),
        algorithm,
        &stored.key_uri,
        activates_at + overlap,
    )
    .await?;
    crate::profile::fan_out_actor_update_conn(state, &mut tx, account)
        .await
        .map_err(|error| KeyStoreError::Publication(Box::new(error)))?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(stored)
}

pub async fn rotate_instance_key(
    state: &crate::AppState,
    algorithm: &str,
    overlap: time::Duration,
    activation_delay: time::Duration,
) -> Result<ActorKey, KeyStoreError> {
    let keyring = state
        .federation_keyring
        .as_deref()
        .ok_or(KeyEncryptionError::MissingConfiguration)?;
    let actor_id = plamenu_ap::urls::InstanceActorUrls::new(&state.config.domain).id;
    let marker = plamenu_db::id::next();
    let (key_uri, public, private) = match algorithm {
        "rsa" => {
            let pair = plamenu_ap::keys::generate_keypair()
                .map_err(|_| KeyStoreError::MismatchedMaterial("generated RSA".into()))?;
            (
                format!("{actor_id}#main-key-{marker}"),
                pair.public_pem.clone(),
                pair.private_pem.clone(),
            )
        }
        "ed25519" => {
            let pair = plamenu_ap::keys::generate_ed25519_keypair();
            (
                format!("{actor_id}#ed25519-key-{marker}"),
                pair.public_multibase.clone(),
                pair.private_multibase.clone(),
            )
        }
        _ => return Err(KeyStoreError::MismatchedMaterial(algorithm.to_owned())),
    };
    let private = Zeroizing::new(private);
    let key = new_key(key_uri, actor_id, algorithm, public, "rotation");
    let encrypted = keyring.encrypt(&key.key_uri, private.as_bytes());
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    // The instance actor has no account row to lock. A transaction-scoped
    // advisory lock gives all algorithms one owner-wide rotation order.
    actor_key::lock_instance_owner(&mut tx).await?;
    let stored = actor_key::put_instance(&mut *tx, &key, envelope(&encrypted))
        .await?
        .ok_or_else(|| KeyStoreError::ConflictingKeyUri(key.key_uri.clone()))?;
    let stored = decrypt_record(keyring, stored)?.record;
    let activates_at = time::OffsetDateTime::now_utc() + activation_delay;
    let stored = actor_key::schedule_activation(&mut *tx, stored.id, activates_at).await?;
    actor_key::expire_other_private_keys(
        &mut *tx,
        "instance",
        None,
        algorithm,
        &stored.key_uri,
        activates_at + overlap,
    )
    .await?;
    crate::instance_actor::fan_out_actor_update_conn(state, &mut tx)
        .await
        .map_err(|error| KeyStoreError::Publication(Box::new(error)))?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(stored)
}

const REWRAP_BATCH: usize = 100;

/// Re-encrypts at most `limit` private rows not already on the configured
/// primary at-rest key. Each row is decrypted and public/private checked before
/// its compare-and-swap update, making every bounded invocation safely
/// resumable.
pub async fn rewrap_batch(state: &crate::AppState, limit: usize) -> Result<usize, KeyStoreError> {
    let keyring = state
        .federation_keyring
        .as_deref()
        .ok_or(KeyEncryptionError::MissingConfiguration)?;
    let mut changed = 0usize;
    let limit = i64::try_from(limit.max(1)).unwrap_or(i64::MAX);
    for record in
        actor_key::private_rows_needing_rewrap(&state.pool, keyring.primary_version(), limit)
            .await?
    {
        let old_version = record
            .encryption_key_version
            .ok_or_else(|| KeyStoreError::MissingVersion(record.key_uri.clone()))?;
        let decrypted = decrypt_record(keyring, record)?;
        let encrypted = keyring.encrypt(&decrypted.record.key_uri, decrypted.private.expose());
        if actor_key::rewrap_private(
            &state.pool,
            decrypted.record.id,
            old_version,
            envelope(&encrypted),
        )
        .await?
        {
            changed += 1;
        }
    }
    Ok(changed)
}

/// Rewraps every stale row in bounded pages. Re-running after interruption
/// resumes at the first remaining old-version row and returns zero once done.
pub async fn rewrap_all(state: &crate::AppState) -> Result<usize, KeyStoreError> {
    let mut total = 0usize;
    loop {
        let changed = rewrap_batch(state, REWRAP_BATCH).await?;
        total += changed;
        if changed == 0 {
            return Ok(total);
        }
    }
}

pub async fn audit_private_storage(state: &crate::AppState) -> Result<usize, KeyStoreError> {
    if actor_key::plaintext_private_count(&state.pool).await? != 0 {
        return Err(KeyStoreError::MismatchedMaterial(
            "legacy plaintext columns".into(),
        ));
    }
    let keyring = state
        .federation_keyring
        .as_deref()
        .ok_or(KeyEncryptionError::MissingConfiguration)?;
    let rows = actor_key::private_rows(&state.pool).await?;
    for record in rows.iter().cloned() {
        let ciphertext = record.encrypted_private_key.as_deref().unwrap_or_default();
        if !ciphertext.starts_with("fsk1.")
            || ciphertext.contains("PRIVATE KEY")
            || ciphertext.starts_with("z3u2")
        {
            return Err(KeyStoreError::MismatchedMaterial(record.key_uri));
        }
        decrypt_record(keyring, record)?;
    }
    Ok(rows.len())
}

/// Final expand/backfill/contract step. Operators run this only after every
/// web/worker process is on the normalized-key release and the rollback window
/// is over. The audit verifies every ciphertext and proves the old columns are
/// empty before the database removes them.
pub async fn contract_legacy_private_storage(
    state: &crate::AppState,
) -> Result<bool, KeyStoreError> {
    // The rollback window deliberately retains legacy plaintext for the old
    // binary. Contract is the destructive boundary: re-verify every source,
    // compare-and-clear it, prove the strict plaintext audit, then drop the
    // columns. If interrupted before the DDL, the pass is resumable.
    backfill_and_preflight(
        &state.pool,
        &state.config,
        LegacyPrivateDisposition::ClearVerified,
    )
    .await?;
    audit_private_storage(state).await?;
    let dropped = actor_key::drop_legacy_private_columns(&state.pool).await?;
    audit_private_storage(state).await?;
    Ok(dropped)
}
