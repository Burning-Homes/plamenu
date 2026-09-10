//! `WebAuthn` security-key credentials.
//!
//! A credential is stored opaquely as the JSON-serialized webauthn-rs
//! `SecurityKey`; the server layer owns the crypto and only ever hands this
//! module a `serde_json::Value`, so `plamenu-db` stays free of the `WebAuthn`
//! dependency. `external_id` is the base64url credential id (looked up to find
//! which stored key an authentication used); `webauthn_id` on `users` is the
//! stable per-user handle every credential shares.

use serde_json::Value;
use sqlx::PgPool;
use time::OffsetDateTime;

use crate::{DbError, id};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct WebauthnCredential {
    pub id: i64,
    pub user_id: i64,
    pub external_id: String,
    pub nickname: String,
    pub credential: Value,
    pub sign_count: i64,
    pub created_at: OffsetDateTime,
}

/// Returns the user's stable `WebAuthn` handle, minting `candidate` on the first
/// call and returning the existing value on every call after (so all of a
/// user's credentials share one handle, as `WebAuthn` requires).
pub async fn ensure_webauthn_id(
    pool: &PgPool,
    user_id: i64,
    candidate: &str,
) -> Result<String, DbError> {
    let handle = sqlx::query_scalar!(
        r#"
        UPDATE users
        SET webauthn_id = COALESCE(webauthn_id, $2)
        WHERE id = $1
        RETURNING webauthn_id AS "webauthn_id!"
        "#,
        user_id,
        candidate,
    )
    .fetch_one(pool)
    .await?;
    Ok(handle)
}

