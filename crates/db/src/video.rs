//! Native video ownership. Mutations share the caller's outbox transaction.

use sqlx::{PgConnection, PgExecutor};

use crate::DbError;

/// Native or independently verified imported video-channel identity. Ordinary
/// community Groups keep their announce-based profile semantics.
pub async fn is_channel(pool: &sqlx::PgPool, account_id: i64) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM accounts a WHERE a.id = $1 AND a.actor_type = 'Group'
               AND (EXISTS (SELECT 1 FROM video_channels c WHERE c.account_id = a.id)
                    OR EXISTS (SELECT 1 FROM remote_video_channels r WHERE r.channel_id = a.id))
           ) AS "channel!""#,
        account_id,
    )
    .fetch_one(pool)
    .await?)
}

/// A public Video and the actor a viewer subscribes to for future videos.
/// Native publications and verified remote Videos use their channel; other
/// remote Videos fall back to their author.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DiscoveryHit {
    pub status_id: i64,
    pub channel_id: i64,
}

/// A bounded page can carry a scan cursor even when every match was hidden.
/// The cursor never carries those posts' titles or media URLs.
pub struct DiscoveryPage {
    pub hits: Vec<DiscoveryHit>,
    pub scan_before: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct DiscoveryRow {
    status_id: Option<i64>,
    channel_id: Option<i64>,
    scan_before: Option<i64>,
    scanned: i64,
}

fn window(rows: Vec<DiscoveryRow>) -> DiscoveryPage {
    let scan_before = rows
        .first()
        .and_then(|row| (row.scanned == 1000).then_some(row.scan_before).flatten());
    let hits = rows
        .into_iter()
        .filter_map(|row| {
            Some(DiscoveryHit {
                status_id: row.status_id?,
                channel_id: row.channel_id?,
            })
        })
        .collect();
    DiscoveryPage { hits, scan_before }
}

/// Verified publication context for a status batch. Drafts, deleted/private
/// statuses and unavailable actors never contribute a public attribution.
/// This is not membership or community voting evidence.
pub async fn publication_contexts_of(
    pool: &sqlx::PgPool,
    status_ids: &[i64],
    viewers: &[Option<i64>],
) -> Result<Vec<(Option<i64>, i64, i64)>, DbError> {
    Ok(sqlx::query_as(
        r"SELECT viewer.id, s.id, c.id FROM statuses s
        CROSS JOIN unnest($2::bigint[]) AS viewer(id)
        JOIN (
            SELECT status_id, channel_id FROM video_publications WHERE status_id=ANY($1)
            UNION SELECT status_id, channel_id FROM remote_video_channels WHERE status_id=ANY($1)
        ) p ON p.status_id=s.id
        JOIN accounts c ON c.id=p.channel_id
        LEFT JOIN video_channels vc ON vc.account_id=c.id
        LEFT JOIN accounts owner ON owner.id=vc.owner_account_id
        WHERE s.id=ANY($1) AND s.object_type='Video' AND s.reblog_of_id IS NULL
          AND s.visibility IN ('public','unlisted') AND s.deleted_at IS NULL -- STUBFILTER: exclude soft-deleted thread stubs
          AND NOT account_hidden(viewer.id,s.account_id)
          AND NOT account_hidden(viewer.id,c.id)
          AND (vc.owner_account_id IS NULL OR NOT account_hidden(viewer.id,vc.owner_account_id))
          AND c.actor_type='Group' AND c.deleted_at IS NULL AND c.suspended_at IS NULL
          AND (vc.account_id IS NULL OR (owner.deleted_at IS NULL AND owner.suspended_at IS NULL))
        ORDER BY s.id,c.id",
    )
    .bind(status_ids)
    .bind(viewers)
    .fetch_all(pool)
    .await?)
}

/// At most four 1,000-match windows per request, never an unbounded scan of
/// filtered spam. HTML Older navigation can advance even across an empty page.
pub async fn discovery_page(
    pool: &sqlx::PgPool,
    channel: Option<i64>,
    query: Option<&str>,
    viewer: Option<i64>,
    followed: bool,
    mut before: Option<i64>,
    limit: i64,
) -> Result<DiscoveryPage, DbError> {
    let limit = limit.clamp(1, 100);
    let mut hits = Vec::new();
    for _ in 0..4 {
        let remaining = limit - i64::try_from(hits.len()).unwrap_or(limit);
        let page = if let Some(channel) = channel {
            channel_window(pool, channel, query, viewer, before, remaining).await?
        } else {
            catalogue_window(pool, query, viewer, followed, before, remaining).await?
        };
        hits.extend(page.hits);
        if i64::try_from(hits.len()).unwrap_or(limit) >= limit || page.scan_before.is_none() {
            return Ok(DiscoveryPage {
                hits,
                scan_before: page.scan_before,
            });
        }
        before = page.scan_before;
    }
    Ok(DiscoveryPage {
        hits,
        scan_before: before,
    })
}

pub async fn discover(
    pool: &sqlx::PgPool,
    query: Option<&str>,
    viewer: Option<i64>,
    followed: bool,
    before: Option<i64>,
    limit: i64,
) -> Result<Vec<DiscoveryHit>, DbError> {
    Ok(
        discovery_page(pool, None, query, viewer, followed, before, limit)
            .await?
            .hits,
    )
}

async fn catalogue_window(
    pool: &sqlx::PgPool,
    query: Option<&str>,
    viewer: Option<i64>,
    followed: bool,
    before: Option<i64>,
    limit: i64,
) -> Result<DiscoveryPage, DbError> {
    Ok(window(
        sqlx::query_as!(
            DiscoveryRow,
            r#"WITH candidates AS MATERIALIZED (
            SELECT s.id, s.account_id,
                   COALESCE(v.channel_id, r.channel_id, s.account_id) AS channel_id
            FROM statuses s
            LEFT JOIN video_publications v ON v.status_id = s.id
            LEFT JOIN remote_video_channels r ON r.status_id = s.id
            WHERE s.object_type = 'Video' AND s.visibility = 'public'
              AND s.reblog_of_id IS NULL AND s.deleted_at IS NULL
              AND s.ingest_provenance <> 'history'
              AND ($1::bigint IS NULL OR s.id < $1)
              AND ($2::text IS NULL OR
                   to_tsvector('simple', coalesce(s.title, '') || ' ' || s.content)
                     @@ websearch_to_tsquery('simple', $2))
              AND EXISTS (SELECT 1 FROM media_attachments m
                          WHERE m.status_id = s.id
                            AND (m.kind IN ('video', 'gifv') OR m.content_type LIKE 'video/%')
                            AND (m.processing = 'complete' OR m.download_on_demand))
              AND (NOT $4::boolean OR EXISTS (
                   SELECT 1 FROM follows f WHERE f.account_id = $3
                     AND f.target_account_id = COALESCE(v.channel_id, r.channel_id, s.account_id)
                     AND NOT f.pending))
            ORDER BY s.id DESC LIMIT 1000
        )
        , visible AS (
            SELECT c.id, c.channel_id FROM candidates c
            LEFT JOIN video_channels vc ON vc.account_id=c.channel_id
            WHERE NOT account_hidden($3, c.account_id)
              AND NOT account_hidden($3, c.channel_id)
              AND (vc.owner_account_id IS NULL OR NOT account_hidden($3, vc.owner_account_id))
        ), scan AS (SELECT min(id) AS scan_before,count(*) AS scanned FROM candidates)
        SELECT v.id AS "status_id?", v.channel_id AS "channel_id?",
            scan.scan_before AS "scan_before?",scan.scanned AS "scanned!"
        FROM scan LEFT JOIN visible v ON true
        ORDER BY v.id DESC NULLS LAST LIMIT $5
-- STUBFILTER: exclude deleted statuses directly or through public_native_videos
"#,
            before,
            query,
            viewer,
            followed,
            limit,
        )
        .fetch_all(pool)
        .await?,
    ))
}

