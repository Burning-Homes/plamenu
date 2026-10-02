//! Opt-in, bounded native delivery aggregates. No viewer identifiers, events,
//! cookies, IPs, referrers or user agents enter the collector or database.
//! Requests count only once the body emits data; bytes are polled body bytes,
//! not confirmed wire delivery, views, P2P totals or unique people.
use crate::{AppState, error::ApiError};
use axum::{
    body::{Body, BodyDataStream, Bytes},
    extract::{Request, State},
    http::Method,
    middleware::Next,
    response::Response,
};
use futures_util::Stream;
use plamenu_db::{DbError, video_channel_access};
use serde::Serialize;
use std::{
    collections::HashMap,
    pin::Pin,
    sync::{
        Arc, Mutex, PoisonError, Weak,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
use time::{Date, OffsetDateTime};

const MAX_BUCKETS: usize = 4096;
const FLUSH_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct Key {
    channel: i64,
    media: i64,
    day: Date,
    lane: &'static str,
    revision: i64,
}
#[derive(Clone, Copy, Default)]
struct Counts {
    requests: i64,
    bytes: i64,
}
#[derive(Default)]
struct Pending {
    buckets: HashMap<Key, Counts>,
    permits: HashMap<(i64, i64), Weak<AtomicBool>>,
}
#[derive(Default)]
pub struct Collector {
    active: AtomicBool,
    pending: Mutex<Pending>,
    /// Serializes policy changes, erasure and batch persistence. Streaming
    /// holds only the short synchronous counter lock, never a DB transaction.
    flush: tokio::sync::Mutex<()>,
}
impl Collector {
    fn permit(&self, channel: i64, revision: i64) -> Option<Arc<AtomicBool>> {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(permit) = pending
            .permits
            .get(&(channel, revision))
            .and_then(Weak::upgrade)
        {
            return Some(permit);
        }
        if pending.permits.len() >= MAX_BUCKETS {
            pending
                .permits
                .retain(|_, permit| permit.strong_count() > 0);
            if pending.permits.len() >= MAX_BUCKETS {
                return None;
            }
        }
        let permit = Arc::new(AtomicBool::new(true));
        pending
            .permits
            .insert((channel, revision), Arc::downgrade(&permit));
        Some(permit)
    }
    fn record(&self, key: Key, permit: &AtomicBool, first: bool, bytes: usize) {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        if !permit.load(Ordering::Relaxed) {
            return;
        }
        if !pending.buckets.contains_key(&key) && pending.buckets.len() >= MAX_BUCKETS {
            return;
        }
        let counts = pending.buckets.entry(key).or_default();
        counts.requests = counts.requests.saturating_add(i64::from(first));
        counts.bytes = counts
            .bytes
            .saturating_add(i64::try_from(bytes).unwrap_or(i64::MAX));
    }
    fn invalidate(&self, channel: i64) {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        pending.permits.retain(|(id, _), permit| {
            if *id != channel {
                return true;
            }
            if let Some(permit) = permit.upgrade() {
                permit.store(false, Ordering::Relaxed);
            }
            false
        });
        pending.buckets.retain(|key, _| key.channel != channel);
    }
}

struct Observed {
    body: BodyDataStream,
    collector: Arc<Collector>,
    key: Key,
    permit: Arc<AtomicBool>,
    started: bool,
}
impl Stream for Observed {
    type Item = Result<Bytes, axum::Error>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let result = Pin::new(&mut self.body).poll_next(cx);
        if let Poll::Ready(Some(Ok(ref bytes))) = result
            && !bytes.is_empty()
        {
            let first = !self.started;
            self.started = true;
            // Attribute a long live response's chunks to their UTC day.
            let key = Key {
                day: OffsetDateTime::now_utc().date(),
                ..self.key
            };
            self.collector.record(key, &self.permit, first, bytes.len());
        }
        result
    }
}

enum Candidate<'a> {
    File(&'a str),
    Media(i64, &'static str),
}
#[allow(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "matches canonical server-generated route suffixes, not upload names"
)]
fn candidate(path: &str) -> Option<Candidate<'_>> {
    let tail = path.strip_prefix("/media/")?;
    if !tail.contains('/') && tail.ends_with(".mp4") {
        return Some(Candidate::File(tail));
    }
    let parts: Vec<_> = tail.split('/').take(7).collect();
    match parts.as_slice() {
        [id, "download"] | ["play", id, "video.mp4"] => {
            Some(Candidate::Media(id.parse().ok()?, "file"))
        }
        ["live", id, "stream.mp4"] | ["live", id, "rewind", _, _] => {
            Some(Candidate::Media(id.parse().ok()?, "live"))
        }
        ["hls", id, "seg"] => Some(Candidate::Media(id.parse().ok()?, "hls")),
        ["hls", id, "local" | "live", rest @ ..]
            if rest.last().is_some_and(|name| {
                name.ends_with(".m4s") || name.ends_with(".mp4") || name.ends_with(".ts")
            }) =>
        {
            Some(Candidate::Media(id.parse().ok()?, "hls"))
        }
        _ => None,
    }
}

