//! The blocking ZIP encoder receives only a small channel of bounded chunks.
use super::{Download, content::Spooled};
use crate::{AppState, error::ApiError};
use std::io::Write as _;
use tokio::{io::AsyncReadExt as _, sync::mpsc};
enum Message {
    Start(String),
    Bytes(Vec<u8>),
}
fn encode(mut receiver: mpsc::Receiver<Message>, file: std::fs::File) -> std::io::Result<()> {
    let mut zip = zip::ZipWriter::new(file);
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    while let Some(message) = receiver.blocking_recv() {
        match message {
            Message::Start(name) => zip
                .start_file(name, options)
                .map_err(std::io::Error::other)?,
            Message::Bytes(bytes) => zip.write_all(&bytes)?,
        }
    }
    zip.finish().map_err(std::io::Error::other)?;
    Ok(())
}
fn name(index: usize, title: &str, artist: &str, extension: &str) -> String {
    let text = if artist.is_empty() {
        title.into()
    } else {
        format!("{artist} - {title}")
    };
    let safe = text
        .chars()
        .map(|character| {
            if character.is_control() || "/\\:*?\"<>|".contains(character) {
                '_'
            } else {
                character
            }
        })
        .take(120)
        .collect::<String>();
    let safe = safe.trim().trim_matches('.');
    let suffix = extension
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect::<String>();
    format!(
        "{:03} - {}.{}",
        index + 1,
        if safe.is_empty() { "audio" } else { safe },
        if suffix.is_empty() { "audio" } else { &suffix }
    )
}
async fn send_file(
    state: &AppState,
    job: &Download,
    source: &Spooled,
    sender: &mpsc::Sender<Message>,
) -> Result<(), ApiError> {
    let mut input = tokio::fs::File::open(source.file.path())
        .await
        .map_err(super::internal)?;
    let mut buffer = vec![0; 65536];
    let mut checked = std::time::Instant::now();
    loop {
        let count = input.read(&mut buffer).await.map_err(super::internal)?;
        if count == 0 {
            return Ok(());
        }
        if state.shutdown.is_cancelled() {
            return Err(ApiError::Conflict("Archive worker stopped".into()));
        }
        if checked.elapsed() >= std::time::Duration::from_secs(1) {
            super::worker::heartbeat(state, job).await?;
            checked = std::time::Instant::now();
        }
        sender
            .send(Message::Bytes(buffer[..count].to_vec()))
            .await
            .map_err(|_| ApiError::ServiceUnavailable("Archive writer stopped".into()))?;
    }
}
pub struct Built {
    pub file: tempfile::NamedTempFile,
    pub included: Vec<i32>,
    pub errors: serde_json::Value,
}
pub async fn build(state: &AppState, job: &Download) -> Result<Built, ApiError> {
    let root = state.config.media_dir.join("tmp");
    tokio::fs::create_dir_all(&root)
        .await
        .map_err(super::internal)?;
    let directory = tempfile::Builder::new()
        .prefix("musicdownload-")
        .tempdir_in(&root)
        .map_err(super::internal)?;
    let file = tempfile::Builder::new()
        .prefix("musicdownload-")
        .suffix(".zip")
        .tempfile_in(&root)
        .map_err(super::internal)?;
    let (sender, receiver) = mpsc::channel(4);
    let target = file.reopen().map_err(super::internal)?;
    let task = tokio::task::spawn_blocking(move || encode(receiver, target));
    let produced = produce(state, job, directory.path(), &sender).await;
    drop(sender);
    task.await
        .map_err(super::internal)?
        .map_err(super::internal)?;
    let (included, errors) = produced?;
    if included.is_empty() {
        return Err(ApiError::Unprocessable(
            "No selected audio could be downloaded".into(),
        ));
    }
    if file.as_file().metadata().map_err(super::internal)?.len()
        > state.config.music.max_download_mib * 1024 * 1024
    {
        return Err(ApiError::Unprocessable(
            "Music archive exceeds its size limit".into(),
        ));
    }
    Ok(Built {
        file,
        included,
        errors: serde_json::Value::Array(errors),
    })
}
async fn produce(
    state: &AppState,
    job: &Download,
    root: &std::path::Path,
    sender: &mpsc::Sender<Message>,
) -> Result<(Vec<i32>, Vec<serde_json::Value>), ApiError> {
    let mut included = Vec::new();
    let mut errors = Vec::new();
    let mut bytes = 0_u64;
    for (index, item) in job.selection.items.iter().enumerate() {
        super::worker::heartbeat(state, job).await?;
        let loaded = async {
            let source = super::selection::playable(state, job.owner_id, item).await?;
            let file = super::content::spool(state, job, item, root).await?;
            Ok::<_, ApiError>((source, file))
        }
        .await;
        match loaded {
            Ok((source,file)) if bytes.saturating_add(file.bytes)+u64::try_from(index+2).expect("bounded selection")*4096<=state.config.music.max_download_mib*1024*1024 => {
                bytes+=file.bytes;
                sender.send(Message::Start(name(index,&source.title,&source.artist,&file.extension))).await.map_err(|_|ApiError::ServiceUnavailable("Archive writer stopped".into()))?;
                send_file(state,job,&file,sender).await?;
                included.push(i32::try_from(index).expect("bounded archive"));
            },
            Ok(_)|Err(ApiError::NotFound|ApiError::BadGateway(_)|ApiError::Unprocessable(_))=>errors.push(serde_json::json!({"index":index,"message":"Audio unavailable or exceeds the archive limit"})),
            Err(error)=>return Err(error),
        }
        sqlx::query("UPDATE music_downloads SET completed_items=$3,item_errors=$4 WHERE id=$1 AND lease=$2 AND state='running' AND lease_until>now()")
            .bind(job.id).bind(job.lease).bind(i32::try_from(index+1).expect("bounded archive")).bind(super::records::Errors::from_value(serde_json::Value::Array(errors.clone()))?).execute(&state.pool).await.map_err(plamenu_db::DbError::from)?;
    }
    Ok((included, errors))
}
