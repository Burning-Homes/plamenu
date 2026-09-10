//! Normalized `ActivityPub` signing and verification keys.
//!
//! A key URI is globally unique and permanently bound to one controller and
//! algorithm. Remote refresh can replace a bounded active set while retaining
//! revoked public rows for audit/late-object diagnostics. Private material is
//! accepted only as an opaque, versioned application ciphertext.

use sqlx::{FromRow, PgExecutor, PgPool};
use time::OffsetDateTime;

use crate::{DbError, id};

/// Mastodon independently accepts ten FEP-521a and ten classic key entries.
/// The normalized repository therefore needs room for the mixed maximum.
pub const MAX_REMOTE_KEYS: usize = 20;

#[derive(Debug, Clone, FromRow)]
pub struct ActorKey {
    pub id: i64,
    pub owner_kind: String,
    pub account_id: Option<i64>,
    pub key_uri: String,
    pub controller_uri: String,
    pub algorithm: String,
    pub public_key: String,
    pub encrypted_private_key: Option<String>,
    pub encryption_key_version: Option<i32>,
    pub source: String,
    pub created_at: OffsetDateTime,
    pub activated_at: OffsetDateTime,
    pub expires_at: Option<OffsetDateTime>,
    pub revoked_at: Option<OffsetDateTime>,
    pub retired_at: Option<OffsetDateTime>,
}

impl ActorKey {
    #[must_use]
    pub fn usable_at(&self, now: OffsetDateTime) -> bool {
        self.revoked_at.is_none()
            && self.retired_at.is_none()
            && self.activated_at <= now
            && self.expires_at.is_none_or(|expiry| expiry > now)
    }
}

#[derive(Debug, Clone)]
pub struct NewPublicKey {
    pub key_uri: String,
    pub controller_uri: String,
    pub algorithm: String,
    /// PEM for RSA, `publicKeyMultibase` for Ed25519/ML-DSA-44.
    pub public_key: String,
    pub source: String,
    pub expires_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, Copy)]
pub struct PrivateEnvelope<'a> {
    pub ciphertext: &'a str,
    pub key_version: i32,
}

#[derive(Clone, FromRow)]
pub struct LegacyAccountKeys {
    pub id: i64,
    pub username: String,
    pub uri: Option<String>,
    pub private_key: Option<String>,
    pub public_key: String,
    pub ed25519_private_key: Option<String>,
    pub ed25519_public_key: Option<String>,
}

#[derive(Clone, FromRow)]
pub struct LegacyInstanceKeys {
    pub private_key: String,
    pub public_key: String,
    pub ed25519_private_key: Option<String>,
    pub ed25519_public_key: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
pub struct LegacyRemoteKeys {
    pub id: i64,
    pub uri: String,
    pub public_key: String,
    pub public_key_id: String,
    pub ed25519_public_key: Option<String>,
}

/// Which pre-normalization private-key columns still exist. Deployments may
/// be anywhere in the expand/backfill/contract sequence, so startup and audit
/// code must not issue a query against a column that has already been dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegacyPrivateColumns {
    pub accounts: bool,
    pub instance: bool,
}

impl LegacyPrivateColumns {
    #[must_use]
    pub const fn any(self) -> bool {
        self.accounts || self.instance
    }
}

const COLUMNS: &str = "id, owner_kind, account_id, key_uri, controller_uri, algorithm, \
public_key, encrypted_private_key, encryption_key_version, source, created_at, \
activated_at, expires_at, revoked_at, retired_at";

/// Serializes all key lifecycle changes for one account owner.
pub async fn lock_account_owner(
    conn: &mut sqlx::PgConnection,
    account_id: i64,
) -> Result<bool, DbError> {
    Ok(
        sqlx::query_scalar::<_, i64>("SELECT id FROM accounts WHERE id = $1 FOR UPDATE")
            .bind(account_id)
            .fetch_optional(conn)
            .await?
            .is_some(),
    )
}

