//! Mastodon-compatible followers synchronization (FEP-8fcf).
//!
//! Mastodon sends a `Collection-Synchronization` header with followers-only
//! status deliveries. The digest is an order-independent XOR of SHA-256 hashes
//! for the actor URIs in a domain-scoped followers collection.

use axum::Json;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use plamenu_ap::activity::id_of;
use plamenu_ap::collection::InlineOrderedCollection;
use plamenu_ap::urls::LocalUserUrls;
use plamenu_db::account::{self, Account};
use plamenu_db::{follow, id, job};
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::Url;

use crate::AppState;
use crate::error::ApiError;

pub const HEADER_NAME: &str = "Collection-Synchronization";
const MAX_COLLECTION_PAGES: usize = 10;
const MAX_COLLECTION_ITEMS: usize = 1_000;
const MAX_SYNC_WALL_TIME: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
struct SyncHeader {
    collection_id: String,
    digest: String,
    url: String,
}

#[derive(Debug, Clone, Copy)]
enum ExpectedDigest {
    Absent,
    Valid([u8; 32]),
    Invalid,
}

impl ExpectedDigest {
    fn from_hex(value: Option<&str>) -> Self {
        let Some(value) = value else {
            return Self::Absent;
        };
        let mut out = [0u8; 32];
        if value.len() != 64 {
            return Self::Invalid;
        }
        for (idx, chunk) in value.as_bytes().chunks(2).enumerate() {
            let Ok(hex) = std::str::from_utf8(chunk) else {
                return Self::Invalid;
            };
            let Ok(byte) = u8::from_str_radix(hex, 16) else {
                return Self::Invalid;
            };
            out[idx] = byte;
        }
        Self::Valid(out)
    }

    fn allows_removal(self, residual: Option<[u8; 32]>) -> bool {
        match self {
            Self::Absent => true,
            Self::Valid(_) => residual == Some([0u8; 32]),
            Self::Invalid => false,
        }
    }

    fn residual_seed(self) -> Option<[u8; 32]> {
        match self {
            Self::Valid(bytes) => Some(bytes),
            Self::Absent | Self::Invalid => None,
        }
    }
}

/// Builds the exact header value Mastodon emits:
/// `collectionId="...", digest="...", url="..."`.
pub async fn synchronization_header(
    state: &AppState,
    signer: &Account,
    inbox_url: &str,
) -> Result<Option<String>, ApiError> {
    if !signer.is_local() {
        return Ok(None);
    }
    let Some(prefix) = origin_prefix(inbox_url) else {
        return Ok(None);
    };
    let uris = follow::follower_uris_matching_prefix(&state.pool, signer.id, &prefix).await?;
    let digest = followers_digest_hex(uris.iter().map(String::as_str));
    let local_actor_urls = LocalUserUrls::for_account(
        &state.config.domain,
        &signer.username,
        signer.uri.as_deref(),
    );
    let sync_url = local_actor_urls.followers_synchronization;
    Ok(Some(format!(
        "collectionId=\"{}\", digest=\"{}\", url=\"{}\"",
        local_actor_urls.followers, digest, sync_url
    )))
}

/// Handles an inbound `Collection-Synchronization` header after the sender's
/// HTTP signature has been verified.
pub async fn process_inbound_header(
    state: &AppState,
    sender: &Account,
    headers: &HeaderMap,
) -> Result<(), ApiError> {
    let Some(raw) = headers
        .get(HEADER_NAME)
        .and_then(|value| value.to_str().ok())
    else {
        return Ok(());
    };
    let Some(parsed) = parse_header(raw) else {
        tracing::warn!("invalid Collection-Synchronization header");
        return Ok(());
    };
    if sender.is_local() {
        return Ok(());
    }
    let Some(sender_uri) = sender.uri.as_deref() else {
        return Ok(());
    };
    let Some(collections) = account::collection_urls_of(&state.pool, sender.id).await? else {
        return Ok(());
    };
    if parsed.collection_id != collections.followers_url
        || !same_host(sender_uri, &parsed.url)
        || local_followers_hash(state, sender.id).await? == parsed.digest
    {
        return Ok(());
    }
    if let Ok(result) = tokio::time::timeout(
        MAX_SYNC_WALL_TIME,
        synchronize_followers(state, sender, &parsed.url, Some(&parsed.digest)),
    )
    .await
    {
        result
    } else {
        tracing::warn!(
            actor = sender.id,
            "followers synchronization exhausted wall-clock budget"
        );
        Ok(())
    }
}

