//! Owner-only file references and explicit operator-enabled source mutations.
use super::{
    folders,
    fs::{Fingerprint, Root},
};
use crate::{AppState, error::ApiError, music_assets, music_queues};
use plamenu_db::DbError;
use serde::{Deserialize, Serialize};
#[derive(sqlx::FromRow, Serialize)]
pub struct File {
    #[serde(serialize_with = "super::serialize_id")]
    pub id: i64,
    #[serde(serialize_with = "super::serialize_optional_id")]
    pub asset_id: Option<i64>,
    pub relative_path: String,
    pub state: String,
    pub available: bool,
    pub last_error: Option<String>,
    #[serde(skip)]
    device: i64,
    #[serde(skip)]
    inode: i64,
    #[serde(skip)]
    size_bytes: i64,
    #[serde(skip)]
    modified_seconds: i64,
    #[serde(skip)]
    modified_nanos: i64,
}
impl File {
    pub(super) fn fingerprint(&self) -> Result<Fingerprint, ApiError> {
        Ok(Fingerprint {
            device: u64::try_from(self.device).map_err(|_| ApiError::NotFound)?,
            inode: u64::try_from(self.inode).map_err(|_| ApiError::NotFound)?,
            bytes: u64::try_from(self.size_bytes).map_err(|_| ApiError::NotFound)?,
            modified_seconds: self.modified_seconds,
            modified_nanos: self.modified_nanos,
        })
    }
}
pub async fn list(
    state: &AppState,
    owner: i64,
    folder: i64,
    before: Option<i64>,
) -> Result<Vec<File>, ApiError> {
    folders::get(state, owner, folder).await?;
    Ok(sqlx::query_as("SELECT * FROM music_local_files WHERE folder_id=$1 AND owner_id=$2 AND ($3::bigint IS NULL OR id<$3) ORDER BY id DESC LIMIT 61")
        .bind(folder).bind(owner).bind(before).fetch_all(&state.pool).await.map_err(DbError::from)?)
}
#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Mutation {
    Rename { revision: i64, name: String },
    WriteTags { revision: i64 },
    Delete { revision: i64, confirm: bool },
}
impl Mutation {
    fn revision(&self) -> i64 {
        match self {
            Self::Rename { revision, .. }
            | Self::WriteTags { revision }
            | Self::Delete { revision, .. } => *revision,
        }
    }
}
async fn resolve(
    state: &AppState,
    owner: i64,
    folder: i64,
    file: i64,
    input: &Mutation,
) -> Result<(File, Root, super::fs::Opened), ApiError> {
    let assignment = folders::get(state, owner, folder).await?;
    if !assignment.enabled || assignment.revision != input.revision() {
        return Err(ApiError::Conflict("Music folder changed; reload".into()));
    }
    let config = state
        .config
        .music_local_roots
        .iter()
        .find(|root| root.id == assignment.root_id)
        .ok_or(ApiError::NotFound)?;
    let allowed = match input {
        Mutation::Rename { .. } => config.allow_rename,
        Mutation::WriteTags { .. } => config.allow_tag_writeback,
        Mutation::Delete { confirm, .. } => config.allow_delete && *confirm,
    };
    if !allowed {
        return Err(ApiError::Forbidden(
            "This source mutation is not enabled by the operator".into(),
        ));
    }
    let row:File=sqlx::query_as("SELECT * FROM music_local_files WHERE id=$1 AND folder_id=$2 AND owner_id=$3 AND available AND state='ready'")
        .bind(file).bind(folder).bind(owner).fetch_optional(&state.pool).await.map_err(DbError::from)?.ok_or(ApiError::NotFound)?;
    let root = Root::open(&config.path).map_err(|_| ApiError::NotFound)?;
    let identity = root.identity().map_err(|_| ApiError::NotFound)?;
    if i64::try_from(identity.0).ok() != Some(assignment.root_device)
        || i64::try_from(identity.1).ok() != Some(assignment.root_inode)
    {
        return Err(ApiError::Conflict(
            "Music mount changed; reassign it".into(),
        ));
    }
    let opened = root
        .file(&row.relative_path)
        .map_err(|_| ApiError::NotFound)?;
    let expected = row.fingerprint()?;
    if opened.fingerprint != expected {
        return Err(ApiError::Conflict("Original changed; rescan first".into()));
    }
    Ok((row, root, opened))
}
pub async fn mutate(
    state: &AppState,
    owner: i64,
    folder: i64,
    file: i64,
    input: &Mutation,
) -> Result<(), ApiError> {
    let launcher =
        crate::media_limits::running_executable().map_err(|e| ApiError::Internal(Box::new(e)))?;
    mutate_with_launcher(state, owner, folder, file, input, &launcher).await
}
/// The explicit launcher permits actual bounded-encoder tests with the CLI.
#[doc(hidden)]
#[allow(
    clippy::too_many_lines,
    reason = "prepare and record one owner-fenced source intent before filesystem recovery"
)]
pub async fn mutate_with_launcher(
    state: &AppState,
    owner: i64,
    folder: i64,
    file: i64,
    input: &Mutation,
    launcher: &std::path::Path,
) -> Result<(), ApiError> {
    let (row, root, opened) = resolve(state, owner, folder, file, input).await?;
    let expected = row.fingerprint()?;
    let replacement = if matches!(input, Mutation::WriteTags { .. }) {
        let asset =
            music_assets::get(state, row.asset_id.ok_or(ApiError::NotFound)?, Some(owner)).await?;
        Some((
            super::tags::prepare(state, &root, &row.relative_path, &opened, &asset, launcher)
                .await?,
            asset.revision,
        ))
    } else {
        None
    };
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, owner).await?;
    music_queues::active_owner(&mut tx, owner).await?;
    let assignment:Option<folders::Folder>=sqlx::query_as("SELECT * FROM music_local_folders WHERE id=$1 AND owner_id=$2 AND enabled AND revision=$3 FOR UPDATE").bind(folder).bind(owner).bind(input.revision()).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
    let assignment =
        assignment.ok_or_else(|| ApiError::Conflict("Music folder changed; reload".into()))?;
    super::journal::ensure_idle(&mut tx, folder).await?;
    let current:File=sqlx::query_as("SELECT * FROM music_local_files WHERE id=$1 AND folder_id=$2 AND owner_id=$3 AND available AND state='ready' FOR UPDATE").bind(file).bind(folder).bind(owner).fetch_optional(&mut *tx).await.map_err(DbError::from)?.ok_or(ApiError::NotFound)?;
    if current.fingerprint()? != expected
        || root
            .file(&row.relative_path)
            .map_err(|_| ApiError::NotFound)?
            .fingerprint
            != expected
    {
        return Err(ApiError::Conflict("Original changed; rescan first".into()));
    }
    let prefix = row.relative_path.rsplit_once('/').map_or("", |(p, _)| p);
    let join = |name: &str| {
        if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}/{name}")
        }
    };
    let (operation, target, after) = match input {
        Mutation::Rename { name, .. } => {
            if name.is_empty()
                || name.len() > 255
                || name.contains(['/', '\\', '\0'])
                || matches!(name.as_str(), "." | "..")
                || name.starts_with(".plamenu-")
            {
                return Err(ApiError::Unprocessable("Invalid filename".into()));
            }
            let target = join(name);
            if target == row.relative_path {
                return Ok(());
            }
            let occupied:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM music_local_files WHERE folder_id=$1 AND relative_path=$2 AND id<>$3)").bind(folder).bind(&target).bind(file).fetch_one(&mut *tx).await.map_err(DbError::from)?;
            if occupied || !root.vacant(&target).map_err(|_| ApiError::NotFound)? {
                return Err(ApiError::Conflict(
                    "Filename is already occupied; choose another name".into(),
                ));
            }
            ("rename", target, None)
        }
        Mutation::Delete { .. } => (
            "delete",
            join(&format!(".plamenu-remove-{}", plamenu_db::id::next())),
            None,
        ),
        Mutation::WriteTags { .. } => {
            let (prepared, revision) = replacement.ok_or(ApiError::NotFound)?;
            let asset =
                music_assets::owned_conn(&mut tx, owner, row.asset_id.ok_or(ApiError::NotFound)?)
                    .await?;
            if asset.revision != revision {
                return Err(ApiError::Conflict(
                    "Track details changed; retry writeback".into(),
                ));
            }
            let (name, fp) = prepared
                .retain(expected)
                .map_err(|_| ApiError::Conflict("Cannot retain prepared source tags".into()))?;
            ("tags", join(&name), Some(fp))
        }
    };
    let intent = super::journal::Intent {
        root_id: assignment.root_id,
        mount: root.identity().map_err(|_| ApiError::NotFound)?,
        parent: root
            .parent_identity(&row.relative_path)
            .map_err(|_| ApiError::NotFound)?,
        source: row.relative_path,
        target,
        before: expected,
        after,
        operation: operation.into(),
    };
    let id = plamenu_db::id::next();
    let persisted = super::journal::record(&mut tx, id, folder, file, owner, &intent).await;
    if let Err(error) = persisted {
        if let Some(after) = intent.after {
            let _ = root.delete_file(&intent.target, after);
        }
        return Err(error.into());
    }
    tx.commit().await.map_err(DbError::from)?;
    super::journal::recover(state, id).await
}

