//! FEP-752d federated Webxdc sessions: package validation, actor/activity wire
//! shapes, membership, coordinator sequencing, replay, and durable host state.

use std::collections::HashSet;
use std::io::{Cursor, Read};

use base64::Engine as _;
use hmac::{Hmac, KeyInit, Mac};
use plamenu_db::account::{self, Account};
use plamenu_db::webxdc::{self, Membership, Session, Update};
use plamenu_db::{actor_key, id, job};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use unicode_normalization::UnicodeNormalization as _;

use crate::AppState;
use crate::error::ApiError;

pub const PROTOCOL: &str = "https://w3id.org/fep/752d";
pub const CONTEXT: &str = "https://w3id.org/fep/752d";
pub const OPEN_REL: &str = "https://w3id.org/fep/752d/open";
pub const MEDIA_TYPE: &str = "application/webxdc+zip";
pub const LEGACY_MEDIA_TYPE: &str = "application/x-webxdc";

pub const MAX_UPDATE_BYTES: i32 = 1024 * 1024;
const MAX_FILES: usize = 1024;
const MAX_PATH_BYTES: usize = 240;
const MAX_PATH_DEPTH: usize = 32;
const MAX_COMPRESSION_RATIO: u64 = 200;

fn protocol_db_error(error: plamenu_db::DbError) -> ApiError {
    match error {
        plamenu_db::DbError::Protocol(message)
            if message.contains("has ended") || message.contains("was deleted") =>
        {
            ApiError::Gone
        }
        plamenu_db::DbError::Protocol(message) if message.contains("storage quota") => {
            ApiError::PayloadTooLargeWithMessage(message)
        }
        plamenu_db::DbError::Protocol(message) if message.contains("submitted too soon") => {
            ApiError::TooManyRequests
        }
        plamenu_db::DbError::Protocol(message) if message.contains("not an active") => {
            ApiError::Forbidden(message)
        }
        plamenu_db::DbError::Protocol(message) => ApiError::Conflict(message),
        other => ApiError::from(other),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PackageError {
    #[error("the Webxdc package must be non-empty and at most {0} MiB")]
    CompressedSize(i32),
    #[error("the Webxdc bundle digest does not match")]
    Digest,
    #[error("the Webxdc bundle is not a valid ZIP archive")]
    Zip,
    #[error("the Webxdc bundle contains an unsafe archive entry: {0}")]
    UnsafeEntry(String),
    #[error("the Webxdc bundle exceeds archive resource limits")]
    ResourceLimit,
    #[error("the Webxdc bundle has no index.html")]
    MissingEntry,
    #[error("the Webxdc manifest.toml is invalid or exceeds metadata limits")]
    Manifest,
    #[error("the Webxdc package exceeds the {kind} limit of {limit_mb} MiB")]
    SizeLimit { kind: &'static str, limit_mb: i32 },
}

#[derive(Debug)]
pub struct ValidatedPackage {
    pub digest_multibase: String,
    pub files: Vec<(String, String, Vec<u8>)>,
    pub metadata: PackageMetadata,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageMetadata {
    pub name: Option<String>,
    pub source_code_url: Option<String>,
    /// Widely deployed xdcget extension. Webxdc itself does not currently
    /// standardize a version field, so callers must treat this as a label.
    pub version: Option<String>,
    pub icon_path: Option<String>,
}

#[derive(Debug)]
pub struct CreateLocal<'a> {
    pub creator: &'a Account,
    pub name: &'a str,
    pub summary: &'a str,
    pub bundle_name: &'a str,
    pub bundle_bytes: &'a [u8],
    pub membership_policy: &'a str,
    pub send_update_interval: i32,
    pub send_update_max_size: i32,
}

#[derive(Debug)]
pub struct CreateLocalFromLibrary<'a> {
    pub creator: &'a Account,
    pub name: &'a str,
    pub summary: &'a str,
    pub version_id: i64,
    pub membership_policy: &'a str,
    pub send_update_interval: i32,
    pub send_update_max_size: i32,
}

#[derive(Debug)]
struct RemoteDescriptor {
    raw: Value,
    coordinator_uri: String,
    creator_uri: String,
    name: String,
    summary: String,
    bundle_id: String,
    bundle_url: String,
    bundle_name: String,
    bundle_media_type: String,
    digest_multibase: String,
    send_update_interval: i32,
    send_update_max_size: i32,
    published_at: OffsetDateTime,
    ended_at: Option<OffsetDateTime>,
}

#[must_use]
pub fn digest_multibase(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut multihash = Vec::with_capacity(34);
    multihash.extend_from_slice(&[0x12, 0x20]);
    multihash.extend_from_slice(&digest);
    format!(
        "u{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(multihash)
    )
}

fn safe_entry(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_PATH_BYTES
        && !name.starts_with('/')
        && !name.starts_with('\\')
        && !name.contains('\\')
        && !name.contains('\0')
        && !name.contains(['%', '?', '#'])
        && !name.chars().any(char::is_control)
        && name.split('/').count() <= MAX_PATH_DEPTH
        && name.split('/').all(|part| !matches!(part, "" | "." | ".."))
}

fn package_media_type(path: &str) -> &'static str {
    match path
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "mp3" => "audio/mpeg",
        "ogg" | "oga" => "audio/ogg",
        "wav" => "audio/wav",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "wasm" => "application/wasm",
        "toml" | "txt" | "md" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[derive(Deserialize)]
struct Manifest {
    name: Option<String>,
    source_code_url: Option<String>,
    tag_name: Option<String>,
}

fn bounded_manifest_text(
    value: Option<String>,
    max: usize,
) -> Result<Option<String>, PackageError> {
    value
        .map(|value| {
            let trimmed = value.trim();
            if trimmed.is_empty() || trimmed.chars().count() > max {
                Err(PackageError::Manifest)
            } else {
                Ok(trimmed.to_owned())
            }
        })
        .transpose()
}

fn package_metadata(files: &[(String, String, Vec<u8>)]) -> Result<PackageMetadata, PackageError> {
    const MAX_MANIFEST_BYTES: usize = 64 * 1024;
    let manifest = files.iter().find(|(path, _, _)| path == "manifest.toml");
    let (name, source_code_url, version) = if let Some((_, _, bytes)) = manifest {
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(PackageError::Manifest);
        }
        let text = std::str::from_utf8(bytes).map_err(|_| PackageError::Manifest)?;
        let parsed: Manifest = toml::from_str(text).map_err(|_| PackageError::Manifest)?;
        let name = bounded_manifest_text(parsed.name, 120)?;
        let source_code_url = bounded_manifest_text(parsed.source_code_url, 2048)?;
        if let Some(source) = source_code_url.as_deref() {
            let parsed = url::Url::parse(source).map_err(|_| PackageError::Manifest)?;
            if !matches!(parsed.scheme(), "http" | "https")
                || parsed.host_str().is_none()
                || !parsed.username().is_empty()
                || parsed.password().is_some()
            {
                return Err(PackageError::Manifest);
            }
        }
        (
            name,
            source_code_url,
            bounded_manifest_text(parsed.tag_name, 120)?,
        )
    } else {
        (None, None, None)
    };
    let icon_path = ["icon.png", "icon.jpg"]
        .into_iter()
        .find(|candidate| files.iter().any(|(path, _, _)| path == candidate))
        .map(str::to_owned);
    Ok(PackageMetadata {
        name,
        source_code_url,
        version,
        icon_path,
    })
}

#[must_use]
pub fn package_fallback_name(filename: &str) -> String {
    let leaf = filename
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(filename)
        .trim();
    let stem = leaf
        .strip_suffix(".xdc")
        .or_else(|| leaf.strip_suffix(".XDC"))
        .unwrap_or(leaf)
        .trim();
    let fallback = if stem.is_empty() { "Webxdc app" } else { stem };
    fallback.chars().take(120).collect()
}

#[must_use]
pub fn package_filename(filename: &str) -> String {
    let leaf = filename
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(filename)
        .trim();
    let leaf = if leaf.is_empty() {
        "application.xdc"
    } else {
        leaf
    };
    leaf.chars().take(255).collect()
}

/// Verifies and expands one package under strict ZIP-bomb and path controls.
pub fn validate_package(
    bytes: &[u8],
    advertised_digest: Option<&str>,
) -> Result<ValidatedPackage, PackageError> {
    validate_package_with_limits(bytes, advertised_digest, webxdc::Limits::default())
}

pub fn validate_package_with_limits(
    bytes: &[u8],
    advertised_digest: Option<&str>,
    limits: webxdc::Limits,
) -> Result<ValidatedPackage, PackageError> {
    if bytes.is_empty() || bytes.len() > limits.bundle_bytes() {
        return Err(PackageError::CompressedSize(limits.bundle_mb));
    }
    let digest = digest_multibase(bytes);
    if advertised_digest.is_some_and(|expected| expected != digest) {
        return Err(PackageError::Digest);
    }
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|_| PackageError::Zip)?;
    if archive.len() > MAX_FILES {
        return Err(PackageError::ResourceLimit);
    }
    let mut total = 0u64;
    let mut names = HashSet::with_capacity(archive.len());
    let mut normalized_names = HashSet::with_capacity(archive.len());
    let mut files = Vec::with_capacity(archive.len());
    let mut has_index = false;
    for index in 0..archive.len() {
        let file = archive.by_index(index).map_err(|_| PackageError::Zip)?;
        let name = file.name().to_owned();
        if file.is_dir() {
            continue;
        }
        let normalized: String = name.nfc().collect();
        if !safe_entry(&name) || !names.insert(name.clone()) || !normalized_names.insert(normalized)
        {
            return Err(PackageError::UnsafeEntry(name));
        }
        if let Some(mode) = file.unix_mode() {
            // Only regular files (or entries with no file-kind bits). Symlinks,
            // devices, sockets and FIFOs are never materialized.
            let kind = mode & 0o170_000;
            if kind != 0 && kind != 0o100_000 {
                return Err(PackageError::UnsafeEntry(name));
            }
        }
        let size = file.size();
        total = total.checked_add(size).ok_or(PackageError::ResourceLimit)?;
        if size > u64::try_from(limits.file_mb).unwrap_or(0) * 1024 * 1024 {
            return Err(PackageError::SizeLimit {
                kind: "individual file",
                limit_mb: limits.file_mb,
            });
        }
        if total > u64::try_from(limits.expanded_mb).unwrap_or(0) * 1024 * 1024 {
            return Err(PackageError::SizeLimit {
                kind: "expanded package",
                limit_mb: limits.expanded_mb,
            });
        }
        let compressed = file.compressed_size().max(1);
        if size / compressed > MAX_COMPRESSION_RATIO {
            return Err(PackageError::ResourceLimit);
        }
        let mut contents = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
        file.take(size + 1)
            .read_to_end(&mut contents)
            .map_err(|_| PackageError::Zip)?;
        if contents.len() as u64 != size {
            return Err(PackageError::Zip);
        }
        has_index |= name == "index.html";
        files.push((name.clone(), package_media_type(&name).to_owned(), contents));
    }
    if !has_index {
        return Err(PackageError::MissingEntry);
    }
    let metadata = package_metadata(&files)?;
    Ok(ValidatedPackage {
        digest_multibase: digest,
        files,
        metadata,
    })
}

