//! Native HTTP output destinations retain transport state independently of tabs.
mod icy;
mod ogg;
pub mod pipeline;
pub mod playback;
mod source;
pub mod stream;
pub mod worker;
use crate::{AppState, error::ApiError, music_assets, music_queues, music_renditions};
use plamenu_db::{DbError, id};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
#[derive(Clone, sqlx::FromRow)]
pub struct Output {
    pub id: i64,
    pub owner_id: i64,
    pub mount: String,
    pub name: String,
    pub description: String,
    pub genre: String,
    pub format: String,
    pub bitrate: i32,
    pub sample_rate: i32,
    pub visibility: String,
    pub listen_token_hash: Option<String>,
    pub enabled: bool,
    pub transport: String,
    pub device: String,
    pub revision: i64,
    pub generation: i64,
    pub worker_lease: Option<i64>,
    pub lease_until: Option<OffsetDateTime>,
    pub current_entry_id: Option<i64>,
    pub position_seconds: f64,
    pub last_error: Option<String>,
    pub codec_header: Vec<u8>,
    pub now_playing: String,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}
impl Output {
    #[must_use]
    pub fn entity(&self, viewer: Option<i64>) -> Value {
        let mut value = json!({"id":self.id.to_string(),"name":self.name,"description":self.description,"genre":self.genre,
            "mount":self.mount,"format":self.format,"bitrate":self.bitrate,"sample_rate":self.sample_rate,
            "now_playing":self.now_playing,"visibility":self.visibility,"enabled":self.enabled,"transport":self.transport,
            "online":self.lease_until.is_some_and(|until|until>OffsetDateTime::now_utc()),
            "url":format!("/music/streams/{}",self.id),"stream_url":format!("/music/live/{}",self.mount)});
        if viewer == Some(self.owner_id) {
            value["revision"] = json!(self.revision.to_string());
            value["device"] = json!(self.device);
            value["position_seconds"] = json!(self.position_seconds);
            value["last_error"] = json!(self.last_error);
        }
        value
    }
}
#[derive(Deserialize)]
pub struct New {
    pub mount: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub genre: String,
    pub format: String,
    pub bitrate: Option<u32>,
    #[serde(default = "sample_rate")]
    pub sample_rate: i32,
    pub visibility: String,
}
fn sample_rate() -> i32 {
    44100
}
fn invalid() -> ApiError {
    ApiError::Unprocessable("Invalid HTTP stream settings".into())
}
async fn validate(state: &AppState, owner: i64, input: &New) -> Result<u32, ApiError> {
    if input.mount.is_empty()
        || input.mount.len() > 64
        || !input
            .mount
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        || !input.mount.as_bytes()[0].is_ascii_alphanumeric()
        || input.name.trim().is_empty()
        || input.name.chars().count() > 200
        || input.description.chars().count() > 4000
        || input.genre.chars().count() > 200
        || [&input.name, &input.description, &input.genre]
            .iter()
            .any(|text| text.contains('\0'))
        || !matches!(input.format.as_str(), "mp3" | "ogg" | "opus")
        || ![22050, 24000, 32000, 44100, 48000].contains(&input.sample_rate)
        || !matches!(input.visibility.as_str(), "public" | "private")
    {
        return Err(invalid());
    }
    Ok(
        music_renditions::negotiate(state, Some(owner), &input.format, input.bitrate)
            .await?
            .bitrate,
    )
}
#[derive(Serialize)]
pub struct Created {
    pub output: Value,
    pub listen_token: Option<String>,
}

