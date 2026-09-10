//! Runtime enforcement helpers for the instance-policy records exposed by the
//! admin API.

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;
use plamenu_db::account::Account;
use plamenu_db::{PgPool, instance_policy};
use sha2::{Digest, Sha256};
use url::Url;

use crate::AppState;
use crate::error::ApiError;

fn validation(message: &str) -> ApiError {
    ApiError::Unprocessable(format!("Validation failed: {message}"))
}

#[must_use]
pub(crate) fn sha256_hex(value: &str) -> String {
    use std::fmt::Write;
    Sha256::digest(value.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut acc, byte| {
            let _ = write!(acc, "{byte:02x}");
            acc
        })
}

pub(crate) fn normalize_domain(raw: Option<&str>) -> Result<String, ApiError> {
    let trimmed = raw.unwrap_or_default().trim().trim_end_matches('/');
    if trimmed.is_empty()
        || trimmed.contains('/')
        || trimmed.contains('@')
        || trimmed.contains('?')
        || trimmed.contains('#')
    {
        return Err(validation("Domain is invalid"));
    }
    let parsed =
        Url::parse(&format!("https://{trimmed}/")).map_err(|_| validation("Domain is invalid"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| validation("Domain is invalid"))?;
    let normalized = match parsed.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    };
    if normalized.is_empty() {
        Err(validation("Domain is invalid"))
    } else {
        Ok(normalized)
    }
}

#[must_use]
pub(crate) fn domain_from_url(raw: &str) -> Option<String> {
    let parsed = Url::parse(raw).ok()?;
    let host = parsed.host_str()?;
    Some(match parsed.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    })
}

#[must_use]
pub(crate) fn domain_from_email(email: &str) -> Option<String> {
    let (_, domain) = email.trim().rsplit_once('@')?;
    let domain = domain.trim().to_ascii_lowercase();
    (!domain.is_empty()).then_some(domain)
}

pub(crate) fn canonical_email(email: &str) -> Result<String, ApiError> {
    let lower = email.to_lowercase();
    let (local, domain) = lower
        .split_once('@')
        .ok_or_else(|| validation("Email is invalid"))?;
    let local = local.replace('.', "");
    let local = local.split('+').next().unwrap_or_default();
    if local.is_empty() || domain.is_empty() {
        return Err(validation("Email is invalid"));
    }
    Ok(format!("{local}@{domain}"))
}

pub(crate) fn canonical_email_hash(email: &str) -> Result<String, ApiError> {
    Ok(sha256_hex(&canonical_email(email)?))
}

fn canonical_email_hash_for_login(email: &str) -> Option<String> {
    canonical_email_hash(email).ok()
}

pub async fn can_federate_domain(
    pool: &PgPool,
    local_domain: &str,
    remote_domain: &str,
) -> Result<bool, plamenu_db::DbError> {
    let normalized = normalize_domain(Some(remote_domain))
        .unwrap_or_else(|_| remote_domain.trim().to_ascii_lowercase());
    if normalized.eq_ignore_ascii_case(local_domain) {
        return Ok(true);
    }
    instance_policy::domain_allows_federation(pool, &normalized).await
}

pub async fn can_federate_url(
    pool: &PgPool,
    local_domain: &str,
    url: &str,
) -> Result<bool, plamenu_db::DbError> {
    let Some(domain) = domain_from_url(url) else {
        return Ok(true);
    };
    can_federate_domain(pool, local_domain, &domain).await
}

pub async fn account_visible(
    pool: &PgPool,
    local_domain: &str,
    account: &Account,
) -> Result<bool, plamenu_db::DbError> {
    if account.is_portable_on(local_domain) {
        return Ok(!account.suspended());
    }
    match account.domain.as_deref() {
        Some(domain) => can_federate_domain(pool, local_domain, domain).await,
        None => Ok(true),
    }
}

/// Account visibility for identity-resolution entry points. Rendering code
/// that already received a published actor keeps using [`account_visible`];
/// keeping activation here avoids adding a per-author query to status pages.
pub async fn public_account_visible(
    pool: &PgPool,
    local_domain: &str,
    account: &Account,
) -> Result<bool, plamenu_db::DbError> {
    if account.is_local()
        && !account.is_group()
        && !plamenu_db::account::is_publicly_available(pool, account.id).await?
    {
        return Ok(false);
    }
    account_visible(pool, local_domain, account).await
}

pub async fn account_id_visible(
    pool: &PgPool,
    account_id: i64,
) -> Result<bool, plamenu_db::DbError> {
    instance_policy::account_domain_allows_federation(pool, account_id).await
}

fn parse_cidr(value: &str) -> Option<(IpAddr, u8)> {
    let (addr, prefix) = value.split_once('/')?;
    let addr: IpAddr = addr.parse().ok()?;
    let prefix: u8 = prefix.parse().ok()?;
    let max = if addr.is_ipv4() { 32 } else { 128 };
    (prefix <= max).then_some((addr, prefix))
}

#[must_use]
pub(crate) fn ip_matches_cidr(ip: IpAddr, cidr: &str) -> bool {
    let Some((network, prefix)) = parse_cidr(cidr) else {
        return false;
    };
    match (ip, network) {
        (IpAddr::V4(ip), IpAddr::V4(network)) => {
            let mask = if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - u32::from(prefix))
            };
            (u32::from(ip) & mask) == (u32::from(network) & mask)
        }
        (IpAddr::V6(ip), IpAddr::V6(network)) => {
            let mask = if prefix == 0 {
                0
            } else {
                u128::MAX << (128 - u32::from(prefix))
            };
            (u128::from(ip) & mask) == (u128::from(network) & mask)
        }
        _ => false,
    }
}

