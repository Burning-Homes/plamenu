//! Authorized-fetch ("secure mode") support: when `authorized_fetch`
//! is enabled, server-to-server GETs of actors, statuses and collections
//! must carry a valid HTTP signature — Mastodon's `AUTHORIZED_FETCH`.
//!
//! Webfinger, nodeinfo and `/actor` stay public: the instance actor is how
//! two secure-mode servers bootstrap each other's keys.

use std::time::SystemTime;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use plamenu_ap::actor::RemoteActor;
use plamenu_db::account::Account;
use plamenu_federation::{PreparedRequestAuth, RequestFacts};

use crate::AppState;
use crate::error::ApiError;
use crate::remote::refresh_remote_actor;

/// Middleware for the `ActivityPub` GET endpoints: a no-op unless
/// authorized fetch is configured, then a 401 without a valid signature.
pub async fn gate(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if !state.authorized_fetch().await {
        return next.run(request).await;
    }
    // Authorized fetch protects the ActivityPub representation only. A browser
    // (HTML) request is served the public web page, so it bypasses the
    // signature check and reaches the handler — which redirects it accordingly.
    let accept = request
        .headers()
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !plamenu_ap::accepts_activity_json(accept) {
        return next.run(request).await;
    }
    match verify_get_request(&state, request.uri(), request.headers()).await {
        Ok(()) => next.run(request).await,
        Err(err) => err.into_response(),
    }
}

/// The [`gate`] middleware's check, applied inline by a handler that serves an
/// `ActivityPub` document off a route the middleware does not wrap — the pretty
/// `/@handle/{id}` and `/@handle/collections/{id}` web routes, which hand a
/// local AP fetch to the canonical (middleware-gated) `get_status` /
/// `get_collection`. A no-op unless secure mode is on; then a valid signature
/// is required, exactly as the gate would demand at the canonical URL. Callers
/// must have already confirmed the request wants `ActivityPub`.
pub(crate) async fn enforce(
    state: &AppState,
    uri: &Uri,
    headers: &HeaderMap,
) -> Result<(), ApiError> {
    if !state.authorized_fetch().await {
        return Ok(());
    }
    verify_get_request(state, uri, headers).await
}

/// Whether an inbound request carries an HTTP signature of either dialect —
/// RFC 9421 (`Signature-Input`) or draft-cavage (`Signature`). Used by the
/// actor route to tell an *unsigned* caller (served a key-only document under
/// secure mode) from a *present-but-invalid* one (still rejected).
pub(crate) fn request_is_signed(headers: &HeaderMap) -> bool {
    headers.contains_key("signature-input") || headers.contains_key("signature")
}

/// A GET-only authentication document, never a delivery actor or account.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct InternalFetchSignerDocument {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    preferred_username: String,
    public_key: plamenu_ap::actor::PublicKey,
    #[serde(default, rename = "inbox")]
    _inbox: Option<String>,
    #[serde(default, rename = "outbox")]
    _outbox: Option<String>,
}

fn internal_fetch_actor_uri(key_id: &str) -> Result<Option<&str>, ApiError> {
    let actor_uri = key_id.split('#').next().unwrap_or(key_id);
    let Ok(parsed) = url::Url::parse(actor_uri) else {
        return Ok(None);
    };
    if parsed.path() != "/internal/fetch" {
        return Ok(None);
    }
    if parsed.query().is_some() || !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(ApiError::Unauthorized(
            "invalid internal fetch signer URI".into(),
        ));
    }
    Ok(Some(actor_uri))
}

fn prepare_get_request(
    state: &AppState,
    uri: &Uri,
    headers: &HeaderMap,
) -> Result<PreparedRequestAuth, ApiError> {
    let path_and_query = uri
        .path_and_query()
        .map_or_else(|| uri.path(), |pq| pq.as_str());
    let target_uri = format!("https://{}{path_and_query}", state.config.domain);
    PreparedRequestAuth::from_get_request(
        &RequestFacts {
            method: "GET",
            target_uri: &target_uri,
            path_and_query,
            headers,
        },
        SystemTime::now(),
    )
    .map_err(|error| ApiError::Unauthorized(error.to_string()))
}

