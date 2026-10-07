//! Shared, bounded admission for an entire remote multivariant ladder.
use std::sync::Arc;
use std::time::Instant;

use sha2::{Digest, Sha256};

use super::{hls, hls_reload};
use crate::error::ApiError;
use crate::state::AppState;

pub async fn snapshot(
    state: &AppState,
    media: i64,
    lane: &hls::Lane,
    revision: i64,
    flight: &tokio::sync::Mutex<hls_reload::MasterFlight>,
    record_outcome: bool,
) -> Result<Arc<hls_reload::MasterSnapshot>, ApiError> {
    let mut entry = flight.lock().await;
    if entry.revision == revision
        && entry
            .refreshed
            .is_some_and(|at| at.elapsed() < hls_reload::REFRESH)
    {
        if record_outcome {
            record(state, media, &mut entry).await;
        }
        return entry.snapshot.clone().ok_or_else(hls_reload::unavailable);
    }
    let result = inspect(state, media, lane, revision).await;
    entry.revision = revision;
    entry.refreshed = Some(Instant::now());
    entry.snapshot = result.as_ref().ok().map(Arc::clone);
    entry.origin_failed = matches!(result, Err(ApiError::NotFound));
    entry.outcome_recorded = false;
    if record_outcome {
        record(state, media, &mut entry).await;
    }
    result
}

async fn record(state: &AppState, media: i64, entry: &mut hls_reload::MasterFlight) {
    if !entry.outcome_recorded && (entry.snapshot.is_some() || entry.origin_failed) {
        entry.outcome_recorded = true;
        hls::note_hls_result(state, "hls-master", media, !entry.origin_failed).await;
    }
}

async fn inspect(
    state: &AppState,
    media: i64,
    lane: &hls::Lane,
    revision: i64,
) -> Result<Arc<hls_reload::MasterSnapshot>, ApiError> {
    hls::ensure_reload_current(state, media, lane, revision).await?;
    let bytes = {
        let _permit = hls_reload::fetch_permit()?;
        tracing::debug!(
            media,
            resource = "master",
            "remote live reload metadata fetch"
        );
        state
            .federation
            .fetch_media_limited(
                &lane.master_url,
                crate::video_mirrors::manifest::MAX_PLAYLIST_BYTES as u64,
            )
            .await
            .map_err(|_| ApiError::NotFound)?
            .bytes
    };
    let body = std::str::from_utf8(&bytes).map_err(|_| ApiError::NotFound)?;
    if body
        .trim_start_matches('\u{feff}')
        .lines()
        .next()
        .map(str::trim)
        != Some("#EXTM3U")
    {
        return Err(ApiError::NotFound);
    }
    let ladder = if let Some(children) = children(body, &lane.master_url) {
        qualify(state, media, revision, body, children).await
    } else {
        None
    };
    hls::ensure_reload_current(state, media, lane, revision).await?;
    Ok(Arc::new(hls_reload::MasterSnapshot {
        body: body.into(),
        ladder,
    }))
}

async fn qualify(
    state: &AppState,
    media: i64,
    revision: i64,
    master: &str,
    children: Vec<String>,
) -> Option<hls_reload::Ladder> {
    let mut shape = None;
    // Sequential, shared child flights: one preflight cannot fan out eight
    // simultaneous reads or create detached work after its request is dropped.
    for child in &children {
        let flight = hls_reload::flight(media, child).ok()?;
        let snapshot = hls::reload_snapshot(state, media, child, revision, &flight, false)
            .await
            .ok()?;
        let closed = snapshot.closed.as_ref()?;
        let current = (closed.target, closed.ended, closed.event);
        if shape.is_some_and(|previous| previous != current) {
            return None;
        }
        shape = Some(current);
    }
    let (target, ended, event) = shape?;
    Some(hls_reload::Ladder {
        children,
        fingerprint: Sha256::digest(master.as_bytes()).into(),
        target,
        ended,
        event,
    })
}