/// Applied only to the public media router. Non-media requests, HEAD,
/// playlists/captions/posters, errors and remote publications never count.
/// With no opted-in channels there are no added request-side SQL reads.
pub async fn observe(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let eligible = request.method() == Method::GET
        && state.config.video_analytics
        && state.video_analytics.active.load(Ordering::Relaxed);
    let path = eligible.then(|| request.uri().path().to_owned());
    let mut response = next.run(request).await;
    if !response.status().is_success() {
        return response;
    }
    let Some(candidate) = path.as_deref().and_then(candidate) else {
        return response;
    };
    let (media, file, lane) = match candidate {
        Candidate::File(file) => (None, Some(file), "file"),
        Candidate::Media(id, lane) => (Some(id), None, lane),
    };
    // The handler has already enforced origin authority. This indexed lookup
    // additionally restricts accounting to a current opted-in native channel.
    // A timeout/failure skips accounting, never blocks or fails playback.
    let query = sqlx::query_as::<_, (i64, i64, i64)>(
        "SELECT v.channel_id,v.media_id,p.generation FROM public_native_videos v
         JOIN video_analytics_policies p ON p.channel_id=v.channel_id AND p.enabled
         WHERE v.media_id IN (SELECT $1::bigint
           UNION SELECT id FROM media_attachments WHERE file_name=$2
           UNION SELECT media_id FROM native_video_hls_renditions WHERE file_name=$2)
         LIMIT 1",
    )
    .bind(media)
    .bind(file)
    .fetch_optional(&state.pool);
    let Ok(Ok(Some((channel, media, revision)))) =
        tokio::time::timeout(Duration::from_millis(100), query).await
    else {
        return response;
    };
    let Some(permit) = state.video_analytics.permit(channel, revision) else {
        return response;
    };
    let body = std::mem::replace(response.body_mut(), Body::empty());
    *response.body_mut() = Body::from_stream(Observed {
        body: body.into_data_stream(),
        collector: Arc::clone(&state.video_analytics),
        permit,
        started: false,
        key: Key {
            channel,
            media,
            lane,
            revision,
            day: OffsetDateTime::now_utc().date(),
        },
    });
    response
}

#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct Policy {
    pub enabled: bool,
    pub retention_days: i32,
}
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct Daily {
    #[serde(serialize_with = "serialize_id")]
    pub media_id: i64,
    #[serde(serialize_with = "serialize_id")]
    pub status_id: i64,
    pub title: String,
    #[serde(serialize_with = "serialize_day")]
    pub day: Date,
    pub lane: String,
    pub requests: i64,
    pub body_bytes: i64,
}
#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde field adapter signature"
)]
fn serialize_day<S: serde::Serializer>(day: &Date, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&day.to_string())
}
#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde field adapter signature"
)]
fn serialize_id<S: serde::Serializer>(id: &i64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&id.to_string())
}

