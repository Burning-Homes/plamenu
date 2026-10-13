//! Device queues retain only references; each response resolves current ACLs.
use crate::{
    AppState,
    error::ApiError,
    music_collections::{self, Entry, Playable},
};
use plamenu_db::{DbError, id};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::FromRow;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Source {
    pub remote_id: Option<String>,
    pub asset_id: Option<String>,
    pub status_id: Option<String>,
    pub episode_id: Option<String>,
}
impl Source {
    pub(crate) fn asset(id: i64) -> Self {
        Self {
            asset_id: Some(id.to_string()),
            remote_id: None,
            status_id: None,
            episode_id: None,
        }
    }
    pub(crate) fn entry(
        &self,
        entry_id: i64,
        queue: i64,
        position: i32,
    ) -> Result<Entry, ApiError> {
        let parse = |s: &str| {
            s.parse::<i64>()
                .ok()
                .filter(|id| *id > 0)
                .ok_or_else(|| ApiError::Unprocessable("Invalid music source".into()))
        };
        let asset_id = self.asset_id.as_deref().map(parse).transpose()?;
        let status_id = self.status_id.as_deref().map(parse).transpose()?;
        let episode_id = self.episode_id.as_deref().map(parse).transpose()?;
        let remote_id = self.remote_id.as_deref().map(parse).transpose()?;
        if [asset_id, status_id, episode_id, remote_id]
            .iter()
            .filter(|v| v.is_some())
            .count()
            != 1
        {
            return Err(ApiError::Unprocessable(
                "Choose exactly one music source".into(),
            ));
        }
        Ok(Entry {
            remote_id,
            episode_id,
            id: entry_id,
            collection_id: queue,
            asset_id,
            status_id,
            position,
        })
    }
}
#[derive(Debug, Clone, FromRow)]
struct Stored {
    id: i64,
    revision: i64,
    current_entry_id: Option<i64>,
    position_seconds: f64,
    repeat_mode: String,
    shuffle_seed: Option<String>,
}
#[derive(Debug, Clone, Serialize)]
pub struct Queue {
    pub device: String,
    pub revision: String,
    pub current_entry_id: Option<String>,
    pub position_seconds: f64,
    pub repeat_mode: String,
    pub shuffle: bool,
    pub entries: Vec<Playable>,
}
#[derive(Debug, Deserialize)]
pub struct Change {
    pub revision: String,
    #[serde(flatten)]
    pub action: Action,
}
#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    Play {
        sources: Vec<Source>,
    },
    Add {
        sources: Vec<Source>,
    },
    Collection {
        collection_id: String,
        #[serde(default)]
        replace: bool,
    },
    Podcast {
        podcast_id: String,
        #[serde(default)]
        oldest: bool,
        offset: Option<i64>,
        #[serde(default)]
        replace: bool,
    },
    Remove {
        entry_id: String,
    },
    Order {
        entry_ids: Vec<String>,
    },
    Move {
        entry_id: String,
        direction: String,
    },
    Select {
        entry_id: String,
    },
    Next {
        #[serde(default)]
        ended: bool,
    },
    Previous,
    Settings {
        repeat_mode: String,
        shuffle: bool,
    },
    Seek {
        seconds: f64,
        entry_id: String,
    },
    Clear,
    Forget,
}
fn device_key(device: &str) -> Result<(), ApiError> {
    if device.is_empty()
        || device.len() > 64
        || !device
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(ApiError::Unprocessable("Invalid playback device".into()));
    }
    Ok(())
}
async fn entries(conn: &mut sqlx::PgConnection, queue: i64) -> Result<Vec<Entry>, ApiError> {
    Ok(sqlx::query_as("SELECT id,queue_id AS collection_id,asset_id,status_id,episode_id,remote_id,position FROM music_queue_entries WHERE queue_id=$1 ORDER BY position,id LIMIT 1000").bind(queue).fetch_all(conn).await.map_err(DbError::from)?)
}
async fn resolve(
    state: &AppState,
    owner: i64,
    stored: &Stored,
    rows: &[Entry],
) -> Result<Vec<Playable>, ApiError> {
    let mut items = music_collections::sources(state, rows, Some(owner)).await?;
    if let Some(seed) = &stored.shuffle_seed {
        items.sort_by_cached_key(|entry| {
            Sha256::digest(format!("{seed}:{}", entry.entry_id).as_bytes()).to_vec()
        });
    }
    Ok(items)
}
fn snapshot(device: &str, stored: &Stored, items: Vec<Playable>) -> Queue {
    let current = stored
        .current_entry_id
        .map(|id| id.to_string())
        .filter(|id| items.iter().any(|item| &item.entry_id == id));
    let duration = current
        .as_ref()
        .and_then(|id| items.iter().find(|item| &item.entry_id == id))
        .map_or(0.0, |item| item.duration);
    Queue {
        device: device.into(),
        revision: stored.revision.to_string(),
        position_seconds: if current.is_some() {
            playback_position(stored.position_seconds, duration)
        } else {
            0.0
        },
        current_entry_id: current,
        repeat_mode: stored.repeat_mode.clone(),
        shuffle: stored.shuffle_seed.is_some(),
        entries: items,
    }
}
fn playback_position(seconds: f64, duration: f64) -> f64 {
    seconds.min(if duration > 0.0 { duration } else { 604_800.0 })
}
fn empty() -> Stored {
    Stored {
        id: 0,
        revision: 0,
        current_entry_id: None,
        position_seconds: 0.0,
        repeat_mode: "off".into(),
        shuffle_seed: None,
    }
}
pub(crate) async fn active_owner(
    conn: &mut sqlx::PgConnection,
    owner: i64,
) -> Result<(), ApiError> {
    let active:Option<i64>=sqlx::query_scalar("SELECT a.id FROM accounts a WHERE a.id=$1 AND a.domain IS NULL AND a.deleted_at IS NULL AND a.suspended_at IS NULL AND NOT EXISTS(SELECT 1 FROM users u WHERE u.account_id=a.id AND (NOT u.approved OR u.confirmed_at IS NULL))").bind(owner).fetch_optional(conn).await.map_err(DbError::from)?;
    active.ok_or(ApiError::NotFound)?;
    Ok(())
}
pub async fn get(state: &AppState, owner: i64, device: &str) -> Result<Queue, ApiError> {
    device_key(device)?;
    let mut conn = state.pool.acquire().await.map_err(DbError::from)?;
    active_owner(&mut conn, owner).await?;
    let stored: Option<Stored> =
        sqlx::query_as("SELECT * FROM music_queues WHERE account_id=$1 AND device_key=$2")
            .bind(owner)
            .bind(device)
            .fetch_optional(&mut *conn)
            .await
            .map_err(DbError::from)?;
    let Some(stored) = stored else {
        return Ok(snapshot(device, &empty(), Vec::new()));
    };
    let rows = entries(&mut conn, stored.id).await?;
    drop(conn);
    let items = resolve(state, owner, &stored, &rows).await?;
    Ok(snapshot(device, &stored, items))
}
/// An additive collection import resolves every source, not just the collection.
pub(crate) async fn collection_sources(
    state: &AppState,
    owner: i64,
    input: &str,
) -> Result<Vec<Source>, ApiError> {
    let collection = input
        .parse::<i64>()
        .map_err(|_| ApiError::Unprocessable("Invalid music collection".into()))?;
    music_collections::get(state, collection, Some(owner)).await?;
    let rows = music_collections::entries(state, collection).await?;
    let visible = music_collections::sources(state, &rows, Some(owner))
        .await?
        .into_iter()
        .map(|p| p.entry_id)
        .collect::<std::collections::HashSet<_>>();
    let sources = rows
        .into_iter()
        .filter(|row| visible.contains(&row.id.to_string()))
        .map(|row| Source {
            remote_id: row.remote_id.map(|id| id.to_string()),
            episode_id: row.episode_id.map(|id| id.to_string()),
            asset_id: row.asset_id.map(|id| id.to_string()),
            status_id: row.status_id.map(|id| id.to_string()),
        })
        .collect::<Vec<_>>();
    if sources.len() > 1000 {
        return Err(ApiError::Unprocessable("Queue limit is 1000 tracks".into()));
    }
    Ok(sources)
}
async fn insert_sources(
    state: &AppState,
    conn: &mut sqlx::PgConnection,
    owner: i64,
    stored: &Stored,
    sources: &[Source],
    rows: &[Entry],
) -> Result<(), ApiError> {
    if sources.is_empty() || sources.len() + rows.len() > 1000 {
        return Err(ApiError::Unprocessable(
            "Choose between 1 and 1000 queue tracks".into(),
        ));
    }
    // Compact after deletion. Original order is independent of shuffled playback.
    for (position, row) in rows.iter().enumerate() {
        sqlx::query("UPDATE music_queue_entries SET position=$2 WHERE id=$1")
            .bind(row.id)
            .bind(i32::try_from(position + 1).unwrap_or(1000))
            .execute(&mut *conn)
            .await
            .map_err(DbError::from)?;
    }
    let mut new = sources
        .iter()
        .enumerate()
        .map(|(offset, source)| {
            source.entry(
                id::next(),
                stored.id,
                i32::try_from(rows.len() + offset + 1).unwrap_or(1000),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    crate::music_remote::normalize_sources(state, &mut new, owner).await?;
    let authorized = music_collections::sources(state, &new, Some(owner)).await?;
    if authorized.len() != new.len() {
        return Err(ApiError::NotFound);
    }
    for row in &new {
        sqlx::query("INSERT INTO music_queue_entries(id,queue_id,asset_id,status_id,episode_id,position,remote_id) VALUES($1,$2,$3,$4,$5,$6,$7)").bind(row.id).bind(stored.id).bind(row.asset_id).bind(row.status_id).bind(row.episode_id).bind(row.position).bind(row.remote_id).execute(&mut *conn).await.map_err(DbError::from)?;
    }
    Ok(())
}
pub async fn change(
    state: &AppState,
    owner: i64,
    device: &str,
    input: &Change,
) -> Result<Queue, ApiError> {
    change_fenced(state, owner, device, input, None).await
}
pub(crate) async fn change_output(
    state: &AppState,
    output: &crate::music_outputs::Output,
    input: &Change,
) -> Result<Queue, ApiError> {
    change_fenced(state, output.owner_id, &output.device, input, Some(output)).await
}
async fn change_fenced(
    state: &AppState,
    owner: i64,
    device: &str,
    input: &Change,
    output: Option<&crate::music_outputs::Output>,
) -> Result<Queue, ApiError> {
    device_key(device)?;
    let expected = input
        .revision
        .parse::<i64>()
        .ok()
        .filter(|r| *r >= 0)
        .ok_or_else(|| ApiError::Unprocessable("Queue revision is required".into()))?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    crate::music_assets::lock_owner(&mut tx, owner).await?;
    active_owner(&mut tx, owner).await?;
    if let Some(output) = output {
        let valid: Option<i64> = sqlx::query_scalar("SELECT id FROM music_outputs WHERE id=$1 AND owner_id=$2 AND generation=$3 AND worker_lease=$4 AND lease_until>now() AND transport='playing' AND enabled FOR UPDATE")
            .bind(output.id).bind(owner).bind(output.generation).bind(output.worker_lease)
            .fetch_optional(&mut *tx).await.map_err(DbError::from)?;
        valid.ok_or_else(|| ApiError::Conflict("HTTP stream playback changed".into()))?;
    }
    let current: Option<Stored> = sqlx::query_as(
        "SELECT * FROM music_queues WHERE account_id=$1 AND device_key=$2 FOR UPDATE",
    )
    .bind(owner)
    .bind(device)
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::from)?;
    if current.as_ref().map_or(0, |q| q.revision) != expected {
        return Err(ApiError::Conflict(
            "Queue changed; reload the player".into(),
        ));
    }
    if matches!(input.action, Action::Forget) {
        sqlx::query("DELETE FROM music_queues WHERE account_id=$1 AND device_key=$2")
            .bind(owner)
            .bind(device)
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
        tx.commit().await.map_err(DbError::from)?;
        return get(state, owner, device).await;
    }
    let mut stored = if let Some(current) = current {
        current
    } else {
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM music_queues WHERE account_id=$1")
                .bind(owner)
                .fetch_one(&mut *tx)
                .await
                .map_err(DbError::from)?;
        if count >= 32 {
            return Err(ApiError::Unprocessable(
                "Playback device limit reached".into(),
            ));
        }
        sqlx::query_as(
            "INSERT INTO music_queues(id,account_id,device_key) VALUES($1,$2,$3) RETURNING *",
        )
        .bind(id::next())
        .bind(owner)
        .bind(device)
        .fetch_one(&mut *tx)
        .await
        .map_err(DbError::from)?
    };
    let rows = entries(&mut tx, stored.id).await?;
    let visible = resolve(state, owner, &stored, &rows).await?;
    // Stale or revoked current sources never select the next entry accidentally.
    if !visible.iter().any(|p| {
        Some(p.entry_id.as_str()) == stored.current_entry_id.map(|id| id.to_string()).as_deref()
    }) {
        stored.current_entry_id = None;
        stored.position_seconds = 0.0;
    }
    if !transport(&mut stored, &rows, &visible, &input.action)? {
        mutate_sources(state, &mut tx, owner, &mut stored, &rows, &input.action).await?;
    }
    // First write produces revision 1; every later command produces its successor.
    let revision = expected
        .checked_add(1)
        .ok_or_else(|| ApiError::Conflict("Queue revision exhausted".into()))?;
    sqlx::query("UPDATE music_queues SET revision=$2,current_entry_id=$3,position_seconds=$4,repeat_mode=$5,shuffle_seed=$6,updated_at=now() WHERE id=$1").bind(stored.id).bind(revision).bind(stored.current_entry_id).bind(stored.position_seconds).bind(&stored.repeat_mode).bind(&stored.shuffle_seed).execute(&mut *tx).await.map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    get(state, owner, device).await
}

fn transport(
    stored: &mut Stored,
    rows: &[Entry],
    visible: &[Playable],
    action: &Action,
) -> Result<bool, ApiError> {
    let parse_entry = |s: &str| {
        s.parse::<i64>()
            .ok()
            .filter(|id| rows.iter().any(|r| r.id == *id))
            .ok_or(ApiError::NotFound)
    };
    match action {
        Action::Select { entry_id } => {
            if !visible.iter().any(|p| &p.entry_id == entry_id) {
                return Err(ApiError::NotFound);
            }
            stored.current_entry_id = Some(parse_entry(entry_id)?);
            stored.position_seconds = 0.0;
        }
        Action::Next { ended } => {
            let index = visible.iter().position(|p| {
                Some(p.entry_id.as_str())
                    == stored.current_entry_id.map(|id| id.to_string()).as_deref()
            });
            let next = if *ended && stored.repeat_mode == "one" && index.is_some() {
                index
            } else {
                match index {
                    None => Some(0),
                    Some(i) if i + 1 < visible.len() => Some(i + 1),
                    Some(_) if stored.repeat_mode == "all" => Some(0),
                    Some(_) => None,
                }
            };
            stored.current_entry_id = next
                .and_then(|i| visible.get(i))
                .and_then(|p| p.entry_id.parse().ok());
            stored.position_seconds = 0.0;
        }
        Action::Previous => previous(stored, visible),
        Action::Settings {
            repeat_mode,
            shuffle,
        } => {
            if !["off", "one", "all"].contains(&repeat_mode.as_str()) {
                return Err(ApiError::Unprocessable("Invalid repeat mode".into()));
            }
            stored.repeat_mode.clone_from(repeat_mode);
            if *shuffle && stored.shuffle_seed.is_none() {
                let mut seed = [0; 16];
                getrandom::fill(&mut seed).map_err(|e| ApiError::Internal(Box::new(e)))?;
                stored.shuffle_seed =
                    Some(seed.into_iter().fold(String::new(), |mut output, b| {
                        use std::fmt::Write as _;
                        let _ = write!(output, "{b:02x}");
                        output
                    }));
            }
            if !shuffle {
                stored.shuffle_seed = None;
            }
        }
        Action::Seek { seconds, entry_id } => {
            if stored.current_entry_id.map(|id| id.to_string()).as_ref() != Some(entry_id) {
                return Err(ApiError::Conflict("Current track changed".into()));
            }
            if !seconds.is_finite() || *seconds < 0.0 {
                return Err(ApiError::Unprocessable("Invalid playback position".into()));
            }
            let duration = visible
                .iter()
                .find(|p| {
                    Some(p.entry_id.as_str())
                        == stored.current_entry_id.map(|id| id.to_string()).as_deref()
                })
                .ok_or(ApiError::NotFound)?
                .duration;
            stored.position_seconds = playback_position(*seconds, duration);
        }
        _ => return Ok(false),
    }
    Ok(true)
}

async fn clear_queue(conn: &mut sqlx::PgConnection, stored: &mut Stored) -> Result<(), ApiError> {
    sqlx::query("DELETE FROM music_queue_entries WHERE queue_id=$1")
        .bind(stored.id)
        .execute(conn)
        .await
        .map_err(DbError::from)?;
    stored.current_entry_id = None;
    stored.position_seconds = 0.0;
    Ok(())
}

async fn mutate_sources(
    state: &AppState,
    conn: &mut sqlx::PgConnection,
    owner: i64,
    stored: &mut Stored,
    rows: &[Entry],
    action: &Action,
) -> Result<(), ApiError> {
    let parse_entry = |s: &str| {
        s.parse::<i64>()
            .ok()
            .filter(|id| rows.iter().any(|r| r.id == *id))
            .ok_or(ApiError::NotFound)
    };
    match action {
        Action::Play { sources } => play_sources(state, conn, owner, stored, sources).await?,
        Action::Add { sources } => {
            insert_sources(state, conn, owner, stored, sources, rows).await?;
        }
        Action::Collection {
            collection_id,
            replace,
        } => {
            let sources = collection_sources(state, owner, collection_id).await?;
            if *replace {
                clear_queue(conn, stored).await?;
            }
            insert_sources(
                state,
                conn,
                owner,
                stored,
                &sources,
                if *replace { &[] } else { rows },
            )
            .await?;
        }
        Action::Podcast {
            podcast_id,
            oldest,
            offset,
            replace,
        } => {
            let show = podcast_id.parse().map_err(|_| ApiError::NotFound)?;
            let sources =
                crate::music_podcasts::page_sources(state, show, owner, *oldest, *offset).await?;
            if *replace {
                clear_queue(conn, stored).await?;
            }
            insert_sources(
                state,
                conn,
                owner,
                stored,
                &sources,
                if *replace { &[] } else { rows },
            )
            .await?;
        }
        Action::Remove { entry_id } => {
            let entry = parse_entry(entry_id)?;
            sqlx::query("DELETE FROM music_queue_entries WHERE queue_id=$1 AND id=$2")
                .bind(stored.id)
                .bind(entry)
                .execute(&mut *conn)
                .await
                .map_err(DbError::from)?;
            if stored.current_entry_id == Some(entry) {
                stored.current_entry_id = None;
                stored.position_seconds = 0.0;
            }
        }
        Action::Order { entry_ids } => order(conn, stored.id, rows, entry_ids).await?,
        Action::Move {
            entry_id,
            direction,
        } => {
            let id = parse_entry(entry_id)?;
            let index = rows
                .iter()
                .position(|row| row.id == id)
                .ok_or(ApiError::NotFound)?;
            let next = match direction.as_str() {
                "up" => index.saturating_sub(1),
                "down" => (index + 1).min(rows.len() - 1),
                _ => return Err(ApiError::Unprocessable("Invalid queue direction".into())),
            };
            let mut ids = rows
                .iter()
                .map(|row| row.id.to_string())
                .collect::<Vec<_>>();
            ids.swap(index, next);
            order(conn, stored.id, rows, &ids).await?;
        }
        Action::Clear => {
            sqlx::query("DELETE FROM music_queue_entries WHERE queue_id=$1")
                .bind(stored.id)
                .execute(&mut *conn)
                .await
                .map_err(DbError::from)?;
            stored.current_entry_id = None;
            stored.position_seconds = 0.0;
        }
        _ => unreachable!("transport commands were handled"),
    }
    Ok(())
}
fn previous(stored: &mut Stored, visible: &[Playable]) {
    let index = visible.iter().position(|p| {
        Some(p.entry_id.as_str()) == stored.current_entry_id.map(|id| id.to_string()).as_deref()
    });
    let prev = match index {
        Some(i) if i > 0 => Some(i - 1),
        Some(0) if stored.repeat_mode == "all" => visible.len().checked_sub(1),
        _ => Some(0),
    };
    stored.current_entry_id = prev
        .and_then(|i| visible.get(i))
        .and_then(|p| p.entry_id.parse().ok());
    stored.position_seconds = 0.0;
}

async fn order(
    conn: &mut sqlx::PgConnection,
    queue: i64,
    rows: &[Entry],
    ids: &[String],
) -> Result<(), ApiError> {
    let order = ids
        .iter()
        .map(|s| s.parse::<i64>().map_err(|_| ApiError::NotFound))
        .collect::<Result<Vec<_>, _>>()?;
    let old = rows
        .iter()
        .map(|r| r.id)
        .collect::<std::collections::HashSet<_>>();
    if order.len() != rows.len()
        || order
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>()
            != old
    {
        return Err(ApiError::Unprocessable(
            "Include each queue entry exactly once".into(),
        ));
    }
    for (index, entry) in order.iter().enumerate() {
        sqlx::query("UPDATE music_queue_entries SET position=$3 WHERE id=$1 AND queue_id=$2")
            .bind(entry)
            .bind(queue)
            .bind(i32::try_from(index + 1).unwrap_or(1000))
            .execute(&mut *conn)
            .await
            .map_err(DbError::from)?;
    }
    Ok(())
}

#[derive(Debug, Serialize, FromRow)]
pub struct Device {
    pub device_key: String,
    #[serde(serialize_with = "crate::music_listening::string_revision")]
    pub revision: i64,
}
pub async fn devices(state: &AppState, owner: i64) -> Result<Vec<Device>, ApiError> {
    let mut conn = state.pool.acquire().await.map_err(DbError::from)?;
    active_owner(&mut conn, owner).await?;
    Ok(sqlx::query_as("SELECT device_key,revision FROM music_queues WHERE account_id=$1 ORDER BY updated_at DESC,id DESC LIMIT 32").bind(owner).fetch_all(&mut *conn).await.map_err(DbError::from)?)
}

/// A server playback output resolves only an entry from its owner's exact device.
pub(crate) async fn output_entry(
    state: &AppState,
    owner: i64,
    device: &str,
    entry: i64,
) -> Result<Entry, ApiError> {
    device_key(device)?;
    let mut conn = state.pool.acquire().await.map_err(DbError::from)?;
    active_owner(&mut conn, owner).await?;
    sqlx::query_as("SELECT e.id,e.queue_id AS collection_id,e.asset_id,e.status_id,e.episode_id,e.remote_id,e.position FROM music_queue_entries e JOIN music_queues q ON q.id=e.queue_id WHERE q.account_id=$1 AND q.device_key=$2 AND e.id=$3")
        .bind(owner).bind(device).bind(entry).fetch_optional(&mut *conn).await.map_err(DbError::from)?.ok_or(ApiError::NotFound)
}

async fn play_sources(
    state: &AppState,
    conn: &mut sqlx::PgConnection,
    owner: i64,
    stored: &mut Stored,
    sources: &[Source],
) -> Result<(), ApiError> {
    clear_queue(conn, stored).await?;
    insert_sources(state, conn, owner, stored, sources, &[]).await?;
    stored.current_entry_id = sqlx::query_scalar(
        "SELECT id FROM music_queue_entries WHERE queue_id=$1 ORDER BY position LIMIT 1",
    )
    .bind(stored.id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(DbError::from)?;
    stored.position_seconds = 0.0;
    Ok(())
}
