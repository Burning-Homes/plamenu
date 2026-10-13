//! Trusted compatibility clients explicitly submit completed plays. Byte reads
//! never call this service, and the account's history opt-out remains decisive.
use super::{preferences_conn, token};
use crate::{AppState, error::ApiError, music_clients::auth::Principal, music_queues::Source};
use plamenu_db::{DbError, id};
use time::OffsetDateTime;
pub(crate) async fn complete(
    state: &AppState,
    principal: &Principal,
    source: &Source,
    request_key: &str,
    played_at: OffsetDateTime,
) -> Result<(), ApiError> {
    principal.require_manage()?;
    if request_key.is_empty()
        || request_key.len() > 64
        || !request_key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        || played_at > OffsetDateTime::now_utc() + time::Duration::minutes(5)
    {
        return Err(ApiError::Unprocessable("Invalid completed play".into()));
    }
    let mut entry = source.entry(1, 0, 1)?;
    crate::music_remote::normalize_sources(
        state,
        std::slice::from_mut(&mut entry),
        principal.owner,
    )
    .await?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    crate::music_assets::lock_owner(&mut tx, principal.owner).await?;
    crate::music_queues::active_owner(&mut tx, principal.owner).await?;
    principal.current(state).await?;
    let playable = super::playable(state, principal.owner, &entry).await?;
    let preferences = preferences_conn(&mut tx, principal.owner).await?;
    if !preferences.history_enabled {
        return Ok(());
    }
    let duration = if playable.duration > 0.0 {
        playable.duration
    } else {
        f64::from(preferences.minimum_seconds)
    };
    let threshold = f64::from(preferences.minimum_seconds)
        .min(duration * f64::from(preferences.minimum_percent) / 100.0);
    sqlx::query("INSERT INTO music_listen_sessions(id,token,account_id,request_key,asset_id,status_id,episode_id,remote_id,duration,threshold_seconds,started_at,last_report_at,reported_seconds,played_seconds,counted_at,history_id,finished_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$11,$9,$9,$11,$12,$11) ON CONFLICT(account_id,request_key) DO NOTHING")
        .bind(id::next()).bind(token()?).bind(principal.owner).bind(request_key).bind(entry.asset_id).bind(entry.status_id).bind(entry.episode_id).bind(entry.remote_id).bind(duration).bind(threshold).bind(played_at).bind(id::next()).execute(&mut *tx).await.map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
