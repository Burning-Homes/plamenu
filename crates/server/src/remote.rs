//! Shared helpers for turning dereferenced remote actors into accounts.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use plamenu_ap::acct::Acct;
use plamenu_ap::actor::{GroupModerators, RemoteActor, image_url, is_moderator_affiliation};
use plamenu_db::account::{self, Account, RemoteAccountData, upsert_remote};
use plamenu_db::actor_key::{self, NewPublicKey};
use plamenu_db::group::PostingPolicy;
use plamenu_db::{
    DbError, PgPool, account_media, gateway, link_verification, media_fetch_failure, remote_group,
};
use plamenu_federation::ResolvedAcct;
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use url::Url;

use crate::AppState;

/// `PropertyValue` attachments of a remote actor as our stored `fields`
/// rows, names and values sanitized (they are attacker-controlled HTML).
fn remote_fields(attachments: &[Value]) -> Vec<account::FieldPair> {
    attachments
        .iter()
        .filter(|entry| entry.get("type").and_then(Value::as_str) == Some("PropertyValue"))
        .take(20)
        .map(|entry| {
            let name = entry.get("name").and_then(Value::as_str).unwrap_or("");
            let value = entry.get("value").and_then(Value::as_str).unwrap_or("");
            account::FieldPair {
                name: plamenu_ap::text::sanitize_remote_text(name),
                value: plamenu_ap::text::sanitize_remote_html(value),
            }
        })
        .collect()
}

fn image_description(value: &Value) -> &str {
    match value {
        Value::Array(items) => items.first().map_or("", image_description),
        Value::Object(map) => map.get("summary").and_then(Value::as_str).unwrap_or(""),
        _ => "",
    }
}

/// Looks up the cached account for an HTTP-signature `keyId`: by the key id
/// the actor document declares, falling back to the keyId's fragment-stripped
/// base as an actor URI for signers (pub-relay) that sign with the bare actor
/// id — which also covers FEP-521a `#ed25519-key` ids. Callers must still
/// verify the signature against the returned key.
///
/// A row only counts when its own actor URI shares the keyId's host: actor
/// documents are attacker-controlled, so a hostile (or, as it happened, a
/// hand-seeded QA) account claiming someone else's `publicKey.id` must not
/// shadow the real owner here — that would misattribute the signer and
/// silently drop the owner's deliveries as unproven forwards.
pub async fn find_cached_signer(pool: &PgPool, key_id: &str) -> Result<Option<Account>, DbError> {
    let same_origin = |account: &Account| {
        account
            .uri
            .as_deref()
            .and_then(host_of)
            .is_some_and(|host| Some(host) == host_of(key_id))
    };
    if let Some(key) = actor_key::usable_by_uri(pool, key_id).await?
        && let Some(account_id) = key.account_id
        && let Some(account) = account::find_by_id(pool, account_id).await?
        && same_origin(&account)
    {
        return Ok(Some(account));
    }
    if let Some(cached) = account::find_by_key_id(pool, key_id).await?
        && same_origin(&cached)
    {
        return Ok(Some(cached));
    }
    Ok(
        account::find_by_uri(pool, key_id.split('#').next().unwrap_or(key_id))
            .await?
            .filter(same_origin),
    )
}

/// Checks an inbound signature (either dialect) against an account's key
/// material: the stored Ed25519 Multikey when the signature asks for one,
/// the RSA PEM otherwise. The inner result is the verification verdict; the
/// outer error is storage failure only.
pub async fn prepared_matches_account(
    pool: &PgPool,
    prepared: &plamenu_federation::PreparedRequestAuth,
    account: &Account,
) -> Result<Result<(), plamenu_federation::SignatureError>, DbError> {
    if let Some(key) = actor_key::usable_by_uri(pool, prepared.key_id()).await?
        && key.account_id == Some(account.id)
        && key.controller_uri == account.uri.as_deref().unwrap_or_default()
    {
        return Ok(match key.algorithm.as_str() {
            "ed25519" => prepared.verify_ed25519_multikey(&key.public_key),
            "rsa" => prepared.verify_rsa_pem(&key.public_key),
            _ => Err(plamenu_federation::SignatureError::BadPublicKey),
        });
    }
    // Compatibility for actors (notably pub-relay) that put their bare actor
    // URI in `keyId` while the dereferenced actor declares a classic RSA
    // `publicKey.id` with a fragment. This is still deterministic exact-key
    // selection: the alias resolves only to that explicitly declared classic
    // key, never to the first RSA key in the normalized set.
    if account.uri.as_deref() == Some(prepared.key_id())
        && let Some(classic_key_id) = account.public_key_id.as_deref().filter(|id| !id.is_empty())
        && let Some(key) = actor_key::usable_by_uri(pool, classic_key_id).await?
        && key.account_id == Some(account.id)
        && key.controller_uri == prepared.key_id()
        && key.algorithm == "rsa"
    {
        return Ok(prepared.verify_rsa_pem(&key.public_key));
    }
    Ok(Err(plamenu_federation::SignatureError::BadPublicKey))
}

fn remote_public_keys(actor: &RemoteActor) -> Result<Vec<NewPublicKey>, DbError> {
    let methods = actor
        .verification_methods()
        .map_err(|error| DbError::Protocol(error.to_string()))?;
    Ok(methods
        .into_iter()
        // External references are resolved by the guarded key-refresh path;
        // never invent material for them during an actor-only store.
        .filter_map(|method| {
            if method.revoked {
                return None;
            }
            Some(NewPublicKey {
                key_uri: method.key_uri,
                controller_uri: method.controller_uri,
                algorithm: method.algorithm?,
                public_key: method.public_key?,
                source: method.source.to_owned(),
                expires_at: method.expires_at,
            })
        })
        .collect())
}

