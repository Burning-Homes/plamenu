//! FEP-ae97 client-side signing gateway, including Minimitra's FEP-ef61
//! compatible identifiers and media API.

use std::time::SystemTime;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use plamenu_ap::activity::{Activity, PUBLIC, id_of, one_or_many};
use plamenu_ap::actor::RemoteActor;
use plamenu_ap::proof::PreparedProof;
use plamenu_db::account::Account;
use plamenu_db::{account, actor_key, block, follow, gateway, instance_settings, invite, job};
use plamenu_federation::{PreparedRequestAuth, RequestFacts};
use serde_json::{Value, json};

use crate::AppState;
use crate::error::ApiError;

const GATEWAY_PATH: &str = "/.well-known/apgateway";
const MEDIA_PATH: &str = "/.well-known/apgateway-media";
const PAGE_SIZE: i64 = 20;
const MAX_AUDIENCE: usize = 200;

fn gateway_base(domain: &str) -> String {
    format!("https://{domain}{GATEWAY_PATH}")
}

fn request_target(domain: &str, uri: &Uri) -> String {
    format!(
        "https://{domain}{}",
        uri.path_and_query()
            .map_or(uri.path(), |value| value.as_str())
    )
}

fn resource_uri(domain: &str, uri: &Uri) -> String {
    format!("https://{domain}{}", uri.path())
}

fn bad_registration(reason: impl Into<String>) -> ApiError {
    ApiError::BadRequest(reason.into())
}

/// Returns the portable DID that owns an object at this gateway.
fn portable_did(id: &str, domain: &str) -> Result<String, ApiError> {
    let prefix = format!("{}/", gateway_base(domain));
    let rest = id
        .strip_prefix(&prefix)
        .ok_or_else(|| ApiError::Forbidden("object is not owned by this gateway".into()))?;
    let did = rest
        .split('/')
        .next()
        .filter(|did| did.starts_with("did:key:z6Mk"))
        .ok_or_else(|| ApiError::BadRequest("invalid portable object ID".into()))?;
    let multikey = did
        .strip_prefix("did:key:")
        .ok_or_else(|| ApiError::BadRequest("invalid portable DID".into()))?;
    plamenu_ap::multikey::decode_ed25519_public(multikey)
        .map_err(|_| ApiError::BadRequest("portable DID is not an Ed25519 did:key".into()))?;
    Ok(did.to_owned())
}

fn canonical_portable_uri(id: &str, domain: &str) -> Result<String, ApiError> {
    let relative = id
        .strip_prefix(&format!("{}/", gateway_base(domain)))
        .ok_or_else(|| ApiError::BadRequest("invalid compatible portable ID".into()))?;
    portable_did(id, domain)?;
    Ok(format!("ap://{relative}"))
}

/// Verifies an FEP-ef61 self-certifying object and returns its ID and DID.
fn verify_portable(document: &Value, domain: &str) -> Result<(String, String), ApiError> {
    let id = document
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::BadRequest("portable object has no ID".into()))?;
    let did = portable_did(id, domain)?;
    let proof = PreparedProof::from_document(document)
        .map_err(|error| ApiError::Forbidden(format!("invalid portable proof: {error}")))?;
    let proof_did = proof
        .verification_method()
        .split('#')
        .next()
        .unwrap_or_default();
    if proof_did != did {
        return Err(ApiError::Forbidden(
            "portable object ID and proof signer do not match".into(),
        ));
    }
    let multikey = did.trim_start_matches("did:key:");
    proof
        .verify(multikey)
        .map_err(|error| ApiError::Forbidden(format!("invalid portable proof: {error}")))?;
    Ok((id.to_owned(), did))
}

fn gateway_is_listed(actor: &Value, domain: &str) -> bool {
    one_or_many(actor.get("gateways")).iter().any(|entry| {
        id_of(entry)
            .is_some_and(|gateway| gateway.trim_end_matches('/') == format!("https://{domain}"))
    })
}

/// FEP-2277 shape checks used by FEP-ae97's origin-security requirement.
/// Outside the one registered actor document, a client must not make a
/// gateway-origin object dereference as an actor or verification method.
fn claims_server_controlled_resource(document: &Value, domain: &str) -> bool {
    if (document.get("inbox").is_some() && document.get("outbox").is_some())
        || document.get("publicKeyMultibase").is_some()
        || document.get("publicKeyPem").is_some()
    {
        return true;
    }
    let local_origin = format!("https://{domain}/");
    ["assertionMethod", "publicKey"].iter().any(|field| {
        one_or_many(document.get(*field))
            .iter()
            .filter_map(|method| id_of(method))
            .any(|id| id.starts_with(&local_origin))
    })
}

/// Parsed registration/update facts whose borrowed key strings are owned by
/// this value (avoids retaining `RemoteVerificationMethod` temporaries).
struct ValidatedActor {
    parsed: RemoteActor,
    client_key_id: String,
    client_public_key: String,
}

