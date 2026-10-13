//! Original bytes and container come from the selected current physical source.
#[cfg(unix)]
use crate::music_local::fs::{Fingerprint, Opened, Root};
use crate::{AppState, error::ApiError, music_assets::Asset, storage::MediaRead};
use plamenu_db::DbError;
use std::path::PathBuf;
#[cfg(unix)]
#[derive(sqlx::FromRow)]
struct Location {
    id: i64,
    root_id: String,
    directory: String,
    root_device: i64,
    root_inode: i64,
    relative_path: String,
    device: i64,
    inode: i64,
    size_bytes: i64,
    modified_seconds: i64,
    modified_nanos: i64,
    original_format: Option<String>,
}
#[cfg(unix)]
struct Mounted {
    file: Opened,
    format: String,
}
#[cfg(unix)]
fn open_location(state: &AppState, location: &Location) -> Option<Opened> {
    let config = state
        .config
        .music_local_roots
        .iter()
        .find(|root| root.id == location.root_id)?;
    if !location.directory.is_empty()
        && !location
            .relative_path
            .strip_prefix(&location.directory)
            .is_some_and(|rest| rest.starts_with('/'))
    {
        return None;
    }
    let root = Root::open(&config.path).ok()?;
    let identity = root.identity().ok()?;
    if i64::try_from(identity.0).ok() != Some(location.root_device)
        || i64::try_from(identity.1).ok() != Some(location.root_inode)
    {
        return None;
    }
    let file = root.file(&location.relative_path).ok()?;
    let expected = Fingerprint {
        device: u64::try_from(location.device).ok()?,
        inode: u64::try_from(location.inode).ok()?,
        bytes: u64::try_from(location.size_bytes).ok()?,
        modified_seconds: location.modified_seconds,
        modified_nanos: location.modified_nanos,
    };
    (file.fingerprint == expected).then_some(file)
}
#[cfg(unix)]
async fn mounted(state: &AppState, asset: &Asset) -> Result<Mounted, ApiError> {
    let mut after = 0i64;
    loop {
        let locations:Vec<Location>=sqlx::query_as("SELECT p.id,f.root_id,f.directory,f.root_device,f.root_inode,p.relative_path,p.device,p.inode,p.size_bytes,p.modified_seconds,p.modified_nanos,p.original_format FROM music_local_files p JOIN music_local_folders f ON f.id=p.folder_id AND f.owner_id=p.owner_id WHERE p.asset_id=$1 AND p.owner_id=$2 AND p.id>$3 AND f.enabled AND p.available AND p.state='ready' ORDER BY p.id LIMIT 100")
            .bind(asset.id).bind(asset.owner_id).bind(after).fetch_all(&state.pool).await.map_err(DbError::from)?;
        if locations.is_empty() {
            return Err(ApiError::NotFound);
        }
        for location in locations {
            after = location.id;
            if let Some(file) = open_location(state, &location) {
                return Ok(Mounted {
                    file,
                    format: location
                        .original_format
                        .unwrap_or_else(|| asset.original_format.clone()),
                });
            }
        }
    }
}
pub(crate) struct Info {
    pub reader: MediaRead,
    pub format: String,
}
pub(crate) async fn open_info(state: &AppState, asset: &Asset) -> Result<Info, ApiError> {
    if !asset.indexed_original {
        return Ok(Info {
            reader: state
                .media
                .open(&asset.original_file)
                .await
                .map_err(|_| ApiError::NotFound)?,
            format: asset.original_format.clone(),
        });
    }
    #[cfg(unix)]
    {
        let original = mounted(state, asset).await?;
        Ok(Info {
            reader: original.file.into_reader(),
            format: original.format,
        })
    }
    #[cfg(not(unix))]
    {
        Err(ApiError::NotFound)
    }
}
pub(crate) async fn open(state: &AppState, asset: &Asset) -> Result<MediaRead, ApiError> {
    Ok(open_info(state, asset).await?.reader)
}
pub(crate) async fn proof(state: &AppState, asset: &Asset) -> Result<String, ApiError> {
    if !asset.indexed_original {
        return Ok(format!(
            "stored:{}:{}",
            asset.original_file, asset.audio_hash
        ));
    }
    #[cfg(unix)]
    {
        let original = mounted(state, asset).await?;
        Ok(format!(
            "mounted:{}:{:?}",
            original.format, original.file.fingerprint
        ))
    }
    #[cfg(not(unix))]
    {
        Err(ApiError::NotFound)
    }
}
/// Holding input pins its descriptor while a child opens its process path.
pub(crate) struct ProcessInput {
    pub path: PathBuf,
    pub(crate) proof: String,
    #[cfg(unix)]
    _held: Option<Opened>,
}
pub(crate) async fn process_input(
    state: &AppState,
    asset: &Asset,
) -> Result<ProcessInput, ApiError> {
    if !asset.indexed_original {
        let path = state
            .media
            .local_path(&asset.original_file)
            .ok_or_else(|| {
                ApiError::ServiceUnavailable(
                    "Audio processing requires local retained media".into(),
                )
            })?;
        return Ok(ProcessInput {
            path,
            proof: format!("stored:{}:{}", asset.original_file, asset.audio_hash),
            #[cfg(unix)]
            _held: None,
        });
    }
    #[cfg(unix)]
    {
        let original = mounted(state, asset).await?;
        Ok(ProcessInput {
            path: original.file.process_path(),
            proof: format!(
                "mounted:{}:{:?}",
                original.format, original.file.fingerprint
            ),
            _held: Some(original.file),
        })
    }
    #[cfg(not(unix))]
    {
        Err(ApiError::NotFound)
    }
}
impl PartialEq for ProcessInput {
    fn eq(&self, other: &Self) -> bool {
        self.proof == other.proof
    }
}
impl Eq for ProcessInput {}
