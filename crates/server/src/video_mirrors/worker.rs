//! One leased native mirror copier. No encoder or companion service is used.
use super::{authorized, manifest, withdraw};
use crate::{AppState, error::ApiError};
use plamenu_db::{id, video_mirrors as db};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::{io::AsyncReadExt, task::JoinHandle};

#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut maintenance_cursor = 0;
        loop {
            if let Err(error) = maintain(&state, &mut maintenance_cursor).await {
                tracing::warn!(%error,"mirror maintenance failed");
            }
            if let Some(config) = state.config.video_redundancy {
                let quota =
                    i64::try_from(config.quota_gib * 1024 * 1024 * 1024).unwrap_or(i64::MAX);
                match db::claim(&state.pool, quota).await {
                    Ok(Some(item)) => {
                        let result =
                            tokio::time::timeout(Duration::from_mins(15), copy(&state, &item))
                                .await;
                        if !matches!(result, Ok(Ok(()))) {
                            tracing::warn!(
                                mirror = item.id,
                                "mirror copy stopped; private retirement remains charged"
                            );
                            if let Err(error) = fail(&state, &item).await {
                                tracing::warn!(%error,"mirror failure retirement failed");
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(error) => tracing::warn!(%error,"mirror claim failed"),
                }
            }
            if !crate::workers::pause(&state, Duration::from_secs(2)).await {
                break;
            }
        }
    })
}

async fn maintain(state: &AppState, cursor: &mut i64) -> Result<(), ApiError> {
    let items=sqlx::query_as::<_,db::Mirror>("SELECT * FROM video_mirrors WHERE state IN ('queued','copying','ready') AND id>$1 ORDER BY id LIMIT 100")
        .bind(*cursor)
        .fetch_all(&state.pool).await.map_err(plamenu_db::DbError::from)?;
    *cursor = items.last().map_or(0, |item| item.id);
    for item in items {
        if !authorized(state, &item).await? {
            withdraw(state, item.id).await?;
        }
    }
    Ok(())
}

