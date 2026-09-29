//! Persisted local broadcasts. Allocation stays private until a Video Create
//! binds the publication in the caller's outbox transaction.

use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use time::OffsetDateTime;

use crate::{DbError, video};

#[derive(Debug, Clone)]
pub struct Broadcast {
    pub media_id: i64,
    pub status_id: Option<i64>,
    pub owner_account_id: i64,
    pub channel_id: i64,
    pub broadcast_uuid: String,
    pub state: String,
    pub permanent: bool,
    pub scheduled_at: Option<OffsetDateTime>,
    pub scheduled_starts: Vec<OffsetDateTime>,
    pub reconnect_deadline: Option<OffsetDateTime>,
    pub key_generation: i64,
    pub dvr_window_seconds: i32,
    pub latency_mode: i32,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct PlaybackOptions {
    pub dvr_window_seconds: i32,
    /// PeerTube-compatible values: balanced=1, resilient=2, low=3.
    pub latency_mode: i32,
}

#[derive(Debug, Clone)]
pub struct PublicDetails {
    pub channel_id: i64,
    pub playback: PlaybackOptions,
    pub scheduled_at: Option<OffsetDateTime>,
    pub scheduled_starts: Vec<OffsetDateTime>,
}

impl PlaybackOptions {
    #[must_use]
    pub const fn valid(self) -> bool {
        matches!(self.dvr_window_seconds, 0 | 300 | 600 | 900 | 1800)
            && matches!(self.latency_mode, 1..=3)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FinishReason {
    Stopped,
    Interrupted,
}

#[derive(Debug, Clone)]
pub struct ActiveSession {
    pub media_id: i64,
    pub status_id: i64,
    pub owner_account_id: i64,
    pub session_id: String,
    pub permanent: bool,
    pub playback: PlaybackOptions,
    /// Set only for RTMP/HTTPS-FLV sessions authenticated by a stream key.
    /// Cookie-authenticated browser capture is independent of key rotation.
    pub ingest_key_generation: Option<i64>,
}

/// An unpublished recording whose worker may need recovery after a restart.
/// The path is derived from these fenced identities,
/// never accepted from the database or a request as a filesystem path.
pub struct ReplayRecovery {
    pub session: ActiveSession,
    pub reconnect_deadline: Option<OffsetDateTime>,
}

pub async fn recoverable_replays(pool: &PgPool) -> Result<Vec<ReplayRecovery>, DbError> {
    let rows = sqlx::query!(
        r#"SELECT ls.media_id, b.status_id AS "status_id!", b.owner_account_id,
                  ls.session_id, b.permanent, b.reconnect_deadline,
                  b.dvr_window_seconds, b.latency_mode
           FROM native_live_sessions ls
           JOIN native_live_broadcasts b ON b.media_id = ls.media_id
           JOIN media_attachments m ON m.id = ls.media_id
           LEFT JOIN media_attachments replay ON replay.id = ls.replay_media_id
           WHERE ls.ended_at IS NOT NULL AND b.status_id IS NOT NULL
             AND (ls.replay_media_id IS NULL OR replay.status_id IS NULL)
             AND (b.permanent OR (b.state IN ('interrupted', 'ended')
                 AND m.file_name IS NULL
                 AND ls.generation = (SELECT max(newer.generation)
                                      FROM native_live_sessions newer
                                      WHERE newer.media_id = ls.media_id)))"#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| ReplayRecovery {
            session: ActiveSession {
                media_id: row.media_id,
                status_id: row.status_id,
                owner_account_id: row.owner_account_id,
                session_id: row.session_id,
                permanent: row.permanent,
                playback: PlaybackOptions {
                    dvr_window_seconds: row.dvr_window_seconds,
                    latency_mode: row.latency_mode,
                },
                ingest_key_generation: None,
            },
            reconnect_deadline: row.reconnect_deadline,
        })
        .collect())
}

#[derive(Debug, Clone)]
pub struct LiveSummary {
    pub status_id: i64,
    pub title: String,
    pub state: String,
    pub scheduled_at: Option<OffsetDateTime>,
}

pub async fn recent_owned(pool: &PgPool, owner: i64) -> Result<Vec<LiveSummary>, DbError> {
    Ok(sqlx::query_as!(
        LiveSummary,
        "SELECT b.status_id AS \"status_id!\", COALESCE(s.title, '') AS \"title!\",
                b.state, b.scheduled_at
         FROM native_live_broadcasts b JOIN statuses s ON s.id = b.status_id
         WHERE b.owner_account_id = $1 ORDER BY b.created_at DESC LIMIT 20",
        owner,
    )
    .fetch_all(pool)
    .await?)
}

/// Only a published on-air session can expose transient HLS bytes.
pub async fn public_session(pool: &PgPool, media_id: i64) -> Result<Option<String>, DbError> {
    Ok(sqlx::query_scalar!(
        "SELECT ls.session_id FROM native_live_sessions ls
         JOIN native_live_broadcasts b ON b.media_id = ls.media_id
         JOIN statuses s ON s.id = b.status_id
         WHERE ls.media_id = $1 AND ls.ended_at IS NULL AND b.state = 'live'
           AND s.visibility IN ('public', 'unlisted')",
        media_id,
    )
    .fetch_optional(pool)
    .await?)
}

#[derive(Debug, Clone)]
pub struct PublicDvr {
    pub session_id: String,
    pub started_at: OffsetDateTime,
    pub window_seconds: i32,
}

/// The live generation and its published rewind limit. Every consumer still
/// checks the ordinary media visibility gate before exposing bytes.
pub async fn public_dvr(pool: &PgPool, media_id: i64) -> Result<Option<PublicDvr>, DbError> {
    Ok(sqlx::query_as!(
        PublicDvr,
        "SELECT ls.session_id, ls.started_at, b.dvr_window_seconds AS window_seconds
         FROM native_live_sessions ls
         JOIN native_live_broadcasts b ON b.media_id = ls.media_id
         JOIN statuses s ON s.id = b.status_id
         WHERE ls.media_id = $1 AND ls.ended_at IS NULL AND b.state = 'live'
           AND b.dvr_window_seconds > 0
           AND s.visibility IN ('public', 'unlisted')",
        media_id,
    )
    .fetch_optional(pool)
    .await?)
}

#[derive(Debug, Clone)]
pub struct PublicPeerSession {
    pub media_id: i64,
    pub video_uuid: String,
}

/// A small batched sidecar for first-party live viewers. Peer sharing is
/// deliberately disabled for the one-second low-latency mode, matching
/// `PeerTube`'s own efficiency guard.
pub async fn public_peer_sessions(
    pool: &PgPool,
    media_ids: &[i64],
) -> Result<std::collections::HashMap<i64, PublicPeerSession>, DbError> {
    if media_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let rows = sqlx::query_as!(
        PublicPeerSession,
        "SELECT ls.media_id, v.uuid AS video_uuid
         FROM native_live_sessions ls
         JOIN native_live_broadcasts b ON b.media_id = ls.media_id
         JOIN video_publications v ON v.media_id = b.media_id AND v.status_id = b.status_id
         JOIN statuses s ON s.id = b.status_id
         JOIN media_attachments m ON m.id = b.media_id AND m.live_state = 'live'
         JOIN accounts a ON a.id = b.owner_account_id AND a.suspended_at IS NULL
         JOIN accounts c ON c.id = b.channel_id AND c.suspended_at IS NULL
         WHERE ls.media_id = ANY($1) AND ls.ended_at IS NULL AND b.state = 'live'
           AND b.latency_mode <> 3 AND s.visibility IN ('public', 'unlisted')
           AND ls.p2p_infohash IS NOT NULL",
        media_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|row| (row.media_id, row)).collect())
}