/// Serializes instance-actor rotations, which have no account row to lock.
pub async fn lock_instance_owner(conn: &mut sqlx::PgConnection) -> Result<(), DbError> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(0x504c_414d_454e_5549_i64)
        .execute(conn)
        .await?;
    Ok(())
}

/// Inserts a local account key idempotently. A conflicting URI whose
/// controller, algorithm or public material differs returns `None`; callers
/// must fail closed instead of silently replacing a published key.
pub async fn put_local<'e, E: PgExecutor<'e>>(
    executor: E,
    account_id: i64,
    key: &NewPublicKey,
    private: PrivateEnvelope<'_>,
) -> Result<Option<ActorKey>, DbError> {
    let sql = format!(
        "INSERT INTO actor_keys
             (id, owner_kind, account_id, key_uri, controller_uri, algorithm,
              public_key, encrypted_private_key, encryption_key_version, source,
              expires_at)
         VALUES ($1, 'account', $2, $3, $4, $5, $6, $7, $8, $9, $10)
         ON CONFLICT (key_uri) DO UPDATE SET
             encrypted_private_key = actor_keys.encrypted_private_key,
             encryption_key_version = actor_keys.encryption_key_version
         WHERE actor_keys.owner_kind = 'account'
           AND actor_keys.account_id = EXCLUDED.account_id
           AND actor_keys.controller_uri = EXCLUDED.controller_uri
           AND actor_keys.algorithm = EXCLUDED.algorithm
           AND actor_keys.public_key = EXCLUDED.public_key
         RETURNING {COLUMNS}"
    );
    Ok(sqlx::query_as::<_, ActorKey>(sqlx::AssertSqlSafe(sql))
        .bind(id::next())
        .bind(account_id)
        .bind(&key.key_uri)
        .bind(&key.controller_uri)
        .bind(&key.algorithm)
        .bind(&key.public_key)
        .bind(private.ciphertext)
        .bind(private.key_version)
        .bind(&key.source)
        .bind(key.expires_at)
        .fetch_optional(executor)
        .await?)
}

/// Inserts the singleton instance actor's encrypted key.
pub async fn put_instance<'e, E: PgExecutor<'e>>(
    executor: E,
    key: &NewPublicKey,
    private: PrivateEnvelope<'_>,
) -> Result<Option<ActorKey>, DbError> {
    let sql = format!(
        "INSERT INTO actor_keys
             (id, owner_kind, account_id, key_uri, controller_uri, algorithm,
              public_key, encrypted_private_key, encryption_key_version, source,
              expires_at)
         VALUES ($1, 'instance', NULL, $2, $3, $4, $5, $6, $7, $8, $9)
         ON CONFLICT (key_uri) DO UPDATE SET
             encrypted_private_key = actor_keys.encrypted_private_key,
             encryption_key_version = actor_keys.encryption_key_version
         WHERE actor_keys.owner_kind = 'instance'
           AND actor_keys.controller_uri = EXCLUDED.controller_uri
           AND actor_keys.algorithm = EXCLUDED.algorithm
           AND actor_keys.public_key = EXCLUDED.public_key
         RETURNING {COLUMNS}"
    );
    Ok(sqlx::query_as::<_, ActorKey>(sqlx::AssertSqlSafe(sql))
        .bind(id::next())
        .bind(&key.key_uri)
        .bind(&key.controller_uri)
        .bind(&key.algorithm)
        .bind(&key.public_key)
        .bind(private.ciphertext)
        .bind(private.key_version)
        .bind(&key.source)
        .bind(key.expires_at)
        .fetch_optional(executor)
        .await?)
}