/// Hashing and decompression must not stall an async worker on large games.
async fn validate_package_async(
    bytes: &[u8],
    digest: Option<&str>,
    limits: webxdc::Limits,
) -> Result<ValidatedPackage, ApiError> {
    static GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
    let permit = GATE
        .acquire()
        .await
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    let bytes = bytes.to_vec();
    let digest = digest.map(str::to_owned);
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        validate_package_with_limits(&bytes, digest.as_deref(), limits)
    })
    .await
    .map_err(|error| ApiError::Internal(Box::new(error)))?
    .map_err(|error| ApiError::Unprocessable(error.to_string()))
}

#[derive(Debug)]
pub struct LibraryUpload<'a> {
    pub actor: &'a Account,
    pub bundle_name: &'a str,
    pub bundle_bytes: &'a [u8],
    pub summary: &'a str,
    pub category: Option<&'a str>,
}

fn library_text_valid(summary: &str, category: Option<&str>) -> bool {
    summary.chars().count() <= 2000
        && category.is_none_or(|value| {
            let value = value.trim();
            !value.is_empty() && value.chars().count() <= 80
        })
}

fn new_library_version<'a>(
    package: &'a ValidatedPackage,
    filename: &'a str,
    canonical_name: &'a str,
    bundle_bytes: &'a [u8],
    source_url: Option<&'a str>,
) -> webxdc::NewLibraryVersion<'a> {
    webxdc::NewLibraryVersion {
        digest_multibase: &package.digest_multibase,
        version: package.metadata.version.as_deref().unwrap_or_default(),
        filename,
        manifest_name: canonical_name,
        source_code_url: package.metadata.source_code_url.as_deref(),
        icon_path: package.metadata.icon_path.as_deref(),
        source_url,
        bundle_bytes,
        files: &package.files,
    }
}

async fn validate_library_upload(
    state: &AppState,
    upload: &LibraryUpload<'_>,
) -> Result<(ValidatedPackage, String, String), ApiError> {
    if !library_text_valid(upload.summary, upload.category) {
        return Err(ApiError::Unprocessable(
            "Invalid Webxdc library description or category".into(),
        ));
    }
    let limits = webxdc::limits(&state.pool).await?;
    let package = validate_package_async(upload.bundle_bytes, None, limits).await?;
    let filename = package_filename(upload.bundle_name);
    let name = package
        .metadata
        .name
        .clone()
        .unwrap_or_else(|| package_fallback_name(&filename));
    Ok((package, filename, name))
}

pub async fn save_personal_app(
    state: &AppState,
    upload: LibraryUpload<'_>,
) -> Result<webxdc::LibraryApp, ApiError> {
    if !plamenu_db::role::for_account(&state.pool, upload.actor.id)
        .await?
        .is_some_and(|role| role.can(plamenu_db::role::permission::CREATE_WEBXDC))
    {
        return Err(ApiError::Forbidden(
            "Your role cannot save Webxdc apps".into(),
        ));
    }
    let (package, filename, name) = validate_library_upload(state, &upload).await?;
    webxdc::create_library_app(
        &state.pool,
        webxdc::NewLibraryApp {
            owner_account_id: Some(upload.actor.id),
            name: &name,
            summary: upload.summary,
            category: upload.category.map(str::trim),
            visibility: "private",
            source_kind: "upload",
            source_url: None,
            catalog_source_id: None,
            external_app_id: None,
            promoted_from_app_id: None,
            created_by_account_id: upload.actor.id,
            version: new_library_version(&package, &filename, &name, upload.bundle_bytes, None),
        },
    )
    .await
    .map_err(protocol_db_error)
}

pub async fn save_instance_app(
    state: &AppState,
    upload: LibraryUpload<'_>,
    visibility: &str,
) -> Result<webxdc::LibraryApp, ApiError> {
    if !plamenu_db::role::for_account(&state.pool, upload.actor.id)
        .await?
        .is_some_and(|role| role.can(plamenu_db::role::permission::MANAGE_WEBXDC))
    {
        return Err(ApiError::Forbidden(
            "Missing Webxdc management permission".into(),
        ));
    }
    let (package, filename, name) = validate_library_upload(state, &upload).await?;
    webxdc::create_library_app(
        &state.pool,
        webxdc::NewLibraryApp {
            owner_account_id: None,
            name: &name,
            summary: upload.summary,
            category: upload.category.map(str::trim),
            visibility,
            source_kind: "upload",
            source_url: None,
            catalog_source_id: None,
            external_app_id: None,
            promoted_from_app_id: None,
            created_by_account_id: upload.actor.id,
            version: new_library_version(&package, &filename, &name, upload.bundle_bytes, None),
        },
    )
    .await
    .map_err(protocol_db_error)
}

pub async fn add_personal_app_version(
    state: &AppState,
    app_id: i64,
    upload: LibraryUpload<'_>,
) -> Result<webxdc::LibraryApp, ApiError> {
    let (package, filename, name) = validate_library_upload(state, &upload).await?;
    let version = new_library_version(&package, &filename, &name, upload.bundle_bytes, None);
    webxdc::add_library_version(
        &state.pool,
        app_id,
        Some(upload.actor.id),
        upload.actor.id,
        version,
    )
    .await
    .map_err(protocol_db_error)
}

pub async fn add_instance_app_version(
    state: &AppState,
    app_id: i64,
    upload: LibraryUpload<'_>,
) -> Result<webxdc::LibraryApp, ApiError> {
    if !plamenu_db::role::for_account(&state.pool, upload.actor.id)
        .await?
        .is_some_and(|role| role.can(plamenu_db::role::permission::MANAGE_WEBXDC))
    {
        return Err(ApiError::Forbidden(
            "Missing Webxdc management permission".into(),
        ));
    }
    let (package, filename, name) = validate_library_upload(state, &upload).await?;
    let version = new_library_version(&package, &filename, &name, upload.bundle_bytes, None);
    webxdc::add_library_version(&state.pool, app_id, None, upload.actor.id, version)
        .await
        .map_err(protocol_db_error)
}

#[derive(Deserialize)]
struct XdcgetEntry {
    app_id: String,
    #[serde(default)]
    tag_name: String,
    url: String,
    #[serde(default)]
    date: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    source_code_url: String,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    icon_relname: Option<String>,
    name: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    size: Option<i64>,
}

fn is_hidden_service_host(host: &str) -> bool {
    host.rsplit_once('.').is_some_and(|(_, suffix)| {
        suffix.eq_ignore_ascii_case("onion") || suffix.eq_ignore_ascii_case("i2p")
    })
}

pub(crate) fn catalog_url(value: &str) -> Option<String> {
    let parsed = url::Url::parse(value).ok()?;
    let host = parsed.host_str()?;
    let allowed =
        parsed.scheme() == "https" || (parsed.scheme() == "http" && is_hidden_service_host(host));
    (allowed
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.fragment().is_none())
    .then(|| parsed.to_string())
}

fn catalog_asset_url(base_url: &str, value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    let absolute = url::Url::parse(base_url).ok()?.join(value).ok()?;
    catalog_url(absolute.as_str())
}

fn json_content_type(content_type: &str) -> bool {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    mime == "application/json" || mime.ends_with("+json")
}

pub async fn refresh_catalog_source(state: &AppState, source_id: i64) -> Result<usize, ApiError> {
    let result = async {
        let source = webxdc::catalog_source(&state.pool, source_id)
            .await?
            .filter(|source| source.enabled)
            .ok_or(ApiError::NotFound)?;
        let fetched = state
            .federation
            .fetch_page(&source.feed_url, "application/json")
            .await
            .map_err(|error| ApiError::BadGateway(error.to_string()))?;
        if !json_content_type(&fetched.content_type) {
            return Err(ApiError::Unprocessable(
                "The catalog did not return a JSON content type".into(),
            ));
        }
        let entries: Vec<XdcgetEntry> = serde_json::from_str(&fetched.body).map_err(|_| {
            ApiError::Unprocessable("The catalog JSON is malformed or too large".into())
        })?;
        if entries.len() > 2000 {
            return Err(ApiError::PayloadTooLargeWithMessage(
                "An external Webxdc catalog may contain at most 2000 apps.".into(),
            ));
        }
        let mut ids = HashSet::with_capacity(entries.len());
        let mut candidates = Vec::with_capacity(entries.len());
        for entry in entries {
            let app_id = entry.app_id.trim();
            let name = entry.name.trim();
            let bundle_url = catalog_url(entry.url.trim()).ok_or_else(|| {
                ApiError::Unprocessable("The catalog contains an unsafe bundle URL".into())
            })?;
            if app_id.is_empty()
                || app_id.chars().count() > 240
                || !ids.insert(app_id.to_owned())
                || name.is_empty()
                || name.chars().count() > 120
                || entry.tag_name.chars().count() > 120
                || entry.description.chars().count() > 2000
                || entry.category.chars().count() > 80
                || entry.size.is_some_and(|size| size < 0)
            {
                return Err(ApiError::Unprocessable(
                    "The catalog contains invalid or duplicate app metadata".into(),
                ));
            }
            let source_code_url = if entry.source_code_url.trim().is_empty() {
                None
            } else {
                catalog_url(entry.source_code_url.trim())
            };
            let icon_url = catalog_asset_url(&fetched.final_url, entry.icon.as_deref())
                .or_else(|| catalog_asset_url(&fetched.final_url, entry.icon_relname.as_deref()));
            let published_at = if entry.date.trim().is_empty() {
                None
            } else {
                Some(
                    OffsetDateTime::parse(entry.date.trim(), &Rfc3339).map_err(|_| {
                        ApiError::Unprocessable("The catalog contains an invalid date".into())
                    })?,
                )
            };
            candidates.push(webxdc::NewCatalogCandidate {
                external_app_id: app_id.to_owned(),
                version: entry.tag_name.trim().to_owned(),
                bundle_url,
                name: name.to_owned(),
                summary: entry.description.trim().to_owned(),
                category: (!entry.category.trim().is_empty())
                    .then(|| entry.category.trim().to_owned()),
                source_code_url,
                icon_url,
                advertised_size: entry.size,
                published_at,
            });
        }
        webxdc::replace_catalog_candidates(&state.pool, source_id, &candidates).await?;
        Ok(candidates.len())
    }
    .await;
    if let Err(error) = &result {
        let _ =
            webxdc::record_catalog_source_error(&state.pool, source_id, &error.to_string()).await;
    }
    result
}