#[allow(
    clippy::too_many_lines,
    reason = "one guarded resolver validates both classic and FEP-521a external key documents"
)]
async fn resolved_remote_public_keys(
    state: &AppState,
    actor: &RemoteActor,
) -> Result<Vec<NewPublicKey>, DbError> {
    let methods = actor
        .verification_methods()
        .map_err(|error| DbError::Protocol(error.to_string()))?;
    let mut keys = Vec::with_capacity(methods.len());
    for method in methods {
        if method.revoked {
            continue;
        }
        if let (Some(algorithm), Some(public_key)) = (method.algorithm, method.public_key) {
            keys.push(NewPublicKey {
                key_uri: method.key_uri,
                controller_uri: method.controller_uri,
                algorithm,
                public_key,
                source: method.source.to_owned(),
                expires_at: method.expires_at,
            });
            continue;
        }
        if !crate::instance_policy::can_federate_url(
            &state.pool,
            &state.config.domain,
            &method.key_uri,
        )
        .await?
        {
            return Err(DbError::Protocol(format!(
                "unsafe external verification method {}",
                method.key_uri
            )));
        }
        let document = state
            .federation
            .fetch_object(&method.key_uri)
            .await
            .map_err(|error| {
                DbError::Protocol(format!(
                    "cannot fetch external verification method {}: {error}",
                    method.key_uri
                ))
            })?;
        if document.get("id").and_then(Value::as_str) != Some(method.key_uri.as_str()) {
            return Err(DbError::Protocol(format!(
                "external verification method {} has a wrong id",
                method.key_uri
            )));
        }
        // A revoked external method is deliberately omitted. The atomic
        // replacement below revokes a previously cached row with this URI.
        if document.get("revoked").is_some() {
            continue;
        }
        let (algorithm, public_key) = if method.source == "classic-external" {
            if document.get("owner").and_then(Value::as_str) != Some(actor.id.as_str()) {
                return Err(DbError::Protocol(format!(
                    "external classic key {} has a wrong owner",
                    method.key_uri
                )));
            }
            let pem = document
                .get("publicKeyPem")
                .and_then(Value::as_str)
                .filter(|pem| !pem.is_empty())
                .ok_or_else(|| {
                    DbError::Protocol(format!(
                        "external classic key {} has no publicKeyPem",
                        method.key_uri
                    ))
                })?;
            ("rsa", pem.to_owned())
        } else {
            if document.get("type").and_then(Value::as_str) != Some("Multikey")
                || document.get("controller").and_then(Value::as_str) != Some(actor.id.as_str())
            {
                return Err(DbError::Protocol(format!(
                    "external verification method {} has a wrong type or controller",
                    method.key_uri
                )));
            }
            let multibase = document
                .get("publicKeyMultibase")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    DbError::Protocol(format!(
                        "external verification method {} has no publicKeyMultibase",
                        method.key_uri
                    ))
                })?;
            let decoded = plamenu_ap::multikey::decode_public(multibase)
                .map_err(|error| DbError::Protocol(error.to_string()))?;
            match decoded {
                plamenu_ap::multikey::PublicMultikey::Rsa(pem) => ("rsa", pem),
                plamenu_ap::multikey::PublicMultikey::Ed25519(_) => {
                    ("ed25519", multibase.to_owned())
                }
                plamenu_ap::multikey::PublicMultikey::MlDsa44(_) => {
                    ("ml-dsa-44", multibase.to_owned())
                }
            }
        };
        let expires_at = match document.get("expires") {
            None => None,
            Some(Value::String(raw)) => {
                Some(OffsetDateTime::parse(raw, &Rfc3339).map_err(|_| {
                    DbError::Protocol(format!(
                        "external verification method {} has an invalid expiry",
                        method.key_uri
                    ))
                })?)
            }
            Some(_) => {
                return Err(DbError::Protocol(format!(
                    "external verification method {} has an invalid expiry",
                    method.key_uri
                )));
            }
        };
        keys.push(NewPublicKey {
            key_uri: method.key_uri,
            controller_uri: actor.id.clone(),
            algorithm: algorithm.to_owned(),
            public_key,
            source: if method.source == "classic-external" {
                method.source.to_owned()
            } else {
                "multikey-external".to_owned()
            },
            expires_at,
        });
    }
    Ok(keys)
}

async fn store_remote_actor_with_resolved_keys(
    state: &AppState,
    actor: &RemoteActor,
    verified_acct: Option<&Acct>,
) -> Result<Account, DbError> {
    if let Some(previous) = account::find_by_uri(&state.pool, &actor.id).await?
        && previous.suspended()
        && previous.suspension_origin.as_deref() == Some("local")
    {
        // A locally imposed suspension freezes the remote document wholesale.
        // In particular, do not let it induce WebFinger or external-key
        // fetches before the inner profile updater gets a chance to reject it.
        return Ok(previous);
    }
    // Resolve and validate the complete key set before mutating actor state.
    // The existing fetcher enforces URL policy, redirect and body-size bounds.
    let keys = resolved_remote_public_keys(state, actor).await?;
    store_remote_actor_using_keys(&state.pool, actor, verified_acct, &keys).await
}

async fn store_remote_actor_using_keys(
    pool: &PgPool,
    actor: &RemoteActor,
    verified_acct: Option<&Acct>,
    keys: &[NewPublicKey],
) -> Result<Account, DbError> {
    if let Some(acct) = verified_acct {
        return store_remote_actor_with_handle(
            pool,
            actor,
            acct.username(),
            acct.domain(),
            Some(keys),
        )
        .await;
    }
    if let Some(previous) = account::find_by_uri(pool, &actor.id).await? {
        return store_remote_actor_with_handle(
            pool,
            actor,
            &previous.username,
            previous.domain.as_deref().unwrap_or_default(),
            Some(keys),
        )
        .await;
    }
    if let Some(acct) = actor.webfinger_acct() {
        return store_remote_actor_with_handle(
            pool,
            actor,
            acct.username(),
            acct.domain(),
            Some(keys),
        )
        .await;
    }
    let domain = host_of(&actor.id).unwrap_or_default();
    if actor.preferred_username.is_empty() {
        let fallback = no_acct_username(&actor.id);
        return store_remote_actor_with_handle(pool, actor, &fallback, domain, Some(keys)).await;
    }
    store_remote_actor_with_handle(pool, actor, &actor.preferred_username, domain, Some(keys)).await
}

fn primary_rsa<'a>(keys: &'a [NewPublicKey], actor: &'a RemoteActor) -> (&'a str, &'a str) {
    if let Some(key) = keys.iter().find(|key| key.algorithm == "rsa") {
        return (&key.public_key, &key.key_uri);
    }
    if actor.public_key.owner == actor.id {
        (&actor.public_key.public_key_pem, &actor.public_key.id)
    } else {
        ("", "")
    }
}

/// Persists a fetched remote actor as an account row. Profile text is
/// attacker-controlled HTML and is sanitized before storage.
pub async fn store_remote_actor(pool: &PgPool, actor: &RemoteActor) -> Result<Account, DbError> {
    // This pool-only helper deliberately cannot authorize a handle change. It
    // remains useful for fixtures and actor ingestion that has no network
    // context, but an existing URI keeps its last stored handle. Production
    // refresh paths use `refresh_remote_actor`, which performs WebFinger
    // loopback before selecting a different handle.
    if let Some(previous) = account::find_by_uri(pool, &actor.id).await? {
        let claimed = claimed_acct(actor);
        if !claimed
            .as_ref()
            .is_some_and(|acct| same_account_handle(&previous, acct))
        {
            tracing::warn!(
                actor_uri = %actor.id,
                "remote actor handle claim not applied without WebFinger confirmation"
            );
        }
        return store_remote_actor_with_handle(
            pool,
            actor,
            &previous.username,
            previous.domain.as_deref().unwrap_or_default(),
            None,
        )
        .await;
    }
    if let Some(acct) = actor.webfinger_acct() {
        return store_remote_actor_as(pool, actor, &acct).await;
    }
    let domain = host_of(&actor.id).unwrap_or_default();
    // FEP-03c1: an actor need not have an `acct:` identity at all — no
    // `preferredUsername`, webfinger optional. Such actors import under a
    // deterministic handle derived from their canonical id; the id-hash
    // suffix keeps two acct-less actors on one domain out of each other's
    // `(username, domain)` uniqueness slot.
    if actor.preferred_username.is_empty() {
        let fallback = no_acct_username(&actor.id);
        return store_remote_actor_with_handle(pool, actor, &fallback, domain, None).await;
    }
    store_remote_actor_with_handle(pool, actor, &actor.preferred_username, domain, None).await
}

fn claimed_acct(actor: &RemoteActor) -> Option<Acct> {
    actor.webfinger_acct().or_else(|| {
        let domain = host_of(&actor.id)?;
        Acct::new(&actor.preferred_username, domain).ok()
    })
}

fn same_account_handle(account: &Account, acct: &Acct) -> bool {
    account.username.eq_ignore_ascii_case(acct.username())
        && account
            .domain
            .as_deref()
            .is_some_and(|domain| domain.eq_ignore_ascii_case(acct.domain()))
}

fn resolution_names_actor(resolved: &ResolvedAcct, actor_id: &str) -> bool {
    resolved
        .candidates
        .iter()
        .any(|candidate| candidate.actor_uri == actor_id)
}

