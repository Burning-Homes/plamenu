//! Explicit, replay-safe listening reports. Byte serving has no history effects.
mod client;
use crate::{
    AppState,
    error::ApiError,
    music_collections::{self, Entry, Playable},
    music_queues::Source,
};
pub(crate) use client::complete as complete_for_client;
use plamenu_db::{DbError, id};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use time::OffsetDateTime;

#[derive(Clone, FromRow, Serialize)]
pub struct Preferences {
    pub history_enabled: bool,
    pub minimum_seconds: i32,
    pub minimum_percent: i32,
    #[serde(serialize_with = "string_revision")]
    pub revision: i64,
}
#[allow(clippy::trivially_copy_pass_by_ref)] // Serde serialize_with requires a borrowed field.
pub(crate) fn string_revision<S: serde::Serializer>(
    value: &i64,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&value.to_string())
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            history_enabled: true,
            minimum_seconds: 30,
            minimum_percent: 50,
            revision: 0,
        }
    }
}
#[derive(Deserialize)]
pub struct PreferenceChange {
    pub revision: String,
    pub history_enabled: bool,
    pub minimum_seconds: i32,
    pub minimum_percent: i32,
}
async fn preferences_conn(
    conn: &mut sqlx::PgConnection,
    owner: i64,
) -> Result<Preferences, ApiError> {
    Ok(sqlx::query_as("SELECT history_enabled,minimum_seconds,minimum_percent,revision FROM music_listening_preferences WHERE account_id=$1").bind(owner).fetch_optional(conn).await.map_err(DbError::from)?.unwrap_or_default())
}
pub async fn preferences(state: &AppState, owner: i64) -> Result<Preferences, ApiError> {
    let mut conn = state.pool.acquire().await.map_err(DbError::from)?;
    crate::music_queues::active_owner(&mut conn, owner).await?;
    preferences_conn(&mut conn, owner).await
}
pub async fn set_preferences(
    state: &AppState,
    owner: i64,
    input: &PreferenceChange,
) -> Result<Preferences, ApiError> {
    let expected = input
        .revision
        .parse::<i64>()
        .ok()
        .filter(|r| *r >= 0)
        .ok_or_else(|| {
            ApiError::Unprocessable("Listening preference revision is required".into())
        })?;
    if !(1..=600).contains(&input.minimum_seconds) || !(1..=100).contains(&input.minimum_percent) {
        return Err(ApiError::Unprocessable(
            "Invalid listening thresholds".into(),
        ));
    }
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    crate::music_assets::lock_owner(&mut tx, owner).await?;
    crate::music_queues::active_owner(&mut tx, owner).await?;
    let current = preferences_conn(&mut tx, owner).await?;
    if current.revision != expected {
        return Err(ApiError::Conflict(
            "Listening preferences changed; reload".into(),
        ));
    }
    let revision = expected
        .checked_add(1)
        .ok_or_else(|| ApiError::Conflict("Listening preference revision exhausted".into()))?;
    sqlx::query("INSERT INTO music_listening_preferences(account_id,history_enabled,minimum_seconds,minimum_percent,revision) VALUES($1,$2,$3,$4,$5) ON CONFLICT(account_id) DO UPDATE SET history_enabled=EXCLUDED.history_enabled,minimum_seconds=EXCLUDED.minimum_seconds,minimum_percent=EXCLUDED.minimum_percent,revision=EXCLUDED.revision").bind(owner).bind(input.history_enabled).bind(input.minimum_seconds).bind(input.minimum_percent).bind(revision).execute(&mut *tx).await.map_err(DbError::from)?;
    if !input.history_enabled {
        sqlx::query("DELETE FROM music_listen_sessions WHERE account_id=$1 AND counted_at IS NULL")
            .bind(owner)
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
    }
    tx.commit().await.map_err(DbError::from)?;
    preferences(state, owner).await
}
#[derive(Deserialize)]
pub struct Start {
    pub request_key: String,
    pub asset_id: Option<String>,
    pub status_id: Option<String>,
    pub episode_id: Option<String>,
    pub remote_id: Option<String>,
    pub queue_device: Option<String>,
    pub queue_entry_id: Option<String>,
}
#[derive(Deserialize)]
pub struct Progress {
    pub played_seconds: f64,
    #[serde(default)]
    pub finish: bool,
}
#[derive(FromRow)]
struct Stored {
    token: String,
    asset_id: Option<i64>,
    status_id: Option<i64>,
    episode_id: Option<i64>,
    remote_id: Option<i64>,
    duration: f64,
    threshold_seconds: f64,
    reported_seconds: f64,
    played_seconds: f64,
    counted_at: Option<OffsetDateTime>,
    history_id: Option<i64>,
    finished_at: Option<OffsetDateTime>,
}
#[derive(Serialize)]
pub struct Session {
    pub enabled: bool,
    pub token: Option<String>,
    pub played_seconds: f64,
    pub counted: bool,
    pub finished: bool,
}
impl Stored {
    fn entity(self) -> Session {
        Session {
            enabled: true,
            token: Some(self.token),
            played_seconds: self.played_seconds,
            counted: self.counted_at.is_some(),
            finished: self.finished_at.is_some(),
        }
    }
    fn entry(&self) -> Entry {
        Entry {
            remote_id: self.remote_id,
            episode_id: self.episode_id,
            id: 1,
            collection_id: 0,
            asset_id: self.asset_id,
            status_id: self.status_id,
            position: 1,
        }
    }
}
async fn playable(state: &AppState, owner: i64, entry: &Entry) -> Result<Playable, ApiError> {
    music_collections::sources(state, std::slice::from_ref(entry), Some(owner))
        .await?
        .into_iter()
        .next()
        .ok_or(ApiError::NotFound)
}
async fn start_entry(state: &AppState, owner: i64, input: &Start) -> Result<Entry, ApiError> {
    if input.queue_device.is_some() || input.queue_entry_id.is_some() {
        if input.asset_id.is_some()
            || input.status_id.is_some()
            || input.episode_id.is_some()
            || input.remote_id.is_some()
        {
            return Err(ApiError::Unprocessable(
                "Choose a direct source or a queue entry".into(),
            ));
        }
        let device = input.queue_device.as_deref().ok_or(ApiError::NotFound)?;
        let entry = input
            .queue_entry_id
            .as_deref()
            .and_then(|s| s.parse::<i64>().ok())
            .ok_or(ApiError::NotFound)?;
        return sqlx::query_as("SELECT e.id,e.queue_id AS collection_id,e.asset_id,e.status_id,e.episode_id,e.remote_id,e.position FROM music_queue_entries e JOIN music_queues q ON q.id=e.queue_id WHERE q.account_id=$1 AND q.device_key=$2 AND e.id=$3").bind(owner).bind(device).bind(entry).fetch_optional(&state.pool).await.map_err(DbError::from)?.ok_or(ApiError::NotFound);
    }
    Source {
        remote_id: input.remote_id.clone(),
        episode_id: input.episode_id.clone(),
        asset_id: input.asset_id.clone(),
        status_id: input.status_id.clone(),
    }
    .entry(1, 0, 1)
}
fn token() -> Result<String, ApiError> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|e| ApiError::Internal(Box::new(e)))?;
    Ok(bytes.into_iter().fold(String::new(), |mut output, b| {
        use std::fmt::Write as _;
        let _ = write!(output, "{b:02x}");
        output
    }))
}
pub async fn start(state: &AppState, owner: i64, input: &Start) -> Result<Session, ApiError> {
    if input.request_key.is_empty()
        || input.request_key.len() > 64
        || !input
            .request_key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(ApiError::Unprocessable("Invalid listen request key".into()));
    }
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    crate::music_assets::lock_owner(&mut tx, owner).await?;
    crate::music_queues::active_owner(&mut tx, owner).await?;
    let mut entry = start_entry(state, owner, input).await?;
    crate::music_remote::normalize_sources(state, std::slice::from_mut(&mut entry), owner).await?;
    let source = playable(state, owner, &entry).await?;
    let pref = preferences_conn(&mut tx, owner).await?;
    if !pref.history_enabled {
        return Ok(Session {
            enabled: false,
            token: None,
            played_seconds: 0.0,
            counted: false,
            finished: false,
        });
    }
    let previous: Option<Stored> = sqlx::query_as(
        "SELECT * FROM music_listen_sessions WHERE account_id=$1 AND request_key=$2",
    )
    .bind(owner)
    .bind(&input.request_key)
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::from)?;
    if let Some(previous) = previous {
        if previous.asset_id != entry.asset_id
            || previous.status_id != entry.status_id
            || previous.episode_id != entry.episode_id
            || previous.remote_id != entry.remote_id
        {
            return Err(ApiError::Conflict(
                "Listen request key belongs to another source".into(),
            ));
        }
        return Ok(previous.entity());
    }
    // Pending abandoned starts expire; counted history is never truncated here.
    sqlx::query("DELETE FROM music_listen_sessions WHERE account_id=$1 AND counted_at IS NULL AND last_report_at < clock_timestamp()-interval '1 day'").bind(owner).execute(&mut *tx).await.map_err(DbError::from)?;
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM music_listen_sessions WHERE account_id=$1 AND counted_at IS NULL AND finished_at IS NULL",
    )
    .bind(owner)
    .fetch_one(&mut *tx)
    .await
    .map_err(DbError::from)?;
    if pending >= 100 {
        return Err(ApiError::Unprocessable(
            "Too many pending listening sessions".into(),
        ));
    }
    let duration = if source.duration.is_finite() {
        source.duration.max(0.0)
    } else {
        0.0
    };
    let threshold = if duration > 0.0 {
        f64::from(pref.minimum_seconds).min(duration * f64::from(pref.minimum_percent) / 100.0)
    } else {
        f64::from(pref.minimum_seconds)
    };
    let stored:Stored=sqlx::query_as("INSERT INTO music_listen_sessions(id,token,account_id,request_key,asset_id,status_id,episode_id,duration,threshold_seconds,remote_id) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) RETURNING *").bind(id::next()).bind(token()?).bind(owner).bind(&input.request_key).bind(entry.asset_id).bind(entry.status_id).bind(entry.episode_id).bind(duration).bind(threshold).bind(entry.remote_id).fetch_one(&mut *tx).await.map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(stored.entity())
}
pub async fn progress(
    state: &AppState,
    owner: i64,
    token: &str,
    input: &Progress,
) -> Result<Session, ApiError> {
    if !input.played_seconds.is_finite() || !(0.0..=604_800.0).contains(&input.played_seconds) {
        return Err(ApiError::Unprocessable("Invalid played time".into()));
    }
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    crate::music_assets::lock_owner(&mut tx, owner).await?;
    crate::music_queues::active_owner(&mut tx, owner).await?;
    let stored: Stored = sqlx::query_as(
        "SELECT * FROM music_listen_sessions WHERE token=$1 AND account_id=$2 FOR UPDATE",
    )
    .bind(token)
    .bind(owner)
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::from)?
    .ok_or(ApiError::NotFound)?;
    playable(state, owner, &stored.entry()).await?;
    if !preferences_conn(&mut tx, owner).await?.history_enabled {
        return Err(ApiError::NotFound);
    }
    if stored.finished_at.is_some() {
        return Ok(stored.entity());
    }
    let elapsed:f64=sqlx::query_scalar("SELECT GREATEST(0,EXTRACT(EPOCH FROM clock_timestamp()-last_report_at))::float8 FROM music_listen_sessions WHERE token=$1").bind(token).fetch_one(&mut *tx).await.map_err(DbError::from)?;
    let delta = (input.played_seconds - stored.reported_seconds)
        .max(0.0)
        .min(elapsed)
        .min(15.0);
    let played = (stored.played_seconds + delta).min(if stored.duration > 0.0 {
        stored.duration
    } else {
        604_800.0
    });
    let counted = played >= stored.threshold_seconds;
    let history_id = stored.history_id.or_else(|| counted.then(id::next));
    let updated:Stored=sqlx::query_as("UPDATE music_listen_sessions SET reported_seconds=GREATEST(reported_seconds,$2),played_seconds=$3,last_report_at=clock_timestamp(),counted_at=CASE WHEN $4 THEN COALESCE(counted_at,clock_timestamp()) ELSE counted_at END,finished_at=CASE WHEN $5 THEN clock_timestamp() ELSE finished_at END,history_id=$6 WHERE token=$1 RETURNING *").bind(token).bind(input.played_seconds).bind(played).bind(counted).bind(input.finish).bind(history_id).fetch_one(&mut *tx).await.map_err(DbError::from)?;
    if counted
        && stored.counted_at.is_none()
        && let Some(episode) = stored.episode_id
    {
        sqlx::query("INSERT INTO music_podcast_plays(account_id,episode_id) VALUES($1,$2) ON CONFLICT DO NOTHING").bind(owner).bind(episode).execute(&mut *tx).await.map_err(DbError::from)?;
    }
    tx.commit().await.map_err(DbError::from)?;
    Ok(updated.entity())
}
#[derive(Serialize)]
pub struct HistoryItem {
    pub id: String,
    pub counted_at: String,
    #[serde(flatten)]
    pub track: Playable,
}
#[derive(FromRow)]
struct HistoryRow {
    id: i64,
    asset_id: Option<i64>,
    status_id: Option<i64>,
    episode_id: Option<i64>,
    remote_id: Option<i64>,
    counted_at: OffsetDateTime,
}
#[derive(Serialize)]
pub struct HistoryPage {
    pub items: Vec<HistoryItem>,
    pub before: Option<String>,
}
/// Resolve before pagination; an inaccessible row never becomes a visible cursor.
pub async fn history(
    state: &AppState,
    owner: i64,
    mut before: Option<i64>,
) -> Result<HistoryPage, ApiError> {
    let mut conn = state.pool.acquire().await.map_err(DbError::from)?;
    crate::music_queues::active_owner(&mut conn, owner).await?;
    drop(conn);
    let mut items = Vec::new();
    loop {
        let rows:Vec<HistoryRow>=sqlx::query_as("SELECT history_id AS id,asset_id,status_id,episode_id,remote_id,counted_at FROM music_listen_sessions WHERE account_id=$1 AND counted_at IS NOT NULL AND ($2::bigint IS NULL OR history_id<$2) ORDER BY history_id DESC LIMIT 100").bind(owner).bind(before).fetch_all(&state.pool).await.map_err(DbError::from)?;
        if rows.is_empty() {
            return Ok(HistoryPage {
                items,
                before: None,
            });
        }
        before = rows.last().map(|r| r.id);
        let entries = rows
            .iter()
            .map(|r| Entry {
                remote_id: r.remote_id,
                episode_id: r.episode_id,
                id: r.id,
                collection_id: 0,
                asset_id: r.asset_id,
                status_id: r.status_id,
                position: 1,
            })
            .collect::<Vec<_>>();
        let sources = music_collections::sources(state, &entries, Some(owner)).await?;
        let dates = rows
            .iter()
            .map(|r| (r.id.to_string(), r.counted_at))
            .collect::<std::collections::HashMap<_, _>>();
        for track in sources {
            let counted_at = dates[&track.entry_id]
                .format(&time::format_description::well_known::Rfc3339)
                .map_err(|e| ApiError::Internal(Box::new(e)))?;
            items.push(HistoryItem {
                id: track.entry_id.clone(),
                counted_at,
                track,
            });
            if items.len() > 50 {
                items.truncate(50);
                let cursor = items.last().map(|item| item.id.clone());
                return Ok(HistoryPage {
                    items,
                    before: cursor,
                });
            }
        }
        if rows.len() < 100 {
            return Ok(HistoryPage {
                items,
                before: None,
            });
        }
    }
}
pub async fn erase(state: &AppState, owner: i64) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    crate::music_assets::lock_owner(&mut tx, owner).await?;
    crate::music_queues::active_owner(&mut tx, owner).await?;
    sqlx::query("DELETE FROM music_client_playing WHERE account_id=$1")
        .bind(owner)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    sqlx::query("DELETE FROM music_listen_sessions WHERE account_id=$1")
        .bind(owner)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
