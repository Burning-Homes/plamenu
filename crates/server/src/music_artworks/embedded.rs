//! Embedded picture packets use the same normalization, quota and ACLs as uploads.
use crate::{AppState, error::ApiError, music_assets};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::io::AsyncReadExt;
const MAX: u64 = 8 * 1024 * 1024;
pub(crate) async fn import(
    state: &AppState,
    owner: i64,
    asset: i64,
    path: &Path,
) -> Result<(), ApiError> {
    let row = music_assets::get(state, asset, Some(owner)).await?;
    if !super::for_target(state, super::Target::Assets, asset, Some(owner))
        .await?
        .is_empty()
    {
        return Ok(());
    }
    let proof = crate::music_original::proof(state, &row).await?;
    let bytes = extract(state, path).await?;
    if bytes.is_empty() {
        return Ok(());
    }
    if crate::music_original::proof(state, &row).await? != proof {
        return Err(ApiError::Conflict(
            "Music source changed during artwork extraction".into(),
        ));
    }
    upload(state, owner, asset, bytes).await
}
pub(crate) async fn upload(
    state: &AppState,
    owner: i64,
    asset: i64,
    bytes: Vec<u8>,
) -> Result<(), ApiError> {
    let row = music_assets::get(state, asset, Some(owner)).await?;
    if !super::for_target(state, super::Target::Assets, asset, Some(owner))
        .await?
        .is_empty()
    {
        return Ok(());
    }
    let artwork = super::upload(
        state,
        owner,
        super::Target::Assets,
        asset,
        "image",
        &row.revision.to_string(),
        bytes,
    )
    .await?;
    if row.indexed_original && row.indexed_tags_revision == Some(row.revision) {
        // A manual edit that races the automatic image write keeps its marker
        // stale, so later filesystem scans cannot overwrite that personalization.
        let mut tx = state
            .pool
            .begin()
            .await
            .map_err(plamenu_db::DbError::from)?;
        crate::music_assets::lock_owner(&mut tx, owner).await?;
        sqlx::query("UPDATE music_assets SET indexed_tags_revision=revision WHERE id=$1 AND owner_id=$2 AND revision=$3 AND indexed_tags_revision=$4 AND EXISTS(SELECT 1 FROM music_artworks WHERE id=$5 AND asset_id=$1)").bind(asset).bind(owner).bind(row.revision+1).bind(row.revision).bind(artwork.id).execute(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
        tx.commit().await.map_err(plamenu_db::DbError::from)?;
    }
    Ok(())
}

pub(crate) async fn extract(state: &AppState, path: &Path) -> Result<Vec<u8>, ApiError> {
    let permit = crate::media_gate::acquire().await;
    let mut child = tokio::process::Command::new(&state.config.ffmpeg_path)
        .args(["-nostdin", "-v", "error"])
        .args(crate::media_transcode::local_input_options())
        .arg("-i")
        .arg(path)
        // Copy one compressed packet. Image decoding and pixel limits belong
        // to the established artwork normalizer, rather than this demuxer.
        .args([
            "-map",
            "0:v:0",
            "-frames:v",
            "1",
            "-c:v",
            "copy",
            "-f",
            "image2pipe",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    let result = async {
        let mut bytes = Vec::new();
        child
            .stdout
            .take()
            .expect("piped picture output")
            .take(MAX + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|error| ApiError::Internal(Box::new(error)))?;
        let status = child
            .wait()
            .await
            .map_err(|error| ApiError::Internal(Box::new(error)))?;
        if !status.success() || u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX {
            return Err(ApiError::Unprocessable(
                "Embedded artwork is unavailable or too large".into(),
            ));
        }
        Ok(bytes)
    };
    let bytes = tokio::select! {
        result=tokio::time::timeout(Duration::from_secs(30),result)=>result.map_err(|_|ApiError::Unprocessable("Embedded artwork extraction timed out".into()))??,
        ()=state.shutdown.cancelled()=>return Err(ApiError::Conflict("Artwork extraction stopped".into())),
    };
    drop(permit);
    Ok(bytes)
}