/// Stores a remote actor fetched through the supplied `WebFinger` resolution.
/// The capability is represented by the full resolution rather than a bare
/// `Acct`, so the actor URI loopback cannot accidentally be skipped.
pub async fn store_remote_actor_from_resolution(
    state: &AppState,
    actor: &RemoteActor,
    resolved: &ResolvedAcct,
) -> Result<Account, DbError> {
    if !resolution_names_actor(resolved, &actor.id) {
        return Err(DbError::Protocol(format!(
            "WebFinger for {} does not name actor {}",
            resolved.acct, actor.id
        )));
    }
    let stored = store_remote_actor_with_resolved_keys(state, actor, Some(&resolved.acct)).await?;
    account::touch_webfingered(&state.pool, stored.id).await?;
    Ok(stored)
}

/// Central remote actor refresh policy. Actor URI is immutable identity;
/// handle metadata changes only after `WebFinger` for the claimed acct loops
/// back to that exact URI. Network/policy failure preserves the old handle but
/// does not prevent independent profile and key fields from refreshing.
pub async fn refresh_remote_actor(
    state: &AppState,
    actor: &RemoteActor,
) -> Result<Account, DbError> {
    let previous = account::find_by_uri(&state.pool, &actor.id).await?;
    if let Some(previous) = &previous
        && previous.suspended()
        && previous.suspension_origin.as_deref() == Some("local")
    {
        return Ok(previous.clone());
    }
    let Some(claimed) = claimed_acct(actor) else {
        return store_remote_actor_with_resolved_keys(state, actor, None).await;
    };

    if previous
        .as_ref()
        .is_some_and(|account| same_account_handle(account, &claimed))
    {
        return store_remote_actor_with_resolved_keys(state, actor, None).await;
    }

    let permitted = crate::instance_policy::can_federate_domain(
        &state.pool,
        &state.config.domain,
        claimed.domain(),
    )
    .await?;
    if permitted {
        match state.federation.resolve_acct(&claimed).await {
            Ok(resolved) if resolution_names_actor(&resolved, &actor.id) => {
                return store_remote_actor_from_resolution(state, actor, &resolved).await;
            }
            Ok(_) => tracing::warn!(
                actor_uri = %actor.id,
                claimed_acct = %claimed,
                "remote handle change rejected: WebFinger does not loop back"
            ),
            Err(error) => tracing::warn!(
                actor_uri = %actor.id,
                claimed_acct = %claimed,
                %error,
                "remote handle change deferred: WebFinger unavailable"
            ),
        }
    } else {
        tracing::warn!(
            actor_uri = %actor.id,
            claimed_acct = %claimed,
            "remote handle change rejected by federation policy"
        );
    }

    // For an existing actor this preserves the verified handle. A newly
    // discovered actor may still be represented under its claimed or
    // deterministic stand-in handle, but remains unverified
    // (`last_webfingered_at IS NULL`) until a later successful resolution.
    store_remote_actor_with_resolved_keys(state, actor, None).await
}

/// The stand-in handle of an actor without any `acct:` identity (FEP-03c1):
/// the id's last readable path segment plus a short stable hash of the full
/// id. Deterministic, so refreshes land on the same handle; unique per id,
/// so acct-uniqueness never conflates two such actors.
fn no_acct_username(actor_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let segment: String = actor_id
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        .take(40)
        .collect();
    let digest = Sha256::digest(actor_id.as_bytes());
    let hash = digest[..4].iter().fold(String::new(), |mut out, byte| {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
        out
    });
    if segment.is_empty() {
        hash
    } else {
        format!("{segment}-{hash}")
    }
}

fn valid_collection_uri(uri: Option<&str>) -> String {
    let Some(uri) = uri else {
        return String::new();
    };
    match Url::parse(uri) {
        Ok(parsed)
            if matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some() =>
        {
            uri.to_owned()
        }
        _ => String::new(),
    }
}

/// Persists a fetched actor under the handle confirmed by `WebFinger`.
///
/// Some deployments serve `WebFinger` on one domain while hosting actors on a
/// different one (for example `acct:user@example.com` with an actor id on
/// `social.example.com`). Mastodon stores the `WebFinger` handle as the account
/// acct and keeps the actor id separately in `uri`.
pub async fn store_remote_actor_as(
    pool: &PgPool,
    actor: &RemoteActor,
    acct: &Acct,
) -> Result<Account, DbError> {
    store_remote_actor_with_handle(pool, actor, acct.username(), acct.domain(), None).await
}

