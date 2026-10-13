//! A short-lived verification cache keeps legacy password clients usable while
//! cold attempts retain native durable login budgets and the bounded crypto gate.
use super::auth::{Principal, current_owner};
use crate::{AppState, error::ApiError};
use plamenu_db::account::Account;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex, PoisonError},
    time::{Duration, Instant},
};
use zeroize::Zeroizing;
struct Cached {
    principal: Principal,
    at: Instant,
}
static CACHE: LazyLock<Mutex<HashMap<String, Cached>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
pub(super) fn fingerprint(password_hash: &str) -> [u8; 32] {
    Sha256::digest(password_hash.as_bytes()).into()
}
pub(super) async fn authenticate(
    state: &AppState,
    username: &str,
    password: &str,
    owner: Option<Account>,
    ip: Option<std::net::IpAddr>,
) -> Result<Principal, ApiError> {
    let denied = || ApiError::Unauthorized("Wrong username or password".into());
    let context = Zeroizing::new(format!("music-client:{username}:{password}"));
    let key = crate::auth::hash_secret(&context);
    let cached = {
        let cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
        cache
            .get(&key)
            .filter(|row| row.at.elapsed() < Duration::from_secs(60))
            .map(|row| row.principal)
    };
    if let Some(principal) = cached
        && owner
            .as_ref()
            .is_some_and(|owner| owner.id == principal.owner)
        && principal.current(state).await.is_ok()
    {
        return Ok(principal);
    }
    crate::rate_limit::check_email(
        state,
        crate::rate_limit::Bucket::LoginAttemptsEmail,
        username,
    )
    .await?;
    crate::rate_limit::check_music_password_ip(state, ip).await?;
    let ip = ip.map(|ip| ip.to_string());
    let user = crate::auth::authenticate_password(
        state,
        username,
        password.to_owned(),
        ip.as_deref(),
        Some("Subsonic"),
    )
    .await
    .map_err(|_| denied())?;
    if user.otp_required_for_login || owner.is_none_or(|owner| owner.id != user.account_id) {
        return Err(denied());
    }
    current_owner(state, user.account_id).await?;
    let principal = Principal {
        owner: user.account_id,
        can_manage: true,
        key_id: None,
        oauth_id: None,
        password_fingerprint: Some(fingerprint(&user.password_hash)),
    };
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    cache.retain(|_, row| row.at.elapsed() < Duration::from_secs(60));
    if cache.len() >= 1024 {
        cache.clear();
    }
    cache.insert(
        key,
        Cached {
            principal,
            at: Instant::now(),
        },
    );
    Ok(principal)
}
