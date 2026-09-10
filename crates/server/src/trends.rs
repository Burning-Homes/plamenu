//! The trending-tag ranking engine — Mastodon's `Trends::Tags#refresh`. Each
//! pass rescoring every tag with a live trend row or a use today: a tag's score
//! is the squared excess of today's distinct-account count over yesterday's,
//! relative to yesterday's, then decayed from its cooldown-bounded peak. Tags
//! scoring below the decay threshold drop out; the rest are re-ranked by score.
//! `refresh_tags` does one pass (what tests call); `spawn` wraps it in the
//! long-running scheduler task (Mastodon runs it every 5 minutes).

use plamenu_db::{preview_card_provider, preview_card_trend, status_trend, tag_trend};
use tokio::task::JoinHandle;

use crate::AppState;
use crate::error::ApiError;
use crate::remote::host_of;

/// Minimum distinct accounts today before a tag can score (Mastodon's
/// `threshold`).
const THRESHOLD: f64 = 5.0;
/// Minimum favourites + reblogs before a status can score (Mastodon's status
/// `threshold`).
const STATUS_THRESHOLD: f64 = 5.0;
/// A status's score decays from creation with this half-life (Mastodon's
/// `score_halflife`, 1 hour).
const STATUS_HALFLIFE_SECS: f64 = 3600.0;
/// Decayed score below which a status stops trending (Mastodon's status
/// `decay_threshold`).
const STATUS_DECAY_THRESHOLD: f64 = 0.3;
/// A trending link's peak decays with this half-life (Mastodon's link
/// `max_score_halflife`, 8 hours).
const LINK_HALFLIFE_SECS: f64 = 8.0 * 3600.0;
/// Decayed score below which a tag stops trending (Mastodon's
/// `decay_threshold`).
const DECAY_THRESHOLD: f64 = 1.0;
/// How fast a peak score decays — Mastodon's `max_score_halflife` (4 hours).
const HALFLIFE_SECS: f64 = 4.0 * 3600.0;
/// A peak older than this is discarded, so the score can climb afresh
/// (Mastodon's `max_score_cooldown`, 2 days).
const COOLDOWN: time::Duration = time::Duration::days(2);
/// How often the scheduler refreshes trends (Mastodon: every 5 minutes).
const REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_mins(5);

/// Recomputes every candidate tag's decayed trend score and re-ranks the set.
/// Scoring always runs (the `trends_enabled` setting only gates the read
/// endpoints); `trendable_by_default` decides the `allowed` flag for tags with
/// no explicit `trendable` override.
pub async fn refresh_tags(state: &AppState) -> Result<(), ApiError> {
    refresh_tags_at(state, time::OffsetDateTime::now_utc()).await
}

