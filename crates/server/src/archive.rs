//! Building the account-archive zip — Mastodon's `BackupService`.
//!
//! Produces a self-contained zip of one local account's data:
//!
//! - `actor.json` — the actor document, with `icon`/`image` and the `outbox`/`likes`/`bookmarks`
//!   links rewritten to point at the in-zip files.
//! - `outbox.json` — an `OrderedCollection` of every status (all visibilities, unlike the public
//!   outbox) as `Create`/`Announce` activities, with each attachment URL rewritten to its relative
//!   `media_attachments/…` path.
//! - `likes.json` / `bookmarks.json` — `OrderedCollection`s of the favourited and bookmarked status
//!   URIs.
//! - `avatar.*` / `header.*` and `media_attachments/*` — the referenced files.
//!
//! The build streams under a fixed memory budget rather than assembling the zip
//! in RAM. A blocking encoder task owns a `ZipWriter` over a
//! temporary file under `{media_dir}/tmp` (disk-backed, so a multi-gigabyte
//! archive never lands in a RAM-backed `/tmp`); the async producer pages the
//! database — statuses, favourites, and bookmarks keyset-paged, media read
//! through the store's streaming `open` in fixed [`MEDIA_CHUNK`] slices — and
//! hands each entry to the encoder over a small bounded channel. Peak resident
//! memory is therefore O(one page + one media chunk + channel depth),
//! independent of account or media size. Bundled media is capped at
//! [`MAX_MEDIA_BYTES`]; the always-complete metadata (statuses, favourites,
//! bookmarks) streams regardless. The archive worker
//! ([`crate::archive_worker`]) drives this and persists the temp file into the
//! media store by rename via [`crate::storage::MediaStore::put_file`].

use std::collections::HashSet;
use std::io::Write as _;

use plamenu_ap::urls::LocalStatusUrls;
use plamenu_db::account::Account;
use plamenu_db::export::ExportRef;
use plamenu_db::{export, status};
use serde_json::{Value, json};
use tempfile::NamedTempFile;
use tokio::io::AsyncReadExt as _;
use tokio::sync::mpsc;
use zip::write::SimpleFileOptions;

use crate::AppState;
use crate::config::Config;
use crate::error::ApiError;

const ACTOR: &str = "actor.json";
const OUTBOX: &str = "outbox.json";
const LIKES: &str = "likes.json";
const BOOKMARKS: &str = "bookmarks.json";
const MEDIA_DIR: &str = "media_attachments";
/// Name of the note written into the archive when [`MAX_MEDIA_BYTES`] omits
/// some media files (the metadata stays complete).
const OMITTED_NOTE: &str = "README-omitted-media.txt";

/// How many statuses / favourites / bookmarks one page walks — Mastodon's
/// `find_in_batches` for the outbox, applied to every keyset-paged collection.
const PAGE: i64 = 200;

/// Fixed slice size for copying a media file from the store into the zip. Peak
/// per-file resident memory is one of these, never the whole attachment.
const MEDIA_CHUNK: usize = 64 * 1024;

/// Bounded depth of the producer→encoder channel. Small, so the async producer
/// can never race ahead and buffer many entries in memory; it blocks on the
/// blocking encoder instead.
const CHANNEL_CAP: usize = 8;

/// Per-archive cap on the total size of *bundled* media (avatar, header, and
/// attachments). The metadata streams (`actor.json`, `outbox.json`,
/// `likes.json`, `bookmarks.json`) are always complete; once the running media
/// total would exceed this, further media files are omitted and a note
/// ([`OMITTED_NOTE`]) naming the count is written into the archive. This bounds
/// the transient scratch file and the stored ZIP so one media-heavy account
/// cannot exhaust the disk or object store. Documented in the operator guide.
pub const MAX_MEDIA_BYTES: u64 = 8 * 1024 * 1024 * 1024; // 8 GiB

