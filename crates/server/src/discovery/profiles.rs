//! Canonical actor verification and the eligibility boundary for stored profiles.
use super::{
    policy::{Access, Outcome, Use},
    store,
    transport::{self, Budget},
};
use crate::AppState;
use plamenu_ap::discovery::{DiscoverySignals, Signal};
use plamenu_db::{
    account,
    discovery_store::{self as db, Entry, Lease, VerifiedEntry},
};
use plamenu_federation::{DiscoveryFormat, FederationError};
use serde_json::Value;
use time::OffsetDateTime;

fn error(message: &str) -> FederationError {
    FederationError::InvalidActor(message.into())
}
fn db_error(error: &plamenu_db::DbError) -> FederationError {
    FederationError::Admission(error.to_string())
}
pub(super) fn signal(value: Signal) -> &'static str {
    match value {
        Signal::Allow => "allow",
        Signal::Deny => "deny",
        Signal::Invalid => "invalid",
        Signal::Missing => "missing",
    }
}

#[must_use]
pub fn public_profile(document: &Value) -> bool {
    if document
        .get("localOnly")
        .is_some_and(|v| v != &Value::Bool(false))
        || document
            .get("suspended")
            .is_some_and(|v| v != &Value::Bool(false))
        || document.get("movedTo").is_some_and(|v| !v.is_null())
    {
        return false;
    }
    if document
        .get("visibility")
        .is_some_and(|v| v.as_str() != Some("public"))
    {
        return false;
    }
    let mut addressed = false;
    let mut public = false;
    for field in ["to", "cc"] {
        if let Some(value) = document.get(field) {
            addressed = true;
            let contains =
                |v: &Value| v.as_str() == Some("https://www.w3.org/ns/activitystreams#Public");
            public |= contains(value)
                || value
                    .as_array()
                    .is_some_and(|values| values.iter().any(contains));
        }
    }
    !addressed || public
}

/// Use declared metadata only; never infer a person's language from their server.
pub(super) fn declared_language(document: &Value) -> Option<&str> {
    crate::ingest::raw_language_code(document).filter(|code| crate::actions::is_language_code(code))
}

pub(super) fn text(value: &str, limit: usize) -> String {
    let normalized = value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('\0', "");
    let mut end = normalized.len().min(limit);
    while !normalized.is_char_boundary(end) {
        end -= 1;
    }
    normalized[..end].to_owned()
}

pub(super) async fn access(state: &AppState, uri: &str) -> Result<Access, plamenu_db::DbError> {
    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, uri).await? {
        return Ok(Access::InstanceBlocked);
    }
    if let Some(domain) = crate::instance_policy::domain_from_url(uri)
        && plamenu_db::instance_policy::is_domain_silenced(&state.pool, &domain).await?
    {
        return Ok(Access::InstanceBlocked);
    }
    if let Some(account) = account::find_by_uri(&state.pool, uri).await?
        && (account.suspended()
            || !crate::instance_policy::account_visible(
                &state.pool,
                &state.config.domain,
                &account,
            )
            .await?
            || account::effectively_silenced(&state.pool, account.id).await?)
    {
        return Ok(Access::InstanceBlocked);
    }
    Ok(Access::Allowed)
}