pub async fn access(state: &AppState, actor: i64, channel: i64) -> Result<String, ApiError> {
    match video_channel_access::role(&state.pool, channel, actor)
        .await?
        .as_deref()
    {
        Some(role @ ("owner" | "editor")) => Ok(role.to_owned()),
        _ => Err(ApiError::NotFound),
    }
}
pub async fn policy(state: &AppState, channel: i64) -> Result<Policy, ApiError> {
    Ok(sqlx::query_as(
        "SELECT enabled,retention_days FROM video_analytics_policies WHERE channel_id=$1",
    )
    .bind(channel)
    .fetch_optional(&state.pool)
    .await
    .map_err(DbError::from)?
    .unwrap_or(Policy {
        enabled: false,
        retention_days: 30,
    }))
}
pub async fn daily(state: &AppState, channel: i64) -> Result<Vec<Daily>, ApiError> {
    if !state.config.video_analytics {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_as(
        "SELECT d.media_id,v.status_id,COALESCE(s.title,'Video') AS title,d.day,d.lane,d.requests,d.body_bytes FROM video_delivery_daily d
        JOIN video_analytics_policies p ON p.channel_id=d.channel_id AND p.enabled
        JOIN video_publications v ON v.media_id=d.media_id AND v.channel_id=d.channel_id
        JOIN statuses s ON s.id=v.status_id AND s.deleted_at IS NULL
        WHERE d.channel_id=$1 AND d.day >= (now() AT TIME ZONE 'UTC')::date-(p.retention_days-1)
        ORDER BY d.day DESC,d.media_id DESC,d.lane LIMIT 1000\n-- STUBFILTER: exclude deleted statuses directly or through public_native_videos",
    )
    .bind(channel)
    .fetch_all(&state.pool)
    .await
    .map_err(DbError::from)?)
}

/// Only the current owner can change collection or erase it. Channel locks
/// fence grants/transfer; collector generation fences already-open bodies.
pub async fn change(
    state: &AppState,
    actor: i64,
    channel: i64,
    enabled: bool,
    retention: i32,
    erase: bool,
) -> Result<(), ApiError> {
    if !erase && !(1..=90).contains(&retention) {
        return Err(ApiError::Unprocessable("Choose 1–90 retention days".into()));
    }
    let _flush = state.video_analytics.flush.lock().await;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    if video_channel_access::lock_access(&mut tx, channel, actor)
        .await?
        .as_deref()
        != Some("owner")
    {
        return Err(ApiError::NotFound);
    }
    // Erasure must not overwrite a concurrent settings change using an old
    // page/API snapshot, nor apply unsaved inputs from the same HTML form.
    let (enabled, retention) = if erase {
        sqlx::query_as::<_,(bool,i32)>("SELECT enabled,retention_days FROM video_analytics_policies WHERE channel_id=$1 FOR UPDATE")
            .bind(channel).fetch_optional(&mut *tx).await.map_err(DbError::from)?.unwrap_or((false,30))
    } else {
        (enabled, retention)
    };
    state.video_analytics.invalidate(channel);
    sqlx::query("INSERT INTO video_analytics_policies(channel_id,enabled,retention_days) VALUES($1,$2,$3)
        ON CONFLICT(channel_id) DO UPDATE SET enabled=EXCLUDED.enabled,retention_days=EXCLUDED.retention_days,generation=video_analytics_policies.generation+1")
        .bind(channel).bind(enabled).bind(retention).execute(&mut *tx).await.map_err(DbError::from)?;
    sqlx::query("DELETE FROM video_delivery_daily WHERE channel_id=$1 AND ($2 OR day < (now() AT TIME ZONE 'UTC')::date-($3-1))")
        .bind(channel).bind(erase || !enabled).bind(retention).execute(&mut *tx).await.map_err(DbError::from)?;
    let active = state.config.video_analytics
        && sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM video_analytics_policies WHERE enabled)",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    state
        .video_analytics
        .active
        .store(active, Ordering::Relaxed);
    Ok(())
}