pub async fn public_infohash_exists(pool: &PgPool, infohash: &str) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar!(
        "SELECT EXISTS (SELECT 1 FROM native_live_sessions ls
         JOIN native_live_broadcasts b ON b.media_id = ls.media_id
         JOIN statuses s ON s.id = b.status_id
         JOIN media_attachments m ON m.id = b.media_id AND m.live_state = 'live'
         JOIN accounts a ON a.id = b.owner_account_id AND a.suspended_at IS NULL
         JOIN accounts c ON c.id = b.channel_id AND c.suspended_at IS NULL
         WHERE ls.p2p_infohash = $1 AND ls.ended_at IS NULL AND b.state = 'live'
           AND b.latency_mode <> 3 AND s.visibility IN ('public', 'unlisted'))",
        infohash,
    )
    .fetch_one(pool)
    .await?
    .unwrap_or(false))
}

pub async fn exists(pool: &PgPool, media_id: i64) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar!(
        "SELECT EXISTS(SELECT 1 FROM native_live_broadcasts WHERE media_id = $1) AS \"exists!\"",
        media_id,
    )
    .fetch_one(pool)
    .await?)
}

/// One batched lookup for rendering local live attachments. An ordinary
/// status page with no live media does not call this at all.
pub async fn details_for_media(
    pool: &PgPool,
    media_ids: &[i64],
) -> Result<std::collections::HashMap<i64, PublicDetails>, DbError> {
    if media_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let rows = sqlx::query!(
        "SELECT media_id, channel_id, dvr_window_seconds, latency_mode, scheduled_at, scheduled_starts
         FROM native_live_broadcasts WHERE media_id = ANY($1)",
        media_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            (
                row.media_id,
                PublicDetails {
                    channel_id: row.channel_id,
                    playback: PlaybackOptions {
                        dvr_window_seconds: row.dvr_window_seconds,
                        latency_mode: row.latency_mode,
                    },
                    scheduled_at: row.scheduled_at,
                    scheduled_starts: row.scheduled_starts,
                },
            )
        })
        .collect())
}