/// A command to the blocking zip encoder: start a new entry, or append bytes to
/// the current one. Chunked so the producer streams media/JSON incrementally.
enum ZipMsg {
    /// Begin a new zip entry. `stored` selects no-compression (already-compressed
    /// media) over deflate (JSON/text).
    Start { name: String, stored: bool },
    /// Append a slice to the current entry.
    Data(Vec<u8>),
}

fn internal<E>(error: E) -> ApiError
where
    E: std::error::Error + Send + Sync + 'static,
{
    ApiError::Internal(Box::new(error))
}

/// Builds the archive zip for `account`, returning a temporary file (under
/// `{media_dir}/tmp`) holding the finished ZIP. The caller moves it into the
/// media store with [`crate::storage::MediaStore::put_file`] and reads its size
/// from filesystem metadata.
pub async fn build(state: &AppState, account: &Account) -> Result<NamedTempFile, ApiError> {
    build_with_media_limit(state, account, MAX_MEDIA_BYTES).await
}

/// [`build`] with an explicit bundled-media byte cap. Exposed so tests can drive
/// the truncation path without a multi-gigabyte fixture.
pub async fn build_with_media_limit(
    state: &AppState,
    account: &Account,
    max_media_bytes: u64,
) -> Result<NamedTempFile, ApiError> {
    // Disk-backed scratch under the media root, so the rename into the store is
    // cheap and a large archive never buffers in a RAM-backed /tmp.
    let temp = scratch_tempfile(&state.config).await?;
    let file = temp.reopen().map_err(internal)?;

    // The blocking encoder owns the ZipWriter; the async producer feeds it.
    let (tx, rx) = mpsc::channel::<ZipMsg>(CHANNEL_CAP);
    let encoder = tokio::task::spawn_blocking(move || encode(rx, file));

    let produced = produce(state, account, max_media_bytes, &tx).await;
    drop(tx); // close the channel so the encoder finalizes the ZIP.

    // Join the encoder first: if it failed (e.g. the disk filled), that is the
    // root cause and the producer's error is just the closed channel.
    match encoder.await.map_err(internal)? {
        Ok(()) => {
            produced?;
            Ok(temp)
        }
        Err(error) => Err(internal(error)),
    }
}

/// The blocking side: drives a `ZipWriter` over the temp `file` from the channel
/// until the producer drops its sender, then finalizes the central directory.
fn encode(mut rx: mpsc::Receiver<ZipMsg>, file: std::fs::File) -> std::io::Result<()> {
    let mut writer = zip::ZipWriter::new(file);
    let deflated =
        SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let stored = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    while let Some(msg) = rx.blocking_recv() {
        match msg {
            ZipMsg::Start { name, stored: raw } => {
                let options = if raw { stored } else { deflated };
                writer
                    .start_file(name, options)
                    .map_err(std::io::Error::other)?;
            }
            ZipMsg::Data(bytes) => writer.write_all(&bytes)?,
        }
    }
    writer.finish().map_err(std::io::Error::other)?;
    Ok(())
}