/// Re-checks a remote actor's rel="me" links when a refresh leaves a URL
/// field unverified (Mastodon's `ProcessAccountService#check_links!`), spread
/// over ten minutes to soften ingest bursts. Already-verified fields keep
/// their stamp through the upsert, so an all-verified profile re-enqueues
/// nothing.
async fn enqueue_link_check(pool: &PgPool, stored: &Account) -> Result<(), DbError> {
    let needs_check = stored.fields.as_array().is_some_and(|fields| {
        fields.iter().any(|field| {
            field.get("verified_at").is_none()
                && field
                    .get("value")
                    .and_then(Value::as_str)
                    .is_some_and(|value| crate::link_verify::remote_field_url(value).is_some())
        })
    });
    if needs_check {
        let jitter = i32::try_from(stored.id % 600).unwrap_or(0);
        link_verification::enqueue_in(pool, stored.id, jitter).await?;
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "one linear mapping from actor document to account row"
)]
async fn store_remote_actor_with_handle(
    pool: &PgPool,
    actor: &RemoteActor,
    username: &str,
    domain: &str,
    resolved_keys: Option<&[NewPublicKey]>,
) -> Result<Account, DbError> {
    // FEP-844e capability learning (the primary RFC 9421 signal): an actor that
    // advertises rfc9421 in its `implements` declares its host can verify our
    // 9421 deliveries. Unlike a bare outbound 200 (the upstream-Pleroma trap,
    // which never advertises the capability), this is an explicit signal, so
    // record a positive verdict for the host. Every delivery target is stored
    // here before we deliver, so this is learned ahead of the first knock.
    // Best-effort: a pref-write failure must never fail the actor store.
    if actor.advertises_rfc9421()
        && let Some(host) = crate::link_preview::url_host(&actor.id)
        && let Err(error) = plamenu_db::signature_prefs::record(pool, &host, true).await
    {
        tracing::warn!(%error, host, "failed to record FEP-844e rfc9421 capability");
    }
    let display_name = plamenu_ap::text::sanitize_remote_text(actor.name.as_deref().unwrap_or(""));
    let note = plamenu_ap::text::sanitize_remote_html(actor.summary.as_deref().unwrap_or(""));
    let remote_image = |value: &Option<Value>| {
        value
            .as_ref()
            .and_then(image_url)
            .filter(|url| plamenu_federation::is_federation_url(url))
            .map(str::to_owned)
    };
    let avatar = remote_image(&actor.icon);
    let header = remote_image(&actor.image);
    let avatar_description =
        plamenu_ap::text::sanitize_remote_text(actor.icon.as_ref().map_or("", image_description));
    let header_description =
        plamenu_ap::text::sanitize_remote_text(actor.image.as_ref().map_or("", image_description));
    let published = actor
        .published
        .as_deref()
        .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok());
    let featured = actor
        .featured
        .as_ref()
        .and_then(plamenu_ap::activity::id_of)
        .filter(|url| plamenu_federation::is_federation_url(url));
    let also_known_as = actor.also_known_as_uris();
    // The human web URL (`/@name`), kept only when it is a real link.
    let web_url = actor
        .web_url()
        .filter(|url| plamenu_federation::is_federation_url(url));
    let followers_url = valid_collection_uri(actor.followers_url());
    let following_url = valid_collection_uri(actor.following_url());
    let outbox_url = valid_collection_uri(actor.outbox_url());
    let feature_approval_policy = actor.interaction_policy.as_ref().map_or(0, |policy| {
        plamenu_ap::quote_policy::parse_capability_policy(
            policy,
            "canFeature",
            &actor.id,
            &followers_url,
            &following_url,
        )
    });
    // The actor's custom emoji (display name / bio) ride its `tag` array,
    // recorded the same way a Note's emoji are.
    crate::emoji::ingest_remote_emoji_tags(pool, None, domain, &actor.tag).await?;
    // The prior state decides whether the avatar/header must be (re)downloaded.
    let prev = account::find_by_uri(pool, &actor.id).await?;
    let mut verification_keys = match resolved_keys {
        Some(keys) => keys.to_vec(),
        None => remote_public_keys(actor)?,
    };
    // A portable actor publishes the gateway-generated RSA key alongside its
    // own client keys. That key is intentionally stored with encrypted private
    // material and must never be replaced/revoked by the ordinary remote-key
    // refresh path; the client still owns every other verification method.
    if actor.id.contains("/.well-known/apgateway/did:key:")
        && let Some(previous) = &prev
        && let Some(portable) = gateway::find_by_account_id(pool, previous.id).await?
    {
        let gateway_key_id = format!("{}#gateway-rsa", portable.actor_uri);
        verification_keys.retain(|key| key.key_uri != gateway_key_id);
    }
    let (rsa_public_key, rsa_key_id) = primary_rsa(&verification_keys, actor);
    // Mastodon's `set_suspension!` ladder. A locally-imposed suspension is
    // never altered by the remote document, and while it holds nothing is
    // taken from the JSON at all (Mastodon skips the key and attribute
    // setters). A remote-reported suspension is mirrored with
    // `suspension_origin = 'remote'`; profile attributes freeze at their
    // pre-suspension values (the inbound document is blanked anyway) while
    // key material keeps refreshing, and the suspension lifts once the
    // origin stops reporting it.
    if let Some(prev_account) = &prev
        && prev_account.suspended()
        && prev_account.suspension_origin.as_deref() == Some("local")
    {
        return Ok(prev_account.clone());
    }
    if actor.suspended
        && let Some(prev_account) = &prev
    {
        if !rsa_public_key.is_empty() {
            account::update_remote_keys(pool, prev_account.id, rsa_public_key, rsa_key_id).await?;
        }
        actor_key::replace_remote(pool, prev_account.id, &actor.id, &verification_keys).await?;
        if !prev_account.suspended() {
            account::suspend(pool, prev_account.id, "remote").await?;
        }
        return Ok(account::find_by_uri(pool, &actor.id)
            .await?
            .unwrap_or_else(|| prev_account.clone()));
    }
    let stored = upsert_remote(
        pool,
        RemoteAccountData {
            username,
            domain,
            uri: &actor.id,
            display_name: &display_name,
            note: &note,
            inbox_url: &actor.inbox,
            shared_inbox_url: actor.shared_inbox_or_empty(),
            public_key_pem: rsa_public_key,
            public_key_id: rsa_key_id,
            avatar_remote_url: avatar.as_deref(),
            header_remote_url: header.as_deref(),
            avatar_description: &avatar_description,
            header_description: &header_description,
            created_at: published,
            fields: remote_fields(&actor.attachment),
            featured_collection_url: featured,
            locked: actor.manually_approves_followers,
            also_known_as: &also_known_as,
            moved_to_uri: actor.moved_to_uri(),
            url: web_url,
            discoverable: actor.discoverable,
            feature_approval_policy,
            is_bot: actor.is_bot(),
            indexable: actor.indexable,
            show_media: actor.show_media,
            show_media_replies: actor.show_replies_in_media,
            show_featured: actor.show_featured,
            memorial: actor.memorial,
            actor_type: actor.actor_type(),
        },
    )
    .await?;
    // A new account arriving already suspended is created (Mastodon does),
    // then immediately marked; a previously remote-suspended one whose origin
    // stopped reporting `suspended` is lifted (`unsuspend!`). Locally-origin
    // suspensions never reach this point (early return above).
    let stored = if actor.suspended == stored.suspended() {
        stored
    } else {
        if actor.suspended {
            account::suspend(pool, stored.id, "remote").await?;
        } else {
            account::unsuspend(pool, stored.id).await?;
        }
        account::find_by_uri(pool, &actor.id)
            .await?
            .unwrap_or(stored)
    };
    account::set_collection_urls(pool, stored.id, &followers_url, &following_url, &outbox_url)
        .await?;
    plamenu_db::remote_history::set_actor_metadata(
        pool,
        stored.id,
        (!outbox_url.is_empty()).then_some(outbox_url.as_str()),
        actor.restricts_unauthenticated_web(),
    )
    .await?;
    actor_key::replace_remote(pool, stored.id, &actor.id, &verification_keys).await?;
    let proofs = plamenu_ap::identity::verified(
        &actor.attachment,
        &actor.id,
        time::OffsetDateTime::now_utc(),
    );
    plamenu_db::identity_proof::replace(pool, stored.id, &proofs).await?;
    // `attributionDomains` (Mastodon's `fediverse:creator` authorization
    // list) rides every refresh, normalized like local input.
    account::set_attribution_domains(
        pool,
        stored.id,
        &crate::profile::normalize_attribution_domains(&actor.attribution_domain_strings()),
    )
    .await?;
    enqueue_link_check(pool, &stored).await?;
    // What a remote community says about itself: the NSFW flag, who may
    // start threads, and where its moderators are published. Mirrored on every
    // refresh so a community that turns a flag back off stops being marked.
    if stored.is_group() {
        remote_group::upsert(
            pool,
            stored.id,
            remote_group::GroupFacts {
                sensitive: actor.sensitive.unwrap_or(false),
                posting_policy: remote_posting_policy(actor).as_str(),
                moderators_uri: moderators_collection_uri(actor).unwrap_or_default(),
            },
        )
        .await?;
        // An actor that names no moderators at all is stating exactly that, so
        // a roster synced earlier is dropped here. The listed cases are the
        // spawned sync's job — they need the network, this does not.
        if actor.group_moderators().is_none() {
            remote_group::set_moderators(pool, stored.id, &[]).await?;
        }
    }

    // Download (or re-download) a profile image when the origin advertises one
    // and we have no cached copy of it yet, or it changed since we last cached
    // — unless a `reject_media` domain block forbids caching from this domain
    // (the entity then keeps serving the remote URL, like an uncached image).
    if plamenu_db::instance_policy::domain_rejects_media(pool, domain).await? {
        return Ok(stored);
    }
    let needs = |new: Option<&str>, prev_file: Option<&str>, prev_url: Option<&str>| {
        new.is_some_and(|n| prev_file.is_none() || prev_url != Some(n))
    };
    let prev = prev.as_ref();
    if needs(
        avatar.as_deref(),
        prev.and_then(|p| p.avatar_file_name.as_deref()),
        prev.and_then(|p| p.avatar_remote_url.as_deref()),
    ) {
        if avatar.as_deref() != prev.and_then(|p| p.avatar_remote_url.as_deref()) {
            media_fetch_failure::clear(pool, account_media::AVATAR, stored.id).await?;
        }
        account_media::enqueue(pool, stored.id, account_media::AVATAR).await?;
    }
    if needs(
        header.as_deref(),
        prev.and_then(|p| p.header_file_name.as_deref()),
        prev.and_then(|p| p.header_remote_url.as_deref()),
    ) {
        if header.as_deref() != prev.and_then(|p| p.header_remote_url.as_deref()) {
            media_fetch_failure::clear(pool, account_media::HEADER, stored.id).await?;
        }
        account_media::enqueue(pool, stored.id, account_media::HEADER).await?;
    }
    Ok(stored)
}

/// Inbound cap on synced featured tags — deliberately far above Mastodon's
/// local limit of 10, per the house rule for inbound federated caps.
const FEATURED_TAGS_SYNC_LIMIT: usize = 100;

