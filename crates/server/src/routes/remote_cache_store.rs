//! A cache row is published only after storage acknowledgement. The private
//! pre-write journal remains charged and revoked on failure or uncertain commit.
//! One owned finalizer holds its cleanup-row fence and resource permit across
//! filesystem I/O, even if the requesting browser leaves. No key is ever reused.
use crate::{AppState, error::ApiError, video_resources::TemporaryReservation};
use plamenu_db::{id, media};

pub(super) fn key(prefix: &str, hash: &str) -> String {
    let entropy = crate::media_processing::storage_stem(0);
    format!("{prefix}{hash}.{}.mp4", &entropy[1..])
}

pub(super) async fn admit(state: &AppState, bytes: u64) -> Result<TemporaryReservation, ApiError> {
    let bytes = bytes
        .checked_mul(2)
        .filter(|value| *value > 0)
        .ok_or(ApiError::NotFound)?;
    for _ in 0..100 {
        if let Some(permit) = state.remote_cache_resources.try_reserve_temporary(bytes) {
            return Ok(permit);
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    Err(ApiError::ServiceUnavailable(
        "Remote media cache is busy; try again shortly".into(),
    ))
}

pub(super) struct Range {
    pub media: i64,
    pub origin: String,
    pub start: i64,
    pub len: i64,
    pub key: String,
    pub bytes: i64,
}

pub(super) async fn store<G: Send + 'static>(
    state: &AppState,
    range: Range,
    source: tempfile::NamedTempFile,
    permit: TemporaryReservation,
    flight: G,
) -> Result<String, ApiError> {
    let state = state.clone();
    let task = tokio::spawn(async move {
        let _permit = permit;
        let _flight = flight;
        let cleanup_id = id::next();
        // A crash leaves this private journal reclaimable after one hour.
        // While the writer lives, FOR UPDATE makes cleanup skip its key.
        sqlx::query(
            "INSERT INTO media_cleanup_jobs (id,file_name,remote_cache_bytes,run_at)
            VALUES ($1,$2,$3,clock_timestamp()+interval '1 hour')",
        )
        .bind(cleanup_id)
        .bind(&range.key)
        .bind(range.bytes)
        .execute(&state.pool)
        .await
        .map_err(plamenu_db::DbError::from)?;
        let mut tx = state
            .pool
            .begin()
            .await
            .map_err(plamenu_db::DbError::from)?;
        sqlx::query("SELECT id FROM media_cleanup_jobs WHERE id=$1 FOR UPDATE")
            .bind(cleanup_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(plamenu_db::DbError::from)?;
        let result = async {
            state
                .media
                .put_file(&range.key, source.path())
                .await
                .map_err(|error| ApiError::Internal(Box::new(error)))?;
            let recorded = media::hls_segment_record(
                &mut *tx,
                range.media,
                &range.origin,
                range.start,
                range.len,
                &range.key,
                range.bytes,
            )
            .await?;
            if recorded {
                tx.commit().await.map_err(plamenu_db::DbError::from)?;
                Ok(range.key.clone())
            } else {
                tx.rollback().await.map_err(plamenu_db::DbError::from)?;
                media::hls_segment_lookup(&state.pool, &range.origin, range.start, range.len)
                    .await?
                    .map(|hit| hit.cache_file)
                    .ok_or(ApiError::NotFound)
            }
        }
        .await;
        // On an uncertain commit, waking an already-released journal is a
        // harmless no-op; never delete a cache row or refund bytes here.
        sqlx::query("UPDATE media_cleanup_jobs SET run_at=clock_timestamp() WHERE id=$1")
            .bind(cleanup_id)
            .execute(&state.pool)
            .await
            .map_err(plamenu_db::DbError::from)?;
        result
    });
    task.await
        .map_err(|error| ApiError::Internal(Box::new(error)))?
}
