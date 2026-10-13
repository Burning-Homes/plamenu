//! Bounded polling watcher, durable scan generations and PCM indexing leases.
use super::{
    folders::Folder,
    fs::{Fingerprint, Root},
};
use crate::{AppState, error::ApiError, music_assets, music_queues};
use plamenu_db::{DbError, id};
use std::time::Duration;
use tokio::task::JoinHandle;
#[derive(sqlx::FromRow)]
struct Pending {
    id: i64,
    relative_path: String,
    device: i64,
    inode: i64,
    size_bytes: i64,
    modified_seconds: i64,
    modified_nanos: i64,
}
fn unavailable() -> ApiError {
    ApiError::Conflict("Music mount changed or is unavailable".into())
}
fn root(state: &AppState, folder: &Folder) -> Result<Root, ApiError> {
    let config = state
        .config
        .music_local_roots
        .iter()
        .find(|root| root.id == folder.root_id)
        .ok_or_else(unavailable)?;
    let root = Root::open(&config.path).map_err(|_| unavailable())?;
    let identity = root.identity().map_err(|_| unavailable())?;
    if i64::try_from(identity.0).ok() != Some(folder.root_device)
        || i64::try_from(identity.1).ok() != Some(folder.root_inode)
    {
        return Err(unavailable());
    }
    Ok(root)
}
pub async fn claim(state: &AppState) -> Result<Option<Folder>, ApiError> {
    let lease = id::next().to_string();
    Ok(sqlx::query_as("UPDATE music_local_folders SET state='scanning',scan_epoch=scan_epoch+1,lease=$1,lease_until=now()+interval '2 minutes',last_error=NULL WHERE id=(SELECT f.id FROM music_local_folders f JOIN accounts a ON a.id=f.owner_id JOIN users u ON u.account_id=a.id WHERE NOT EXISTS(SELECT 1 FROM music_local_mutations m WHERE m.folder_id=f.id) AND f.enabled AND f.next_scan_at<=now() AND (f.lease IS NULL OR f.lease_until<=now()) AND (f.state IN('queued','scanning','indexing') OR f.watch) AND a.deleted_at IS NULL AND a.suspended_at IS NULL AND u.approved AND u.confirmed_at IS NOT NULL ORDER BY f.next_scan_at,f.id FOR UPDATE OF f SKIP LOCKED LIMIT 1) RETURNING *")
        .bind(lease).fetch_optional(&state.pool).await.map_err(DbError::from)?)
}
async fn heartbeat(state: &AppState, folder: &Folder) -> Result<(), ApiError> {
    if state.shutdown.is_cancelled() {
        return Err(unavailable());
    }
    let count=sqlx::query("UPDATE music_local_folders SET lease_until=now()+interval '2 minutes' WHERE id=$1 AND enabled AND revision=$2 AND lease=$3 AND lease_until>now() AND EXISTS(SELECT 1 FROM accounts a JOIN users u ON u.account_id=a.id WHERE a.id=owner_id AND a.deleted_at IS NULL AND a.suspended_at IS NULL AND u.approved AND u.confirmed_at IS NOT NULL)")
        .bind(folder.id).bind(folder.revision).bind(&folder.lease).execute(&state.pool).await.map_err(DbError::from)?.rows_affected();
    if count != 1 {
        return Err(unavailable());
    }
    Ok(())
}
async fn scan_records(
    state: &AppState,
    folder: &Folder,
) -> Result<Vec<(String, Fingerprint)>, ApiError> {
    let scan_root = root(state, folder)?;
    let directory = folder.directory.clone();
    let choices = folder.options.0.clone();
    let mut scanning =
        tokio::task::spawn_blocking(move || scan_root.scan_with_options(&directory, &choices));
    let started = std::time::Instant::now();
    loop {
        tokio::select! {
            result=&mut scanning=>return result.map_err(|error|ApiError::Internal(Box::new(error)))?.map_err(|_|unavailable()),
            ()=tokio::time::sleep(Duration::from_secs(20))=>{
                if started.elapsed()>Duration::from_mins(10){return Err(unavailable());}
                heartbeat(state,folder).await?;
            },
            ()=state.shutdown.cancelled()=>return Err(unavailable()),
        }
    }
}