pub async fn fetch(state: &AppState, lease: &Lease) -> Result<(), FederationError> {
    if access(state, &lease.target_uri)
        .await
        .map_err(|error| db_error(&error))?
        != Access::Allowed
    {
        db::invalidate_entry(
            &state.pool,
            &lease.target_uri,
            db::Invalidation::Denied,
            "local_moderation",
        )
        .await
        .map_err(|error| db_error(&error))?;
        return Err(FederationError::DiscoveryStale);
    }
    let response = match transport::fetch(
        state,
        lease,
        &lease.target_uri,
        DiscoveryFormat::ActivityPub,
    )
    .await
    {
        Ok(response) => response,
        Err(FederationError::Status(status @ (401 | 403 | 404 | 410))) => {
            let canonical = lease.cursor.as_deref() == Some("canonical")
                || lease.entry_revision.is_some()
                || lease.operation == "revalidate_entry";
            db::finish_entry_refusal(
                &state.pool,
                lease,
                status == 410 && canonical,
                if status == 410 {
                    "gone"
                } else {
                    "access_unavailable"
                },
            )
            .await
            .map_err(|error| db_error(&error))?;
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    verify(state, lease, &response.document).await
}

async fn verify(state: &AppState, lease: &Lease, document: &Value) -> Result<(), FederationError> {
    let id = document
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| error("actor has no canonical identity"))?;
    let id = url::Url::parse(id)
        .map_err(|_| error("invalid actor identity"))?
        .to_string();
    if id != lease.target_uri {
        if lease.operation == "refresh_entry"
            && lease.cursor.is_none()
            && lease.entry_revision.is_none()
        {
            db::finish_profile_alias(&state.pool, lease, &id)
                .await
                .map_err(|error| db_error(&error))?;
        } else {
            db::finish_entry_refusal(&state.pool, lease, false, "identity_mismatch")
                .await
                .map_err(|error| db_error(&error))?;
        }
        return Ok(());
    }
    if !(Budget { state, lease }).allowed().await? {
        return Err(FederationError::DiscoveryStale);
    }
    if document.get("type").and_then(Value::as_str) == Some("Tombstone") {
        db::finish_entry_refusal(&state.pool, lease, true, "tombstone")
            .await
            .map_err(|error| db_error(&error))?;
        return Ok(());
    }
    if !matches!(
        document.get("type").and_then(Value::as_str),
        Some("Person" | "Service" | "Application")
    ) {
        db::finish_entry_refusal(&state.pool, lease, false, "unsupported_profile_type")
            .await
            .map_err(|error| db_error(&error))?;
        return Ok(());
    }
    save_profile(state, lease, &id, document, "profile").await
}

pub(super) async fn save_profile(
    state: &AppState,
    lease: &Lease,
    id: &str,
    document: &Value,
    kind: &str,
) -> Result<(), FederationError> {
    let signals = DiscoverySignals::from_document(document);
    let refused =
        matches!(signals.discoverable, Signal::Deny | Signal::Invalid) || !public_profile(document);
    let username = document
        .get("preferredUsername")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| refused.then_some(""))
        .ok_or_else(|| error("actor has no username"))?;
    let title = text(
        document
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(username),
        1024,
    );
    let summary = text(
        &crate::filters::plain_text(
            document
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        ),
        8192,
    );
    if access(state, id).await.map_err(|error| db_error(&error))? != Access::Allowed {
        db::invalidate_entry(
            &state.pool,
            id,
            db::Invalidation::Denied,
            "local_moderation",
        )
        .await
        .map_err(|error| db_error(&error))?;
        return Err(FederationError::DiscoveryStale);
    }
    if !refused {
        db::save_actor_label(&state.pool, lease, id, username)
            .await
            .map_err(|e| db_error(&e))?;
    }
    let account_id = account::find_by_uri(&state.pool, id)
        .await
        .map_err(|error| db_error(&error))?
        .map(|a| a.id);
    let now = OffsetDateTime::now_utc();
    let result = db::finish_entry_with_topics(
        &state.pool,
        lease,
        &VerifiedEntry {
            outbox_uri: if kind == "profile" {
                super::outbox::advertised(document, id)
            } else {
                None
            },
            published_at: None,
            sensitive: kind == "community" && document.get("sensitive") == Some(&Value::Bool(true)),
            author_revision: None,
            kind,
            actor_uri: id,
            account_id,
            discoverable: signal(signals.discoverable),
            indexable: signal(signals.indexable),
            visibility: if public_profile(document) {
                "public"
            } else {
                "unknown"
            },
            title: &title,
            summary: &summary,
            language: declared_language(document),
            verified_at: now,
            valid_until: now + time::Duration::hours(24),
        },
        &super::topics::explicit_tags(document),
    )
    .await
    .map_err(|error| db_error(&error))?;
    if result == db::Completion::Capacity {
        db::defer_job(&state.pool, lease, 3600, "catalogue_full")
            .await
            .map_err(|error| db_error(&error))?;
    }
    Ok(())
}

/// A read never fetches. Current moderation and expired evidence suppress even
/// a previously verified profile, while known local blocks clear its payload.
/// Step 5 must additionally apply the viewer's own blocks/mutes/dismissals.
pub async fn eligible(state: &AppState, uri: &str) -> Result<Option<Entry>, plamenu_db::DbError> {
    let Some(entry) = db::entry(&state.pool, uri).await? else {
        return Ok(None);
    };
    let Some(source) = db::source(&state.pool, entry.source_id).await? else {
        return Ok(None);
    };
    let access = access(state, uri).await?;
    if access != Access::Allowed && entry.state == "active" {
        db::invalidate_entry(
            &state.pool,
            uri,
            db::Invalidation::Denied,
            "local_moderation",
        )
        .await?;
        return Ok(None);
    }
    let settings = db::settings(&state.pool).await?;
    let decision = store::evaluate_entry(
        &settings,
        &source,
        &entry,
        Use::ListProfile,
        access,
        OffsetDateTime::now_utc(),
    )?;
    Ok((decision.outcome == Outcome::Allow).then_some(entry))
}

/// Called only after the inbox has authenticated an actor updating itself.
/// Refusals are immediate; positive claims still require canonical verification.
pub async fn verified_update(
    state: &AppState,
    uri: &str,
    document: &Value,
) -> Result<(), plamenu_db::DbError> {
    if document.get("id").and_then(Value::as_str) != Some(uri) {
        return Ok(());
    }
    if document.get("type").and_then(Value::as_str) == Some("Group") {
        db::invalidate_entry(
            &state.pool,
            uri,
            db::Invalidation::Denied,
            "community_updated",
        )
        .await?;
        return Ok(());
    }
    db::forget_actor_topics(&state.pool, uri).await?;
    let signals = DiscoverySignals::from_document(document);
    db::revoke_actor(
        &state.pool,
        uri,
        signal(signals.discoverable),
        signal(signals.indexable),
        public_profile(document),
    )
    .await
}