fn validate_actor_document(
    raw: &Value,
    domain: &str,
    registered: Option<&gateway::GatewayActor>,
) -> Result<ValidatedActor, ApiError> {
    let (actor_id, _did) = verify_portable(raw, domain)?;
    let actor: RemoteActor = serde_json::from_value(raw.clone())
        .map_err(|error| bad_registration(format!("invalid actor: {error}")))?;
    let expected_outbox = format!("{actor_id}/outbox");
    if actor.id != actor_id
        || !matches!(
            actor.kind.as_str(),
            "Person" | "Service" | "Application" | "Group"
        )
        || actor.preferred_username.trim().is_empty()
        || actor.preferred_username.chars().count() > 64
        || !gateway_is_listed(raw, domain)
        || actor.inbox != format!("{actor_id}/inbox")
        || actor.outbox_url() != Some(expected_outbox.as_str())
    {
        return Err(bad_registration("actor is not valid for this gateway"));
    }

    let methods = actor
        .verification_methods()
        .map_err(|error| bad_registration(format!("invalid actor keys: {error}")))?;
    let canonical_actor_id = canonical_portable_uri(actor_id.as_str(), domain)?;
    let expected_client_key_id = format!("{canonical_actor_id}#main-key");
    let main = methods
        .iter()
        .find(|method| {
            method.key_uri == expected_client_key_id
                && method.algorithm.as_deref() == Some("rsa")
                && !method.revoked
        })
        .and_then(|method| {
            method
                .public_key
                .as_ref()
                .map(|key| (method.key_uri.clone(), key.clone()))
        })
        .ok_or_else(|| bad_registration("actor has no usable client RSA #main-key"))?;

    let gateway_key_id = format!("{actor_id}#gateway-rsa");
    let published_gateway = methods
        .iter()
        .find(|method| method.key_uri == gateway_key_id);
    match registered {
        None if published_gateway.is_some() => {
            return Err(bad_registration(
                "unregistered actor claims a server-controlled gateway key",
            ));
        }
        Some(stored) => {
            if actor.preferred_username != stored.username {
                return Err(bad_registration("actor username cannot be replaced"));
            }
            if main.0 != stored.client_rsa_key_id || main.1 != stored.client_rsa_public_key {
                return Err(bad_registration("actor client key cannot be replaced"));
            }
            let expected =
                plamenu_ap::multikey::decode_rsa_public(&stored.gateway_rsa_public_multikey)
                    .map_err(|error| ApiError::Internal(Box::new(error)))?;
            let valid = published_gateway.is_some_and(|method| {
                method.algorithm.as_deref() == Some("rsa")
                    && method.public_key.as_deref() == Some(expected.as_str())
                    && method.controller_uri == actor_id
                    && !method.revoked
            });
            if !valid {
                return Err(bad_registration(
                    "actor does not publish this gateway's RSA key",
                ));
            }
        }
        None => {}
    }

    Ok(ValidatedActor {
        parsed: actor,
        client_key_id: main.0,
        client_public_key: main.1,
    })
}

pub async fn metadata(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "uploadMedia": format!("https://{}{MEDIA_PATH}", state.config.domain),
    }))
}

fn gateway_keys(actor: &gateway::GatewayActor) -> Value {
    json!({
        "assertionMethod": [{
            "id": format!("{}#gateway-rsa", actor.actor_uri),
            "type": "Multikey",
            "controller": actor.actor_uri,
            "publicKeyMultibase": actor.gateway_rsa_public_multikey,
        }],
    })
}

async fn registration_invite(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<i64>, ApiError> {
    let settings = state.settings_cache.get(&state.pool).await?;
    if settings.registrations_mode() == instance_settings::RegistrationsMode::Open {
        return Ok(None);
    }
    let code = headers
        .get("x-invite-code")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| bad_registration("invite code is required"))?;
    invite::find_valid_by_code(&state.pool, code)
        .await?
        .map(|invite| Some(invite.id))
        .ok_or_else(|| bad_registration("invalid invite code"))
}