/// Inbound cap on a remote community's mirrored moderator roster. Generous
/// against any real mod team (Lemmy's largest are a handful), and a bound on
/// how many actor fetches one collection can provoke: every unknown entry
/// costs a dereference, so an attacker-authored collection is capped here
/// rather than allowed to walk us through its whole instance.
const GROUP_MODERATORS_SYNC_LIMIT: usize = 50;

/// Who may start threads in a remote community, resolved from what its actor
/// publishes. Our own `postingPolicy` term wins when present (only another
/// Plamenu emits it, and only it can express "members only"); otherwise
/// Lemmy's boolean gives the two states its vocabulary has.
fn remote_posting_policy(actor: &RemoteActor) -> PostingPolicy {
    match actor.posting_policy.as_deref() {
        Some(stated @ ("anyone" | "members" | "mods")) => PostingPolicy::parse(stated),
        _ if actor.posting_restricted_to_mods == Some(true) => PostingPolicy::Mods,
        _ => PostingPolicy::Anyone,
    }
}

/// The collection to dereference for a Group actor's moderators, if any. An
/// inline roster (`PeerTube`'s shape) needs no fetch and so has no URI here.
fn moderators_collection_uri(actor: &RemoteActor) -> Option<&str> {
    match actor.group_moderators()? {
        GroupModerators::Collection(uri) | GroupModerators::Affiliations(uri) => Some(uri),
        GroupModerators::Inline(_) => None,
    }
}

/// The best-effort remote-collection refreshes coalesced by
/// [`RemoteRefreshCoordinator`]. Each `(account, kind)` pair is refreshed by at
/// most one spawned task at a time and no more than once per
/// [`REFRESH_MIN_INTERVAL`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RefreshKind {
    /// The actor's `featuredTags` collection ([`spawn_featured_tags_sync`]).
    FeaturedTags,
    /// The actor's `outbox` `totalItems` status count ([`spawn_outbox_stats_sync`]).
    OutboxStats,
    /// A Group actor's moderator roster ([`spawn_group_moderators_sync`]).
    GroupModerators,
}

/// Minimum spacing between two refreshes of the same `(account, kind)`. A remote
/// actor can send signed self-`Update`s without bound, and each accepted one
/// would otherwise spawn a fresh detached fetch; these refreshes are best-effort
/// (Mastodon keeps featured tags current between refreshes via live
/// `Add`/`Remove(Hashtag)`, and the outbox status count is a display nicety), so
/// a flood collapses to at most one refresh per actor per window.
const REFRESH_MIN_INTERVAL: Duration = Duration::from_mins(5);

/// Global ceiling on concurrently-spawned remote-refresh tasks. Each carries an
/// `AppState` clone and an attacker-supplied collection URI until its fetch
/// drains; the federation client caps *active* fetches at 32 but lets the rest
/// queue, so without this ceiling a flood of distinct-actor `Update`s behind a
/// slow origin would grow the task/waiter population without bound. Over the cap
/// a refresh is dropped, not queued — the work is best-effort.
const MAX_INFLIGHT_REFRESHES: usize = 256;

/// Cap on remembered last-refresh timestamps before expired ones are pruned, so
/// a flood of distinct-actor `Update`s cannot retain freshness stamps without
/// bound (they are only consulted within [`REFRESH_MIN_INTERVAL`]).
const REFRESH_STAMP_CAP: usize = 4096;

/// Process-wide count of remote-refresh tasks actually spawned — i.e. admitted
/// past coalescing, the freshness gate, and the global cap. Exposed via
/// [`refresh_spawn_count`] so a test can assert a flood of identical `Update`s
/// admits a bounded number of refreshes.
static REFRESH_SPAWN_COUNT: AtomicU64 = AtomicU64::new(0);

/// The number of remote-refresh tasks spawned so far this process.
#[must_use]
pub fn refresh_spawn_count() -> u64 {
    REFRESH_SPAWN_COUNT.load(Ordering::Relaxed)
}

/// Coalesces and rate-limits the best-effort remote-collection refreshes that
/// [`spawn_featured_tags_sync`]/[`spawn_outbox_stats_sync`] would otherwise
/// spawn unconditionally on every accepted actor `Update`. Held
/// behind an `Arc` in [`AppState`].
///
/// [`Self::try_admit`] is one synchronous critical section — never held across
/// an await — so a burst of concurrent `Update`s for one actor serializes here
/// and only the first is admitted. A durable, restart-surviving refresh *job*
/// (the finding's optional "if the work must survive restart" half) is left to
/// the broader queue-durability work.
#[derive(Default)]
pub struct RemoteRefreshCoordinator {
    inner: Arc<Mutex<RefreshState>>,
}

#[derive(Default)]
struct RefreshState {
    /// `(account, kind)` pairs whose refresh task is currently running: bounds
    /// the live task population (against [`MAX_INFLIGHT_REFRESHES`]) and
    /// coalesces concurrent duplicates.
    in_flight: HashSet<(i64, RefreshKind)>,
    /// The last time each `(account, kind)` was admitted — the freshness gate
    /// for rapid sequential repeats (kept even after the task ends).
    last_refresh: HashMap<(i64, RefreshKind), Instant>,
}

impl RemoteRefreshCoordinator {
    /// Claims a refresh slot for `(account_id, kind)`, or returns `None` when the
    /// refresh should be skipped: another is already in flight for the pair, one
    /// ran within [`REFRESH_MIN_INTERVAL`], or the global in-flight cap is
    /// reached. On success the returned [`RefreshGuard`] holds the in-flight slot
    /// until dropped (move it into the spawned task), and the freshness clock is
    /// stamped now so rapid repeats stay refused even after the task finishes.
    fn try_admit(&self, account_id: i64, kind: RefreshKind) -> Option<RefreshGuard> {
        let key = (account_id, kind);
        let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        // Prune expired stamps once the map has grown, bounding the freshness
        // map under a flood of distinct actors.
        if state.last_refresh.len() >= REFRESH_STAMP_CAP {
            state
                .last_refresh
                .retain(|_, at| at.elapsed() < REFRESH_MIN_INTERVAL);
        }
        if state
            .last_refresh
            .get(&key)
            .is_some_and(|at| at.elapsed() < REFRESH_MIN_INTERVAL)
        {
            return None; // refreshed too recently
        }
        if state.in_flight.contains(&key) {
            return None; // already running (a concurrent duplicate)
        }
        if state.in_flight.len() >= MAX_INFLIGHT_REFRESHES {
            return None; // global cap reached — shed rather than queue
        }
        state.in_flight.insert(key);
        state.last_refresh.insert(key, Instant::now());
        REFRESH_SPAWN_COUNT.fetch_add(1, Ordering::Relaxed);
        Some(RefreshGuard {
            inner: Arc::clone(&self.inner),
            key,
        })
    }

    #[cfg(test)]
    fn in_flight_len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .in_flight
            .len()
    }
}

/// The claim on a remote-refresh slot. Its `Drop` frees the in-flight entry (the
/// global-cap slot) on *every* exit of the spawned task — normal end, error, or
/// cancellation — so the coordinator can never leak a permanently-occupied slot.
/// The freshness stamp is deliberately kept so a sequential repeat is still
/// refused.
pub struct RefreshGuard {
    inner: Arc<Mutex<RefreshState>>,
    key: (i64, RefreshKind),
}

impl Drop for RefreshGuard {
    fn drop(&mut self) {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .in_flight
            .remove(&self.key);
    }
}