#[derive(Debug, Clone)]
pub struct CalendarBroadcast {
    pub status_id: i64,
    pub broadcast_uuid: String,
    pub title: String,
    pub scheduled_starts: Vec<OffsetDateTime>,
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone)]
pub struct CalendarOccurrence {
    pub status_id: i64,
    pub broadcast_uuid: String,
    pub title: String,
    pub start_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// A public or unlisted local broadcast may publish a calendar for people
/// who already have its URL. Private/local statuses never enter this query.
pub async fn calendar_broadcast(
    pool: &PgPool,
    status_id: i64,
) -> Result<Option<CalendarBroadcast>, DbError> {
    Ok(sqlx::query_as!(
        CalendarBroadcast,
        "SELECT b.status_id AS \"status_id!\", b.broadcast_uuid,
                CASE WHEN s.sensitive OR s.spoiler_text <> ''
                     THEN 'Sensitive live broadcast'
                     ELSE COALESCE(s.title, 'Live broadcast') END AS \"title!\",
                b.scheduled_starts, s.updated_at
         FROM native_live_broadcasts b
         JOIN statuses s ON s.id = b.status_id
         JOIN accounts a ON a.id = b.channel_id
         JOIN accounts owner ON owner.id = s.account_id
         WHERE b.status_id = $1 AND s.visibility IN ('public', 'unlisted')
           AND s.deleted_at IS NULL AND a.domain IS NULL AND a.suspended_at IS NULL
           AND owner.suspended_at IS NULL",
        status_id,
    )
    .fetch_optional(pool)
    .await?)
}

/// The channel feed is strictly public: an unlisted broadcast is never
/// disclosed by a channel-level list or calendar subscription.
pub async fn channel_calendar_broadcasts(
    pool: &PgPool,
    channel_id: i64,
) -> Result<Vec<CalendarOccurrence>, DbError> {
    Ok(sqlx::query_as!(
        CalendarOccurrence,
        "SELECT b.status_id AS \"status_id!\", b.broadcast_uuid,
                CASE WHEN s.sensitive OR s.spoiler_text <> ''
                     THEN 'Sensitive live broadcast'
                     ELSE COALESCE(s.title, 'Live broadcast') END AS \"title!\",
                future.start_at AS \"start_at!\", s.updated_at
         FROM native_live_broadcasts b
         JOIN statuses s ON s.id = b.status_id
         JOIN accounts a ON a.id = b.channel_id
         JOIN accounts owner ON owner.id = s.account_id
         CROSS JOIN LATERAL unnest(b.scheduled_starts) future(start_at)
         WHERE b.channel_id = $1 AND s.visibility = 'public'
           AND s.deleted_at IS NULL AND a.domain IS NULL AND a.suspended_at IS NULL
           AND owner.suspended_at IS NULL
           AND future.start_at > now()
         ORDER BY future.start_at, b.status_id LIMIT 200",
        channel_id,
    )
    .fetch_all(pool)
    .await?)
}

pub async fn calendar_channel_name(
    pool: &PgPool,
    channel_id: i64,
) -> Result<Option<String>, DbError> {
    Ok(sqlx::query_scalar!(
        "SELECT COALESCE(NULLIF(a.display_name, ''), a.username) AS \"name!\"
         FROM video_channels c JOIN accounts a ON a.id = c.account_id
         JOIN accounts owner ON owner.id = c.owner_account_id
         WHERE c.account_id = $1 AND a.domain IS NULL AND a.suspended_at IS NULL
           AND owner.suspended_at IS NULL",
        channel_id,
    )
    .fetch_optional(pool)
    .await?)
}

/// Edit or clear a public calendar announcement only while no publisher is
/// connected. Row-level updates serialize with `claim`/`claim_owned`.
pub async fn update_schedule(
    pool: &PgPool,
    status_id: i64,
    owner: i64,
    scheduled_starts: &[OffsetDateTime],
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let media: Option<i64> =
        sqlx::query_scalar("SELECT media_id FROM video_publications WHERE status_id=$1")
            .bind(status_id)
            .fetch_optional(&mut *tx)
            .await?;
    if let Some(media) = media {
        if !crate::video_channel_access::lock_publication(&mut tx, media, owner).await? {
            return Ok(false);
        }
    } else {
        return Ok(false);
    }
    let scheduled_at = scheduled_starts.first().copied();
    let changed = sqlx::query!(
        "UPDATE native_live_broadcasts b SET scheduled_at = $3, scheduled_starts = $4
         FROM statuses s WHERE b.status_id = $1 AND b.owner_account_id = $2
           AND s.id = b.status_id AND s.account_id = $2 AND s.uri IS NULL
           AND b.state = 'waiting'
           AND NOT EXISTS (SELECT 1 FROM native_live_sessions ls
                           WHERE ls.media_id = b.media_id AND ls.ended_at IS NULL)",
        status_id,
        owner,
        scheduled_at,
        scheduled_starts,
    )
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if changed {
        sqlx::query!(
            "UPDATE statuses SET edited_at = now(), updated_at = now() WHERE id = $1",
            status_id,
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(changed)
}

/// Change only an idle local broadcast. A running `FFmpeg` process has already
/// fixed its segment cadence/window; changing these fields mid-session would
/// advertise settings that do not match its real playlist.
pub async fn update_playback(
    pool: &PgPool,
    status_id: i64,
    owner: i64,
    options: PlaybackOptions,
) -> Result<bool, DbError> {
    if !options.valid() {
        return Ok(false);
    }
    let mut tx = pool.begin().await?;
    let media: Option<i64> =
        sqlx::query_scalar("SELECT media_id FROM video_publications WHERE status_id=$1")
            .bind(status_id)
            .fetch_optional(&mut *tx)
            .await?;
    if let Some(media) = media {
        if !crate::video_channel_access::lock_publication(&mut tx, media, owner).await? {
            return Ok(false);
        }
    } else {
        return Ok(false);
    }
    let changed = sqlx::query!(
        "UPDATE native_live_broadcasts b SET dvr_window_seconds = $3, latency_mode = $4
         FROM statuses s WHERE b.status_id = $1 AND b.owner_account_id = $2
           AND s.id = b.status_id AND s.account_id = $2 AND s.uri IS NULL
           AND b.state = 'waiting'
           AND NOT EXISTS (SELECT 1 FROM native_live_sessions ls
                           WHERE ls.media_id = b.media_id AND ls.ended_at IS NULL)",
        status_id,
        owner,
        options.dvr_window_seconds,
        options.latency_mode,
    )
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if changed {
        sqlx::query!(
            "UPDATE statuses SET edited_at = now(), updated_at = now() WHERE id = $1",
            status_id,
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(changed)
}

/// Claim exactly one publisher for a published, non-ended broadcast. The
/// digest is compared in PostgreSQL; a guessed public UUID is never enough.
pub async fn claim(pool: &PgPool, key_hash: &[u8; 32]) -> Result<Option<ActiveSession>, DbError> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query!(
        "SELECT media_id, status_id, owner_account_id, permanent,
                dvr_window_seconds, latency_mode, key_generation FROM native_live_broadcasts
         WHERE ingest_key_hash = $1 AND status_id IS NOT NULL AND state <> 'ended'
         FOR UPDATE",
        key_hash.as_slice(),
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let Some(status_id) = row.status_id else {
        return Ok(None);
    };
    let session = activate(
        &mut tx,
        row.media_id,
        status_id,
        row.owner_account_id,
        row.permanent,
        PlaybackOptions {
            dvr_window_seconds: row.dvr_window_seconds,
            latency_mode: row.latency_mode,
        },
        Some(row.key_generation),
    )
    .await?;
    tx.commit().await?;
    Ok(session)
}

/// A cookie-authenticated, CSRF-checked browser can claim its own broadcast
/// without ever learning or sending the encoder's RTMP key.
pub async fn claim_owned(
    pool: &PgPool,
    status_id: i64,
    owner_account_id: i64,
) -> Result<Option<ActiveSession>, DbError> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query!(
        "SELECT b.media_id, b.status_id, b.owner_account_id, b.permanent,
                b.dvr_window_seconds, b.latency_mode
         FROM native_live_broadcasts b JOIN statuses s ON s.id = b.status_id
         WHERE b.status_id = $1 AND b.owner_account_id = $2
           AND s.uri IS NULL AND b.state <> 'ended' FOR UPDATE OF b",
        status_id,
        owner_account_id,
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let Some(status_id) = row.status_id else {
        return Ok(None);
    };
    let session = activate(
        &mut tx,
        row.media_id,
        status_id,
        row.owner_account_id,
        row.permanent,
        PlaybackOptions {
            dvr_window_seconds: row.dvr_window_seconds,
            latency_mode: row.latency_mode,
        },
        None,
    )
    .await?;
    tx.commit().await?;
    Ok(session)
}

async fn activate(
    tx: &mut Transaction<'_, Postgres>,
    media_id: i64,
    status_id: i64,
    owner_account_id: i64,
    permanent: bool,
    playback: PlaybackOptions,
    ingest_key_generation: Option<i64>,
) -> Result<Option<ActiveSession>, DbError> {
    if !crate::video_channel_access::lock_publication(tx, media_id, owner_account_id).await? {
        return Ok(None);
    }
    let generation = sqlx::query_scalar!(
        "SELECT COALESCE(max(generation), 0) + 1 FROM native_live_sessions WHERE media_id = $1",
        media_id,
    )
    .fetch_one(&mut **tx)
    .await?
    .unwrap_or(1);
    let publication_uuid = sqlx::query_scalar!(
        "SELECT uuid FROM video_publications WHERE media_id = $1 AND status_id = $2",
        media_id,
        status_id,
    )
    .fetch_optional(&mut **tx)
    .await?;
    let p2p_infohash = publication_uuid
        .as_deref()
        .map(|uuid| crate::native_video_hls::p2p_infohash(uuid, 0));
    let session_id = sqlx::query_scalar!(
        "INSERT INTO native_live_sessions (media_id, generation, p2p_infohash)
         VALUES ($1, $2, $3) ON CONFLICT DO NOTHING RETURNING session_id",
        media_id,
        generation,
        p2p_infohash,
    )
    .fetch_optional(&mut **tx)
    .await?;
    let Some(session_id) = session_id else {
        return Ok(None);
    };
    sqlx::query!(
        "UPDATE native_live_broadcasts SET state = 'live',
         scheduled_starts = CASE WHEN NOT permanent THEN '{}'::timestamptz[]
                                 WHEN state = 'waiting' THEN scheduled_starts[2:16]
                                 ELSE scheduled_starts END,
         scheduled_at = CASE WHEN NOT permanent THEN NULL
                             WHEN state = 'waiting' THEN scheduled_starts[2]
                             ELSE scheduled_at END,
         reconnect_deadline = NULL, started_at = COALESCE(started_at, now()) WHERE media_id = $1",
        media_id,
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query!(
        "UPDATE media_attachments SET live_state = 'live',
         hls_master_url = '/media/hls/' || $1::text || '/master.m3u8' WHERE id = $1",
        media_id,
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query!(
        "UPDATE statuses SET edited_at = now(), updated_at = now() WHERE id = $1",
        status_id,
    )
    .execute(&mut **tx)
    .await?;
    Ok(Some(ActiveSession {
        media_id,
        status_id,
        owner_account_id,
        session_id,
        permanent,
        playback,
        ingest_key_generation,
    }))
}

/// A rotated key revokes an already connected key-authenticated publisher.
/// The session fence also rejects a delayed poll from an older generation.
pub async fn ingest_key_current(pool: &PgPool, session: &ActiveSession) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM native_live_broadcasts b
         JOIN native_live_sessions ls ON ls.media_id = b.media_id
         JOIN video_channels c ON c.account_id=b.channel_id
         JOIN accounts ca ON ca.id=c.account_id JOIN accounts o ON o.id=c.owner_account_id
         JOIN accounts a ON a.id=b.owner_account_id
         WHERE b.media_id = $1 AND b.status_id = $2 AND b.state = 'live'
           AND ($3::bigint IS NULL OR b.key_generation=$3) AND ls.session_id = $4 AND ls.ended_at IS NULL
           AND ca.suspended_at IS NULL AND o.suspended_at IS NULL AND a.suspended_at IS NULL
           AND (c.owner_account_id=b.owner_account_id OR EXISTS(SELECT 1 FROM video_channel_grants g
             WHERE g.channel_id=c.account_id AND g.account_id=b.owner_account_id AND g.accepted)))",
    )
    .bind(session.media_id).bind(session.status_id).bind(session.ingest_key_generation).bind(&session.session_id)
    .fetch_one(pool)
    .await?)
}

/// The generation fence prevents a delayed disconnect from stopping a newer
/// reconnect. An unexpected loss gives either kind of broadcast one minute
/// to reclaim the same Video with a new session. A deliberate stop immediately
/// returns permanent broadcasts to waiting and ends one-shot broadcasts.
pub async fn finish(
    pool: &PgPool,
    session: &ActiveSession,
    reason: FinishReason,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let closed = sqlx::query!(
        "UPDATE native_live_sessions SET ended_at = now()
         WHERE session_id = $1 AND media_id = $2 AND ended_at IS NULL",
        session.session_id,
        session.media_id,
    )
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if closed {
        let interrupted = reason == FinishReason::Interrupted;
        let state = sqlx::query_scalar!(
            "UPDATE native_live_broadcasts SET
                 state = CASE WHEN $2 THEN 'interrupted' WHEN permanent THEN 'waiting' ELSE 'ended' END,
                 reconnect_deadline = CASE WHEN $2 THEN now() + interval '60 seconds' ELSE NULL END,
                 ended_at = CASE WHEN $2 OR permanent THEN NULL ELSE now() END
             WHERE media_id = $1 AND NOT EXISTS
                 (SELECT 1 FROM native_live_sessions WHERE media_id = $1 AND ended_at IS NULL)
             RETURNING state",
            session.media_id,
            interrupted,
        )
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(state) = state {
            sqlx::query!(
                "UPDATE media_attachments SET live_state = $2,
                 hls_master_url = NULL WHERE id = $1",
                session.media_id,
                state,
            )
            .execute(&mut *tx)
            .await?;
            sqlx::query!(
                "UPDATE statuses SET edited_at = now(), updated_at = now() WHERE id = $1",
                session.status_id,
            )
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    Ok(closed)
}

/// Reserve the exact finished replay bytes before moving them into public
/// storage. Cleanup jobs are a crash journal: a killed finalizer leaves its
/// uncommitted files private, scheduled for deletion after one hour, then
/// refunds the charge only after the cleanup worker confirms deletion.
#[allow(
    clippy::too_many_lines,
    reason = "quota, partial-journal recovery and new reservations share one transaction"
)]
pub async fn reserve_replay(
    pool: &PgPool,
    session: &ActiveSession,
    keys: &[String],
    sizes: &[i64],
    limits: &crate::video_storage::Limits,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    crate::video_storage::lock(&mut tx).await?;
    let eligible: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM native_live_sessions ls
         JOIN native_live_broadcasts b ON b.media_id = ls.media_id
         JOIN media_attachments m ON m.id = b.media_id
         LEFT JOIN media_attachments replay ON replay.id = ls.replay_media_id
         WHERE ls.session_id = $1 AND ls.media_id = $2 AND ls.ended_at IS NOT NULL
           AND m.status_id = $3 AND (ls.replay_media_id IS NULL OR replay.status_id IS NULL)
           AND (b.permanent OR (b.state = 'ended' AND m.file_name IS NULL
               AND ls.generation = (SELECT max(generation) FROM native_live_sessions WHERE media_id = $2))))",
    )
    .bind(&session.session_id)
    .bind(session.media_id)
    .bind(session.status_id)
    .fetch_one(&mut *tx)
    .await?;
    if !eligible {
        return Ok(false);
    }
    // A process may have died after charging exact bytes (or even after
    // writing one of the two files) but before the publication transaction.
    // Reuse that journal atomically; charging the same immutable keys twice
    // would either fail the unique constraint or strand a valid recording.
    let existing: Vec<(String, i64, i64, Option<i64>)> = sqlx::query_as(
        "SELECT file_name, account_id, media_id, reserved_bytes
         FROM video_storage_reservations WHERE file_name = ANY($1) FOR UPDATE",
    )
    .bind(keys)
    .fetch_all(&mut *tx)
    .await?;
    if !existing.is_empty() {
        // Cleanup acknowledges files independently. A crash can therefore
        // leave one exact charge/journal while the other key was removed. Only
        // reserve the missing bytes; never double-charge the surviving key.
        let compatible = existing.iter().all(|(stored, owner, media, bytes)| {
            keys.iter().zip(sizes).any(|(key, size)| {
                stored == key
                    && *owner == session.owner_account_id
                    && *media == session.media_id
                    && *bytes == Some(*size)
            })
        });
        if !compatible {
            return Ok(false);
        }
        let existing_keys: Vec<String> = existing.iter().map(|row| row.0.clone()).collect();
        let refreshed = sqlx::query(
            "UPDATE media_cleanup_jobs SET run_at = now() + interval '1 hour'
             WHERE file_name = ANY($1)",
        )
        .bind(&existing_keys)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if refreshed != u64::try_from(existing.len()).unwrap_or(u64::MAX) {
            return Ok(false);
        }
        let missing: Vec<(String, i64)> = keys
            .iter()
            .zip(sizes)
            .filter(|(key, _)| !existing_keys.contains(key))
            .map(|(key, size)| (key.clone(), *size))
            .collect();
        if !missing.is_empty() {
            let missing_keys: Vec<String> = missing.iter().map(|(key, _)| key.clone()).collect();
            let missing_sizes: Vec<i64> = missing.iter().map(|(_, size)| *size).collect();
            if !crate::video_storage::reserve_locked(
                &mut tx,
                session.owner_account_id,
                session.media_id,
                &missing_keys,
                &missing_sizes,
                limits,
            )
            .await?
            {
                return Ok(false);
            }
            let ids: Vec<i64> = missing.iter().map(|_| crate::id::next()).collect();
            sqlx::query(
                "INSERT INTO media_cleanup_jobs (id, file_name, run_at)
                 SELECT input.id, input.file_name, now() + interval '1 hour'
                 FROM unnest($1::bigint[], $2::text[]) AS input(id, file_name)",
            )
            .bind(ids)
            .bind(missing_keys)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        return Ok(true);
    }
    if !crate::video_storage::reserve_locked(
        &mut tx,
        session.owner_account_id,
        session.media_id,
        keys,
        sizes,
        limits,
    )
    .await?
    {
        return Ok(false);
    }
    let ids: Vec<i64> = keys.iter().map(|_| crate::id::next()).collect();
    sqlx::query(
        "INSERT INTO media_cleanup_jobs (id, file_name, run_at)
         SELECT input.id, input.file_name, now() + interval '1 hour'
         FROM unnest($1::bigint[], $2::text[]) AS input(id, file_name)",
    )
    .bind(ids)
    .bind(keys)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

pub struct Replay<'a> {
    pub file_name: &'a str,
    pub poster_name: &'a str,
    pub file_size: i64,
    pub poster_size: i64,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub poster_width: i32,
    pub poster_height: i32,
    pub duration: f64,
    pub frame_rate: Option<&'a str>,
    pub bitrate: Option<i64>,
    pub blurhash: Option<&'a str>,
}

/// Allocate the private attachment exactly once for a recurring episode.
/// Its storage keys remain inaccessible until the separate Video and outbox
/// commit together. A retry after a process crash reuses the same media id.
pub async fn prepare_recurring_replay(
    pool: &PgPool,
    session: &ActiveSession,
    replay: &Replay<'_>,
) -> Result<Option<i64>, DbError> {
    if !session.permanent {
        return Ok(None);
    }
    let mut tx = pool.begin().await?;
    let row: Option<Option<i64>> = sqlx::query_scalar(
        "SELECT ls.replay_media_id FROM native_live_sessions ls
         JOIN native_live_broadcasts b ON b.media_id = ls.media_id
         WHERE ls.session_id = $1 AND ls.media_id = $2 AND ls.ended_at IS NOT NULL
           AND b.permanent AND b.status_id = $3 AND b.owner_account_id = $4
         FOR UPDATE OF ls",
    )
    .bind(&session.session_id)
    .bind(session.media_id)
    .bind(session.status_id)
    .bind(session.owner_account_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(existing) = row else {
        return Ok(None);
    };
    let media_id = existing.unwrap_or_else(crate::id::next);
    if existing.is_none() {
        sqlx::query(
            "INSERT INTO media_attachments
             (id, account_id, file_name, small_file_name, content_type, kind,
              file_size, thumbnail_file_size, width, height, small_width,
              small_height, duration, frame_rate, bitrate, blurhash)
             VALUES ($1, $2, $3, $4, 'video/mp4', 'video', $5, $6, $7,
                     $8, $9, $10, $11, $12, $13, $14)",
        )
        .bind(media_id)
        .bind(session.owner_account_id)
        .bind(replay.file_name)
        .bind(replay.poster_name)
        .bind(replay.file_size)
        .bind(replay.poster_size)
        .bind(replay.width)
        .bind(replay.height)
        .bind(replay.poster_width)
        .bind(replay.poster_height)
        .bind(replay.duration)
        .bind(replay.frame_rate)
        .bind(replay.bitrate)
        .bind(replay.blurhash)
        .execute(&mut *tx)
        .await?;
        // Snapshot creator-selected terms and warnings with the independently
        // allocated episode. A retry must not produce an unlabelled recording.
        sqlx::query("INSERT INTO video_metadata(media_id,category,licence,spoiler_text,sensitive,licence_version_known)
            SELECT $1,vm.category,vm.licence,s.spoiler_text,s.sensitive,COALESCE(vm.licence_version_known,false)
            FROM statuses s LEFT JOIN video_metadata vm ON vm.media_id=$2 WHERE s.id=$3")
            .bind(media_id).bind(session.media_id).bind(session.status_id).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO video_distribution(media_id,download_enabled,embed_enabled) SELECT $1,download_enabled,embed_enabled FROM video_distribution WHERE media_id=$2")
            .bind(media_id).bind(session.media_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE native_live_sessions SET replay_media_id = $2 WHERE session_id = $1")
            .bind(&session.session_id)
            .bind(media_id)
            .execute(&mut *tx)
            .await?;
    } else {
        let valid: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM media_attachments WHERE id = $1
             AND status_id IS NULL AND account_id = $2 AND file_name = $3
             AND small_file_name = $4 AND file_size = $5 AND thumbnail_file_size = $6)",
        )
        .bind(media_id)
        .bind(session.owner_account_id)
        .bind(replay.file_name)
        .bind(replay.poster_name)
        .bind(replay.file_size)
        .bind(replay.poster_size)
        .fetch_one(&mut *tx)
        .await?;
        if !valid {
            return Ok(None);
        }
    }
    tx.commit().await?;
    Ok(Some(media_id))
}

/// Called within the same transaction as Create(Video) and its outbox jobs.
/// Without this claim, another publication path cannot consume an unpublished
/// episode or cancel the storage cleanup journal.
pub async fn bind_recurring_replay(
    conn: &mut PgConnection,
    session_id: &str,
    media_id: i64,
    status_id: i64,
    owner: i64,
    channel_id: i64,
) -> Result<bool, DbError> {
    let keys: Option<(String, String)> = sqlx::query_as(
        "SELECT m.file_name, m.small_file_name FROM native_live_sessions ls
         JOIN native_live_broadcasts b ON b.media_id = ls.media_id
         JOIN media_attachments m ON m.id = ls.replay_media_id
         WHERE ls.session_id = $1 AND ls.replay_media_id = $2
           AND ls.ended_at IS NOT NULL AND b.permanent
           AND b.owner_account_id = $4 AND b.channel_id = $5
           AND m.status_id = $3 AND m.account_id = $4
           AND EXISTS (SELECT 1 FROM video_publications v
                       WHERE v.status_id = $3 AND v.media_id = $2 AND v.channel_id = $5)",
    )
    .bind(session_id)
    .bind(media_id)
    .bind(status_id)
    .bind(owner)
    .bind(channel_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((file_name, poster_name)) = keys else {
        return Ok(false);
    };
    sqlx::query("DELETE FROM media_cleanup_jobs WHERE file_name = ANY($1)")
        .bind(vec![file_name, poster_name])
        .execute(conn)
        .await?;
    Ok(true)
}

pub struct ReplayPublication {
    pub username: String,
    pub channel_id: i64,
    pub title: String,
    pub text: String,
    pub visibility: String,
    pub language: Option<String>,
    pub content_type: String,
    pub media_id: i64,
    pub spoiler_text: String,
    pub sensitive: bool,
}

#[derive(sqlx::FromRow)]
struct ReplayPublicationRow {
    username: String,
    channel_id: i64,
    title: String,
    text: String,
    visibility: String,
    language: Option<String>,
    content_type: String,
    media_id: i64,
    generation: i64,
    spoiler_text: String,
    sensitive: bool,
}

pub async fn recurring_replay_publication(
    pool: &PgPool,
    session: &ActiveSession,
) -> Result<Option<ReplayPublication>, DbError> {
    let row: Option<ReplayPublicationRow> = sqlx::query_as(
        "SELECT a.username, b.channel_id, COALESCE(s.title, 'Live video') AS title,
                s.text, s.visibility, s.language, s.content_type,
                ls.replay_media_id AS media_id, ls.generation,
                COALESCE(NULLIF(s.spoiler_text,''),vm.spoiler_text,'') AS spoiler_text,
                (s.sensitive OR COALESCE(vm.sensitive,false)) AS sensitive
         FROM native_live_sessions ls
         JOIN native_live_broadcasts b ON b.media_id = ls.media_id
         JOIN statuses s ON s.id = b.status_id
         JOIN accounts a ON a.id = b.owner_account_id
         JOIN media_attachments m ON m.id = ls.replay_media_id
         LEFT JOIN video_metadata vm ON vm.media_id=m.id
         WHERE ls.session_id = $1 AND ls.media_id = $2 AND b.permanent
           AND ls.ended_at IS NOT NULL AND m.status_id IS NULL",
    )
    .bind(&session.session_id)
    .bind(session.media_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| ReplayPublication {
        username: row.username,
        channel_id: row.channel_id,
        title: format!(
            "{} — Replay {generation}",
            row.title.chars().take(95).collect::<String>(),
            generation = row.generation,
        ),
        text: row.text,
        visibility: row.visibility,
        language: row.language,
        content_type: row.content_type,
        media_id: row.media_id,
        spoiler_text: row.spoiler_text,
        sensitive: row.sensitive,
    }))
}

pub struct SessionReplay {
    pub generation: i64,
    pub ended_at: Option<OffsetDateTime>,
    pub status_id: Option<i64>,
}

pub async fn session_replays(pool: &PgPool, media_id: i64) -> Result<Vec<SessionReplay>, DbError> {
    Ok(sqlx::query_as!(
        SessionReplay,
        "SELECT ls.generation, ls.ended_at, m.status_id FROM native_live_sessions ls
         LEFT JOIN media_attachments m ON m.id = ls.replay_media_id
         WHERE ls.media_id = $1 ORDER BY ls.generation DESC LIMIT 30",
        media_id,
    )
    .fetch_all(pool)
    .await?)
}

/// Public thread-page navigation for a reusable live. Only published,
/// independently viewable public/unlisted Videos are linked; failed or
/// private allocations never become discoverable through the live page.
pub async fn published_replays_by_status(
    pool: &PgPool,
    status_id: i64,
) -> Result<Vec<(i64, i64)>, DbError> {
    Ok(sqlx::query_as(
        "SELECT ls.generation, s.id FROM native_live_broadcasts b
         JOIN native_live_sessions ls ON ls.media_id = b.media_id
         JOIN media_attachments m ON m.id = ls.replay_media_id
         JOIN statuses s ON s.id = m.status_id
         JOIN video_publications v ON v.status_id = s.id AND v.media_id = m.id
         JOIN accounts a ON a.id = m.account_id AND a.suspended_at IS NULL
         JOIN accounts c ON c.id = v.channel_id AND c.suspended_at IS NULL
         WHERE b.status_id = $1 AND b.permanent
           AND s.visibility IN ('public', 'unlisted')
         ORDER BY ls.generation DESC LIMIT 30",
    )
    .bind(status_id)
    .fetch_all(pool)
    .await?)
}

pub async fn contains_unpublished_replay(
    pool: &PgPool,
    media_ids: &[i64],
) -> Result<bool, DbError> {
    if media_ids.is_empty() {
        return Ok(false);
    }
    Ok(sqlx::query_scalar!(
        "SELECT EXISTS (SELECT 1 FROM native_live_sessions ls
         JOIN media_attachments m ON m.id = ls.replay_media_id
         WHERE ls.replay_media_id = ANY($1) AND m.status_id IS NULL) AS \"exists!\"",
        media_ids,
    )
    .fetch_one(pool)
    .await?)
}

/// The publication changes from a live Video to an ordinary VOD only after
/// both files exist. The session link and cancellation of crash cleanup jobs
/// commit in that same transaction, so a failed/uncertain commit cannot
/// publish a half-written replay or later delete a committed one.
pub async fn publish_replay(
    pool: &PgPool,
    session: &ActiveSession,
    replay: Replay<'_>,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let changed = sqlx::query(
        "UPDATE media_attachments m SET
             file_name = $3, small_file_name = $4, content_type = 'video/mp4',
             kind = 'video', file_size = $5, thumbnail_file_size = $6,
             width = $7, height = $8, small_width = $9, small_height = $10,
             duration = $11, frame_rate = $12, bitrate = $13, blurhash = $14,
             live_state = NULL, live_permanent = false, hls_master_url = NULL
         FROM native_live_broadcasts b, native_live_sessions ls
         WHERE m.id = $1 AND m.status_id = $2 AND m.file_name IS NULL
           AND b.media_id = m.id AND b.state = 'ended' AND NOT b.permanent
           AND ls.session_id = $15 AND ls.media_id = m.id AND ls.ended_at IS NOT NULL
           AND ls.generation = (SELECT max(generation) FROM native_live_sessions WHERE media_id = m.id)",
    )
    .bind(session.media_id)
    .bind(session.status_id)
    .bind(replay.file_name)
    .bind(replay.poster_name)
    .bind(replay.file_size)
    .bind(replay.poster_size)
    .bind(replay.width)
    .bind(replay.height)
    .bind(replay.poster_width)
    .bind(replay.poster_height)
    .bind(replay.duration)
    .bind(replay.frame_rate)
    .bind(replay.bitrate)
    .bind(replay.blurhash)
    .bind(&session.session_id)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if changed {
        sqlx::query("UPDATE native_live_sessions SET replay_media_id = $2 WHERE session_id = $1")
            .bind(&session.session_id)
            .bind(session.media_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM media_cleanup_jobs WHERE file_name = ANY($1)")
            .bind(vec![replay.file_name, replay.poster_name])
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE statuses SET edited_at = now(), updated_at = now() WHERE id = $1")
            .bind(session.status_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(changed)
}

/// A failed finalization can be reclaimed immediately by the ordinary media
/// cleanup worker. Successful publication removes these jobs atomically.
pub async fn abandon_replay(pool: &PgPool, keys: &[String]) -> Result<(), DbError> {
    sqlx::query("UPDATE media_cleanup_jobs SET run_at = now() WHERE file_name = ANY($1)")
        .bind(keys)
        .execute(pool)
        .await?;
    Ok(())
}

/// An interrupted one-shot can be saved after its reconnect deadline only
/// when this is still the latest generation. An actual reconnect keeps the
/// broadcast live and cannot accidentally publish the older partial file.
pub async fn replay_eligible(pool: &PgPool, session: &ActiveSession) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM native_live_sessions ls
         JOIN native_live_broadcasts b ON b.media_id = ls.media_id
         WHERE ls.session_id = $1 AND ls.media_id = $2 AND ls.ended_at IS NOT NULL
           AND (ls.replay_media_id IS NULL OR EXISTS
               (SELECT 1 FROM media_attachments m WHERE m.id = ls.replay_media_id AND m.status_id IS NULL))
           AND (b.permanent OR (b.state = 'ended'
               AND ls.generation = (SELECT max(generation) FROM native_live_sessions WHERE media_id = $2))))",
    )
    .bind(&session.session_id)
    .bind(session.media_id)
    .fetch_one(pool)
    .await?)
}