/// Replaces a remote actor's active public-key set atomically. The `ActivityPub`
/// layer independently bounds FEP-521a and classic key collections to ten
/// entries each, so at most twenty candidates are accepted. Missing old keys
/// are revoked, not deleted, so a refresh cannot make historical
/// ownership/audit evidence disappear.
pub async fn replace_remote(
    pool: &PgPool,
    account_id: i64,
    controller_uri: &str,
    keys: &[NewPublicKey],
) -> Result<Vec<ActorKey>, DbError> {
    if keys.len() > MAX_REMOTE_KEYS {
        return Err(sqlx::Error::Protocol(format!(
            "remote actor supplied {} keys; limit is {MAX_REMOTE_KEYS}",
            keys.len()
        ))
        .into());
    }
    let mut seen = std::collections::HashSet::with_capacity(keys.len());
    if keys.iter().any(|key| {
        key.controller_uri != controller_uri
            || !seen.insert(key.key_uri.as_str())
            || !matches!(key.algorithm.as_str(), "rsa" | "ed25519" | "ml-dsa-44")
    }) {
        return Err(sqlx::Error::Protocol(
            "conflicting, duplicate, or wrong-controller remote key".to_owned(),
        )
        .into());
    }

    let mut tx = pool.begin().await?;
    // Remote refreshes for one actor must be ordered. Without this lock, two
    // concurrent documents can each insert their own set and then revoke the
    // other's rows, producing a mixed or entirely revoked result.
    let locked: Option<i64> =
        sqlx::query_scalar("SELECT id FROM accounts WHERE id = $1 FOR UPDATE")
            .bind(account_id)
            .fetch_optional(&mut *tx)
            .await?;
    if locked.is_none() {
        return Err(sqlx::Error::RowNotFound.into());
    }
    let mut stored = Vec::with_capacity(keys.len());
    let sql = format!(
        "INSERT INTO actor_keys
             (id, owner_kind, account_id, key_uri, controller_uri, algorithm,
              public_key, source, expires_at)
         VALUES ($1, 'account', $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (key_uri) DO UPDATE SET
             source = EXCLUDED.source,
             expires_at = EXCLUDED.expires_at,
             activated_at = least(actor_keys.activated_at, now()),
             revoked_at = NULL,
             retired_at = NULL
         WHERE actor_keys.owner_kind = 'account'
           AND actor_keys.account_id = EXCLUDED.account_id
           AND actor_keys.controller_uri = EXCLUDED.controller_uri
           AND actor_keys.algorithm = EXCLUDED.algorithm
           AND actor_keys.public_key = EXCLUDED.public_key
           AND actor_keys.encrypted_private_key IS NULL
         RETURNING {COLUMNS}"
    );
    for key in keys {
        let row = sqlx::query_as::<_, ActorKey>(sqlx::AssertSqlSafe(sql.clone()))
            .bind(id::next())
            .bind(account_id)
            .bind(&key.key_uri)
            .bind(&key.controller_uri)
            .bind(&key.algorithm)
            .bind(&key.public_key)
            .bind(&key.source)
            .bind(key.expires_at)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(row) = row else {
            return Err(sqlx::Error::Protocol(format!(
                "key URI {} is already bound to different material or controller",
                key.key_uri
            ))
            .into());
        };
        stored.push(row);
    }
    let active_uris: Vec<&str> = keys.iter().map(|key| key.key_uri.as_str()).collect();
    sqlx::query(
        "UPDATE actor_keys SET revoked_at = coalesce(revoked_at, now())
         WHERE owner_kind = 'account' AND account_id = $1
           AND encrypted_private_key IS NULL
           AND NOT (key_uri = ANY($2::text[]))",
    )
    .bind(account_id)
    .bind(&active_uris)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(stored)
}

pub async fn by_uri(pool: &PgPool, key_uri: &str) -> Result<Option<ActorKey>, DbError> {
    let sql = format!("SELECT {COLUMNS} FROM actor_keys WHERE key_uri = $1");
    Ok(sqlx::query_as::<_, ActorKey>(sqlx::AssertSqlSafe(sql))
        .bind(key_uri)
        .fetch_optional(pool)
        .await?)
}

pub async fn usable_by_uri(pool: &PgPool, key_uri: &str) -> Result<Option<ActorKey>, DbError> {
    let sql = format!(
        "SELECT {COLUMNS} FROM actor_keys
         WHERE key_uri = $1 AND activated_at <= now()
           AND revoked_at IS NULL AND retired_at IS NULL
           AND (expires_at IS NULL OR expires_at > now())"
    );
    Ok(sqlx::query_as::<_, ActorKey>(sqlx::AssertSqlSafe(sql))
        .bind(key_uri)
        .fetch_optional(pool)
        .await?)
}

pub async fn usable_for_account(pool: &PgPool, account_id: i64) -> Result<Vec<ActorKey>, DbError> {
    let sql = format!(
        "SELECT {COLUMNS} FROM actor_keys
         WHERE owner_kind = 'account' AND account_id = $1
           AND activated_at <= now() AND revoked_at IS NULL AND retired_at IS NULL
           AND (expires_at IS NULL OR expires_at > now())
         ORDER BY algorithm, (expires_at IS NULL) DESC, activated_at DESC, id DESC"
    );
    Ok(sqlx::query_as::<_, ActorKey>(sqlx::AssertSqlSafe(sql))
        .bind(account_id)
        .fetch_all(pool)
        .await?)
}

/// Keys an actor document must advertise. A pending rotation is deliberately
/// published before its `activated_at` instant so peers can ingest the new
/// verification method while outbound signing still uses the previous key.
pub async fn published_for_account<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
) -> Result<Vec<ActorKey>, DbError> {
    let sql = format!(
        "SELECT {COLUMNS} FROM actor_keys
         WHERE owner_kind = 'account' AND account_id = $1
           AND revoked_at IS NULL AND retired_at IS NULL
           AND (expires_at IS NULL OR expires_at > now())
         ORDER BY algorithm, activated_at DESC, id DESC"
    );
    Ok(sqlx::query_as::<_, ActorKey>(sqlx::AssertSqlSafe(sql))
        .bind(account_id)
        .fetch_all(pool)
        .await?)
}