/// Reindexing is an explicit undo of a tombstone; it never copies the original.
pub async fn restore(
    state: &AppState,
    owner: i64,
    folder: i64,
    file: i64,
    revision: i64,
) -> Result<(), ApiError> {
    let assignment = folders::get(state, owner, folder).await?;
    if !assignment.enabled || assignment.revision != revision {
        return Err(ApiError::Conflict("Music folder changed; reload".into()));
    }
    let config = state
        .config
        .music_local_roots
        .iter()
        .find(|root| root.id == assignment.root_id)
        .ok_or(ApiError::NotFound)?;
    let root = Root::open(&config.path).map_err(|_| ApiError::NotFound)?;
    let identity = root.identity().map_err(|_| ApiError::NotFound)?;
    if i64::try_from(identity.0).ok() != Some(assignment.root_device)
        || i64::try_from(identity.1).ok() != Some(assignment.root_inode)
    {
        return Err(ApiError::Conflict(
            "Music mount changed; reassign it".into(),
        ));
    }
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, owner).await?;
    music_queues::active_owner(&mut tx, owner).await?;
    let active:Option<i64>=sqlx::query_scalar("SELECT id FROM music_local_folders WHERE id=$1 AND owner_id=$2 AND revision=$3 AND enabled FOR UPDATE").bind(folder).bind(owner).bind(revision).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
    active.ok_or_else(|| ApiError::Conflict("Music folder changed; reload".into()))?;
    super::journal::ensure_idle(&mut tx, folder).await?;
    let path:Option<String>=sqlx::query_scalar("SELECT relative_path FROM music_local_files WHERE id=$1 AND folder_id=$2 AND owner_id=$3 AND state='ignored' FOR UPDATE").bind(file).bind(folder).bind(owner).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
    let path = path.ok_or(ApiError::NotFound)?;
    let fp = root
        .file(&path)
        .map_err(|_| ApiError::NotFound)?
        .fingerprint;
    sqlx::query("UPDATE music_local_files SET available=true,state='queued',device=$2,inode=$3,size_bytes=$4,modified_seconds=$5,modified_nanos=$6,last_error=NULL WHERE id=$1")
        .bind(file).bind(i64::try_from(fp.device).map_err(|_|ApiError::NotFound)?).bind(i64::try_from(fp.inode).map_err(|_|ApiError::NotFound)?).bind(i64::try_from(fp.bytes).map_err(|_|ApiError::NotFound)?).bind(fp.modified_seconds).bind(fp.modified_nanos).execute(&mut *tx).await.map_err(DbError::from)?;
    sqlx::query("UPDATE music_local_folders SET revision=revision+1,state='queued',lease=NULL,lease_until=NULL,next_scan_at=now() WHERE id=$1").bind(folder).execute(&mut *tx).await.map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