/// A process crash cannot leave a broadcast falsely on-air forever. This is
/// called before the single-instance publisher listener begins accepting.
pub async fn recover_orphaned(pool: &PgPool) -> Result<Vec<ActiveSession>, DbError> {
    let rows = sqlx::query!(
        r#"SELECT ls.media_id, b.status_id AS "status_id!", b.owner_account_id, b.permanent,
                  ls.session_id, b.dvr_window_seconds, b.latency_mode
           FROM native_live_sessions ls
           JOIN native_live_broadcasts b ON b.media_id = ls.media_id
           WHERE ls.ended_at IS NULL AND b.status_id IS NOT NULL"#,
    )
    .fetch_all(pool)
    .await?;
    let sessions: Vec<ActiveSession> = rows
        .into_iter()
        .map(|row| ActiveSession {
            media_id: row.media_id,
            status_id: row.status_id,
            owner_account_id: row.owner_account_id,
            permanent: row.permanent,
            session_id: row.session_id,
            playback: PlaybackOptions {
                dvr_window_seconds: row.dvr_window_seconds,
                latency_mode: row.latency_mode,
            },
            ingest_key_generation: None,
        })
        .collect();
    for row in &sessions {
        finish(pool, row, FinishReason::Interrupted).await?;
    }
    Ok(sessions)
}

