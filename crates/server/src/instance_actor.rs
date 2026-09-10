//! The instance actor — Mastodon's "representative": an `Application` actor
//! served at `/actor` whose key signs this server's outbound fetches, so
//! remotes running authorized-fetch ("secure mode") answer us.

use axum::Json;
use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use plamenu_ap::actor::{InstanceActor, Multikey, PublicKey};
use plamenu_ap::collection::OrderedCollection;
use plamenu_ap::urls::InstanceActorUrls;

use crate::AppState;
use crate::error::ApiError;

/// `GET /actor`. Served without Accept or signature gating — even in
/// authorized-fetch mode, like Mastodon — because remotes must be able to
/// fetch this key to verify our signed GETs (and vice versa).
pub async fn document(state: &AppState) -> Result<InstanceActor, ApiError> {
    let keyring = state.federation_keyring.as_deref().ok_or_else(|| {
        ApiError::Internal(Box::new(
            crate::crypto::KeyEncryptionError::MissingConfiguration,
        ))
    })?;
    crate::key_store::ensure_instance(&state.pool, keyring, &state.config.domain)
        .await
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    document_conn(state, &mut conn).await
}

/// Renders already-initialized instance keys inside a rotation transaction.
pub async fn document_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
) -> Result<InstanceActor, ApiError> {
    let keys = plamenu_db::actor_key::published_for_instance(&mut *conn).await?;
    let rsa = keys
        .iter()
        .find(|key| key.algorithm == "rsa")
        .ok_or(ApiError::NotFound)?;
    let actor_id = InstanceActorUrls::new(&state.config.domain).id;
    let mut methods = Vec::with_capacity(keys.len());
    for algorithm in ["ed25519", "rsa", "ml-dsa-44"] {
        for key in keys
            .iter()
            .filter(|key| key.algorithm == algorithm && key.controller_uri == actor_id)
        {
            let method = match algorithm {
                "ed25519" | "ml-dsa-44" => Some(Multikey {
                    id: key.key_uri.clone(),
                    kind: "Multikey".to_owned(),
                    controller: actor_id.clone(),
                    public_key_multibase: key.public_key.clone(),
                }),
                "rsa" => Multikey::rsa(key.key_uri.clone(), actor_id.clone(), &key.public_key),
                _ => None,
            };
            methods.extend(method);
        }
    }
    let mut actor = InstanceActor::new(&state.config.domain, &rsa.public_key, None);
    actor.public_key = PublicKey {
        id: rsa.key_uri.clone(),
        owner: actor_id,
        public_key_pem: rsa.public_key.clone(),
    };
    actor.assertion_method = methods;
    Ok(actor)
}

pub async fn get_actor(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    Ok((
        [(header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8)],
        Json(document(&state).await?),
    ))
}

/// Distributes pending instance keys while the previous key still signs.
pub async fn fan_out_actor_update(state: &AppState) -> Result<usize, ApiError> {
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    fan_out_actor_update_conn(state, &mut conn).await
}

/// Connection-scoped delivery for an enclosing mutation transaction.
pub async fn fan_out_actor_update_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
) -> Result<usize, ApiError> {
    let actor = document_conn(state, conn).await?;
    let actor_id = InstanceActorUrls::new(&state.config.domain).id;
    let activity = plamenu_ap::activity::update_actor_for_actor(
        &actor_id,
        serde_json::to_value(actor).map_err(|error| ApiError::Internal(Box::new(error)))?,
        plamenu_db::id::next(),
    );
    let inboxes = plamenu_db::account::known_remote_inboxes(&mut *conn).await?;
    plamenu_db::job::enqueue_many_from_instance(&mut *conn, &inboxes, &activity).await?;
    Ok(inboxes.len())
}

/// `GET /actor/outbox`: a permanently empty collection, like Mastodon's.
pub async fn get_outbox(State(state): State<AppState>) -> impl IntoResponse {
    let urls = InstanceActorUrls::new(&state.config.domain);
    (
        [(header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8)],
        Json(OrderedCollection::new(&urls.outbox, 0)),
    )
}