/// [`refresh_tags`] against an explicit reference instant.
///
/// The instant, not just the date: it picks the candidate day *and* drives the
/// cooldown test, the peak stamp and the decay exponent, so a caller that
/// supplied only a date would still be scoring against the wall clock.
///
/// This exists for the benchmark, whose dataset is built once and read for as
/// long as it survives. Scoped to "today", the job ranked ~15,000 candidates on
/// the day the dataset was built and exactly one on every day after — while the
/// same budget printed "ok" either way, which is why the benchmark was marked
/// known-bad rather than trusted.
#[allow(clippy::cast_precision_loss)] // account counts are far below 2^52
pub async fn refresh_tags_at(state: &AppState, now: time::OffsetDateTime) -> Result<(), ApiError> {
    let today = now.date();
    let yesterday = today - time::Duration::days(1);
    let settings = state.settings_cache.get(&state.pool).await?;
    let trendable_default = settings.trendable_by_default;

    // Scoring is pure math over the one score_inputs read, so the pass
    // accumulates its decisions and flushes each kind as one statement — the
    // per-candidate writes made a pass O(candidates) round trips (~15,000 tag
    // candidates on the day the bench dataset was built).
    let mut peaks: Vec<(i64, f64)> = Vec::new();
    let mut upserts: Vec<(i64, f64, bool)> = Vec::new();
    let mut deletes: Vec<i64> = Vec::new();
    for input in tag_trend::score_inputs(&state.pool, today, yesterday).await? {
        let allowed = input.trendable.unwrap_or(trendable_default);
        let expected = if input.expected == 0 {
            1.0
        } else {
            input.expected as f64
        };
        let observed = input.observed as f64;

        let mut max_score = input.max_score.unwrap_or(0.0);
        let mut max_at = input.max_score_at;
        // Discard a peak older than the cooldown so the score can climb afresh.
        if max_at.is_none_or(|at| at < now - COOLDOWN) {
            max_score = 0.0;
        }

        let score = if expected > observed || observed < THRESHOLD {
            0.0
        } else {
            (observed - expected).powi(2) / expected
        };
        if score > max_score {
            max_score = score;
            max_at = Some(now);
            peaks.push((input.tag_id, max_score));
        }

        let decayed = match max_at {
            Some(at) => max_score * 0.5_f64.powf((now - at).as_seconds_f64() / HALFLIFE_SECS),
            None => 0.0,
        };
        if decayed >= DECAY_THRESHOLD {
            upserts.push((input.tag_id, decayed, allowed));
        } else {
            deletes.push(input.tag_id);
        }
    }
    tag_trend::set_peak_many(&state.pool, &peaks, now).await?;
    tag_trend::upsert_many(&state.pool, &upserts).await?;
    tag_trend::delete_many(&state.pool, &deletes).await?;

    tag_trend::recalculate_ranks(&state.pool).await?;
    Ok(())
}

/// Recomputes every candidate status's decayed trend score and re-ranks the set
/// (Mastodon's `Trends::Statuses#refresh`). A status's raw score is the squared
/// excess of its favourite+reblog count over 1 (0 unless that count reaches 5),
/// decayed from the post's creation with a 1-hour half-life; ineligible or
/// sub-threshold statuses drop out. `allowed` mirrors the status's `trendable`.
#[allow(clippy::cast_precision_loss)] // engagement counts are far below 2^52
pub async fn refresh_statuses(state: &AppState) -> Result<(), ApiError> {
    let now = time::OffsetDateTime::now_utc();
    let today = now.date();
    let settings = state.settings_cache.get(&state.pool).await?;
    let trendable_default = settings.trendable_by_default;

    // Accumulate-and-flush like refresh_tags_at: two statements instead of one
    // per candidate.
    let mut upserts: Vec<status_trend::TrendUpsert> = Vec::new();
    let mut deletes: Vec<i64> = Vec::new();
    for input in status_trend::score_inputs(&state.pool, today).await? {
        let observed = (input.favourites + input.reblogs) as f64;
        let score = if observed < STATUS_THRESHOLD {
            0.0
        } else {
            // expected is fixed at 1, so (observed - 1)² / 1.
            (observed - 1.0).powi(2)
        };
        let decayed = if score == 0.0 || !input.eligible {
            0.0
        } else {
            score * 0.5_f64.powf((now - input.created_at).as_seconds_f64() / STATUS_HALFLIFE_SECS)
        };
        if decayed >= STATUS_DECAY_THRESHOLD {
            let allowed = input.trendable.unwrap_or(trendable_default);
            upserts.push(status_trend::TrendUpsert {
                status_id: input.status_id,
                account_id: input.account_id,
                score: decayed,
                language: input.language.clone(),
                allowed,
            });
        } else {
            deletes.push(input.status_id);
        }
    }
    status_trend::upsert_many(&state.pool, &upserts).await?;
    status_trend::delete_many(&state.pool, &deletes).await?;

    status_trend::recalculate_ranks(&state.pool).await?;
    Ok(())
}

