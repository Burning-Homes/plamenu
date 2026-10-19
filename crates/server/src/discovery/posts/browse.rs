use super::{ApiError, AppState, Entry, OffsetDateTime, Use, db, eligible, status};
use crate::discovery::browse::{Cursor, Page};
use std::collections::HashMap;

pub async fn page(
    state: &AppState,
    viewer: i64,
    cursor: Option<Cursor>,
    term: &str,
) -> Result<Page, ApiError> {
    let mut position = match cursor {
        Some(cursor) => cursor,
        None => Cursor {
            rotation: OffsetDateTime::now_utc().unix_timestamp() / 86400 + 1,
            watermark: db::browse_watermark(&state.pool).await?,
            ..Cursor::default()
        },
    };
    let candidates = db::catalogue_positions(
        &state.pool,
        viewer,
        position.watermark,
        position.ordinal,
        position.after,
        db::BrowseFilter {
            source: None,
            kind: "post",
            term,
            ..db::BrowseFilter::default()
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
        if term.is_empty()
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
        if let Some(mut entry) = eligible(
            state,
            viewer,
            candidate.id,
            if term.is_empty() {
                Use::Recommend
            } else {
                Use::IndexPost
            },
        )
        .await?
            && apply_filters(state, &filters, &mut entry).await?
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

pub(crate) async fn apply_filters(
    state: &AppState,
    filters: &[crate::filters::CompiledFilter],
    entry: &mut Entry,
) -> Result<bool, ApiError> {
    apply_filters_in_context(state, filters, entry, "public").await
}

pub(crate) async fn apply_filters_in_context(
    state: &AppState,
    filters: &[crate::filters::CompiledFilter],
    entry: &mut Entry,
    context: &str,
) -> Result<bool, ApiError> {
    let known = status::find_by_uri(&state.pool, &entry.canonical_uri).await?;
    let ids: Vec<_> = known.iter().map(|s| s.id).collect();
    let text = format!(
        "{}\n{}",
        entry.title.as_deref().unwrap_or_default(),
        entry.summary.as_deref().unwrap_or_default()
    );
    let matched = crate::filters::filtered_value(filters, &text, &ids)?;
    for matched in matched.as_array().into_iter().flatten() {
        let filter = &matched["filter"];
        if !filter["context"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(context)))
        {
            continue;
        }
        match filter["filter_action"].as_str() {
            Some("hide") => return Ok(false),
            Some("warn" | "blur") => {
                entry.sensitive = true;
                let title = filter["title"].as_str().unwrap_or_default();
                entry.title = Some(format!(
                    "{title} · {}",
                    entry.title.as_deref().unwrap_or_default()
                ));
            }
            _ => {}
        }
    }
    Ok(true)
}