async fn fail(state: &AppState, item: &db::Mirror) -> Result<(), ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let changed=sqlx::query("UPDATE video_mirrors SET state='failed',lease_until=NULL,last_error='Copy failed, expired or lost current authority',updated_at=clock_timestamp()
        WHERE id=$1 AND generation=$2 AND state='copying'")
        .bind(item.id).bind(item.generation).execute(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
    if changed.rows_affected() == 1 {
        sqlx::query("DELETE FROM video_mirror_assets WHERE mirror_id=$1 AND generation=$2")
            .bind(item.id)
            .bind(item.generation)
            .execute(&mut *tx)
            .await
            .map_err(plamenu_db::DbError::from)?;
        sqlx::query("UPDATE media_cleanup_jobs SET run_at=clock_timestamp() WHERE mirror_id=$1 AND mirror_generation=$2")
            .bind(item.id).bind(item.generation).execute(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

#[derive(Clone)]
struct Dependency {
    url: String,
    playlist: bool,
    name: String,
}

fn name(url: &str, playlist: bool) -> Result<String, ApiError> {
    if playlist {
        return Ok(format!(
            "p{}.m3u8",
            digest_hex(Sha256::digest(url.as_bytes()).as_slice())
        ));
    }
    let parsed = super::https(url)?;
    let name = parsed.path().rsplit('/').next().unwrap_or_default();
    if !manifest::valid_name(name) {
        return Err(ApiError::BadRequest(
            "Unsupported media basename in mirror".into(),
        ));
    }
    Ok(name.to_owned())
}

pub(super) fn digest_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    output
}

fn content_type(url: &str, playlist: bool) -> &'static str {
    if playlist {
        return "application/vnd.apple.mpegurl";
    }
    match url.split('?').next().unwrap_or_default().rsplit('.').next() {
        Some("vtt") => "text/vtt",
        Some("aac") => "audio/aac",
        Some("ts") => "video/mp2t",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one finite breadth-first copy with a single durable publication boundary"
)]
async fn copy(state: &AppState, item: &db::Mirror) -> Result<(), ApiError> {
    if !authorized(state, item).await? {
        return Err(ApiError::NotFound);
    }
    let config = state.config.video_redundancy.ok_or(ApiError::NotFound)?;
    let max = config.max_mirror_mib * 1024 * 1024;
    let permit = Arc::new(
        state
            .video_resources
            .try_reserve_temporary(max * 2)
            .ok_or_else(|| ApiError::ServiceUnavailable("Mirror scratch budget is busy".into()))?,
    );
    let scratch = Arc::new(
        tempfile::Builder::new()
            .prefix(".plamenu-mirror-copy-")
            .tempdir_in(&state.config.media_dir)
            .map_err(|error| ApiError::Internal(Box::new(error)))?,
    );
    let mut pending = VecDeque::from([Dependency {
        url: item.source_master_url.clone(),
        playlist: true,
        name: "master.m3u8".into(),
    }]);
    let mut seen = HashMap::new();
    seen.insert(item.source_master_url.clone(), true);
    let mut public_names = HashSet::from(["master.m3u8".to_owned()]);
    // The client API's progressive compatibility file may be distinct from
    // the adaptive ladder. Keep it too so a non-JS reader can survive source
    // loss without silently becoming dependent on a JavaScript HLS player.
    if let Some(attachment) = plamenu_db::media::find_by_ids(&state.pool, &[item.media_id])
        .await?
        .into_iter()
        .next()
    {
        for url in [attachment.remote_url, attachment.remote_audio_url]
            .into_iter()
            .flatten()
        {
            if super::https(&url)?.origin() != super::https(&item.source_master_url)?.origin() {
                return Err(ApiError::NotFound);
            }
            let public = name(&url, false)?;
            if seen.insert(url.clone(), false).is_some() {
                continue;
            }
            if !public_names.insert(public.clone()) {
                return Err(ApiError::NotFound);
            }
            pending.push_back(Dependency {
                url,
                playlist: false,
                name: public,
            });
        }
    }
    if let Some(peers) = plamenu_db::remote_hls_peers::for_media(&state.pool, &[item.media_id])
        .await?
        .remove(&item.media_id)
    {
        let checksum = super::https(&peers.sha256_url)?;
        if checksum.origin() != super::https(&item.source_master_url)?.origin() {
            return Err(ApiError::NotFound);
        }
        let public = name(&peers.sha256_url, false)?;
        if !public_names.insert(public.clone()) {
            return Err(ApiError::NotFound);
        }
        seen.insert(peers.sha256_url.clone(), false);
        pending.push_back(Dependency {
            url: peers.sha256_url,
            playlist: false,
            name: public,
        });
    }
    let mut total = 0_u64;
    let mut playlists = 0;
    let mut media_files = 0;
    while let Some(dependency) = pending.pop_front() {
        if !authorized(state, item).await? || state.shutdown.is_cancelled() {
            return Err(ApiError::NotFound);
        }
        let file = tempfile::NamedTempFile::new_in(scratch.path())
            .map_err(|error| ApiError::Internal(Box::new(error)))?;
        let remaining = max
            .checked_sub(total)
            .filter(|bytes| *bytes > 0)
            .ok_or(ApiError::PayloadTooLarge)?;
        let limit = if dependency.playlist {
            remaining.min(manifest::MAX_PLAYLIST_BYTES as u64)
        } else {
            remaining
        };
        let download_path = file.path().to_owned();
        let download = state.federation.fetch_mirror_to_file(
            &dependency.url,
            &download_path,
            limit,
            config.bandwidth_mib_per_second * 1024 * 1024,
        );
        tokio::pin!(download);
        let fetched = loop {
            tokio::select! {
                result=&mut download=>break result.map_err(|error|ApiError::Internal(Box::new(error)))?,
                ()=tokio::time::sleep(Duration::from_secs(2))=>{
                    if !authorized(state,item).await? || state.shutdown.is_cancelled() {return Err(ApiError::NotFound);}
                }
            }
        };
        if fetched.bytes_written == 0 {
            return Err(ApiError::NotFound);
        }
        total = total
            .checked_add(fetched.bytes_written)
            .filter(|bytes| *bytes <= max)
            .ok_or(ApiError::PayloadTooLarge)?;
        if dependency.playlist {
            playlists += 1;
            if playlists > 64 {
                return Err(ApiError::PayloadTooLarge);
            }
            let text = tokio::fs::read_to_string(file.path())
                .await
                .map_err(|error| ApiError::Internal(Box::new(error)))?;
            let base = super::https(&dependency.url)?;
            // Keep original URI resolution for canonical fallback. Reject
            // directory-changing redirects rather than guessing child paths.
            if super::https(&fetched.final_url)?.join(".").ok() != base.join(".").ok() {
                return Err(ApiError::BadRequest(
                    "Mirror playlist changed its canonical directory".into(),
                ));
            }
            for reference in manifest::references(&base, &text)
                .map_err(|error| ApiError::BadRequest(error.into()))?
            {
                // The approved origin owns this entire immutable graph. Do
                // not let one approved publisher turn a mirror into a crawler
                // of unapproved cross-origin/CDN dependencies.
                if super::https(&reference.url)?.origin()
                    != super::https(&item.source_master_url)?.origin()
                {
                    return Err(ApiError::Forbidden("Cross-origin mirror dependencies require a separately supported source layout".into()));
                }
                if let Some(existing) = seen.get(&reference.url) {
                    if *existing != reference.playlist {
                        return Err(ApiError::BadRequest(
                            "Conflicting HLS dependency types".into(),
                        ));
                    }
                    continue;
                }
                if seen.len() >= manifest::MAX_REFERENCES {
                    return Err(ApiError::PayloadTooLarge);
                }
                let public = name(&reference.url, reference.playlist)?;
                if !public_names.insert(public.clone()) {
                    return Err(ApiError::BadRequest(
                        "Conflicting media basenames in mirror".into(),
                    ));
                }
                seen.insert(reference.url.clone(), reference.playlist);
                pending.push_back(Dependency {
                    url: reference.url,
                    playlist: reference.playlist,
                    name: public,
                });
            }
        } else {
            media_files += 1;
        }
        store(
            state,
            item,
            &dependency,
            file,
            fetched.bytes_written,
            Arc::clone(&scratch),
            Arc::clone(&permit),
        )
        .await?;
    }
    if media_files == 0 {
        return Err(ApiError::NotFound);
    }
    publish(state, item, total).await
}

async fn store(
    state: &AppState,
    item: &db::Mirror,
    dependency: &Dependency,
    input: tempfile::NamedTempFile,
    bytes: u64,
    scratch: Arc<tempfile::TempDir>,
    permit: Arc<crate::video_resources::TemporaryReservation>,
) -> Result<(), ApiError> {
    let mut file = tokio::fs::File::open(input.path())
        .await
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .await
            .map_err(|error| ApiError::Internal(Box::new(error)))?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    let hash = digest_hex(hash.finalize().as_slice());
    let state = state.clone();
    let item = item.clone();
    let dependency = dependency.clone();
    // Cancellation of the caller must not cancel a filesystem writer and
    // allow cleanup to race it. This owned finalizer retains its input, scratch
    // reservation and cleanup-row lock until the write/commit actually ends.
    let task = tokio::spawn(async move {
        let _scratch = scratch;
        let _permit = permit;
        let key = format!("mirror{}.bin", crate::media_processing::storage_stem(0));
        let journal = id::next();
        let bytes = i64::try_from(bytes).map_err(|_| ApiError::PayloadTooLarge)?;
        sqlx::query("INSERT INTO media_cleanup_jobs(id,file_name,mirror_bytes,mirror_id,mirror_generation,run_at)
        VALUES($1,$2,$3,$4,$5,clock_timestamp()+interval '1 hour')")
        .bind(journal).bind(&key).bind(bytes).bind(item.id).bind(item.generation).execute(&state.pool).await.map_err(plamenu_db::DbError::from)?;
        let mut tx = state
            .pool
            .begin()
            .await
            .map_err(plamenu_db::DbError::from)?;
        sqlx::query("SELECT id FROM media_cleanup_jobs WHERE id=$1 FOR UPDATE")
            .bind(journal)
            .fetch_one(&mut *tx)
            .await
            .map_err(plamenu_db::DbError::from)?;
        let current=sqlx::query_scalar::<_,i64>("SELECT id FROM video_mirrors WHERE id=$1 AND generation=$2 AND state='copying' AND lease_until>clock_timestamp() AND expires_at>clock_timestamp() FOR UPDATE")
        .bind(item.id).bind(item.generation).fetch_optional(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
        if current.is_none() || !super::authorized_conn(&state, &mut tx, &item).await? {
            return Err(ApiError::NotFound);
        }
        state
            .media
            .put_file(&key, input.path())
            .await
            .map_err(|error| ApiError::Internal(Box::new(error)))?;
        sqlx::query("INSERT INTO video_mirror_assets(id,mirror_id,generation,source_url,public_name,file_name,bytes,content_type,sha256)
        VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
        .bind(id::next()).bind(item.id).bind(item.generation).bind(&dependency.url).bind(&dependency.name).bind(&key)
        .bind(bytes).bind(content_type(&dependency.url,dependency.playlist)).bind(hash).execute(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
        sqlx::query("DELETE FROM media_cleanup_jobs WHERE id=$1")
            .bind(journal)
            .execute(&mut *tx)
            .await
            .map_err(plamenu_db::DbError::from)?;
        tx.commit().await.map_err(plamenu_db::DbError::from)?;
        Ok::<_, ApiError>(())
    });
    task.await
        .map_err(|error| ApiError::Internal(Box::new(error)))?
}

async fn publish(state: &AppState, item: &db::Mirror, bytes: u64) -> Result<(), ApiError> {
    let post = plamenu_db::status::find_by_id(&state.pool, item.status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let target = plamenu_db::account::find_by_id(&state.pool, post.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let uri = format!("https://{}/cachefiles/{}", state.config.domain, item.id);
    let mut offer = serde_json::to_value(
        plamenu_ap::cache_file::CacheFile::new(
            uri.clone(),
            item.source_uri.clone(),
            format!("https://{}/media/mirrors/{}/", state.config.domain, item.id),
            item.expires_at,
        )
        .map_err(|error| ApiError::Internal(Box::new(error)))?,
    )
    .map_err(|error| ApiError::Internal(Box::new(error)))?;
    offer[super::CANONICAL_PLAYLISTS] = serde_json::json!(true);
    let create = serde_json::json!({"@context":super::cache_context(),"id":format!("{uri}/create"),"type":"Create",
        "actor":format!("https://{}/actor",state.config.domain),"object":offer});
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let current=sqlx::query_scalar::<_,i64>("SELECT id FROM video_mirrors WHERE id=$1 AND generation=$2 AND state='copying' AND lease_until>clock_timestamp() AND expires_at>clock_timestamp() FOR UPDATE")
        .bind(item.id).bind(item.generation).fetch_optional(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
    if current.is_none() || !super::authorized_conn(state, &mut tx, item).await? {
        return Err(ApiError::NotFound);
    }
    sqlx::query("UPDATE video_mirrors SET state='ready',reserved_bytes=$3,lease_until=NULL,create_activity=$4,updated_at=clock_timestamp() WHERE id=$1 AND generation=$2")
        .bind(item.id).bind(item.generation).bind(i64::try_from(bytes).map_err(|_|ApiError::PayloadTooLarge)?).bind(&create)
        .execute(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
    plamenu_db::job::enqueue_many_from_instance(
        &mut *tx,
        &[target.preferred_inbox().to_owned()],
        &create,
    )
    .await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}