/// A fetch-only signer has an authenticated origin for origin-based policy,
/// but no Account id for follower, block or private conversation membership.
pub(crate) async fn verified_internal_fetch_domain(
    state: &AppState,
    uri: &Uri,
    headers: &HeaderMap,
) -> Result<Option<String>, ApiError> {
    if !request_is_signed(headers) {
        return Ok(None);
    }
    // Classify the Cavage key before date/crypto validation: a recognized
    // fetch-only principal must fail closed if revalidation subsequently fails.
    // Non-dedicated callers retain the existing anonymous fallback behavior.
    let key_id = if headers.contains_key("signature-input") {
        // RFC 9421 parsing includes freshness checks; propagate any failure
        // rather than discard a potentially dedicated authenticated origin.
        let prepared = prepare_get_request(state, uri, headers)?;
        prepared.key_id().to_owned()
    } else {
        let Some(signature) = headers
            .get("signature")
            .and_then(|value| value.to_str().ok())
        else {
            return Ok(None);
        };
        let Ok(params) = plamenu_federation::signature::parse_signature_header(signature) else {
            return Ok(None);
        };
        params.key_id
    };
    let Some(actor_uri) = internal_fetch_actor_uri(&key_id)? else {
        return Ok(None);
    };
    verify_get_request(state, uri, headers).await?;
    crate::instance_policy::domain_from_url(actor_uri)
        .map(Some)
        .ok_or_else(|| ApiError::Unauthorized("invalid internal fetch signer origin".into()))
}

/// Authenticate public AP GETs without assigning audience membership to a
/// fetch-only principal. Dedicated fetch actors never enter the account cache.
pub(crate) async fn verify_get_request(
    state: &AppState,
    uri: &Uri,
    headers: &HeaderMap,
) -> Result<(), ApiError> {
    let prepared = prepare_get_request(state, uri, headers)?;
    let Some(actor_uri) = internal_fetch_actor_uri(prepared.key_id())? else {
        return verified_get_sender(state, uri, headers).await.map(|_| ());
    };
    let signer_origin = url::Url::parse(actor_uri)
        .map_err(|_| ApiError::Unauthorized("invalid internal fetch signer URI".into()))?;
    let local_url = url::Url::parse(&format!("https://{}/", state.config.domain))
        .map_err(|_| ApiError::Unauthorized("invalid local signer origin".into()))?;
    if !plamenu_federation::is_federation_url(actor_uri)
        || signer_origin.host_str() == local_url.host_str()
    {
        return Err(ApiError::Unauthorized(
            "invalid internal fetch signer URI".into(),
        ));
    }
    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, actor_uri)
        .await?
    {
        return Err(ApiError::Forbidden("This domain is blocked".into()));
    }
    if let Some(previous) = plamenu_db::account::find_by_uri(&state.pool, actor_uri).await?
        && previous.suspended()
    {
        return Err(ApiError::Forbidden("This account is suspended".into()));
    }
    // Cavage's general verifier historically ignores the algorithm label.
    // Restrict only this new GET-only branch to supported RSA conventions.
    if !prepared.is_rfc9421() {
        let signature = headers
            .get("signature")
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| ApiError::Unauthorized("missing signature".into()))?;
        let params = plamenu_federation::signature::parse_signature_header(signature)
            .map_err(|error| ApiError::Unauthorized(error.to_string()))?;
        if !matches!(
            params.algorithm.as_deref(),
            None | Some("rsa-sha256" | "hs2019")
        ) {
            return Err(ApiError::Unauthorized(
                "unsupported internal fetch signature algorithm".into(),
            ));
        }
    }
    let object = state
        .federation
        .fetch_object(actor_uri)
        .await
        .map_err(|error| ApiError::Unauthorized(format!("cannot dereference signer: {error}")))?;
    let document: InternalFetchSignerDocument = serde_json::from_value(object)
        .map_err(|error| ApiError::Unauthorized(format!("cannot dereference signer: {error}")))?;
    // The same guarded fetch preserves SSRF, redirects, size/content limits
    // and exact object id. Never synthesize an inbox or persist this document.
    if document.id != actor_uri
        || document.kind != "Application"
        || document.preferred_username != "internal.fetch"
        || document.public_key.owner != document.id
        || document.public_key.id != prepared.key_id()
        || document.public_key.id.split('#').next() != Some(actor_uri)
        || prepared.wants_ed25519()
    {
        return Err(ApiError::Unauthorized(
            "invalid internal fetch signer document".into(),
        ));
    }
    // Fetch the current key each time: rotations refetch the same guarded URI,
    // stale keys cannot gain audience identity through account/key caches.
    prepared
        .verify_rsa_pem(&document.public_key.public_key_pem)
        .map_err(|error| ApiError::Unauthorized(error.to_string()))
}

