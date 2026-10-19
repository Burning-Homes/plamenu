//! Public text excerpts verified at canonical object and actor authorities.
use super::{
    outbox,
    policy::{Access, Outcome, Use},
    profiles, store, transport,
};
use crate::{AppState, error::ApiError};
use plamenu_ap::{
    activity::{id_of, one_or_many},
    discovery::{DiscoverySignals, Signal},
};
use plamenu_db::{
    account_domain_block,
    discovery_store::{self as db, Entry, Lease},
    status,
};
use plamenu_federation::{DiscoveryFormat, FederationError};
use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

mod browse;
pub use browse::page;
pub(crate) use browse::{apply_filters, apply_filters_in_context};

const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";
fn db_error(error: &plamenu_db::DbError) -> FederationError {
    FederationError::Admission(error.to_string())
}

#[must_use]
pub fn author_uri(document: &Value) -> Option<&str> {
    let authors = one_or_many(document.get("attributedTo"));
    (authors.len() == 1).then(|| id_of(&authors[0])).flatten()
}

/// Public in `to` is required; public `cc` alone is unlisted. A narrower
/// declared audience or local/member restriction wins over public addressing.
#[must_use]
pub fn public_post(document: &Value) -> bool {
    matches!(
        document.get("type").and_then(Value::as_str),
        Some("Note" | "Article" | "Page" | "Question")
    ) && one_or_many(document.get("to"))
        .iter()
        .any(|v| id_of(v) == Some(PUBLIC))
        && one_or_many(document.get("audience"))
            .iter()
            .all(|v| id_of(v) == Some(PUBLIC))
        && ["bto", "bcc"]
            .iter()
            .all(|key| one_or_many(document.get(*key)).is_empty())
        && ["localOnly", "suspended"]
            .iter()
            .all(|key| document.get(*key).is_none_or(|v| v == &Value::Bool(false)))
        && document
            .get("visibility")
            .is_none_or(|v| v.as_str() == Some("public"))
        && document.get("sensitive").is_none_or(Value::is_boolean)
        && !matches!(
            DiscoverySignals::from_document(document).indexable,
            Signal::Deny | Signal::Invalid
        )
}