/// The signed partial collection Mastodon fetches from the header's `url`.
pub async fn partial_collection_response(
    state: &AppState,
    account: &Account,
    requester: &Account,
) -> Result<Response, ApiError> {
    let Some(requester_uri) = requester.uri.as_deref() else {
        return Err(ApiError::Unauthorized("cannot scope requester".into()));
    };
    let Some(prefix) = origin_prefix(requester_uri) else {
        return Err(ApiError::Unauthorized("cannot scope requester".into()));
    };
    let items = follow::follower_uris_matching_prefix(&state.pool, account.id, &prefix).await?;
    let sync_url = LocalUserUrls::for_account(
        &state.config.domain,
        &account.username,
        account.uri.as_deref(),
    )
    .followers_synchronization;
    let document =
        InlineOrderedCollection::new(&sync_url, items.into_iter().map(Value::String).collect());
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8),
            (header::CACHE_CONTROL, "max-age=0, private"),
        ],
        Json(document),
    )
        .into_response())
}

async fn local_followers_hash(
    state: &AppState,
    target_account_id: i64,
) -> Result<String, ApiError> {
    let followers = follow::local_follower_identities(&state.pool, target_account_id).await?;
    let uris: Vec<String> = followers
        .iter()
        .map(|follower| {
            LocalUserUrls::for_account(
                &state.config.domain,
                &follower.username,
                follower.uri.as_deref(),
            )
            .id
        })
        .collect();
    Ok(followers_digest_hex(uris.iter().map(String::as_str)))
}

async fn synchronize_followers(
    state: &AppState,
    remote: &Account,
    collection_url: &str,
    expected_digest: Option<&str>,
) -> Result<(), ApiError> {
    let expected = ExpectedDigest::from_hex(expected_digest);
    let mut residual = expected.residual_seed();
    let mut expected_local_ids = Vec::new();
    let complete = process_collection(
        state,
        remote,
        collection_url,
        &mut residual,
        &mut expected_local_ids,
    )
    .await?;
    if complete && expected.allows_removal(residual) {
        remove_unexpected_local_followers(state, remote, &expected_local_ids).await?;
    }
    Ok(())
}

async fn process_collection(
    state: &AppState,
    remote: &Account,
    collection_url: &str,
    residual: &mut Option<[u8; 32]>,
    expected_local_ids: &mut Vec<i64>,
) -> Result<bool, ApiError> {
    let Ok(mut collection) = state.federation.fetch_object(collection_url).await else {
        return Ok(false);
    };
    if let Some(first) = collection.get("first") {
        if let Some(first_url) = id_of(first) {
            if !same_host(collection_url, first_url) {
                return Ok(false);
            }
            let Ok(first_page) = state.federation.fetch_object(first_url).await else {
                return Ok(false);
            };
            collection = first_page;
        } else if first.is_object() {
            collection = first.clone();
        }
    }

    let mut processed = 0usize;
    for _ in 0..MAX_COLLECTION_PAGES {
        let remaining = MAX_COLLECTION_ITEMS.saturating_sub(processed);
        if remaining == 0 {
            return Ok(false);
        }
        let mut items = collection_items(&collection);
        if items.len() > remaining {
            items.truncate(remaining);
            return Ok(false);
        }
        processed += items.len();
        process_page(state, remote, &items, residual, expected_local_ids).await?;
        let Some(next_url) = collection.get("next").and_then(id_of) else {
            return Ok(true);
        };
        if !same_host(collection_url, next_url) {
            return Ok(false);
        }
        let Ok(next_page) = state.federation.fetch_object(next_url).await else {
            return Ok(false);
        };
        collection = next_page;
    }
    Ok(false)
}

async fn process_page(
    state: &AppState,
    remote: &Account,
    items: &[String],
    residual: &mut Option<[u8; 32]>,
    expected_local_ids: &mut Vec<i64>,
) -> Result<(), ApiError> {
    let local_uris: Vec<&str> = items
        .iter()
        .map(String::as_str)
        .filter(|uri| crate::local_identity::has_local_actor_shape(&state.config.domain, uri))
        .collect();
    let locals =
        crate::local_identity::find_actors(&state.pool, &state.config.domain, &local_uris).await?;
    for uri in items {
        if let Some(bytes) = residual {
            xor_digest(bytes, uri);
        }
        let Some(local) = locals.get(uri) else {
            continue;
        };
        expected_local_ids.push(local.id);
        match follow::find(&state.pool, local.id, remote.id).await? {
            Some(edge) if edge.pending => {
                follow::mark_accepted(&state.pool, local.id, remote.id).await?;
            }
            Some(_) => {}
            None => send_unexpected_follow_undo(state, local, remote).await?,
        }
    }
    Ok(())
}

