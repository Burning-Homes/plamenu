//! Publishing and reading verified FEP-c390 statements.

use crate::{AppState, error::ApiError};
use plamenu_db::{
    account::{self, Account},
    identity_proof,
};
use serde_json::Value;

#[must_use]
pub fn actor_id(domain: &str, account: &Account) -> String {
    plamenu_ap::urls::LocalUserUrls::for_account(domain, &account.username, account.uri.as_deref())
        .id
}

pub async fn list_conn(
    conn: &mut plamenu_db::PgConnection,
    account: &Account,
    uri: &str,
) -> Result<Vec<Value>, ApiError> {
    if account.suspended() {
        return Ok(vec![]);
    }
    let documents = identity_proof::list(conn, account.id).await?;
    Ok(plamenu_ap::identity::verified(
        &documents,
        uri,
        time::OffsetDateTime::now_utc(),
    ))
}

pub async fn list(state: &AppState, account: &Account) -> Result<Vec<Value>, ApiError> {
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    list_conn(&mut conn, account, &actor_id(&state.config.domain, account)).await
}

/// A replacement for the same DID is allowed. Storage and the outgoing actor
/// Update commit together; the private identity key never reaches the server.
pub async fn publish(
    state: &AppState,
    account_id: i64,
    document: Value,
) -> Result<Vec<Value>, ApiError> {
    change(state, account_id, Some(document), None).await
}

pub async fn remove(
    state: &AppState,
    account_id: i64,
    subject: &str,
) -> Result<Vec<Value>, ApiError> {
    change(state, account_id, None, Some(subject)).await
}

async fn change(
    state: &AppState,
    account_id: i64,
    document: Option<Value>,
    remove_subject: Option<&str>,
) -> Result<Vec<Value>, ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    if !identity_proof::lock_local(&mut *tx, account_id).await? {
        return Err(ApiError::NotFound);
    }
    let account = account::find_by_id(&mut *tx, account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let uri = actor_id(&state.config.domain, &account);
    let mut documents = list_conn(&mut tx, &account, &uri).await?;
    if let Some(document) = document {
        let subject =
            plamenu_ap::identity::verify(&document, &uri, time::OffsetDateTime::now_utc())
                .map_err(|e| ApiError::Unprocessable(e.into()))?;
        documents.retain(|d| d["subject"].as_str() != Some(subject));
        if documents.len() >= plamenu_ap::identity::MAX_PROOFS {
            return Err(ApiError::Unprocessable(
                "At most ten identity proofs are allowed".into(),
            ));
        }
        documents.push(document);
    } else if let Some(subject) = remove_subject {
        documents.retain(|d| d["subject"].as_str() != Some(subject));
    }
    identity_proof::replace(&mut *tx, account_id, &documents).await?;
    crate::profile::fan_out_actor_update_conn(state, &mut tx, &account).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(documents)
}
