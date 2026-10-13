//! Private mount assignments use operator roots and native library containers.
use crate::{AppState, error::ApiError, music_assets, music_collections, music_queues};
use plamenu_db::{DbError, id};
use serde::{Deserialize, Serialize};
#[derive(Clone, sqlx::FromRow, Serialize)]
pub struct Folder {
    #[serde(serialize_with = "super::serialize_id")]
    pub id: i64,
    #[serde(serialize_with = "super::serialize_id")]
    pub owner_id: i64,
    #[serde(serialize_with = "super::serialize_id")]
    pub collection_id: i64,
    pub root_id: String,
    pub directory: String,
    #[serde(skip)]
    pub root_device: i64,
    #[serde(skip)]
    pub root_inode: i64,
    pub enabled: bool,
    pub watch: bool,
    #[sqlx(try_from = "super::options::Options")]
    pub options: sqlx::types::Json<super::options::Options>,
    pub revision: i64,
    pub scan_epoch: i64,
    pub state: String,
    #[serde(skip)]
    pub lease: Option<String>,
    pub last_error: Option<String>,
    pub discovered: i64,
    pub indexed: i64,
    pub failed: i64,
}
#[derive(Deserialize)]
pub struct New {
    pub owner_id: i64,
    pub root_id: String,
    #[serde(default)]
    pub directory: String,
    pub name: String,
    #[serde(default = "watch_default")]
    pub watch: bool,
    #[serde(default)]
    pub options: super::options::Options,
}
fn watch_default() -> bool {
    true
}
pub async fn list(state: &AppState, owner: i64) -> Result<Vec<Folder>, ApiError> {
    Ok(sqlx::query_as(
        "SELECT * FROM music_local_folders WHERE owner_id=$1 ORDER BY id DESC LIMIT 32",
    )
    .bind(owner)
    .fetch_all(&state.pool)
    .await
    .map_err(DbError::from)?)
}
pub async fn get(state: &AppState, owner: i64, folder: i64) -> Result<Folder, ApiError> {
    sqlx::query_as("SELECT * FROM music_local_folders WHERE id=$1 AND owner_id=$2")
        .bind(folder)
        .bind(owner)
        .fetch_optional(&state.pool)
        .await
        .map_err(DbError::from)?
        .ok_or(ApiError::NotFound)
}
/// Callers must enforce operator permission before assigning a root to an owner.
#[cfg(unix)]
pub async fn assign(state: &AppState, input: &New) -> Result<Folder, ApiError> {
    input.options.validate()?;
    let config = state
        .config
        .music_local_roots
        .iter()
        .find(|root| root.id == input.root_id)
        .ok_or(ApiError::NotFound)?;
    let root = super::fs::Root::open(&config.path)
        .map_err(|_| ApiError::Unprocessable("Music root is unavailable".into()))?;
    // Validate the scoped directory from the held root before creating anything.
    root.check_directory(&input.directory)
        .map_err(|_| ApiError::Unprocessable("Invalid music folder".into()))?;
    let (device, inode) = root.identity().map_err(|_| ApiError::NotFound)?;
    let new = music_collections::New {
        name: input.name.clone(),
        description: String::new(),
        kind: "library".into(),
        visibility: "private".into(),
    };
    if new.name.trim().is_empty() || new.name.chars().count() > 200 || new.name.contains('\0') {
        return Err(ApiError::Unprocessable("Invalid library name".into()));
    }
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, input.owner_id).await?;
    music_queues::active_owner(&mut tx, input.owner_id).await?;
    let (folders,collections):(i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM music_local_folders WHERE owner_id=$1),(SELECT count(*) FROM music_collections WHERE owner_id=$1)").bind(input.owner_id).fetch_one(&mut *tx).await.map_err(DbError::from)?;
    if folders >= 32 || collections >= 100 {
        return Err(ApiError::Unprocessable(
            "Music folder or library limit reached".into(),
        ));
    }
    let collection = id::next();
    sqlx::query("INSERT INTO music_collections(id,owner_id,name,kind) VALUES($1,$2,$3,'library')")
        .bind(collection)
        .bind(input.owner_id)
        .bind(new.name.trim())
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    let row=sqlx::query_as("INSERT INTO music_local_folders(id,owner_id,collection_id,root_id,directory,root_device,root_inode,watch,options) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9) RETURNING *")
        .bind(id::next()).bind(input.owner_id).bind(collection).bind(&input.root_id).bind(&input.directory).bind(i64::try_from(device).map_err(|_|ApiError::NotFound)?).bind(i64::try_from(inode).map_err(|_|ApiError::NotFound)?).bind(input.watch).bind(&input.options).fetch_one(&mut *tx).await.map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(row)
}
pub async fn refresh(
    state: &AppState,
    owner: i64,
    folder: i64,
    revision: i64,
    watch: bool,
    enabled: bool,
) -> Result<Folder, ApiError> {
    configure(state, owner, folder, revision, watch, enabled, None).await
}
pub async fn configure(
    state: &AppState,
    owner: i64,
    folder: i64,
    revision: i64,
    watch: bool,
    enabled: bool,
    options: Option<&super::options::Options>,
) -> Result<Folder, ApiError> {
    if let Some(options) = options {
        options.validate()?;
    }
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, owner).await?;
    music_queues::active_owner(&mut tx, owner).await?;
    super::journal::ensure_idle(&mut tx, folder).await?;
    let row=sqlx::query_as("UPDATE music_local_folders SET watch=$4,enabled=$5,options=COALESCE($6,options),revision=revision+1,state='queued',next_scan_at=now(),lease=NULL,lease_until=NULL WHERE id=$1 AND owner_id=$2 AND revision=$3 RETURNING *")
        .bind(folder).bind(owner).bind(revision).bind(watch).bind(enabled).bind(options).fetch_optional(&mut *tx).await.map_err(DbError::from)?.ok_or_else(||ApiError::Conflict("Music folder changed; reload".into()))?;
    sqlx::query("UPDATE music_local_files SET state='queued',last_error=NULL WHERE folder_id=$1 AND owner_id=$2 AND state='failed'").bind(folder).bind(owner).execute(&mut *tx).await.map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(row)
}

/// Disconnect only removes the native library and its mounted references.
pub async fn disconnect(
    state: &AppState,
    owner: i64,
    folder: i64,
    revision: i64,
) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, owner).await?;
    music_queues::active_owner(&mut tx, owner).await?;
    let current: Option<Folder> = sqlx::query_as(
        "SELECT * FROM music_local_folders WHERE id=$1 AND owner_id=$2 AND revision=$3 FOR UPDATE",
    )
    .bind(folder)
    .bind(owner)
    .bind(revision)
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::from)?;
    let current =
        current.ok_or_else(|| ApiError::Conflict("Music folder changed; reload".into()))?;
    let collection = music_collections::get(state, current.collection_id, Some(owner)).await?;
    crate::music_library_federation::changed_conn(state, &mut tx, &collection, true).await?;
    sqlx::query("DELETE FROM music_collections WHERE id=$1 AND owner_id=$2")
        .bind(collection.id)
        .bind(owner)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