fn webxdc_download_content_type(content_type: &str) -> bool {
    matches!(
        content_type
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        MEDIA_TYPE | LEGACY_MEDIA_TYPE | "application/zip" | "application/octet-stream"
    )
}

pub async fn import_catalog_candidate(
    state: &AppState,
    actor: &Account,
    source_id: i64,
    external_app_id: &str,
) -> Result<webxdc::LibraryApp, ApiError> {
    if !plamenu_db::role::for_account(&state.pool, actor.id)
        .await?
        .is_some_and(|role| role.can(plamenu_db::role::permission::MANAGE_WEBXDC))
    {
        return Err(ApiError::Forbidden(
            "Missing Webxdc management permission".into(),
        ));
    }
    import_catalog_for_owner(state, actor, source_id, external_app_id, None).await
}

pub async fn import_personal_catalog_candidate(
    state: &AppState,
    actor: &Account,
    source_id: i64,
    external_app_id: &str,
) -> Result<webxdc::LibraryApp, ApiError> {
    if !plamenu_db::role::for_account(&state.pool, actor.id)
        .await?
        .is_some_and(|role| role.can(plamenu_db::role::permission::CREATE_WEBXDC))
    {
        return Err(ApiError::Forbidden(
            "Your role cannot save Webxdc apps".into(),
        ));
    }
    import_catalog_for_owner(state, actor, source_id, external_app_id, Some(actor.id)).await
}

async fn import_catalog_for_owner(
    state: &AppState,
    actor: &Account,
    source_id: i64,
    external_app_id: &str,
    owner_account_id: Option<i64>,
) -> Result<webxdc::LibraryApp, ApiError> {
    let source = webxdc::catalog_source(&state.pool, source_id)
        .await?
        .filter(|source| source.enabled)
        .ok_or(ApiError::NotFound)?;
    let candidate = webxdc::catalog_candidate(&state.pool, source_id, external_app_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let limits = webxdc::limits(&state.pool).await?;
    let fetched = state
        .federation
        .fetch_media_limited(&candidate.bundle_url, limits.bundle_bytes() as u64)
        .await
        .map_err(|error| ApiError::BadGateway(error.to_string()))?;
    if !webxdc_download_content_type(&fetched.content_type) {
        return Err(ApiError::Unprocessable(
            "The catalog bundle has an unsupported content type".into(),
        ));
    }
    let package = validate_package_async(&fetched.bytes, None, limits).await?;
    let filename = url::Url::parse(&fetched.final_url)
        .ok()
        .and_then(|url| {
            url.path_segments()
                .and_then(Iterator::last)
                .map(package_filename)
        })
        .unwrap_or_else(|| "application.xdc".to_owned());
    let name = package
        .metadata
        .name
        .clone()
        .unwrap_or_else(|| package_fallback_name(&filename));
    let version_label = package
        .metadata
        .version
        .as_deref()
        .unwrap_or(candidate.version.as_str());
    let version = webxdc::NewLibraryVersion {
        digest_multibase: &package.digest_multibase,
        version: version_label,
        filename: &filename,
        manifest_name: &name,
        source_code_url: package.metadata.source_code_url.as_deref(),
        icon_path: package.metadata.icon_path.as_deref(),
        source_url: Some(&fetched.final_url),
        bundle_bytes: &fetched.bytes,
        files: &package.files,
    };
    if let Some(existing) =
        webxdc::external_library_app(&state.pool, source_id, external_app_id, owner_account_id)
            .await?
    {
        if existing.digest_multibase == package.digest_multibase {
            return Ok(existing);
        }
        return webxdc::add_library_version(
            &state.pool,
            existing.id,
            owner_account_id,
            actor.id,
            version,
        )
        .await
        .map_err(protocol_db_error);
    }
    webxdc::create_library_app(
        &state.pool,
        webxdc::NewLibraryApp {
            owner_account_id,
            name: &name,
            summary: &candidate.summary,
            category: candidate.category.as_deref(),
            visibility: if owner_account_id.is_some() {
                "private"
            } else {
                "hidden"
            },
            source_kind: "external",
            source_url: Some(&source.feed_url),
            catalog_source_id: Some(source.id),
            external_app_id: Some(&candidate.external_app_id),
            promoted_from_app_id: None,
            created_by_account_id: actor.id,
            version,
        },
    )
    .await
    .map_err(protocol_db_error)
}

fn https_uri(value: &Value, field: &str) -> Result<String, ApiError> {
    let uri = value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::BadRequest(format!("Webxdc actor has no {field}")))?;
    let parsed: url::Url = uri
        .parse()
        .map_err(|_| ApiError::BadRequest(format!("Webxdc actor has an invalid {field}")))?;
    if parsed.scheme() != "https" || parsed.host_str().is_none() {
        return Err(ApiError::BadRequest(format!(
            "Webxdc actor {field} is not HTTPS"
        )));
    }
    Ok(uri.to_owned())
}

fn type_contains(value: &Value, expected: &str) -> bool {
    match value.get("type") {
        Some(Value::String(kind)) => kind == expected,
        Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind.as_str() == Some(expected)),
        _ => false,
    }
}

fn values(value: Option<&Value>) -> Vec<&Value> {
    match value {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(value) => vec![value],
        None => Vec::new(),
    }
}

fn parse_remote_descriptor(raw: Value) -> Result<RemoteDescriptor, ApiError> {
    if !type_contains(&raw, "Group") || !type_contains(&raw, "WebxdcSession") {
        return Err(ApiError::BadRequest(
            "actor must have both Group and WebxdcSession types".into(),
        ));
    }
    let coordinator_uri = https_uri(&raw, "id")?;
    for field in ["inbox", "outbox", "followers"] {
        https_uri(&raw, field)?;
    }
    if raw.get("webxdcProtocol").and_then(Value::as_str) != Some(PROTOCOL) {
        return Err(ApiError::BadRequest(
            "actor has the wrong Webxdc protocol marker".into(),
        ));
    }
    let creator_uri = raw
        .get("attributedTo")
        .and_then(plamenu_ap::activity::id_of)
        .ok_or_else(|| ApiError::BadRequest("Webxdc actor has no creator".into()))?
        .to_owned();
    let attachments: Vec<&Value> = values(raw.get("attachment"))
        .into_iter()
        .filter(|item| {
            item.get("type").and_then(Value::as_str) == Some("Document")
                && matches!(
                    item.get("mediaType").and_then(Value::as_str),
                    Some(MEDIA_TYPE | LEGACY_MEDIA_TYPE)
                )
        })
        .collect();
    if attachments.len() != 1 {
        return Err(ApiError::BadRequest(
            "Webxdc actor must have exactly one qualifying bundle".into(),
        ));
    }
    let bundle = attachments[0];
    let bundle_id = https_uri(bundle, "id")?;
    let bundle_url = https_uri(bundle, "url")?;
    let digest_multibase = bundle
        .get("digestMultibase")
        .and_then(Value::as_str)
        .filter(|value| value.starts_with('u'))
        .ok_or_else(|| ApiError::BadRequest("Webxdc bundle has no digestMultibase".into()))?
        .to_owned();
    let send_update_interval = raw
        .get("sendUpdateInterval")
        .and_then(Value::as_i64)
        .and_then(|value| i32::try_from(value).ok())
        .filter(|value| *value >= 0)
        .ok_or_else(|| ApiError::BadRequest("invalid sendUpdateInterval".into()))?;
    let send_update_max_size = raw
        .get("sendUpdateMaxSize")
        .and_then(Value::as_i64)
        .and_then(|value| i32::try_from(value).ok())
        .filter(|value| (1..=MAX_UPDATE_BYTES).contains(value))
        .ok_or_else(|| ApiError::BadRequest("invalid sendUpdateMaxSize".into()))?;
    let parse_time = |field: &str| -> Result<Option<OffsetDateTime>, ApiError> {
        raw.get(field)
            .and_then(Value::as_str)
            .map(|value| {
                OffsetDateTime::parse(value, &Rfc3339)
                    .map_err(|_| ApiError::BadRequest(format!("invalid Webxdc {field}")))
            })
            .transpose()
    };
    Ok(RemoteDescriptor {
        name: raw
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("Webxdc session")
            .to_owned(),
        summary: raw
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        bundle_name: bundle
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("application.xdc")
            .to_owned(),
        bundle_media_type: bundle["mediaType"]
            .as_str()
            .unwrap_or(MEDIA_TYPE)
            .to_owned(),
        published_at: parse_time("published")?.unwrap_or_else(OffsetDateTime::now_utc),
        ended_at: parse_time("endTime")?,
        raw,
        coordinator_uri,
        creator_uri,
        bundle_id,
        bundle_url,
        digest_multibase,
        send_update_interval,
        send_update_max_size,
    })
}

