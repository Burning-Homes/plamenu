//! Saved station configuration, independently authorized from playback outputs.
use crate::{AppState, error::ApiError, music_assets, music_queues};
use plamenu_db::{DbError, id};
use serde::Deserialize;
use serde_json::{Value, json};
use time::OffsetDateTime;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Radio {
    pub id: i64,
    pub owner_id: i64,
    pub name: String,
    pub description: String,
    pub config: String,
    pub visibility: String,
    pub revision: i64,
    pub updated_at: OffsetDateTime,
}
impl Radio {
    #[must_use]
    pub fn entity(&self, viewer: Option<i64>) -> Value {
        let mut value = json!({"id":self.id.to_string(),"name":self.name,
            "description":self.description,"visibility":self.visibility,
            "revision":self.revision.to_string(),"url":format!("/music/radios/{}",self.id)});
        if viewer == Some(self.owner_id) {
            value["owner_id"] = json!(self.owner_id.to_string());
            value["config"] = json!(self.config);
        }
        value
    }
}
#[derive(Debug, Deserialize)]
pub struct New {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub config: String,
    #[serde(default = "private")]
    pub visibility: String,
}
fn private() -> String {
    "private".into()
}
fn validate(input: &New) -> Result<(), ApiError> {
    if input.name.trim().is_empty()
        || input.name.chars().count() > 200
        || input.description.chars().count() > 4000
        || input.config.chars().count() > 50_000
        || [&input.name, &input.description, &input.config]
            .iter()
            .any(|value| value.contains('\0'))
        || !["private", "local", "public"].contains(&input.visibility.as_str())
    {
        return Err(ApiError::Unprocessable(
            "Invalid radio configuration".into(),
        ));
    }
    Ok(())
}
pub async fn create(state: &AppState, owner: i64, input: &New) -> Result<Radio, ApiError> {
    validate(input)?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, owner).await?;
    music_queues::active_owner(&mut tx, owner).await?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM music_radios WHERE owner_id=$1")
        .bind(owner)
        .fetch_one(&mut *tx)
        .await
        .map_err(DbError::from)?;
    if count >= 100 {
        return Err(ApiError::Unprocessable("Radio limit reached".into()));
    }
    let radio = sqlx::query_as("INSERT INTO music_radios(id,owner_id,name,description,config,visibility) VALUES($1,$2,$3,$4,$5,$6) RETURNING *")
        .bind(id::next()).bind(owner).bind(input.name.trim()).bind(&input.description)
        .bind(&input.config).bind(&input.visibility).fetch_one(&mut *tx).await.map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(radio)
}
macro_rules! visible {
    () => { "a.suspended_at IS NULL AND a.deleted_at IS NULL AND NOT EXISTS(SELECT 1 FROM users u WHERE u.account_id=a.id AND (NOT u.approved OR u.confirmed_at IS NULL)) AND (r.owner_id=$2 OR r.visibility='public' OR (r.visibility='local' AND EXISTS(SELECT 1 FROM accounts v WHERE v.id=$2 AND v.domain IS NULL)) OR EXISTS(SELECT 1 FROM music_radio_grants g WHERE g.radio_id=r.id AND g.account_id=$2)) AND NOT EXISTS(SELECT 1 FROM blocks b WHERE (b.account_id=$2 AND b.target_account_id=r.owner_id) OR (b.account_id=r.owner_id AND b.target_account_id=$2))" };
}
pub async fn get(state: &AppState, radio: i64, viewer: Option<i64>) -> Result<Radio, ApiError> {
    for_ids(state, &[radio], viewer)
        .await?
        .pop()
        .ok_or(ApiError::NotFound)
}
/// Direct and batched reads share the same current station permission rule.
pub async fn for_ids(
    state: &AppState,
    ids: &[i64],
    viewer: Option<i64>,
) -> Result<Vec<Radio>, ApiError> {
    for_ids_pool(&state.pool, ids, viewer).await
}
pub(crate) async fn for_ids_pool(
    pool: &sqlx::PgPool,
    ids: &[i64],
    viewer: Option<i64>,
) -> Result<Vec<Radio>, ApiError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    if ids.len() > 100 {
        return Err(ApiError::Unprocessable(
            "Music station batch is too large".into(),
        ));
    }
    Ok(sqlx::query_as(concat!(
        "SELECT r.* FROM music_radios r JOIN accounts a ON a.id=r.owner_id WHERE r.id=ANY($1) AND ",
        visible!()
    ))
    .bind(ids)
    .bind(viewer)
    .fetch_all(pool)
    .await
    .map_err(DbError::from)?)
}
pub async fn list(
    state: &AppState,
    viewer: Option<i64>,
    mine: bool,
    before: Option<i64>,
) -> Result<Vec<Radio>, ApiError> {
    search(state, viewer, mine, before, "").await
}
pub async fn search(
    state: &AppState,
    viewer: Option<i64>,
    mine: bool,
    before: Option<i64>,
    query: &str,
) -> Result<Vec<Radio>, ApiError> {
    if mine && viewer.is_none() {
        return Err(ApiError::NotFound);
    }
    let query = query.trim();
    if query.chars().count() > 200 || query.contains('\0') || before.is_some_and(|v| v <= 0) {
        return Err(ApiError::Unprocessable(
            "Invalid station search or cursor".into(),
        ));
    }
    Ok(sqlx::query_as(concat!("SELECT r.* FROM music_radios r JOIN accounts a ON a.id=r.owner_id WHERE ($1::bigint IS NULL OR r.id<$1) AND ",visible!()," AND (($3 AND r.owner_id=$2) OR (NOT $3 AND r.visibility='public' AND a.silenced_at IS NULL)) AND NOT EXISTS(SELECT 1 FROM mutes m WHERE m.account_id=$2 AND m.target_account_id=r.owner_id AND (m.expires_at IS NULL OR m.expires_at>now())) AND ($4='' OR strpos(lower(r.name || ' ' || r.description),lower($4))>0) ORDER BY r.id DESC LIMIT 51"))
        .bind(before).bind(viewer).bind(mine).bind(query).fetch_all(&state.pool).await.map_err(DbError::from)?)
}
async fn lock(
    conn: &mut sqlx::PgConnection,
    owner: i64,
    radio: i64,
    revision: i64,
) -> Result<Radio, ApiError> {
    music_assets::lock_owner(conn, owner).await?;
    music_queues::active_owner(conn, owner).await?;
    let current: Radio =
        sqlx::query_as("SELECT * FROM music_radios WHERE id=$1 AND owner_id=$2 FOR UPDATE")
            .bind(radio)
            .bind(owner)
            .fetch_optional(conn)
            .await
            .map_err(DbError::from)?
            .ok_or(ApiError::NotFound)?;
    if current.revision != revision {
        return Err(ApiError::Conflict(
            "Radio changed; reload before editing".into(),
        ));
    }
    Ok(current)
}
pub async fn update(
    state: &AppState,
    owner: i64,
    radio: i64,
    revision: i64,
    input: &New,
) -> Result<Radio, ApiError> {
    validate(input)?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    lock(&mut tx, owner, radio, revision).await?;
    let updated = sqlx::query_as("UPDATE music_radios SET name=$2,description=$3,config=$4,visibility=$5,revision=revision+1,updated_at=now() WHERE id=$1 RETURNING *")
        .bind(radio).bind(input.name.trim()).bind(&input.description).bind(&input.config).bind(&input.visibility)
        .fetch_one(&mut *tx).await.map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(updated)
}
pub async fn delete(
    state: &AppState,
    owner: i64,
    radio: i64,
    revision: i64,
) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    lock(&mut tx, owner, radio, revision).await?;
    sqlx::query("DELETE FROM music_radios WHERE id=$1")
        .bind(radio)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