/// Best-effort sync of a remote profile's featured tags from its actor's
/// `featuredTags` collection — Mastodon's
/// `SynchronizeFeaturedTagsCollectionWorker`, as a spawned task so actor
/// ingestion never blocks on the extra fetch. The collection must live on
/// the actor's own host; live `Add`/`Remove(Hashtag)` activities keep the
/// rows current between refreshes.
pub fn spawn_featured_tags_sync(state: &AppState, account: &Account, collection_uri: Option<&str>) {
    let Some(uri) = collection_uri else { return };
    let (Some(account_uri), Some(actor_host)) = (
        account.uri.as_deref(),
        account.uri.as_deref().and_then(host_of),
    ) else {
        return;
    };
    if host_of(uri) != Some(actor_host) {
        tracing::debug!(actor = %account_uri, uri, "featuredTags collection on a foreign host; skipping");
        return;
    }
    // Coalesce/rate-limit so a flood of signed self-`Update`s cannot spawn an
    // unbounded pile of detached fetch tasks.
    let Some(guard) = state
        .remote_refresh
        .try_admit(account.id, RefreshKind::FeaturedTags)
    else {
        return;
    };
    let state = state.clone();
    let uri = uri.to_owned();
    let account_id = account.id;
    tokio::spawn(async move {
        let _guard = guard;
        if let Err(error) = sync_featured_tags(&state, account_id, &uri).await {
            tracing::debug!(uri, error = %error, "featured-tags sync failed");
        }
    });
}

/// Best-effort refresh of a remote account's status total from the
/// `totalItems` its outbox collection reports — Mastodon's
/// `ProcessAccountService#set_fetchable_attributes!` reading
/// `outbox_total_items` into `statuses_count` — as a spawned task so actor
/// ingestion never blocks on the extra fetch. The collection must live on
/// the actor's own host.
pub fn spawn_outbox_stats_sync(state: &AppState, account: &Account, outbox_uri: Option<&str>) {
    let Some(uri) = outbox_uri else { return };
    let (Some(account_uri), Some(actor_host)) = (
        account.uri.as_deref(),
        account.uri.as_deref().and_then(host_of),
    ) else {
        return;
    };
    if host_of(uri) != Some(actor_host) {
        tracing::debug!(actor = %account_uri, uri, "outbox collection on a foreign host; skipping");
        return;
    }
    // Coalesce/rate-limit so a flood of signed self-`Update`s cannot spawn an
    // unbounded pile of detached fetch tasks.
    let Some(guard) = state
        .remote_refresh
        .try_admit(account.id, RefreshKind::OutboxStats)
    else {
        return;
    };
    let state = state.clone();
    let uri = uri.to_owned();
    let account_id = account.id;
    tokio::spawn(async move {
        let _guard = guard;
        if let Err(error) = sync_outbox_stats(&state, account_id, &uri).await {
            tracing::debug!(uri, error = %error, "outbox stats sync failed");
        }
    });
}

/// Where a remote community's moderators are published, owned so the spawned
/// sync outlives the actor document it was read from.
#[derive(Debug, Clone)]
pub enum ModeratorSource {
    /// An `attributedTo` collection of actor IRIs (Lemmy's shape, and ours).
    Collection(String),
    /// A FEP-5219 `affiliations` collection of `Relationship` items (Mitra's
    /// shape, and ours).
    Affiliations(String),
    /// Moderators named inline on the actor (`PeerTube`'s shape); no fetch.
    Inline(Vec<String>),
}

/// Best-effort sync of a remote community's moderator roster — Lemmy's
/// `handle_community_moderators`, as a spawned task so actor ingestion never
/// blocks on the extra fetches. A collection must live on the actor's own host
/// (it is the actor's own endpoint); the *members* of that collection may live
/// anywhere, because a community moderated from another instance is ordinary
/// in Lemmy.
pub fn spawn_group_moderators_sync(state: &AppState, account: &Account, actor: &RemoteActor) {
    if !account.is_group() {
        return;
    }
    let (Some(account_uri), Some(actor_host)) = (
        account.uri.as_deref(),
        account.uri.as_deref().and_then(host_of),
    ) else {
        return;
    };
    let same_host = |uri: &str| {
        let matches = host_of(uri) == Some(actor_host);
        if !matches {
            tracing::debug!(actor = %account_uri, uri, "group moderators collection on a foreign host; skipping");
        }
        matches
    };
    let source = match actor.group_moderators() {
        Some(GroupModerators::Collection(uri)) if same_host(uri) => {
            ModeratorSource::Collection(uri.to_owned())
        }
        Some(GroupModerators::Affiliations(uri)) if same_host(uri) => {
            ModeratorSource::Affiliations(uri.to_owned())
        }
        Some(GroupModerators::Inline(ids)) => {
            ModeratorSource::Inline(ids.into_iter().map(str::to_owned).collect())
        }
        // Nothing published (cleared at ingest) or published somewhere we
        // won't follow.
        _ => return,
    };
    // Coalesce/rate-limit so a flood of signed self-`Update`s cannot spawn an
    // unbounded pile of detached fetch tasks.
    let Some(guard) = state
        .remote_refresh
        .try_admit(account.id, RefreshKind::GroupModerators)
    else {
        return;
    };
    let state = state.clone();
    let account_id = account.id;
    tokio::spawn(async move {
        let _guard = guard;
        if let Err(error) = sync_group_moderators(&state, account_id, &source).await {
            tracing::debug!(?source, error = %error, "group moderators sync failed");
        }
    });
}

/// The synchronous body of [`spawn_group_moderators_sync`] (public for tests).
///
/// Every entry is resolved to an account — a local one when the community
/// names one of our own users, a known row, or a freshly fetched actor — and
/// the roster is then replaced wholesale, since the origin's collection is
/// authoritative for its own community. An entry that cannot be resolved is
/// skipped rather than failing the sync: a mod list outlives the instance one
/// of its members was on.
pub async fn sync_group_moderators(
    state: &AppState,
    group_account_id: i64,
    source: &ModeratorSource,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let uris: Vec<String> = match source {
        ModeratorSource::Inline(ids) => ids.clone(),
        ModeratorSource::Collection(uri) => {
            let collection = state.federation.fetch_object(uri).await?;
            collection_items(&collection)
                .iter()
                .filter_map(|item| plamenu_ap::activity::id_of(item).map(str::to_owned))
                .collect()
        }
        ModeratorSource::Affiliations(uri) => {
            let collection = state.federation.fetch_object(uri).await?;
            collection_items(&collection)
                .iter()
                .filter(|item| {
                    item.get("relationship")
                        .and_then(Value::as_str)
                        .is_some_and(is_moderator_affiliation)
                })
                .filter_map(|item| {
                    item.get("subject")
                        .and_then(plamenu_ap::activity::id_of)
                        .map(str::to_owned)
                })
                .collect()
        }
    };
    let mut moderator_ids: Vec<i64> = Vec::new();
    for uri in uris.iter().take(GROUP_MODERATORS_SYNC_LIMIT) {
        let Some(account) = resolve_moderator(state, uri).await else {
            continue;
        };
        // A community is not its own moderator: `PeerTube` lists the channel
        // beside its owner, and an `attributedTo` collection can name the
        // group itself.
        if account.is_group() || moderator_ids.contains(&account.id) {
            continue;
        }
        moderator_ids.push(account.id);
    }
    remote_group::set_moderators(&state.pool, group_account_id, &moderator_ids).await?;
    Ok(())
}

/// One moderator entry as an account: a local user when the IRI is one of
/// ours (local rows carry no `uri`, so they are unreachable by lookup),
/// otherwise a known remote row, otherwise a fresh fetch — subject to the
/// same federation policy as any other actor fetch. `None` on anything that
/// does not resolve.
async fn resolve_moderator(state: &AppState, uri: &str) -> Option<Account> {
    if crate::local_identity::has_local_actor_shape(&state.config.domain, uri) {
        return crate::local_identity::find_actor(&state.pool, &state.config.domain, uri)
            .await
            .ok()
            .flatten();
    }
    if let Ok(Some(known)) = account::find_by_uri(&state.pool, uri).await {
        return Some(known);
    }
    let host = host_of(uri)?;
    if !crate::instance_policy::can_federate_domain(&state.pool, &state.config.domain, host)
        .await
        .unwrap_or(false)
    {
        return None;
    }
    let fetched = state.federation.fetch_actor(uri).await.ok()?;
    refresh_remote_actor(state, &fetched).await.ok()
}