pub async fn ip_denies_access(
    pool: &PgPool,
    remote_ip: Option<IpAddr>,
) -> Result<bool, plamenu_db::DbError> {
    let Some(remote_ip) = remote_ip else {
        return Ok(false);
    };
    let blocks = instance_policy::active_ip_blocks(pool).await?;
    Ok(blocks
        .iter()
        .any(|block| block.severity == "no_access" && ip_matches_cidr(remote_ip, &block.ip)))
}

pub async fn ensure_ip_login_allowed(
    pool: &PgPool,
    remote_ip: Option<IpAddr>,
) -> Result<(), ApiError> {
    if ip_denies_access(pool, remote_ip).await? {
        Err(ApiError::Forbidden("This IP address is blocked".into()))
    } else {
        Ok(())
    }
}

pub async fn ensure_email_login_allowed(pool: &PgPool, email: &str) -> Result<(), ApiError> {
    if let Some(domain) = domain_from_email(email)
        && instance_policy::find_email_domain_block_by_domain(pool, &domain)
            .await?
            .is_some()
    {
        return Err(ApiError::Forbidden("This e-mail address is blocked".into()));
    }
    if let Some(hash) = canonical_email_hash_for_login(email)
        && instance_policy::find_canonical_email_block_by_hash(pool, &hash)
            .await?
            .is_some()
    {
        return Err(ApiError::Forbidden("This e-mail address is blocked".into()));
    }
    Ok(())
}

#[must_use]
pub(crate) fn ip_from_connect_info(
    connect_info: Option<&ConnectInfo<SocketAddr>>,
) -> Option<IpAddr> {
    connect_info.map(|info| info.0.ip())
}

/// The real client address behind the configured trusted proxies — Rails'
/// `remote_ip` semantics. When the TCP peer is a trusted proxy (loopback only
/// by default; deployments behind a proxy list its address explicitly), the
/// `X-Forwarded-For` chain is walked right to left
/// and the first untrusted hop wins; a header sent by an untrusted peer is
/// ignored outright, so clients cannot spoof their address. When every hop is
/// trusted (requests originating on the proxy host itself), the leftmost
/// forwarded entry — or failing that the peer — is used.
#[must_use]
pub(crate) fn client_ip(
    headers: &axum::http::HeaderMap,
    peer: Option<IpAddr>,
    trusted_proxies: &[String],
) -> Option<IpAddr> {
    let peer = peer?;
    let trusted = |ip: IpAddr| trusted_proxies.iter().any(|cidr| ip_matches_cidr(ip, cidr));
    if !trusted(peer) {
        return Some(peer);
    }
    let forwarded: Vec<IpAddr> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|entry| entry.trim().parse().ok())
        .collect();
    forwarded
        .iter()
        .rev()
        .find(|ip| !trusted(**ip))
        .or_else(|| forwarded.first())
        .copied()
        .or(Some(peer))
}

pub struct RemoteIp(pub Option<IpAddr>);

impl FromRequestParts<AppState> for RemoteIp {
    type Rejection = Infallible;

    fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> impl std::future::Future<Output = Result<Self, Self::Rejection>> {
        std::future::ready(Ok(Self(client_ip(
            &parts.headers,
            ip_from_connect_info(parts.extensions.get::<ConnectInfo<SocketAddr>>()),
            &state.config.trusted_proxies,
        ))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_email_matches_admin_api() {
        assert_eq!(
            canonical_email("First.Last+tag@Example.COM").unwrap(),
            "firstlast@example.com"
        );
    }

    #[test]
    fn client_ip_walks_forwarded_chain_from_trusted_peer() {
        let trusted = vec!["127.0.0.0/8".to_owned(), "10.0.0.0/8".to_owned()];
        let peer: IpAddr = "127.0.0.1".parse().unwrap();
        let mut headers = axum::http::HeaderMap::new();

        // No forwarded header: all hops trusted, fall back to the peer.
        assert_eq!(client_ip(&headers, Some(peer), &trusted), Some(peer));

        // Client, then an internal hop: the rightmost untrusted entry wins.
        headers.insert("x-forwarded-for", "203.0.113.7, 10.0.0.2".parse().unwrap());
        assert_eq!(
            client_ip(&headers, Some(peer), &trusted),
            "203.0.113.7".parse().ok()
        );

        // A spoofed extra hop appended by the client is skipped: the entry
        // closest to us that is untrusted is the real client.
        headers.insert(
            "x-forwarded-for",
            "198.51.100.9, 203.0.113.7".parse().unwrap(),
        );
        assert_eq!(
            client_ip(&headers, Some(peer), &trusted),
            "203.0.113.7".parse().ok()
        );

        // Entirely trusted chain: leftmost forwarded entry.
        headers.insert("x-forwarded-for", "10.0.0.3, 10.0.0.2".parse().unwrap());
        assert_eq!(
            client_ip(&headers, Some(peer), &trusted),
            "10.0.0.3".parse().ok()
        );
    }

    #[test]
    fn client_ip_ignores_header_from_untrusted_peer() {
        let trusted = vec!["127.0.0.0/8".to_owned()];
        let peer: IpAddr = "203.0.113.50".parse().unwrap();
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-forwarded-for", "198.51.100.9".parse().unwrap());
        assert_eq!(client_ip(&headers, Some(peer), &trusted), Some(peer));
        assert_eq!(client_ip(&headers, None, &trusted), None);
    }

    #[test]
    fn ip_cidr_matching_masks_host_bits() {
        assert!(
            "192.0.2.7"
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip_matches_cidr(ip, "192.0.2.3/24"))
        );
        assert!(
            !"192.0.3.7"
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip_matches_cidr(ip, "192.0.2.3/24"))
        );
        assert!(
            "2001:db8::1"
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip_matches_cidr(ip, "2001:db8::/32"))
        );
    }
}