/// Moves a newly-inserted private key's signing activation into the future.
/// The insert and this update are performed under the same owner lock and
/// transaction, so no worker can observe the transient immediate activation.
pub async fn schedule_activation<'e, E: PgExecutor<'e>>(
    executor: E,
    key_id: i64,
    activated_at: OffsetDateTime,
) -> Result<ActorKey, DbError> {
    let sql = format!(
        "UPDATE actor_keys SET activated_at = $2
         WHERE id = $1 AND encrypted_private_key IS NOT NULL
         RETURNING {COLUMNS}"
    );
    Ok(sqlx::query_as::<_, ActorKey>(sqlx::AssertSqlSafe(sql))
        .bind(key_id)
        .bind(activated_at)
        .fetch_one(executor)
        .await?)
}

pub async fn usable_for_instance<'e, E: PgExecutor<'e>>(
    executor: E,
) -> Result<Vec<ActorKey>, DbError> {
    let sql = format!(
        "SELECT {COLUMNS} FROM actor_keys
         WHERE owner_kind = 'instance' AND activated_at <= now()
           AND revoked_at IS NULL AND retired_at IS NULL
           AND (expires_at IS NULL OR expires_at > now())
         ORDER BY algorithm, (expires_at IS NULL) DESC, activated_at DESC, id DESC"
    );
    Ok(sqlx::query_as::<_, ActorKey>(sqlx::AssertSqlSafe(sql))
        .fetch_all(executor)
        .await?)
}