pub async fn grant(
    state: &AppState,
    owner: i64,
    radio: i64,
    member: i64,
    enabled: bool,
) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, owner).await?;
    music_queues::active_owner(&mut tx, owner).await?;
    let permitted: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM music_radios WHERE id=$1 AND owner_id=$2)")
            .bind(radio)
            .bind(owner)
            .fetch_one(&mut *tx)
            .await
            .map_err(DbError::from)?;
    if !permitted {
        return Err(ApiError::NotFound);
    }
    if enabled {
        if !music_assets::lock_recipient(&mut tx, member).await? {
            return Err(ApiError::Unprocessable(
                "Choose an active local member".into(),
            ));
        }
        let inserted:Option<i64>=sqlx::query_scalar("INSERT INTO music_radio_grants(radio_id,account_id) VALUES($1,$2) ON CONFLICT DO NOTHING RETURNING id").bind(radio).bind(member).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
        if let Some(grant) = inserted {
            crate::music_shares::notify_grant(
                state,
                &mut tx,
                owner,
                member,
                crate::music_saved::Kind::Radio,
                grant,
            )
            .await?;
        }
    } else {
        sqlx::query("DELETE FROM music_radio_grants WHERE radio_id=$1 AND account_id=$2")
            .bind(radio)
            .bind(member)
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
    }
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
pub async fn grants(
    state: &AppState,
    owner: i64,
    radio: i64,
) -> Result<Vec<(i64, String)>, ApiError> {
    let item = get(state, radio, Some(owner)).await?;
    if item.owner_id != owner {
        return Err(ApiError::NotFound);
    }
    Ok(sqlx::query_as("SELECT a.id,a.username FROM music_radio_grants g JOIN accounts a ON a.id=g.account_id WHERE g.radio_id=$1 ORDER BY a.username,a.id LIMIT 1000")
        .bind(radio).fetch_all(&state.pool).await.map_err(DbError::from)?)
}