#[allow(
    clippy::too_many_lines,
    reason = "registration validates, provisions, and atomically links one portable actor"
)]
pub async fn register(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(raw): Json<Value>,
) -> Result<Response, ApiError> {
    // Validate the portable proof and client key even on the idempotent path;
    // knowledge of an actor URI alone must never disclose its gateway key.
    let claimed_id = raw
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| bad_registration("portable actor has no ID"))?;
    if let Some(existing) = gateway::find_by_actor_uri(&state.pool, claimed_id).await? {
        let existing_account = account::find_by_id(&state.pool, existing.account_id)
            .await?
            .ok_or(ApiError::Gone)?;
        if existing_account.suspended() {
            return Err(ApiError::Forbidden(
                "portable account is suspended on this gateway".into(),
            ));
        }
        // Minimitra deliberately strips previously returned gateway keys before
        // re-registering; other clients may send their current actor. Accept
        // both, but never a wrong server key.
        let validated = validate_actor_document(&raw, &state.config.domain, None)
            .or_else(|_| validate_actor_document(&raw, &state.config.domain, Some(&existing)))
            .map_err(|error| registration_error(claimed_id, error))?;
        if validated.client_key_id != existing.client_rsa_key_id
            || validated.client_public_key != existing.client_rsa_public_key
        {
            return Err(bad_registration("actor client key cannot be replaced"));
        }
        return Ok((StatusCode::OK, Json(gateway_keys(&existing))).into_response());
    }
    let initial = validate_actor_document(&raw, &state.config.domain, None)
        .map_err(|error| registration_error(claimed_id, error))?;

    if account::local_username_reserved(&state.pool, &initial.parsed.preferred_username).await? {
        return Err(bad_registration("actor handle is already registered"));
    }

    let invite_id = registration_invite(&state, &headers).await?;
    let rsa = crate::auth::generate_keypair_gated().await?;
    let gateway_multikey =
        plamenu_ap::multikey::encode_rsa_public(&rsa.public_pem).ok_or_else(|| {
            ApiError::Internal(Box::new(std::io::Error::other(
                "generated RSA key is not serializable",
            )))
        })?;

    // Reuse the mature remote-actor projection for normalized client keys and
    // same-instance delivery. The raw signed document remains authoritative
    // in `gateway_actors` and is what gateway GETs serve.
    let account_preexisted = account::find_by_uri(&state.pool, &initial.parsed.id)
        .await?
        .is_some();
    let account = crate::remote::store_remote_actor(&state.pool, &initial.parsed).await?;
    account::set_collection_urls(
        &state.pool,
        account.id,
        initial.parsed.followers_url().unwrap_or_default(),
        initial.parsed.following_url().unwrap_or_default(),
        initial.parsed.outbox_url().unwrap_or_default(),
    )
    .await?;

    let keyring = state.federation_keyring.as_deref().ok_or_else(|| {
        ApiError::Internal(Box::new(
            crate::crypto::KeyEncryptionError::MissingConfiguration,
        ))
    })?;
    let setup = async {
        let mut tx = state
            .pool
            .begin()
            .await
            .map_err(plamenu_db::DbError::from)?;
        actor_key::lock_account_owner(&mut tx, account.id).await?;
        let inserted = gateway::insert(
            &mut *tx,
            gateway::NewGatewayActor {
                account_id: account.id,
                actor_uri: &initial.parsed.id,
                inbox_uri: &initial.parsed.inbox,
                outbox_uri: initial.parsed.outbox_url().unwrap_or_default(),
                username: &initial.parsed.preferred_username,
                actor: &raw,
                client_rsa_key_id: &initial.client_key_id,
                client_rsa_public_key: &initial.client_public_key,
                gateway_rsa_public_multikey: &gateway_multikey,
            },
        )
        .await
        .map_err(|error| match error {
            plamenu_db::DbError::UsernameTaken => {
                bad_registration("actor handle is already registered")
            }
            other => ApiError::from(other),
        })?;
        if inserted {
            crate::key_store::provision_gateway_rsa_tx(
                &mut tx,
                keyring,
                &account,
                &initial.parsed.id,
                &rsa,
            )
            .await
            .map_err(|error| ApiError::Internal(Box::new(error)))?;
            gateway::mark_account_portable(&mut *tx, account.id)
                .await
                .map_err(|error| match error {
                    plamenu_db::DbError::UsernameTaken => {
                        bad_registration("actor handle is already registered")
                    }
                    other => ApiError::from(other),
                })?;
            if let Some(invite_id) = invite_id {
                sqlx::query("UPDATE invites SET uses = uses + 1 WHERE id = $1")
                    .bind(invite_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(plamenu_db::DbError::from)?;
            }
        }
        tx.commit().await.map_err(plamenu_db::DbError::from)?;
        Ok::<_, ApiError>(inserted)
    }
    .await;
    let inserted = match setup {
        Ok(inserted) => inserted,
        Err(error) => {
            // `store_remote_actor` commits before gateway-specific state. A
            // failed first registration must not leave that cache-only row
            // behind; a pre-existing/federated account is not ours to remove.
            if !account_preexisted
                && gateway::find_by_actor_uri(&state.pool, &initial.parsed.id)
                    .await?
                    .is_none()
                && let Err(cleanup_error) =
                    account::delete_by_uri(&state.pool, &initial.parsed.id).await
            {
                tracing::warn!(error = %cleanup_error, actor = %initial.parsed.id, "failed to clean up rejected portable registration");
            }
            return Err(error);
        }
    };

    if inserted
        && let Err(error) =
            crate::registration::notify_staff_about_account_signup(&state, account.id).await
    {
        tracing::warn!(error = %error.chain(), account = account.id, "portable sign-up staff notification failed");
    }

    let stored = gateway::find_by_actor_uri(&state.pool, &initial.parsed.id)
        .await?
        .ok_or_else(|| bad_registration("actor handle is already registered"))?;
    let status = if inserted {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(gateway_keys(&stored))).into_response())
}

fn registration_error(actor_id: &str, error: ApiError) -> ApiError {
    tracing::warn!(actor_id, error = %error, "portable actor registration rejected");
    match error {
        ApiError::Internal(_) => error,
        other => bad_registration(other.to_string()),
    }
}

async fn prepared_client_request(
    state: &AppState,
    uri: &Uri,
    headers: &HeaderMap,
    body: Option<&[u8]>,
    allow_query_omission: bool,
) -> Result<(PreparedRequestAuth, gateway::GatewayActor), ApiError> {
    let path_and_query = uri
        .path_and_query()
        .map_or(uri.path(), |value| value.as_str());
    let target = request_target(&state.config.domain, uri);
    let facts = RequestFacts {
        method: if body.is_some() { "POST" } else { "GET" },
        target_uri: &target,
        path_and_query,
        headers,
    };
    let prepared = match body {
        Some(body) => PreparedRequestAuth::from_post_request(&facts, body, SystemTime::now()),
        None => PreparedRequestAuth::from_bodyless_request(
            &facts,
            SystemTime::now(),
            allow_query_omission,
        ),
    }
    .map_err(|error| ApiError::Unauthorized(error.to_string()))?;
    let signer = gateway::find_by_client_key_id(&state.pool, prepared.key_id())
        .await?
        .ok_or_else(|| ApiError::Unauthorized("signature key is not registered".into()))?;
    prepared
        .verify_rsa_pem(&signer.client_rsa_public_key)
        .map_err(|error| ApiError::Unauthorized(error.to_string()))?;
    Ok((prepared, signer))
}

async fn signed_client_for_collection(
    state: &AppState,
    owner: &gateway::GatewayActor,
    uri: &Uri,
    headers: &HeaderMap,
) -> Result<(), ApiError> {
    let (_, signer) = prepared_client_request(state, uri, headers, None, true).await?;
    if signer.account_id != owner.account_id {
        return Err(ApiError::Forbidden(
            "signature actor is not the collection owner".into(),
        ));
    }
    ensure_active(state, owner).await?;
    Ok(())
}

