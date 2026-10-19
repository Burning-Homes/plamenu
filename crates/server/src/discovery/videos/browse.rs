use super::{AppState, db};
use crate::{
    discovery::{
        browse::{Cursor, Page},
        policy::{Access, Outcome, Use},
        profiles, store,
    },
    error::ApiError,
};
use plamenu_db::{account_domain_block, status};
use std::collections::HashMap;
use time::OffsetDateTime;

pub async fn eligible(
    state: &AppState,
    viewer: i64,
    id: i64,
    usage: Use,
) -> Result<Option<db::Entry>, ApiError> {
    let Some(entry) = db::entry_by_id(&state.pool, id).await? else {
        return Ok(None);
    };
    if entry.kind != "video" || db::viewer_hides(&state.pool, viewer, id).await? {
        return Ok(None);
    }
    let Some(details) = db::video_details(&state.pool, id).await? else {
        return Ok(None);
    };
    let settings = db::settings(&state.pool).await?;
    let Some(source) = db::source(&state.pool, entry.source_id).await? else {
        return Ok(None);
    };
    if store::evaluate_entry(
        &settings,
        &source,
        &entry,
        Use::IndexVideo,
        Access::Allowed,
        OffsetDateTime::now_utc(),
    )?
    .outcome
        != Outcome::Allow
        || (usage == Use::Recommend
            && store::evaluate_entry(
                &settings,
                &source,
                &entry,
                Use::Recommend,
                Access::Allowed,
                OffsetDateTime::now_utc(),
            )?
            .outcome
                != Outcome::Allow)
    {
        return Ok(None);
    }
    let domains = account_domain_block::all_domains(&state.pool, viewer).await?;
    for uri in [&details.owner_uri, &details.channel_uri] {
        if crate::instance_policy::domain_from_url(uri).is_some_and(|d| domains.contains(&d))
            || db::video_actor_hidden(&state.pool, viewer, uri).await?
        {
            return Ok(None);
        }
        if profiles::access(state, uri).await? != Access::Allowed {
            db::invalidate_entry(
                &state.pool,
                &entry.canonical_uri,
                db::Invalidation::Denied,
                "local_moderation",
            )
            .await?;
            return Ok(None);
        }
    }
    if let Some(known) = status::find_by_uri(&state.pool, &entry.canonical_uri).await? {
        let deleted = status::is_deleted(&state.pool, known.id).await?;
        if known.visibility != "public" || deleted {
            db::invalidate_owned_video(
                &state.pool,
                &details.owner_uri,
                &entry.canonical_uri,
                deleted,
                "stored_video_restricted",
            )
            .await?;
            return Ok(None);
        }
        if !crate::entities::can_view(&state.pool, &known, Some(viewer)).await? {
            return Ok(None);
        }
    }
    Ok(Some(entry))
}
pub async fn page(
    state: &AppState,
    viewer: i64,
    cursor: Option<Cursor>,
    source: Option<i64>,
    term: &str,
    channel: &str,
) -> Result<Page, ApiError> {
    let mut position = match cursor {
        Some(p) => p,
        None => Cursor {
            rotation: OffsetDateTime::now_utc().unix_timestamp() / 86400 + 1,
            watermark: db::browse_watermark(&state.pool).await?,
            ..Default::default()
        },
    };
    let candidates = db::catalogue_positions(
        &state.pool,
        viewer,
        position.watermark,
        position.ordinal,
        position.after,
        db::BrowseFilter {
            source,
            kind: "video",
            term,
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
    let filters = crate::filters::compiled_filters(&state.pool, viewer).await?;
    let mut origins = HashMap::<String, usize>::new();
    let mut entries = Vec::new();
    let mut authors = std::collections::HashSet::new();
    for candidate in candidates {
        if entries.len() == 20 || origins.get(&candidate.origin).copied().unwrap_or(0) == 4 {
            return Ok(Page {
                entries,
                next: Some(position),
            });
        }
        // Preserve the candidate for the next page instead of losing it to
        // deduplication. Stable author rounds put other authors first.
        if channel.is_empty()
            && term.is_empty()
            && candidate
                .actor_uri
                .as_ref()
                .is_some_and(|a| authors.contains(a))
        {
            return Ok(Page {
                entries,
                next: Some(position),
            });
        }
        position.creator_ordinal = candidate.creator_ordinal;
        position.ordinal = candidate.ordinal;
        position.source_order = candidate.source_order;
        position.after = candidate.id;
        if !channel.is_empty()
            && db::video_details(&state.pool, candidate.id)
                .await?
                .is_none_or(|d| d.channel_uri != channel)
        {
            continue;
        }
        if let Some(mut entry) = eligible(
            state,
            viewer,
            candidate.id,
            if term.is_empty() {
                Use::Recommend
            } else {
                Use::IndexVideo
            },
        )
        .await?
            && crate::discovery::posts::apply_filters(state, &filters, &mut entry).await?
        {
            *origins.entry(candidate.origin).or_default() += 1;
            if let Some(actor) = &entry.actor_uri {
                authors.insert(actor.clone());
            }
            entries.push(entry);
        }
    }
    Ok(Page {
        entries,
        next: full.then_some(position),
    })
}
