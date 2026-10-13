//! Durable roll-forward: filesystem proof precedes an atomic database update.
mod database;
use super::fs::{Fingerprint, Root};
use crate::{AppState, error::ApiError, music_assets};
use plamenu_db::DbError;
use serde::{Deserialize, Serialize};
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Intent {
    pub root_id: String,
    pub mount: (u64, u64),
    pub parent: (u64, u64),
    pub source: String,
    pub target: String,
    pub before: Fingerprint,
    pub after: Option<Fingerprint>,
    pub operation: String,
}
#[derive(sqlx::FromRow)]
struct Row {
    folder_id: i64,
    file_id: i64,
    owner_id: i64,
    #[sqlx(try_from = "database::IntentRecord")]
    intent: sqlx::types::Json<Intent>,
    applied: bool,
}
fn conflict() -> ApiError {
    ApiError::Conflict("Source mutation awaits recovery; check the original mount and files".into())
}
fn root(state: &AppState, i: &Intent) -> Result<Root, ApiError> {
    if !matches!(i.operation.as_str(), "rename" | "delete" | "tags") {
        return Err(conflict());
    }
    let config = state
        .config
        .music_local_roots
        .iter()
        .find(|r| r.id == i.root_id)
        .ok_or_else(conflict)?;
    let root = Root::open(&config.path).map_err(|_| conflict())?;
    if root.identity().map_err(|_| conflict())? != i.mount
        || root.parent_identity(&i.source).map_err(|_| conflict())? != i.parent
    {
        return Err(conflict());
    }
    Ok(root)
}
fn fingerprint(root: &Root, path: &str) -> Result<Option<Fingerprint>, ApiError> {
    match root.file(path) {
        Ok(f) => Ok(Some(f.fingerprint)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(conflict()),
    }
}
fn move_to(root: &Root, from: &str, to: &str, fp: Fingerprint) -> Result<(), ApiError> {
    let name = to.rsplit('/').next().ok_or_else(conflict)?;
    let path = root.rename_file(from, name, fp).map_err(|_| conflict())?;
    if path != to {
        return Err(conflict());
    }
    Ok(())
}
/// Idempotence is determined by exact inodes, never by filename existence alone.
fn materialize(root: &Root, i: &Intent) -> Result<(), ApiError> {
    let source = fingerprint(root, &i.source)?;
    let target = fingerprint(root, &i.target)?;
    if i.operation == "tags" {
        let after = i.after.ok_or_else(conflict)?;
        if source == Some(after) && target == Some(i.before) {
            return Ok(());
        }
        if source != Some(i.before) || target != Some(after) {
            return Err(conflict());
        }
        root.exchange_files(&i.source, &i.target, i.before, after)
            .map_err(|_| conflict())
    } else {
        if source.is_none() && target == Some(i.before) {
            return Ok(());
        }
        if source != Some(i.before) || target.is_some() {
            return Err(conflict());
        }
        move_to(root, &i.source, &i.target, i.before)
    }
}
fn cleanup(root: &Root, i: &Intent) -> Result<(), ApiError> {
    if i.operation == "rename" {
        return Ok(());
    }
    if let Some(fp) = fingerprint(root, &i.target)? {
        if fp != i.before {
            return Err(conflict());
        }
        root.delete_file(&i.target, i.before)
            .map_err(|_| conflict())?;
    }
    Ok(())
}
/// When an index/account disappeared, undo a pending mutation without granting
/// permission to overwrite any new file. Applied operations only clean backups.
fn cancel_removed(root: &Root, i: &Intent) -> Result<(), ApiError> {
    let source = fingerprint(root, &i.source)?;
    let target = fingerprint(root, &i.target)?;
    if i.operation == "tags" {
        let after = i.after.ok_or_else(conflict)?;
        if source == Some(after) && target == Some(i.before) {
            root.exchange_files(&i.source, &i.target, after, i.before)
                .map_err(|_| conflict())?;
        }
        if fingerprint(root, &i.source)? != Some(i.before) {
            return Err(conflict());
        }
        if let Some(fp) = fingerprint(root, &i.target)? {
            if fp != after {
                return Err(conflict());
            }
            root.delete_file(&i.target, after).map_err(|_| conflict())?;
        }
    } else if source.is_none() && target == Some(i.before) {
        move_to(root, &i.target, &i.source, i.before)?;
    } else if source != Some(i.before) {
        return Err(conflict());
    }
    Ok(())
}
pub async fn recover(state: &AppState, id: i64) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    // Read owner before locking to preserve the native owner -> folder lock order.
    let owner: Option<i64> =
        sqlx::query_scalar("SELECT owner_id FROM music_local_mutations WHERE id=$1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(DbError::from)?;
    let Some(owner) = owner else {
        return Ok(());
    };
    music_assets::lock_owner(&mut tx, owner).await?;
    let row: Option<Row> =
        sqlx::query_as("SELECT * FROM music_local_mutations WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(DbError::from)?;
    let Some(row) = row else {
        return Ok(());
    };
    let i = &row.intent.0;
    let root = root(state, i)?;
    if !row.applied {
        let exists:Option<i64>=sqlx::query_scalar("SELECT p.id FROM music_local_files p JOIN music_local_folders f ON f.id=p.folder_id WHERE p.id=$1 AND p.folder_id=$2 AND p.owner_id=$3 FOR UPDATE OF f,p").bind(row.file_id).bind(row.folder_id).bind(row.owner_id).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
        if exists.is_none() {
            cancel_removed(&root, i)?;
            sqlx::query("DELETE FROM music_local_mutations WHERE id=$1")
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(DbError::from)?;
            tx.commit().await.map_err(DbError::from)?;
            return Ok(());
        }
        if let Err(error) = materialize(&root, i) {
            // A destination collision must not permanently lock an unchanged
            // source. Cancel only when the original still has its exact proof.
            if fingerprint(&root, &i.source)? == Some(i.before) {
                if i.operation == "tags"
                    && let Some(after) = i.after
                    && fingerprint(&root, &i.target)? == Some(after)
                {
                    root.delete_file(&i.target, after).map_err(|_| conflict())?;
                }
                sqlx::query("DELETE FROM music_local_mutations WHERE id=$1")
                    .bind(id)
                    .execute(&mut *tx)
                    .await
                    .map_err(DbError::from)?;
                tx.commit().await.map_err(DbError::from)?;
            }
            return Err(error);
        }
        match i.operation.as_str() {
            "rename" => {
                sqlx::query("UPDATE music_local_files SET relative_path=$2 WHERE id=$1")
                    .bind(row.file_id)
                    .bind(&i.target)
                    .execute(&mut *tx)
                    .await
                    .map_err(DbError::from)?;
            }
            "delete" => {
                sqlx::query(
                    "UPDATE music_local_files SET available=false,state='missing' WHERE id=$1",
                )
                .bind(row.file_id)
                .execute(&mut *tx)
                .await
                .map_err(DbError::from)?;
            }
            "tags" => {
                let fp = i.after.ok_or_else(conflict)?;
                sqlx::query("UPDATE music_local_files SET device=$2,inode=$3,size_bytes=$4,modified_seconds=$5,modified_nanos=$6 WHERE id=$1")
     .bind(row.file_id).bind(i64::try_from(fp.device).map_err(|_|conflict())?).bind(i64::try_from(fp.inode).map_err(|_|conflict())?).bind(i64::try_from(fp.bytes).map_err(|_|conflict())?).bind(fp.modified_seconds).bind(fp.modified_nanos).execute(&mut *tx).await.map_err(DbError::from)?;
                sqlx::query("UPDATE music_assets SET original_bytes=$2 WHERE indexed_original AND id=(SELECT asset_id FROM music_local_files WHERE id=$1)").bind(row.file_id).bind(i64::try_from(fp.bytes).map_err(|_|conflict())?).execute(&mut *tx).await.map_err(DbError::from)?;
            }
            _ => return Err(conflict()),
        }
        sqlx::query("UPDATE music_local_mutations SET applied=true WHERE id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
    }
    tx.commit().await.map_err(DbError::from)?;
    // Retain applied intent until backup cleanup succeeds, including on restart.
    cleanup(&root, i)?;
    sqlx::query("DELETE FROM music_local_mutations WHERE id=$1 AND applied")
        .bind(id)
        .execute(&state.pool)
        .await
        .map_err(DbError::from)?;
    sqlx::query("UPDATE music_local_folders SET state='queued',last_error=NULL,next_scan_at=now() WHERE id=$1 AND state='failed' AND NOT EXISTS(SELECT 1 FROM music_local_mutations m WHERE m.folder_id=$1)").bind(row.folder_id).execute(&state.pool).await.map_err(DbError::from)?;
    Ok(())
}
pub async fn run_due(state: &AppState) -> Result<(), ApiError> {
    let ids:Vec<i64>=sqlx::query_scalar("SELECT id FROM music_local_mutations WHERE next_attempt_at<=now() ORDER BY next_attempt_at,id LIMIT 32").fetch_all(&state.pool).await.map_err(DbError::from)?;
    for id in ids {
        if state.shutdown.is_cancelled() {
            break;
        }
        if recover(state, id).await.is_err() {
            sqlx::query("UPDATE music_local_folders SET state='failed',last_error='Source mutation awaits recovery; check the original mount and files' WHERE id=(SELECT folder_id FROM music_local_mutations WHERE id=$1)").bind(id).execute(&state.pool).await.map_err(DbError::from)?;
            sqlx::query("UPDATE music_local_mutations SET next_attempt_at=now()+interval '60 seconds' WHERE id=$1").bind(id).execute(&state.pool).await.map_err(DbError::from)?;
        }
    }
    Ok(())
}
pub(super) async fn ensure_idle(
    conn: &mut sqlx::PgConnection,
    folder: i64,
) -> Result<(), ApiError> {
    let pending: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM music_local_mutations WHERE folder_id=$1)")
            .bind(folder)
            .fetch_one(conn)
            .await
            .map_err(DbError::from)?;
    if pending {
        return Err(conflict());
    }
    Ok(())
}

pub(super) async fn record(
    conn: &mut sqlx::PgConnection,
    id: i64,
    folder: i64,
    file: i64,
    owner: i64,
    intent: &Intent,
) -> Result<(), DbError> {
    sqlx::query("INSERT INTO music_local_mutations(id,folder_id,file_id,owner_id,intent) VALUES($1,$2,$3,$4,$5)").bind(id).bind(folder).bind(file).bind(owner).bind(database::IntentRecord::from(intent)).execute(&mut *conn).await?;
    // Fence a previously claimed scanner before any source file changes.
    sqlx::query("UPDATE music_local_folders SET revision=revision+1,state='queued',lease=NULL,lease_until=NULL,next_scan_at=now() WHERE id=$1").bind(folder).execute(conn).await?;
    Ok(())
}