/// The items of a fetched collection: on the collection itself or on an
/// inlined `first` page, under either `items` spelling. The collections read
/// this way (featured tags, moderator rosters) are small and served inline by
/// every implementation that publishes them, so no further paging is walked.
fn collection_items(collection: &Value) -> Vec<Value> {
    ["items", "orderedItems"]
        .iter()
        .find_map(|key| collection.get(key).and_then(Value::as_array))
        .or_else(|| {
            let first = collection.get("first")?;
            ["items", "orderedItems"]
                .iter()
                .find_map(|key| first.get(key).and_then(Value::as_array))
        })
        .cloned()
        .unwrap_or_default()
}

/// The synchronous body of [`spawn_outbox_stats_sync`] (public for tests).
/// A collection without a numeric `totalItems` keeps the stored baseline,
/// like Mastodon's `collection_info`.
pub async fn sync_outbox_stats(
    state: &AppState,
    account_id: i64,
    uri: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let collection = state.federation.fetch_object(uri).await?;
    let Some(total) = collection.get("totalItems").and_then(Value::as_u64) else {
        return Ok(());
    };
    let total = i64::try_from(total).unwrap_or(i64::MAX);
    plamenu_db::account::set_remote_statuses_count(&state.pool, account_id, total).await?;
    Ok(())
}

/// The synchronous body of [`spawn_featured_tags_sync`] (public for tests).
pub async fn sync_featured_tags(
    state: &AppState,
    account_id: i64,
    uri: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let collection = state.federation.fetch_object(uri).await?;
    let items = collection_items(&collection);
    let names: Vec<String> = items
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("Hashtag"))
        .filter_map(|item| item.get("name").and_then(Value::as_str))
        .map(|name| name.trim_start_matches(['#', '＃']).to_lowercase())
        .filter(|name| !name.is_empty())
        .take(FEATURED_TAGS_SYNC_LIMIT)
        .collect();
    let mut keep = Vec::with_capacity(names.len());
    for name in &names {
        let tag_id = plamenu_db::tag::ensure(&state.pool, name).await?;
        plamenu_db::featured_tag::feature(&state.pool, account_id, tag_id).await?;
        keep.push(tag_id);
    }
    // Anything featured locally that the origin no longer lists is dropped —
    // the collection is authoritative for its own account.
    for row in plamenu_db::featured_tag::list(&state.pool, account_id).await? {
        if !keep.contains(&row.tag_id) {
            plamenu_db::featured_tag::unfeature(&state.pool, account_id, row.tag_id).await?;
        }
    }
    Ok(())
}

/// Whether a webfinger candidate is worth fetching for `class`. A hint that
/// definitively contradicts the class is skipped to avoid a needless fetch; an
/// absent hint is always fetched, and the fetched actor's real type decides.
fn candidate_matches_class(
    class: account::ActorClass,
    candidate: &plamenu_federation::WebfingerCandidate,
) -> bool {
    use account::ActorClass;
    match (class, candidate.advertised_type.as_deref()) {
        (ActorClass::Any, _) | (_, None) => true,
        (ActorClass::Group, Some(kind)) => kind == "Group",
        (ActorClass::PersonLike, Some(kind)) => kind != "Group",
    }
}

/// Resolves `user@domain` to every stored account matching `class`: known rows
/// (re-webfingered once stale, Mastodon's 1-day TTL) plus any actor freshly
/// discovered through `WebFinger`. Best effort — the network is allowed to be
/// down, and a fetch failure for one actor never drops another.
///
/// A single handle can name several actors: a Lemmy host serves a Person
/// (`/u/name`) and a Group (`/c/name`) under one `acct`, distinguished only by
/// actor id and type. `class` says which are wanted; [`account::ActorClass::Any`]
/// returns them all (bare-handle discovery).
#[allow(
    clippy::too_many_lines,
    reason = "one linear cache-check → webfinger → per-candidate fetch/store flow"
)]
pub async fn resolve_remote_accounts(
    state: &AppState,
    acct: &Acct,
    class: account::ActorClass,
) -> Result<Vec<Account>, DbError> {
    // This routine is a remote-only boundary. In a split-domain deployment,
    // both the canonical handle domain and the actor host belong to us; never
    // WebFinger either one back through the federation client and risk
    // persisting a local actor as a remote account.
    if state.config.is_local_domain(acct.domain()) {
        return Ok(Vec::new());
    }
    if !crate::instance_policy::can_federate_domain(
        &state.pool,
        &state.config.domain,
        acct.domain(),
    )
    .await?
    {
        return Ok(Vec::new());
    }
    // Known rows OF THIS CLASS. A cached Person must never satisfy — or block —
    // a Group lookup, so the cache is scoped to the requested class.
    let cached =
        account::find_remote_by_acct_class(&state.pool, acct.username(), acct.domain(), class)
            .await?;
    // All cached-and-fresh short-circuits: the 1-day TTL throttles re-discovery.
    let mut all_fresh = !cached.is_empty();
    for known in &cached {
        if account::webfinger_is_stale(&state.pool, known.id).await? {
            all_fresh = false;
            break;
        }
    }
    if all_fresh {
        return Ok(cached);
    }
    let resolved = match state.federation.resolve_acct(acct).await {
        Ok(resolved) => resolved,
        Err(error) => {
            tracing::debug!(%acct, %error, "remote account webfinger resolution failed");
            return Ok(cached);
        }
    };
    if !crate::instance_policy::can_federate_domain(
        &state.pool,
        &state.config.domain,
        resolved.acct.domain(),
    )
    .await?
    {
        tracing::debug!(
            acct = %resolved.acct,
            "remote account rejected by domain federation policy"
        );
        return Ok(Vec::new());
    }
    // Fetch and store each candidate that could be of the requested class. The
    // fetched actor document — not the WebFinger hint — is authoritative for the
    // actor's real type, so a hostile hint cannot pass a wrong actor off as the
    // class. Failures are per-candidate: one bad fetch never drops the rest.
    let mut fresh: Vec<Account> = Vec::new();
    for candidate in &resolved.candidates {
        if !candidate_matches_class(class, candidate) {
            continue;
        }
        if !crate::instance_policy::can_federate_url(
            &state.pool,
            &state.config.domain,
            &candidate.actor_uri,
        )
        .await?
        {
            tracing::debug!(
                acct = %resolved.acct,
                actor_uri = %candidate.actor_uri,
                "remote actor URI rejected by federation policy"
            );
            continue;
        }
        let actor = match state.federation.fetch_actor(&candidate.actor_uri).await {
            Ok(actor) => actor,
            Err(error) => {
                tracing::debug!(
                    acct = %resolved.acct,
                    actor_uri = %candidate.actor_uri,
                    %error,
                    "remote account actor fetch failed"
                );
                continue;
            }
        };
        // The real type is authoritative; a Person hiding behind a Group hint
        // (or vice versa) is dropped from a typed lookup.
        if !class.matches(actor.actor_type()) {
            continue;
        }
        if !resolution_names_actor(&resolved, &actor.id) {
            tracing::warn!(
                acct = %resolved.acct,
                actor_uri = %actor.id,
                "fetched actor id was not advertised by WebFinger"
            );
            continue;
        }
        match store_remote_actor_from_resolution(state, &actor, &resolved).await {
            Ok(account) => {
                // Owncast keeps its playable HLS URL in WebFinger rather than
                // in the federated go-live Note. Learning that optional
                // capability is positive (NodeInfo-verified) and best-effort;
                // it must never change ordinary actor resolution semantics.
                if let Err(error) =
                    crate::owncast::learn_from_resolution(state, &account, &resolved).await
                {
                    tracing::debug!(account = account.id, %error, "Owncast capability storage failed");
                }
                spawn_featured_tags_sync(state, &account, actor.featured_tags_uri());
                spawn_outbox_stats_sync(state, &account, actor.outbox_url());
                spawn_group_moderators_sync(state, &account, &actor);
                fresh.push(account);
            }
            Err(error) => {
                tracing::warn!(
                    acct = %resolved.acct,
                    actor_uri = %candidate.actor_uri,
                    %error,
                    "remote account storage failed"
                );
            }
        }
    }
    // Never drop a known actor because refreshing a *different* one failed, or
    // because WebFinger transiently stopped advertising it: keep cached rows
    // that weren't just refreshed.
    for known in cached {
        if !fresh.iter().any(|a| a.id == known.id) {
            fresh.push(known);
        }
    }
    Ok(fresh)
}