async fn remove_unexpected_local_followers(
    state: &AppState,
    remote: &Account,
    expected_local_ids: &[i64],
) -> Result<(), ApiError> {
    let current = follow::local_follower_ids(&state.pool, remote.id).await?;
    for account_id in current {
        if expected_local_ids.contains(&account_id) {
            continue;
        }
        if let Some(local) = account::find_by_id(&state.pool, account_id).await? {
            crate::actions::unfollow_account(state, &local, remote).await?;
        }
    }
    Ok(())
}

async fn send_unexpected_follow_undo(
    state: &AppState,
    local: &Account,
    remote: &Account,
) -> Result<(), ApiError> {
    let Some(remote_uri) = remote.uri.as_deref() else {
        return Ok(());
    };
    let marker = id::next();
    let follow_activity =
        plamenu_ap::activity::follow(&state.config.domain, &local.username, marker, remote_uri);
    let undo = plamenu_ap::activity::undo(&state.config.domain, &local.username, follow_activity);
    job::enqueue(&state.pool, local.id, &remote.inbox_url, &undo).await?;
    Ok(())
}

fn collection_items(collection: &Value) -> Vec<String> {
    collection
        .get("items")
        .or_else(|| collection.get("orderedItems"))
        .map_or_else(Vec::new, items_from_value)
}

fn items_from_value(value: &Value) -> Vec<String> {
    match value {
        Value::Array(items) => items.iter().filter_map(id_of).map(str::to_owned).collect(),
        other => id_of(other)
            .map(|id| vec![id.to_owned()])
            .unwrap_or_default(),
    }
}

fn parse_header(raw: &str) -> Option<SyncHeader> {
    let mut collection_id = None;
    let mut digest = None;
    let mut url = None;
    for part in raw.split(',') {
        let (name, value) = part.trim().split_once('=')?;
        let value = value.trim().strip_prefix('"')?.strip_suffix('"')?;
        match name.trim() {
            "collectionId" => collection_id = Some(value.to_owned()),
            "digest" => digest = Some(value.to_owned()),
            "url" => url = Some(value.to_owned()),
            _ => {}
        }
    }
    Some(SyncHeader {
        collection_id: collection_id?,
        digest: digest?,
        url: url?,
    })
}

fn origin_prefix(raw_url: &str) -> Option<String> {
    let parsed = Url::parse(raw_url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let host = parsed.host_str()?;
    let mut prefix = format!("{}://{host}", parsed.scheme());
    if let Some(port) = parsed.port() {
        prefix.push(':');
        prefix.push_str(&port.to_string());
    }
    Some(prefix)
}

fn same_host(left: &str, right: &str) -> bool {
    let (Ok(left), Ok(right)) = (Url::parse(left), Url::parse(right)) else {
        return false;
    };
    match (left.host_str(), right.host_str()) {
        (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
        _ => false,
    }
}

fn followers_digest_hex<'a>(uris: impl Iterator<Item = &'a str>) -> String {
    let mut digest = [0u8; 32];
    for uri in uris {
        xor_digest(&mut digest, uri);
    }
    hex(&digest)
}

fn xor_digest(accumulator: &mut [u8; 32], uri: &str) {
    let hashed = Sha256::digest(uri.as_bytes());
    for (left, right) in accumulator.iter_mut().zip(hashed) {
        *left ^= right;
    }
}

pub(crate) fn hex(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut out, "{byte:02x}").expect("writing to string cannot fail");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_order_independent_xor_of_sha256_bytes() {
        let first = followers_digest_hex(["https://a.test/u/1", "https://a.test/u/2"].into_iter());
        let second = followers_digest_hex(["https://a.test/u/2", "https://a.test/u/1"].into_iter());
        assert_eq!(first, second);
        assert_ne!(
            first,
            followers_digest_hex(["https://a.test/u/1"].into_iter())
        );
    }

    #[test]
    fn parses_mastodon_header_shape() {
        let parsed = parse_header(
            r#"collectionId="https://remote/users/a/followers", digest="abcd", url="https://remote/sync""#,
        )
        .unwrap();
        assert_eq!(parsed.collection_id, "https://remote/users/a/followers");
        assert_eq!(parsed.digest, "abcd");
        assert_eq!(parsed.url, "https://remote/sync");
    }
}