/// Resolve a durable reconnect deadline. Row locks serialize this with a
/// concurrent claim; an already reconnected session cannot be expired.
pub async fn expire_interruptions(pool: &PgPool) -> Result<Vec<i64>, DbError> {
    let mut tx = pool.begin().await?;
    let rows = sqlx::query!(
        "UPDATE native_live_broadcasts b SET
             state = CASE WHEN b.permanent THEN 'waiting' ELSE 'ended' END,
             reconnect_deadline = NULL,
             ended_at = CASE WHEN b.permanent THEN NULL ELSE now() END
         WHERE b.state = 'interrupted' AND b.reconnect_deadline <= now()
           AND NOT EXISTS (SELECT 1 FROM native_live_sessions ls
                           WHERE ls.media_id = b.media_id AND ls.ended_at IS NULL)
         RETURNING b.media_id, b.status_id, b.state",
    )
    .fetch_all(&mut *tx)
    .await?;
    let mut status_ids = Vec::with_capacity(rows.len());
    for row in rows {
        sqlx::query!(
            "UPDATE media_attachments SET live_state = $2, hls_master_url = NULL WHERE id = $1",
            row.media_id,
            row.state,
        )
        .execute(&mut *tx)
        .await?;
        if let Some(status_id) = row.status_id {
            sqlx::query!(
                "UPDATE statuses SET edited_at = now(), updated_at = now() WHERE id = $1",
                status_id,
            )
            .execute(&mut *tx)
            .await?;
            status_ids.push(status_id);
        }
    }
    tx.commit().await?;
    Ok(status_ids)
}