fn session_self_addr(state: &AppState, participant_uri: &str, session_uri: &str) -> String {
    let secret = state
        .config
        .encryption_secret
        .as_ref()
        .expect("validated startup configuration has an encryption secret")
        .expose();
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts keys of any length");
    mac.update(participant_uri.as_bytes());
    mac.update(&[0]);
    mac.update(session_uri.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

/// Creates a hosted session with its own signing actor and admits the creator
/// through the same durable membership state used for remote participants.
pub async fn create_local(state: &AppState, input: CreateLocal<'_>) -> Result<Session, ApiError> {
    validate_local_session_input(
        state,
        input.creator,
        input.name,
        input.summary,
        input.membership_policy,
        input.send_update_interval,
        input.send_update_max_size,
    )
    .await?;
    let limits = webxdc::limits(&state.pool).await?;
    let package = validate_package_async(input.bundle_bytes, None, limits).await?;
    create_local_with_package(
        state,
        input.creator,
        input.name,
        input.summary,
        input.bundle_name,
        input.membership_policy,
        input.send_update_interval,
        input.send_update_max_size,
        webxdc::SessionPackage::Upload {
            digest_multibase: &package.digest_multibase,
            bundle_bytes: input.bundle_bytes,
            files: &package.files,
        },
    )
    .await
}

pub async fn create_local_from_library(
    state: &AppState,
    input: CreateLocalFromLibrary<'_>,
) -> Result<Session, ApiError> {
    validate_local_session_input(
        state,
        input.creator,
        input.name,
        input.summary,
        input.membership_policy,
        input.send_update_interval,
        input.send_update_max_size,
    )
    .await?;
    let app = webxdc::usable_version(&state.pool, input.version_id, input.creator.id)
        .await?
        .ok_or(ApiError::NotFound)?;
    create_local_with_package(
        state,
        input.creator,
        input.name,
        input.summary,
        &app.filename,
        input.membership_policy,
        input.send_update_interval,
        input.send_update_max_size,
        webxdc::SessionPackage::Existing {
            digest_multibase: &app.digest_multibase,
        },
    )
    .await
}

async fn validate_local_session_input(
    state: &AppState,
    creator: &Account,
    name: &str,
    summary: &str,
    membership_policy: &str,
    send_update_interval: i32,
    send_update_max_size: i32,
) -> Result<(), ApiError> {
    if !plamenu_db::role::for_account(&state.pool, creator.id)
        .await?
        .is_some_and(|role| role.can(plamenu_db::role::permission::CREATE_WEBXDC))
    {
        return Err(ApiError::Forbidden(
            "Your role cannot create Webxdc sessions".into(),
        ));
    }
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 120 {
        return Err(ApiError::Unprocessable(
            "A session name of at most 120 characters is required".into(),
        ));
    }
    if summary.chars().count() > 2000 {
        return Err(ApiError::Unprocessable(
            "The session description is too long".into(),
        ));
    }
    if !matches!(membership_policy, "open" | "approval") {
        return Err(ApiError::Unprocessable(
            "Unknown session membership policy".into(),
        ));
    }
    if send_update_interval < 0 || send_update_max_size <= 0 {
        return Err(ApiError::Unprocessable(
            "Invalid durable update limits".into(),
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn create_local_with_package(
    state: &AppState,
    creator: &Account,
    name: &str,
    summary: &str,
    bundle_name: &str,
    membership_policy: &str,
    send_update_interval: i32,
    send_update_max_size: i32,
    package: webxdc::SessionPackage<'_>,
) -> Result<Session, ApiError> {
    let name = name.trim();
    let session_id = id::next();
    let session_uri = format!("https://{}/webxdc/{session_id}", state.config.domain);
    let bundle_url = format!("{session_uri}/bundle.xdc");
    let creator_uri = crate::entities::account_uri(&state.config.domain, creator);
    let self_addr = session_self_addr(state, &creator_uri, &session_uri);
    let rsa = crate::auth::generate_keypair_gated().await?;
    let ed25519 = plamenu_ap::keys::generate_ed25519_keypair();
    let keyring = state.federation_keyring.as_deref().ok_or_else(|| {
        ApiError::Internal(Box::new(
            crate::crypto::KeyEncryptionError::MissingConfiguration,
        ))
    })?;
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let (account, session) = webxdc::create_local_tx(
        &mut tx,
        webxdc::NewLocalSession {
            session_id,
            name,
            summary: summary.trim(),
            coordinator_uri: &session_uri,
            creator_account_id: creator.id,
            creator_uri: &creator_uri,
            bundle_id: &bundle_url,
            bundle_url: &bundle_url,
            bundle_name,
            bundle_media_type: MEDIA_TYPE,
            package,
            send_update_interval,
            send_update_max_size,
            membership_policy,
            public_key_pem: &rsa.public_pem,
            self_addr: &self_addr,
        },
    )
    .await
    .map_err(protocol_db_error)?;
    crate::key_store::provision_account_tx(
        &mut tx,
        keyring,
        &state.config.domain,
        &account,
        &rsa,
        &ed25519,
    )
    .await
    .map_err(|error| ApiError::Internal(Box::new(error)))?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(session)
}

/// Resolves, validates, downloads, verifies and caches a public remote session.
/// Merely resolving a timeline invitation never calls this; the explicit open
/// flow does.
pub async fn fetch_remote(state: &AppState, uri: &str) -> Result<Session, ApiError> {
    if let Some(cached) = webxdc::find_by_uri(&state.pool, uri).await? {
        return Ok(cached);
    }
    if webxdc::tombstone_by_uri(&state.pool, uri).await?.is_some() {
        return Err(ApiError::Gone);
    }
    let raw = state
        .federation
        .fetch_object(uri)
        .await
        .map_err(|error| ApiError::BadGateway(error.to_string()))?;
    let descriptor = parse_remote_descriptor(raw)?;
    let limits = webxdc::limits(&state.pool).await?;
    let fetched = state
        .federation
        .fetch_media_limited(&descriptor.bundle_url, limits.bundle_bytes() as u64)
        .await
        .map_err(|error| ApiError::BadGateway(error.to_string()))?;
    let package =
        validate_package_async(&fetched.bytes, Some(&descriptor.digest_multibase), limits).await?;

    // The generic account store understands Group but not the FEP's required
    // additional type. Normalize only that parser view; the original document
    // remains the source of session metadata above.
    let mut actor_view = descriptor.raw.clone();
    actor_view["type"] = Value::String("Group".into());
    if actor_view
        .get("preferredUsername")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .is_empty()
    {
        actor_view["preferredUsername"] = Value::String(format!(
            "webxdc_{}",
            &descriptor.digest_multibase[1..descriptor.digest_multibase.len().min(13)]
        ));
    }
    let actor: plamenu_ap::actor::RemoteActor =
        serde_json::from_value(actor_view).map_err(|error| {
            ApiError::BadRequest(format!("invalid Webxdc coordinator actor: {error}"))
        })?;
    let account = crate::remote::refresh_remote_actor(state, &actor).await?;
    webxdc::store_remote(
        &state.pool,
        webxdc::NewRemoteSession {
            account_id: account.id,
            creator_uri: &descriptor.creator_uri,
            name: &descriptor.name,
            summary: &descriptor.summary,
            coordinator_uri: &descriptor.coordinator_uri,
            bundle_id: &descriptor.bundle_id,
            bundle_url: &descriptor.bundle_url,
            bundle_name: &descriptor.bundle_name,
            bundle_media_type: &descriptor.bundle_media_type,
            digest_multibase: &descriptor.digest_multibase,
            bundle_bytes: &fetched.bytes,
            send_update_interval: descriptor.send_update_interval,
            send_update_max_size: descriptor.send_update_max_size,
            published_at: descriptor.published_at,
            ended_at: descriptor.ended_at,
            files: &package.files,
        },
    )
    .await
    .map_err(protocol_db_error)
}

fn context() -> Value {
    json!([plamenu_ap::AS_CONTEXT, CONTEXT])
}

pub async fn actor_document(state: &AppState, session: &Session) -> Result<Value, ApiError> {
    let account = account::find_by_id(&state.pool, session.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let keys = actor_key::usable_for_account(&state.pool, account.id).await?;
    let rsa = keys.iter().find(|key| key.algorithm == "rsa");
    let ed = keys.iter().find(|key| key.algorithm == "ed25519");
    let followers = format!("{}/followers", session.coordinator_uri);
    let published = session
        .published_at
        .format(&Rfc3339)
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    let mut actor = json!({
        "@context": context(),
        "id": session.coordinator_uri,
        "type": ["Group", "WebxdcSession"],
        "name": session.name,
        "summary": session.summary,
        "attributedTo": session.creator_uri,
        "inbox": format!("{}/inbox", session.coordinator_uri),
        "outbox": format!("{}/outbox", session.coordinator_uri),
        "followers": followers,
        "url": {"type": "Link", "href": session.coordinator_uri, "mediaType": "text/html"},
        "webxdcProtocol": PROTOCOL,
        "attachment": {
            "id": session.bundle_id,
            "type": "Document",
            "name": session.bundle_name,
            "mediaType": session.bundle_media_type,
            "url": session.bundle_url,
            "digestMultibase": session.digest_multibase,
        },
        "sendUpdateInterval": session.send_update_interval,
        "sendUpdateMaxSize": session.send_update_max_size,
        "published": published,
    });
    if let Some(rsa) = rsa {
        actor["publicKey"] = json!({
            "id": rsa.key_uri,
            "owner": session.coordinator_uri,
            "publicKeyPem": rsa.public_key,
        });
    }
    if let Some(ed) = ed {
        actor["assertionMethod"] = json!([{
            "id": ed.key_uri,
            "type": "Multikey",
            "controller": session.coordinator_uri,
            "publicKeyMultibase": ed.public_key,
        }]);
    }
    if let Some(ended) = session.ended_at {
        let ended = ended
            .format(&Rfc3339)
            .map_err(|error| ApiError::Internal(Box::new(error)))?;
        actor["updated"] = Value::String(ended.clone());
        actor["endTime"] = Value::String(ended);
    }
    Ok(actor)
}

#[must_use]
pub fn invitation_link(session: &Session) -> Value {
    json!({
        "type": "Link",
        "href": session.coordinator_uri,
        "mediaType": "text/html",
        "name": format!("Open {}", session.name),
        "rel": OPEN_REL,
    })
}

/// Adds the progressive-enhancement invitation fields to an otherwise ordinary
/// Note/Create. The ordinary HTTPS anchor in `content` remains the fallback.
pub fn enhance_invitation(value: &mut Value, session_uri: &str, session_name: &str) {
    let is_create = value.get("type").and_then(Value::as_str) == Some("Create");
    let cc = {
        let note = if is_create {
            &mut value["object"]
        } else {
            &mut *value
        };
        note["audience"] = Value::String(session_uri.to_owned());
        let link = json!({
            "type": "Link", "href": session_uri, "mediaType": "text/html",
            "name": format!("Open {session_name}"), "rel": OPEN_REL,
        });
        match note.get_mut("attachment") {
            Some(Value::Array(items)) => items.push(link),
            _ => note["attachment"] = json!([link]),
        }
        if let Some(items) = note.get_mut("cc").and_then(Value::as_array_mut)
            && !items.iter().any(|item| item.as_str() == Some(session_uri))
        {
            items.push(Value::String(session_uri.to_owned()));
        }
        note["cc"].clone()
    };
    if is_create {
        value["audience"] = Value::String(session_uri.to_owned());
        value["cc"] = cc;
    }
}

fn follow_activity(participant_uri: &str, session_uri: &str, follow_id: &str) -> Value {
    json!({
        "@context": context(),
        "id": follow_id,
        "type": "Follow",
        "actor": participant_uri,
        "object": session_uri,
        "context": session_uri,
        "audience": session_uri,
        "to": session_uri,
        "webxdcProtocol": PROTOCOL,
    })
}

fn accept_activity(session: &Session, membership: &Membership) -> Value {
    json!({
        "@context": context(),
        "id": format!("{}/activities/accept-{}", session.coordinator_uri, id::next()),
        "type": "Accept",
        "actor": session.coordinator_uri,
        "object": membership.follow_id,
        "to": membership.participant_uri,
        "audience": session.coordinator_uri,
        "webxdcProtocol": PROTOCOL,
        "webxdcMaxSerial": membership.replay_boundary,
    })
}

fn reject_follow_activity(session: &Session, follow: &Value, participant_uri: &str) -> Value {
    let encoded = serde_json::to_vec(follow).unwrap_or_default();
    let marker = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(encoded));
    json!({
        "@context": context(),
        "id": format!("{}/activities/reject-{}", session.coordinator_uri, &marker[..24]),
        "type": "Reject",
        "actor": session.coordinator_uri,
        "object": follow,
        "to": participant_uri,
        "audience": session.coordinator_uri,
        "webxdcProtocol": PROTOCOL,
    })
}

#[must_use]
pub fn announce_activity(session: &Session, update: &Update) -> Value {
    json!({
        "@context": context(),
        "id": update.announce_id,
        "type": "Announce",
        "actor": session.coordinator_uri,
        "object": update.raw_create,
        "webxdcSerial": update.serial,
        "to": format!("{}/followers", session.coordinator_uri),
        "audience": session.coordinator_uri,
        "published": update.created_at.format(&Rfc3339).unwrap_or_default(),
    })
}

fn remove_activity(session: &Session, participant_uri: &str) -> Value {
    json!({
        "@context": context(),
        "id": format!("{}/activities/remove-{}", session.coordinator_uri, id::next()),
        "type": "Remove", "actor": session.coordinator_uri,
        "object": participant_uri,
        "target": format!("{}/followers", session.coordinator_uri),
        "to": participant_uri, "audience": session.coordinator_uri,
    })
}

#[must_use]
pub fn delete_activity(session: &Session, deleted_at: OffsetDateTime) -> Value {
    let deleted = deleted_at.format(&Rfc3339).unwrap_or_default();
    json!({
        "@context": context(),
        "id": format!("{}/activities/delete", session.coordinator_uri),
        "type": "Delete",
        "actor": session.coordinator_uri,
        "object": {
            "id": session.coordinator_uri,
            "type": "Tombstone",
            "formerType": ["Group", "WebxdcSession"],
            "deleted": deleted,
        },
        "to": format!("{}/followers", session.coordinator_uri),
        "audience": session.coordinator_uri,
        "webxdcProtocol": PROTOCOL,
    })
}

fn parse_update_create(
    raw: &Value,
    session: &Session,
    actor_uri: &str,
) -> Result<(String, String, Value), ApiError> {
    let create_id = raw
        .get("id")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| ApiError::BadRequest("Webxdc Create has no id".into()))?;
    if raw.get("actor").and_then(Value::as_str) != Some(actor_uri)
        || raw.get("to").and_then(plamenu_ap::activity::id_of)
            != Some(session.coordinator_uri.as_str())
        || raw.get("context").and_then(plamenu_ap::activity::id_of)
            != Some(session.coordinator_uri.as_str())
        || raw.get("audience").and_then(plamenu_ap::activity::id_of)
            != Some(session.coordinator_uri.as_str())
    {
        return Err(ApiError::BadRequest(
            "Webxdc Create has inconsistent session addressing".into(),
        ));
    }
    let object = raw
        .get("object")
        .filter(|v| v.is_object())
        .ok_or_else(|| ApiError::BadRequest("Webxdc Create has no object".into()))?;
    if !type_contains(object, "WebxdcUpdate")
        || object
            .get("attributedTo")
            .and_then(plamenu_ap::activity::id_of)
            != Some(actor_uri)
        || object.get("context").and_then(plamenu_ap::activity::id_of)
            != Some(session.coordinator_uri.as_str())
        || object.get("audience").and_then(plamenu_ap::activity::id_of)
            != Some(session.coordinator_uri.as_str())
    {
        return Err(ApiError::BadRequest(
            "invalid WebxdcUpdate attribution or session".into(),
        ));
    }
    let object_id = object
        .get("id")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| ApiError::BadRequest("WebxdcUpdate has no id".into()))?;
    let update = object
        .get("webxdcUpdate")
        .filter(|v| v.is_object())
        .ok_or_else(|| ApiError::BadRequest("webxdcUpdate must be an object".into()))?;
    validate_update(update, session.send_update_max_size)?;
    Ok((create_id.to_owned(), object_id.to_owned(), update.clone()))
}

pub fn validate_update(update: &Value, max_size: i32) -> Result<(), ApiError> {
    let object = update
        .as_object()
        .ok_or_else(|| ApiError::BadRequest("update must be an object".into()))?;
    if !object.contains_key("payload") {
        return Err(ApiError::BadRequest("update has no payload".into()));
    }
    if serde_json::to_vec(update).map_or(usize::MAX, |bytes| bytes.len())
        > usize::try_from(max_size).unwrap_or(0)
    {
        return Err(ApiError::PayloadTooLarge);
    }
    if let Some(href) = object.get("href") {
        let href = href
            .as_str()
            .ok_or_else(|| ApiError::BadRequest("update href must be text".into()))?;
        let path = href.split(['?', '#']).next().unwrap_or_default();
        if href.starts_with(['/', '\\'])
            || href.chars().any(char::is_control)
            || href.contains('\\')
            || url::Url::parse(href).is_ok()
            || path.split('/').any(|segment| segment == "..")
        {
            return Err(ApiError::BadRequest(
                "update href must be an in-app relative reference".into(),
            ));
        }
    }
    for key in ["info", "document", "summary"] {
        if let Some(value) = object.get(key) {
            let Some(text) = value.as_str() else {
                return Err(ApiError::BadRequest(format!("update {key} must be text")));
            };
            if text.chars().count() > 10_000
                || text.chars().any(|character| {
                    character.is_control() && !matches!(character, '\n' | '\r' | '\t')
                })
            {
                return Err(ApiError::BadRequest(format!(
                    "update {key} exceeds safe text limits"
                )));
            }
        }
    }
    if let Some(notify) = object.get("notify") {
        let notify = notify
            .as_object()
            .ok_or_else(|| ApiError::BadRequest("update notify must be an object".into()))?;
        if notify.len() > 256
            || notify.values().any(|value| {
                value
                    .as_str()
                    .is_none_or(|text| text.chars().count() > 1000)
            })
        {
            return Err(ApiError::BadRequest("update notify is malformed".into()));
        }
    }
    Ok(())
}

/// Starts a local participant's protocol-marked membership. Local coordinators
/// commit the same membership directly; remote coordinators receive a signed
/// Follow through the ordinary durable delivery queue.
pub async fn join(
    state: &AppState,
    session: &Session,
    participant: &Account,
) -> Result<Membership, ApiError> {
    if session.ended() {
        return Err(ApiError::Gone);
    }
    let participant_uri = crate::entities::account_uri(&state.config.domain, participant);
    let follow_id = format!(
        "{participant_uri}/webxdc-follows/{}-{}",
        session.id,
        id::next()
    );
    let self_addr = session_self_addr(state, &participant_uri, &session.coordinator_uri);
    let coordinator = account::find_by_id(&state.pool, session.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if coordinator.is_local() {
        if session.membership_policy == "approval" {
            return webxdc::request_membership(
                &state.pool,
                session.id,
                participant.id,
                &participant_uri,
                &follow_id,
                &self_addr,
            )
            .await
            .map_err(Into::into);
        }
        let mut tx = state
            .pool
            .begin()
            .await
            .map_err(plamenu_db::DbError::from)?;
        let membership = webxdc::accept_membership(
            &mut tx,
            session.id,
            participant.id,
            &participant_uri,
            &follow_id,
            &self_addr,
        )
        .await
        .map_err(protocol_db_error)?;
        tx.commit().await.map_err(plamenu_db::DbError::from)?;
        state.webxdc_realtime.refresh(session.id);
        return Ok(membership);
    }
    let membership = webxdc::request_membership(
        &state.pool,
        session.id,
        participant.id,
        &participant_uri,
        &follow_id,
        &self_addr,
    )
    .await?;
    job::enqueue(
        &state.pool,
        participant.id,
        &coordinator.inbox_url,
        &follow_activity(&participant_uri, &session.coordinator_uri, &follow_id),
    )
    .await?;
    state.webxdc_realtime.refresh(session.id);
    Ok(membership)
}

/// Approves a pending membership and queues Accept plus the complete replay in
/// the same transaction as the atomic boundary/participant commit.
pub async fn approve(
    state: &AppState,
    session: &Session,
    participant_id: i64,
) -> Result<Membership, ApiError> {
    let pending = webxdc::membership(&state.pool, session.id, participant_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if pending.accepted {
        return Ok(pending);
    }
    let participant = account::find_by_id(&state.pool, participant_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let membership = webxdc::accept_membership(
        &mut tx,
        session.id,
        participant.id,
        &pending.participant_uri,
        &pending.follow_id,
        &pending.self_addr,
    )
    .await
    .map_err(protocol_db_error)?;
    if !participant.is_local() {
        job::enqueue_tx(
            &mut *tx,
            session.account_id,
            &participant.inbox_url,
            &accept_activity(session, &membership),
        )
        .await?;
        for update in
            webxdc::updates_through(&mut tx, session.id, membership.replay_boundary).await?
        {
            job::enqueue_tx(
                &mut *tx,
                session.account_id,
                &participant.inbox_url,
                &announce_activity(session, &update),
            )
            .await?;
        }
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    state.webxdc_realtime.refresh(session.id);
    Ok(membership)
}

pub async fn leave(
    state: &AppState,
    session: &Session,
    participant: &Account,
) -> Result<(), ApiError> {
    if session.creator_account_id == Some(participant.id) {
        return Err(ApiError::Conflict(
            "The session creator must delete the session for everyone".into(),
        ));
    }
    let Some(membership) = webxdc::membership(&state.pool, session.id, participant.id).await?
    else {
        return Ok(());
    };
    let coordinator = account::find_by_id(&state.pool, session.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if coordinator.is_local() {
        webxdc::remove_membership(&state.pool, session.id, participant.id).await?;
    } else {
        let undo = json!({
            "@context": context(), "id": format!("{}/undo", membership.follow_id),
            "type": "Undo", "actor": membership.participant_uri,
            "object": membership.follow_id, "to": session.coordinator_uri,
            "audience": session.coordinator_uri,
        });
        let mut tx = state
            .pool
            .begin()
            .await
            .map_err(plamenu_db::DbError::from)?;
        job::enqueue_tx(&mut *tx, participant.id, &coordinator.inbox_url, &undo).await?;
        webxdc::remove_membership_tx(&mut tx, session.id, participant.id).await?;
        webxdc::discard_remote_if_unreferenced_tx(&mut tx, session.id).await?;
        tx.commit().await.map_err(plamenu_db::DbError::from)?;
    }
    state.webxdc_realtime.invalidate(
        session.id,
        Some(crate::webxdc_realtime::Peer::Account(participant.id)),
    );
    Ok(())
}

/// Remove a remote cache for every local participant, notifying its host.
pub async fn evict_remote(state: &AppState, session: &Session) -> Result<(), ApiError> {
    let coordinator = account::find_by_id(&state.pool, session.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if coordinator.is_local() {
        return Err(ApiError::Forbidden("This session is hosted locally".into()));
    }
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    if !webxdc::lock_live_tx(&mut tx, session.id).await? {
        return Ok(());
    }
    let memberships: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT m.participant_account_id, m.participant_uri, m.follow_id FROM webxdc_memberships m
         JOIN accounts a ON a.id=m.participant_account_id WHERE m.session_id=$1 AND a.domain IS NULL")
        .bind(session.id).fetch_all(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
    for (participant, uri, follow) in memberships {
        let undo = json!({"@context": context(), "id": format!("{follow}/undo"),
            "type": "Undo", "actor": uri, "object": follow, "to": session.coordinator_uri,
            "audience": session.coordinator_uri});
        job::enqueue_tx(&mut *tx, participant, &coordinator.inbox_url, &undo).await?;
        webxdc::remove_membership_tx(&mut tx, session.id, participant).await?;
    }
    webxdc::discard_remote_if_unreferenced_tx(&mut tx, session.id).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    state.webxdc_realtime.invalidate(session.id, None);
    Ok(())
}

fn authored_create(session: &Session, participant_uri: &str, update: &Value) -> Value {
    let marker = id::next();
    json!({
        "@context": context(),
        "id": format!("{participant_uri}/webxdc-activities/{marker}"),
        "type": "Create", "actor": participant_uri,
        "to": session.coordinator_uri, "context": session.coordinator_uri,
        "audience": session.coordinator_uri,
        "object": {
            "id": format!("{participant_uri}/webxdc-updates/{marker}"),
            "type": "WebxdcUpdate", "attributedTo": participant_uri,
            "context": session.coordinator_uri, "audience": session.coordinator_uri,
            "webxdcUpdate": update,
        },
        "published": OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_default(),
    })
}

pub async fn submit_update(
    state: &AppState,
    session: &Session,
    participant: &Account,
    update: Value,
) -> Result<Option<Update>, ApiError> {
    validate_update(&update, session.send_update_max_size)?;
    let membership = webxdc::membership(&state.pool, session.id, participant.id)
        .await?
        .filter(|membership| membership.accepted)
        .ok_or_else(|| ApiError::Forbidden("You have not joined this Webxdc session".into()))?;
    let create = authored_create(session, &membership.participant_uri, &update);
    let (create_id, object_id, value) =
        parse_update_create(&create, session, &membership.participant_uri)?;
    let coordinator = account::find_by_id(&state.pool, session.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !coordinator.is_local() {
        if !webxdc::reserve_remote_submission(
            &state.pool,
            session.id,
            participant.id,
            session.send_update_interval,
        )
        .await?
        {
            return Err(ApiError::TooManyRequests);
        }
        job::enqueue(&state.pool, participant.id, &coordinator.inbox_url, &create).await?;
        return Ok(None);
    }
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let (stored, inserted) = webxdc::sequence_update(
        &mut tx,
        webxdc::SequenceUpdate {
            session_id: session.id,
            participant_account_id: participant.id,
            create_id: &create_id,
            object_id: &object_id,
            actor_uri: &membership.participant_uri,
            raw_create: &create,
            webxdc_update: &value,
            announce_id_prefix: &session.coordinator_uri,
        },
    )
    .await
    .map_err(protocol_db_error)?;
    if inserted {
        let inboxes = webxdc::remote_inboxes_tx(&mut tx, session.id).await?;
        job::enqueue_many_tx(
            &mut *tx,
            session.account_id,
            &inboxes,
            &announce_activity(session, &stored),
        )
        .await?;
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(Some(stored))
}

/// Sequences a durable update authored by a coordinator-local browser guest.
/// Guest identities never become `ActivityPub` accounts, but their attributed
/// Creates are redistributed by the session coordinator like every other
/// accepted update.
pub async fn submit_guest_update(
    state: &AppState,
    session: &Session,
    guest: &webxdc::Guest,
    update: Value,
) -> Result<Update, ApiError> {
    validate_update(&update, session.send_update_max_size)?;
    if !guest.accepted || guest.session_id != session.id {
        return Err(ApiError::Forbidden(
            "You have not joined this Webxdc session".into(),
        ));
    }
    let coordinator = account::find_by_id(&state.pool, session.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !coordinator.is_local() {
        return Err(ApiError::Forbidden(
            "Guest mode is available only on the session coordinator".into(),
        ));
    }
    let marker = id::next();
    let create_id = format!(
        "{}/activities/guest-{}-{marker}",
        session.coordinator_uri, guest.id
    );
    let object_id = format!(
        "{}/updates/guest-{}-{marker}",
        session.coordinator_uri, guest.id
    );
    let create = json!({
        "@context": context(),
        "id": create_id,
        "type": "Create",
        "actor": guest.participant_uri,
        "to": session.coordinator_uri,
        "context": session.coordinator_uri,
        "audience": session.coordinator_uri,
        "object": {
            "id": object_id,
            "type": "WebxdcUpdate",
            "attributedTo": guest.participant_uri,
            "context": session.coordinator_uri,
            "audience": session.coordinator_uri,
            "webxdcUpdate": update,
        },
        "published": OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_default(),
    });
    let (parsed_create_id, parsed_object_id, value) =
        parse_update_create(&create, session, &guest.participant_uri)?;
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let (stored, inserted) = webxdc::sequence_guest_update(
        &mut tx,
        webxdc::SequenceGuestUpdate {
            session_id: session.id,
            guest_id: guest.id,
            create_id: &parsed_create_id,
            object_id: &parsed_object_id,
            actor_uri: &guest.participant_uri,
            raw_create: &create,
            webxdc_update: &value,
            announce_id_prefix: &session.coordinator_uri,
        },
    )
    .await
    .map_err(protocol_db_error)?;
    if inserted {
        let inboxes = webxdc::remote_inboxes_tx(&mut tx, session.id).await?;
        job::enqueue_many_tx(
            &mut *tx,
            session.account_id,
            &inboxes,
            &announce_activity(session, &stored),
        )
        .await?;
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(stored)
}

async fn session_tombstoned(state: &AppState, session_uri: &str) -> Result<bool, ApiError> {
    Ok(webxdc::tombstone_by_uri(&state.pool, session_uri)
        .await?
        .is_some())
}

/// Handles a marked Follow addressed to a hosted session. Returns false when
/// the activity is not FEP-752d traffic, allowing the ordinary Group handler to
/// continue.
pub async fn handle_follow(
    state: &AppState,
    sender: &Account,
    raw: &Value,
) -> Result<bool, ApiError> {
    let Some(session_uri) = raw.get("object").and_then(plamenu_ap::activity::id_of) else {
        return Ok(false);
    };
    if session_tombstoned(state, session_uri).await? {
        return Ok(true);
    }
    let Some(session) = webxdc::find_by_uri(&state.pool, session_uri).await? else {
        return Ok(false);
    };
    let coordinator = account::find_by_id(&state.pool, session.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !coordinator.is_local() {
        return Ok(false);
    }
    let sender_uri = sender
        .uri
        .as_deref()
        .ok_or_else(|| ApiError::BadRequest("sender has no actor URI".into()))?;
    let valid = raw.get("webxdcProtocol").and_then(Value::as_str) == Some(PROTOCOL)
        && raw.get("actor").and_then(Value::as_str) == Some(sender_uri)
        && raw.get("context").and_then(plamenu_ap::activity::id_of) == Some(session_uri)
        && raw.get("audience").and_then(plamenu_ap::activity::id_of) == Some(session_uri)
        && raw.get("to").and_then(plamenu_ap::activity::id_of) == Some(session_uri);
    if !valid || session.ended() {
        job::enqueue(
            &state.pool,
            session.account_id,
            &sender.inbox_url,
            &reject_follow_activity(&session, raw, sender_uri),
        )
        .await?;
        return Ok(true);
    }
    let follow_id = raw
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::BadRequest("Webxdc Follow has no id".into()))?;
    let self_addr = session_self_addr(state, sender_uri, session_uri);
    let pending = webxdc::request_membership(
        &state.pool,
        session.id,
        sender.id,
        sender_uri,
        follow_id,
        &self_addr,
    )
    .await?;
    if session.membership_policy == "open" {
        approve(state, &session, pending.participant_account_id).await?;
    }
    Ok(true)
}

fn validate_closure_shape(
    object: &Value,
    bundle: &Value,
    session_uri: &str,
    session: &Session,
) -> Result<(), ApiError> {
    let inbox = format!("{session_uri}/inbox");
    let outbox = format!("{session_uri}/outbox");
    let followers = format!("{session_uri}/followers");
    let url_matches = object.get("url").is_none_or(|url| {
        url.as_str() == Some(session_uri)
            || (url.get("type").and_then(Value::as_str) == Some("Link")
                && url.get("href").and_then(Value::as_str) == Some(session_uri)
                && url.get("mediaType").and_then(Value::as_str) == Some("text/html"))
    });
    let has_authentication = ["publicKey", "assertionMethod", "verificationMethod"]
        .iter()
        .any(|field| object.get(*field).is_some());
    let exact = object
        .get("name")
        .is_none_or(|value| value.as_str() == Some(session.name.as_str()))
        && object
            .get("summary")
            .is_none_or(|value| value.as_str() == Some(session.summary.as_str()))
        && object
            .get("attributedTo")
            .and_then(plamenu_ap::activity::id_of)
            == Some(session.creator_uri.as_str())
        && object.get("inbox").and_then(Value::as_str) == Some(inbox.as_str())
        && object.get("outbox").and_then(Value::as_str) == Some(outbox.as_str())
        && object.get("followers").and_then(Value::as_str) == Some(followers.as_str())
        && url_matches
        && has_authentication
        && object.get("sendUpdateInterval").and_then(Value::as_i64)
            == Some(i64::from(session.send_update_interval))
        && object.get("sendUpdateMaxSize").and_then(Value::as_i64)
            == Some(i64::from(session.send_update_max_size))
        && bundle.get("id").and_then(Value::as_str) == Some(session.bundle_id.as_str())
        && bundle.get("mediaType").and_then(Value::as_str)
            == Some(session.bundle_media_type.as_str())
        && bundle.get("digestMultibase").and_then(Value::as_str)
            == Some(session.digest_multibase.as_str());
    if !exact {
        return Err(ApiError::Conflict(
            "Webxdc closure changed immutable or required actor fields".into(),
        ));
    }
    Ok(())
}

/// Consumes the complete actor replacement used to close a remote session.
/// It also intercepts attempts to mutate an accepted `WebxdcUpdate`, whose log
/// is immutable under the FEP.
pub async fn handle_update(
    state: &AppState,
    sender: &Account,
    raw: &Value,
) -> Result<bool, ApiError> {
    let Some(object) = raw.get("object").filter(|value| value.is_object()) else {
        return Ok(false);
    };
    if type_contains(object, "WebxdcUpdate") {
        if let Some(session_uri) = object.get("context").and_then(plamenu_ap::activity::id_of)
            && (webxdc::find_by_uri(&state.pool, session_uri)
                .await?
                .is_some()
                || session_tombstoned(state, session_uri).await?)
        {
            return Err(ApiError::Conflict(
                "accepted Webxdc updates are immutable".into(),
            ));
        }
        return Ok(false);
    }
    if !type_contains(object, "Group") || !type_contains(object, "WebxdcSession") {
        return Ok(false);
    }
    let Some(session_uri) = object.get("id").and_then(Value::as_str) else {
        return Err(ApiError::BadRequest("Webxdc actor update has no id".into()));
    };
    if session_tombstoned(state, session_uri).await? && sender.uri.as_deref() == Some(session_uri) {
        return Ok(true);
    }
    let Some(session) = webxdc::find_by_uri(&state.pool, session_uri).await? else {
        return Ok(false);
    };
    let followers_target = format!("{session_uri}/followers");
    if sender.id != session.account_id
        || raw.get("actor").and_then(Value::as_str) != Some(session_uri)
        || raw.get("to").and_then(plamenu_ap::activity::id_of) != Some(followers_target.as_str())
        || raw.get("audience").and_then(plamenu_ap::activity::id_of) != Some(session_uri)
        || object.get("webxdcProtocol").and_then(Value::as_str) != Some(PROTOCOL)
    {
        return Err(ApiError::Forbidden(
            "Webxdc actor update is from the wrong coordinator".into(),
        ));
    }
    let bundles: Vec<_> = values(object.get("attachment"))
        .into_iter()
        .filter(|item| {
            item.get("type").and_then(Value::as_str) == Some("Document")
                && matches!(
                    item.get("mediaType").and_then(Value::as_str),
                    Some(MEDIA_TYPE | LEGACY_MEDIA_TYPE)
                )
        })
        .collect();
    if bundles.len() != 1 {
        return Err(ApiError::BadRequest(
            "complete Webxdc actor must retain exactly one qualifying bundle".into(),
        ));
    }
    let bundle = bundles[0];
    validate_closure_shape(object, bundle, session_uri, &session)?;
    let new_bundle_url = https_uri(bundle, "url")?;
    if new_bundle_url != session.bundle_url {
        let limits = webxdc::limits(&state.pool).await?;
        let fetched = state
            .federation
            .fetch_media_limited(&new_bundle_url, limits.bundle_bytes() as u64)
            .await
            .map_err(|error| ApiError::BadGateway(error.to_string()))?;
        validate_package_async(&fetched.bytes, Some(&session.digest_multibase), limits).await?;
        webxdc::update_bundle_url(&state.pool, session.id, &new_bundle_url).await?;
    }
    let ended = object
        .get("endTime")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::BadRequest("Webxdc actor update has no endTime".into()))?;
    let ended = OffsetDateTime::parse(ended, &Rfc3339)
        .map_err(|_| ApiError::BadRequest("invalid Webxdc endTime".into()))?;
    object
        .get("updated")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ApiError::BadRequest("complete Webxdc closure has no updated timestamp".into())
        })
        .and_then(|value| {
            OffsetDateTime::parse(value, &Rfc3339)
                .map(|_| ())
                .map_err(|_| ApiError::BadRequest("invalid Webxdc updated timestamp".into()))
        })?;
    let mut actor_view = object.clone();
    actor_view["type"] = Value::String("Group".into());
    if actor_view.get("preferredUsername").is_none() {
        actor_view["preferredUsername"] = Value::String(sender.username.clone());
    }
    let actor: plamenu_ap::actor::RemoteActor = serde_json::from_value(actor_view)
        .map_err(|error| ApiError::BadRequest(format!("invalid complete Webxdc actor: {error}")))?;
    crate::remote::refresh_remote_actor(state, &actor).await?;
    webxdc::close_at(&state.pool, session.id, ended).await?;
    state.webxdc_realtime.invalidate(session.id, None);
    Ok(true)
}

pub async fn handle_accept(
    state: &AppState,
    sender: &Account,
    raw: &Value,
) -> Result<bool, ApiError> {
    if raw.get("webxdcProtocol").and_then(Value::as_str) != Some(PROTOCOL) {
        return Ok(false);
    }
    let follow_id = raw
        .get("object")
        .and_then(plamenu_ap::activity::id_of)
        .ok_or_else(|| ApiError::BadRequest("Webxdc Accept has no Follow".into()))?;
    let boundary = raw
        .get("webxdcMaxSerial")
        .and_then(Value::as_i64)
        .filter(|value| (0..=webxdc::MAX_SERIAL).contains(value))
        .ok_or_else(|| ApiError::BadRequest("invalid webxdcMaxSerial".into()))?;
    let membership = webxdc::membership_by_follow(&state.pool, follow_id)
        .await?
        .ok_or_else(|| ApiError::BadRequest("unknown Webxdc Follow".into()))?;
    let session = webxdc::find(&state.pool, membership.session_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if sender.id != session.account_id
        || raw.get("actor").and_then(Value::as_str) != Some(session.coordinator_uri.as_str())
        || raw.get("to").and_then(plamenu_ap::activity::id_of)
            != Some(membership.participant_uri.as_str())
        || raw.get("audience").and_then(plamenu_ap::activity::id_of)
            != Some(session.coordinator_uri.as_str())
    {
        return Err(ApiError::BadRequest(
            "Webxdc Accept is from the wrong coordinator".into(),
        ));
    }
    webxdc::mark_accepted(&state.pool, follow_id, boundary).await?;
    state.webxdc_realtime.refresh(session.id);
    Ok(true)
}

pub async fn handle_reject(
    state: &AppState,
    sender: &Account,
    raw: &Value,
) -> Result<bool, ApiError> {
    let Some(follow_id) = raw.get("object").and_then(plamenu_ap::activity::id_of) else {
        return Ok(false);
    };
    let Some(membership) = webxdc::membership_by_follow(&state.pool, follow_id).await? else {
        return Ok(false);
    };
    let session = webxdc::find(&state.pool, membership.session_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if sender.id != session.account_id
        || raw.get("actor").and_then(Value::as_str) != Some(session.coordinator_uri.as_str())
        || raw.get("to").and_then(plamenu_ap::activity::id_of)
            != Some(membership.participant_uri.as_str())
        || raw.get("audience").and_then(plamenu_ap::activity::id_of)
            != Some(session.coordinator_uri.as_str())
    {
        return Err(ApiError::BadRequest(
            "Webxdc Reject is from the wrong coordinator".into(),
        ));
    }
    webxdc::remove_membership(&state.pool, session.id, membership.participant_account_id).await?;
    state.webxdc_realtime.invalidate(
        session.id,
        Some(crate::webxdc_realtime::Peer::Account(
            membership.participant_account_id,
        )),
    );
    Ok(true)
}

pub async fn handle_create(
    state: &AppState,
    sender: &Account,
    raw: &Value,
) -> Result<bool, ApiError> {
    let Some(object) = raw.get("object") else {
        return Ok(false);
    };
    if !type_contains(object, "WebxdcUpdate") {
        return Ok(false);
    }
    let session_uri = raw
        .get("context")
        .and_then(plamenu_ap::activity::id_of)
        .ok_or_else(|| ApiError::BadRequest("Webxdc Create has no session context".into()))?;
    if session_tombstoned(state, session_uri).await? {
        return Ok(true);
    }
    let Some(session) = webxdc::find_by_uri(&state.pool, session_uri).await? else {
        return Ok(false);
    };
    let coordinator = account::find_by_id(&state.pool, session.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !coordinator.is_local() {
        return Ok(false);
    }
    let sender_uri = sender
        .uri
        .as_deref()
        .ok_or_else(|| ApiError::BadRequest("sender has no actor URI".into()))?;
    let (create_id, object_id, update) = parse_update_create(raw, &session, sender_uri)?;
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let (stored, inserted) = webxdc::sequence_update(
        &mut tx,
        webxdc::SequenceUpdate {
            session_id: session.id,
            participant_account_id: sender.id,
            create_id: &create_id,
            object_id: &object_id,
            actor_uri: sender_uri,
            raw_create: raw,
            webxdc_update: &update,
            announce_id_prefix: &session.coordinator_uri,
        },
    )
    .await
    .map_err(protocol_db_error)?;
    if inserted {
        let inboxes = webxdc::remote_inboxes_tx(&mut tx, session.id).await?;
        job::enqueue_many_tx(
            &mut *tx,
            session.account_id,
            &inboxes,
            &announce_activity(&session, &stored),
        )
        .await?;
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(true)
}

pub async fn handle_announce(
    state: &AppState,
    sender: &Account,
    raw: &Value,
) -> Result<bool, ApiError> {
    let Some(serial) = raw.get("webxdcSerial").and_then(Value::as_i64) else {
        return Ok(false);
    };
    if !(1..=webxdc::MAX_SERIAL).contains(&serial) {
        return Err(ApiError::BadRequest("invalid webxdcSerial".into()));
    }
    let actor_uri = raw
        .get("actor")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::BadRequest("Webxdc Announce has no actor".into()))?;
    if session_tombstoned(state, actor_uri).await? && sender.uri.as_deref() == Some(actor_uri) {
        return Ok(true);
    }
    let Some(session) = webxdc::find_by_uri(&state.pool, actor_uri).await? else {
        return Ok(false);
    };
    if sender.id != session.account_id
        || raw.get("audience").and_then(plamenu_ap::activity::id_of) != Some(actor_uri)
    {
        return Err(ApiError::BadRequest(
            "Webxdc Announce is from the wrong coordinator".into(),
        ));
    }
    // The creator may be remote/unknown on a consumed session; authorization is
    // instead established by at least one accepted local membership.
    if !webxdc::has_active_local_membership(&state.pool, session.id).await? {
        return Err(ApiError::Forbidden(
            "no active local Webxdc membership".into(),
        ));
    }
    let inner = raw
        .get("object")
        .filter(|value| value.is_object())
        .ok_or_else(|| ApiError::BadRequest("Webxdc Announce has no embedded Create".into()))?;
    let inner_actor = inner
        .get("actor")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::BadRequest("embedded Webxdc Create has no actor".into()))?;
    let (create_id, object_id, update) = parse_update_create(inner, &session, inner_actor)?;
    let announce_id = raw
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::BadRequest("Webxdc Announce has no id".into()))?;
    webxdc::store_announced(
        &state.pool,
        webxdc::AnnouncedUpdate {
            session_id: session.id,
            serial,
            create_id: &create_id,
            object_id: &object_id,
            actor_uri: inner_actor,
            raw_create: inner,
            webxdc_update: &update,
            announce_id,
        },
    )
    .await
    .map_err(protocol_db_error)?;
    Ok(true)
}

pub async fn handle_undo(
    state: &AppState,
    sender: &Account,
    raw: &Value,
) -> Result<bool, ApiError> {
    let Some(follow_id) = raw.get("object").and_then(plamenu_ap::activity::id_of) else {
        return Ok(false);
    };
    let Some(membership) = webxdc::membership_by_follow(&state.pool, follow_id).await? else {
        return Ok(false);
    };
    if sender.id != membership.participant_account_id {
        return Err(ApiError::Forbidden(
            "Webxdc Undo actor does not own the membership".into(),
        ));
    }
    webxdc::remove_membership(&state.pool, membership.session_id, sender.id).await?;
    state.webxdc_realtime.invalidate(
        membership.session_id,
        Some(crate::webxdc_realtime::Peer::Account(sender.id)),
    );
    Ok(true)
}

pub async fn handle_remove(
    state: &AppState,
    sender: &Account,
    raw: &Value,
) -> Result<bool, ApiError> {
    let Some(actor_uri) = raw.get("object").and_then(plamenu_ap::activity::id_of) else {
        return Ok(false);
    };
    let Some(session_uri) = raw.get("audience").and_then(plamenu_ap::activity::id_of) else {
        return Ok(false);
    };
    if session_tombstoned(state, session_uri).await? && sender.uri.as_deref() == Some(session_uri) {
        return Ok(true);
    }
    let Some(session) = webxdc::find_by_uri(&state.pool, session_uri).await? else {
        return Ok(false);
    };
    let followers_uri = format!("{session_uri}/followers");
    if sender.id != session.account_id
        || raw.get("target").and_then(plamenu_ap::activity::id_of) != Some(followers_uri.as_str())
    {
        return Ok(false);
    }
    let Some(participant_id) =
        webxdc::participant_id_by_uri(&state.pool, session.id, actor_uri).await?
    else {
        return Ok(true);
    };
    webxdc::remove_membership(&state.pool, session.id, participant_id).await?;
    state.webxdc_realtime.invalidate(
        session.id,
        Some(crate::webxdc_realtime::Peer::Account(participant_id)),
    );
    Ok(true)
}

/// Consumes an authoritative session-actor `Delete` and immediately purges the
/// locally cached executable state. Redeliveries are recognized by the
/// retained tombstone and remain idempotent.
pub async fn handle_delete(
    state: &AppState,
    sender: &Account,
    raw: &Value,
) -> Result<bool, ApiError> {
    let Some(session_uri) = raw.get("object").and_then(plamenu_ap::activity::id_of) else {
        return Ok(false);
    };
    if raw.get("actor").and_then(Value::as_str) != Some(session_uri)
        || sender.uri.as_deref() != Some(session_uri)
    {
        return Ok(false);
    }
    if webxdc::tombstone_by_uri(&state.pool, session_uri)
        .await?
        .is_some()
    {
        return Ok(true);
    }
    let Some(session) = webxdc::find_by_uri(&state.pool, session_uri).await? else {
        return Ok(false);
    };
    if sender.id != session.account_id {
        return Err(ApiError::Forbidden(
            "Webxdc Delete is from the wrong coordinator".into(),
        ));
    }
    let deleted_at = raw
        .get("object")
        .and_then(|object| object.get("deleted"))
        .and_then(Value::as_str)
        .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok())
        .unwrap_or_else(OffsetDateTime::now_utc);
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    webxdc::purge_as_tombstone_tx(&mut tx, session.id, deleted_at).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    state.webxdc_realtime.invalidate(session.id, None);
    Ok(true)
}

pub async fn close_local(state: &AppState, session: &Session) -> Result<Session, ApiError> {
    if session.ended() {
        return Ok(session.clone());
    }
    let coordinator = account::find_by_id(&state.pool, session.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !coordinator.is_local() {
        return Err(ApiError::Forbidden(
            "Only the coordinator can close this session".into(),
        ));
    }
    let ended_at = OffsetDateTime::now_utc();
    let mut projected = session.clone();
    projected.ended_at = Some(ended_at);
    let update = json!({
        "@context": context(),
        "id": format!("{}/activities/close", projected.coordinator_uri),
        "type": "Update", "actor": projected.coordinator_uri,
        "object": actor_document(state, &projected).await?,
        "to": format!("{}/followers", projected.coordinator_uri),
        "audience": projected.coordinator_uri,
    });
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let closed = webxdc::close_at_tx(&mut tx, session.id, ended_at)
        .await?
        .ok_or(ApiError::NotFound)?;
    let inboxes = webxdc::remote_inboxes_tx(&mut tx, session.id).await?;
    job::enqueue_many_tx(&mut *tx, closed.account_id, &inboxes, &update).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    state.webxdc_realtime.invalidate(session.id, None);
    Ok(closed)
}

/// Deletes a locally coordinated session for everyone. The remote participant
/// snapshot and its durable `Delete` jobs are committed before the same
/// transaction removes all executable, history, membership, and guest data.
pub async fn delete_local(state: &AppState, session: &Session) -> Result<(), ApiError> {
    let coordinator = account::find_by_id(&state.pool, session.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !coordinator.is_local() {
        return Err(ApiError::Forbidden(
            "Only the coordinator can delete this session".into(),
        ));
    }
    let deleted_at = OffsetDateTime::now_utc();
    let activity = delete_activity(session, deleted_at);
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    if !webxdc::lock_live_tx(&mut tx, session.id).await? {
        tx.rollback().await.map_err(plamenu_db::DbError::from)?;
        return Ok(());
    }
    let inboxes = webxdc::remote_inboxes_tx(&mut tx, session.id).await?;
    job::enqueue_many_tx(&mut *tx, session.account_id, &inboxes, &activity).await?;
    webxdc::purge_as_tombstone_tx(&mut tx, session.id, deleted_at).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    state.webxdc_realtime.invalidate(session.id, None);
    Ok(())
}

pub async fn remove_participant(
    state: &AppState,
    session: &Session,
    participant: &Account,
) -> Result<(), ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    if !participant.is_local() {
        job::enqueue_tx(
            &mut *tx,
            session.account_id,
            &participant.inbox_url,
            &remove_activity(session, participant.uri.as_deref().unwrap_or_default()),
        )
        .await?;
    }
    webxdc::remove_membership_tx(&mut tx, session.id, participant.id).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    state.webxdc_realtime.invalidate(
        session.id,
        Some(crate::webxdc_realtime::Peer::Account(participant.id)),
    );
    Ok(())
}

/// Recognizing an invitation never fetches or starts its app.
pub async fn invitation_in_text(
    pool: &plamenu_db::PgPool,
    text: &str,
) -> Result<Option<(i64, String, String)>, ApiError> {
    Ok(webxdc::invitation_in_urls(pool, &crate::compose::urls_in(text)).await?)
}

#[must_use]
pub fn invitation_entity(id: Option<i64>, uri: &str, name: &str) -> Value {
    let open_url = id.map_or_else(
        || {
            let query = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("url", uri)
                .finish();
            format!("/webxdc/open?{query}")
        },
        |id| format!("/webxdc/session/{id}"),
    );
    json!({"name":name,"url":uri,"open_url":open_url})
}

#[must_use]
pub fn remote_invitation(object: &Value) -> Option<(&str, &str)> {
    let attachment = object.get("attachment")?;
    let entries = attachment
        .as_array()
        .map_or_else(|| vec![attachment], |v| v.iter().collect());
    entries.into_iter().take(32).find_map(|entry| {
        let rel = &entry["rel"];
        if entry["type"] != "Link"
            || !(rel == OPEN_REL
                || rel
                    .as_array()
                    .is_some_and(|v| v.iter().any(|r| r == OPEN_REL)))
        {
            return None;
        }
        let uri = entry["href"].as_str()?;
        let parsed = url::Url::parse(uri).ok()?;
        if parsed.scheme() != "https"
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || uri.len() > 2048
        {
            return None;
        }
        let name = entry["name"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 512)
            .unwrap_or("Shared app");
        Some((uri, name.strip_prefix("Open ").unwrap_or(name)))
    })
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write as _};

    use serde_json::json;
    use zip::write::SimpleFileOptions;

    use super::*;

    fn package(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in entries {
            writer
                .start_file(*name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn package_digest_and_files_are_verified() {
        let bytes = package(&[("index.html", b"<h1>Hello</h1>"), ("app.js", b"ok()")]);
        let digest = digest_multibase(&bytes);
        let validated = validate_package(&bytes, Some(&digest)).unwrap();
        assert!(
            digest.starts_with("uEi"),
            "sha256 multihash prefix is encoded"
        );
        assert_eq!(digest.len(), 47);
        assert_eq!(validated.files.len(), 2);
        assert_eq!(validated.files[0].1, "text/html; charset=utf-8");
        assert!(matches!(
            validate_package(
                &bytes,
                Some("uEiAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
            ),
            Err(PackageError::Digest)
        ));
    }

    #[test]
    fn package_rejects_traversal_and_ambiguous_unicode_names() {
        let traversal = package(&[("index.html", b"ok"), ("../escape.js", b"bad")]);
        assert!(matches!(
            validate_package(&traversal, None),
            Err(PackageError::UnsafeEntry(_))
        ));

        let ambiguous = package(&[
            ("index.html", b"ok"),
            ("caf\u{e9}.txt", b"one"),
            ("cafe\u{301}.txt", b"two"),
        ]);
        assert!(matches!(
            validate_package(&ambiguous, None),
            Err(PackageError::UnsafeEntry(_))
        ));
    }

    #[test]
    fn durable_update_validation_preserves_unknown_members_but_closes_escapes() {
        validate_update(
            &json!({"payload": null, "futureMember": {"kept": true}, "href": "board/1#x"}),
            1024,
        )
        .unwrap();
        for bad in [
            json!({"payload": 1, "href": "https://tracker.example/x"}),
            json!({"payload": 1, "href": "../outside"}),
            json!({"payload": 1, "notify": "not-an-object"}),
        ] {
            assert!(validate_update(&bad, 1024).is_err());
        }
    }

    #[test]
    fn invitation_keeps_the_ordinary_note_and_adds_fep_linking() {
        let mut create = json!({
            "type": "Create",
            "object": {
                "type": "Note",
                "content": "<p>Join at <a href=\"https://social.example/webxdc/1\">the session</a></p>",
                "cc": ["https://www.w3.org/ns/activitystreams#Public"]
            }
        });
        enhance_invitation(&mut create, "https://social.example/webxdc/1", "Chess");
        assert!(
            create["object"]["content"]
                .as_str()
                .unwrap()
                .contains("<a ")
        );
        assert_eq!(create["object"]["attachment"][0]["rel"], OPEN_REL);
        assert_eq!(
            create["object"]["audience"],
            "https://social.example/webxdc/1"
        );
        assert_eq!(create["audience"], "https://social.example/webxdc/1");
    }
}