/// Every registered key for a user, oldest first — the settings list and the
/// source of `exclude`/`allow` credential lists for the ceremonies.
pub async fn list_by_user(pool: &PgPool, user_id: i64) -> Result<Vec<WebauthnCredential>, DbError> {
    let rows = sqlx::query_as!(
        WebauthnCredential,
        r#"
        SELECT id, user_id, external_id, nickname, credential, sign_count, created_at
        FROM webauthn_credentials
        WHERE user_id = $1
        ORDER BY created_at, id
        "#,
        user_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// How many keys a user has registered — drives whether the login challenge
/// offers the security-key path and whether the settings page shows the list.
pub async fn count_by_user(pool: &PgPool, user_id: i64) -> Result<i64, DbError> {
    let count = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM webauthn_credentials WHERE user_id = $1"#,
        user_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// Whether the user already has a key under this nickname (a fast pre-check so
/// the ceremony is not started for a name that would collide on insert).
pub async fn nickname_taken(pool: &PgPool, user_id: i64, nickname: &str) -> Result<bool, DbError> {
    let taken = sqlx::query_scalar!(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM webauthn_credentials WHERE user_id = $1 AND nickname = $2
        ) AS "taken!"
        "#,
        user_id,
        nickname,
    )
    .fetch_one(pool)
    .await?;
    Ok(taken)
}

/// Stores a freshly registered credential. A duplicate nickname surfaces as
/// [`DbError::WebauthnNicknameTaken`]; a duplicate `external_id` (the same
/// physical key re-enrolled) falls through as the raw unique violation — the
/// registration ceremony already excludes known keys, so it should not happen.
pub async fn create(
    pool: &PgPool,
    user_id: i64,
    external_id: &str,
    nickname: &str,
    credential: &Value,
    sign_count: i64,
) -> Result<WebauthnCredential, DbError> {
    let credential = sqlx::query_as!(
        WebauthnCredential,
        r#"
        INSERT INTO webauthn_credentials
            (id, user_id, external_id, nickname, credential, sign_count)
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING id, user_id, external_id, nickname, credential, sign_count, created_at
        "#,
        id::next(),
        user_id,
        external_id,
        nickname,
        credential,
        sign_count,
    )
    .fetch_one(pool)
    .await
    .map_err(|err| match &err {
        sqlx::Error::Database(db)
            if db.constraint() == Some("webauthn_credentials_user_nickname_key") =>
        {
            DbError::WebauthnNicknameTaken
        }
        _ => DbError::Sqlx(err),
    })?;
    Ok(credential)
}

/// Rewrites a credential after a successful authentication (webauthn-rs bumps
/// the signature counter and backup flags inside the serialized `SecurityKey`).
pub async fn update_after_auth(
    pool: &PgPool,
    id: i64,
    credential: &Value,
    sign_count: i64,
) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE webauthn_credentials SET credential = $2, sign_count = $3 WHERE id = $1",
        id,
        credential,
        sign_count,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Removes one of the user's keys (scoped to `user_id` so a session cannot
/// delete another account's credential). Returns whether a row was removed.
pub async fn delete(pool: &PgPool, user_id: i64, id: i64) -> Result<bool, DbError> {
    let deleted = sqlx::query!(
        "DELETE FROM webauthn_credentials WHERE id = $1 AND user_id = $2",
        id,
        user_id,
    )
    .execute(pool)
    .await?;
    Ok(deleted.rows_affected() == 1)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::account::{self, NewLocalAccount};

    async fn seed_user(pool: &PgPool) -> i64 {
        let account = account::create_local(
            pool,
            NewLocalAccount {
                username: "alice",
                display_name: "alice",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap();
        crate::user::create(pool, account.id, Some("alice@example.com"), "hash")
            .await
            .unwrap()
            .id
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn webauthn_id_is_minted_once(pool: PgPool) {
        let user_id = seed_user(&pool).await;
        let first = ensure_webauthn_id(&pool, user_id, "handle-a")
            .await
            .unwrap();
        assert_eq!(first, "handle-a");
        // A later call keeps the original handle rather than replacing it.
        let second = ensure_webauthn_id(&pool, user_id, "handle-b")
            .await
            .unwrap();
        assert_eq!(second, "handle-a");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn crud_and_nickname_uniqueness(pool: PgPool) {
        let user_id = seed_user(&pool).await;
        assert_eq!(count_by_user(&pool, user_id).await.unwrap(), 0);
        assert!(!nickname_taken(&pool, user_id, "Yubikey").await.unwrap());

        let cred = create(
            &pool,
            user_id,
            "ext-1",
            "Yubikey",
            &json!({ "cred": "opaque" }),
            0,
        )
        .await
        .unwrap();
        assert_eq!(count_by_user(&pool, user_id).await.unwrap(), 1);
        assert!(nickname_taken(&pool, user_id, "Yubikey").await.unwrap());

        // A second key under the same nickname is rejected as such.
        let dup = create(
            &pool,
            user_id,
            "ext-2",
            "Yubikey",
            &json!({ "cred": "opaque" }),
            0,
        )
        .await;
        assert!(matches!(dup, Err(DbError::WebauthnNicknameTaken)));

        // The counter update round-trips.
        update_after_auth(&pool, cred.id, &json!({ "cred": "bumped" }), 7)
            .await
            .unwrap();
        let listed = list_by_user(&pool, user_id).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].sign_count, 7);
        assert_eq!(listed[0].credential, json!({ "cred": "bumped" }));

        // Deletion is scoped to the owner.
        assert!(!delete(&pool, user_id + 999, cred.id).await.unwrap());
        assert!(delete(&pool, user_id, cred.id).await.unwrap());
        assert_eq!(count_by_user(&pool, user_id).await.unwrap(), 0);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn disabling_totp_removes_keys(pool: PgPool) {
        let user_id = seed_user(&pool).await;
        create(&pool, user_id, "ext-1", "Key", &json!({}), 0)
            .await
            .unwrap();
        // `disable_otp` clears every second factor, security keys included.
        crate::user::disable_otp(&pool, user_id).await.unwrap();
        assert_eq!(count_by_user(&pool, user_id).await.unwrap(), 0);
    }
}