pub async fn process(state: &AppState, folder: &Folder) -> Result<(), ApiError> {
    let records = scan_records(state, folder).await?;
    reconcile(state, folder, &records).await?;
    let pending:Vec<Pending>=sqlx::query_as("SELECT * FROM music_local_files WHERE folder_id=$1 AND available AND state IN('queued','indexing') ORDER BY relative_path LIMIT 10000")
        .bind(folder.id).fetch_all(&state.pool).await.map_err(DbError::from)?;
    for file in pending {
        heartbeat(state, folder).await?;
        let work = index(state, folder, &file);
        tokio::pin!(work);
        let result = loop {
            tokio::select! {
                result=&mut work=>break result,
                ()=tokio::time::sleep(Duration::from_secs(20))=>heartbeat(state,folder).await?,
                ()=state.shutdown.cancelled()=>return Err(unavailable()),
            }
        };
        if result.is_err() {
            // Never expose absolute mount paths or decoder stderr to owners.
            sqlx::query("UPDATE music_local_files SET state='failed',last_error='Audio metadata or decoding failed',lease=NULL,lease_until=NULL WHERE id=$1 AND folder_id=$2 AND EXISTS(SELECT 1 FROM music_local_folders WHERE id=$2 AND lease=$3 AND revision=$4)")
                .bind(file.id).bind(folder.id).bind(&folder.lease).bind(folder.revision).execute(&state.pool).await.map_err(DbError::from)?;
        }
    }
    heartbeat(state, folder).await?;
    // Mounted library membership tracks current indexed files. Removing a
    // disappeared reference does not erase listening history or other libraries.
    sqlx::query("DELETE FROM music_collection_entries e USING music_assets a WHERE e.collection_id=$1 AND e.asset_id=a.id AND a.indexed_original AND NOT EXISTS(SELECT 1 FROM music_local_files p WHERE p.folder_id=$2 AND p.asset_id=a.id AND p.available AND p.state='ready') AND EXISTS(SELECT 1 FROM music_local_folders WHERE id=$2 AND lease=$3 AND revision=$4)")
        .bind(folder.collection_id).bind(folder.id).bind(&folder.lease).bind(folder.revision).execute(&state.pool).await.map_err(DbError::from)?;
    sqlx::query("UPDATE music_local_folders SET state='idle',lease=NULL,lease_until=NULL,last_scan_at=now(),next_scan_at=now()+interval '60 seconds',discovered=(SELECT count(*) FROM music_local_files WHERE folder_id=$1 AND available),indexed=(SELECT count(*) FROM music_local_files WHERE folder_id=$1 AND available AND state='ready'),failed=(SELECT count(*) FROM music_local_files WHERE folder_id=$1 AND available AND state='failed') WHERE id=$1 AND lease=$2 AND revision=$3")
        .bind(folder.id).bind(&folder.lease).bind(folder.revision).execute(&state.pool).await.map_err(DbError::from)?;
    Ok(())
}
async fn reconcile(
    state: &AppState,
    folder: &Folder,
    records: &[(String, Fingerprint)],
) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, folder.owner_id).await?;
    music_queues::active_owner(&mut tx, folder.owner_id).await?;
    fence(&mut tx, folder).await?;
    for (path, fp) in records {
        sqlx::query("INSERT INTO music_local_files(id,folder_id,owner_id,relative_path,device,inode,size_bytes,modified_seconds,modified_nanos,seen_epoch) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) ON CONFLICT(folder_id,relative_path) DO UPDATE SET device=EXCLUDED.device,inode=EXCLUDED.inode,size_bytes=EXCLUDED.size_bytes,modified_seconds=EXCLUDED.modified_seconds,modified_nanos=EXCLUDED.modified_nanos,seen_epoch=EXCLUDED.seen_epoch,available=true,state=CASE WHEN music_local_files.state='ignored' THEN 'ignored' WHEN (music_local_files.device,music_local_files.inode,music_local_files.size_bytes,music_local_files.modified_seconds,music_local_files.modified_nanos) IS DISTINCT FROM (EXCLUDED.device,EXCLUDED.inode,EXCLUDED.size_bytes,EXCLUDED.modified_seconds,EXCLUDED.modified_nanos) OR music_local_files.state='missing' THEN 'queued' ELSE music_local_files.state END,updated_at=now()")
            .bind(id::next()).bind(folder.id).bind(folder.owner_id).bind(path).bind(i64::try_from(fp.device).map_err(|_|unavailable())?).bind(i64::try_from(fp.inode).map_err(|_|unavailable())?).bind(i64::try_from(fp.bytes).map_err(|_|unavailable())?).bind(fp.modified_seconds).bind(fp.modified_nanos).bind(folder.scan_epoch).execute(&mut *tx).await.map_err(DbError::from)?;
    }
    sqlx::query("UPDATE music_local_files SET available=false,state=CASE WHEN state='ignored' THEN state ELSE 'missing' END,lease=NULL,lease_until=NULL WHERE folder_id=$1 AND seen_epoch<>$2")
        .bind(folder.id).bind(folder.scan_epoch).execute(&mut *tx).await.map_err(DbError::from)?;
    sqlx::query("UPDATE music_local_folders SET state='indexing' WHERE id=$1")
        .bind(folder.id)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
