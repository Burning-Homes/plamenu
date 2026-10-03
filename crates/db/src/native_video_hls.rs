//! Atomic local HLS metadata. The corresponding fMP4 bodies are immutable
//! attempt outputs; neither this table nor a playlist is a storage journal.

use sqlx::{PgConnection, PgPool};
use std::collections::HashMap;

use crate::DbError;

/// One verified progressive MP4 owned by its existing native rendition/job.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow)]
#[serde(deny_unknown_fields)]
pub struct Progressive {
    pub file_name: String,
    pub size_bytes: i64,
    pub sha256: String,
    pub width: i32,
    pub height: i32,
    pub duration: f64,
    pub frame_rate: Option<String>,
    pub bitrate: i64,
    pub audio_codec: Option<String>,
}

impl Progressive {
    #[must_use]
    pub fn valid(&self) -> bool {
        self.file_name
            .split_once(".draft.progressive")
            .is_some_and(|(stem, tail)| {
                !stem.is_empty()
                    && stem.bytes().all(|b| b.is_ascii_alphanumeric())
                    && tail == format!("{}.mp4", self.height)
            })
            && self.size_bytes > 0
            && self.width > 0
            && self.height > 0
            && self.duration.is_finite()
            && self.duration > 0.0
            && self.bitrate > 0
            && self.sha256.len() == 64
            && self
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            && self.audio_codec.as_deref().is_none_or(|c| c == "aac")
    }
}

pub async fn set_progressive(
    conn: &mut PgConnection,
    media_id: i64,
    generation: i64,
    height: i32,
    progressive: &Progressive,
) -> Result<(), DbError> {
    if progressive.height != height || !progressive.valid() {
        return Err(sqlx::Error::Protocol("Invalid progressive rendition metadata".into()).into());
    }
    let affected = sqlx::query(
        "INSERT INTO native_video_progressive_files
         (media_id,generation,height,file_name,size_bytes,sha256,width,duration,frame_rate,bitrate,audio_codec)
         SELECT r.media_id,r.generation,r.height,$4,$5,$6,$7,$8,$9,$10,$11
         FROM native_video_hls_renditions r
         JOIN video_draft_outputs o ON o.file_name=$4 AND o.media_id=r.media_id
             AND o.generation=r.generation AND NOT o.retired
         WHERE r.media_id=$1 AND r.generation=$2 AND r.height=$3
         ON CONFLICT (media_id) DO UPDATE SET file_name=EXCLUDED.file_name,
             size_bytes=EXCLUDED.size_bytes,sha256=EXCLUDED.sha256,width=EXCLUDED.width,
             duration=EXCLUDED.duration,frame_rate=EXCLUDED.frame_rate,
             bitrate=EXCLUDED.bitrate,audio_codec=EXCLUDED.audio_codec
         WHERE native_video_progressive_files.generation=EXCLUDED.generation
             AND native_video_progressive_files.height=EXCLUDED.height",
    )
    .bind(media_id).bind(generation).bind(height).bind(&progressive.file_name)
    .bind(progressive.size_bytes).bind(&progressive.sha256).bind(progressive.width)
    .bind(progressive.duration).bind(&progressive.frame_rate).bind(progressive.bitrate)
    .bind(&progressive.audio_codec).execute(conn).await?.rows_affected();
    if affected != 1 {
        return Err(sqlx::Error::RowNotFound.into());
    }
    Ok(())
}

pub async fn progressive_for_media(
    conn: &mut PgConnection,
    media_id: i64,
) -> Result<Option<Progressive>, DbError> {
    let data = sqlx::query_as::<_, Progressive>("SELECT p.* FROM native_video_progressive_files p JOIN native_video_hls_renditions r ON r.media_id=p.media_id AND r.height=p.height AND r.generation=p.generation JOIN video_drafts d ON d.media_id=r.media_id AND d.generation=r.generation AND d.state='ready' JOIN media_attachments m ON m.id=r.media_id AND m.processing='complete' JOIN video_draft_outputs o ON o.file_name=p.file_name AND o.media_id=r.media_id AND o.generation=r.generation AND NOT o.retired WHERE r.media_id=$1")
        .bind(media_id).fetch_optional(conn).await?;
    Ok(data.filter(Progressive::valid))
}

