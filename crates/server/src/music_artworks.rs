//! Protected music artwork, with crash-safe writes and cascade-safe cleanup.
pub(crate) mod embedded;
use crate::{
    AppState,
    error::ApiError,
    media_processing, music_assets, music_collections,
    music_entities::{self, Kind},
};
use plamenu_db::{DbError, id};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::FromRow;

pub const MAX_BYTES: usize = 8 * 1024 * 1024;
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    Assets,
    Artists,
    Albums,
    Collections,
}
impl Target {
    #[must_use]
    pub fn path(self) -> &'static str {
        match self {
            Self::Assets => "assets",
            Self::Artists => "artists",
            Self::Albums => "albums",
            Self::Collections => "collections",
        }
    }
    pub fn parse(value: &str) -> Result<Self, ApiError> {
        match value {
            "assets" => Ok(Self::Assets),
            "artists" => Ok(Self::Artists),
            "albums" => Ok(Self::Albums),
            "collections" => Ok(Self::Collections),
            _ => Err(ApiError::NotFound),
        }
    }
    fn entity_kind(self) -> Option<Kind> {
        match self {
            Self::Artists => Some(Kind::Artist),
            Self::Albums => Some(Kind::Album),
            _ => None,
        }
    }
}
#[derive(Debug, Clone, FromRow)]
pub struct Artwork {
    pub id: i64,
    pub owner_id: i64,
    pub target_kind: String,
    pub target_id: i64,
    pub slot: String,
    pub file_name: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub width: i32,
    pub height: i32,
}
impl Artwork {
    #[must_use]
    pub fn entity(&self) -> Value {
        json!({"id":self.id.to_string(),"url":format!("/music/artwork/{}",self.id),"slot":self.slot,"content_type":self.content_type,"width":self.width,"height":self.height})
    }
}
pub async fn authorize(
    state: &AppState,
    target: Target,
    entity: i64,
    viewer: Option<i64>,
) -> Result<(i64, i64), ApiError> {
    if let Some(kind) = target.entity_kind() {
        match music_entities::get(state, kind, entity, viewer).await {
            Ok(row) => return Ok((row.owner_id, row.revision)),
            Err(ApiError::NotFound) if kind == Kind::Album => {
                // A track can expose its album cover without exposing album notes.
                let track: Option<music_assets::Asset> = sqlx::query_as(
                    music_assets::asset_query!(" WHERE t.album_id=$1 ", " LIMIT 1"),
                )
                .bind(entity)
                .bind(viewer)
                .fetch_optional(&state.pool)
                .await
                .map_err(DbError::from)?;
                let track = track.ok_or(ApiError::NotFound)?;
                return Ok((track.owner_id, track.revision));
            }
            Err(error) => return Err(error),
        }
    }
    match target {
        Target::Assets => {
            let row = music_assets::get(state, entity, viewer).await?;
            Ok((row.owner_id, row.revision))
        }
        Target::Collections => {
            let row = music_collections::get(state, entity, viewer).await?;
            Ok((row.owner_id, row.revision))
        }
        Target::Artists | Target::Albums => unreachable!("entity kinds handled above"),
    }
}
pub async fn get(state: &AppState, artwork: i64, viewer: Option<i64>) -> Result<Artwork, ApiError> {
    let row: Artwork=sqlx::query_as("SELECT a.* FROM music_artworks a WHERE id=$1 AND NOT EXISTS(SELECT 1 FROM media_cleanup_jobs c WHERE c.file_name=a.file_name)").bind(artwork).fetch_optional(&state.pool).await.map_err(DbError::from)?.ok_or(ApiError::NotFound)?;
    authorize(
        state,
        Target::parse(&row.target_kind)?,
        row.target_id,
        viewer,
    )
    .await?;
    Ok(row)
}
pub async fn for_target(
    state: &AppState,
    target: Target,
    entity: i64,
    viewer: Option<i64>,
) -> Result<Vec<Artwork>, ApiError> {
    authorize(state, target, entity, viewer).await?;
    Ok(sqlx::query_as("SELECT a.* FROM music_artworks a WHERE target_kind=$1 AND target_id=$2 AND NOT EXISTS(SELECT 1 FROM media_cleanup_jobs c WHERE c.file_name=a.file_name) ORDER BY slot")
        .bind(target.path()).bind(entity).fetch_all(&state.pool).await.map_err(DbError::from)?)
}
fn revision(value: &str) -> Result<i64, ApiError> {
    value
        .parse::<i64>()
        .ok()
        .filter(|v| *v > 0)
        .ok_or_else(|| ApiError::Unprocessable("Music revision is required".into()))
}
fn validate_slot(slot: &str) -> Result<(), ApiError> {
    if ["image", "cover"].contains(&slot) {
        Ok(())
    } else {
        Err(ApiError::NotFound)
    }
}
async fn lock(
    conn: &mut sqlx::PgConnection,
    owner: i64,
    target: Target,
    entity: i64,
    expected: i64,
) -> Result<Vec<i64>, ApiError> {
    music_assets::lock_owner(conn, owner).await?;
    // Assets always precede reusable metadata in the lock order.
    let sources = if let Some(kind) = target.entity_kind() {
        music_entities::lock_sources(conn, owner, kind, entity).await?
    } else if target == Target::Assets {
        vec![entity]
    } else {
        Vec::new()
    };
    let query = match target {
        Target::Assets => {
            "SELECT revision FROM music_assets WHERE id=$1 AND owner_id=$2 FOR UPDATE"
        }
        Target::Artists => {
            "SELECT revision FROM music_artists WHERE id=$1 AND owner_id=$2 FOR UPDATE"
        }
        Target::Albums => {
            "SELECT revision FROM music_albums WHERE id=$1 AND owner_id=$2 FOR UPDATE"
        }
        Target::Collections => {
            "SELECT revision FROM music_collections WHERE id=$1 AND owner_id=$2 FOR UPDATE"
        }
    };
    let found: Option<i64> = sqlx::query_scalar(query)
        .bind(entity)
        .bind(owner)
        .fetch_optional(conn)
        .await
        .map_err(DbError::from)?;
    if found.ok_or(ApiError::NotFound)? != expected {
        return Err(ApiError::Conflict(
            "Music changed; reload before editing artwork".into(),
        ));
    }
    Ok(sources)
}
async fn bump(conn: &mut sqlx::PgConnection, target: Target, entity: i64) -> Result<(), ApiError> {
    let query = match target {
        Target::Assets => "UPDATE music_assets SET revision=revision+1 WHERE id=$1",
        Target::Artists => {
            "UPDATE music_artists SET revision=revision+1,updated_at=now() WHERE id=$1"
        }
        Target::Albums => {
            "UPDATE music_albums SET revision=revision+1,updated_at=now() WHERE id=$1"
        }
        Target::Collections => {
            "UPDATE music_collections SET revision=revision+1,updated_at=now() WHERE id=$1"
        }
    };
    sqlx::query(query)
        .bind(entity)
        .execute(conn)
        .await
        .map_err(DbError::from)?;
    Ok(())
}
/// Normalize bytes before storing; originals, SVG and metadata never become
/// public artwork. A committed reservation predates every physical write.
pub async fn upload(
    state: &AppState,
    owner: i64,
    target: Target,
    entity: i64,
    slot: &str,
    expected: &str,
    input: Vec<u8>,
) -> Result<Artwork, ApiError> {
    validate_slot(slot)?;
    let expected = revision(expected)?;
    if input.len() > MAX_BYTES {
        return Err(ApiError::Unprocessable(
            "Artwork must fit within 8 MiB".into(),
        ));
    }
    let (actual, current) = authorize(state, target, entity, Some(owner)).await?;
    if actual != owner {
        return Err(ApiError::NotFound);
    }
    if current != expected {
        return Err(ApiError::Conflict(
            "Music changed; reload before editing artwork".into(),
        ));
    }
    let settings = state.settings_cache.get(&state.pool).await?;
    let mut params =
        media_processing::EncodeParams::from_settings(&settings, &state.config.ffmpeg_path);
    params.max_image_bytes = params.max_image_bytes.min(MAX_BYTES);
    let image =
        media_processing::process_image_blocking(input, 1024.min(params.max_edge), params).await?;
    let artwork = id::next();
    let file = format!("musicart{artwork}.{}", image.extension);
    let bytes = i64::try_from(image.bytes.len()).map_err(|e| ApiError::Internal(Box::new(e)))?;
    music_assets::reserve(state, owner, std::slice::from_ref(&file), &[bytes]).await?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    // The cleanup worker cannot retire this journal during the put/commit.
    let reserved:Option<i64>=sqlx::query_scalar("SELECT id FROM media_cleanup_jobs WHERE file_name=$1 AND attempts=0 AND run_at>now() FOR UPDATE").bind(&file).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
    if reserved.is_none() {
        return Err(ApiError::Conflict(
            "Artwork upload was interrupted; retry".into(),
        ));
    }
    let sources = lock(&mut tx, owner, target, entity, expected).await?;
    state
        .media
        .put(&file, image.bytes)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    sqlx::query("DELETE FROM music_artworks WHERE target_kind=$1 AND target_id=$2 AND slot=$3")
        .bind(target.path())
        .bind(entity)
        .bind(slot)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    sqlx::query("INSERT INTO music_artworks(id,owner_id,asset_id,artist_id,album_id,collection_id,slot,file_name,content_type,size_bytes,width,height,cleanup_job_id) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)")
        .bind(artwork).bind(owner).bind((target==Target::Assets).then_some(entity)).bind((target==Target::Artists).then_some(entity)).bind((target==Target::Albums).then_some(entity)).bind((target==Target::Collections).then_some(entity))
        .bind(slot).bind(&file).bind(image.content_type).bind(bytes).bind(i32::try_from(image.width).unwrap_or(1024)).bind(i32::try_from(image.height).unwrap_or(1024)).bind(id::next()).execute(&mut *tx).await.map_err(DbError::from)?;
    sqlx::query("DELETE FROM media_cleanup_jobs WHERE file_name=$1")
        .bind(&file)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    bump(&mut tx, target, entity).await?;
    let edited = changed_conn(state, &mut tx, owner, target, entity, &sources).await?;
    tx.commit().await.map_err(DbError::from)?;
    for status in &edited {
        crate::music_publications::publish_update(state, Some(status)).await;
    }
    get(state, artwork, Some(owner)).await
}
pub async fn remove(
    state: &AppState,
    owner: i64,
    target: Target,
    entity: i64,
    slot: &str,
    expected: &str,
) -> Result<(), ApiError> {
    validate_slot(slot)?;
    let expected = revision(expected)?;
    let (actual, _) = authorize(state, target, entity, Some(owner)).await?;
    if actual != owner {
        return Err(ApiError::NotFound);
    }
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    let sources = lock(&mut tx, owner, target, entity, expected).await?;
    sqlx::query("DELETE FROM music_artworks WHERE target_kind=$1 AND target_id=$2 AND slot=$3")
        .bind(target.path())
        .bind(entity)
        .bind(slot)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    bump(&mut tx, target, entity).await?;
    let edited = changed_conn(state, &mut tx, owner, target, entity, &sources).await?;
    tx.commit().await.map_err(DbError::from)?;
    for status in &edited {
        crate::music_publications::publish_update(state, Some(status)).await;
    }
    Ok(())
}