/// Prepare a fileless, private waiting Video and its credential digest. The
/// returned row is not discoverable or playable until `bind_publication` is
/// called by the transactional Create(Video) path.
#[allow(
    clippy::too_many_arguments,
    reason = "the broadcast allocation identity and playback policy are explicit"
)]
pub async fn allocate(
    pool: &PgPool,
    media_id: i64,
    owner: i64,
    channel_id: i64,
    key_hash: &[u8; 32],
    permanent: bool,
    scheduled_starts: &[OffsetDateTime],
    playback: PlaybackOptions,
) -> Result<Option<Broadcast>, DbError> {
    if !playback.valid() {
        return Ok(None);
    }
    let scheduled_at = scheduled_starts.first().copied();
    let mut tx = pool.begin().await?;
    if !video::lock_owned_channel(&mut tx, channel_id, owner).await? {
        return Ok(None);
    }
    // Account row serialization limits simultaneous allocations even when
    // creation requests arrive on different HTTP connections.
    sqlx::query!("SELECT id FROM accounts WHERE id = $1 FOR UPDATE", owner)
        .fetch_one(&mut *tx)
        .await?;
    let count = sqlx::query_scalar!(
        "SELECT count(*) FROM native_live_broadcasts WHERE owner_account_id = $1 AND state <> 'ended'",
        owner
    )
    .fetch_one(&mut *tx)
    .await?
    .unwrap_or(0);
    if count >= 10 {
        return Ok(None);
    }
    sqlx::query!(
        "INSERT INTO media_attachments
         (id, account_id, content_type, kind, processing, live_state, live_permanent)
         VALUES ($1, $2, 'application/x-mpegURL', 'video', 'complete', 'waiting', $3)",
        media_id,
        owner,
        permanent,
    )
    .execute(&mut *tx)
    .await?;
    let row = sqlx::query_as!(
        Broadcast,
        r#"INSERT INTO native_live_broadcasts
            (media_id, owner_account_id, channel_id, ingest_key_hash, permanent, scheduled_at,
             scheduled_starts, dvr_window_seconds, latency_mode)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
           RETURNING media_id, status_id, owner_account_id, channel_id,
                     broadcast_uuid, state, permanent, scheduled_at, scheduled_starts,
                     reconnect_deadline, key_generation, dvr_window_seconds, latency_mode"#,
        media_id,
        owner,
        channel_id,
        key_hash.as_slice(),
        permanent,
        scheduled_at,
        scheduled_starts,
        playback.dvr_window_seconds,
        playback.latency_mode,
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(row))
}