pub async fn fetch(
    state: &AppState,
    lease: &Lease,
    known: Option<&Entry>,
) -> Result<(), FederationError> {
    let settings = db::settings(&state.pool).await.map_err(|e| db_error(&e))?;
    if settings
        .disabled_mechanisms
        .iter()
        .any(|m| m == "outbox" || m == "public_post_indexing")
    {
        return Err(FederationError::DiscoveryStale);
    }
    let Some(actor_uri) = known
        .and_then(|e| e.actor_uri.as_deref())
        .or(lease.cursor.as_deref())
        .filter(|uri| outbox::same_origin(uri, &lease.target_uri))
    else {
        return Err(FederationError::InvalidActor(
            "post has no canonical author hint".into(),
        ));
    };
    let Some(actor) = db::entry(&state.pool, actor_uri)
        .await
        .map_err(|e| db_error(&e))?
        .filter(|a| outbox::author_allows(a, &settings))
    else {
        db::finish_job(&state.pool, lease)
            .await
            .map_err(|e| db_error(&e))?;
        return Ok(());
    };
    if profiles::access(state, actor_uri)
        .await
        .map_err(|e| db_error(&e))?
        != Access::Allowed
    {
        db::invalidate_entry(
            &state.pool,
            actor_uri,
            db::Invalidation::Denied,
            "local_moderation",
        )
        .await
        .map_err(|e| db_error(&e))?;
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
        Ok(value) => value,
        Err(FederationError::Status(code @ (401 | 403 | 404 | 410))) => {
            db::finish_entry_refusal(
                &state.pool,
                lease,
                code == 410,
                if code == 410 {
                    "gone"
                } else {
                    "access_unavailable"
                },
            )
            .await
            .map_err(|e| db_error(&e))?;
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let document = &response.document;
    if id_of(document) != Some(lease.target_uri.as_str())
        || author_uri(document) != Some(actor_uri)
        || !public_post(document)
    {
        let tombstone = id_of(document) == Some(lease.target_uri.as_str())
            && document.get("type").and_then(Value::as_str) == Some("Tombstone");
        db::finish_entry_refusal(
            &state.pool,
            lease,
            tombstone,
            if tombstone {
                "tombstone"
            } else {
                "ineligible_post"
            },
        )
        .await
        .map_err(|e| db_error(&e))?;
        return Ok(());
    }
    save(state, lease, &actor, document).await
}

async fn save(
    state: &AppState,
    lease: &Lease,
    actor: &Entry,
    document: &Value,
) -> Result<(), FederationError> {
    let published = document
        .get("published")
        .and_then(Value::as_str)
        .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok());
    let now = OffsetDateTime::now_utc();
    if published.is_none_or(|at| at > now + time::Duration::minutes(5)) {
        return Err(FederationError::InvalidActor(
            "post has no valid publication time".into(),
        ));
    }
    let warning = document
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let title = profiles::text(
        &crate::filters::plain_text(if warning.is_empty() {
            document
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
        } else {
            warning
        }),
        1024,
    );
    let summary = profiles::text(
        &crate::filters::plain_text(
            document
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        ),
        8192,
    );
    if summary.is_empty() && title.is_empty() {
        db::finish_job(&state.pool, lease)
            .await
            .map_err(|e| db_error(&e))?;
        return Ok(());
    }
    if profiles::access(state, &actor.canonical_uri)
        .await
        .map_err(|e| db_error(&e))?
        != Access::Allowed
    {
        db::invalidate_entry(
            &state.pool,
            &actor.canonical_uri,
            db::Invalidation::Denied,
            "local_moderation",
        )
        .await
        .map_err(|e| db_error(&e))?;
        return Err(FederationError::DiscoveryStale);
    }
    let result = db::finish_entry_with_topics(
        &state.pool,
        lease,
        &db::VerifiedEntry {
            kind: "post",
            actor_uri: &actor.canonical_uri,
            account_id: actor.account_id,
            discoverable: &actor.discoverable,
            indexable: &actor.indexable,
            visibility: "public",
            title: &title,
            summary: &summary,
            language: profiles::declared_language(document),
            verified_at: now,
            valid_until: now + time::Duration::days(180),
            outbox_uri: None,
            published_at: published,
            sensitive: !warning.is_empty() || document.get("sensitive") == Some(&Value::Bool(true)),
            author_revision: Some(actor.revision),
        },
        &super::topics::explicit_tags(document),
    )
    .await
    .map_err(|e| db_error(&e))?;
    if result == db::Completion::Capacity {
        db::defer_job(&state.pool, lease, 3600, "catalogue_full")
            .await
            .map_err(|e| db_error(&e))?;
    }
    Ok(())
}

/// Serving uses live author policy, viewer exclusions and ordinary stored-post
/// visibility where available. It never resolves a URI or starts network work.
pub async fn eligible(
    state: &AppState,
    viewer: i64,
    id: i64,
    usage: Use,
) -> Result<Option<Entry>, ApiError> {
    let Some(entry) = db::entry_by_id(&state.pool, id).await? else {
        return Ok(None);
    };
    if entry.kind != "post" || db::viewer_hides(&state.pool, viewer, id).await? {
        return Ok(None);
    }
    let Some(actor_uri) = entry.actor_uri.as_deref() else {
        return Ok(None);
    };
    let settings = db::settings(&state.pool).await?;
    let Some(actor) = db::entry(&state.pool, actor_uri)
        .await?
        .filter(|a| outbox::author_allows(a, &settings))
    else {
        return Ok(None);
    };
    if profiles::access(state, actor_uri).await? != Access::Allowed {
        db::invalidate_entry(
            &state.pool,
            actor_uri,
            db::Invalidation::Denied,
            "local_moderation",
        )
        .await?;
        return Ok(None);
    }
    let domains = account_domain_block::all_domains(&state.pool, viewer).await?;
    if crate::instance_policy::domain_from_url(actor_uri).is_some_and(|d| domains.contains(&d)) {
        return Ok(None);
    }
    let Some(source) = db::source(&state.pool, entry.source_id).await? else {
        return Ok(None);
    };
    if actor.source_id != entry.source_id
        || store::evaluate_entry(
            &settings,
            &source,
            &entry,
            Use::IndexPost,
            Access::Allowed,
            OffsetDateTime::now_utc(),
        )?
        .outcome
            != Outcome::Allow
    {
        return Ok(None);
    }
    if usage == Use::Recommend {
        let mut current = entry.clone();
        current.discoverable.clone_from(&actor.discoverable);
        if store::evaluate_entry(
            &settings,
            &source,
            &current,
            Use::Recommend,
            Access::Allowed,
            OffsetDateTime::now_utc(),
        )?
        .outcome
            != Outcome::Allow
        {
            return Ok(None);
        }
    }
    if !stored_post_visible(state, viewer, &entry).await? {
        return Ok(None);
    }
    Ok(Some(entry))
}

async fn stored_post_visible(
    state: &AppState,
    viewer: i64,
    entry: &Entry,
) -> Result<bool, ApiError> {
    if let Some(known) = status::find_by_uri(&state.pool, &entry.canonical_uri).await? {
        let deleted = status::is_deleted(&state.pool, known.id).await?;
        if known.visibility != "public" || deleted {
            if let Some(actor) = entry.actor_uri.as_deref() {
                db::invalidate_owned_post(
                    &state.pool,
                    actor,
                    &entry.canonical_uri,
                    deleted,
                    "stored_post_restricted",
                )
                .await?;
            }
            return Ok(false);
        }
        return crate::entities::can_view(&state.pool, &known, Some(viewer)).await;
    }
    Ok(true)
}

/// Drop old excerpts even for a positive edit: only a new canonical fetch may
/// publish replacement text. Ownership is checked again inside the store lock.
pub async fn verified_update(
    state: &AppState,
    actor: &str,
    document: &Value,
) -> Result<(), plamenu_db::DbError> {
    if let Some(uri) = id_of(document) {
        db::invalidate_owned_video(&state.pool, actor, uri, false, "video_updated").await?;
    }
    if let Some(uri) = id_of(document)
        && author_uri(document) == Some(actor)
    {
        db::invalidate_owned_post(&state.pool, actor, uri, false, "post_updated").await?;
    }
    Ok(())
}

/// Called only for an authenticated sender, before ordinary deletion handling.
pub async fn verified_delete(
    state: &AppState,
    actor: Option<&str>,
    uri: &str,
) -> Result<(), plamenu_db::DbError> {
    if let Some(actor) = actor {
        if actor == uri {
            db::invalidate_entry(
                &state.pool,
                uri,
                db::Invalidation::Withdrawn,
                "actor_deleted",
            )
            .await?;
        } else {
            db::invalidate_owned_video(&state.pool, actor, uri, true, "video_deleted").await?;
            db::invalidate_owned_post(&state.pool, actor, uri, true, "post_deleted").await?;
        }
    }
    Ok(())
}