/// Instance-actor counterpart of [`published_for_account`].
pub async fn published_for_instance<'e, E: PgExecutor<'e>>(
    executor: E,
) -> Result<Vec<ActorKey>, DbError> {
    let sql = format!(
        "SELECT {COLUMNS} FROM actor_keys
         WHERE owner_kind = 'instance'
           AND revoked_at IS NULL AND retired_at IS NULL
           AND (expires_at IS NULL OR expires_at > now())
         ORDER BY algorithm, activated_at DESC, id DESC"
    );
    Ok(sqlx::query_as::<_, ActorKey>(sqlx::AssertSqlSafe(sql))
        .fetch_all(executor)
        .await?)
}

pub async fn revoke(pool: &PgPool, key_uri: &str) -> Result<bool, DbError> {
    Ok(sqlx::query(
        "UPDATE actor_keys SET revoked_at = coalesce(revoked_at, now())
         WHERE key_uri = $1 AND revoked_at IS NULL",
    )
    .bind(key_uri)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

pub async fn retire(pool: &PgPool, key_uri: &str) -> Result<bool, DbError> {
    Ok(sqlx::query(
        "UPDATE actor_keys SET retired_at = coalesce(retired_at, now())
         WHERE key_uri = $1 AND retired_at IS NULL",
    )
    .bind(key_uri)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

pub async fn expire_other_private_keys<'e, E: PgExecutor<'e>>(
    executor: E,
    owner_kind: &str,
    account_id: Option<i64>,
    algorithm: &str,
    except_key_uri: &str,
    expires_at: OffsetDateTime,
) -> Result<u64, DbError> {
    Ok(sqlx::query(
        "UPDATE actor_keys
         SET expires_at = CASE
             WHEN expires_at IS NULL OR expires_at > $5 THEN $5
             ELSE expires_at END
         WHERE owner_kind = $1
           AND account_id IS NOT DISTINCT FROM $2
           AND algorithm = $3 AND key_uri <> $4
           AND encrypted_private_key IS NOT NULL
           AND revoked_at IS NULL AND retired_at IS NULL",
    )
    .bind(owner_kind)
    .bind(account_id)
    .bind(algorithm)
    .bind(except_key_uri)
    .bind(expires_at)
    .execute(executor)
    .await?
    .rows_affected())
}

pub async fn rewrap_private(
    pool: &PgPool,
    key_id: i64,
    old_version: i32,
    private: PrivateEnvelope<'_>,
) -> Result<bool, DbError> {
    Ok(sqlx::query(
        "UPDATE actor_keys
         SET encrypted_private_key = $3, encryption_key_version = $4
         WHERE id = $1 AND encryption_key_version = $2
           AND encrypted_private_key IS NOT NULL",
    )
    .bind(key_id)
    .bind(old_version)
    .bind(private.ciphertext)
    .bind(private.key_version)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

pub async fn private_rows(pool: &PgPool) -> Result<Vec<ActorKey>, DbError> {
    let sql = format!(
        "SELECT {COLUMNS} FROM actor_keys WHERE encrypted_private_key IS NOT NULL ORDER BY id"
    );
    Ok(sqlx::query_as::<_, ActorKey>(sqlx::AssertSqlSafe(sql))
        .fetch_all(pool)
        .await?)
}

/// A bounded page of private rows still encrypted by a non-primary root.
/// Rewrap workers repeatedly call this from the beginning because each
/// successful CAS removes the row from the result set; interruption therefore
/// needs no cursor checkpoint and concurrent rewrappers cannot lose work.
pub async fn private_rows_needing_rewrap(
    pool: &PgPool,
    primary_version: i32,
    limit: i64,
) -> Result<Vec<ActorKey>, DbError> {
    let sql = format!(
        "SELECT {COLUMNS} FROM actor_keys
         WHERE encrypted_private_key IS NOT NULL
           AND encryption_key_version <> $1
         ORDER BY id LIMIT $2"
    );
    Ok(sqlx::query_as::<_, ActorKey>(sqlx::AssertSqlSafe(sql))
        .bind(primary_version)
        .bind(limit)
        .fetch_all(pool)
        .await?)
}

pub async fn legacy_private_columns(pool: &PgPool) -> Result<LegacyPrivateColumns, DbError> {
    let accounts: bool = sqlx::query_scalar(
        "SELECT count(*) = 2
         FROM information_schema.columns
         WHERE table_schema = current_schema() AND table_name = 'accounts'
           AND column_name IN ('private_key', 'ed25519_private_key')",
    )
    .fetch_one(pool)
    .await?;
    let instance: bool = sqlx::query_scalar(
        "SELECT count(*) = 2
         FROM information_schema.columns
         WHERE table_schema = current_schema() AND table_name = 'instance_actor_keys'
           AND column_name IN ('private_key', 'ed25519_private_key')",
    )
    .fetch_one(pool)
    .await?;
    Ok(LegacyPrivateColumns { accounts, instance })
}

/// Plaintext sources awaiting verified encryption backfill. Empty strings are
/// already-cleared legacy instance placeholders and are omitted.
pub async fn legacy_account_rows_after(
    pool: &PgPool,
    after_id: i64,
    limit: i64,
) -> Result<Vec<LegacyAccountKeys>, DbError> {
    Ok(sqlx::query_as::<_, LegacyAccountKeys>(
        "SELECT id, username, uri, private_key, public_key,
                ed25519_private_key, ed25519_public_key
         FROM accounts
         WHERE domain IS NULL AND id > $1
           AND (private_key IS NOT NULL OR ed25519_private_key IS NOT NULL)
         ORDER BY id LIMIT $2",
    )
    .bind(after_id)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

pub async fn legacy_instance_row(pool: &PgPool) -> Result<Option<LegacyInstanceKeys>, DbError> {
    Ok(sqlx::query_as::<_, LegacyInstanceKeys>(
        "SELECT private_key, public_key, ed25519_private_key, ed25519_public_key
         FROM instance_actor_keys WHERE id = 1 AND private_key <> ''",
    )
    .fetch_optional(pool)
    .await?)
}

pub async fn legacy_remote_rows_after(
    pool: &PgPool,
    after_id: i64,
    limit: i64,
) -> Result<Vec<LegacyRemoteKeys>, DbError> {
    Ok(sqlx::query_as::<_, LegacyRemoteKeys>(
        "SELECT id, uri, public_key, public_key_id, ed25519_public_key
         FROM accounts
         WHERE domain IS NOT NULL AND uri IS NOT NULL AND id > $1
           AND NOT EXISTS (
               SELECT 1 FROM actor_keys k
               WHERE k.owner_kind = 'account' AND k.account_id = accounts.id)
         ORDER BY id LIMIT $2",
    )
    .bind(after_id)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

/// Clears an account's plaintext only if it still equals the material that was
/// verified and encrypted. Concurrent changes therefore cannot be erased.
pub async fn clear_legacy_account_private(
    pool: &PgPool,
    account_id: i64,
    rsa_private: Option<&str>,
    ed25519_private: Option<&str>,
) -> Result<bool, DbError> {
    Ok(sqlx::query(
        "UPDATE accounts SET private_key = NULL, ed25519_private_key = NULL
         WHERE id = $1 AND domain IS NULL
           AND private_key IS NOT DISTINCT FROM $2
           AND ed25519_private_key IS NOT DISTINCT FROM $3",
    )
    .bind(account_id)
    .bind(rsa_private)
    .bind(ed25519_private)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

pub async fn clear_legacy_instance_private(
    pool: &PgPool,
    rsa_private: &str,
    ed25519_private: Option<&str>,
) -> Result<bool, DbError> {
    Ok(sqlx::query(
        "UPDATE instance_actor_keys
         SET private_key = '', ed25519_private_key = NULL
         WHERE id = 1 AND private_key = $1
           AND ed25519_private_key IS NOT DISTINCT FROM $2",
    )
    .bind(rsa_private)
    .bind(ed25519_private)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

pub async fn plaintext_private_count(pool: &PgPool) -> Result<i64, DbError> {
    let columns = legacy_private_columns(pool).await?;
    let account_count: i64 = if columns.accounts {
        sqlx::query_scalar(
            "SELECT count(*) FROM accounts
             WHERE private_key IS NOT NULL OR ed25519_private_key IS NOT NULL",
        )
        .fetch_one(pool)
        .await?
    } else {
        0
    };
    let instance_count: i64 = if columns.instance {
        sqlx::query_scalar(
            "SELECT count(*) FROM instance_actor_keys
             WHERE private_key <> '' OR ed25519_private_key IS NOT NULL",
        )
        .fetch_one(pool)
        .await?
    } else {
        0
    };
    Ok(account_count + instance_count)
}

/// Contract step, deliberately operator-triggered after the rolling-deploy
/// rollback window. It refuses to destroy the source columns while even one
/// usable plaintext value remains. All current writers are column-agnostic,
/// so normal account creation continues after this succeeds.
pub async fn drop_legacy_private_columns(pool: &PgPool) -> Result<bool, DbError> {
    let columns = legacy_private_columns(pool).await?;
    if !columns.any() {
        return Ok(false);
    }
    if plaintext_private_count(pool).await? != 0 {
        return Err(sqlx::Error::Protocol(
            "refusing to drop non-empty legacy private-key columns".to_owned(),
        )
        .into());
    }
    let mut tx = pool.begin().await?;
    if columns.accounts {
        sqlx::query(
            "ALTER TABLE accounts
             DROP COLUMN private_key,
             DROP COLUMN ed25519_private_key",
        )
        .execute(&mut *tx)
        .await?;
    }
    if columns.instance {
        sqlx::query(
            "ALTER TABLE instance_actor_keys
             DROP COLUMN private_key,
             DROP COLUMN ed25519_private_key",
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_predicate_honours_boundaries() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let mut key = ActorKey {
            id: 1,
            owner_kind: "account".into(),
            account_id: Some(1),
            key_uri: "https://example/actor#key".into(),
            controller_uri: "https://example/actor".into(),
            algorithm: "ed25519".into(),
            public_key: "uAA".into(),
            encrypted_private_key: None,
            encryption_key_version: None,
            source: "multikey".into(),
            created_at: now,
            activated_at: now,
            expires_at: None,
            revoked_at: None,
            retired_at: None,
        };
        assert!(key.usable_at(now));
        key.expires_at = Some(now);
        assert!(!key.usable_at(now));
        key.expires_at = None;
        key.revoked_at = Some(now);
        assert!(!key.usable_at(now));
    }

    #[sqlx::test]
    async fn remote_keys_are_bounded_exact_and_lifecycle_aware(pool: PgPool) {
        sqlx::query(
            "INSERT INTO accounts (id, username, domain, public_key, uri)
             VALUES (1, 'alice', 'remote.example', '', 'https://remote.example/actors/alice')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let controller = "https://remote.example/actors/alice";
        let keys: Vec<NewPublicKey> = (0..MAX_REMOTE_KEYS)
            .map(|index| NewPublicKey {
                key_uri: format!("{controller}#key-{index}"),
                controller_uri: controller.to_owned(),
                algorithm: if index % 2 == 0 { "ed25519" } else { "rsa" }.to_owned(),
                public_key: format!("public-{index}"),
                source: "multikey".to_owned(),
                expires_at: None,
            })
            .collect();
        assert_eq!(
            replace_remote(&pool, 1, controller, &keys)
                .await
                .unwrap()
                .len(),
            MAX_REMOTE_KEYS
        );
        assert_eq!(
            usable_for_account(&pool, 1).await.unwrap().len(),
            MAX_REMOTE_KEYS
        );

        let mut excessive = keys.clone();
        excessive.push(NewPublicKey {
            key_uri: format!("{controller}#too-many"),
            controller_uri: controller.to_owned(),
            algorithm: "ml-dsa-44".to_owned(),
            public_key: "public-over-limit".to_owned(),
            source: "multikey-external".to_owned(),
            expires_at: None,
        });
        assert!(
            replace_remote(&pool, 1, controller, &excessive)
                .await
                .is_err()
        );
        assert_eq!(
            usable_for_account(&pool, 1).await.unwrap().len(),
            MAX_REMOTE_KEYS
        );

        let exact = &keys[0].key_uri;
        assert!(usable_by_uri(&pool, exact).await.unwrap().is_some());
        assert!(revoke(&pool, exact).await.unwrap());
        assert!(usable_by_uri(&pool, exact).await.unwrap().is_none());
        assert!(
            by_uri(&pool, exact)
                .await
                .unwrap()
                .unwrap()
                .revoked_at
                .is_some()
        );
    }

    #[sqlx::test]
    async fn concurrent_remote_refreshes_leave_one_complete_active_set(pool: PgPool) {
        sqlx::query(
            "INSERT INTO accounts (id, username, domain, public_key, uri)
             VALUES (1, 'alice', 'remote.example', '', 'https://remote.example/actors/alice')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let controller = "https://remote.example/actors/alice";
        let first = vec![NewPublicKey {
            key_uri: format!("{controller}#first"),
            controller_uri: controller.to_owned(),
            algorithm: "ed25519".to_owned(),
            public_key: "first-public".to_owned(),
            source: "multikey".to_owned(),
            expires_at: None,
        }];
        let second = vec![NewPublicKey {
            key_uri: format!("{controller}#second"),
            controller_uri: controller.to_owned(),
            algorithm: "ml-dsa-44".to_owned(),
            public_key: "second-public".to_owned(),
            source: "multikey".to_owned(),
            expires_at: None,
        }];
        let (a, b) = tokio::join!(
            replace_remote(&pool, 1, controller, &first),
            replace_remote(&pool, 1, controller, &second),
        );
        a.unwrap();
        b.unwrap();
        let usable = usable_for_account(&pool, 1).await.unwrap();
        assert_eq!(usable.len(), 1);
        assert!(matches!(
            usable[0].key_uri.as_str(),
            "https://remote.example/actors/alice#first"
                | "https://remote.example/actors/alice#second"
        ));
        let historical =
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM actor_keys WHERE account_id = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(historical, 2, "the losing set is revoked, not erased");
    }

    #[sqlx::test]
    async fn contract_refuses_plaintext_then_physically_drops_empty_columns(pool: PgPool) {
        let before = legacy_private_columns(&pool).await.unwrap();
        assert_eq!(
            before,
            LegacyPrivateColumns {
                accounts: true,
                instance: true
            }
        );
        sqlx::query(
            "INSERT INTO accounts (id, username, private_key, public_key, uri)
             VALUES (1, 'legacy', 'plaintext-secret', 'public', 'https://local/legacy')",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(plaintext_private_count(&pool).await.unwrap(), 1);
        assert!(drop_legacy_private_columns(&pool).await.is_err());
        sqlx::query("UPDATE accounts SET private_key = NULL WHERE id = 1")
            .execute(&pool)
            .await
            .unwrap();

        assert!(drop_legacy_private_columns(&pool).await.unwrap());
        assert_eq!(
            legacy_private_columns(&pool).await.unwrap(),
            LegacyPrivateColumns {
                accounts: false,
                instance: false
            }
        );
        assert_eq!(plaintext_private_count(&pool).await.unwrap(), 0);
        assert!(!drop_legacy_private_columns(&pool).await.unwrap());

        // Current account writers do not name either dropped column.
        let account = crate::account::create_local(
            &pool,
            crate::account::NewLocalAccount {
                username: "modern",
                display_name: "Modern",
                note: "",
                public_key_pem: "public-only",
            },
        )
        .await
        .unwrap();
        assert_eq!(account.username, "modern");
    }
}
