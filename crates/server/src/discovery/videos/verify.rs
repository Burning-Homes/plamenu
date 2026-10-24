use super::{
    AppState, DiscoveryFormat, FederationError, Lease, Value, db, db_error, invalid, profiles,
    public_record, record_hint, transport,
};
use crate::discovery::{policy::Access, topics};
use plamenu_ap::{
    activity::{id_of, one_or_many},
    discovery::{DiscoverySignals, Signal},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

async fn refusal(state: &AppState, lease: &Lease, withdrawn: bool) -> Result<(), FederationError> {
    db::finish_entry_refusal(&state.pool, lease, withdrawn, "video_unavailable")
        .await
        .map_err(|e| db_error(&e))?;
    Ok(())
}
pub(super) fn public_video(v: &Value, hint: &db::VideoHint) -> bool {
    let authors = one_or_many(v.get("attributedTo"));
    let owner = authors
        .iter()
        .any(|v| id_of(v) == Some(hint.owner_uri.as_str()));
    let channel = authors
        .iter()
        .any(|v| id_of(v) == Some(hint.channel_uri.as_str()));
    v["type"] == "Video"
        && owner
        && (authors.len() == 1 || (authors.len() == 2 && channel))
        && one_or_many(v.get("to"))
            .iter()
            .any(|v| id_of(v) == Some(PUBLIC))
        && one_or_many(v.get("audience"))
            .iter()
            .all(|v| [Some(PUBLIC), Some(hint.channel_uri.as_str())].contains(&id_of(v)))
        && ["bto", "bcc"]
            .iter()
            .all(|k| one_or_many(v.get(*k)).is_empty())
        && ["localOnly", "suspended", "isLiveBroadcast"]
            .iter()
            .all(|k| v.get(*k).is_none_or(|v| v == &Value::Bool(false)))
        && v.get("visibility")
            .is_none_or(|v| v.as_str() == Some("public"))
        && v.get("state").is_none_or(|v| v == &Value::from(1))
        && v.get("sensitive").is_none_or(Value::is_boolean)
}
fn merge(signals: [Signal; 3]) -> Signal {
    for signal in [Signal::Deny, Signal::Invalid, Signal::Missing] {
        if signals.contains(&signal) {
            return signal;
        }
    }
    Signal::Allow
}
async fn document(
    state: &AppState,
    lease: &Lease,
    uri: &str,
    format: DiscoveryFormat,
) -> Result<Value, FederationError> {
    Ok(transport::fetch(state, lease, uri, format).await?.document)
}

pub async fn fetch(state: &AppState, lease: &Lease) -> Result<(), FederationError> {
    let Some(hint) = db::video_hint(&state.pool, lease)
        .await
        .map_err(|e| db_error(&e))?
    else {
        return Err(FederationError::DiscoveryStale);
    };
    match verify(state, lease, &hint).await {
        Err(FederationError::Status(code @ (401 | 403 | 404 | 410))) => {
            refusal(state, lease, code == 410).await
        }
        result => result,
    }
}
async fn verify(
    state: &AppState,
    lease: &Lease,
    hint: &db::VideoHint,
) -> Result<(), FederationError> {
    for actor in [&hint.owner_uri, &hint.channel_uri] {
        if profiles::access(state, actor)
            .await
            .map_err(|e| db_error(&e))?
            != Access::Allowed
        {
            return refusal(state, lease, false).await;
        }
    }
    let video = document(
        state,
        lease,
        &lease.target_uri,
        DiscoveryFormat::ActivityPub,
    )
    .await?;
    let identity = id_of(&video) == Some(lease.target_uri.as_str());
    if !identity || !public_video(&video, hint) {
        return refusal(state, lease, identity && video["type"] == "Tombstone").await;
    }
    let origin = url::Url::parse(&lease.target_uri)
        .map_err(|_| invalid())?
        .origin()
        .ascii_serialization();
    let record = document(
        state,
        lease,
        &format!("{origin}/api/v1/videos/{}", hint.uuid),
        DiscoveryFormat::Json,
    )
    .await?;
    if !public_record(&record, true)
        || !record_hint(&record, &origin).is_some_and(|(uri, h)| {
            uri == lease.target_uri
                && h.owner_uri == hint.owner_uri
                && h.channel_uri == hint.channel_uri
                && h.uuid == hint.uuid
        })
    {
        return refusal(state, lease, false).await;
    }
    let owner = actor_document(state, lease, &hint.owner_uri).await?;
    let channel = actor_document(state, lease, &hint.channel_uri).await?;
    if id_of(&owner) != Some(hint.owner_uri.as_str())
        || !matches!(
            owner["type"].as_str(),
            Some("Person" | "Service" | "Application")
        )
        || !profiles::public_profile(&owner)
        || id_of(&channel) != Some(hint.channel_uri.as_str())
        || channel["type"] != "Group"
        || !profiles::public_profile(&channel)
        || one_or_many(channel.get("attributedTo")).len() != 1
        || one_or_many(channel.get("attributedTo"))
            .first()
            .and_then(id_of)
            != Some(hint.owner_uri.as_str())
    {
        return refusal(
            state,
            lease,
            (id_of(&owner) == Some(hint.owner_uri.as_str()) && owner["type"] == "Tombstone")
                || (id_of(&channel) == Some(hint.channel_uri.as_str())
                    && channel["type"] == "Tombstone"),
        )
        .await;
    }
    for actor in [&hint.owner_uri, &hint.channel_uri] {
        if profiles::access(state, actor)
            .await
            .map_err(|e| db_error(&e))?
            != Access::Allowed
        {
            return refusal(state, lease, false).await;
        }
    }
    db::save_actor_evidence(&state.pool, lease, &hint.owner_uri, &owner)
        .await
        .map_err(|e| db_error(&e))?;
    db::save_actor_evidence(&state.pool, lease, &hint.channel_uri, &channel)
        .await
        .map_err(|e| db_error(&e))?;
    retain(state, lease, hint, &video, &owner, &channel, &record).await
}
async fn retain(
    state: &AppState,
    lease: &Lease,
    hint: &db::VideoHint,
    video: &Value,
    owner: &Value,
    channel: &Value,
    record: &Value,
) -> Result<(), FederationError> {
    let [object, creator, group] = [video, owner, channel].map(DiscoverySignals::from_document);
    let discoverable = merge([
        object.discoverable,
        creator.discoverable,
        group.discoverable,
    ]);
    let indexable = merge([object.indexable, creator.indexable, group.indexable]);
    let now = OffsetDateTime::now_utc();
    let published = video["published"]
        .as_str()
        .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
        .filter(|at| *at <= now + time::Duration::minutes(5))
        .ok_or_else(invalid)?;
    let duration = record["duration"]
        .as_i64()
        .filter(|d| (0..=604_800).contains(d))
        .and_then(|d| i32::try_from(d).ok())
        .ok_or_else(invalid)?;
    let title = profiles::text(
        &crate::filters::plain_text(video["name"].as_str().ok_or_else(invalid)?),
        1024,
    );
    let description = profiles::text(
        &crate::filters::plain_text(video["content"].as_str().unwrap_or_default()),
        8192,
    );
    let channel_title = profiles::text(
        channel["name"]
            .as_str()
            .or(channel["preferredUsername"].as_str())
            .unwrap_or_default(),
        1024,
    );
    let settings = db::settings(&state.pool).await.map_err(|e| db_error(&e))?;
    let promote = discoverable == Signal::Allow
        || (discoverable == Signal::Missing && settings.allow_missing_discoverable);
    let result = db::finish_video_entry(
        &state.pool,
        lease,
        &db::VerifiedEntry {
            kind: "video",
            actor_uri: &hint.owner_uri,
            account_id: None,
            discoverable: profiles::signal(discoverable),
            indexable: profiles::signal(indexable),
            visibility: "public",
            title: &title,
            summary: &description,
            language: profiles::declared_language(video).or_else(|| super::record_language(record)),
            verified_at: now,
            valid_until: now + time::Duration::hours(24),
            outbox_uri: None,
            published_at: Some(published),
            sensitive: video["sensitive"] == true
                || record["nsfw"] == true
                || video["summary"].as_str().is_some_and(|s| !s.is_empty()),
            author_revision: None,
        },
        &db::VerifiedVideo {
            hint,
            channel_title: if promote { &channel_title } else { "" },
            duration,
        },
        &topics::explicit_tags(video),
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

async fn actor_document(
    state: &AppState,
    lease: &Lease,
    uri: &str,
) -> Result<Value, FederationError> {
    if let Some(value) = db::actor_evidence(&state.pool, uri)
        .await
        .map_err(|e| db_error(&e))?
    {
        return Ok(value);
    }
    let document = document(state, lease, uri, DiscoveryFormat::ActivityPub).await?;
    if id_of(&document) == Some(uri) {
        let signals = DiscoverySignals::from_document(&document);
        let withdrawn = document["type"] == "Tombstone";
        let public = !withdrawn && profiles::public_profile(&document);
        if !public || matches!(signals.indexable, Signal::Deny | Signal::Invalid) {
            // Preserve this target's refusal before revoking the actor, which
            // cancels all dependent jobs (including the current lease). Fresh
            // negative evidence must also suppress other retained videos.
            refusal(state, lease, withdrawn).await?;
            if withdrawn {
                db::invalidate_entry(
                    &state.pool,
                    uri,
                    db::Invalidation::Withdrawn,
                    "actor_deleted",
                )
                .await
                .map_err(|e| db_error(&e))?;
            } else {
                db::revoke_actor(
                    &state.pool,
                    uri,
                    profiles::signal(signals.discoverable),
                    profiles::signal(signals.indexable),
                    public,
                )
                .await
                .map_err(|e| db_error(&e))?;
            }
            return Err(FederationError::DiscoveryStale);
        }
        // A public actor's discovery opt-out restricts promotion, independently
        // of indexing. Canonical ownership checks still precede caching it;
        // save_actor_evidence also fences recommendations of cached siblings.
    }
    Ok(document)
}