/// Same transaction as status, attachment, publication and delivery jobs.
/// A pre-provisioned live cannot be published in another channel or by a
/// different owner, nor bound to two statuses after a retry.
pub async fn bind_publication(
    conn: &mut PgConnection,
    media_id: i64,
    status_id: i64,
    owner: i64,
    channel_id: i64,
) -> Result<bool, DbError> {
    Ok(sqlx::query!(
        "UPDATE native_live_broadcasts SET status_id = $2
         WHERE media_id = $1 AND status_id IS NULL
           AND owner_account_id = $3 AND channel_id = $4 AND state = 'waiting'",
        media_id,
        status_id,
        owner,
        channel_id,
    )
    .execute(conn)
    .await?
    .rows_affected()
        == 1)
}

/// Failed initial publication leaves no credential or private orphan. This
/// deletes nothing that another successful request managed to publish.
pub async fn discard_unpublished(pool: &PgPool, media_id: i64, owner: i64) -> Result<(), DbError> {
    sqlx::query!(
        "DELETE FROM media_attachments WHERE id = $1 AND account_id = $2 AND status_id IS NULL",
        media_id,
        owner,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn owned_by_status(
    pool: &PgPool,
    status_id: i64,
    owner: i64,
) -> Result<Option<Broadcast>, DbError> {
    if crate::video_channel_access::publication_author(pool, status_id, owner)
        .await?
        .is_none()
    {
        return Ok(None);
    }
    Ok(sqlx::query_as!(
        Broadcast,
        r#"SELECT b.media_id, b.status_id, b.owner_account_id, b.channel_id,
                  b.broadcast_uuid, b.state, b.permanent, b.scheduled_at, b.scheduled_starts,
                  b.reconnect_deadline, b.key_generation,
                  b.dvr_window_seconds, b.latency_mode
           FROM native_live_broadcasts b JOIN statuses s ON s.id = b.status_id
           WHERE b.status_id = $1 AND b.owner_account_id = $2
             AND s.account_id = $2 AND s.uri IS NULL"#,
        status_id,
        owner,
    )
    .fetch_optional(pool)
    .await?)
}

/// A new key invalidates the previous one immediately; only its digest is
/// stored. Callers show the plaintext just once in a private no-store page.
pub async fn rotate_key(
    pool: &PgPool,
    status_id: i64,
    owner: i64,
    key_hash: &[u8; 32],
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let media: Option<i64> =
        sqlx::query_scalar("SELECT media_id FROM video_publications WHERE status_id=$1")
            .bind(status_id)
            .fetch_optional(&mut *tx)
            .await?;
    if let Some(media) = media {
        if !crate::video_channel_access::lock_publication(&mut tx, media, owner).await? {
            return Ok(false);
        }
    } else {
        return Ok(false);
    }
    let changed = sqlx::query!(
        "UPDATE native_live_broadcasts b SET ingest_key_hash = $3,
            key_generation = b.key_generation + 1
         FROM statuses s WHERE b.status_id = $1 AND b.owner_account_id = $2
           AND s.id = b.status_id AND s.account_id = $2 AND s.uri IS NULL
           AND b.state <> 'ended'",
        status_id,
        owner,
        key_hash.as_slice(),
    )
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    tx.commit().await?;
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{account, id, media, status};

    #[sqlx::test]
    async fn recurring_replay_is_private_idempotent_and_independent_of_newer_session(pool: PgPool) {
        let owner = account::create_local(
            &pool,
            account::NewLocalAccount {
                username: "replayowner",
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap();
        let channel = account::create_local(
            &pool,
            account::NewLocalAccount {
                username: "replaychannel",
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap();
        video::create_channel(&mut pool.acquire().await.unwrap(), channel.id, owner.id)
            .await
            .unwrap();
        let live = status::create_local(
            &pool,
            status::NewLocalStatus::new(owner.id, "<p>Live</p>", "public", None),
        )
        .await
        .unwrap();
        let media_id = id::next();
        allocate(
            &pool,
            media_id,
            owner.id,
            channel.id,
            &[5; 32],
            true,
            &[],
            PlaybackOptions {
                dvr_window_seconds: 600,
                latency_mode: 1,
            },
        )
        .await
        .unwrap()
        .unwrap();
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("UPDATE media_attachments SET status_id = $2 WHERE id = $1")
            .bind(media_id)
            .bind(live.id)
            .execute(&mut *tx)
            .await
            .unwrap();
        video::publish(&mut tx, live.id, channel.id, media_id)
            .await
            .unwrap();
        assert!(
            bind_publication(&mut tx, media_id, live.id, owner.id, channel.id)
                .await
                .unwrap()
        );
        tx.commit().await.unwrap();

        let changed_options = PlaybackOptions {
            dvr_window_seconds: 300,
            latency_mode: 3,
        };
        assert!(
            !update_playback(&pool, live.id, channel.id, changed_options)
                .await
                .unwrap()
        );
        assert!(
            update_playback(&pool, live.id, owner.id, changed_options)
                .await
                .unwrap()
        );
        assert_eq!(
            details_for_media(&pool, &[media_id]).await.unwrap()[&media_id].playback,
            changed_options
        );
        let planned =
            OffsetDateTime::from_unix_timestamp(OffsetDateTime::now_utc().unix_timestamp() + 7200)
                .unwrap();
        let next_planned = planned + time::Duration::hours(2);
        assert!(
            !update_schedule(&pool, live.id, channel.id, &[planned])
                .await
                .unwrap()
        );
        assert!(
            update_schedule(&pool, live.id, owner.id, &[planned, next_planned])
                .await
                .unwrap()
        );
        let details = details_for_media(&pool, &[media_id]).await.unwrap();
        assert_eq!(details[&media_id].scheduled_at, Some(planned));
        assert_eq!(details[&media_id].scheduled_starts, [planned, next_planned]);
        assert_eq!(
            calendar_broadcast(&pool, live.id)
                .await
                .unwrap()
                .unwrap()
                .scheduled_starts,
            [planned, next_planned]
        );
        assert_eq!(
            channel_calendar_broadcasts(&pool, channel.id)
                .await
                .unwrap()
                .len(),
            2
        );
        sqlx::query("UPDATE statuses SET visibility = 'unlisted' WHERE id = $1")
            .bind(live.id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(calendar_broadcast(&pool, live.id).await.unwrap().is_some());
        assert!(
            channel_calendar_broadcasts(&pool, channel.id)
                .await
                .unwrap()
                .is_empty()
        );
        sqlx::query("UPDATE statuses SET visibility = 'private' WHERE id = $1")
            .bind(live.id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(calendar_broadcast(&pool, live.id).await.unwrap().is_none());
        sqlx::query("UPDATE statuses SET visibility = 'public' WHERE id = $1")
            .bind(live.id)
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query("UPDATE statuses SET title = 'Secret launch', sensitive = true WHERE id = $1")
            .bind(live.id)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            calendar_broadcast(&pool, live.id)
                .await
                .unwrap()
                .unwrap()
                .title,
            "Sensitive live broadcast"
        );
        assert_eq!(
            channel_calendar_broadcasts(&pool, channel.id)
                .await
                .unwrap()[0]
                .title,
            "Sensitive live broadcast"
        );
        sqlx::query(
            "UPDATE statuses SET sensitive = false, spoiler_text = 'Content warning' WHERE id = $1",
        )
        .bind(live.id)
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            calendar_broadcast(&pool, live.id)
                .await
                .unwrap()
                .unwrap()
                .title,
            "Sensitive live broadcast"
        );
        assert_eq!(
            channel_calendar_broadcasts(&pool, channel.id)
                .await
                .unwrap()[0]
                .title,
            "Sensitive live broadcast"
        );
        sqlx::query("UPDATE statuses SET spoiler_text = '' WHERE id = $1")
            .bind(live.id)
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query("UPDATE accounts SET suspended_at = now() WHERE id = $1")
            .bind(owner.id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(calendar_broadcast(&pool, live.id).await.unwrap().is_none());
        assert!(
            channel_calendar_broadcasts(&pool, channel.id)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            calendar_channel_name(&pool, channel.id)
                .await
                .unwrap()
                .is_none()
        );
        sqlx::query("UPDATE accounts SET suspended_at = NULL WHERE id = $1")
            .bind(owner.id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(calendar_broadcast(&pool, live.id).await.unwrap().is_some());
        assert_eq!(
            channel_calendar_broadcasts(&pool, channel.id)
                .await
                .unwrap()
                .len(),
            2
        );
        assert!(
            calendar_channel_name(&pool, channel.id)
                .await
                .unwrap()
                .is_some()
        );

        let first = claim_owned(&pool, live.id, owner.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.playback, changed_options);
        let details = details_for_media(&pool, &[media_id]).await.unwrap();
        assert_eq!(details[&media_id].scheduled_at, Some(next_planned));
        assert_eq!(details[&media_id].scheduled_starts, [next_planned]);
        assert!(
            !update_schedule(&pool, live.id, owner.id, &[planned])
                .await
                .unwrap()
        );
        assert!(
            !update_playback(
                &pool,
                live.id,
                owner.id,
                PlaybackOptions {
                    dvr_window_seconds: 600,
                    latency_mode: 1,
                }
            )
            .await
            .unwrap()
        );
        assert!(finish(&pool, &first, FinishReason::Stopped).await.unwrap());
        let second = claim_owned(&pool, live.id, owner.id)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(first.session_id, second.session_id);
        let details = details_for_media(&pool, &[media_id]).await.unwrap();
        assert_eq!(details[&media_id].scheduled_at, None);
        assert!(details[&media_id].scheduled_starts.is_empty());
        assert!(replay_eligible(&pool, &first).await.unwrap());
        assert!(
            recoverable_replays(&pool)
                .await
                .unwrap()
                .iter()
                .any(|row| row.session.session_id == first.session_id)
        );

        let file_name = format!("{media_id}.abc.livereplay.mp4");
        let poster_name = format!("{media_id}.abc.livereplay.small.jpg");
        let replay = Replay {
            file_name: &file_name,
            poster_name: &poster_name,
            file_size: 100,
            poster_size: 20,
            width: Some(320),
            height: Some(180),
            poster_width: 320,
            poster_height: 180,
            duration: 5.0,
            frame_rate: Some("15"),
            bitrate: Some(1000),
            blurhash: None,
        };
        let keys = vec![file_name.clone(), poster_name.clone()];
        let limits = crate::video_storage::Limits {
            per_account: 1000,
            global: 1000,
        };
        assert!(
            reserve_replay(&pool, &first, &keys, &[100, 20], &limits)
                .await
                .unwrap()
        );
        let removed_job: i64 =
            sqlx::query_scalar("SELECT id FROM media_cleanup_jobs WHERE file_name = $1")
                .bind(&poster_name)
                .fetch_one(&pool)
                .await
                .unwrap();
        crate::video_storage::complete_deleted(&pool, &[removed_job])
            .await
            .unwrap();
        assert!(
            reserve_replay(&pool, &first, &keys, &[100, 20], &limits)
                .await
                .unwrap()
        );
        let charged: i64 = sqlx::query_scalar(
            "SELECT sum(reserved_bytes)::bigint FROM video_storage_reservations",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(charged, 120);
        let replay_id = prepare_recurring_replay(&pool, &first, &replay)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            prepare_recurring_replay(&pool, &first, &replay)
                .await
                .unwrap(),
            Some(replay_id)
        );
        assert!(
            contains_unpublished_replay(&pool, &[replay_id])
                .await
                .unwrap()
        );
        assert!(
            media::public_access_blocked(&pool, replay_id)
                .await
                .unwrap()
        );
        assert!(!media::public_file_allowed(&pool, &file_name).await.unwrap());
        assert!(
            !media::public_file_allowed(&pool, &poster_name)
                .await
                .unwrap()
        );

        let vod = status::create_local(
            &pool,
            status::NewLocalStatus::new(owner.id, "<p>Replay</p>", "public", None),
        )
        .await
        .unwrap();
        let mut tx = pool.begin().await.unwrap();
        assert!(
            video::lock_publishable_media(
                &mut tx,
                replay_id,
                owner.id,
                channel.id,
                Some(&first.session_id),
                None,
            )
            .await
            .unwrap()
        );
        sqlx::query("UPDATE media_attachments SET status_id = $2 WHERE id = $1")
            .bind(replay_id)
            .bind(vod.id)
            .execute(&mut *tx)
            .await
            .unwrap();
        video::publish(&mut tx, vod.id, channel.id, replay_id)
            .await
            .unwrap();
        assert!(
            bind_recurring_replay(
                &mut tx,
                &first.session_id,
                replay_id,
                vod.id,
                owner.id,
                channel.id,
            )
            .await
            .unwrap()
        );
        tx.commit().await.unwrap();
        let cleanup_jobs: i64 =
            sqlx::query_scalar("SELECT count(*) FROM media_cleanup_jobs WHERE file_name = ANY($1)")
                .bind(&keys)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(cleanup_jobs, 0);
        assert!(
            !contains_unpublished_replay(&pool, &[replay_id])
                .await
                .unwrap()
        );
        assert!(
            !media::public_access_blocked(&pool, replay_id)
                .await
                .unwrap()
        );
        assert!(media::public_file_allowed(&pool, &file_name).await.unwrap());
        assert!(
            media::public_file_allowed(&pool, &poster_name)
                .await
                .unwrap()
        );
        assert!(
            !recoverable_replays(&pool)
                .await
                .unwrap()
                .iter()
                .any(|row| row.session.session_id == first.session_id)
        );
        assert_eq!(
            session_replays(&pool, media_id).await.unwrap()[1].status_id,
            Some(vod.id)
        );
        assert_eq!(
            published_replays_by_status(&pool, live.id).await.unwrap(),
            vec![(1, vod.id)]
        );
    }
}
