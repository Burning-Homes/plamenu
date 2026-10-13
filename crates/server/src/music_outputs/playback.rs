//! Checkpoints never overwrite a newer seek or a reclaimed output generation.
use super::Output;
use crate::{AppState, error::ApiError, music_queues::Queue};
use plamenu_db::DbError;
pub struct Listen {
    token: Option<String>,
    played: f64,
    baseline: f64,
}
impl Listen {
    pub async fn begin(state: &AppState, row: &Output, queue: &Queue) -> Result<Self, ApiError> {
        let session = crate::music_listening::start(
            state,
            row.owner_id,
            &crate::music_listening::Start {
                request_key: plamenu_db::id::next().to_string(),
                asset_id: None,
                status_id: None,
                episode_id: None,
                remote_id: None,
                queue_device: Some(row.device.clone()),
                queue_entry_id: queue.current_entry_id.clone(),
            },
        )
        .await?;
        Ok(Self {
            token: session.token,
            played: 0.0,
            baseline: 0.0,
        })
    }
    pub fn new_segment(&mut self) {
        self.baseline = self.played;
    }
    pub async fn report(
        &mut self,
        state: &AppState,
        row: &Output,
        seconds: f64,
        finish: bool,
    ) -> Result<(), ApiError> {
        self.played = self.played.max(self.baseline + seconds);
        if let Some(token) = &self.token {
            match crate::music_listening::progress(
                state,
                row.owner_id,
                token,
                &crate::music_listening::Progress {
                    played_seconds: self.played,
                    finish,
                },
            )
            .await
            {
                Ok(_) => {}
                Err(ApiError::NotFound) => self.token = None,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}
pub async fn checkpoint(
    state: &AppState,
    row: &Output,
    queue: &Queue,
    position: f64,
) -> Result<bool, ApiError> {
    let revision = queue
        .revision
        .parse::<i64>()
        .map_err(|_| ApiError::NotFound)?;
    let entry = queue
        .current_entry_id
        .as_deref()
        .and_then(|value| value.parse::<i64>().ok())
        .ok_or(ApiError::NotFound)?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    crate::music_assets::lock_owner(&mut tx, row.owner_id).await?;
    let live:Option<i64>=sqlx::query_scalar("SELECT id FROM music_outputs WHERE id=$1 AND generation=$2 AND worker_lease=$3 AND lease_until>now() AND transport='playing' AND enabled FOR UPDATE")
        .bind(row.id).bind(row.generation).bind(row.worker_lease).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
    if live.is_none() {
        return Ok(false);
    }
    let changed=sqlx::query("UPDATE music_queues SET position_seconds=$5 WHERE account_id=$1 AND device_key=$2 AND revision=$3 AND current_entry_id=$4")
        .bind(row.owner_id).bind(&row.device).bind(revision).bind(entry).bind(position).execute(&mut *tx).await.map_err(DbError::from)?;
    if changed.rows_affected() != 1 {
        return Ok(false);
    }
    sqlx::query("UPDATE music_outputs SET current_entry_id=$2,position_seconds=$3 WHERE id=$1")
        .bind(row.id)
        .bind(entry)
        .bind(position)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(true)
}