/// Verifies the signature on a GET and resolves the signing actor,
/// dereferencing the keyId when the key is unknown or stale (key rotation).
pub(crate) async fn verified_get_sender(
    state: &AppState,
    uri: &Uri,
    headers: &HeaderMap,
) -> Result<Account, ApiError> {
    let prepared = prepare_get_request(state, uri, headers)?;
    let key_id = prepared.key_id();
    // Audience-sensitive callers require a real delivery/account identity.
    // Refuse fetch-only principals before consulting any persisted account.
    if internal_fetch_actor_uri(key_id)?.is_some() {
        return Err(ApiError::Unauthorized(
            "fetch-only signer has no account identity".into(),
        ));
    }

    if let Some(cached) = crate::remote::find_verified_cached_signer(&state.pool, &prepared).await?
    {
        if !crate::instance_policy::account_visible(&state.pool, &state.config.domain, &cached)
            .await?
        {
            return Err(ApiError::Forbidden("This domain is blocked".into()));
        }
        return Ok(cached);
    }

    // Unknown (or rotated) key: the keyId minus its fragment dereferences to
    // the owning actor. Never our own domain — local keys do not sign GETs
    // at us, and fetching ourselves is always wrong.
    let actor_uri = key_id.split('#').next().unwrap_or(key_id);
    if !plamenu_federation::is_federation_url(actor_uri)
        || actor_uri.starts_with(&format!("https://{}/", state.config.domain))
    {
        return Err(ApiError::Unauthorized(format!(
            "cannot resolve signature key {key_id}"
        )));
    }
    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, actor_uri)
        .await?
    {
        return Err(ApiError::Forbidden("This domain is blocked".into()));
    }
    let fetched = fetch_get_signer_actor(state, key_id, actor_uri).await?;
    // The key must really be the fetched actor's own. Fragment (and bare
    // pub-relay) keyIds fetch the actor directly; GoToSocial `/main-key`
    // keyIds are followed back to their owner actor above.
    if !fetched.owns_key(key_id) {
        return Err(ApiError::Unauthorized(format!(
            "keyId {key_id} does not belong to the claimed actor {} (declared key {})",
            fetched.id, fetched.public_key.id
        )));
    }
    let stored = refresh_remote_actor(state, &fetched).await?;
    crate::remote::prepared_matches_account(&state.pool, &prepared, &stored)
        .await?
        .map_err(|e| ApiError::Unauthorized(format!("{e} (keyId {key_id})")))?;
    Ok(stored)
}

async fn fetch_get_signer_actor(
    state: &AppState,
    key_id: &str,
    actor_uri: &str,
) -> Result<RemoteActor, ApiError> {
    // A GoToSocial `/main-key` endpoint is deliberately an incomplete actor
    // stub. Trying to parse it as an actor first both cannot succeed and, in
    // the real transport, records a resource failure that suppresses the
    // canonical-object fallback below. Resolve the stub in one guarded fetch.
    if actor_uri != key_id || !key_id.ends_with("/main-key") {
        match state.federation.fetch_actor(actor_uri).await {
            Ok(actor) => return Ok(actor),
            Err(err) if actor_uri != key_id => {
                return Err(ApiError::Unauthorized(format!(
                    "cannot dereference signer: {err}"
                )));
            }
            Err(_) => {}
        }
    }

    // GoToSocial keyIds use `/main-key` instead of a `#main-key` fragment. The
    // key endpoint is a public stub whose `id` is the owning actor, so follow
    // that id once and parse the final actor document.
    let object = state
        .federation
        .fetch_object_following(key_id)
        .await
        .map_err(|e| ApiError::Unauthorized(format!("cannot dereference signer: {e}")))?;
    let actor = serde_json::from_value::<RemoteActor>(object)
        .map_err(|e| ApiError::Unauthorized(format!("cannot dereference signer: {e}")))?;
    if actor.preferred_username.is_empty() && actor.webfinger_acct().is_none() {
        return Err(ApiError::Unauthorized(
            "cannot dereference signer: actor has neither preferredUsername nor webfinger".into(),
        ));
    }
    Ok(actor)
}