/// Whether an actor may be shown as a public video library. Native channels
/// also depend on their owner; a suspended/blocked owner must not leave an
/// otherwise discoverable channel page behind.
pub async fn library_visible(
    pool: &sqlx::PgPool,
    channel_id: i64,
    viewer: Option<i64>,
) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM accounts a
             LEFT JOIN video_channels v ON v.account_id = a.id
             WHERE a.id = $1 AND NOT account_hidden($2, a.id)
               AND (v.owner_account_id IS NULL OR NOT account_hidden($2, v.owner_account_id))
           ) AS "visible!""#,
        channel_id,
        viewer,
    )
    .fetch_one(pool)
    .await?)
}

/// A single channel/creator's public Video library, indexed by
/// `video_publications(channel_id, status_id)` for native publications,
/// `remote_video_channels(channel_id, status_id)` for verified imported
/// channels and `statuses(account_id, id)` for author-only imported Videos.
/// Both branches filter before their bounded window, so search and deep
/// keyset pages cannot disappear behind unrelated global catalogue entries.
pub async fn discover_channel(
    pool: &sqlx::PgPool,
    channel_id: i64,
    query: Option<&str>,
    viewer: Option<i64>,
    before: Option<i64>,
    limit: i64,
) -> Result<Vec<DiscoveryHit>, DbError> {
    Ok(
        discovery_page(pool, Some(channel_id), query, viewer, false, before, limit)
            .await?
            .hits,
    )
}

