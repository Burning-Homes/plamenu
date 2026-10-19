//! Member-facing catalogue reads. No resolution or acquisition occurs here.
use std::collections::{HashMap, HashSet};

use plamenu_db::{DbError, account_domain_block, discovery_store as db};
use time::OffsetDateTime;

use crate::AppState;

#[derive(Clone, Copy, Default, serde::Deserialize, serde::Serialize)]
pub struct Cursor {
    #[serde(default)]
    pub journey: Option<i64>,
    #[serde(default)]
    pub rotation: i64,
    #[serde(default)]
    pub source_order: i64,
    pub watermark: i64,
    pub ordinal: i64,
    #[serde(default)]
    pub creator_ordinal: i64,
    pub after: i64,
}

pub struct Page {
    pub entries: Vec<db::Entry>,
    pub next: Option<Cursor>,
}

pub async fn source_visible<S: std::hash::BuildHasher>(
    state: &AppState,
    source: &db::Source,
    domains: &HashSet<String, S>,
) -> Result<bool, DbError> {
    let now = OffsetDateTime::now_utc();
    let Some(domain) = crate::instance_policy::domain_from_url(&source.origin) else {
        return Ok(false);
    };
    Ok(source.enabled
        && source.verified_at.is_some_and(|v| v <= now)
        && source
            .capabilities
            .iter()
            .any(|v| v == "directory" || v == "community_directory" || v == "video_catalogue")
        && !domains.contains(&domain)
        && crate::instance_policy::can_federate_url(
            &state.pool,
            &state.config.domain,
            &source.origin,
        )
        .await?
        && !plamenu_db::instance_policy::is_domain_silenced(&state.pool, &domain).await?)
}

pub async fn eligible(
    state: &AppState,
    viewer: i64,
    id: i64,
) -> Result<Option<db::Entry>, DbError> {
    let Some(entry) = db::entry_by_id(&state.pool, id).await? else {
        return Ok(None);
    };
    if !["profile", "community"].contains(&entry.kind.as_str())
        || db::viewer_hides(&state.pool, viewer, id).await?
    {
        return Ok(None);
    }
    let domains = account_domain_block::all_domains(&state.pool, viewer).await?;
    if crate::instance_policy::domain_from_url(&entry.canonical_uri)
        .is_some_and(|domain| domains.contains(&domain))
    {
        return Ok(None);
    }
    super::profiles::eligible(state, &entry.canonical_uri).await
}

pub async fn page(
    state: &AppState,
    viewer: i64,
    cursor: Option<Cursor>,
    source: Option<i64>,
) -> Result<Page, DbError> {
    page_kind(state, viewer, cursor, source, "profile").await
}

pub async fn page_kind(
    state: &AppState,
    viewer: i64,
    cursor: Option<Cursor>,
    source: Option<i64>,
    kind: &str,
) -> Result<Page, DbError> {
    search_page_kind(state, viewer, cursor, source, kind, "").await
}

pub async fn search_page_kind(
    state: &AppState,
    viewer: i64,
    cursor: Option<Cursor>,
    source: Option<i64>,
    kind: &str,
    term: &str,
) -> Result<Page, DbError> {
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
            source,
            kind,
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
    let full_batch = candidates.len() == 200;
    let domains = account_domain_block::all_domains(&state.pool, viewer).await?;
    let mut origins = HashMap::<String, usize>::new();
    let mut entries = Vec::new();
    for candidate in candidates {
        // Stop before a capped candidate, preserving it for the next page.
        // With one source a page is four cards; extra sources fill up to 20.
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
        if crate::instance_policy::domain_from_url(&candidate.canonical_uri)
            .is_some_and(|domain| domains.contains(&domain))
        {
            continue;
        }
        if let Some(entry) = super::profiles::eligible(state, &candidate.canonical_uri).await? {
            *origins.entry(candidate.origin).or_default() += 1;
            entries.push(entry);
        }
    }
    Ok(Page {
        entries,
        next: full_batch.then_some(position),
    })
}

/// A human-readable state key; never expose remote errors or infer that an
/// unreachable endpoint deleted its profiles. Paused acquisition can still
/// serve fresh cards.
#[must_use]
pub fn status(
    settings: &db::Settings,
    diagnostic: &db::Diagnostics,
    queue: &[db::QueueDiagnostic],
) -> &'static str {
    status_for(settings, diagnostic, queue, "directory")
}

#[must_use]
pub fn status_for(
    settings: &db::Settings,
    diagnostic: &db::Diagnostics,
    queue: &[db::QueueDiagnostic],
    method: &str,
) -> &'static str {
    if !settings.enabled
        || settings
            .disabled_mechanisms
            .iter()
            .any(|v| v == "profile_listing")
    {
        "discovery-disabled"
    } else if settings.paused {
        "discovery-paused"
    } else if settings
        .disabled_mechanisms
        .iter()
        .any(|v| v == method || v == "source_inspection" || v == "metadata_cache")
    {
        "discovery-methods-disabled"
    } else if queue
        .iter()
        .any(|job| job.reason.as_deref() == Some("budget_exhausted"))
        || diagnostic.requests_24h >= settings.requests_per_day
        || diagnostic.charged_bytes_24h > settings.bytes_per_day.saturating_sub(2 * 1024 * 1024)
    {
        "discovery-budget"
    } else if diagnostic.leased == 0
        && diagnostic.sources > 0
        && diagnostic.last_failure_at > diagnostic.last_success_at
    {
        "discovery-unsupported"
    } else if diagnostic.leased > 0 || diagnostic.queued > 0 {
        "discovery-loading"
    } else {
        "discovery-ready"
    }
}

/// All main discovery tabs share the same sorting, filters and seen history.
pub async fn recovery_page(
    state: &AppState,
    viewer: i64,
    cursor: Option<Cursor>,
    options: &db::DiscoveryOptions<'_>,
) -> Result<Page, crate::error::ApiError> {
    let mut position = cursor.unwrap_or_default();
    if position.rotation == 0 {
        position.rotation = plamenu_db::id::next();
    }
    let (journey, ids) = db::discovery_journey(
        &state.pool,
        viewer,
        position.journey,
        position.rotation,
        options,
    )
    .await?;
    position.journey = Some(journey);
    position.watermark = ids.iter().copied().max().unwrap_or(0);
    let filters = crate::filters::compiled_filters(&state.pool, viewer).await?;
    let mut entries = Vec::new();
    let start = usize::try_from(position.ordinal).unwrap_or(0);
    for id in ids.iter().skip(start) {
        position.ordinal += 1;
        position.after = *id;
        let candidate = match options.kind {
            "post" => {
                super::posts::eligible(
                    state,
                    viewer,
                    *id,
                    if options.term.is_empty() {
                        super::policy::Use::Recommend
                    } else {
                        super::policy::Use::IndexPost
                    },
                )
                .await?
            }
            "video" => {
                super::videos::eligible(
                    state,
                    viewer,
                    *id,
                    if options.term.is_empty() {
                        super::policy::Use::Recommend
                    } else {
                        super::policy::Use::IndexVideo
                    },
                )
                .await?
            }
            _ => eligible(state, viewer, *id).await?,
        };
        if let Some(mut entry) = candidate
            && super::posts::apply_filters(state, &filters, &mut entry).await?
        {
            entries.push(entry);
        }
        if entries.len() == 20 {
            break;
        }
    }
    let next =
        (usize::try_from(position.ordinal).unwrap_or(usize::MAX) < ids.len()).then_some(position);
    Ok(Page { entries, next })
}
