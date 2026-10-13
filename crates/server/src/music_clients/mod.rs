//! Scoped native music-client credentials, independent of bearer API identity.
pub mod auth;
mod password;
use crate::{AppState, crypto::SecretBox, error::ApiError};
use plamenu_db::{DbError, id};
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

#[derive(sqlx::FromRow, Serialize)]
pub struct Key {
    #[serde(serialize_with = "crate::music_remote::serialize_id")]
    pub id: i64,
    #[serde(skip)]
    pub account_id: i64,
    pub label: String,
    pub can_manage: bool,
    #[serde(default)]
    pub broadcast_playing: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_used_at: Option<OffsetDateTime>,
    #[serde(skip)]
    encrypted_token: String,
    #[serde(skip)]
    token_hash: String,
    #[serde(skip)]
    #[sqlx(rename = "key_version")]
    version: i32,
    #[serde(skip)]
    parent_token_id: Option<i64>,
}
#[derive(Deserialize)]
pub struct Request {
    pub label: String,
    #[serde(default)]
    pub can_manage: bool,
    #[serde(default)]
    pub broadcast_playing: bool,
    pub days: i64,
}
#[derive(Serialize)]
pub struct Issued {
    pub key: Key,
    pub password: String,
}
fn secret_box(state: &AppState, version: i32) -> Result<SecretBox, ApiError> {
    let config = &state.config;
    let secret = if version == config.encryption_secret_version {
        config.encryption_secret.as_ref()
    } else {
        config
            .encryption_previous_secrets
            .iter()
            .find(|(key, _)| *key == version)
            .map(|(_, secret)| secret)
    };
    Ok(SecretBox::new(
        secret
            .ok_or_else(|| {
                ApiError::Unprocessable(
                    "Music client credentials need a configured encryption key".into(),
                )
            })?
            .expose(),
        b"plamenu:music-client-key",
    ))
}
fn context(owner: i64, key: i64) -> String {
    format!("music-client:{owner}:{key}")
}
pub async fn create(
    state: &AppState,
    owner: i64,
    parent: Option<i64>,
    input: &Request,
) -> Result<Issued, ApiError> {
    if input.label.trim().is_empty()
        || input.label.chars().count() > 100
        || input.label.contains('\0')
        || !(1..=365).contains(&input.days)
        || (input.broadcast_playing && !input.can_manage)
    {
        return Err(ApiError::Unprocessable(
            "Invalid music client credential".into(),
        ));
    }
    auth::current_owner(state, owner).await?;
    let key = id::next();
    let password = format!("pmusic_{}", crate::auth::generate_secret());
    let version = state.config.encryption_secret_version;
    let encrypted = secret_box(state, version)?
        .encrypt_with_aad(password.as_bytes(), context(owner, key).as_bytes());
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    crate::music_assets::lock_owner(&mut tx, owner).await?;
    crate::music_queues::active_owner(&mut tx, owner).await?;
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM music_client_keys WHERE account_id=$1 AND expires_at>now()",
    )
    .bind(owner)
    .fetch_one(&mut *tx)
    .await
    .map_err(DbError::from)?;
    if count >= 10 {
        return Err(ApiError::Unprocessable(
            "Too many active music client credentials".into(),
        ));
    }
    if let Some(parent) = parent {
        let scopes:Option<String>=sqlx::query_scalar("SELECT t.scopes FROM oauth_tokens t JOIN users u ON u.id=t.user_id WHERE t.id=$1 AND u.account_id=$2 AND t.revoked_at IS NULL").bind(parent).bind(owner).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
        let scopes =
            scopes.ok_or_else(|| ApiError::Unauthorized("Invalid authorizing token".into()))?;
        if !crate::auth::scopes_satisfy(&scopes, "read:statuses")
            || (input.can_manage && !crate::auth::scopes_satisfy(&scopes, "write:statuses"))
        {
            return Err(ApiError::Forbidden(
                "Music client permissions exceed the authorizing token".into(),
            ));
        }
    }
    let row=sqlx::query_as("INSERT INTO music_client_keys(id,account_id,label,token_hash,encrypted_token,key_version,can_manage,parent_token_id,expires_at,broadcast_playing) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) RETURNING *")
        .bind(key).bind(owner).bind(input.label.trim()).bind(crate::auth::hash_secret(&password)).bind(encrypted).bind(version).bind(input.can_manage).bind(parent).bind(OffsetDateTime::now_utc()+Duration::days(input.days)).bind(input.broadcast_playing).fetch_one(&mut *tx).await.map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(Issued { key: row, password })
}
pub async fn list(state: &AppState, owner: i64) -> Result<Vec<Key>, ApiError> {
    auth::current_owner(state, owner).await?;
    let mut conn = state.pool.acquire().await.map_err(DbError::from)?;
    crate::music_queues::active_owner(&mut conn, owner).await?;
    Ok(sqlx::query_as(
        "SELECT * FROM music_client_keys WHERE account_id=$1 ORDER BY id DESC LIMIT 100",
    )
    .bind(owner)
    .fetch_all(&mut *conn)
    .await
    .map_err(DbError::from)?)
}
pub async fn revoke(state: &AppState, owner: i64, key: i64) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    crate::music_assets::lock_owner(&mut tx, owner).await?;
    crate::music_queues::active_owner(&mut tx, owner).await?;
    let deleted = sqlx::query("DELETE FROM music_client_keys WHERE id=$1 AND account_id=$2")
        .bind(key)
        .bind(owner)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?
        .rows_affected();
    if deleted == 0 {
        return Err(ApiError::NotFound);
    }
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}

impl Drop for Issued {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.password.zeroize();
    }
}
