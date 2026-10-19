use super::{
    AppState, DiscoveryFormat, FederationError, Lease, Value, db, db_error, outbox, public_record,
    transport,
};
use crate::discovery::{policy::Access, profiles};
use plamenu_ap::discovery::{DiscoverySignals, Signal};

#[must_use]
pub fn public_community(document: &Value) -> bool {
    document.get("type").and_then(Value::as_str) == Some("Group")
        && profiles::public_profile(document)
        && ["deleted", "removed", "hidden"]
            .iter()
            .all(|key| document.get(*key).is_none_or(|v| v == &Value::Bool(false)))
        && document.get("sensitive").is_none_or(Value::is_boolean)
        && plamenu_ap::activity::one_or_many(document.get("audience"))
            .iter()
            .all(|v| {
                plamenu_ap::activity::id_of(v)
                    == Some("https://www.w3.org/ns/activitystreams#Public")
            })
}

async fn refusal(state: &AppState, lease: &Lease, withdrawn: bool) -> Result<(), FederationError> {
    db::finish_entry_refusal(&state.pool, lease, withdrawn, "community_unavailable")
        .await
        .map_err(|e| db_error(&e))?;
    Ok(())
}

pub async fn fetch(state: &AppState, lease: &Lease) -> Result<(), FederationError> {
    let settings = db::settings(&state.pool).await.map_err(|e| db_error(&e))?;
    if lease.search_run_id.is_none()
        && settings
            .disabled_mechanisms
            .iter()
            .any(|m| m == "community_directory")
    {
        return Err(FederationError::DiscoveryDeferred(
            "community_directory_disabled",
        ));
    }
    if profiles::access(state, &lease.target_uri)
        .await
        .map_err(|e| db_error(&e))?
        != Access::Allowed
    {
        return refusal(state, lease, false).await;
    }
    let response = match transport::fetch(
        state,
        lease,
        &lease.target_uri,
        DiscoveryFormat::ActivityPub,
    )
    .await
    {
        Ok(r) => r,
        Err(FederationError::Status(code @ (401 | 403 | 404 | 410))) => {
            return refusal(state, lease, code == 410).await;
        }
        Err(error) => return Err(error),
    };
    let document = &response.document;
    let identity = document.get("id").and_then(Value::as_str) == Some(lease.target_uri.as_str());
    if !identity || !public_community(document) {
        return refusal(
            state,
            lease,
            identity && document.get("type").and_then(Value::as_str) == Some("Tombstone"),
        )
        .await;
    }
    if matches!(
        DiscoverySignals::from_document(document).discoverable,
        Signal::Deny | Signal::Invalid
    ) {
        return profiles::save_profile(state, lease, &lease.target_uri, document, "community")
            .await;
    }
    let Some(name) = document
        .get("preferredUsername")
        .and_then(Value::as_str)
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 255
                && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
    else {
        return refusal(state, lease, false).await;
    };
    let mut detail = url::Url::parse(&lease.target_uri).map_err(|_| super::invalid())?;
    detail.set_path("/api/v3/community");
    detail.set_query(None);
    detail.query_pairs_mut().append_pair("name", name);
    let detail = match transport::fetch(state, lease, detail.as_str(), DiscoveryFormat::Json).await
    {
        Ok(r) => r,
        Err(FederationError::Status(code @ (401 | 403 | 404 | 410))) => {
            return refusal(state, lease, code == 410).await;
        }
        Err(error) => return Err(error),
    };
    let record = &detail.document["community_view"]["community"];
    if !public_record(record)
        || record.get("actor_id").and_then(Value::as_str) != Some(lease.target_uri.as_str())
        || !outbox::same_origin(&lease.target_uri, &detail.final_url)
    {
        return refusal(state, lease, false).await;
    }
    let mut document = document.clone();
    if record.get("nsfw") == Some(&Value::Bool(true)) {
        document["sensitive"] = Value::Bool(true);
    }
    profiles::save_profile(state, lease, &lease.target_uri, &document, "community").await
}