async fn channel_window(
    pool: &sqlx::PgPool,
    channel_id: i64,
    query: Option<&str>,
    viewer: Option<i64>,
    before: Option<i64>,
    limit: i64,
) -> Result<DiscoveryPage, DbError> {
    Ok(window(
        sqlx::query_as!(
            DiscoveryRow,
            r#"WITH candidates AS MATERIALIZED (
            SELECT scoped.id, scoped.account_id, scoped.channel_id FROM (
                (SELECT s.id, s.account_id, v.channel_id
                 FROM video_publications v JOIN statuses s ON s.id = v.status_id
                 WHERE v.channel_id = $1
                   AND s.object_type = 'Video' AND s.visibility = 'public'
                   AND s.reblog_of_id IS NULL AND s.deleted_at IS NULL
                   AND s.ingest_provenance <> 'history'
                   AND ($2::bigint IS NULL OR s.id < $2)
                   AND ($3::text IS NULL OR
                        to_tsvector('simple', coalesce(s.title, '') || ' ' || s.content)
                          @@ websearch_to_tsquery('simple', $3))
                   AND EXISTS (SELECT 1 FROM media_attachments m
                               WHERE m.status_id = s.id
                                 AND (m.kind IN ('video', 'gifv') OR m.content_type LIKE 'video/%')
                                 AND (m.processing = 'complete' OR m.download_on_demand))
                ORDER BY v.status_id DESC LIMIT 1000)
                UNION ALL
                (SELECT s.id, s.account_id, r.channel_id
                 FROM remote_video_channels r JOIN statuses s ON s.id = r.status_id
                 WHERE r.channel_id = $1
                   AND s.object_type = 'Video' AND s.visibility = 'public'
                   AND s.reblog_of_id IS NULL AND s.deleted_at IS NULL
                   AND s.ingest_provenance <> 'history'
                   AND ($2::bigint IS NULL OR s.id < $2)
                   AND ($3::text IS NULL OR
                        to_tsvector('simple', coalesce(s.title, '') || ' ' || s.content)
                          @@ websearch_to_tsquery('simple', $3))
                   AND EXISTS (SELECT 1 FROM media_attachments m
                               WHERE m.status_id = s.id
                                 AND (m.kind IN ('video', 'gifv') OR m.content_type LIKE 'video/%')
                                 AND (m.processing = 'complete' OR m.download_on_demand))
                 ORDER BY r.status_id DESC LIMIT 1000)
                UNION ALL
                (SELECT s.id, s.account_id, s.account_id AS channel_id
                 FROM statuses s
                 WHERE s.account_id = $1
                   AND NOT EXISTS (SELECT 1 FROM video_publications v WHERE v.status_id = s.id)
                   AND NOT EXISTS (SELECT 1 FROM remote_video_channels r WHERE r.status_id = s.id)
                   AND s.object_type = 'Video' AND s.visibility = 'public'
                   AND s.reblog_of_id IS NULL AND s.deleted_at IS NULL
                   AND s.ingest_provenance <> 'history'
                   AND ($2::bigint IS NULL OR s.id < $2)
                   AND ($3::text IS NULL OR
                        to_tsvector('simple', coalesce(s.title, '') || ' ' || s.content)
                          @@ websearch_to_tsquery('simple', $3))
                   AND EXISTS (SELECT 1 FROM media_attachments m
                               WHERE m.status_id = s.id
                                 AND (m.kind IN ('video', 'gifv') OR m.content_type LIKE 'video/%')
                                 AND (m.processing = 'complete' OR m.download_on_demand))
                 ORDER BY s.id DESC LIMIT 1000)
            ) scoped ORDER BY scoped.id DESC LIMIT 1000
        )
        , visible AS (
            SELECT c.id, c.channel_id FROM candidates c
            LEFT JOIN video_channels vc ON vc.account_id=c.channel_id
            WHERE NOT account_hidden($4, c.account_id)
              AND NOT account_hidden($4, c.channel_id)
              AND (vc.owner_account_id IS NULL OR NOT account_hidden($4, vc.owner_account_id))
        ), scan AS (SELECT min(id) AS scan_before,count(*) AS scanned FROM candidates)
        SELECT v.id AS "status_id?", v.channel_id AS "channel_id?",
            scan.scan_before AS "scan_before?",scan.scanned AS "scanned!"
        FROM scan LEFT JOIN visible v ON true
        ORDER BY v.id DESC NULLS LAST LIMIT $5
-- STUBFILTER: exclude deleted statuses directly or through public_native_videos
"#,
            channel_id,
            before,
            query,
            viewer,
            limit,
        )
        .fetch_all(pool)
        .await?,
    ))
}

