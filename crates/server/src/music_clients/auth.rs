//! Subsonic password, encoded password, API key and salted-token authentication.
use super::{Key, context, secret_box};
use crate::{AppState, error::ApiError};
use md5::{Digest, Md5};
use plamenu_db::{DbError, account, user};
use time::{Duration, OffsetDateTime};
use zeroize::Zeroizing;

#[derive(Default)]
pub struct Credentials<'a> {
    pub username: &'a str,
    pub password: Option<&'a str>,
    pub api_key: Option<&'a str>,
    pub token: Option<&'a str>,
    pub salt: Option<&'a str>,
}
/// The protocol adapter's principal conveys no general OAuth or listen identity.
#[derive(Clone, Copy)]
pub struct Principal {
    pub owner: i64,
    pub can_manage: bool,
    pub key_id: Option<i64>,
    pub oauth_id: Option<i64>,
    pub(super) password_fingerprint: Option<[u8; 32]>,
}
fn denied() -> ApiError {
    ApiError::Unauthorized("Wrong username or password".into())
}
fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) || !value.is_ascii() {
        return None;
    }
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let high = char::from(pair[0]).to_digit(16)?;
            let low = char::from(pair[1]).to_digit(16)?;
            u8::try_from(high * 16 + low).ok()
        })
        .collect()
}
async fn key_current(state: &AppState, row: &Key) -> Result<bool, ApiError> {
    if row.expires_at <= OffsetDateTime::now_utc() {
        return Ok(false);
    }
    if let Some(token) = row.parent_token_id {
        let scopes:Option<String>=sqlx::query_scalar("SELECT t.scopes FROM oauth_tokens t JOIN users u ON u.id=t.user_id WHERE t.id=$1 AND u.account_id=$2 AND t.revoked_at IS NULL").bind(token).bind(row.account_id).fetch_optional(&state.pool).await.map_err(DbError::from)?;
        return Ok(scopes.is_some_and(|scopes| {
            crate::auth::scopes_satisfy(&scopes, "read:statuses")
                && (!row.can_manage || crate::auth::scopes_satisfy(&scopes, "write:statuses"))
        }));
    }
    Ok(true)
}
async fn accepted(state: &AppState, row: &Key) -> Result<Principal, ApiError> {
    if !key_current(state, row).await? {
        return Err(denied());
    }
    if row
        .last_used_at
        .is_none_or(|used| OffsetDateTime::now_utc() - used >= Duration::minutes(5))
    {
        sqlx::query(
            "UPDATE music_client_keys SET last_used_at=now() WHERE id=$1 AND expires_at>now()",
        )
        .bind(row.id)
        .execute(&state.pool)
        .await
        .map_err(DbError::from)?;
    }
    Ok(Principal {
        owner: row.account_id,
        can_manage: row.can_manage,
        key_id: Some(row.id),
        oauth_id: row.parent_token_id,
        password_fingerprint: None,
    })
}
async fn password_key(state: &AppState, owner: i64, password: &str) -> Result<Principal, ApiError> {
    let row:Option<Key>=sqlx::query_as("SELECT * FROM music_client_keys WHERE account_id=$1 AND token_hash=$2 AND expires_at>now()")
        .bind(owner).bind(crate::auth::hash_secret(password)).fetch_optional(&state.pool).await.map_err(DbError::from)?;
    accepted(state, &row.ok_or_else(denied)?).await
}
async fn salted(
    state: &AppState,
    owner: i64,
    token: &str,
    salt: &str,
) -> Result<Principal, ApiError> {
    if token.len() != 32 || salt.is_empty() || salt.len() > 256 {
        return Err(denied());
    }
    let expected = decode_hex(token).ok_or_else(denied)?;
    let expected =
        md5::digest::Output::<Md5>::try_from(expected.as_slice()).map_err(|_| denied())?;
    let rows:Vec<Key>=sqlx::query_as("SELECT * FROM music_client_keys WHERE account_id=$1 AND expires_at>now() ORDER BY id DESC LIMIT 10").bind(owner).fetch_all(&state.pool).await.map_err(DbError::from)?;
    for row in rows {
        let Ok(boxed) = secret_box(state, row.version) else {
            continue;
        };
        let Some(bytes) =
            boxed.decrypt_with_aad(&row.encrypted_token, context(owner, row.id).as_bytes())
        else {
            continue;
        };
        let bytes = Zeroizing::new(bytes);
        let Ok(password) = std::str::from_utf8(&bytes) else {
            continue;
        };
        if crate::auth::hash_secret(password) != row.token_hash {
            continue;
        }
        let actual = Md5::new()
            .chain_update(&bytes)
            .chain_update(salt.as_bytes())
            .finalize();
        if md5::digest::CtOutput::<Md5>::from(actual)
            == md5::digest::CtOutput::<Md5>::from(expected)
        {
            return accepted(state, &row).await;
        }
    }
    Err(denied())
}
pub async fn authenticate(
    state: &AppState,
    input: &Credentials<'_>,
) -> Result<Principal, ApiError> {
    authenticate_at(state, input, None).await
}
pub async fn authenticate_at(
    state: &AppState,
    input: &Credentials<'_>,
    ip: Option<std::net::IpAddr>,
) -> Result<Principal, ApiError> {
    if input.username.is_empty()
        || input.username.len() > 100
        || input.password.is_some_and(|value| value.len() > 4096)
        || input.api_key.is_some_and(|value| value.len() > 4096)
    {
        return Err(denied());
    }
    let owner = account::find_local_by_username(&state.pool, input.username).await?;
    if input.token.is_some() || input.salt.is_some() {
        let owner = owner.ok_or_else(denied)?;
        current_owner(state, owner.id).await?;
        return salted(
            state,
            owner.id,
            input.token.ok_or_else(denied)?,
            input.salt.ok_or_else(denied)?,
        )
        .await;
    }
    let (password, real_password) = if let Some(key) = input.api_key {
        (Zeroizing::new(key.to_owned()), false)
    } else {
        let raw = input.password.ok_or_else(denied)?;
        let password = if let Some(hex) = raw.strip_prefix("enc:") {
            String::from_utf8(decode_hex(hex).ok_or_else(denied)?).map_err(|_| denied())?
        } else {
            raw.to_owned()
        };
        (Zeroizing::new(password), true)
    };
    if password.starts_with("pmusic_") {
        let owner = owner.ok_or_else(denied)?;
        current_owner(state, owner.id).await?;
        return password_key(state, owner.id, &password).await;
    }
    if let Ok(current) = crate::auth::user_for_token(state, &password).await {
        if owner
            .as_ref()
            .is_none_or(|owner| current.account.id != owner.id)
        {
            return Err(denied());
        }
        current.require_scope("read:statuses")?;
        return Ok(Principal {
            owner: current.account.id,
            can_manage: current.has_scope("write:statuses"),
            key_id: None,
            oauth_id: Some(current.token_id),
            password_fingerprint: None,
        });
    }
    if !real_password {
        return Err(denied());
    }
    super::password::authenticate(state, input.username, &password, owner, ip).await
}