fn children(body: &str, master: &str) -> Option<Vec<String>> {
    let body = body.trim_start_matches('\u{feff}');
    let base = url::Url::parse(master).ok()?;
    let prefix = base.join(".").ok()?.to_string();
    let mut children = Vec::new();
    let mut pending = false;
    let mut variant = false;
    for line in body.lines().map(str::trim) {
        let uri = if line.starts_with("#EXT-X-STREAM-INF:") {
            if pending {
                return None;
            }
            pending = true;
            variant = true;
            continue;
        } else if let Some(attributes) = line.strip_prefix("#EXT-X-MEDIA:") {
            match hls::attr(attributes, "TYPE").as_deref() {
                Some("AUDIO" | "VIDEO") => hls::attr(attributes, "URI"),
                Some("CLOSED-CAPTIONS") if hls::attr(attributes, "URI").is_none() => None,
                _ => return None,
            }
        } else if line.starts_with("#EXT-X-I-FRAME")
            || line.starts_with("#EXT-X-DEFINE:")
            || line.starts_with("#EXT-X-SESSION-KEY:")
            || line.starts_with("#EXT-X-SERVER-CONTROL:")
        {
            return None;
        } else if !line.is_empty() && !line.starts_with('#') {
            if !pending {
                return None;
            }
            pending = false;
            Some(line.to_owned())
        } else {
            continue;
        };
        if let Some(uri) = uri {
            let url = base.join(&uri).ok()?.to_string();
            if url.len() > 4096
                || !url.starts_with(&prefix)
                || !plamenu_federation::is_federation_url(&url)
                || !hls::is_m3u8(&url)
            {
                return None;
            }
            if !children.contains(&url) {
                children.push(url);
            }
            if children.len() > 8 {
                return None;
            }
        }
    }
    (!pending && variant && !children.is_empty()).then_some(children)
}

/// A child may have changed after preflight. Withdraw the whole ladder's
/// capability before any inconsistent child response leaves the handler.
pub async fn advertised(
    flight: &tokio::sync::Mutex<hls_reload::MasterFlight>,
    revision: i64,
    fingerprint: [u8; 32],
    closed: &hls_reload::ClosedPlaylist,
) -> bool {
    let mut entry = flight.lock().await;
    if entry.revision != revision {
        return false;
    }
    let Some(snapshot) = entry.snapshot.as_ref() else {
        return false;
    };
    let Some(ladder) = snapshot.ladder.as_ref() else {
        return false;
    };
    if ladder.fingerprint != fingerprint {
        return false;
    }
    if ladder.target != closed.target
        || ladder.ended != closed.ended
        || ladder.event != closed.event
    {
        // Retain the raw master so ordinary playback continues. No per-viewer
        // retry/fan-out is needed until the shared one-second preflight expires.
        entry.snapshot = Some(Arc::new(hls_reload::MasterSnapshot {
            body: snapshot.body.clone(),
            ladder: None,
        }));
        return false;
    }
    !ladder.ended
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write;
    #[test]
    fn alternatives_count_toward_unique_cap_and_unsupported_groups_are_not_skipped() {
        let master = "https://owned.invalid/live/master.m3u8";
        let mut variants = String::new();
        for n in 0..8 {
            writeln!(variants, "#EXT-X-STREAM-INF:BANDWIDTH=1000\n{n}.m3u8").unwrap();
        }
        assert_eq!(
            children(&("#EXTM3U\n".to_owned() + &variants), master)
                .unwrap()
                .len(),
            8
        );
        let audio = "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",URI=\"audio.m3u8\"\n";
        assert!(children(&("#EXTM3U\n".to_owned() + audio + &variants), master).is_none());
        let duplicate = "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",URI=\"0.m3u8\"\n";
        assert_eq!(
            children(&("#EXTM3U\n".to_owned() + duplicate + &variants), master)
                .unwrap()
                .len(),
            8
        );
        assert!(children("#EXTM3U\n#EXT-X-MEDIA:TYPE=SUBTITLES,URI=\"text.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH=1\n0.m3u8\n", master).is_none());
        assert!(
            children(
                "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nhttps://elsewhere.invalid/0.m3u8\n",
                master
            )
            .is_none()
        );
    }
}