async fn fence(conn: &mut sqlx::PgConnection, folder: &Folder) -> Result<(), ApiError> {
    let found:Option<i64>=sqlx::query_scalar("SELECT id FROM music_local_folders WHERE id=$1 AND enabled AND revision=$2 AND lease=$3 AND lease_until>now() FOR UPDATE")
        .bind(folder.id).bind(folder.revision).bind(&folder.lease).fetch_optional(conn).await.map_err(DbError::from)?;
    found.ok_or_else(unavailable)?;
    Ok(())
}
async fn index(state: &AppState, folder: &Folder, file: &Pending) -> Result<(), ApiError> {
    let root = root(state, folder)?;
    let opened = root.file(&file.relative_path).map_err(|_| unavailable())?;
    let fp = opened.fingerprint;
    if i64::try_from(fp.device).ok() != Some(file.device)
        || i64::try_from(fp.inode).ok() != Some(file.inode)
        || i64::try_from(fp.bytes).ok() != Some(file.size_bytes)
        || fp.modified_seconds != file.modified_seconds
        || fp.modified_nanos != file.modified_nanos
    {
        return Err(unavailable());
    }
    let path = opened.process_path();
    let probe = crate::media_transcode::probe_music(&state.config, &path).await?;
    let name = std::path::Path::new(&file.relative_path)
        .file_stem()
        .and_then(|v| v.to_str())
        .unwrap_or("Untitled track");
    let description = music_assets::describe_import(probe, name)?;
    let hash = music_assets::fingerprint(state, &path, &description.properties).await?;
    if root
        .file(&file.relative_path)
        .map_err(|_| unavailable())?
        .fingerprint
        != fp
    {
        return Err(unavailable());
    }
    let (asset, fresh) = commit_index(state, folder, file, &description, &hash).await?;
    if fresh
        && description.has_artwork
        && crate::music_artworks::embedded::import(state, folder.owner_id, asset, &path)
            .await
            .is_err()
    {
        sqlx::query("UPDATE music_local_files SET last_error='Embedded artwork could not be read or stored' WHERE id=$1 AND asset_id=$2 AND state='ready'").bind(file.id).bind(asset).execute(&state.pool).await.map_err(DbError::from)?;
    }
    Ok(())
}
async fn commit_index(
    state: &AppState,
    folder: &Folder,
    file: &Pending,
    description: &music_assets::ImportDescription,
    hash: &str,
) -> Result<(i64, bool), ApiError> {
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, folder.owner_id).await?;
    music_queues::active_owner(&mut tx, folder.owner_id).await?;
    fence(&mut tx, folder).await?;
    let current:Option<i64>=sqlx::query_scalar("SELECT id FROM music_local_files WHERE id=$1 AND folder_id=$2 AND available AND state IN('queued','indexing') AND device=$3 AND inode=$4 AND size_bytes=$5 AND modified_seconds=$6 AND modified_nanos=$7 FOR UPDATE")
        .bind(file.id).bind(folder.id).bind(file.device).bind(file.inode).bind(file.size_bytes).bind(file.modified_seconds).bind(file.modified_nanos).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
    current.ok_or_else(unavailable)?;
    let duplicate: Option<i64> =
        sqlx::query_scalar("SELECT id FROM music_assets WHERE owner_id=$1 AND audio_hash=$2")
            .bind(folder.owner_id)
            .bind(hash)
            .fetch_optional(&mut *tx)
            .await
            .map_err(DbError::from)?;
    let mut updated = None;
    let fresh = duplicate.is_none();
    let asset = if let Some(existing) = duplicate {
        existing
    } else {
        let asset = id::next();
        let input = &description.input;
        let (artist, album) =
            music_assets::entities(&mut tx, folder.owner_id, &input.music).await?;
        sqlx::query("INSERT INTO music_assets(id,owner_id,title,artist_id,album_id,genre,track_number,disc_number,musicbrainz_id,original_file,stream_file,original_bytes,stream_bytes,audio_hash,duration,original_format,release_year,indexed_original,indexed_tags_revision) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,0,$13,$14,$15,$16,true,1)")
            .bind(asset).bind(folder.owner_id).bind(&input.title).bind(artist).bind(album).bind(&input.music.genre).bind(input.music.track_number).bind(input.music.disc_number).bind(&input.music.musicbrainz_id).bind(format!("music{asset}.orig")).bind(format!("music{asset}.mp3")).bind(file.size_bytes).bind(hash).bind(description.duration).bind(description.format).bind(input.music.release_year).execute(&mut *tx).await.map_err(DbError::from)?;
        asset
    };
    if !fresh {
        // The first current file is the metadata authority for equal audio.
        // A duplicate filename must not overwrite the primary track's title.
        let primary:bool=sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM music_local_files WHERE asset_id=$1 AND id<>$2 AND id<$2 AND available AND state='ready')").bind(asset).bind(file.id).fetch_one(&mut *tx).await.map_err(DbError::from)?;
        if primary {
            updated =
                super::sync::update(state, &mut tx, folder.owner_id, asset, description).await?;
        }
    }
    sqlx::query("UPDATE music_local_files SET asset_id=$2,original_format=$3,state='ready',last_error=NULL,updated_at=now() WHERE id=$1").bind(file.id).bind(asset).bind(description.format).execute(&mut *tx).await.map_err(DbError::from)?;
    let (present,position):(bool,i32)=sqlx::query_as("SELECT EXISTS(SELECT 1 FROM music_collection_entries WHERE collection_id=$1 AND asset_id=$2),COALESCE((SELECT max(position) FROM music_collection_entries WHERE collection_id=$1),0)").bind(folder.collection_id).bind(asset).fetch_one(&mut *tx).await.map_err(DbError::from)?;
    if !present && position < 10000 {
        sqlx::query("INSERT INTO music_collection_entries(id,collection_id,asset_id,position) VALUES($1,$2,$3,$4)").bind(id::next()).bind(folder.collection_id).bind(asset).bind(position+1).execute(&mut *tx).await.map_err(DbError::from)?;
        sqlx::query(
            "UPDATE music_collections SET revision=revision+1,updated_at=now() WHERE id=$1",
        )
        .bind(folder.collection_id)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    }
    tx.commit().await.map_err(DbError::from)?;
    crate::music_publications::publish_update(state, updated.as_ref()).await;
    Ok((asset, fresh))
}
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if state.shutdown.is_cancelled() {
                break;
            }
            let _ = super::journal::run_due(&state).await;
            match claim(&state).await {
                Ok(Some(folder)) => {
                    if process(&state, &folder).await.is_err() {
                        let _=sqlx::query("UPDATE music_local_folders SET state='failed',last_error='Music folder is unavailable or changed; check its mount and retry',lease=NULL,lease_until=NULL,next_scan_at=now()+interval '60 seconds' WHERE id=$1 AND lease=$2").bind(folder.id).bind(&folder.lease).execute(&state.pool).await;
                    }
                }
                Ok(None) | Err(_) => {
                    tokio::select! {()=state.shutdown.cancelled()=>break,()=tokio::time::sleep(Duration::from_secs(5))=>{}}
                }
            }
        }
    })
}