pub async fn create(state: &AppState, owner: i64, input: &New) -> Result<Created, ApiError> {
    let bitrate = validate(state, owner, input).await?;
    let raw = (input.visibility == "private").then(crate::auth::generate_secret);
    let hash = raw.as_deref().map(crate::auth::hash_secret);
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, owner).await?;
    music_queues::active_owner(&mut tx, owner).await?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM music_outputs WHERE owner_id=$1")
        .bind(owner)
        .fetch_one(&mut *tx)
        .await
        .map_err(DbError::from)?;
    if count >= 20 {
        return Err(ApiError::Unprocessable("HTTP stream limit reached".into()));
    }
    let output_id = id::next();
    let output:Output=sqlx::query_as("INSERT INTO music_outputs(id,owner_id,mount,name,description,genre,format,bitrate,sample_rate,visibility,listen_token_hash,device) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) RETURNING *")
        .bind(output_id).bind(owner).bind(&input.mount).bind(input.name.trim()).bind(&input.description).bind(&input.genre)
        .bind(&input.format).bind(i32::try_from(bitrate).map_err(|_|invalid())?).bind(input.sample_rate).bind(&input.visibility).bind(hash)
        .bind(format!("output-{output_id}")).fetch_one(&mut *tx).await.map_err(|error|match error {
            sqlx::Error::Database(ref db) if db.is_unique_violation()=>ApiError::Conflict("This stream mount is already used".into()),
            error=>DbError::from(error).into(),
        })?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(Created {
        output: output.entity(Some(owner)),
        listen_token: raw,
    })
}
macro_rules! visible {
    ()=>{"a.domain IS NULL AND a.suspended_at IS NULL AND a.deleted_at IS NULL AND NOT EXISTS(SELECT 1 FROM users u WHERE u.account_id=a.id AND (NOT u.approved OR u.confirmed_at IS NULL)) AND NOT EXISTS(SELECT 1 FROM blocks b WHERE (b.account_id=$2 AND b.target_account_id=o.owner_id) OR (b.account_id=o.owner_id AND b.target_account_id=$2)) AND (o.owner_id=$2 OR o.visibility='public')"};
}
pub async fn get(state: &AppState, output: i64, viewer: Option<i64>) -> Result<Output, ApiError> {
    sqlx::query_as(concat!(
        "SELECT o.* FROM music_outputs o JOIN accounts a ON a.id=o.owner_id WHERE o.id=$1 AND ",
        visible!()
    ))
    .bind(output)
    .bind(viewer)
    .fetch_optional(&state.pool)
    .await
    .map_err(DbError::from)?
    .ok_or(ApiError::NotFound)
}
pub async fn list(
    state: &AppState,
    viewer: Option<i64>,
    mine: bool,
    before: Option<i64>,
    query: &str,
) -> Result<Vec<Output>, ApiError> {
    if query.chars().count() > 200
        || query.contains('\0')
        || before.is_some_and(|id| id <= 0)
        || (mine && viewer.is_none())
    {
        return Err(invalid());
    }
    Ok(sqlx::query_as(concat!("SELECT o.* FROM music_outputs o JOIN accounts a ON a.id=o.owner_id WHERE ($1::bigint IS NULL OR o.id<$1) AND ",visible!()," AND (($3 AND o.owner_id=$2) OR (NOT $3 AND o.visibility='public' AND a.silenced_at IS NULL)) AND ($4='' OR strpos(lower(o.name||' '||o.description||' '||o.genre),lower($4))>0) ORDER BY o.id DESC LIMIT 51"))
        .bind(before).bind(viewer).bind(mine).bind(query.trim()).fetch_all(&state.pool).await.map_err(DbError::from)?)
}