/// Bounded best-effort batches. A crash/database failure may undercount;
/// no unbounded durable event queue or ambiguous-commit retry double count.
pub async fn flush(state: &AppState) -> Result<(), DbError> {
    let _flush = state.video_analytics.flush.lock().await;
    let buckets = {
        let mut pending = state
            .video_analytics
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        std::mem::take(&mut pending.buckets)
    };
    let mut tx = state.pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout='5s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut *tx)
        .await?;
    // Row locks fence concurrent policy changes on other nodes as well.
    let mut channels: Vec<_> = buckets.keys().map(|key| key.channel).collect();
    channels.sort_unstable();
    channels.dedup();
    sqlx::query("SELECT channel_id FROM video_analytics_policies WHERE channel_id=ANY($1) ORDER BY channel_id FOR UPDATE")
        .bind(&channels).fetch_all(&mut *tx).await?;
    let active = state.config.video_analytics
        && sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM video_analytics_policies WHERE enabled)",
        )
        .fetch_one(&mut *tx)
        .await?;
    state
        .video_analytics
        .active
        .store(active, Ordering::Relaxed);
    if active && !buckets.is_empty() {
        let mut media = Vec::with_capacity(buckets.len());
        let mut days = Vec::with_capacity(buckets.len());
        let mut lanes = Vec::with_capacity(buckets.len());
        let mut requests = Vec::with_capacity(buckets.len());
        let mut bytes = Vec::with_capacity(buckets.len());
        let mut revisions = Vec::with_capacity(buckets.len());
        let mut channel_ids = Vec::with_capacity(buckets.len());
        for (key, counts) in buckets {
            channel_ids.push(key.channel);
            media.push(key.media);
            days.push(key.day);
            lanes.push(key.lane.to_owned());
            requests.push(counts.requests);
            bytes.push(counts.bytes);
            revisions.push(key.revision);
        }
        sqlx::query("WITH incoming AS (SELECT * FROM UNNEST(
            $1::bigint[],$2::bigint[],$3::date[],$4::text[],$5::bigint[],$6::bigint[],$7::bigint[])
            AS i(channel_id,media_id,day,lane,requests,body_bytes,generation))
            INSERT INTO video_delivery_daily(channel_id,media_id,day,lane,requests,body_bytes)
            SELECT v.channel_id,v.media_id,i.day,i.lane,i.requests,i.body_bytes FROM incoming i
            JOIN public_native_videos v ON v.channel_id=i.channel_id AND v.media_id=i.media_id
            JOIN video_analytics_policies p ON p.channel_id=v.channel_id AND p.enabled AND p.generation=i.generation
            WHERE i.day >= (now() AT TIME ZONE 'UTC')::date-(p.retention_days-1)
            ON CONFLICT(channel_id,media_id,day,lane) DO UPDATE SET
              requests=LEAST(video_delivery_daily.requests::numeric+EXCLUDED.requests,9223372036854775807)::bigint,
              body_bytes=LEAST(video_delivery_daily.body_bytes::numeric+EXCLUDED.body_bytes,9223372036854775807)::bigint")
            .bind(channel_ids).bind(media).bind(days).bind(lanes).bind(requests).bind(bytes).bind(revisions)
            .execute(&mut *tx).await?;
    }
    sqlx::query("WITH expired AS (SELECT d.channel_id,d.media_id,d.day,d.lane FROM video_delivery_daily d
        JOIN video_analytics_policies p ON p.channel_id=d.channel_id
        WHERE (NOT p.enabled OR NOT $1 OR d.day < (now() AT TIME ZONE 'UTC')::date-(p.retention_days-1)
        OR NOT EXISTS(SELECT 1 FROM video_publications v JOIN statuses s ON s.id=v.status_id
          WHERE v.media_id=d.media_id AND v.channel_id=d.channel_id AND s.deleted_at IS NULL))
        LIMIT 4096 FOR UPDATE OF d SKIP LOCKED)
        DELETE FROM video_delivery_daily d USING expired e
        WHERE (d.channel_id,d.media_id,d.day,d.lane)=(e.channel_id,e.media_id,e.day,e.lane)\n-- STUBFILTER: exclude deleted statuses directly or through public_native_videos")
        .bind(state.config.video_analytics).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