/// The async side: pages the database and streams every entry to the encoder.
async fn produce(
    state: &AppState,
    account: &Account,
    max_media_bytes: u64,
    tx: &mpsc::Sender<ZipMsg>,
) -> Result<(), ApiError> {
    let domain = state.config.domain.as_str();
    // Media files referenced by the outbox, discovered as we stream it.
    let mut referenced_media: HashSet<String> = HashSet::new();

    // actor.json — the actor document, rewired to the in-zip files.
    let actor = actor_json(state, account).await?;
    send_value(tx, ACTOR, &actor).await?;

    // outbox.json — every status, oldest first, streamed page by page.
    stream_outbox(state, account, domain, tx, &mut referenced_media).await?;

    // likes.json / bookmarks.json — favourited/bookmarked status URIs, paged.
    stream_uri_collection(state, account, domain, tx, RefKind::Favourites).await?;
    stream_uri_collection(state, account, domain, tx, RefKind::Bookmarks).await?;

    // Media, capped: avatar, header, then every referenced attachment.
    let mut media_bytes: u64 = 0;
    let mut omitted: u64 = 0;
    if let Some(name) = &account.avatar_file_name {
        stream_media(
            state,
            tx,
            name,
            &format!("avatar.{}", extension(name)),
            max_media_bytes,
            &mut media_bytes,
            &mut omitted,
        )
        .await?;
    }
    if let Some(name) = &account.header_file_name {
        stream_media(
            state,
            tx,
            name,
            &format!("header.{}", extension(name)),
            max_media_bytes,
            &mut media_bytes,
            &mut omitted,
        )
        .await?;
    }
    for name in &referenced_media {
        stream_media(
            state,
            tx,
            name,
            &format!("{MEDIA_DIR}/{name}"),
            max_media_bytes,
            &mut media_bytes,
            &mut omitted,
        )
        .await?;
    }

    if omitted > 0 {
        let note = format!(
            "This archive omitted {omitted} media file(s) because the total bundled \
             media would have exceeded the {} MiB per-archive limit.\n\nThe metadata \
             above (actor.json, outbox.json, likes.json, bookmarks.json) is complete. \
             The omitted files remain retrievable from the server while their posts \
             exist.\n",
            max_media_bytes / (1024 * 1024),
        );
        send_entry(tx, OMITTED_NOTE, false, note.into_bytes()).await?;
    }
    Ok(())
}

/// Streams the `outbox.json` `OrderedCollection` one [`PAGE`] of statuses at a
/// time, recording every referenced media file. `totalItems` is emitted last
/// (JSON object key order is free) so the count needs no separate query.
async fn stream_outbox(
    state: &AppState,
    account: &Account,
    domain: &str,
    tx: &mpsc::Sender<ZipMsg>,
    referenced_media: &mut HashSet<String>,
) -> Result<(), ApiError> {
    send(
        tx,
        ZipMsg::Start {
            name: OUTBOX.to_owned(),
            stored: false,
        },
    )
    .await?;
    send(
        tx,
        ZipMsg::Data(collection_prefix(
            &plamenu_ap::activity::quote_context(),
            OUTBOX,
        )?),
    )
    .await?;

    let mut count: u64 = 0;
    let mut after: Option<i64> = None;
    loop {
        let batch = status::archive_page(&state.pool, account.id, after, PAGE).await?;
        let Some(last) = batch.last() else { break };
        after = Some(last.id);
        for mut activity in crate::routes::actors::outbox_items(state, account, &batch).await? {
            rewrite_attachments(&mut activity, domain, referenced_media);
            let json = serde_json::to_vec(&activity).map_err(internal)?;
            send(tx, ZipMsg::Data(item_chunk(count == 0, json))).await?;
            count += 1;
        }
    }

    send(
        tx,
        ZipMsg::Data(format!("],\"totalItems\":{count}}}").into_bytes()),
    )
    .await?;
    Ok(())
}

/// Which relationship collection [`stream_uri_collection`] pages.
#[derive(Clone, Copy)]
enum RefKind {
    Favourites,
    Bookmarks,
}

