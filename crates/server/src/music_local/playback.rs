//! Publishing mounted tracks prepares a portable, quota-accounted playback copy.
use crate::{AppState, error::ApiError, music_assets, music_renditions};
use plamenu_db::DbError;
pub(crate) async fn prepare_publication(
    state: &AppState,
    owner: i64,
    asset: i64,
) -> Result<(), ApiError> {
    let source = music_assets::get(state, asset, Some(owner)).await?;
    if source.owner_id != owner {
        return Err(ApiError::NotFound);
    }
    if source.visibility != "public" {
        return Err(ApiError::Unprocessable(
            "Make the track public before publishing Audio".into(),
        ));
    }
    if !source.indexed_original {
        return Ok(());
    }
    crate::music_original::open(state, &source).await?;
    if source.stream_bytes > 0 {
        return Ok(());
    }
    let selection = music_renditions::negotiate(state, Some(owner), "mp3", Some(192)).await?;
    let requested = music_renditions::request(state, asset, Some(owner), selection).await?;
    let started = std::time::Instant::now();
    let ready = loop {
        let current = music_renditions::get(state, asset, requested.id, Some(owner)).await?;
        if current.state == "ready" {
            break current;
        }
        if !matches!(current.state.as_str(), "queued" | "running") {
            return Err(ApiError::Unprocessable(
                "Could not prepare published audio".into(),
            ));
        }
        if started.elapsed() > std::time::Duration::from_secs(30) {
            return Err(ApiError::ServiceUnavailable(
                "Playback preparation is running; publish again when it is ready".into(),
            ));
        }
        tokio::select! {()=state.shutdown.cancelled()=>return Err(ApiError::Conflict("Playback preparation stopped".into())),()=tokio::time::sleep(std::time::Duration::from_millis(250))=>{}}
    };
    let key = ready.file_name.as_ref().ok_or(ApiError::NotFound)?;
    let scratch = crate::media_transcode::scratch_dir(&state.config).await?;
    let output = scratch.path().join("publication.mp3");
    let mut input = state
        .media
        .open(key)
        .await
        .map_err(|_| ApiError::NotFound)?;
    if i64::try_from(input.len).ok() != Some(ready.size_bytes) {
        return Err(ApiError::NotFound);
    }
    let mut file = tokio::fs::File::create(&output)
        .await
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    let copied = tokio::io::copy(&mut input.reader, &mut file)
        .await
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    if copied != input.len {
        return Err(ApiError::NotFound);
    }
    drop(file);
    reserve_playback(state, &source, ready.size_bytes).await?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, owner).await?;
    crate::music_queues::active_owner(&mut tx, owner).await?;
    // The reservation deliberately hides the unfinished playback key from
    // ordinary readers. This owner-fenced writer reads its row directly.
    let current:Option<(String,i64,String)>=sqlx::query_as("SELECT audio_hash,stream_bytes,visibility FROM music_assets WHERE id=$1 AND owner_id=$2 FOR UPDATE").bind(asset).bind(owner).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
    let (audio_hash, stream_bytes, visibility) = current.ok_or(ApiError::NotFound)?;
    crate::music_original::open(state, &source).await?;
    if audio_hash != source.audio_hash || visibility != "public" {
        return Err(ApiError::Conflict(
            "Music source changed; prepare playback again".into(),
        ));
    }
    let journal:Option<i64>=sqlx::query_scalar("SELECT id FROM media_cleanup_jobs WHERE file_name=$1 AND attempts=0 AND run_at>now() FOR UPDATE").bind(&source.stream_file).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
    journal.ok_or_else(|| ApiError::Conflict("Playback reservation expired".into()))?;
    if stream_bytes == 0 {
        state
            .media
            .put_file(&source.stream_file, &output)
            .await
            .map_err(|error| ApiError::Internal(Box::new(error)))?;
        sqlx::query("UPDATE music_assets SET stream_bytes=$2 WHERE id=$1")
            .bind(asset)
            .bind(ready.size_bytes)
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
    }
    sqlx::query("DELETE FROM media_cleanup_jobs WHERE file_name=$1")
        .bind(&source.stream_file)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}

async fn reserve_playback(
    state: &AppState,
    source: &music_assets::Asset,
    bytes: i64,
) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, source.owner_id).await?;
    let current:Option<i64>=sqlx::query_scalar("SELECT stream_bytes FROM music_assets WHERE id=$1 AND owner_id=$2 AND audio_hash=$3 AND visibility='public' FOR UPDATE").bind(source.id).bind(source.owner_id).bind(&source.audio_hash).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
    let current = current.ok_or(ApiError::NotFound)?;
    if current > 0 {
        return Err(ApiError::Conflict(
            "Playback copy became ready; retry publishing".into(),
        ));
    }
    let pending: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM media_cleanup_jobs WHERE file_name=$1)")
            .bind(&source.stream_file)
            .fetch_one(&mut *tx)
            .await
            .map_err(DbError::from)?;
    if pending {
        return Err(ApiError::ServiceUnavailable(
            "Published playback copy is being prepared; retry shortly".into(),
        ));
    }
    music_assets::reserve_conn(
        state,
        &mut tx,
        source.owner_id,
        std::slice::from_ref(&source.stream_file),
        &[bytes],
    )
    .await?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