/// Resolves `user@domain` to a single stored account of `class` — the
/// deterministic oldest match — fetching it through `WebFinger` if unknown.
/// `None` when nothing of that class resolves (best effort). This is the
/// Mastodon-compatible reading of a `@name@host` (`PersonLike`) or `!name@host`
/// (Group) handle.
pub async fn resolve_remote_account(
    state: &AppState,
    acct: &Acct,
    class: account::ActorClass,
) -> Result<Option<Account>, DbError> {
    Ok(resolve_remote_accounts(state, acct, class)
        .await?
        .into_iter()
        .min_by_key(|account| account.id))
}

/// `https://host[:port]/...` → `host[:port]` — plus the same for `http://`
/// when the host is a hidden service, the one place plain http is a real
/// federation origin (mirrors [`plamenu_federation::is_federation_url`]).
///
/// Everything else stays `None`, and callers must keep treating `None` as
/// "not a federation origin", never as a comparable value: many call sites
/// compare two `host_of` results for same-origin, and before this learned
/// hidden-service hosts, two *different* `.onion` origins both mapped to
/// `None` and compared equal. It also fed `accounts.domain` = `""` for every
/// onion actor, which rendered handles as `@nina@` and broke every
/// handle-based flow downstream (mentions, reply prefill, dedup by acct).
#[must_use]
pub fn host_of(uri: &str) -> Option<&str> {
    let (rest, plain_http) = match uri.strip_prefix("https://") {
        Some(rest) => (rest, false),
        None => (uri.strip_prefix("http://")?, true),
    };
    let host = rest.split(['/', '?', '#']).next()?;
    if host.is_empty() {
        return None;
    }
    if plain_http {
        let bare = host.rsplit_once(':').map_or(host, |(bare, _)| bare);
        if !plamenu_federation::is_hidden_service(bare) {
            return None;
        }
    }
    Some(host)
}

#[cfg(test)]
mod tests {
    use super::{MAX_INFLIGHT_REFRESHES, RefreshKind, RemoteRefreshCoordinator, host_of};

    #[test]
    fn host_of_accepts_https_and_hidden_http_only() {
        assert_eq!(
            host_of("https://remote.example/users/a"),
            Some("remote.example")
        );
        assert_eq!(
            host_of("https://remote.example:8443/users/a"),
            Some("remote.example:8443")
        );
        assert_eq!(host_of("http://xyz.onion/users/nina"), Some("xyz.onion"));
        assert_eq!(
            host_of("http://xyz.onion:8080/users/nina"),
            Some("xyz.onion:8080")
        );
        assert_eq!(host_of("http://abc.i2p/users/a"), Some("abc.i2p"));

        // Plain http on the clearnet is not a federation origin…
        assert_eq!(host_of("http://remote.example/users/a"), None);
        // …and a clearnet host dressed up with an onion label stays refused.
        assert_eq!(host_of("http://xyz.onion.example.com/users/a"), None);
        assert_eq!(host_of("ftp://remote.example/x"), None);
        assert_eq!(host_of("https:///users/a"), None);
        assert_eq!(host_of("not a uri"), None);
    }

    /// The regression that motivated the hidden-service arm: two DIFFERENT
    /// onion origins must never compare as same-origin, which they did when
    /// both collapsed to `None`.
    #[test]
    fn host_of_distinguishes_two_onion_origins() {
        let a = host_of("http://aaa.onion/users/x");
        let b = host_of("http://bbb.onion/users/y");
        assert!(a.is_some() && b.is_some());
        assert_ne!(a, b);
    }

    // Coalescing + the freshness gate: one actor's refresh is admitted once, a
    // second (concurrent duplicate or immediate repeat) is refused, distinct
    // actors/kinds are independent, and the freshness stamp outlives the task.
    #[test]
    fn refresh_coordinator_coalesces_and_rate_limits() {
        let coord = RemoteRefreshCoordinator::default();

        let bob_tags = coord
            .try_admit(1, RefreshKind::FeaturedTags)
            .expect("first refresh admitted");
        assert_eq!(coord.in_flight_len(), 1);

        // A duplicate while the first is in flight is refused...
        assert!(
            coord.try_admit(1, RefreshKind::FeaturedTags).is_none(),
            "duplicate refresh for the same (account, kind) is coalesced"
        );
        // ...but a different kind or a different account is independent.
        let bob_outbox = coord
            .try_admit(1, RefreshKind::OutboxStats)
            .expect("a different kind for the same actor is admitted");
        let carol_tags = coord
            .try_admit(2, RefreshKind::FeaturedTags)
            .expect("a different actor is admitted");
        assert_eq!(coord.in_flight_len(), 3);

        // Dropping a guard frees its in-flight (global-cap) slot...
        drop(bob_tags);
        assert_eq!(coord.in_flight_len(), 2);
        // ...but the freshness stamp is kept, so an immediate repeat — the
        // sequential-flood case, where the task already finished — is refused.
        assert!(
            coord.try_admit(1, RefreshKind::FeaturedTags).is_none(),
            "an immediate repeat is refused by the freshness gate even after the task ends"
        );
        assert_eq!(coord.in_flight_len(), 2);

        drop(bob_outbox);
        drop(carol_tags);
        assert_eq!(coord.in_flight_len(), 0);
    }

    // The global cap bounds the live task population across *distinct* actors,
    // and a freed slot admits again.
    #[test]
    fn refresh_coordinator_enforces_the_global_cap() {
        let coord = RemoteRefreshCoordinator::default();

        // Distinct actors, all held, fill the cap. (Distinct keys, so neither
        // coalescing nor freshness fires first — only the cap can refuse.)
        let mut held: Vec<_> = (0..MAX_INFLIGHT_REFRESHES)
            .map(|id| {
                coord
                    .try_admit(i64::try_from(id).unwrap(), RefreshKind::FeaturedTags)
                    .expect("admitted below the cap")
            })
            .collect();
        assert_eq!(coord.in_flight_len(), MAX_INFLIGHT_REFRESHES);

        // A fresh distinct actor is now shed, not queued.
        let over = i64::try_from(MAX_INFLIGHT_REFRESHES).unwrap();
        assert!(
            coord.try_admit(over, RefreshKind::FeaturedTags).is_none(),
            "over the global cap a refresh is refused"
        );

        // Freeing a slot re-opens admission for a new distinct actor.
        held.pop();
        assert_eq!(coord.in_flight_len(), MAX_INFLIGHT_REFRESHES - 1);
        assert!(
            coord
                .try_admit(over + 1, RefreshKind::FeaturedTags)
                .is_some(),
            "a freed slot admits again"
        );
    }
}