/// `PeerTube` 8.3 / P2P Media Loader v4's twenty-byte ASCII `WebTorrent`
/// infohash. Resolution zero denotes a stream without advertised dimensions.
#[must_use]
pub fn p2p_infohash(uuid: &str, resolution: i32) -> String {
    use base64::Engine as _;
    use sha1::Digest as _;
    let resolution = if resolution > 0 {
        resolution.to_string()
    } else {
        "default".into()
    };
    let swarm = format!("pt-v2-{uuid}-main-default-{resolution}");
    let digest = sha1::Sha1::digest(swarm.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(&digest[..15])
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Rendition {
    pub media_id: i64,
    pub generation: i64,
    pub height: i32,
    pub width: i32,
    pub bandwidth: i64,
    pub size_bytes: i64,
    pub file_name: String,
    pub playlist: String,
    pub segment_sha256: serde_json::Value,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct LadderEntry {
    pub height: i32,
    pub width: i32,
    pub bandwidth: i64,
}

/// Small `ActivityPub` link metadata. Never load playlist bodies for a page of
/// federated objects; those can be megabytes for long videos.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct FileTag {
    pub media_id: i64,
    pub content_revision: i64,
    pub height: i32,
    pub width: i32,
    pub size_bytes: i64,
    pub file_name: String,
    pub has_segment_sha256: bool,
    pub progressive: Option<serde_json::Value>,
}

pub async fn tags_for_media(
    conn: &mut PgConnection,
    media_ids: &[i64],
) -> Result<HashMap<i64, Vec<FileTag>>, DbError> {
    if media_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query_as::<_,FileTag>(
        "SELECT r.media_id, m.video_content_revision AS content_revision, r.height, r.width, r.size_bytes, r.file_name,
                (r.segment_sha256 <> '{}'::jsonb) AS has_segment_sha256,
                CASE WHEN m.processing='complete' AND EXISTS(SELECT 1 FROM video_draft_outputs o WHERE o.file_name=p.file_name AND o.media_id=r.media_id AND o.generation=r.generation AND NOT o.retired) AND NOT EXISTS(SELECT 1 FROM media_cleanup_jobs j WHERE j.file_name=p.file_name) THEN to_jsonb(p) - ARRAY['media_id','generation'] ELSE NULL END AS progressive
         FROM native_video_hls_renditions r
         LEFT JOIN native_video_progressive_files p ON p.media_id=r.media_id AND p.height=r.height AND p.generation=r.generation
         JOIN media_attachments m ON m.id=r.media_id
         JOIN video_drafts d ON d.media_id = r.media_id AND d.generation = r.generation AND d.state = 'ready'
         WHERE r.media_id = ANY($1)
         ORDER BY r.height DESC",
    )
    .bind(media_ids)
    .fetch_all(conn)
    .await?;
    let mut by_media: HashMap<i64, Vec<FileTag>> = HashMap::new();
    for row in rows {
        by_media.entry(row.media_id).or_default().push(row);
    }
    Ok(by_media)
}

pub async fn replace(
    conn: &mut PgConnection,
    media_id: i64,
    generation: i64,
    renditions: &[Rendition],
) -> Result<(), DbError> {
    sqlx::query!(
        "DELETE FROM native_video_hls_renditions WHERE media_id = $1",
        media_id,
    )
    .execute(&mut *conn)
    .await?;
    for rendition in renditions {
        sqlx::query!(
            "INSERT INTO native_video_hls_renditions
             (media_id, generation, height, width, bandwidth, size_bytes, file_name, playlist, segment_sha256)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            media_id,
            generation,
            rendition.height,
            rendition.width,
            rendition.bandwidth,
            rendition.size_bytes,
            rendition.file_name,
            rendition.playlist,
            rendition.segment_sha256,
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Only committed, unsuspended publications expose playlist metadata. The
/// file route has an independent check before serving the referenced bytes.
pub async fn public_ladder(pool: &PgPool, media_id: i64) -> Result<Vec<LadderEntry>, DbError> {
    public_ladder_revision(pool, media_id, None).await
}
pub async fn public_ladder_revision(
    pool: &PgPool,
    media_id: i64,
    revision: Option<i64>,
) -> Result<Vec<LadderEntry>, DbError> {
    Ok(sqlx::query_as!(
        LadderEntry,
        "SELECT r.height, r.width, r.bandwidth
         FROM native_video_hls_renditions r
         JOIN media_attachments m ON m.id = r.media_id AND m.processing = 'complete'
         JOIN video_drafts d ON d.media_id = m.id AND d.generation = r.generation AND d.state = 'ready'
         JOIN public_native_videos v ON v.media_id = m.id AND v.status_id = m.status_id
         WHERE r.media_id = $1 AND ($2::bigint IS NULL OR m.video_content_revision=$2)
         ORDER BY r.height DESC",
        media_id,
        revision,
    )
    .fetch_all(pool)
    .await?)
}

pub async fn public_playlist(
    pool: &PgPool,
    media_id: i64,
    height: i32,
    revision: Option<i64>,
) -> Result<Option<String>, DbError> {
    Ok(sqlx::query_scalar!(
        "SELECT r.playlist FROM native_video_hls_renditions r
         JOIN media_attachments m ON m.id = r.media_id AND m.processing = 'complete'
         JOIN video_drafts d ON d.media_id = m.id AND d.generation = r.generation AND d.state = 'ready'
         JOIN public_native_videos v ON v.media_id = m.id AND v.status_id = m.status_id
         WHERE r.media_id = $1 AND r.height = $2 AND ($3::bigint IS NULL OR m.video_content_revision=$3)",
        media_id,
        height,
        revision,
    )
    .fetch_optional(pool)
    .await?)
}

/// Integrity metadata is gated by the same committed-publication predicate as
/// the playlists. Old renditions without hashes simply have no P2P lane.
pub async fn public_sha256(
    pool: &PgPool,
    media_id: i64,
    revision: Option<i64>,
) -> Result<Option<serde_json::Value>, DbError> {
    let rows = sqlx::query!(
        "SELECT r.file_name, r.segment_sha256 FROM native_video_hls_renditions r
         JOIN media_attachments m ON m.id = r.media_id AND m.processing = 'complete'
         JOIN video_drafts d ON d.media_id = m.id AND d.generation = r.generation AND d.state = 'ready'
         JOIN public_native_videos v ON v.media_id = m.id AND v.status_id = m.status_id
         WHERE r.media_id = $1 AND ($2::bigint IS NULL OR m.video_content_revision=$2)",
        media_id,
        revision,
    )
    .fetch_all(pool)
    .await?;
    if rows.is_empty()
        || rows.iter().any(|r| {
            r.segment_sha256
                .as_object()
                .is_none_or(serde_json::Map::is_empty)
        })
    {
        return Ok(None);
    }
    let mut manifest = serde_json::Map::new();
    for row in rows {
        manifest.insert(row.file_name, row.segment_sha256);
    }
    Ok(Some(serde_json::Value::Object(manifest)))
}

/// Local video UUIDs with complete integrity metadata, batched for API pages.
pub async fn public_peer_uuids(
    pool: &PgPool,
    media_ids: &[i64],
) -> Result<HashMap<i64, (String, i64)>, DbError> {
    if media_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query!(
        "SELECT r.media_id, v.uuid AS \"uuid!\", m.video_content_revision FROM native_video_hls_renditions r
         JOIN media_attachments m ON m.id = r.media_id AND m.processing = 'complete'
         JOIN video_drafts d ON d.media_id = m.id AND d.generation = r.generation AND d.state = 'ready'
         JOIN public_native_videos v ON v.media_id = m.id AND v.status_id = m.status_id
         WHERE r.media_id = ANY($1)
         GROUP BY r.media_id, v.uuid, m.video_content_revision
         HAVING bool_and(r.segment_sha256 <> '{}'::jsonb AND r.p2p_infohash IS NOT NULL)",
        media_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.media_id, (r.uuid, r.video_content_revision)))
        .collect())
}

pub async fn assign_infohash(
    conn: &mut PgConnection,
    media_id: i64,
    height: i32,
    infohash: &str,
) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE native_video_hls_renditions SET p2p_infohash = $3
         WHERE media_id = $1 AND height = $2 AND segment_sha256 <> '{}'::jsonb",
        media_id,
        height,
        infohash,
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// The tracker only accepts active, public native videos in its indexed
/// allowlist. No arbitrary swarm may consume memory or relay signaling here.
pub async fn public_infohash_exists(pool: &PgPool, infohash: &str) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar!(
        "SELECT EXISTS (SELECT 1 FROM native_video_hls_renditions r
         JOIN media_attachments m ON m.id = r.media_id AND m.processing = 'complete'
         JOIN video_drafts d ON d.media_id = m.id AND d.generation = r.generation AND d.state = 'ready'
         JOIN public_native_videos v ON v.media_id = m.id AND v.status_id = m.status_id
         WHERE r.p2p_infohash = $1)",
        infohash,
    )
    .fetch_one(pool)
    .await?
    .unwrap_or(false))
}