/// Reconciles the verified channel of an imported Video. An Update can move a
/// Video between channels; no attribution leaves it in the author's library.
pub async fn set_remote_channel(
    pool: &sqlx::PgPool,
    status_id: i64,
    channel_id: Option<i64>,
) -> Result<(), DbError> {
    if let Some(channel_id) = channel_id {
        sqlx::query(
            "INSERT INTO remote_video_channels (status_id, channel_id) VALUES ($1, $2) \
             ON CONFLICT (status_id) DO UPDATE SET channel_id = EXCLUDED.channel_id",
        )
        .bind(status_id)
        .bind(channel_id)
        .execute(pool)
        .await?;
    } else {
        sqlx::query("DELETE FROM remote_video_channels WHERE status_id = $1")
            .bind(status_id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RemoteChannelBackfill {
    pub status_id: i64,
    pub uri: String,
    pub account_id: i64,
    pub attempts: i32,
}

/// Lease at most one older public remote Video for channel verification. The
/// catalogue's partial newest index bounds the ordered scan; leases survive a
/// worker crash and let another process retry after five minutes.
pub async fn claim_remote_channel_backfill(
    pool: &sqlx::PgPool,
) -> Result<Option<RemoteChannelBackfill>, DbError> {
    Ok(sqlx::query_as!(
        RemoteChannelBackfill,
        r#"WITH candidates AS MATERIALIZED (
            SELECT s.id, s.uri, s.account_id
            FROM statuses s
            JOIN accounts a ON a.id = s.account_id
            LEFT JOIN remote_video_channels r ON r.status_id = s.id
            LEFT JOIN remote_video_channel_checks c ON c.status_id = s.id
            WHERE s.object_type = 'Video' AND s.visibility = 'public'
              AND s.reblog_of_id IS NULL AND s.deleted_at IS NULL
              AND s.ingest_provenance <> 'history'
              AND s.uri IS NOT NULL AND a.domain IS NOT NULL
              AND r.status_id IS NULL
              AND (c.status_id IS NULL OR c.retry_at <= now())
            ORDER BY s.id DESC LIMIT 1 FOR UPDATE OF s SKIP LOCKED
        ), claimed AS (
            INSERT INTO remote_video_channel_checks (status_id, retry_at)
            SELECT id, now() + interval '5 minutes' FROM candidates
            ON CONFLICT (status_id) DO UPDATE SET retry_at = EXCLUDED.retry_at
              WHERE remote_video_channel_checks.retry_at <= now()
            RETURNING status_id, attempts
        )
        SELECT c.id AS "status_id!", c.uri AS "uri!", c.account_id AS "account_id!",
               claimed.attempts AS "attempts!"
        FROM candidates c JOIN claimed ON claimed.status_id = c.id
-- STUBFILTER: exclude deleted statuses directly or through public_native_videos
"#,
    )
    .fetch_optional(pool)
    .await?)
}

/// A conclusive origin check suppresses future refetches. A later signed
/// Video Update can still reconcile the channel in the ordinary ingest path.
pub async fn complete_remote_channel_check(
    pool: &sqlx::PgPool,
    status_id: i64,
) -> Result<(), DbError> {
    sqlx::query(
        "INSERT INTO remote_video_channel_checks (status_id, retry_at, checked_at) \
         SELECT id, NULL, now() FROM statuses WHERE id = $1 \
         ON CONFLICT (status_id) DO UPDATE \
         SET retry_at = NULL, checked_at = now()",
    )
    .bind(status_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Retry origin or Group-fetch failures with backoff; the claim's lease is
/// overwritten only by the worker that finished the attempt.
pub async fn retry_remote_channel_check(
    pool: &sqlx::PgPool,
    status_id: i64,
    delay: std::time::Duration,
) -> Result<(), DbError> {
    sqlx::query(
        "UPDATE remote_video_channel_checks \
         SET retry_at = now() + make_interval(secs => $2), attempts = attempts + 1 \
         WHERE status_id = $1 AND retry_at IS NOT NULL",
    )
    .bind(status_id)
    .bind(delay.as_secs_f64())
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Channel {
    pub account_id: i64,
    pub owner_account_id: i64,
}

#[derive(Debug, Clone)]
pub struct Publication {
    pub status_id: i64,
    pub channel_id: i64,
    pub media_id: i64,
    pub uuid: String,
    pub channel_uri: String,
    pub file_size: i64,
    pub poster_file_name: Option<String>,
    pub poster_width: Option<i32>,
    pub poster_height: Option<i32>,
    pub poster_alt: Option<String>,
    pub live_dvr_window_seconds: Option<i32>,
    pub live_latency_mode: Option<i32>,
    pub live_scheduled_at: Option<time::OffsetDateTime>,
    pub live_scheduled_starts: Option<Vec<time::OffsetDateTime>>,
}

pub async fn create_channel(
    conn: &mut PgConnection,
    account_id: i64,
    owner_account_id: i64,
) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE accounts SET actor_type = 'Group' WHERE id = $1",
        account_id
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "INSERT INTO video_channels (account_id, owner_account_id) VALUES ($1, $2)",
        account_id,
        owner_account_id,
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

pub async fn channel<'e, E: PgExecutor<'e>>(
    conn: E,
    account_id: i64,
) -> Result<Option<Channel>, DbError> {
    Ok(sqlx::query_as!(
        Channel,
        "SELECT account_id, owner_account_id FROM video_channels WHERE account_id = $1",
        account_id,
    )
    .fetch_optional(conn)
    .await?)
}

pub async fn owned_channels<'e, E: PgExecutor<'e>>(
    conn: E,
    owner: i64,
) -> Result<Vec<Channel>, DbError> {
    Ok(sqlx::query_as!(Channel,
        "SELECT c.account_id, c.owner_account_id FROM video_channels c JOIN accounts a ON a.id=c.account_id WHERE c.owner_account_id=$1 AND a.deleted_at IS NULL ORDER BY c.account_id",
        owner,
    ).fetch_all(conn).await?)
}

/// Lock ownership while publishing so a later ownership-transfer API can use
/// the same row lock and cannot race a publication's permission check.
pub async fn lock_owned_channel(
    conn: &mut PgConnection,
    channel_id: i64,
    owner: i64,
) -> Result<bool, DbError> {
    Ok(
        crate::video_channel_access::lock_access(conn, channel_id, owner)
            .await?
            .is_some(),
    )
}

pub async fn publish(
    conn: &mut PgConnection,
    status_id: i64,
    channel_id: i64,
    media_id: i64,
) -> Result<(), DbError> {
    sqlx::query!(
        // Explicit film authoring overrides the ordinary attachment endpoint's
        // short-silent-clip heuristic. Client API films must not autoplay/loop.
        "WITH film AS (
             UPDATE media_attachments SET kind = 'video'
             WHERE id = $3 AND status_id = $1 RETURNING id
         )
         INSERT INTO video_publications (status_id, channel_id, media_id)
         SELECT $1, $2, id FROM film",
        status_id,
        channel_id,
        media_id,
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// Hold the media row through publication, fencing draft cancellation/expiry.
/// A durable draft is publishable only in its selected channel while ready and
/// unexpired. Ordinary completed client-API attachments remain supported.
pub async fn lock_publishable_media(
    conn: &mut PgConnection,
    media_id: i64,
    owner: i64,
    channel_id: i64,
    replay_session_id: Option<&str>,
    scheduled_status_id: Option<i64>,
) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar!(
        "SELECT m.id FROM media_attachments m LEFT JOIN video_drafts d ON d.media_id = m.id
         WHERE m.id = $1 AND m.account_id = $2 AND m.status_id IS NULL AND m.processing = 'complete'
           AND m.scheduled_status_id IS NOT DISTINCT FROM $5
           AND (d.media_id IS NULL OR (d.account_id = $2 AND d.channel_id = $3
                AND d.state = 'ready' AND d.expires_at > clock_timestamp()))
           AND (NOT EXISTS (SELECT 1 FROM native_live_sessions ls WHERE ls.replay_media_id = m.id)
                OR EXISTS (SELECT 1 FROM native_live_sessions ls
                    JOIN native_live_broadcasts b ON b.media_id = ls.media_id
                    WHERE ls.replay_media_id = m.id AND ls.session_id = $4
                      AND ls.ended_at IS NOT NULL AND b.permanent
                      AND b.owner_account_id = $2 AND b.channel_id = $3))
         FOR UPDATE OF m",
        media_id,
        owner,
        channel_id,
        replay_session_id,
        scheduled_status_id,
    )
    .fetch_optional(conn)
    .await?
    .is_some())
}

/// A channel's local timeline entry represents the same Announce sent to peers.
/// Copy the source audience: promoting an unlisted film to a public boost would
/// leak it into discovery. This is not a community submission or moderator vote.
pub async fn create_announcement(
    conn: &mut PgConnection,
    channel_id: i64,
    status_id: i64,
    uri: &str,
) -> Result<i64, DbError> {
    Ok(sqlx::query_scalar!(
        r#"INSERT INTO statuses (id, uri, account_id, content, visibility, reblog_of_id,
                                sensitive, language, created_at, sort_at)
           SELECT $1, $2, $3, '', s.visibility, s.id, s.sensitive, s.language,
                  s.created_at, s.sort_at
           FROM statuses s WHERE s.id = $4 AND s.visibility IN ('public', 'unlisted')
           ON CONFLICT (account_id, reblog_of_id) WHERE reblog_of_id IS NOT NULL
           DO UPDATE SET uri = EXCLUDED.uri
           RETURNING id"#,
        crate::id::next(),
        uri,
        channel_id,
        status_id,
    )
    .fetch_one(conn)
    .await?)
}

pub async fn publications<'e, E: PgExecutor<'e>>(
    conn: E,
    ids: &[i64],
) -> Result<Vec<Publication>, DbError> {
    Ok(sqlx::query_as!(
        Publication,
        r#"SELECT v.status_id, v.channel_id, v.media_id, v.uuid,
                  a.uri AS "channel_uri!", COALESCE(m.file_size, 0) AS "file_size!",
                  p.file_name AS "poster_file_name?", p.width AS "poster_width?", p.height AS "poster_height?",
                  d.poster_alt AS "poster_alt?",
                  b.dvr_window_seconds AS "live_dvr_window_seconds?",
                  b.latency_mode AS "live_latency_mode?",
                  b.scheduled_at AS "live_scheduled_at?",
                  b.scheduled_starts AS "live_scheduled_starts?"
           FROM video_publications v JOIN accounts a ON a.id = v.channel_id
           JOIN media_attachments m ON m.id = v.media_id
           LEFT JOIN video_compatibility_posters p ON p.media_id = v.media_id
           LEFT JOIN video_drafts d ON d.media_id = v.media_id
           LEFT JOIN native_live_broadcasts b ON b.media_id = v.media_id
           WHERE v.status_id = ANY($1)"#,
        ids,
    )
    .fetch_all(conn)
    .await?)
}

#[derive(Debug)]
pub struct OutboxEntry {
    pub status_id: i64,
    pub uuid: String,
    pub status_uri: Option<String>,
    pub author_uri: Option<String>,
    pub username: String,
    pub created_at: time::OffsetDateTime,
    pub visibility: String,
}

/// Public channel history; moderation and both owner/channel relationships
/// apply before pagination. Future restricted publications must not leak here.
pub async fn outbox_page(
    pool: &sqlx::PgPool,
    channel_id: i64,
    viewer: Option<i64>,
    before: Option<i64>,
    limit: i64,
) -> Result<Vec<OutboxEntry>, DbError> {
    Ok(sqlx::query_as!(
        OutboxEntry,
        r#"SELECT v.status_id, v.uuid, s.uri AS status_uri, a.uri AS author_uri,
                  a.username, s.created_at, s.visibility
           FROM video_publications v JOIN statuses s ON s.id = v.status_id
           JOIN accounts a ON a.id = s.account_id
           WHERE v.channel_id = $1 AND s.visibility IN ('public', 'unlisted')
             AND s.deleted_at IS NULL -- STUBFILTER
             AND NOT account_hidden($2, s.account_id) AND NOT account_hidden($2, v.channel_id)
             AND ($3::bigint IS NULL OR s.id < $3)
           ORDER BY s.id DESC LIMIT $4"#,
        channel_id,
        viewer,
        before,
        limit,
    )
    .fetch_all(pool)
    .await?)
}

pub async fn outbox_count(
    pool: &sqlx::PgPool,
    channel_id: i64,
    viewer: Option<i64>,
) -> Result<i64, DbError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM video_publications v JOIN statuses s ON s.id = v.status_id
           WHERE v.channel_id = $1 AND s.visibility IN ('public', 'unlisted')
             AND s.deleted_at IS NULL -- STUBFILTER
             AND NOT account_hidden($2, s.account_id) AND NOT account_hidden($2, v.channel_id)"#,
        channel_id, viewer,
    ).fetch_one(pool).await?)
}