/// Streams a `likes.json`/`bookmarks.json` `OrderedCollection` of status URIs,
/// keyset-paged so the whole set is never resident. Mastodon omits `totalItems`
/// on these, so we do too.
async fn stream_uri_collection(
    state: &AppState,
    account: &Account,
    domain: &str,
    tx: &mpsc::Sender<ZipMsg>,
    kind: RefKind,
) -> Result<(), ApiError> {
    let name = match kind {
        RefKind::Favourites => LIKES,
        RefKind::Bookmarks => BOOKMARKS,
    };
    send(
        tx,
        ZipMsg::Start {
            name: name.to_owned(),
            stored: false,
        },
    )
    .await?;
    send(
        tx,
        ZipMsg::Data(collection_prefix(&json!(plamenu_ap::AS_CONTEXT), name)?),
    )
    .await?;

    let mut first = true;
    let mut after: Option<i64> = None;
    loop {
        let page: Vec<ExportRef> = match kind {
            RefKind::Favourites => {
                export::favourites_page(&state.pool, account.id, after, PAGE).await?
            }
            RefKind::Bookmarks => {
                export::bookmarks_page(&state.pool, account.id, after, PAGE).await?
            }
        };
        let Some(last) = page.last() else { break };
        after = Some(last.cursor);
        for row in &page {
            let uri = row.uri.clone().unwrap_or_else(|| {
                row.author_uri.as_ref().map_or_else(
                    || LocalStatusUrls::new(domain, &row.author_username, row.status_id).id,
                    |actor_id| {
                        LocalStatusUrls::from_actor_id(
                            domain,
                            &row.author_username,
                            actor_id,
                            row.status_id,
                        )
                        .id
                    },
                )
            });
            let json = serde_json::to_vec(&Value::String(uri)).map_err(internal)?;
            send(tx, ZipMsg::Data(item_chunk(first, json))).await?;
            first = false;
        }
    }

    send(tx, ZipMsg::Data(b"]}".to_vec())).await?;
    Ok(())
}

/// Streams one media file into the zip through the store's `open` reader in
/// [`MEDIA_CHUNK`] slices — never buffering the whole file. A missing file is
/// skipped (like Mastodon). Once `*media_bytes` would exceed `max`, the file is
/// omitted and `*omitted` is incremented; the metadata stays complete.
async fn stream_media(
    state: &AppState,
    tx: &mpsc::Sender<ZipMsg>,
    file_name: &str,
    entry_name: &str,
    max: u64,
    media_bytes: &mut u64,
    omitted: &mut u64,
) -> Result<(), ApiError> {
    let opened = match state.media.open(file_name).await {
        Ok(opened) => opened,
        Err(error) => {
            // Mastodon likewise skips attachments whose file has vanished.
            tracing::debug!(%error, file_name, "archive: skipping missing media file");
            return Ok(());
        }
    };
    if media_bytes.saturating_add(opened.len) > max {
        *omitted += 1;
        return Ok(());
    }

    send(
        tx,
        ZipMsg::Start {
            name: entry_name.to_owned(),
            stored: true,
        },
    )
    .await?;
    let mut reader = opened.reader;
    let mut buf = vec![0u8; MEDIA_CHUNK];
    loop {
        let read = reader.read(&mut buf).await.map_err(internal)?;
        if read == 0 {
            break;
        }
        send(tx, ZipMsg::Data(buf[..read].to_vec())).await?;
        *media_bytes = media_bytes.saturating_add(read as u64);
    }
    Ok(())
}

/// Sends one complete small entry (`actor.json`, the omitted-media note).
async fn send_value(tx: &mpsc::Sender<ZipMsg>, name: &str, value: &Value) -> Result<(), ApiError> {
    let bytes = serde_json::to_vec(value).map_err(internal)?;
    send_entry(tx, name, false, bytes).await
}

async fn send_entry(
    tx: &mpsc::Sender<ZipMsg>,
    name: &str,
    stored: bool,
    bytes: Vec<u8>,
) -> Result<(), ApiError> {
    send(
        tx,
        ZipMsg::Start {
            name: name.to_owned(),
            stored,
        },
    )
    .await?;
    send(tx, ZipMsg::Data(bytes)).await
}

/// Sends one message, mapping a closed channel (the encoder died) to an error;
/// the encoder's own error is surfaced by the join in [`build_with_media_limit`].
async fn send(tx: &mpsc::Sender<ZipMsg>, msg: ZipMsg) -> Result<(), ApiError> {
    tx.send(msg)
        .await
        .map_err(|_| ApiError::Internal("archive encoder stopped early".into()))
}