pub(crate) async fn raw_image_conn(
    conn: &mut sqlx::PgConnection,
    target: Target,
    entity: i64,
) -> Result<Value, ApiError> {
    Ok(sqlx::query_scalar::<_,Value>("SELECT to_jsonb(a) FROM music_artworks a WHERE target_kind=$1 AND target_id=$2 AND NOT EXISTS(SELECT 1 FROM media_cleanup_jobs c WHERE c.file_name=a.file_name) ORDER BY CASE WHEN slot=$3 THEN 0 ELSE 1 END LIMIT 1")
        .bind(target.path()).bind(entity).bind(if target==Target::Albums || target==Target::Collections {"cover"} else {"image"}).fetch_optional(conn).await.map_err(DbError::from)?.unwrap_or(Value::Null))
}
#[must_use]
pub(crate) fn image(domain: &str, raw: &Value) -> Value {
    raw["id"].as_i64().filter(|id|*id>0).map_or(Value::Null,|id|json!({"type":"Image","url":format!("https://{domain}/music/artwork/{id}"),"mediaType":raw["content_type"],"width":raw["width"],"height":raw["height"]}))
}
async fn changed_conn(
    state: &AppState,
    conn: &mut sqlx::PgConnection,
    owner: i64,
    target: Target,
    entity: i64,
    sources: &[i64],
) -> Result<Vec<plamenu_db::status::Status>, ApiError> {
    let mut references = Vec::new();
    if let Some(kind) = target.entity_kind() {
        references.push((kind, entity));
        sqlx::query("UPDATE music_assets SET revision=revision+1 WHERE id=ANY($1) AND owner_id=$2")
            .bind(sources)
            .bind(owner)
            .execute(&mut *conn)
            .await
            .map_err(DbError::from)?;
        if kind == Kind::Artist {
            let albums:Vec<i64>=sqlx::query_scalar("UPDATE music_albums SET revision=revision+1,updated_at=now() WHERE owner_id=$1 AND artist_id=$2 RETURNING id").bind(owner).bind(entity).fetch_all(&mut *conn).await.map_err(DbError::from)?;
            references.extend(albums.into_iter().map(|id| (Kind::Album, id)));
        }
    }
    let mut edited = Vec::new();
    for source in sources {
        if let Some(status) =
            crate::music_publications::synchronize_conn(state, conn, owner, *source).await?
        {
            edited.push(status);
        }
    }
    if !references.is_empty() {
        music_entities::reconcile_conn(state, conn, owner, &references).await?;
    }
    if target == Target::Collections {
        let collection: music_collections::Collection =
            sqlx::query_as("SELECT * FROM music_collections WHERE id=$1 AND owner_id=$2")
                .bind(entity)
                .bind(owner)
                .fetch_one(&mut *conn)
                .await
                .map_err(DbError::from)?;
        crate::music_library_federation::changed_conn(state, conn, &collection, false).await?;
    }
    Ok(edited)
}