#[must_use]
pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Err(error) = flush(&state).await {
                tracing::warn!(%error, "video delivery analytics batch dropped; playback unaffected");
                state.video_analytics.active.store(false, Ordering::Relaxed);
            }
            if !crate::workers::pause(&state, FLUSH_INTERVAL).await {
                if let Err(error) = flush(&state).await {
                    tracing::warn!(%error, "final video analytics batch dropped");
                }
                return;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn body_accounting_does_not_read_ahead_or_count_unpolled_chunks() {
        use futures_util::{StreamExt, stream};
        let collector = Arc::new(Collector::default());
        let key = Key {
            channel: 1,
            media: 2,
            revision: 0,
            day: OffsetDateTime::now_utc().date(),
            lane: "live",
        };
        let body = Body::from_stream(stream::iter([
            Ok::<_, std::io::Error>(Bytes::from_static(b"abc")),
            Ok(Bytes::from_static(b"not polled")),
        ]));
        let mut observed = Observed {
            body: body.into_data_stream(),
            collector: Arc::clone(&collector),
            key,
            permit: collector.permit(1, 0).unwrap(),
            started: false,
        };
        assert!(collector.pending.lock().unwrap().buckets.is_empty());
        assert_eq!(observed.next().await.unwrap().unwrap(), b"abc"[..]);
        drop(observed);
        let pending = collector.pending.lock().unwrap();
        assert_eq!(pending.buckets[&key].requests, 1);
        assert_eq!(pending.buckets[&key].bytes, 3);
    }
    #[test]
    fn paths_exclude_metadata_and_unrelated_media() {
        for path in [
            "/",
            "/media/a.jpg",
            "/media/5/captions.vtt",
            "/media/hls/5/master.m3u8",
            "/media/hls/5/sha256.json",
            "/media/hls/5/native/360/index.m3u8",
            "/media/proxy/av/5",
        ] {
            assert!(candidate(path).is_none(), "{path}");
        }
        for path in [
            "/media/a.mp4",
            "/media/5/download",
            "/media/play/5/video.mp4",
            "/media/hls/5/seg",
            "/media/live/5/stream.mp4",
            "/media/hls/5/local/2/3.m4s",
        ] {
            assert!(candidate(path).is_some(), "{path}");
        }
    }
    #[test]
    fn bounded_saturating_counters_and_erasure_generation() {
        let collector = Collector::default();
        let permit = collector.permit(1, 0).unwrap();
        let unrelated = collector.permit(2, 0).unwrap();
        let key = Key {
            channel: 1,
            media: 2,
            revision: 0,
            day: OffsetDateTime::now_utc().date(),
            lane: "file",
        };
        for media in 0..i64::try_from(MAX_BUCKETS).unwrap() + 10 {
            collector.record(Key { media, ..key }, &permit, true, 32);
        }
        assert_eq!(collector.pending.lock().unwrap().buckets.len(), MAX_BUCKETS);
        collector.record(key, &permit, false, usize::MAX);
        collector.record(key, &permit, true, 1);
        assert_eq!(
            collector.pending.lock().unwrap().buckets[&key].bytes,
            i64::MAX
        );
        collector.invalidate(1);
        collector.record(key, &permit, true, 9);
        assert!(collector.pending.lock().unwrap().buckets.is_empty());
        collector.record(Key { channel: 2, ..key }, &unrelated, true, 9);
        assert_eq!(collector.pending.lock().unwrap().buckets.len(), 1);
    }
    #[test]
    fn live_permits_are_bounded_and_dead_permits_are_reclaimed() {
        let collector = Collector::default();
        let permits: Vec<_> = (0..MAX_BUCKETS)
            .map(|channel| {
                collector
                    .permit(i64::try_from(channel).unwrap(), 0)
                    .unwrap()
            })
            .collect();
        assert!(collector.permit(99999, 0).is_none());
        assert!(collector.permit(0, 0).is_some());
        drop(permits);
        assert!(collector.permit(99999, 0).is_some());
        assert_eq!(collector.pending.lock().unwrap().permits.len(), 1);
    }
}