/// Recomputes every candidate link's decayed trend score and re-ranks the set
/// (Mastodon's `Trends::Links#refresh`). Links score like tags — today's vs
/// yesterday's distinct-account counts, decayed from a 2-day cooldown-bounded
/// peak with an 8-hour half-life — but `allowed` resolves through the card's own
/// `trendable` override, else its publisher's (domain-level) trendable flag.
#[allow(clippy::cast_precision_loss)] // account counts are far below 2^52
pub async fn refresh_links(state: &AppState) -> Result<(), ApiError> {
    let now = time::OffsetDateTime::now_utc();
    let today = now.date();
    let yesterday = today - time::Duration::days(1);
    let trendable_domains = preview_card_provider::trendable_domains(&state.pool).await?;

    // Accumulate-and-flush like refresh_tags_at; provider rows are ensured as
    // one deduplicated set (dozens of cards from one publisher used to re-issue
    // the identical per-host ensure).
    let mut hosts: Vec<String> = Vec::new();
    let mut peaks: Vec<(i64, f64)> = Vec::new();
    let mut upserts: Vec<preview_card_trend::TrendUpsert> = Vec::new();
    let mut deletes: Vec<i64> = Vec::new();
    for input in preview_card_trend::score_inputs(&state.pool, today, yesterday).await? {
        // A publisher row for every scored card's domain, so moderators can
        // review the domain even before it trends (Mastodon creates these
        // lazily via `matching_domain`/review).
        if let Some(host) = host_of(&input.url) {
            hosts.push(host.to_string());
        }
        let expected = if input.expected == 0 {
            1.0
        } else {
            input.expected as f64
        };
        let observed = input.observed as f64;

        let mut max_score = input.max_score.unwrap_or(0.0);
        let mut max_at = input.max_score_at;
        if max_at.is_none_or(|at| at < now - COOLDOWN) {
            max_score = 0.0;
        }

        let score = if expected > observed || observed < THRESHOLD {
            0.0
        } else {
            (observed - expected).powi(2) / expected
        };
        if score > max_score {
            max_score = score;
            max_at = Some(now);
            peaks.push((input.preview_card_id, max_score));
        }

        // A card with no declared language never trends (Mastodon's
        // `valid_locale?` gate).
        let decayed = match max_at {
            Some(at) if max_score > 0.0 && input.language.is_some() => {
                max_score * 0.5_f64.powf((now - at).as_seconds_f64() / LINK_HALFLIFE_SECS)
            }
            _ => 0.0,
        };
        if decayed >= DECAY_THRESHOLD {
            let allowed = match input.trendable {
                Some(trendable) => trendable,
                None => host_of(&input.url).is_some_and(|host| trendable_domains.contains(host)),
            };
            upserts.push(preview_card_trend::TrendUpsert {
                preview_card_id: input.preview_card_id,
                score: decayed,
                language: input.language.clone(),
                allowed,
            });
        } else {
            deletes.push(input.preview_card_id);
        }
    }
    hosts.sort_unstable();
    hosts.dedup();
    preview_card_provider::ensure_many(&state.pool, &hosts).await?;
    preview_card_trend::set_peak_many(&state.pool, &peaks, now).await?;
    preview_card_trend::upsert_many(&state.pool, &upserts).await?;
    preview_card_trend::delete_many(&state.pool, &deletes).await?;

    preview_card_trend::recalculate_ranks(&state.pool).await?;
    Ok(())
}

/// One full trend refresh pass across every trend type.
pub async fn refresh_all(state: &AppState) -> Result<(), ApiError> {
    refresh_tags(state).await?;
    refresh_statuses(state).await?;
    refresh_links(state).await?;
    Ok(())
}

/// Runs the trend refresh on an interval until the process exits.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("trends refresher started");
        loop {
            if !crate::workers::pause(&state, REFRESH_INTERVAL).await {
                return;
            }
            if let Err(error) = refresh_all(&state).await {
                tracing::error!(error = %error.chain(), "trends refresh failed");
            }
        }
    })
}
