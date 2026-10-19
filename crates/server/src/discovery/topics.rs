//! Explicit connections in the local catalogue. No crawling, inference, imports
//! or persistent trail history. Each hop rechecks the same live eligibility.
use std::collections::{BTreeSet, HashMap};

use plamenu_db::{account, collection, discovery_store as db};
use serde_json::Value;
use time::OffsetDateTime;

use super::{
    browse::{Cursor, Page},
    policy::Use,
};
use crate::{AppState, error::ApiError};

#[must_use]
pub fn explicit_tags(document: &Value) -> Vec<String> {
    plamenu_ap::activity::one_or_many(document.get("tag"))
        .iter()
        .take(64)
        .filter(|tag| tag.get("type").and_then(Value::as_str) == Some("Hashtag"))
        .filter_map(|tag| {
            tag.get("name")
                .and_then(Value::as_str)
                .and_then(db::normalize_topic)
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(db::TOPICS_PER_ENTRY)
        .collect()
}

pub async fn enabled(state: &AppState) -> Result<bool, ApiError> {
    let settings = db::settings(&state.pool).await?;
    Ok(settings.enabled
        && !settings
            .disabled_mechanisms
            .iter()
            .any(|m| m == "recommendations"))
}

/// Post filters apply to trails and topic labels too, before exposing any text
/// or relations. A searchable but non-discoverable author is never promoted.
pub async fn eligible(
    state: &AppState,
    viewer: i64,
    id: i64,
) -> Result<Option<db::Entry>, ApiError> {
    if !enabled(state).await? {
        return Ok(None);
    }
    let Some(entry) = db::entry_by_id(&state.pool, id).await? else {
        return Ok(None);
    };
    if entry.kind == "video" {
        let Some(mut entry) = super::videos::eligible(state, viewer, id, Use::Recommend).await?
        else {
            return Ok(None);
        };
        let filters = crate::filters::compiled_filters(&state.pool, viewer).await?;
        return Ok(super::posts::apply_filters(state, &filters, &mut entry)
            .await?
            .then_some(entry));
    }
    if entry.kind != "post" {
        return Ok(super::browse::eligible(state, viewer, id).await?);
    }
    let Some(mut entry) = super::posts::eligible(state, viewer, id, Use::Recommend).await? else {
        return Ok(None);
    };
    let filters = crate::filters::compiled_filters(&state.pool, viewer).await?;
    Ok(super::posts::apply_filters(state, &filters, &mut entry)
        .await?
        .then_some(entry))
}

pub async fn page(
    state: &AppState,
    viewer: i64,
    cursor: Option<Cursor>,
    topic: &str,
) -> Result<Page, ApiError> {
    let mut position = match cursor {
        Some(c) => c,
        None => Cursor {
            rotation: OffsetDateTime::now_utc().unix_timestamp() / 86400 + 1,
            watermark: db::browse_watermark(&state.pool).await?,
            ..Cursor::default()
        },
    };
    let mut entries = Vec::new();
    if !enabled(state).await? {
        return Ok(Page {
            entries,
            next: None,
        });
    }
    let candidates = db::catalogue_positions(
        &state.pool,
        viewer,
        position.watermark,
        position.ordinal,
        position.after,
        db::BrowseFilter {
            kind: "all",
            topic,
            ..Default::default()
        },
        (
            position.rotation,
            position.source_order,
            position.creator_ordinal,
        ),
    )
    .await?;
    let full = candidates.len() == 200;
    let mut origins = HashMap::<String, usize>::new();
    for candidate in candidates {
        if entries.len() == 20 || origins.get(&candidate.origin).copied().unwrap_or(0) == 4 {
            return Ok(Page {
                entries,
                next: Some(position),
            });
        }
        position.creator_ordinal = candidate.creator_ordinal;
        position.ordinal = candidate.ordinal;
        position.source_order = candidate.source_order;
        position.after = candidate.id;
        if let Some(entry) = eligible(state, viewer, candidate.id).await? {
            *origins.entry(candidate.origin).or_default() += 1;
            entries.push(entry);
        }
    }
    Ok(Page {
        entries,
        next: full.then_some(position),
    })
}

/// A bounded sample of available topics, derived only from visible cards.
/// Sensitive cards don't advertise their tags outside their disclosure.
pub async fn available(state: &AppState, viewer: i64) -> Result<Vec<String>, ApiError> {
    let mut topics = BTreeSet::new();
    if !enabled(state).await? {
        return Ok(Vec::new());
    }
    let candidates = db::catalogue_positions(
        &state.pool,
        viewer,
        db::browse_watermark(&state.pool).await?,
        0,
        0,
        db::BrowseFilter {
            kind: "all",
            ..Default::default()
        },
        (0, 0, 0),
    )
    .await?;
    for candidate in candidates {
        if let Some(entry) = eligible(state, viewer, candidate.id).await?
            && !entry.sensitive
        {
            topics.extend(db::entry_topics(&state.pool, entry.id).await?);
        }
    }
    for c in collections(state, viewer, "", "").await? {
        if !c.collection.sensitive
            && let Some(id) = c.collection.tag_id
            && let Some(name) = plamenu_db::tag::name_of(&state.pool, id).await?
            && let Some(topic) = db::normalize_topic(&name)
        {
            topics.insert(topic);
        }
    }
    Ok(topics.into_iter().take(80).collect())
}

pub struct PublicCollection {
    pub collection: collection::Collection,
    pub owner: account::Account,
    pub entries: Vec<db::Entry>,
}

pub async fn public_collection(
    state: &AppState,
    viewer: i64,
    id: i64,
) -> Result<Option<PublicCollection>, ApiError> {
    if !enabled(state).await? {
        return Ok(None);
    }
    // This also enforces owner blocks/mutes and public local ownership.
    if !db::collection_available(&state.pool, viewer, id).await? {
        return Ok(None);
    }
    let Some(collection) = collection::find(&state.pool, id).await? else {
        return Ok(None);
    };
    if !collection.local || !collection.discoverable {
        return Ok(None);
    }
    let Some(owner) = account::find_by_id(&state.pool, collection.account_id).await? else {
        return Ok(None);
    };
    if !owner.is_local()
        || owner.discoverable != Some(true)
        || owner.suspended()
        || account::effectively_silenced(&state.pool, owner.id).await?
        || !crate::instance_policy::account_visible(&state.pool, &state.config.domain, &owner)
            .await?
    {
        return Ok(None);
    }
    let mut entries = Vec::new();
    for item in collection::items_for(&state.pool, id, collection.account_id, Some(viewer))
        .await?
        .into_iter()
        .take(25)
    {
        if item.state != "accepted" {
            continue;
        }
        let Some(member) = item.account_id else {
            continue;
        };
        let Some(member) = account::find_by_id(&state.pool, member).await? else {
            continue;
        };
        if !member.is_local() && item.approval_uri.is_none() {
            continue;
        }
        let Some(uri) = member.uri else {
            continue;
        };
        if let Some(entry) = db::entry(&state.pool, &uri).await?
            && let Some(entry) = eligible(state, viewer, entry.id).await?
        {
            entries.push(entry);
        }
    }
    Ok((!entries.is_empty()).then_some(PublicCollection {
        collection,
        owner,
        entries,
    }))
}

pub async fn collections(
    state: &AppState,
    viewer: i64,
    topic: &str,
    actor: &str,
) -> Result<Vec<PublicCollection>, ApiError> {
    let mut result = Vec::new();
    if !enabled(state).await? {
        return Ok(result);
    }
    for id in db::topic_collections(&state.pool, viewer, topic, actor).await? {
        if let Some(collection) = public_collection(state, viewer, id).await?
            && (actor.is_empty() || collection.entries.iter().any(|e| e.canonical_uri == actor))
        {
            result.push(collection);
            if result.len() == 20 {
                break;
            }
        }
    }
    Ok(result)
}

pub async fn authored(
    state: &AppState,
    viewer: i64,
    actor: &str,
) -> Result<Vec<db::Entry>, ApiError> {
    let mut entries = Vec::new();
    let candidates = db::catalogue_positions(
        &state.pool,
        viewer,
        db::browse_watermark(&state.pool).await?,
        0,
        0,
        db::BrowseFilter {
            kind: "post",
            actor,
            ..Default::default()
        },
        (0, 0, 0),
    )
    .await?;
    for candidate in candidates {
        if let Some(entry) = eligible(state, viewer, candidate.id).await? {
            entries.push(entry);
        }
        if entries.len() == 4 {
            break;
        }
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_topics_only_explicit_bounded_hashtags() {
        let doc = serde_json::json!({"summary":"#inferred", "tag":[
            {"type":"Hashtag","name":"#Gardens"}, {"type":"Hashtag","name":"gardens"},
            {"type":"Mention","name":"#hidden"}, {"type":"Hashtag","name":"#сад"},
            {"type":"Hashtag","name":"https://evil.example"}, {"type":"Hashtag","name":"two words"}
        ]});
        assert_eq!(explicit_tags(&doc), ["gardens", "сад"]);
        let doc = serde_json::json!({"tag":(0..100).map(|n|serde_json::json!({"type":"Hashtag","name":format!("tag{n}")})).collect::<Vec<_>>()});
        assert_eq!(explicit_tags(&doc).len(), 16);
        assert!(explicit_tags(&serde_json::json!({"tag":"#not_a_tag_object"})).is_empty());
    }
}