pub(super) async fn current_owner(state: &AppState, owner: i64) -> Result<(), ApiError> {
    let user = user::find_by_account_id(&state.pool, owner)
        .await?
        .ok_or_else(denied)?;
    let account = account::find_by_id(&state.pool, owner)
        .await?
        .ok_or_else(denied)?;
    if !user.confirmed()
        || !user.approved
        || user.disabled
        || account.domain.is_some()
        || account::is_deleted(&state.pool, owner).await?
        || account.suspended()
    {
        return Err(denied());
    }
    if let Some(email) = &user.email {
        crate::instance_policy::ensure_email_login_allowed(&state.pool, email).await?;
    }
    Ok(())
}
impl Principal {
    pub fn require_manage(&self) -> Result<(), ApiError> {
        if self.can_manage {
            Ok(())
        } else {
            Err(ApiError::Forbidden("This music client is read-only".into()))
        }
    }
    /// Used by guarded binary bodies as well as request handlers.
    pub async fn current(&self, state: &AppState) -> Result<(), ApiError> {
        current_owner(state, self.owner).await?;
        if let Some(expected) = self.password_fingerprint {
            let user = user::find_by_account_id(&state.pool, self.owner)
                .await?
                .ok_or_else(denied)?;
            if user.otp_required_for_login
                || super::password::fingerprint(&user.password_hash) != expected
            {
                return Err(denied());
            }
        }
        if let Some(key) = self.key_id {
            let row: Option<Key> =
                sqlx::query_as("SELECT * FROM music_client_keys WHERE id=$1 AND account_id=$2")
                    .bind(key)
                    .bind(self.owner)
                    .fetch_optional(&state.pool)
                    .await
                    .map_err(DbError::from)?;
            if !key_current(state, &row.ok_or_else(denied)?).await? {
                return Err(denied());
            }
        } else if let Some(token) = self.oauth_id {
            let scopes:Option<String>=sqlx::query_scalar("SELECT t.scopes FROM oauth_tokens t JOIN users u ON u.id=t.user_id WHERE t.id=$1 AND u.account_id=$2 AND t.revoked_at IS NULL").bind(token).bind(self.owner).fetch_optional(&state.pool).await.map_err(DbError::from)?;
            if !scopes.is_some_and(|scopes| {
                crate::auth::scopes_satisfy(&scopes, "read:statuses")
                    && (!self.can_manage || crate::auth::scopes_satisfy(&scopes, "write:statuses"))
            }) {
                return Err(denied());
            }
        }
        Ok(())
    }
}
