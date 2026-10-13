//! Only the track's selected normalized artwork is written into its source.
use crate::{AppState, error::ApiError, music_artworks, music_assets::Asset};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::io::Write;
pub(super) struct Cover {
    pub png: tempfile::NamedTempFile,
    pub metadata: tempfile::NamedTempFile,
}
pub(super) async fn load(state: &AppState, asset: &Asset) -> Result<Option<Cover>, ApiError> {
    let mut images = music_artworks::for_target(
        state,
        music_artworks::Target::Assets,
        asset.id,
        Some(asset.owner_id),
    )
    .await?;
    images.sort_by_key(|image| image.slot != "image");
    let Some(art) = images.into_iter().next() else {
        return Ok(None);
    };
    if art.size_bytes <= 0 || art.size_bytes > 8 * 1024 * 1024 {
        return Err(ApiError::Unprocessable(
            "Track artwork exceeds the writeback limit".into(),
        ));
    }
    let bytes = state
        .media
        .get(&art.file_name)
        .await
        .map_err(|_| ApiError::NotFound)?;
    if i64::try_from(bytes.len()).ok() != Some(art.size_bytes) {
        return Err(ApiError::NotFound);
    }
    let encoded = tokio::task::spawn_blocking(move || {
        let image = image::load_from_memory(&bytes)
            .map_err(|error| ApiError::Internal(Box::new(error)))?
            .to_rgba8();
        if image.width() > 1024 || image.height() > 1024 {
            return Err(ApiError::Unprocessable(
                "Track artwork is not normalized".into(),
            ));
        }
        let mut output = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut output, image::ImageFormat::Png)
            .map_err(|error| ApiError::Internal(Box::new(error)))?;
        Ok((output.into_inner(), image.width(), image.height()))
    })
    .await
    .map_err(|error| ApiError::Internal(Box::new(error)))??;
    let mut png = tempfile::Builder::new()
        .prefix("music-cover-")
        .suffix(".png")
        .tempfile()
        .map_err(internal)?;
    png.write_all(&encoded.0).map_err(internal)?;
    png.flush().map_err(internal)?;
    let mut metadata = tempfile::Builder::new()
        .prefix("music-cover-")
        .suffix(".ffmeta")
        .tempfile()
        .map_err(internal)?;
    let picture = picture_block(&encoded.0, encoded.1, encoded.2)?;
    writeln!(
        metadata,
        ";FFMETADATA1\nmetadata_block_picture={}",
        STANDARD.encode(picture)
    )
    .map_err(internal)?;
    metadata.flush().map_err(internal)?;
    Ok(Some(Cover { png, metadata }))
}
fn internal(error: std::io::Error) -> ApiError {
    ApiError::Internal(Box::new(error))
}
fn picture_block(bytes: &[u8], width: u32, height: u32) -> Result<Vec<u8>, ApiError> {
    let mut picture = Vec::with_capacity(bytes.len() + 64);
    picture.extend(3u32.to_be_bytes());
    picture.extend(9u32.to_be_bytes());
    picture.extend(b"image/png");
    picture.extend(0u32.to_be_bytes());
    for number in [
        width,
        height,
        32,
        0,
        u32::try_from(bytes.len())
            .map_err(|_| ApiError::Unprocessable("Track artwork is too large".into()))?,
    ] {
        picture.extend(number.to_be_bytes());
    }
    picture.extend(bytes);
    Ok(picture)
}