/// One `orderedItems` element with its leading comma when it is not the first.
fn item_chunk(first: bool, json: Vec<u8>) -> Vec<u8> {
    if first {
        json
    } else {
        let mut chunk = Vec::with_capacity(json.len() + 1);
        chunk.push(b',');
        chunk.extend_from_slice(&json);
        chunk
    }
}

/// The opening bytes of an `OrderedCollection` up to (and including) the
/// `orderedItems` array's `[`, with the given `@context`. Items and the closing
/// `]…}` are streamed after it.
fn collection_prefix(context: &Value, id: &str) -> Result<Vec<u8>, ApiError> {
    let ctx = serde_json::to_vec(context).map_err(internal)?;
    let id = serde_json::to_vec(id).map_err(internal)?;
    let mut prefix = Vec::with_capacity(ctx.len() + id.len() + 64);
    prefix.extend_from_slice(br#"{"@context":"#);
    prefix.extend_from_slice(&ctx);
    prefix.extend_from_slice(br#","id":"#);
    prefix.extend_from_slice(&id);
    prefix.extend_from_slice(br#","type":"OrderedCollection","orderedItems":["#);
    Ok(prefix)
}

/// Scratch space under the media root, like the on-demand A/V lane: the finished
/// ZIP moves into the store by rename, and a large archive never lands in a
/// RAM-backed `/tmp`.
async fn scratch_tempfile(config: &Config) -> Result<NamedTempFile, ApiError> {
    let root = config.media_dir.join("tmp");
    tokio::fs::create_dir_all(&root).await.map_err(internal)?;
    tokio::task::spawn_blocking(move || {
        tempfile::Builder::new()
            .prefix("archive-")
            .suffix(".zip")
            .tempfile_in(&root)
    })
    .await
    .map_err(internal)?
    .map_err(internal)
}

/// Serializes the actor document and rewrites the fields that must reference
/// the in-zip files instead of live URLs, like Mastodon's `dump_actor!`.
async fn actor_json(state: &AppState, account: &Account) -> Result<Value, ApiError> {
    let actor = crate::profile::local_actor(state, account).await?;
    let mut value = serde_json::to_value(&actor).map_err(internal)?;
    if let Some(name) = &account.avatar_file_name {
        set_image_url(&mut value, "icon", &format!("avatar.{}", extension(name)));
    }
    if let Some(name) = &account.header_file_name {
        set_image_url(&mut value, "image", &format!("header.{}", extension(name)));
    }
    value["outbox"] = json!(OUTBOX);
    value["likes"] = json!(LIKES);
    value["bookmarks"] = json!(BOOKMARKS);
    Ok(value)
}

/// Points an actor image object's (`icon`/`image`) `url` at an in-zip file.
fn set_image_url(actor: &mut Value, key: &str, filename: &str) {
    if let Some(image) = actor.get_mut(key)
        && image.is_object()
    {
        image["url"] = json!(filename);
    }
}

/// Rewrites each of a `Create` activity's attachment URLs from the live
/// `https://{domain}/media/{file}` form to the relative `media_attachments/…`
/// path, recording the file names to pull into the zip. `Announce` activities
/// (boosts) carry no own attachments and are left untouched.
fn rewrite_attachments(activity: &mut Value, domain: &str, referenced: &mut HashSet<String>) {
    let prefix = format!("https://{domain}/media/");
    let Some(attachments) = activity
        .get_mut("object")
        .and_then(|object| object.get_mut("attachment"))
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    for attachment in attachments {
        let Some(url) = attachment.get("url").and_then(Value::as_str) else {
            continue;
        };
        if let Some(file) = url.strip_prefix(&prefix) {
            let file = file.to_owned();
            attachment["url"] = json!(format!("{MEDIA_DIR}/{file}"));
            referenced.insert(file);
        }
    }
}

/// The file-name extension (without the dot), lowercased; `bin` when absent.
fn extension(file_name: &str) -> String {
    file_name
        .rsplit_once('.')
        .map_or_else(|| "bin".to_owned(), |(_, ext)| ext.to_ascii_lowercase())
}