#[cfg(test)]
mod discovery_tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};
    use crate::follow;
    use crate::status::{self, NewLocalStatus};

    async fn actor(pool: &sqlx::PgPool, username: &str) -> i64 {
        account::create_local(
            pool,
            NewLocalAccount {
                username,
                display_name: username,
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap()
        .id
    }

    async fn film(
        pool: &sqlx::PgPool,
        owner: i64,
        title: &str,
        description: &str,
        visibility: &str,
        video: bool,
        attach: bool,
    ) -> (i64, Option<i64>) {
        let mut new = NewLocalStatus::new(owner, description, visibility, None);
        new.title = Some(title);
        new.object_type = video.then_some("Video");
        let stored = status::create_local(pool, new).await.unwrap();
        let media_id = if attach {
            let id = crate::id::next();
            sqlx::query(
                "INSERT INTO media_attachments \
                 (id, account_id, status_id, file_name, content_type, kind) \
                 VALUES ($1, $2, $3, 'fixture.mp4', 'video/mp4', 'video')",
            )
            .bind(id)
            .bind(owner)
            .bind(stored.id)
            .execute(pool)
            .await
            .unwrap();
            Some(id)
        } else {
            None
        };
        (stored.id, media_id)
    }

    #[sqlx::test]
    async fn discovery_scopes_search_follows_moderation_and_cursor(pool: sqlx::PgPool) {
        let owner = actor(&pool, "filmowner").await;
        let channel = actor(&pool, "filmchannel").await;
        let viewer = actor(&pool, "filmviewer").await;
        sqlx::query("INSERT INTO video_channels (account_id, owner_account_id) VALUES ($1, $2)")
            .bind(channel)
            .bind(owner)
            .execute(&pool)
            .await
            .unwrap();
        let (first, media) = film(
            &pool,
            owner,
            "Blue ocean",
            "<p>A meadow by the sea.</p>",
            "public",
            true,
            true,
        )
        .await;
        sqlx::query(
            "INSERT INTO video_publications (status_id, channel_id, media_id) VALUES ($1, $2, $3)",
        )
        .bind(first)
        .bind(channel)
        .bind(media.unwrap())
        .execute(&pool)
        .await
        .unwrap();
        let (second, _) = film(
            &pool,
            owner,
            "Golden sunset",
            "<p>Warm sky</p>",
            "public",
            true,
            true,
        )
        .await;
        film(
            &pool,
            owner,
            "Unlisted ocean",
            "<p>Private link</p>",
            "unlisted",
            true,
            true,
        )
        .await;
        film(
            &pool,
            owner,
            "Private ocean",
            "<p>Secret</p>",
            "private",
            true,
            true,
        )
        .await;
        film(
            &pool,
            owner,
            "Video-looking Note",
            "<p>Not a film</p>",
            "public",
            false,
            true,
        )
        .await;
        film(
            &pool,
            owner,
            "Empty ocean",
            "<p>Not playable</p>",
            "public",
            true,
            false,
        )
        .await;
        let (history, _) = film(
            &pool,
            owner,
            "History ocean",
            "<p>Backfill</p>",
            "public",
            true,
            true,
        )
        .await;
        sqlx::query("UPDATE statuses SET ingest_provenance = 'history' WHERE id = $1")
            .bind(history)
            .execute(&pool)
            .await
            .unwrap();

        let all = discover(&pool, None, Some(viewer), false, None, 20)
            .await
            .unwrap();
        assert_eq!(
            all.iter().map(|hit| hit.status_id).collect::<Vec<_>>(),
            [second, first]
        );
        assert_eq!(all[1].channel_id, channel);
        assert_eq!(
            discover(&pool, Some("ocean"), Some(viewer), false, None, 20)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            discover(&pool, Some("meadow"), Some(viewer), false, None, 20)
                .await
                .unwrap()
                .len(),
            1
        );
        let newest = discover(&pool, None, Some(viewer), false, None, 1)
            .await
            .unwrap();
        assert_eq!(newest[0].status_id, second);
        assert_eq!(
            discover(&pool, None, Some(viewer), false, Some(second), 1)
                .await
                .unwrap()[0]
                .status_id,
            first
        );

        assert!(
            discover(&pool, None, Some(viewer), true, None, 20)
                .await
                .unwrap()
                .is_empty()
        );
        follow::create_request(&pool, viewer, channel, None)
            .await
            .unwrap();
        assert!(
            discover(&pool, None, Some(viewer), true, None, 20)
                .await
                .unwrap()
                .is_empty()
        );
        follow::mark_accepted(&pool, viewer, channel).await.unwrap();
        let followed = discover(&pool, None, Some(viewer), true, None, 20)
            .await
            .unwrap();
        assert_eq!(
            followed.iter().map(|hit| hit.status_id).collect::<Vec<_>>(),
            [first]
        );
        sqlx::query("INSERT INTO blocks (id, account_id, target_account_id) VALUES ($1, $2, $3)")
            .bind(crate::id::next())
            .bind(viewer)
            .bind(channel)
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            discover(&pool, None, Some(viewer), true, None, 20)
                .await
                .unwrap()
                .is_empty()
        );
        sqlx::query("UPDATE statuses SET deleted_at = now() WHERE id = $1")
            .bind(second)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            discover(&pool, None, None, false, None, 20)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[sqlx::test]
    async fn channel_library_scopes_publication_search_cursor_and_moderation(pool: sqlx::PgPool) {
        let owner = actor(&pool, "libraryowner").await;
        let channel = actor(&pool, "librarychannel").await;
        let viewer = actor(&pool, "libraryviewer").await;
        sqlx::query("INSERT INTO video_channels (account_id, owner_account_id) VALUES ($1, $2)")
            .bind(channel)
            .bind(owner)
            .execute(&pool)
            .await
            .unwrap();
        let (first, first_media) =
            film(&pool, owner, "Blue film", "Sea", "public", true, true).await;
        let (second, second_media) =
            film(&pool, owner, "Gold film", "Sky", "public", true, true).await;
        let (unlisted, unlisted_media) = film(
            &pool,
            owner,
            "Hidden film",
            "Private link",
            "unlisted",
            true,
            true,
        )
        .await;
        for (status_id, media_id) in [
            (first, first_media),
            (second, second_media),
            (unlisted, unlisted_media),
        ] {
            sqlx::query("INSERT INTO video_publications (status_id, channel_id, media_id) VALUES ($1, $2, $3)")
                .bind(status_id)
                .bind(channel)
                .bind(media_id.unwrap())
                .execute(&pool)
                .await
                .unwrap();
        }
        let (creator_video, _) =
            film(&pool, owner, "Solo film", "Fallback", "public", true, true).await;
        assert!(library_visible(&pool, channel, Some(viewer)).await.unwrap());
        assert_eq!(
            discover_channel(&pool, channel, None, Some(viewer), None, 20)
                .await
                .unwrap()
                .iter()
                .map(|hit| hit.status_id)
                .collect::<Vec<_>>(),
            [second, first]
        );
        assert_eq!(
            discover_channel(&pool, channel, Some("Blue"), Some(viewer), None, 20)
                .await
                .unwrap()[0]
                .status_id,
            first
        );
        assert_eq!(
            discover_channel(&pool, channel, None, Some(viewer), Some(second), 20)
                .await
                .unwrap()[0]
                .status_id,
            first
        );
        assert_eq!(
            discover_channel(&pool, owner, None, Some(viewer), None, 20)
                .await
                .unwrap()[0]
                .status_id,
            creator_video
        );

        sqlx::query("INSERT INTO blocks (id, account_id, target_account_id) VALUES ($1, $2, $3)")
            .bind(crate::id::next())
            .bind(viewer)
            .bind(channel)
            .execute(&pool)
            .await
            .unwrap();
        assert!(!library_visible(&pool, channel, Some(viewer)).await.unwrap());
        assert!(
            discover_channel(&pool, channel, None, Some(viewer), None, 20)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(library_visible(&pool, owner, Some(viewer)).await.unwrap());
        sqlx::query("UPDATE accounts SET suspended_at = now() WHERE id = $1")
            .bind(owner)
            .execute(&pool)
            .await
            .unwrap();
        assert!(!library_visible(&pool, channel, None).await.unwrap());
        assert!(!library_visible(&pool, owner, None).await.unwrap());
        assert!(
            discover_channel(&pool, owner, None, None, None, 20)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[sqlx::test]
    async fn remote_channel_attribution_moves_library_and_followed_video(pool: sqlx::PgPool) {
        let author = actor(&pool, "remotefilmauthor").await;
        let channel = actor(&pool, "remotefilmchannel").await;
        let viewer = actor(&pool, "remotefilmviewer").await;
        let (video, _) = film(
            &pool,
            author,
            "Channel film",
            "Screen",
            "public",
            true,
            true,
        )
        .await;
        // Remote video rows can have no `kind`; their MIME type still makes
        // them playable Videos in API serialization and in this catalogue.
        sqlx::query("UPDATE media_attachments SET kind = NULL WHERE status_id = $1")
            .bind(video)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            discover(&pool, None, Some(viewer), false, None, 20)
                .await
                .unwrap()[0]
                .channel_id,
            author
        );
        set_remote_channel(&pool, video, Some(channel))
            .await
            .unwrap();
        assert_eq!(
            discover(&pool, None, Some(viewer), false, None, 20)
                .await
                .unwrap()[0]
                .channel_id,
            channel
        );
        assert!(
            discover_channel(&pool, author, None, Some(viewer), None, 20)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            discover_channel(&pool, channel, None, Some(viewer), None, 20)
                .await
                .unwrap()[0]
                .status_id,
            video
        );
        assert!(
            discover(&pool, None, Some(viewer), true, None, 20)
                .await
                .unwrap()
                .is_empty()
        );
        follow::create_request(&pool, viewer, channel, None)
            .await
            .unwrap();
        follow::mark_accepted(&pool, viewer, channel).await.unwrap();
        assert_eq!(
            discover(&pool, None, Some(viewer), true, None, 20)
                .await
                .unwrap()[0]
                .status_id,
            video
        );
        set_remote_channel(&pool, video, None).await.unwrap();
        assert!(
            discover(&pool, None, Some(viewer), true, None, 20)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            discover_channel(&pool, author, None, Some(viewer), None, 20)
                .await
                .unwrap()[0]
                .status_id,
            video
        );
    }

    #[sqlx::test]
    async fn remote_channel_backfill_claim_is_leased_and_settled(pool: sqlx::PgPool) {
        let author = actor(&pool, "legacyfilmowner").await;
        sqlx::query("UPDATE accounts SET domain = 'remote.example', uri = 'https://remote.example/users/legacyfilmowner' WHERE id = $1")
            .bind(author)
            .execute(&pool)
            .await
            .unwrap();
        let (video, _) = film(&pool, author, "Old film", "Archive", "public", true, true).await;
        sqlx::query("UPDATE statuses SET uri = 'https://remote.example/videos/watch/old-film', ingest_provenance = 'delivery' WHERE id = $1")
            .bind(video)
            .execute(&pool)
            .await
            .unwrap();
        let claimed = claim_remote_channel_backfill(&pool).await.unwrap().unwrap();
        assert_eq!(claimed.status_id, video);
        assert_eq!(claimed.account_id, author);
        assert_eq!(claimed.attempts, 0);
        assert!(
            claim_remote_channel_backfill(&pool)
                .await
                .unwrap()
                .is_none()
        );
        retry_remote_channel_check(&pool, video, std::time::Duration::from_secs(0))
            .await
            .unwrap();
        let retried = claim_remote_channel_backfill(&pool).await.unwrap().unwrap();
        assert_eq!(retried.attempts, 1);
        complete_remote_channel_check(&pool, video).await.unwrap();
        assert!(
            claim_remote_channel_backfill(&pool)
                .await
                .unwrap()
                .is_none()
        );
    }
}