async fn lock(
    conn: &mut sqlx::PgConnection,
    owner: i64,
    output: i64,
    revision: i64,
) -> Result<Output, ApiError> {
    music_assets::lock_owner(conn, owner).await?;
    music_queues::active_owner(conn, owner).await?;
    let row: Output =
        sqlx::query_as("SELECT * FROM music_outputs WHERE id=$1 AND owner_id=$2 FOR UPDATE")
            .bind(output)
            .bind(owner)
            .fetch_optional(&mut *conn)
            .await
            .map_err(DbError::from)?
            .ok_or(ApiError::NotFound)?;
    if row.revision != revision {
        return Err(ApiError::Conflict(
            "HTTP stream changed; reload before editing".into(),
        ));
    }
    Ok(row)
}
pub async fn update(
    state: &AppState,
    owner: i64,
    output: i64,
    revision: i64,
    input: &New,
) -> Result<Created, ApiError> {
    let bitrate = validate(state, owner, input).await?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    let old = lock(&mut tx, owner, output, revision).await?;
    let raw = (input.visibility == "private" && old.visibility != "private")
        .then(crate::auth::generate_secret);
    let hash = if input.visibility == "public" {
        None
    } else {
        raw.as_deref()
            .map(crate::auth::hash_secret)
            .or(old.listen_token_hash)
    };
    let row:Output=sqlx::query_as("UPDATE music_outputs SET mount=$2,name=$3,description=$4,genre=$5,format=$6,bitrate=$7,sample_rate=$8,visibility=$9,listen_token_hash=$10,revision=revision+1,generation=generation+1,worker_lease=NULL,lease_until=NULL,last_error=NULL,updated_at=now() WHERE id=$1 RETURNING *")
        .bind(output).bind(&input.mount).bind(input.name.trim()).bind(&input.description).bind(&input.genre).bind(&input.format)
        .bind(i32::try_from(bitrate).map_err(|_|invalid())?).bind(input.sample_rate).bind(&input.visibility).bind(hash)
        .fetch_one(&mut *tx).await.map_err(|error|match error {
            sqlx::Error::Database(ref db) if db.is_unique_violation()=>ApiError::Conflict("This stream mount is already used".into()),
            error=>DbError::from(error).into(),
        })?;
    sqlx::query("DELETE FROM music_output_chunks WHERE output_id=$1")
        .bind(output)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(Created {
        output: row.entity(Some(owner)),
        listen_token: raw,
    })
}
pub async fn rotate_token(
    state: &AppState,
    owner: i64,
    output: i64,
    revision: i64,
) -> Result<Created, ApiError> {
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    let row = lock(&mut tx, owner, output, revision).await?;
    if row.visibility != "private" {
        return Err(invalid());
    }
    let raw = crate::auth::generate_secret();
    let row:Output=sqlx::query_as("UPDATE music_outputs SET listen_token_hash=$2,revision=revision+1,generation=generation+1,worker_lease=NULL,lease_until=NULL,updated_at=now() WHERE id=$1 RETURNING *")
        .bind(output).bind(crate::auth::hash_secret(&raw)).fetch_one(&mut *tx).await.map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(Created {
        output: row.entity(Some(owner)),
        listen_token: Some(raw),
    })
}
pub async fn delete(
    state: &AppState,
    owner: i64,
    output: i64,
    revision: i64,
) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    lock(&mut tx, owner, output, revision).await?;
    sqlx::query("DELETE FROM music_outputs WHERE id=$1")
        .bind(output)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}

/// A listener capability grants bytes only; it never becomes a user or queue token.
pub async fn authorize(
    state: &AppState,
    output: i64,
    token: Option<&str>,
) -> Result<Output, ApiError> {
    let row:Output=sqlx::query_as("SELECT o.* FROM music_outputs o JOIN accounts a ON a.id=o.owner_id WHERE o.id=$1 AND a.domain IS NULL AND a.deleted_at IS NULL AND a.suspended_at IS NULL AND NOT EXISTS(SELECT 1 FROM users u WHERE u.account_id=a.id AND (NOT u.approved OR u.confirmed_at IS NULL))")
        .bind(output).fetch_optional(&state.pool).await.map_err(DbError::from)?.ok_or(ApiError::NotFound)?;
    if !row.enabled
        || (row.visibility == "private"
            && row.listen_token_hash.as_deref() != token.map(crate::auth::hash_secret).as_deref())
    {
        return Err(ApiError::NotFound);
    }
    Ok(row)
}
#[derive(Deserialize)]
pub struct Command {
    pub revision: String,
    pub operation: String,
}
pub async fn command(
    state: &AppState,
    owner: i64,
    output: i64,
    input: &Command,
) -> Result<Output, ApiError> {
    let revision = input.revision.parse().map_err(|_| invalid())?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    let row = lock(&mut tx, owner, output, revision).await?;
    let (enabled, transport) = match input.operation.as_str() {
        "play" => (true, "playing"),
        "pause" => (row.enabled, "paused"),
        "stop" => (row.enabled, "stopped"),
        "enable" => (true, row.transport.as_str()),
        "disable" => (false, "paused"),
        _ => return Err(invalid()),
    };
    let updated:Output=sqlx::query_as("UPDATE music_outputs SET enabled=$2,transport=$3,revision=revision+1,generation=generation+1,worker_lease=NULL,lease_until=NULL,last_error=NULL,position_seconds=CASE WHEN $3='stopped' THEN 0 ELSE position_seconds END,updated_at=now() WHERE id=$1 RETURNING *")
        .bind(output).bind(enabled).bind(transport).fetch_one(&mut *tx).await.map_err(DbError::from)?;
    sqlx::query("DELETE FROM music_output_chunks WHERE output_id=$1")
        .bind(output)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    if input.operation == "stop" {
        sqlx::query("UPDATE music_queues SET position_seconds=0,revision=revision+1,updated_at=now() WHERE account_id=$1 AND device_key=$2")
            .bind(owner).bind(&row.device).execute(&mut *tx).await.map_err(DbError::from)?;
    }
    tx.commit().await.map_err(DbError::from)?;
    Ok(updated)
}