async fn ensure_active(
    state: &AppState,
    owner: &gateway::GatewayActor,
) -> Result<Account, ApiError> {
    let account = account::find_by_id(&state.pool, owner.account_id)
        .await?
        .ok_or(ApiError::Gone)?;
    if account.suspended() {
        return Err(ApiError::Forbidden(
            "portable account is suspended on this gateway".into(),
        ));
    }
    Ok(account)
}

fn query_after(uri: &Uri) -> Option<String> {
    url::form_urlencoded::parse(uri.query()?.as_bytes())
        .find(|(key, _)| key == "after")
        .map(|(_, value)| value.into_owned())
}

fn collection_json(id: &str, owner: &gateway::GatewayActor, items: &[Value]) -> Value {
    json!({
        "@context": plamenu_ap::AS_CONTEXT,
        "id": id,
        "type": "OrderedCollection",
        "attributedTo": owner.actor_uri,
        "orderedItems": items,
    })
}

pub async fn get(
    State(state): State<AppState>,
    Path(_path): Path<String>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let resource = resource_uri(&state.config.domain, &uri);
    if resource.ends_with("/inbox") {
        let owner = gateway::find_by_inbox_uri(&state.pool, &resource)
            .await?
            .ok_or(ApiError::NotFound)?;
        signed_client_for_collection(&state, &owner, &uri, &headers).await?;
        let items = gateway::collection_items(
            &state.pool,
            owner.account_id,
            "inbox",
            query_after(&uri).as_deref(),
            PAGE_SIZE,
        )
        .await?;
        return Ok((
            [(header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8)],
            Json(collection_json(
                &request_target(&state.config.domain, &uri),
                &owner,
                &items,
            )),
        )
            .into_response());
    }
    if resource.ends_with("/outbox") {
        let owner = gateway::find_by_outbox_uri(&state.pool, &resource)
            .await?
            .ok_or(ApiError::NotFound)?;
        let signed = headers.contains_key("signature") || headers.contains_key("signature-input");
        let items = if signed {
            signed_client_for_collection(&state, &owner, &uri, &headers).await?;
            gateway::collection_items(
                &state.pool,
                owner.account_id,
                "outbox",
                query_after(&uri).as_deref(),
                PAGE_SIZE,
            )
            .await?
        } else {
            Vec::new()
        };
        return Ok((
            [(header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8)],
            Json(collection_json(&resource, &owner, &items)),
        )
            .into_response());
    }
    let value = if let Some(actor) = gateway::find_by_actor_uri(&state.pool, &resource).await? {
        actor.actor
    } else {
        gateway::find_object(&state.pool, &resource)
            .await?
            .ok_or(ApiError::NotFound)?
    };
    Ok((
        [(header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8)],
        Json(value),
    )
        .into_response())
}

fn collect_ids(value: Option<&Value>, output: &mut Vec<String>) {
    for item in one_or_many(value) {
        if let Some(id) = id_of(item)
            && !output.iter().any(|known| known == id)
        {
            output.push(id.to_owned());
        }
    }
}

fn intended_audience(activity: &Value) -> Vec<String> {
    let mut audience = Vec::new();
    for field in ["to", "cc", "bto", "bcc", "audience"] {
        collect_ids(activity.get(field), &mut audience);
    }
    let object = activity.get("object");
    if let Some(object) = object.filter(|value| value.is_object()) {
        for field in ["to", "cc", "bto", "bcc", "audience"] {
            collect_ids(object.get(field), &mut audience);
        }
    }
    match activity.get("type").and_then(Value::as_str) {
        Some("Follow" | "Block") => collect_ids(object, &mut audience),
        Some("Accept" | "Reject") => {
            if let Some(inner) = object.filter(|value| value.is_object()) {
                collect_ids(inner.get("actor"), &mut audience);
            }
        }
        Some("Undo") => {
            if let Some(inner) = object.filter(|value| value.is_object())
                && matches!(
                    inner.get("type").and_then(Value::as_str),
                    Some("Follow" | "Block")
                )
            {
                collect_ids(inner.get("object"), &mut audience);
            }
        }
        Some("Move" | "Add" | "Remove") => collect_ids(activity.get("target"), &mut audience),
        _ => {}
    }
    audience
}

async fn resolve_account(state: &AppState, uri: &str) -> Result<Account, ApiError> {
    if crate::local_identity::has_local_actor_shape(&state.config.domain, uri)
        && let Some(local) =
            crate::local_identity::find_actor(&state.pool, &state.config.domain, uri).await?
    {
        return Ok(local);
    }
    if let Some(known) = account::find_by_uri(&state.pool, uri).await? {
        return Ok(known);
    }
    let fetched = state.federation.fetch_actor(uri).await.map_err(|error| {
        ApiError::BadGateway(format!("cannot resolve recipient {uri}: {error}"))
    })?;
    crate::remote::refresh_remote_actor(state, &fetched)
        .await
        .map_err(ApiError::from)
}

fn delivery_inbox_url(account: &Account) -> Option<String> {
    if account.is_local() {
        // The authenticated dispatcher projects the activity directly for a
        // Plamenu-owned actor. Posting it back through our public inbox would
        // duplicate notifications and relationship responses.
        None
    } else {
        (!account.preferred_inbox().is_empty()).then(|| account.preferred_inbox().to_owned())
    }
}

