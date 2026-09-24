//! PeerTube-compatible HLS peer hints for a remote video. The publisher's
//! signed `ActivityPub` object is the source of truth; absent/unsafe hints
//! remove the row and preserve ordinary HTTP playback.

use std::collections::HashMap;

use sqlx::PgPool;

use crate::DbError;

#[derive(Debug, Clone)]
pub struct RemoteHlsPeers {
    pub video_uuid: String,
    pub trackers: Vec<String>,
    pub sha256_url: String,
}

pub async fn replace(
    pool: &PgPool,
    media_id: i64,
    hints: Option<&RemoteHlsPeers>,
) -> Result<(), DbError> {
    if let Some(hints) = hints {
        sqlx::query!(
            "INSERT INTO remote_hls_peers (media_id, video_uuid, trackers, sha256_url)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (media_id) DO UPDATE SET
                 video_uuid = EXCLUDED.video_uuid,
                 trackers = EXCLUDED.trackers,
                 sha256_url = EXCLUDED.sha256_url",
            media_id,
            hints.video_uuid,
            &hints.trackers,
            hints.sha256_url,
        )
        .execute(pool)
        .await?;
    } else {
        sqlx::query!("DELETE FROM remote_hls_peers WHERE media_id = $1", media_id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

pub async fn for_media(
    pool: &PgPool,
    media_ids: &[i64],
) -> Result<HashMap<i64, RemoteHlsPeers>, DbError> {
    if media_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query!(
        "SELECT media_id, video_uuid, trackers, sha256_url
         FROM remote_hls_peers WHERE media_id = ANY($1)",
        media_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            (
                row.media_id,
                RemoteHlsPeers {
                    video_uuid: row.video_uuid,
                    trackers: row.trackers,
                    sha256_url: row.sha256_url,
                },
            )
        })
        .collect())
}