async fn delivery_inboxes(
    state: &AppState,
    owner: &gateway::GatewayActor,
    activity: &Value,
) -> Result<Vec<String>, ApiError> {
    let audience = intended_audience(activity);
    if audience.len() > MAX_AUDIENCE {
        return Err(ApiError::BadRequest(
            "activity audience is too large".into(),
        ));
    }
    let followers = format!("{}/followers", owner.actor_uri);
    let mut inboxes = Vec::new();
    for target in audience {
        if target == PUBLIC || target == owner.actor_uri {
            continue;
        }
        if target == followers {
            inboxes.extend(gateway::follower_inboxes(&state.pool, owner.account_id).await?);
            continue;
        }
        let recipient = resolve_account(state, &target).await?;
        if let Some(inbox) = delivery_inbox_url(&recipient) {
            inboxes.push(inbox);
        }
    }
    inboxes.sort();
    inboxes.dedup();
    Ok(inboxes)
}

fn portable_embedded(activity: &Value) -> Option<&Value> {
    matches!(
        activity.get("type").and_then(Value::as_str),
        Some("Create" | "Update")
    )
    .then(|| activity.get("object"))
    .flatten()
    .filter(|object| object.is_object())
}

async fn project_outgoing_relationship(
    executor: &mut sqlx::PgConnection,
    owner: &gateway::GatewayActor,
    activity: &Value,
) -> Result<(), plamenu_db::DbError> {
    let Some(kind) = activity.get("type").and_then(Value::as_str) else {
        return Ok(());
    };
    if !matches!(kind, "Accept" | "Reject") {
        return Ok(());
    }
    let Some(object) = activity.get("object") else {
        return Ok(());
    };
    let referenced;
    let follow = if object.is_object() {
        object
    } else if let Some(uri) = id_of(object) {
        referenced =
            gateway::find_collection_item(&mut *executor, owner.account_id, "inbox", uri).await?;
        let Some(follow) = referenced.as_ref() else {
            return Ok(());
        };
        follow
    } else {
        return Ok(());
    };
    if follow.get("type").and_then(Value::as_str) != Some("Follow")
        || id_of(follow.get("object").unwrap_or(&Value::Null)) != Some(owner.actor_uri.as_str())
    {
        return Ok(());
    }
    let Some(follower) = follow.get("actor").and_then(id_of) else {
        return Ok(());
    };
    let follower_account_id: Option<i64> =
        sqlx::query_scalar("SELECT id FROM accounts WHERE uri = $1")
            .bind(follower)
            .fetch_optional(&mut *executor)
            .await?;
    match kind {
        "Accept" => {
            gateway::set_follower_accepted(&mut *executor, owner.account_id, follower, true)
                .await?;
            if let Some(follower_account_id) = follower_account_id {
                follow::mark_accepted(&mut *executor, follower_account_id, owner.account_id)
                    .await?;
            }
            Ok(())
        }
        "Reject" => {
            gateway::remove_follower(&mut *executor, owner.account_id, follower).await?;
            if let Some(follower_account_id) = follower_account_id {
                follow::delete(&mut *executor, follower_account_id, owner.account_id).await?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn embedded_kind(activity: &Activity) -> Option<&str> {
    activity.object.get("type").and_then(Value::as_str)
}

/// Projects relationships initiated by a portable client. Returns `true`
/// when the generic inbound dispatcher must be skipped: remote/portable
/// targets control their own follow verdicts, while an ordinary Plamenu-owned
/// local target can use the established auto-accept/request behavior.
async fn project_portable_outgoing(
    state: &AppState,
    owner: &gateway::GatewayActor,
    activity: &Activity,
) -> Result<bool, ApiError> {
    match (activity.kind.as_str(), embedded_kind(activity)) {
        ("Follow", _) => {
            let target_uri = activity
                .object_id()
                .ok_or_else(|| ApiError::BadRequest("Follow has no target".into()))?;
            let target = resolve_account(state, target_uri).await?;
            if target.is_local() {
                return Ok(false);
            }
            let uri = activity
                .id
                .as_deref()
                .ok_or_else(|| ApiError::BadRequest("Follow has no ID".into()))?;
            follow::create_outgoing(&state.pool, owner.account_id, target.id, uri).await?;
            Ok(true)
        }
        ("Undo", Some("Follow")) => {
            let target_uri = activity
                .object
                .get("object")
                .and_then(id_of)
                .ok_or_else(|| ApiError::BadRequest("Undo(Follow) has no target".into()))?;
            let target = resolve_account(state, target_uri).await?;
            if target.is_local() {
                return Ok(false);
            }
            follow::delete(&state.pool, owner.account_id, target.id).await?;
            Ok(true)
        }
        ("Block", _) => {
            let target_uri = activity
                .object_id()
                .ok_or_else(|| ApiError::BadRequest("Block has no target".into()))?;
            let target = resolve_account(state, target_uri).await?;
            if target.is_local() {
                return Ok(false);
            }
            follow::delete(&state.pool, owner.account_id, target.id).await?;
            follow::delete(&state.pool, target.id, owner.account_id).await?;
            if let Some(target_uri) = target.uri.as_deref() {
                gateway::remove_follower(&state.pool, owner.account_id, target_uri).await?;
            }
            block::create(
                &state.pool,
                owner.account_id,
                target.id,
                activity.id.as_deref(),
            )
            .await?;
            Ok(true)
        }
        ("Undo", Some("Block")) => {
            let target_uri = activity
                .object
                .get("object")
                .and_then(id_of)
                .ok_or_else(|| ApiError::BadRequest("Undo(Block) has no target".into()))?;
            let target = resolve_account(state, target_uri).await?;
            if target.is_local() {
                return Ok(false);
            }
            block::delete(&state.pool, owner.account_id, target.id).await?;
            Ok(true)
        }
        ("Accept" | "Reject", Some("Follow")) => Ok(true),
        // A bare Follow id is resolved by `project_outgoing_relationship`
        // from the durable inbox collection.
        ("Accept" | "Reject", None) if activity.object_id().is_some() => Ok(true),
        // Deleting the actor is a gateway deregistration operation, not a
        // remote actor deletion to project before the outbox row is durable.
        ("Delete", _) if activity.object_id() == Some(owner.actor_uri.as_str()) => Ok(true),
        _ => Ok(false),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one FEP-ae97 validation and persistence transaction is kept auditable in order"
)]
async fn push_outbox(
    state: &AppState,
    owner: gateway::GatewayActor,
    raw: Value,
) -> Result<StatusCode, ApiError> {
    if crate::webxdc_realtime::is_packet(&raw) {
        return Err(ApiError::BadRequest(
            "ephemeral Webxdc packets require a live host channel".into(),
        ));
    }
    let sender = ensure_active(state, &owner).await?;
    let (activity_uri, signer_did) = verify_portable(&raw, &state.config.domain)?;
    let activity: Activity = serde_json::from_value(raw.clone())
        .map_err(|_| ApiError::BadRequest("body is not an activity".into()))?;
    if claims_server_controlled_resource(&raw, &state.config.domain) {
        return Err(ApiError::Forbidden(
            "activity claims a server-controlled resource".into(),
        ));
    }
    let actor_id = activity
        .actor_id()
        .ok_or_else(|| ApiError::BadRequest("activity has no actor".into()))?;
    if actor_id != owner.actor_uri {
        return Err(ApiError::Forbidden(
            "activity actor is not the outbox owner".into(),
        ));
    }
    if portable_did(actor_id, &state.config.domain)? != signer_did {
        return Err(ApiError::Forbidden(
            "activity and actor portable identities differ".into(),
        ));
    }

    let embedded = portable_embedded(&raw);
    let mut update_actor = None;
    if let Some(object) = embedded {
        let (object_uri, embedded_did) = verify_portable(object, &state.config.domain)?;
        if embedded_did != signer_did {
            return Err(ApiError::Forbidden(
                "embedded object belongs to another portable identity".into(),
            ));
        }
        if object_uri == owner.actor_uri {
            if activity.kind != "Update" {
                return Err(ApiError::BadRequest(
                    "the actor document can only be submitted in Update".into(),
                ));
            }
            validate_actor_document(object, &state.config.domain, Some(&owner))?;
            update_actor = Some(object);
        } else {
            if claims_server_controlled_resource(object, &state.config.domain) {
                return Err(ApiError::Forbidden(
                    "object claims a server-controlled resource".into(),
                ));
            }
            let owners: Vec<&str> = one_or_many(object.get("attributedTo"))
                .iter()
                .filter_map(id_of)
                .collect();
            if !owners.contains(&owner.actor_uri.as_str()) {
                return Err(ApiError::Forbidden(
                    "embedded object is not attributed to the outbox owner".into(),
                ));
            }
        }
    }

    if embedded
        .and_then(|object| object.get("id"))
        .and_then(Value::as_str)
        == Some(activity_uri.as_str())
    {
        return Err(ApiError::BadRequest(
            "activity and embedded object must have different IDs".into(),
        ));
    }

    let inboxes = delivery_inboxes(state, &owner, &raw).await?;
    // Serialize every portable URI before any social projection. Without this
    // preflight, a reused activity/object ID could mutate follows or statuses
    // and only then lose the final uniqueness race in the gateway store.
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let mut locked_uris = vec![activity_uri.as_str()];
    if let Some(object_uri) = embedded
        .and_then(|object| object.get("id"))
        .and_then(Value::as_str)
    {
        locked_uris.push(object_uri);
    }
    locked_uris.sort_unstable();
    locked_uris.dedup();
    for object_uri in locked_uris {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(object_uri)
            .execute(&mut *tx)
            .await
            .map_err(plamenu_db::DbError::from)?;
    }
    if gateway::collection_item_owner(&mut *tx, &activity_uri)
        .await?
        .is_some()
        || gateway::object_owner(&mut *tx, &activity_uri)
            .await?
            .is_some()
    {
        return Err(ApiError::BadRequest(
            "activity ID has already been used".into(),
        ));
    }
    if let Some(object_uri) = embedded
        .and_then(|object| object.get("id"))
        .and_then(Value::as_str)
        && let Some(existing_owner) = gateway::object_owner(&mut *tx, object_uri).await?
        && (activity.kind != "Update" || existing_owner != owner.account_id)
    {
        return Err(ApiError::BadRequest(
            "object ID has already been used".into(),
        ));
    }

    let projected = project_portable_outgoing(state, &owner, &activity).await?;
    if !projected {
        super::inbox::dispatch_authenticated(state, &sender, &activity, &raw)
            .await
            .map_err(|error| {
                tracing::error!(%error, kind = %activity.kind, "portable activity projection failed");
                error
            })?;
    }
    if !gateway::insert_collection_item(&mut *tx, owner.account_id, "outbox", &activity_uri, &raw)
        .await?
    {
        return Err(ApiError::BadRequest(
            "activity ID has already been used".into(),
        ));
    }
    gateway::put_object(&mut *tx, owner.account_id, &activity_uri, &raw).await?;
    if let Some(object) = embedded {
        let object_id = object["id"].as_str().unwrap_or_default();
        if update_actor.is_some() || activity.kind == "Update" {
            gateway::replace_object(&mut *tx, owner.account_id, object_id, object).await?;
        } else if !gateway::put_object(&mut *tx, owner.account_id, object_id, object).await? {
            return Err(ApiError::BadRequest(
                "object ID has already been used".into(),
            ));
        }
    }
    if let Some(actor) = update_actor {
        gateway::update_actor(&mut *tx, owner.account_id, actor).await?;
    }
    project_outgoing_relationship(&mut tx, &owner, &raw).await?;
    job::enqueue_many_tx(&mut *tx, owner.account_id, &inboxes, &raw).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(StatusCode::ACCEPTED)
}

async fn project_incoming_relationship(
    state: &AppState,
    owner: &gateway::GatewayActor,
    signer: &Account,
    activity: &Value,
) -> Result<(), ApiError> {
    let kind = activity
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if kind == "Follow"
        && id_of(activity.get("object").unwrap_or(&Value::Null)) == Some(&owner.actor_uri)
    {
        // Plamenu-owned followers consume portable posts from the ordinary
        // status/follow graph; sending back into our own HTTP inbox would be a
        // loop. Remote and portable followers additionally need raw gateway
        // fan-out for their independently operated clients.
        if let Some(inbox) = delivery_inbox_url(signer) {
            gateway::upsert_follower(
                &state.pool,
                owner.account_id,
                signer.uri.as_deref().unwrap_or_default(),
                &inbox,
                false,
            )
            .await?;
        }
        follow::create_request(
            &state.pool,
            signer.id,
            owner.account_id,
            activity.get("id").and_then(Value::as_str),
        )
        .await?;
    } else if (kind == "Undo"
        && activity
            .get("object")
            .and_then(|inner| inner.get("type"))
            .and_then(Value::as_str)
            == Some("Follow")
        && activity
            .get("object")
            .and_then(|inner| inner.get("object"))
            .and_then(id_of)
            == Some(&owner.actor_uri))
        || (kind == "Delete" && activity.get("object").and_then(id_of) == signer.uri.as_deref())
    {
        gateway::remove_follower(
            &state.pool,
            owner.account_id,
            signer.uri.as_deref().unwrap_or_default(),
        )
        .await?;
        follow::delete(&state.pool, signer.id, owner.account_id).await?;
    }
    Ok(())
}

async fn push_inbox(
    state: &AppState,
    owner: gateway::GatewayActor,
    uri: &Uri,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    match ensure_active(state, &owner).await {
        Ok(_) => {}
        Err(ApiError::Forbidden(_)) => {
            // A suspended portable account is locally unavailable.
            // Acknowledge remote delivery so peers do not retry forever, but
            // neither expose it to the client nor mutate local social state.
            return Ok(StatusCode::ACCEPTED);
        }
        Err(error) => return Err(error),
    }
    let raw: Value = serde_json::from_slice(&body)
        .map_err(|_| ApiError::BadRequest("body is not valid JSON".into()))?;
    // Portable inbox/outbox collections persist complete activities. This
    // gateway has no ephemeral client transport, so never archive packets.
    if crate::webxdc_realtime::is_packet(&raw) {
        return Err(ApiError::BadRequest(
            "ephemeral Webxdc packets require a live host channel".into(),
        ));
    }
    let activity: Activity = serde_json::from_value(raw.clone())
        .map_err(|_| ApiError::BadRequest("body is not an activity".into()))?;
    let activity_id = activity
        .id
        .as_deref()
        .ok_or_else(|| ApiError::BadRequest("activity has no ID".into()))?;
    let actor_id = activity
        .actor_id()
        .ok_or_else(|| ApiError::BadRequest("activity has no actor".into()))?;
    let path_and_query = uri
        .path_and_query()
        .map_or(uri.path(), |value| value.as_str());
    let target = request_target(&state.config.domain, uri);
    let prepared = PreparedRequestAuth::from_post_request(
        &RequestFacts {
            method: "POST",
            target_uri: &target,
            path_and_query,
            headers,
        },
        &body,
        SystemTime::now(),
    )
    .map_err(|error| ApiError::Unauthorized(error.to_string()))?;
    let signer = super::inbox::verified_signer(state, &prepared, actor_id).await?;
    // Deduplicate this recipient's inbox before dispatching into ordinary
    // social state. The same activity remains valid in another portable
    // actor's inbox, so the database key is collection-scoped.
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(activity_id)
        .execute(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?;
    if gateway::find_collection_item(&mut *tx, owner.account_id, "inbox", activity_id)
        .await?
        .is_some()
    {
        tx.commit().await.map_err(plamenu_db::DbError::from)?;
        return Ok(StatusCode::ACCEPTED);
    }
    project_incoming_relationship(state, &owner, &signer, &raw).await?;
    let relationship_only = activity.kind == "Follow"
        || (activity.kind == "Undo" && embedded_kind(&activity) == Some("Follow"));
    // A Plamenu-owned actor, or another portable actor hosted by this gateway,
    // has already projected its activity at the source.  This POST exists so
    // the recipient's client can poll the exact activity; ingesting it again
    // would turn a local status (whose stored URI is intentionally null) into
    // a second URI-backed row.  Activities from genuinely remote senders still
    // need the ordinary social projection below.
    let sender_is_hosted = signer.has_local_account_on(&state.config.domain);
    if !relationship_only && !sender_is_hosted {
        super::inbox::dispatch_authenticated(state, &signer, &activity, &raw).await?;
    }
    if !gateway::insert_collection_item(&mut *tx, owner.account_id, "inbox", activity_id, &raw)
        .await?
    {
        return Err(ApiError::Conflict(
            "activity was concurrently delivered to this inbox".into(),
        ));
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(StatusCode::ACCEPTED)
}

pub async fn post(
    State(state): State<AppState>,
    Path(_path): Path<String>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let resource = resource_uri(&state.config.domain, &uri);
    if resource.ends_with("/outbox") {
        let owner = gateway::find_by_outbox_uri(&state.pool, &resource)
            .await?
            .ok_or(ApiError::NotFound)?;
        let raw = serde_json::from_slice(&body)
            .map_err(|_| ApiError::BadRequest("body is not valid JSON".into()))?;
        return push_outbox(&state, owner, raw).await;
    }
    if resource.ends_with("/inbox") {
        let owner = gateway::find_by_inbox_uri(&state.pool, &resource)
            .await?
            .ok_or(ApiError::NotFound)?;
        return push_inbox(&state, owner, &uri, &headers, body).await;
    }
    Err(ApiError::NotFound)
}

fn media_type(value: Option<&str>) -> Option<(&'static str, usize)> {
    let value = value?.split(';').next()?.trim().to_ascii_lowercase();
    let image_limit = crate::media_processing::MAX_UPLOAD_BYTES;
    let av_limit = crate::media_processing::MAX_AV_UPLOAD_BYTES;
    match value.as_str() {
        "image/jpeg" => Some(("jpg", image_limit)),
        "image/png" => Some(("png", image_limit)),
        "image/gif" => Some(("gif", image_limit)),
        "image/webp" => Some(("webp", image_limit)),
        "image/avif" => Some(("avif", image_limit)),
        "audio/mpeg" => Some(("mp3", av_limit)),
        "audio/ogg" => Some(("ogg", av_limit)),
        "audio/wav" | "audio/x-wav" => Some(("wav", av_limit)),
        "audio/flac" => Some(("flac", av_limit)),
        "video/mp4" => Some(("mp4", av_limit)),
        "video/webm" => Some(("webm", av_limit)),
        "video/ogg" => Some(("ogv", av_limit)),
        _ => None,
    }
}

pub async fn upload_media(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let (_, signer) = prepared_client_request(&state, &uri, &headers, Some(&body), false).await?;
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let (extension, limit) = media_type(Some(content_type))
        .ok_or_else(|| ApiError::BadRequest("unsupported media type".into()))?;
    if body.len() > limit {
        return Err(ApiError::PayloadTooLarge);
    }
    let (hashlink, digest) = plamenu_ap::hashlink::encode(&body);
    let existing = gateway::find_media(&state.pool, &digest).await?;
    let needs_store = existing.is_none();
    let file_name = existing.map_or_else(
        || {
            format!(
                "{}.gateway.{extension}",
                crate::media_processing::storage_stem(plamenu_db::id::next())
            )
        },
        |media| media.file_name,
    );
    if needs_store {
        state
            .media
            .put(&file_name, body.to_vec())
            .await
            .map_err(|error| ApiError::Internal(Box::new(error)))?;
    }
    gateway::insert_media(
        &state.pool,
        signer.account_id,
        &digest,
        &file_name,
        content_type.split(';').next().unwrap_or(content_type),
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        [(header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8)],
        Json(json!({ "type": "Document", "url": hashlink })),
    )
        .into_response())
}

pub async fn get_media(
    State(state): State<AppState>,
    Path(hashlink): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let digest = plamenu_ap::hashlink::decode(&hashlink)
        .map_err(|_| ApiError::BadRequest("invalid hashlink".into()))?;
    let media = gateway::find_media(&state.pool, &digest)
        .await?
        .ok_or(ApiError::NotFound)?;
    super::media::stream_stored_file(
        &state,
        &media.file_name,
        &media.content_type,
        "public, max-age=31536000, immutable",
        &headers,
    )
    .await
}

pub async fn delete_media(
    State(state): State<AppState>,
    Path(hashlink): Path<String>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let path_and_query = uri
        .path_and_query()
        .map_or(uri.path(), |value| value.as_str());
    let target = request_target(&state.config.domain, &uri);
    let facts = RequestFacts {
        method: "DELETE",
        target_uri: &target,
        path_and_query,
        headers: &headers,
    };
    let prepared = PreparedRequestAuth::from_bodyless_request(&facts, SystemTime::now(), false)
        .map_err(|error| ApiError::Unauthorized(error.to_string()))?;
    let signer = gateway::find_by_client_key_id(&state.pool, prepared.key_id())
        .await?
        .ok_or_else(|| ApiError::Forbidden("signature actor is not registered".into()))?;
    prepared
        .verify_rsa_pem(&signer.client_rsa_public_key)
        .map_err(|error| ApiError::Unauthorized(error.to_string()))?;
    let digest = plamenu_ap::hashlink::decode(&hashlink)
        .map_err(|_| ApiError::BadRequest("invalid hashlink".into()))?;
    let (file_name, remaining) = gateway::delete_media(&state.pool, signer.account_id, &digest)
        .await?
        .ok_or(ApiError::NotFound)?;
    if remaining == 0 {
        state
            .media
            .delete(&file_name)
            .await
            .map_err(|error| ApiError::Internal(Box::new(error)))?;
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audience_includes_wrapped_addressing_and_relationship_target() {
        let audience = intended_audience(&json!({
            "type": "Follow",
            "to": "https://one.example/actor",
            "object": "https://two.example/actor",
        }));
        assert_eq!(
            audience,
            ["https://one.example/actor", "https://two.example/actor"]
        );

        let audience = intended_audience(&json!({
            "type": "Create",
            "cc": PUBLIC,
            "object": { "to": ["https://three.example/actor"] },
        }));
        assert_eq!(audience, [PUBLIC, "https://three.example/actor"]);
    }

    #[test]
    fn media_allowlist_has_separate_image_and_av_limits() {
        assert_eq!(
            media_type(Some("image/png; charset=binary")),
            Some(("png", crate::media_processing::MAX_UPLOAD_BYTES))
        );
        assert_eq!(
            media_type(Some("video/mp4")),
            Some(("mp4", crate::media_processing::MAX_AV_UPLOAD_BYTES))
        );
        assert_eq!(media_type(Some("text/html")), None);
    }
}
