//! Persistence for FEP-752d Webxdc sessions.

use std::collections::HashMap;

use serde_json::Value;
use sqlx::{FromRow, PgConnection, PgPool, Postgres, QueryBuilder};
use time::OffsetDateTime;

use crate::account::Account;
use crate::{DbError, id};

pub const MAX_SERIAL: i64 = 9_007_199_254_740_991;
/// Live operator limits in MiB. Packages are retained both zipped and expanded.
#[derive(Debug, Clone, Copy, FromRow)]
pub struct Limits {
    pub bundle_mb: i32,
    pub expanded_mb: i32,
    pub file_mb: i32,
    pub session_mb: i32,
    pub account_mb: i32,
    pub total_mb: i32,
    pub personal_apps: i32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            bundle_mb: 256,
            expanded_mb: 512,
            file_mb: 256,
            session_mb: 1024,
            account_mb: 1024,
            total_mb: 10240,
            personal_apps: 50,
        }
    }
}

impl Limits {
    #[must_use]
    pub fn valid(self) -> bool {
        (1..=512).contains(&self.bundle_mb)
            && (1..=1024).contains(&self.expanded_mb)
            && (1..=512).contains(&self.file_mb)
            && (1..=65_536).contains(&self.session_mb)
            && (1..=1_048_576).contains(&self.account_mb)
            && (1..=1_048_576).contains(&self.total_mb)
            && (0..=10_000).contains(&self.personal_apps)
            && self.file_mb <= self.expanded_mb
            && self.session_mb >= self.bundle_mb + self.expanded_mb
            && self.account_mb >= self.session_mb
    }

    #[must_use]
    pub fn bundle_bytes(self) -> usize {
        usize::try_from(self.bundle_mb).unwrap_or(0) * 1024 * 1024
    }
}

pub async fn limits<'e, E: sqlx::PgExecutor<'e>>(executor: E) -> Result<Limits, DbError> {
    Ok(sqlx::query_as("SELECT bundle_mb, expanded_mb, file_mb, session_mb, account_mb, total_mb, personal_apps FROM webxdc_settings WHERE singleton")
        .fetch_one(executor).await?)
}

pub async fn set_limits(pool: &PgPool, limits: Limits) -> Result<(), DbError> {
    if !limits.valid() {
        return Err(DbError::Protocol("Invalid Webxdc limits".into()));
    }
    sqlx::query("UPDATE webxdc_settings SET bundle_mb=$1, expanded_mb=$2, file_mb=$3, session_mb=$4, account_mb=$5, total_mb=$6, personal_apps=$7 WHERE singleton")
        .bind(limits.bundle_mb).bind(limits.expanded_mb).bind(limits.file_mb)
        .bind(limits.session_mb).bind(limits.account_mb).bind(limits.total_mb)
        .bind(limits.personal_apps).execute(pool).await?;
    Ok(())
}

#[derive(Debug, Clone, FromRow)]
pub struct Session {
    pub id: i64,
    pub account_id: i64,
    pub coordinator_uri: String,
    pub creator_account_id: Option<i64>,
    pub creator_uri: String,
    pub name: String,
    pub summary: String,
    pub bundle_id: String,
    pub bundle_url: String,
    pub bundle_name: String,
    pub bundle_media_type: String,
    pub digest_multibase: String,
    pub send_update_interval: i32,
    pub send_update_max_size: i32,
    pub membership_policy: String,
    pub last_serial: i64,
    pub published_at: OffsetDateTime,
    pub ended_at: Option<OffsetDateTime>,
}

impl Session {
    #[must_use]
    pub fn is_local(&self, account: &Account) -> bool {
        account.is_local()
    }

    #[must_use]
    pub fn ended(&self) -> bool {
        self.ended_at.is_some()
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct Tombstone {
    pub session_id: i64,
    pub coordinator_uri: String,
    pub deleted_at: OffsetDateTime,
}

#[derive(Debug, Clone, FromRow)]
pub struct PackageFile {
    pub session_id: i64,
    pub path: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, FromRow)]
pub struct Membership {
    pub session_id: i64,
    pub participant_account_id: i64,
    pub participant_uri: String,
    pub follow_id: String,
    pub accepted: bool,
    pub replay_boundary: i64,
    pub self_addr: String,
    pub joined_at: OffsetDateTime,
    pub accepted_at: Option<OffsetDateTime>,
    pub last_submitted_at: Option<OffsetDateTime>,
}

/// A coordinator-local participant authenticated by a per-session browser
/// token. Guests are deliberately not first-class accounts and have no
/// authority outside this session.
#[derive(Debug, Clone, FromRow)]
pub struct Guest {
    pub id: i64,
    pub session_id: i64,
    pub display_name: String,
    pub token_hash: String,
    pub participant_uri: String,
    pub accepted: bool,
    pub replay_boundary: i64,
    pub self_addr: String,
    pub joined_at: OffsetDateTime,
    pub accepted_at: Option<OffsetDateTime>,
    pub last_submitted_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, FromRow)]
pub struct Update {
    pub session_id: i64,
    pub serial: i64,
    pub create_id: String,
    pub object_id: String,
    pub actor_uri: String,
    pub raw_create: Value,
    pub webxdc_update: Value,
    pub announce_id: String,
    pub created_at: OffsetDateTime,
}

#[derive(Debug)]
pub struct NewLocalSession<'a> {
    pub session_id: i64,
    pub name: &'a str,
    pub summary: &'a str,
    pub coordinator_uri: &'a str,
    pub creator_account_id: i64,
    pub creator_uri: &'a str,
    pub bundle_id: &'a str,
    pub bundle_url: &'a str,
    pub bundle_name: &'a str,
    pub bundle_media_type: &'a str,
    pub package: SessionPackage<'a>,
    pub send_update_interval: i32,
    pub send_update_max_size: i32,
    pub membership_policy: &'a str,
    pub public_key_pem: &'a str,
    pub self_addr: &'a str,
}

#[derive(Debug)]
pub enum SessionPackage<'a> {
    Upload {
        digest_multibase: &'a str,
        bundle_bytes: &'a [u8],
        files: &'a [(String, String, Vec<u8>)],
    },
    Existing {
        digest_multibase: &'a str,
    },
}

#[derive(Debug)]
pub struct NewRemoteSession<'a> {
    pub account_id: i64,
    pub creator_uri: &'a str,
    pub name: &'a str,
    pub summary: &'a str,
    pub coordinator_uri: &'a str,
    pub bundle_id: &'a str,
    pub bundle_url: &'a str,
    pub bundle_name: &'a str,
    pub bundle_media_type: &'a str,
    pub digest_multibase: &'a str,
    pub bundle_bytes: &'a [u8],
    pub send_update_interval: i32,
    pub send_update_max_size: i32,
    pub published_at: OffsetDateTime,
    pub ended_at: Option<OffsetDateTime>,
    pub files: &'a [(String, String, Vec<u8>)],
}

#[derive(Debug)]
pub struct SequenceUpdate<'a> {
    pub session_id: i64,
    pub participant_account_id: i64,
    pub create_id: &'a str,
    pub object_id: &'a str,
    pub actor_uri: &'a str,
    pub raw_create: &'a Value,
    pub webxdc_update: &'a Value,
    pub announce_id_prefix: &'a str,
}

#[derive(Debug)]
pub struct SequenceGuestUpdate<'a> {
    pub session_id: i64,
    pub guest_id: i64,
    pub create_id: &'a str,
    pub object_id: &'a str,
    pub actor_uri: &'a str,
    pub raw_create: &'a Value,
    pub webxdc_update: &'a Value,
    pub announce_id_prefix: &'a str,
}

#[derive(Debug)]
pub struct AnnouncedUpdate<'a> {
    pub session_id: i64,
    pub serial: i64,
    pub create_id: &'a str,
    pub object_id: &'a str,
    pub actor_uri: &'a str,
    pub raw_create: &'a Value,
    pub webxdc_update: &'a Value,
    pub announce_id: &'a str,
}

fn package_storage_bytes(bundle: &[u8], files: &[(String, String, Vec<u8>)]) -> i64 {
    let expanded = files.iter().fold(0_i64, |total, (_, _, bytes)| {
        total.saturating_add(i64::try_from(bytes.len()).unwrap_or(i64::MAX))
    });
    i64::try_from(bundle.len())
        .unwrap_or(i64::MAX)
        .saturating_add(expanded)
}

fn update_storage_bytes(raw_create: &Value, webxdc_update: &Value) -> i64 {
    let raw = serde_json::to_vec(raw_create).map_or(i64::MAX, |bytes| {
        i64::try_from(bytes.len()).unwrap_or(i64::MAX)
    });
    let update = serde_json::to_vec(webxdc_update).map_or(i64::MAX, |bytes| {
        i64::try_from(bytes.len()).unwrap_or(i64::MAX)
    });
    raw.saturating_add(update)
}

// All retained-payload reservations and last-reference cleanup use this lock.
// Session mutations acquire their session row first, then this shared quota lock.
async fn storage_lock(conn: &mut PgConnection) -> Result<(), DbError> {
    sqlx::query("SELECT pg_advisory_xact_lock(752, 1)")
        .execute(conn)
        .await?;
    Ok(())
}

#[derive(Debug, FromRow)]
pub struct StorageUsage {
    pub package_bytes: i64,
    pub data_bytes: i64,
    pub packages: i64,
    pub sessions: i64,
}

impl StorageUsage {
    #[must_use]
    pub fn total_bytes(&self) -> i64 {
        self.package_bytes.saturating_add(self.data_bytes)
    }
}

/// Retained payload, excluding PostgreSQL row/index overhead and browser storage.
/// An account pays for each distinct package referenced by one of its sessions
/// or personal library versions once, even if another account also uses it.
/// Instance totals count each physical package once.
pub async fn storage_usage<'e, E: sqlx::PgExecutor<'e>>(
    executor: E,
    creator: Option<i64>,
) -> Result<StorageUsage, DbError> {
    Ok(sqlx::query_as(
        "SELECT
           (SELECT coalesce(sum(p.storage_bytes),0)::bigint FROM webxdc_packages p
            WHERE $1::bigint IS NULL
               OR EXISTS (SELECT 1 FROM webxdc_sessions owned_session
                  WHERE owned_session.digest_multibase=p.digest_multibase
                    AND owned_session.creator_account_id=$1)
               OR EXISTS (SELECT 1 FROM webxdc_app_versions owned_version
                  JOIN webxdc_apps owned_app ON owned_app.id=owned_version.app_id
                  WHERE owned_version.digest_multibase=p.digest_multibase
                    AND owned_app.owner_account_id=$1)) AS package_bytes,
           coalesce(sum(greatest(s.storage_bytes-p.storage_bytes,0)),0)::bigint AS data_bytes,
           (SELECT count(*)::bigint FROM webxdc_packages counted_package
            WHERE $1::bigint IS NULL
               OR EXISTS (SELECT 1 FROM webxdc_sessions counted_session
                  WHERE counted_session.digest_multibase=counted_package.digest_multibase
                    AND counted_session.creator_account_id=$1)
               OR EXISTS (SELECT 1 FROM webxdc_app_versions counted_version
                  JOIN webxdc_apps counted_app ON counted_app.id=counted_version.app_id
                  WHERE counted_version.digest_multibase=counted_package.digest_multibase
                    AND counted_app.owner_account_id=$1)) AS packages,
           count(*)::bigint AS sessions
         FROM webxdc_sessions s JOIN webxdc_packages p USING (digest_multibase)
         WHERE $1::bigint IS NULL OR s.creator_account_id=$1",
    )
    .bind(creator)
    .fetch_one(executor)
    .await?)
}

fn check_session_quota(limits: Limits, current: i64, additional: i64) -> Result<(), DbError> {
    if additional < 0
        || current.saturating_add(additional) > i64::from(limits.session_mb) * 1024 * 1024
    {
        return Err(DbError::Protocol(format!(
            "Webxdc session storage quota exceeded ({} MiB).",
            limits.session_mb
        )));
    }
    Ok(())
}

async fn check_retained_quota(
    conn: &mut PgConnection,
    limits: Limits,
    creator: Option<i64>,
    account_additional: i64,
    instance_additional: i64,
) -> Result<(), DbError> {
    // A lowered quota blocks growth, but must not block zero-cost reuse.
    if instance_additional > 0
        && storage_usage(&mut *conn, None)
            .await?
            .total_bytes()
            .saturating_add(instance_additional)
            > i64::from(limits.total_mb) * 1024 * 1024
    {
        return Err(DbError::Protocol(format!(
            "Webxdc server storage quota exceeded ({} MiB).",
            limits.total_mb
        )));
    }
    if let Some(creator) = creator
        && account_additional > 0
        && storage_usage(&mut *conn, Some(creator))
            .await?
            .total_bytes()
            .saturating_add(account_additional)
            > i64::from(limits.account_mb) * 1024 * 1024
    {
        return Err(DbError::Protocol(format!(
            "Webxdc account storage quota exceeded ({} MiB). Delete unused sessions to free space.",
            limits.account_mb
        )));
    }
    Ok(())
}

async fn enforce_storage_quota(
    conn: &mut PgConnection,
    creator: Option<i64>,
    current: i64,
    additional: i64,
) -> Result<(), DbError> {
    storage_lock(conn).await?;
    let limits = limits(&mut *conn).await?;
    check_session_quota(limits, current, additional)?;
    check_retained_quota(conn, limits, creator, additional, additional).await
}

async fn reserve_package(
    conn: &mut PgConnection,
    creator: Option<i64>,
    digest: &str,
    bundle: &[u8],
    files: &[(String, String, Vec<u8>)],
    size: i64,
) -> Result<i64, DbError> {
    storage_lock(conn).await?;
    let limits = limits(&mut *conn).await?;
    check_session_quota(limits, 0, size)?;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM webxdc_packages WHERE digest_multibase=$1)",
    )
    .bind(digest)
    .fetch_one(&mut *conn)
    .await?;
    let owned: bool = sqlx::query_scalar(
        "SELECT EXISTS(
        SELECT 1 FROM webxdc_sessions WHERE creator_account_id=$1 AND digest_multibase=$2
        UNION ALL
        SELECT 1 FROM webxdc_app_versions v JOIN webxdc_apps a ON a.id=v.app_id
         WHERE a.owner_account_id=$1 AND v.digest_multibase=$2)",
    )
    .bind(creator)
    .bind(digest)
    .fetch_one(&mut *conn)
    .await?;
    check_retained_quota(
        conn,
        limits,
        creator,
        if owned { 0 } else { size },
        if exists { 0 } else { size },
    )
    .await?;
    if !exists {
        sqlx::query("INSERT INTO webxdc_packages (digest_multibase,bundle_bytes,storage_bytes) VALUES ($1,$2,$3)")
            .bind(digest).bind(bundle).bind(size).execute(&mut *conn).await?;
        for (path, media_type, bytes) in files {
            sqlx::query("INSERT INTO webxdc_package_files (digest_multibase,path,media_type,bytes) VALUES ($1,$2,$3,$4)")
                .bind(digest).bind(path).bind(media_type).bind(bytes).execute(&mut *conn).await?;
        }
    }
    Ok(size)
}

async fn reserve_existing_package(
    conn: &mut PgConnection,
    creator: Option<i64>,
    digest: &str,
) -> Result<i64, DbError> {
    storage_lock(conn).await?;
    let limits = limits(&mut *conn).await?;
    let size: i64 =
        sqlx::query_scalar("SELECT storage_bytes FROM webxdc_packages WHERE digest_multibase=$1")
            .bind(digest)
            .fetch_optional(&mut *conn)
            .await?
            .ok_or_else(|| DbError::Protocol("Webxdc package is no longer available".into()))?;
    check_session_quota(limits, 0, size)?;
    let owned: bool = sqlx::query_scalar(
        "SELECT EXISTS(
           SELECT 1 FROM webxdc_sessions WHERE creator_account_id=$1 AND digest_multibase=$2
           UNION ALL
           SELECT 1 FROM webxdc_app_versions v JOIN webxdc_apps a ON a.id=v.app_id
            WHERE a.owner_account_id=$1 AND v.digest_multibase=$2)",
    )
    .bind(creator)
    .bind(digest)
    .fetch_one(&mut *conn)
    .await?;
    check_retained_quota(conn, limits, creator, if owned { 0 } else { size }, 0).await?;
    Ok(size)
}

pub async fn bundle(pool: &PgPool, session_id: i64) -> Result<Option<Vec<u8>>, DbError> {
    Ok(sqlx::query_scalar("SELECT p.bundle_bytes FROM webxdc_sessions s JOIN webxdc_packages p USING (digest_multibase) WHERE s.id=$1")
        .bind(session_id).fetch_optional(pool).await?)
}

const LIBRARY_APP_SELECT: &str = "SELECT a.id, a.owner_account_id, a.name, a.summary, a.category,
            a.visibility, a.source_kind, a.source_url, a.catalog_source_id,
            a.external_app_id, a.promoted_from_app_id, a.created_by_account_id,
            a.created_at, a.updated_at,
            v.id AS version_id, v.digest_multibase, v.version, v.filename,
            v.manifest_name, v.source_code_url, v.icon_path,
            v.source_url AS version_source_url, v.created_at AS version_created_at,
            p.storage_bytes AS package_bytes,
            octet_length(p.bundle_bytes)::bigint AS bundle_bytes,
            owner.username AS owner_username,
            owner.domain AS owner_domain,
            owner.display_name AS owner_display_name,
            (SELECT promoted.id FROM webxdc_apps promoted
              WHERE promoted.promoted_from_app_id=a.id
                AND promoted.owner_account_id IS NULL) AS promoted_instance_app_id,
            CASE WHEN a.promoted_from_app_id IS NOT NULL THEN EXISTS(
                SELECT 1 FROM webxdc_apps source
                JOIN webxdc_app_versions source_version
                  ON source_version.app_id=source.id AND source_version.current
                WHERE source.id=a.promoted_from_app_id
                  AND source_version.digest_multibase<>v.digest_multibase)
              ELSE EXISTS(
                SELECT 1 FROM webxdc_apps promoted
                JOIN webxdc_app_versions promoted_version
                  ON promoted_version.app_id=promoted.id AND promoted_version.current
                WHERE promoted.promoted_from_app_id=a.id
                  AND promoted_version.digest_multibase<>v.digest_multibase)
            END AS update_available
       FROM webxdc_apps a
       JOIN webxdc_app_versions v ON v.app_id=a.id AND v.current
       JOIN webxdc_packages p ON p.digest_multibase=v.digest_multibase
       LEFT JOIN accounts owner ON owner.id=a.owner_account_id";

#[derive(Debug, Clone, FromRow)]
pub struct LibraryApp {
    pub id: i64,
    pub owner_account_id: Option<i64>,
    pub name: String,
    pub summary: String,
    pub category: Option<String>,
    pub visibility: String,
    pub source_kind: String,
    pub source_url: Option<String>,
    pub catalog_source_id: Option<i64>,
    pub external_app_id: Option<String>,
    pub promoted_from_app_id: Option<i64>,
    pub created_by_account_id: Option<i64>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub version_id: i64,
    pub digest_multibase: String,
    pub version: String,
    pub filename: String,
    pub manifest_name: String,
    pub source_code_url: Option<String>,
    pub icon_path: Option<String>,
    pub version_source_url: Option<String>,
    pub version_created_at: OffsetDateTime,
    pub package_bytes: i64,
    pub bundle_bytes: i64,
    pub owner_username: Option<String>,
    pub owner_domain: Option<String>,
    pub owner_display_name: Option<String>,
    pub promoted_instance_app_id: Option<i64>,
    pub update_available: bool,
}

#[derive(Debug)]
pub struct NewLibraryApp<'a> {
    pub owner_account_id: Option<i64>,
    pub name: &'a str,
    pub summary: &'a str,
    pub category: Option<&'a str>,
    pub visibility: &'a str,
    pub source_kind: &'a str,
    pub source_url: Option<&'a str>,
    pub catalog_source_id: Option<i64>,
    pub external_app_id: Option<&'a str>,
    pub promoted_from_app_id: Option<i64>,
    pub created_by_account_id: i64,
    pub version: NewLibraryVersion<'a>,
}

#[derive(Debug)]
pub struct NewLibraryVersion<'a> {
    pub digest_multibase: &'a str,
    pub version: &'a str,
    pub filename: &'a str,
    pub manifest_name: &'a str,
    pub source_code_url: Option<&'a str>,
    pub icon_path: Option<&'a str>,
    pub source_url: Option<&'a str>,
    pub bundle_bytes: &'a [u8],
    pub files: &'a [(String, String, Vec<u8>)],
}

fn validate_library_shape(new: &NewLibraryApp<'_>) -> Result<(), DbError> {
    let personal = new.owner_account_id.is_some();
    if new.name.trim().is_empty()
        || new.name.chars().count() > 120
        || new.summary.chars().count() > 2000
        || new
            .category
            .is_some_and(|value| value.is_empty() || value.chars().count() > 80)
        || new.version.filename.is_empty()
        || new.version.filename.chars().count() > 255
        || new.version.manifest_name.is_empty()
        || new.version.manifest_name.chars().count() > 120
        || new.version.version.chars().count() > 120
        || (personal && new.visibility != "private")
        || (!personal && !matches!(new.visibility, "hidden" | "instance" | "public"))
    {
        return Err(DbError::Protocol("Invalid Webxdc library metadata".into()));
    }
    Ok(())
}

async fn library_app_with<'e, E: sqlx::PgExecutor<'e>>(
    executor: E,
    id: i64,
) -> Result<Option<LibraryApp>, DbError> {
    let sql = format!("{LIBRARY_APP_SELECT} WHERE a.id=$1");
    Ok(sqlx::query_as::<_, LibraryApp>(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(executor)
        .await?)
}

pub async fn create_library_app(
    pool: &PgPool,
    new: NewLibraryApp<'_>,
) -> Result<LibraryApp, DbError> {
    validate_library_shape(&new)?;
    let mut tx = pool.begin().await?;
    storage_lock(&mut tx).await?;
    let limits = limits(&mut *tx).await?;
    if let Some(owner) = new.owner_account_id {
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM webxdc_apps WHERE owner_account_id=$1")
                .bind(owner)
                .fetch_one(&mut *tx)
                .await?;
        if count >= i64::from(limits.personal_apps) {
            return Err(DbError::Protocol(format!(
                "Personal Webxdc app limit reached ({} apps).",
                limits.personal_apps
            )));
        }
    }
    let storage_bytes = package_storage_bytes(new.version.bundle_bytes, new.version.files);
    reserve_package(
        &mut tx,
        new.owner_account_id,
        new.version.digest_multibase,
        new.version.bundle_bytes,
        new.version.files,
        storage_bytes,
    )
    .await?;
    let app_id = id::next();
    sqlx::query(
        "INSERT INTO webxdc_apps
           (id, owner_account_id, name, summary, category, visibility,
            source_kind, source_url, catalog_source_id, external_app_id,
            promoted_from_app_id, created_by_account_id)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)",
    )
    .bind(app_id)
    .bind(new.owner_account_id)
    .bind(new.name.trim())
    .bind(new.summary.trim())
    .bind(new.category)
    .bind(new.visibility)
    .bind(new.source_kind)
    .bind(new.source_url)
    .bind(new.catalog_source_id)
    .bind(new.external_app_id)
    .bind(new.promoted_from_app_id)
    .bind(new.created_by_account_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO webxdc_app_versions
           (id, app_id, digest_multibase, version, filename, manifest_name,
            source_code_url, icon_path, source_url, created_by_account_id)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
    )
    .bind(id::next())
    .bind(app_id)
    .bind(new.version.digest_multibase)
    .bind(new.version.version)
    .bind(new.version.filename)
    .bind(new.version.manifest_name)
    .bind(new.version.source_code_url)
    .bind(new.version.icon_path)
    .bind(new.version.source_url)
    .bind(new.created_by_account_id)
    .execute(&mut *tx)
    .await?;
    let app = library_app_with(&mut *tx, app_id)
        .await?
        .expect("new library app has a current version");
    tx.commit().await?;
    Ok(app)
}

pub async fn add_library_version(
    pool: &PgPool,
    app_id: i64,
    expected_owner: Option<i64>,
    actor_id: i64,
    version: NewLibraryVersion<'_>,
) -> Result<LibraryApp, DbError> {
    let mut tx = pool.begin().await?;
    let owner: Option<i64> =
        sqlx::query_scalar("SELECT owner_account_id FROM webxdc_apps WHERE id=$1 FOR UPDATE")
            .bind(app_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| DbError::Protocol("Webxdc library app was not found".into()))?;
    if owner != expected_owner {
        return Err(DbError::Protocol(
            "Webxdc library app is not editable".into(),
        ));
    }
    let size = package_storage_bytes(version.bundle_bytes, version.files);
    reserve_package(
        &mut tx,
        owner,
        version.digest_multibase,
        version.bundle_bytes,
        version.files,
        size,
    )
    .await?;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM webxdc_app_versions
                       WHERE app_id=$1 AND digest_multibase=$2)",
    )
    .bind(app_id)
    .bind(version.digest_multibase)
    .fetch_one(&mut *tx)
    .await?;
    if exists {
        return Err(DbError::Protocol(
            "That Webxdc package is already a version of this app".into(),
        ));
    }
    sqlx::query("UPDATE webxdc_app_versions SET current=false WHERE app_id=$1 AND current")
        .bind(app_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO webxdc_app_versions
           (id, app_id, digest_multibase, version, filename, manifest_name,
            source_code_url, icon_path, source_url, created_by_account_id)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
    )
    .bind(id::next())
    .bind(app_id)
    .bind(version.digest_multibase)
    .bind(version.version)
    .bind(version.filename)
    .bind(version.manifest_name)
    .bind(version.source_code_url)
    .bind(version.icon_path)
    .bind(version.source_url)
    .bind(actor_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE webxdc_apps SET name=$2, updated_at=now() WHERE id=$1")
        .bind(app_id)
        .bind(version.manifest_name)
        .execute(&mut *tx)
        .await?;
    let app = library_app_with(&mut *tx, app_id)
        .await?
        .expect("updated library app has a current version");
    tx.commit().await?;
    Ok(app)
}

async fn create_promoted_app(
    conn: &mut PgConnection,
    source_app_id: i64,
    source: &LibraryApp,
    actor_id: i64,
) -> Result<LibraryApp, DbError> {
    storage_lock(conn).await?;
    let app_id = id::next();
    sqlx::query(
        "INSERT INTO webxdc_apps
           (id,name,summary,category,visibility,source_kind,source_url,
            promoted_from_app_id,created_by_account_id)
         VALUES ($1,$2,$3,$4,'instance','promotion',$5,$6,$7)",
    )
    .bind(app_id)
    .bind(&source.name)
    .bind(&source.summary)
    .bind(&source.category)
    .bind(&source.source_url)
    .bind(source_app_id)
    .bind(actor_id)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO webxdc_app_versions
           (id,app_id,digest_multibase,version,filename,manifest_name,
            source_code_url,icon_path,source_url,created_by_account_id)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
    )
    .bind(id::next())
    .bind(app_id)
    .bind(&source.digest_multibase)
    .bind(&source.version)
    .bind(&source.filename)
    .bind(&source.manifest_name)
    .bind(&source.source_code_url)
    .bind(&source.icon_path)
    .bind(&source.version_source_url)
    .bind(actor_id)
    .execute(&mut *conn)
    .await?;
    Ok(library_app_with(&mut *conn, app_id)
        .await?
        .expect("promoted app has a current version"))
}

async fn update_promoted_app(
    conn: &mut PgConnection,
    app_id: i64,
    source: &LibraryApp,
    actor_id: i64,
) -> Result<LibraryApp, DbError> {
    let target = library_app_with(&mut *conn, app_id)
        .await?
        .ok_or_else(|| DbError::Protocol("Promoted Webxdc app is incomplete".into()))?;
    if target.digest_multibase == source.digest_multibase {
        return Ok(target);
    }
    storage_lock(conn).await?;
    sqlx::query("UPDATE webxdc_app_versions SET current=false WHERE app_id=$1 AND current")
        .bind(app_id)
        .execute(&mut *conn)
        .await?;
    let restored = sqlx::query(
        "UPDATE webxdc_app_versions SET current=true
         WHERE app_id=$1 AND digest_multibase=$2",
    )
    .bind(app_id)
    .bind(&source.digest_multibase)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if restored == 0 {
        sqlx::query(
            "INSERT INTO webxdc_app_versions
               (id,app_id,digest_multibase,version,filename,manifest_name,
                source_code_url,icon_path,source_url,created_by_account_id)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
        )
        .bind(id::next())
        .bind(app_id)
        .bind(&source.digest_multibase)
        .bind(&source.version)
        .bind(&source.filename)
        .bind(&source.manifest_name)
        .bind(&source.source_code_url)
        .bind(&source.icon_path)
        .bind(&source.version_source_url)
        .bind(actor_id)
        .execute(&mut *conn)
        .await?;
    }
    sqlx::query(
        "UPDATE webxdc_apps SET name=$2,summary=$3,category=$4,updated_at=now()
         WHERE id=$1",
    )
    .bind(app_id)
    .bind(&source.name)
    .bind(&source.summary)
    .bind(&source.category)
    .execute(&mut *conn)
    .await?;
    Ok(library_app_with(&mut *conn, app_id)
        .await?
        .expect("updated promotion has a current version"))
}

pub async fn promote_personal_app(
    pool: &PgPool,
    source_app_id: i64,
    actor_id: i64,
) -> Result<LibraryApp, DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "SELECT id FROM webxdc_apps
         WHERE id=$1 AND owner_account_id IS NOT NULL FOR SHARE",
    )
    .bind(source_app_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| DbError::Protocol("Personal Webxdc app was not found".into()))?;
    let source = library_app_with(&mut *tx, source_app_id)
        .await?
        .filter(|app| app.owner_account_id.is_some())
        .ok_or_else(|| DbError::Protocol("Personal Webxdc app was not found".into()))?;
    let existing =
        sqlx::query_scalar::<_, i64>("SELECT id FROM webxdc_apps WHERE promoted_from_app_id=$1")
            .bind(source_app_id)
            .fetch_optional(&mut *tx)
            .await?;
    let app = match existing {
        Some(app_id) => update_promoted_app(&mut tx, app_id, &source, actor_id).await?,
        None => create_promoted_app(&mut tx, source_app_id, &source, actor_id).await?,
    };
    tx.commit().await?;
    Ok(app)
}

pub async fn library_app(pool: &PgPool, app_id: i64) -> Result<Option<LibraryApp>, DbError> {
    library_app_with(pool, app_id).await
}

async fn list_library_where(
    pool: &PgPool,
    clause: &str,
    account_id: Option<i64>,
) -> Result<Vec<LibraryApp>, DbError> {
    let sql = format!(
        "{LIBRARY_APP_SELECT} WHERE {clause}
         ORDER BY (a.owner_account_id IS NULL) DESC, lower(a.name), a.id"
    );
    let query = sqlx::query_as::<_, LibraryApp>(sqlx::AssertSqlSafe(sql));
    Ok(match account_id {
        Some(account_id) => query.bind(account_id).fetch_all(pool).await?,
        None => query.fetch_all(pool).await?,
    })
}

pub async fn library_for_account(
    pool: &PgPool,
    account_id: i64,
) -> Result<Vec<LibraryApp>, DbError> {
    list_library_where(
        pool,
        "a.owner_account_id=$1 OR (a.owner_account_id IS NULL AND a.visibility IN ('instance','public'))",
        Some(account_id),
    )
    .await
}

pub async fn personal_library(pool: &PgPool, account_id: i64) -> Result<Vec<LibraryApp>, DbError> {
    list_library_where(pool, "a.owner_account_id=$1", Some(account_id)).await
}

pub async fn admin_library(pool: &PgPool) -> Result<Vec<LibraryApp>, DbError> {
    list_library_where(pool, "a.owner_account_id IS NULL", None).await
}

pub async fn public_library(pool: &PgPool) -> Result<Vec<LibraryApp>, DbError> {
    list_library_where(
        pool,
        "a.owner_account_id IS NULL AND a.visibility='public'",
        None,
    )
    .await
}

pub async fn personal_apps_for_admin(pool: &PgPool) -> Result<Vec<LibraryApp>, DbError> {
    list_library_where(pool, "a.owner_account_id IS NOT NULL", None).await
}

pub async fn usable_version(
    pool: &PgPool,
    version_id: i64,
    account_id: i64,
) -> Result<Option<LibraryApp>, DbError> {
    let sql = format!(
        "{LIBRARY_APP_SELECT} WHERE v.id=$1 AND
         (a.owner_account_id=$2 OR (a.owner_account_id IS NULL
          AND a.visibility IN ('instance','public')))"
    );
    Ok(sqlx::query_as::<_, LibraryApp>(sqlx::AssertSqlSafe(sql))
        .bind(version_id)
        .bind(account_id)
        .fetch_optional(pool)
        .await?)
}

pub async fn set_library_visibility(
    pool: &PgPool,
    app_id: i64,
    visibility: &str,
) -> Result<Option<LibraryApp>, DbError> {
    if !matches!(visibility, "hidden" | "instance" | "public") {
        return Err(DbError::Protocol("Invalid Webxdc app visibility".into()));
    }
    let updated = sqlx::query_scalar::<_, i64>(
        "UPDATE webxdc_apps SET visibility=$2,updated_at=now()
         WHERE id=$1 AND owner_account_id IS NULL RETURNING id",
    )
    .bind(app_id)
    .bind(visibility)
    .fetch_optional(pool)
    .await?;
    match updated {
        Some(id) => library_app(pool, id).await,
        None => Ok(None),
    }
}

pub async fn delete_personal_app(
    pool: &PgPool,
    app_id: i64,
    owner_id: i64,
) -> Result<bool, DbError> {
    Ok(
        sqlx::query("DELETE FROM webxdc_apps WHERE id=$1 AND owner_account_id=$2")
            .bind(app_id)
            .bind(owner_id)
            .execute(pool)
            .await?
            .rows_affected()
            == 1,
    )
}

pub async fn delete_instance_app(pool: &PgPool, app_id: i64) -> Result<bool, DbError> {
    Ok(
        sqlx::query("DELETE FROM webxdc_apps WHERE id=$1 AND owner_account_id IS NULL")
            .bind(app_id)
            .execute(pool)
            .await?
            .rows_affected()
            == 1,
    )
}

#[derive(Debug, FromRow)]
pub struct PackageAsset {
    pub media_type: String,
    pub bytes: Vec<u8>,
}

pub async fn library_icon(
    pool: &PgPool,
    version_id: i64,
    public_only: bool,
) -> Result<Option<PackageAsset>, DbError> {
    Ok(sqlx::query_as(
        "SELECT f.media_type,f.bytes FROM webxdc_app_versions v
         JOIN webxdc_apps a ON a.id=v.app_id
         JOIN webxdc_package_files f ON f.digest_multibase=v.digest_multibase
                                   AND f.path=v.icon_path
         WHERE v.id=$1 AND (NOT $2 OR a.visibility='public')",
    )
    .bind(version_id)
    .bind(public_only)
    .fetch_optional(pool)
    .await?)
}

pub async fn library_icon_for_account(
    pool: &PgPool,
    version_id: i64,
    account_id: i64,
) -> Result<Option<PackageAsset>, DbError> {
    Ok(sqlx::query_as(
        "SELECT f.media_type,f.bytes FROM webxdc_app_versions v
         JOIN webxdc_apps a ON a.id=v.app_id
         JOIN webxdc_package_files f ON f.digest_multibase=v.digest_multibase
                                   AND f.path=v.icon_path
         WHERE v.id=$1 AND (a.owner_account_id=$2 OR
              (a.owner_account_id IS NULL AND a.visibility IN ('instance','public')))",
    )
    .bind(version_id)
    .bind(account_id)
    .fetch_optional(pool)
    .await?)
}

pub async fn public_library_bundle(
    pool: &PgPool,
    version_id: i64,
) -> Result<Option<Vec<u8>>, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT p.bundle_bytes FROM webxdc_app_versions v
         JOIN webxdc_apps a ON a.id=v.app_id
         JOIN webxdc_packages p USING(digest_multibase)
         WHERE v.id=$1 AND a.visibility='public'",
    )
    .bind(version_id)
    .fetch_optional(pool)
    .await?)
}

#[derive(Debug, Clone, FromRow)]
pub struct CatalogSource {
    pub id: i64,
    pub name: String,
    pub feed_url: String,
    pub adapter: String,
    pub enabled: bool,
    pub last_fetched_at: Option<OffsetDateTime>,
    pub last_error: Option<String>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, FromRow)]
pub struct CatalogCandidate {
    pub source_id: i64,
    pub external_app_id: String,
    pub version: String,
    pub bundle_url: String,
    pub name: String,
    pub summary: String,
    pub category: Option<String>,
    pub source_code_url: Option<String>,
    pub advertised_size: Option<i64>,
    pub published_at: Option<OffsetDateTime>,
    pub seen_at: OffsetDateTime,
    pub imported_app_id: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct NewCatalogCandidate {
    pub external_app_id: String,
    pub version: String,
    pub bundle_url: String,
    pub name: String,
    pub summary: String,
    pub category: Option<String>,
    pub source_code_url: Option<String>,
    pub advertised_size: Option<i64>,
    pub published_at: Option<OffsetDateTime>,
}

pub async fn catalog_sources(pool: &PgPool) -> Result<Vec<CatalogSource>, DbError> {
    Ok(sqlx::query_as(
        "SELECT id,name,feed_url,adapter,enabled,last_fetched_at,last_error,
                created_at,updated_at
         FROM webxdc_catalog_sources ORDER BY lower(name),id",
    )
    .fetch_all(pool)
    .await?)
}

pub async fn catalog_source(
    pool: &PgPool,
    source_id: i64,
) -> Result<Option<CatalogSource>, DbError> {
    Ok(sqlx::query_as(
        "SELECT id,name,feed_url,adapter,enabled,last_fetched_at,last_error,
                created_at,updated_at
         FROM webxdc_catalog_sources WHERE id=$1",
    )
    .bind(source_id)
    .fetch_optional(pool)
    .await?)
}

pub async fn create_catalog_source(
    pool: &PgPool,
    name: &str,
    feed_url: &str,
) -> Result<CatalogSource, DbError> {
    let mut tx = pool.begin().await?;
    storage_lock(&mut tx).await?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM webxdc_catalog_sources")
        .fetch_one(&mut *tx)
        .await?;
    if count >= 20 {
        return Err(DbError::Protocol(
            "At most 20 external Webxdc catalog sources may be configured".into(),
        ));
    }
    let id = id::next();
    sqlx::query(
        "INSERT INTO webxdc_catalog_sources(id,name,feed_url)
         VALUES($1,$2,$3)",
    )
    .bind(id)
    .bind(name)
    .bind(feed_url)
    .execute(&mut *tx)
    .await?;
    let source = sqlx::query_as(
        "SELECT id,name,feed_url,adapter,enabled,last_fetched_at,last_error,
                created_at,updated_at
         FROM webxdc_catalog_sources WHERE id=$1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(source)
}

pub async fn delete_catalog_source(pool: &PgPool, source_id: i64) -> Result<bool, DbError> {
    Ok(
        sqlx::query("DELETE FROM webxdc_catalog_sources WHERE id=$1")
            .bind(source_id)
            .execute(pool)
            .await?
            .rows_affected()
            == 1,
    )
}

pub async fn replace_catalog_candidates(
    pool: &PgPool,
    source_id: i64,
    candidates: &[NewCatalogCandidate],
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM webxdc_catalog_candidates WHERE source_id=$1")
        .bind(source_id)
        .execute(&mut *tx)
        .await?;
    if !candidates.is_empty() {
        let mut query = QueryBuilder::<Postgres>::new(
            "INSERT INTO webxdc_catalog_candidates
             (source_id,external_app_id,version,bundle_url,name,summary,category,
              source_code_url,advertised_size,published_at) ",
        );
        query.push_values(candidates, |mut row, candidate| {
            row.push_bind(source_id)
                .push_bind(&candidate.external_app_id)
                .push_bind(&candidate.version)
                .push_bind(&candidate.bundle_url)
                .push_bind(&candidate.name)
                .push_bind(&candidate.summary)
                .push_bind(&candidate.category)
                .push_bind(&candidate.source_code_url)
                .push_bind(candidate.advertised_size)
                .push_bind(candidate.published_at);
        });
        query.build().execute(&mut *tx).await?;
    }
    sqlx::query(
        "UPDATE webxdc_catalog_sources
         SET last_fetched_at=now(),last_error=NULL,updated_at=now() WHERE id=$1",
    )
    .bind(source_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn record_catalog_source_error(
    pool: &PgPool,
    source_id: i64,
    error: &str,
) -> Result<(), DbError> {
    sqlx::query("UPDATE webxdc_catalog_sources SET last_error=$2,updated_at=now() WHERE id=$1")
        .bind(source_id)
        .bind(error.chars().take(1000).collect::<String>())
        .execute(pool)
        .await?;
    Ok(())
}

const CATALOG_CANDIDATE_SELECT: &str =
    "SELECT c.source_id,c.external_app_id,c.version,c.bundle_url,c.name,
            c.summary,c.category,c.source_code_url,c.advertised_size,
            c.published_at,c.seen_at,
            (SELECT a.id FROM webxdc_apps a WHERE a.catalog_source_id=c.source_id
               AND a.external_app_id=c.external_app_id
               AND a.owner_account_id IS NULL) AS imported_app_id
       FROM webxdc_catalog_candidates c";

pub async fn catalog_candidates(
    pool: &PgPool,
    source_id: i64,
) -> Result<Vec<CatalogCandidate>, DbError> {
    let sql = format!(
        "{CATALOG_CANDIDATE_SELECT} WHERE c.source_id=$1
         ORDER BY lower(c.name),c.external_app_id LIMIT 500"
    );
    Ok(
        sqlx::query_as::<_, CatalogCandidate>(sqlx::AssertSqlSafe(sql))
            .bind(source_id)
            .fetch_all(pool)
            .await?,
    )
}

pub async fn catalog_candidate(
    pool: &PgPool,
    source_id: i64,
    external_app_id: &str,
) -> Result<Option<CatalogCandidate>, DbError> {
    let sql = format!("{CATALOG_CANDIDATE_SELECT} WHERE c.source_id=$1 AND c.external_app_id=$2");
    Ok(
        sqlx::query_as::<_, CatalogCandidate>(sqlx::AssertSqlSafe(sql))
            .bind(source_id)
            .bind(external_app_id)
            .fetch_optional(pool)
            .await?,
    )
}

async fn reserve_update_storage(
    conn: &mut PgConnection,
    creator_account_id: Option<i64>,
    session_storage_bytes: i64,
    raw_create: &Value,
    webxdc_update: &Value,
) -> Result<i64, DbError> {
    let additional_bytes = update_storage_bytes(raw_create, webxdc_update);
    enforce_storage_quota(
        conn,
        creator_account_id,
        session_storage_bytes,
        additional_bytes,
    )
    .await?;
    Ok(additional_bytes)
}

fn next_serial(
    last_serial: i64,
    last_submitted_at: Option<OffsetDateTime>,
    send_update_interval: i32,
) -> Result<i64, DbError> {
    if let Some(last) = last_submitted_at {
        let interval = time::Duration::milliseconds(i64::from(send_update_interval));
        if OffsetDateTime::now_utc() - last < interval {
            return Err(DbError::Protocol(
                "Webxdc update was submitted too soon".into(),
            ));
        }
    }
    if last_serial >= MAX_SERIAL {
        return Err(DbError::Protocol("Webxdc serial range exhausted".into()));
    }
    Ok(last_serial + 1)
}

const ACCOUNT_COLS: &str = "id, username, domain, display_name, note, public_key, \
created_at, updated_at, uri, inbox_url, shared_inbox_url, public_key_id, \
avatar_file_name, header_file_name, avatar_remote_url, header_remote_url, \
account_fields_json(id) AS fields, note_source, locked, also_known_as, moved_to_uri, \
url, discoverable, feature_approval_policy, is_bot, indexable, hide_collections, \
avatar_description, header_description, suspended_at, silenced_at, sensitized_at, \
suspension_origin, show_media, show_media_replies, show_featured, memorial, actor_type";

async fn reserve_session_package<'a>(
    conn: &mut PgConnection,
    creator_account_id: i64,
    package: SessionPackage<'a>,
) -> Result<(&'a str, i64), DbError> {
    match package {
        SessionPackage::Upload {
            digest_multibase,
            bundle_bytes,
            files,
        } => {
            let storage_bytes = package_storage_bytes(bundle_bytes, files);
            let reserved = reserve_package(
                conn,
                Some(creator_account_id),
                digest_multibase,
                bundle_bytes,
                files,
                storage_bytes,
            )
            .await?;
            Ok((digest_multibase, reserved))
        }
        SessionPackage::Existing { digest_multibase } => {
            let reserved =
                reserve_existing_package(conn, Some(creator_account_id), digest_multibase).await?;
            Ok((digest_multibase, reserved))
        }
    }
}

/// Inserts the coordinator Group account, immutable session metadata, expanded
/// package and creator membership inside the caller's key-provisioning
/// transaction.
pub async fn create_local_tx(
    conn: &mut PgConnection,
    new: NewLocalSession<'_>,
) -> Result<(Account, Session), DbError> {
    let session_id = new.session_id;
    let (digest_multibase, storage_bytes) =
        reserve_session_package(conn, new.creator_account_id, new.package).await?;
    let username = format!("webxdc_{session_id}");
    let account_sql = format!(
        "INSERT INTO accounts
             (id, username, display_name, note, public_key, actor_type, locked,
              uri, inbox_url, outbox_url, followers_url, url, discoverable,
              indexable, is_internal)
         VALUES ($1, $2, $3, $4, $5, 'Group', $6, $7, $8, $9, $10, $7,
                 false, false, true)
         RETURNING {ACCOUNT_COLS}"
    );
    let account = sqlx::query_as::<_, Account>(sqlx::AssertSqlSafe(account_sql))
        .bind(session_id)
        .bind(username)
        .bind(new.name)
        .bind(new.summary)
        .bind(new.public_key_pem)
        .bind(new.membership_policy == "approval")
        .bind(new.coordinator_uri)
        .bind(format!("{}/inbox", new.coordinator_uri))
        .bind(format!("{}/outbox", new.coordinator_uri))
        .bind(format!("{}/followers", new.coordinator_uri))
        .fetch_one(&mut *conn)
        .await?;

    let session = sqlx::query_as!(
        Session,
        r#"
        INSERT INTO webxdc_sessions
            (id, account_id, coordinator_uri, creator_account_id, creator_uri,
             name, summary, bundle_id, bundle_url, bundle_name, bundle_media_type,
             digest_multibase, send_update_interval,
             send_update_max_size, membership_policy, storage_bytes)
        VALUES ($1, $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
        RETURNING id, account_id, coordinator_uri, creator_account_id, creator_uri,
                  name, summary, bundle_id, bundle_url, bundle_name, bundle_media_type,
                  digest_multibase, send_update_interval,
                  send_update_max_size, membership_policy, last_serial,
                  published_at, ended_at
        "#,
        session_id,
        new.coordinator_uri,
        new.creator_account_id,
        new.creator_uri,
        new.name,
        new.summary,
        new.bundle_id,
        new.bundle_url,
        new.bundle_name,
        new.bundle_media_type,
        digest_multibase,
        new.send_update_interval,
        new.send_update_max_size,
        new.membership_policy,
        storage_bytes,
    )
    .fetch_one(&mut *conn)
    .await?;

    let follow_id = format!(
        "{}/memberships/{}",
        new.coordinator_uri, new.creator_account_id
    );
    sqlx::query!(
        "INSERT INTO webxdc_memberships
             (session_id, participant_account_id, participant_uri, follow_id,
              accepted, replay_boundary, self_addr, accepted_at)
         VALUES ($1, $2, $3, $4, true, 0, $5, now())",
        session_id,
        new.creator_account_id,
        new.creator_uri,
        follow_id,
        new.self_addr,
    )
    .execute(&mut *conn)
    .await?;
    Ok((account, session))
}

pub async fn find(pool: &PgPool, session_id: i64) -> Result<Option<Session>, DbError> {
    Ok(sqlx::query_as!(
        Session,
        "SELECT id, account_id, coordinator_uri, creator_account_id, creator_uri,
                name, summary, bundle_id, bundle_url, bundle_name, bundle_media_type,
                digest_multibase, send_update_interval,
                send_update_max_size, membership_policy, last_serial,
                published_at, ended_at
         FROM webxdc_sessions s WHERE id = $1
           AND NOT EXISTS (SELECT 1 FROM webxdc_tombstones t WHERE t.session_id = s.id)",
        session_id,
    )
    .fetch_optional(pool)
    .await?)
}

pub async fn external_library_app(
    pool: &PgPool,
    source_id: i64,
    external_app_id: &str,
    owner_account_id: Option<i64>,
) -> Result<Option<LibraryApp>, DbError> {
    let sql = format!(
        "{LIBRARY_APP_SELECT} WHERE a.catalog_source_id=$1
         AND a.external_app_id=$2 AND a.owner_account_id IS NOT DISTINCT FROM $3"
    );
    Ok(sqlx::query_as::<_, LibraryApp>(sqlx::AssertSqlSafe(sql))
        .bind(source_id)
        .bind(external_app_id)
        .bind(owner_account_id)
        .fetch_optional(pool)
        .await?)
}

pub async fn find_by_uri(pool: &PgPool, uri: &str) -> Result<Option<Session>, DbError> {
    Ok(sqlx::query_as!(
        Session,
        "SELECT id, account_id, coordinator_uri, creator_account_id, creator_uri,
                name, summary, bundle_id, bundle_url, bundle_name, bundle_media_type,
                digest_multibase, send_update_interval,
                send_update_max_size, membership_policy, last_serial,
                published_at, ended_at
         FROM webxdc_sessions s WHERE coordinator_uri = $1
           AND NOT EXISTS (SELECT 1 FROM webxdc_tombstones t WHERE t.session_id = s.id)",
        uri,
    )
    .fetch_optional(pool)
    .await?)
}

pub async fn tombstone_by_id(pool: &PgPool, session_id: i64) -> Result<Option<Tombstone>, DbError> {
    Ok(sqlx::query_as!(
        Tombstone,
        "SELECT session_id, coordinator_uri, deleted_at
         FROM webxdc_tombstones WHERE session_id = $1",
        session_id,
    )
    .fetch_optional(pool)
    .await?)
}

pub async fn tombstone_by_uri(
    pool: &PgPool,
    coordinator_uri: &str,
) -> Result<Option<Tombstone>, DbError> {
    Ok(sqlx::query_as!(
        Tombstone,
        "SELECT session_id, coordinator_uri, deleted_at
         FROM webxdc_tombstones WHERE coordinator_uri = $1",
        coordinator_uri,
    )
    .fetch_optional(pool)
    .await?)
}

pub async fn file(
    pool: &PgPool,
    session_id: i64,
    path: &str,
) -> Result<Option<PackageFile>, DbError> {
    Ok(sqlx::query_as!(
        PackageFile,
        r#"SELECT session_id AS "session_id!", path AS "path!", media_type AS "media_type!", bytes AS "bytes!" FROM webxdc_files
         WHERE session_id = $1 AND path = $2"#,
        session_id,
        path,
    )
    .fetch_optional(pool)
    .await?)
}

pub async fn membership(
    pool: &PgPool,
    session_id: i64,
    participant_account_id: i64,
) -> Result<Option<Membership>, DbError> {
    Ok(sqlx::query_as!(
        Membership,
        "SELECT session_id, participant_account_id, participant_uri, follow_id,
                accepted, replay_boundary, self_addr, joined_at, accepted_at,
                last_submitted_at
         FROM webxdc_memberships
         WHERE session_id = $1 AND participant_account_id = $2",
        session_id,
        participant_account_id,
    )
    .fetch_optional(pool)
    .await?)
}

/// The membership stores the canonical actor URI even for legacy local
/// accounts whose generic account row has no persisted URI.
pub async fn participant_id_by_uri(
    pool: &PgPool,
    session_id: i64,
    participant_uri: &str,
) -> Result<Option<i64>, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT participant_account_id FROM webxdc_memberships
         WHERE session_id = $1 AND participant_uri = $2",
    )
    .bind(session_id)
    .bind(participant_uri)
    .fetch_optional(pool)
    .await?)
}

pub async fn membership_by_follow(
    pool: &PgPool,
    follow_id: &str,
) -> Result<Option<Membership>, DbError> {
    Ok(sqlx::query_as!(
        Membership,
        "SELECT session_id, participant_account_id, participant_uri, follow_id,
                accepted, replay_boundary, self_addr, joined_at, accepted_at,
                last_submitted_at
         FROM webxdc_memberships WHERE follow_id = $1",
        follow_id,
    )
    .fetch_optional(pool)
    .await?)
}

/// Applies the participant-host interval before a remote-coordinator Create
/// enters federation. False means the current call must be throttled.
pub async fn reserve_remote_submission(
    pool: &PgPool,
    session_id: i64,
    participant_account_id: i64,
    interval_ms: i32,
) -> Result<bool, DbError> {
    Ok(sqlx::query!(
        "UPDATE webxdc_memberships SET last_submitted_at = now()
         WHERE session_id = $1 AND participant_account_id = $2 AND accepted
           AND (last_submitted_at IS NULL
                OR last_submitted_at <= now() - ($3::bigint * interval '1 millisecond'))",
        session_id,
        participant_account_id,
        i64::from(interval_ms),
    )
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

pub async fn has_complete_prefix(
    pool: &PgPool,
    session_id: i64,
    boundary: i64,
) -> Result<bool, DbError> {
    if boundary == 0 {
        return Ok(true);
    }
    Ok(sqlx::query_scalar!(
        r#"SELECT count(*) = $2
                  AND min(serial) = 1
                  AND max(serial) = $2 AS "complete!"
           FROM webxdc_updates WHERE session_id = $1 AND serial <= $2"#,
        session_id,
        boundary,
    )
    .fetch_one(pool)
    .await?)
}

pub async fn list_for_participant(
    pool: &PgPool,
    participant_account_id: i64,
    ended: bool,
) -> Result<Vec<(Session, Membership)>, DbError> {
    let rows = sqlx::query!(
        "SELECT s.id, s.account_id, s.coordinator_uri, s.creator_account_id,
                s.creator_uri, s.name, s.summary, s.bundle_id, s.bundle_url,
                s.bundle_name, s.bundle_media_type, s.digest_multibase,
                s.send_update_interval, s.send_update_max_size,
                s.membership_policy, s.last_serial, s.published_at, s.ended_at,
                m.participant_uri, m.follow_id, m.accepted, m.replay_boundary,
                m.self_addr, m.joined_at, m.accepted_at, m.last_submitted_at
         FROM webxdc_memberships m
         JOIN webxdc_sessions s ON s.id = m.session_id
         WHERE m.participant_account_id = $1
           AND (s.ended_at IS NOT NULL) = $2
           AND NOT EXISTS (
               SELECT 1 FROM webxdc_tombstones t WHERE t.session_id = s.id
           )
         ORDER BY s.published_at DESC",
        participant_account_id,
        ended,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let session = Session {
                id: row.id,
                account_id: row.account_id,
                coordinator_uri: row.coordinator_uri,
                creator_account_id: row.creator_account_id,
                creator_uri: row.creator_uri,
                name: row.name,
                summary: row.summary,
                bundle_id: row.bundle_id,
                bundle_url: row.bundle_url,
                bundle_name: row.bundle_name,
                bundle_media_type: row.bundle_media_type,
                digest_multibase: row.digest_multibase,
                send_update_interval: row.send_update_interval,
                send_update_max_size: row.send_update_max_size,
                membership_policy: row.membership_policy,
                last_serial: row.last_serial,
                published_at: row.published_at,
                ended_at: row.ended_at,
            };
            let membership = Membership {
                session_id: row.id,
                participant_account_id,
                participant_uri: row.participant_uri,
                follow_id: row.follow_id,
                accepted: row.accepted,
                replay_boundary: row.replay_boundary,
                self_addr: row.self_addr,
                joined_at: row.joined_at,
                accepted_at: row.accepted_at,
                last_submitted_at: row.last_submitted_at,
            };
            (session, membership)
        })
        .collect())
}

async fn mark_internal(conn: &mut PgConnection, account_id: i64) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE accounts SET is_internal = true WHERE id = $1",
        account_id,
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// Creates or refreshes a cached remote session after the actor and bundle
/// have been validated. The immutable identity/digest guard makes a changed
/// bundle an error rather than an update.
pub async fn store_remote(pool: &PgPool, new: NewRemoteSession<'_>) -> Result<Session, DbError> {
    let mut tx = pool.begin().await?;
    if sqlx::query_scalar!(
        r#"SELECT EXISTS(
             SELECT 1 FROM webxdc_tombstones WHERE coordinator_uri = $1
           ) AS "exists!""#,
        new.coordinator_uri,
    )
    .fetch_one(&mut *tx)
    .await?
    {
        return Err(DbError::Protocol("Webxdc session was deleted".into()));
    }
    let storage_bytes = package_storage_bytes(new.bundle_bytes, new.files);
    mark_internal(&mut tx, new.account_id).await?;
    let session_id = if let Some(existing) = sqlx::query!(
        "SELECT id, bundle_id, digest_multibase, bundle_media_type
         FROM webxdc_sessions WHERE coordinator_uri = $1 FOR UPDATE",
        new.coordinator_uri,
    )
    .fetch_optional(&mut *tx)
    .await?
    {
        if existing.bundle_id != new.bundle_id
            || existing.digest_multibase != new.digest_multibase
            || existing.bundle_media_type != new.bundle_media_type
        {
            return Err(DbError::Protocol(
                "Webxdc session changed its immutable bundle".into(),
            ));
        }
        existing.id
    } else {
        id::next()
    };
    reserve_package(
        &mut tx,
        None,
        new.digest_multibase,
        new.bundle_bytes,
        new.files,
        storage_bytes,
    )
    .await?;
    let session = sqlx::query_as!(
        Session,
        r#"
        INSERT INTO webxdc_sessions
            (id, account_id, coordinator_uri, creator_uri, name, summary,
             bundle_id, bundle_url, bundle_name, bundle_media_type,
             digest_multibase, send_update_interval,
             send_update_max_size, membership_policy, published_at, ended_at,
             storage_bytes)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                'open', $14, $15, $16)
        ON CONFLICT (coordinator_uri) DO UPDATE SET
            name = EXCLUDED.name, summary = EXCLUDED.summary,
            bundle_url = EXCLUDED.bundle_url, ended_at = EXCLUDED.ended_at
        RETURNING id, account_id, coordinator_uri, creator_account_id, creator_uri,
                  name, summary, bundle_id, bundle_url, bundle_name, bundle_media_type,
                  digest_multibase, send_update_interval,
                  send_update_max_size, membership_policy, last_serial,
                  published_at, ended_at
        "#,
        session_id,
        new.account_id,
        new.coordinator_uri,
        new.creator_uri,
        new.name,
        new.summary,
        new.bundle_id,
        new.bundle_url,
        new.bundle_name,
        new.bundle_media_type,
        new.digest_multibase,
        new.send_update_interval,
        new.send_update_max_size,
        new.published_at,
        new.ended_at,
        storage_bytes,
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(session)
}

pub async fn request_membership(
    pool: &PgPool,
    session_id: i64,
    participant_account_id: i64,
    participant_uri: &str,
    follow_id: &str,
    self_addr: &str,
) -> Result<Membership, DbError> {
    Ok(sqlx::query_as!(
        Membership,
        "INSERT INTO webxdc_memberships
             (session_id, participant_account_id, participant_uri, follow_id, self_addr)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (session_id, participant_account_id) DO UPDATE SET
             follow_id = CASE WHEN webxdc_memberships.accepted
                              THEN webxdc_memberships.follow_id ELSE EXCLUDED.follow_id END
         RETURNING session_id, participant_account_id, participant_uri, follow_id,
                   accepted, replay_boundary, self_addr, joined_at, accepted_at,
                   last_submitted_at",
        session_id,
        participant_account_id,
        participant_uri,
        follow_id,
        self_addr,
    )
    .fetch_one(pool)
    .await?)
}

/// Commits the participant and replay boundary under the session row lock.
pub async fn accept_membership(
    conn: &mut PgConnection,
    session_id: i64,
    participant_account_id: i64,
    participant_uri: &str,
    follow_id: &str,
    self_addr: &str,
) -> Result<Membership, DbError> {
    let locked = sqlx::query!(
        "SELECT last_serial, ended_at FROM webxdc_sessions WHERE id = $1 FOR UPDATE",
        session_id
    )
    .fetch_one(&mut *conn)
    .await?;
    if locked.ended_at.is_some() {
        return Err(DbError::Protocol("Webxdc session has ended".into()));
    }
    let boundary = locked.last_serial;
    Ok(sqlx::query_as!(
        Membership,
        "INSERT INTO webxdc_memberships
             (session_id, participant_account_id, participant_uri, follow_id,
              accepted, replay_boundary, self_addr, accepted_at)
         VALUES ($1, $2, $3, $4, true, $5, $6, now())
         ON CONFLICT (session_id, participant_account_id) DO UPDATE SET
             accepted = true, replay_boundary = EXCLUDED.replay_boundary,
             accepted_at = coalesce(webxdc_memberships.accepted_at, now())
         RETURNING session_id, participant_account_id, participant_uri, follow_id,
                   accepted, replay_boundary, self_addr, joined_at, accepted_at,
                   last_submitted_at",
        session_id,
        participant_account_id,
        participant_uri,
        follow_id,
        boundary,
        self_addr,
    )
    .fetch_one(&mut *conn)
    .await?)
}

pub async fn mark_accepted(
    pool: &PgPool,
    follow_id: &str,
    boundary: i64,
) -> Result<Option<Membership>, DbError> {
    Ok(sqlx::query_as!(
        Membership,
        "UPDATE webxdc_memberships
         SET accepted = true, replay_boundary = $2, accepted_at = coalesce(accepted_at, now())
         WHERE follow_id = $1
         RETURNING session_id, participant_account_id, participant_uri, follow_id,
                   accepted, replay_boundary, self_addr, joined_at, accepted_at,
                   last_submitted_at",
        follow_id,
        boundary,
    )
    .fetch_optional(pool)
    .await?)
}

pub async fn remove_membership(
    pool: &PgPool,
    session_id: i64,
    participant_account_id: i64,
) -> Result<bool, DbError> {
    Ok(sqlx::query!(
        "DELETE FROM webxdc_memberships
         WHERE session_id = $1 AND participant_account_id = $2",
        session_id,
        participant_account_id,
    )
    .execute(pool)
    .await?
    .rows_affected()
        > 0)
}

pub async fn remove_membership_tx(
    conn: &mut PgConnection,
    session_id: i64,
    participant_account_id: i64,
) -> Result<bool, DbError> {
    Ok(sqlx::query!(
        "DELETE FROM webxdc_memberships
         WHERE session_id = $1 AND participant_account_id = $2",
        session_id,
        participant_account_id,
    )
    .execute(conn)
    .await?
    .rows_affected()
        > 0)
}

/// Creates a browser guest under the same session lock used for membership
/// boundaries. Open sessions accept immediately; moderated sessions retain a
/// pending row until their creator approves it.
#[allow(clippy::too_many_arguments)]
pub async fn create_guest(
    pool: &PgPool,
    guest_id: i64,
    session_id: i64,
    display_name: &str,
    token_hash: &str,
    participant_uri: &str,
    self_addr: &str,
    accept_immediately: bool,
) -> Result<Guest, DbError> {
    let mut tx = pool.begin().await?;
    let session = sqlx::query!(
        "SELECT last_serial, ended_at FROM webxdc_sessions WHERE id = $1 FOR UPDATE",
        session_id,
    )
    .fetch_one(&mut *tx)
    .await?;
    if session.ended_at.is_some() {
        return Err(DbError::Protocol("Webxdc session has ended".into()));
    }
    let replay_boundary = if accept_immediately {
        session.last_serial
    } else {
        0
    };
    let guest = sqlx::query_as!(
        Guest,
        "INSERT INTO webxdc_guests
             (id, session_id, display_name, token_hash, participant_uri,
              accepted, replay_boundary, self_addr, accepted_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8,
                 CASE WHEN $6 THEN now() ELSE NULL END)
         RETURNING id, session_id, display_name, token_hash, participant_uri,
                   accepted, replay_boundary, self_addr, joined_at, accepted_at,
                   last_submitted_at",
        guest_id,
        session_id,
        display_name,
        token_hash,
        participant_uri,
        accept_immediately,
        replay_boundary,
        self_addr,
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(guest)
}

pub async fn guest_by_token(
    pool: &PgPool,
    session_id: i64,
    token_hash: &str,
) -> Result<Option<Guest>, DbError> {
    Ok(sqlx::query_as!(
        Guest,
        "SELECT id, session_id, display_name, token_hash, participant_uri,
                accepted, replay_boundary, self_addr, joined_at, accepted_at,
                last_submitted_at
         FROM webxdc_guests WHERE session_id = $1 AND token_hash = $2",
        session_id,
        token_hash,
    )
    .fetch_optional(pool)
    .await?)
}

pub async fn guests(pool: &PgPool, session_id: i64) -> Result<Vec<Guest>, DbError> {
    Ok(sqlx::query_as!(
        Guest,
        "SELECT id, session_id, display_name, token_hash, participant_uri,
                accepted, replay_boundary, self_addr, joined_at, accepted_at,
                last_submitted_at
         FROM webxdc_guests WHERE session_id = $1 ORDER BY joined_at",
        session_id,
    )
    .fetch_all(pool)
    .await?)
}

pub async fn approve_guest(
    pool: &PgPool,
    session_id: i64,
    guest_id: i64,
) -> Result<Option<Guest>, DbError> {
    let mut tx = pool.begin().await?;
    let session = sqlx::query!(
        "SELECT last_serial, ended_at FROM webxdc_sessions WHERE id = $1 FOR UPDATE",
        session_id,
    )
    .fetch_one(&mut *tx)
    .await?;
    if session.ended_at.is_some() {
        return Err(DbError::Protocol("Webxdc session has ended".into()));
    }
    let guest = sqlx::query_as!(
        Guest,
        "UPDATE webxdc_guests SET accepted = true, replay_boundary = $3,
                accepted_at = coalesce(accepted_at, now())
         WHERE session_id = $1 AND id = $2
         RETURNING id, session_id, display_name, token_hash, participant_uri,
                   accepted, replay_boundary, self_addr, joined_at, accepted_at,
                   last_submitted_at",
        session_id,
        guest_id,
        session.last_serial,
    )
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(guest)
}

pub async fn remove_guest(pool: &PgPool, session_id: i64, guest_id: i64) -> Result<bool, DbError> {
    Ok(sqlx::query!(
        "DELETE FROM webxdc_guests WHERE session_id = $1 AND id = $2",
        session_id,
        guest_id,
    )
    .execute(pool)
    .await?
    .rows_affected()
        > 0)
}

pub async fn accepted_participants(
    pool: &PgPool,
    session_id: i64,
) -> Result<Vec<(Membership, Account)>, DbError> {
    let memberships = sqlx::query_as!(
        Membership,
        "SELECT session_id, participant_account_id, participant_uri, follow_id,
                accepted, replay_boundary, self_addr, joined_at, accepted_at,
                last_submitted_at
         FROM webxdc_memberships WHERE session_id = $1 AND accepted
         ORDER BY joined_at",
        session_id,
    )
    .fetch_all(pool)
    .await?;
    let ids: Vec<i64> = memberships
        .iter()
        .map(|m| m.participant_account_id)
        .collect();
    let accounts = crate::account::find_by_ids(pool, &ids).await?;
    Ok(memberships
        .into_iter()
        .filter_map(|m| {
            accounts
                .iter()
                .find(|a| a.id == m.participant_account_id)
                .cloned()
                .map(|a| (m, a))
        })
        .collect())
}

pub async fn participants(
    pool: &PgPool,
    session_id: i64,
) -> Result<Vec<(Membership, Account)>, DbError> {
    let memberships = sqlx::query_as!(
        Membership,
        "SELECT session_id, participant_account_id, participant_uri, follow_id,
                accepted, replay_boundary, self_addr, joined_at, accepted_at,
                last_submitted_at
         FROM webxdc_memberships WHERE session_id = $1 ORDER BY joined_at",
        session_id,
    )
    .fetch_all(pool)
    .await?;
    let ids: Vec<i64> = memberships
        .iter()
        .map(|membership| membership.participant_account_id)
        .collect();
    let accounts = crate::account::find_by_ids(pool, &ids).await?;
    Ok(memberships
        .into_iter()
        .filter_map(|membership| {
            accounts
                .iter()
                .find(|account| account.id == membership.participant_account_id)
                .cloned()
                .map(|account| (membership, account))
        })
        .collect())
}

pub async fn has_active_local_membership(pool: &PgPool, session_id: i64) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS(
             SELECT 1 FROM webxdc_memberships m
             JOIN accounts a ON a.id = m.participant_account_id
             WHERE m.session_id = $1 AND m.accepted AND a.domain IS NULL
           ) AS "exists!""#,
        session_id,
    )
    .fetch_one(pool)
    .await?)
}

pub async fn updates_after(
    pool: &PgPool,
    session_id: i64,
    serial: i64,
) -> Result<Vec<Update>, DbError> {
    Ok(sqlx::query_as!(
        Update,
        "SELECT session_id, serial, create_id, object_id, actor_uri, raw_create,
                webxdc_update, announce_id, created_at
         FROM webxdc_updates WHERE session_id = $1 AND serial > $2
         ORDER BY serial LIMIT 1000",
        session_id,
        serial,
    )
    .fetch_all(pool)
    .await?)
}

pub async fn all_updates(pool: &PgPool, session_id: i64) -> Result<Vec<Update>, DbError> {
    Ok(sqlx::query_as!(
        Update,
        "SELECT session_id, serial, create_id, object_id, actor_uri, raw_create,
                webxdc_update, announce_id, created_at
         FROM webxdc_updates WHERE session_id = $1 ORDER BY serial",
        session_id,
    )
    .fetch_all(pool)
    .await?)
}

pub async fn updates_through(
    conn: &mut PgConnection,
    session_id: i64,
    boundary: i64,
) -> Result<Vec<Update>, DbError> {
    Ok(sqlx::query_as!(
        Update,
        "SELECT session_id, serial, create_id, object_id, actor_uri, raw_create,
                webxdc_update, announce_id, created_at
         FROM webxdc_updates WHERE session_id = $1 AND serial <= $2 ORDER BY serial",
        session_id,
        boundary,
    )
    .fetch_all(&mut *conn)
    .await?)
}

/// Allocates the next serial and inserts the immutable update under one session
/// row lock. A byte-for-byte-equivalent redelivery returns the existing row;
/// identifier reuse with changed content fails closed.
pub async fn sequence_update(
    conn: &mut PgConnection,
    new: SequenceUpdate<'_>,
) -> Result<(Update, bool), DbError> {
    let session = sqlx::query!(
        "SELECT last_serial, ended_at, send_update_interval,
                creator_account_id, storage_bytes
         FROM webxdc_sessions WHERE id = $1 FOR UPDATE",
        new.session_id,
    )
    .fetch_one(&mut *conn)
    .await?;
    if session.ended_at.is_some() {
        return Err(DbError::Protocol("Webxdc session has ended".into()));
    }
    let membership = sqlx::query!(
        "SELECT accepted, last_submitted_at FROM webxdc_memberships
         WHERE session_id = $1 AND participant_account_id = $2 FOR UPDATE",
        new.session_id,
        new.participant_account_id,
    )
    .fetch_optional(&mut *conn)
    .await?
    .filter(|membership| membership.accepted)
    .ok_or_else(|| DbError::Protocol("actor is not an active Webxdc participant".into()))?;
    if let Some(existing) = sqlx::query_as!(
        Update,
        "SELECT session_id, serial, create_id, object_id, actor_uri, raw_create,
                webxdc_update, announce_id, created_at FROM webxdc_updates
         WHERE create_id = $1 OR object_id = $2",
        new.create_id,
        new.object_id,
    )
    .fetch_optional(&mut *conn)
    .await?
    {
        if existing.session_id == new.session_id
            && existing.create_id == new.create_id
            && existing.object_id == new.object_id
            && existing.actor_uri == new.actor_uri
            && existing.raw_create == *new.raw_create
        {
            return Ok((existing, false));
        }
        return Err(DbError::Protocol(
            "Webxdc update identifier was reused".into(),
        ));
    }
    let serial = next_serial(
        session.last_serial,
        membership.last_submitted_at,
        session.send_update_interval,
    )?;
    let additional_bytes = reserve_update_storage(
        conn,
        session.creator_account_id,
        session.storage_bytes,
        new.raw_create,
        new.webxdc_update,
    )
    .await?;
    let announce_id = format!("{}/activities/announce-{serial}", new.announce_id_prefix);
    let update = sqlx::query_as!(
        Update,
        "INSERT INTO webxdc_updates
             (session_id, serial, create_id, object_id, actor_uri, raw_create,
              webxdc_update, announce_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         RETURNING session_id, serial, create_id, object_id, actor_uri, raw_create,
                   webxdc_update, announce_id, created_at",
        new.session_id,
        serial,
        new.create_id,
        new.object_id,
        new.actor_uri,
        new.raw_create,
        new.webxdc_update,
        announce_id,
    )
    .fetch_one(&mut *conn)
    .await?;
    sqlx::query!(
        "UPDATE webxdc_sessions
         SET last_serial = $2, last_activity_at = now(),
             storage_bytes = storage_bytes + $3
         WHERE id = $1",
        new.session_id,
        serial,
        additional_bytes,
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "UPDATE webxdc_memberships SET last_submitted_at = now()
         WHERE session_id = $1 AND participant_account_id = $2",
        new.session_id,
        new.participant_account_id,
    )
    .execute(&mut *conn)
    .await?;
    Ok((update, true))
}

/// Coordinator sequencing for a session-scoped browser guest. This mirrors
/// `sequence_update`, but authorizes against `webxdc_guests` instead of an
/// `ActivityPub` account membership.
pub async fn sequence_guest_update(
    conn: &mut PgConnection,
    new: SequenceGuestUpdate<'_>,
) -> Result<(Update, bool), DbError> {
    let session = sqlx::query!(
        "SELECT last_serial, ended_at, send_update_interval,
                creator_account_id, storage_bytes
         FROM webxdc_sessions WHERE id = $1 FOR UPDATE",
        new.session_id,
    )
    .fetch_one(&mut *conn)
    .await?;
    if session.ended_at.is_some() {
        return Err(DbError::Protocol("Webxdc session has ended".into()));
    }
    let guest = sqlx::query!(
        "SELECT accepted, last_submitted_at FROM webxdc_guests
         WHERE session_id = $1 AND id = $2 FOR UPDATE",
        new.session_id,
        new.guest_id,
    )
    .fetch_optional(&mut *conn)
    .await?
    .filter(|guest| guest.accepted)
    .ok_or_else(|| DbError::Protocol("guest is not an active Webxdc participant".into()))?;
    if let Some(existing) = sqlx::query_as!(
        Update,
        "SELECT session_id, serial, create_id, object_id, actor_uri, raw_create,
                webxdc_update, announce_id, created_at FROM webxdc_updates
         WHERE create_id = $1 OR object_id = $2",
        new.create_id,
        new.object_id,
    )
    .fetch_optional(&mut *conn)
    .await?
    {
        if existing.session_id == new.session_id
            && existing.create_id == new.create_id
            && existing.object_id == new.object_id
            && existing.actor_uri == new.actor_uri
            && existing.raw_create == *new.raw_create
        {
            return Ok((existing, false));
        }
        return Err(DbError::Protocol(
            "Webxdc update identifier was reused".into(),
        ));
    }
    let serial = next_serial(
        session.last_serial,
        guest.last_submitted_at,
        session.send_update_interval,
    )?;
    let additional_bytes = reserve_update_storage(
        conn,
        session.creator_account_id,
        session.storage_bytes,
        new.raw_create,
        new.webxdc_update,
    )
    .await?;
    let announce_id = format!("{}/activities/announce-{serial}", new.announce_id_prefix);
    let update = sqlx::query_as!(
        Update,
        "INSERT INTO webxdc_updates
             (session_id, serial, create_id, object_id, actor_uri, raw_create,
              webxdc_update, announce_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         RETURNING session_id, serial, create_id, object_id, actor_uri, raw_create,
                   webxdc_update, announce_id, created_at",
        new.session_id,
        serial,
        new.create_id,
        new.object_id,
        new.actor_uri,
        new.raw_create,
        new.webxdc_update,
        announce_id,
    )
    .fetch_one(&mut *conn)
    .await?;
    sqlx::query!(
        "UPDATE webxdc_sessions
         SET last_serial = $2, last_activity_at = now(),
             storage_bytes = storage_bytes + $3
         WHERE id = $1",
        new.session_id,
        serial,
        additional_bytes,
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "UPDATE webxdc_guests SET last_submitted_at = now()
         WHERE session_id = $1 AND id = $2",
        new.session_id,
        new.guest_id,
    )
    .execute(&mut *conn)
    .await?;
    Ok((update, true))
}

/// Remote inboxes in the active participant set. Call only while holding the
/// session row lock used by acceptance and sequencing, so the fan-out snapshot
/// shares the update's commit boundary.
pub async fn remote_inboxes_tx(
    conn: &mut PgConnection,
    session_id: i64,
) -> Result<Vec<String>, DbError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT DISTINCT a.inbox_url AS "inbox!"
           FROM webxdc_memberships m
           JOIN accounts a ON a.id = m.participant_account_id
           WHERE m.session_id = $1 AND m.accepted
             AND a.domain IS NOT NULL AND a.inbox_url <> ''
           ORDER BY a.inbox_url"#,
        session_id,
    )
    .fetch_all(&mut *conn)
    .await?)
}

/// Locks one non-tombstoned session for a lifecycle transition. Callers take
/// this before snapshotting participants so no concurrently sequenced update
/// can commit between the final participant set and a purge.
pub async fn lock_live_tx(conn: &mut PgConnection, session_id: i64) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar!(
        "SELECT s.id FROM webxdc_sessions s
         WHERE s.id = $1
           AND NOT EXISTS (
               SELECT 1 FROM webxdc_tombstones t WHERE t.session_id = s.id
           )
         FOR UPDATE",
        session_id,
    )
    .fetch_optional(conn)
    .await?
    .is_some())
}

pub async fn store_announced(
    pool: &PgPool,
    new: AnnouncedUpdate<'_>,
) -> Result<(Update, bool), DbError> {
    let mut tx = pool.begin().await?;
    let session = sqlx::query!(
        "SELECT last_serial, creator_account_id, storage_bytes
         FROM webxdc_sessions WHERE id = $1 FOR UPDATE",
        new.session_id
    )
    .fetch_one(&mut *tx)
    .await?;
    if let Some(existing) = sqlx::query_as!(
        Update,
        "SELECT session_id, serial, create_id, object_id, actor_uri, raw_create,
                webxdc_update, announce_id, created_at FROM webxdc_updates
         WHERE (session_id = $1 AND serial = $2) OR create_id = $3 OR object_id = $4",
        new.session_id,
        new.serial,
        new.create_id,
        new.object_id,
    )
    .fetch_optional(&mut *tx)
    .await?
    {
        if existing.session_id == new.session_id
            && existing.serial == new.serial
            && existing.create_id == new.create_id
            && existing.object_id == new.object_id
            && existing.actor_uri == new.actor_uri
            && existing.raw_create == *new.raw_create
            && existing.webxdc_update == *new.webxdc_update
            && existing.announce_id == new.announce_id
        {
            tx.commit().await?;
            return Ok((existing, false));
        }
        return Err(DbError::Protocol("conflicting Webxdc announcement".into()));
    }
    let additional_bytes = reserve_update_storage(
        &mut tx,
        session.creator_account_id,
        session.storage_bytes,
        new.raw_create,
        new.webxdc_update,
    )
    .await?;
    let update = sqlx::query_as!(
        Update,
        "INSERT INTO webxdc_updates
             (session_id, serial, create_id, object_id, actor_uri, raw_create,
              webxdc_update, announce_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         RETURNING session_id, serial, create_id, object_id, actor_uri, raw_create,
                   webxdc_update, announce_id, created_at",
        new.session_id,
        new.serial,
        new.create_id,
        new.object_id,
        new.actor_uri,
        new.raw_create,
        new.webxdc_update,
        new.announce_id,
    )
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE webxdc_sessions
         SET last_serial = greatest(last_serial, $2), last_activity_at = now(),
             storage_bytes = storage_bytes + $3
         WHERE id = $1",
        new.session_id,
        new.serial,
        additional_bytes,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((update, true))
}

pub async fn attach_invitation(
    pool: &PgPool,
    status_id: i64,
    session_id: i64,
) -> Result<(), DbError> {
    sqlx::query!(
        "INSERT INTO webxdc_invitations (status_id, session_id) VALUES ($1, $2)
         ON CONFLICT (status_id) DO UPDATE SET session_id = EXCLUDED.session_id",
        status_id,
        session_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn attach_invitation_tx(
    conn: &mut PgConnection,
    status_id: i64,
    session_id: i64,
) -> Result<(), DbError> {
    sqlx::query!(
        "INSERT INTO webxdc_invitations (status_id, session_id) VALUES ($1, $2)
         ON CONFLICT (status_id) DO UPDATE SET session_id = EXCLUDED.session_id",
        status_id,
        session_id,
    )
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn invitations_for_statuses(
    pool: &PgPool,
    status_ids: &[i64],
) -> Result<HashMap<i64, (i64, String, String)>, DbError> {
    if status_ids.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(sqlx::query!(
        "SELECT i.status_id, s.id AS session_id, s.coordinator_uri, s.name
         FROM webxdc_invitations i JOIN webxdc_sessions s ON s.id = i.session_id
         WHERE i.status_id = ANY($1)",
        status_ids,
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| {
        (
            row.status_id,
            (row.session_id, row.coordinator_uri, row.name),
        )
    })
    .collect())
}

/// Replaces a live session and all of its cascading child data with a small
/// durable tombstone. The ordinary account and key rows are deliberately not
/// deleted so queued `Delete` deliveries can still be signed.
pub async fn purge_as_tombstone_tx(
    conn: &mut PgConnection,
    session_id: i64,
    deleted_at: OffsetDateTime,
) -> Result<bool, DbError> {
    let Some(coordinator) = sqlx::query!(
        "SELECT s.coordinator_uri, s.account_id, a.username, a.domain
         FROM webxdc_sessions s
         JOIN accounts a ON a.id = s.account_id
         WHERE s.id = $1 FOR UPDATE OF s, a",
        session_id,
    )
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(false);
    };
    let reserved_username = coordinator
        .domain
        .is_none()
        .then_some(coordinator.username.as_str());
    sqlx::query!(
        "INSERT INTO webxdc_tombstones
             (session_id, coordinator_uri, deleted_at, coordinator_account_id,
              coordinator_username)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (session_id) DO NOTHING",
        session_id,
        coordinator.coordinator_uri,
        deleted_at,
        coordinator.account_id,
        reserved_username,
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "UPDATE accounts
         SET is_internal = true,
             suspended_at = coalesce(suspended_at, $2),
             suspension_origin = CASE WHEN domain IS NULL THEN 'local' ELSE 'remote' END,
             deleted_at = coalesce(deleted_at, $2),
             discoverable = false,
             indexable = false
         WHERE id = $1",
        coordinator.account_id,
        deleted_at,
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!("DELETE FROM webxdc_sessions WHERE id = $1", session_id)
        .execute(&mut *conn)
        .await?;
    Ok(true)
}

/// Drops a cached remote session once the last local account has left it. No
/// tombstone is created: this is a local cache decision, not an assertion that
/// the remote coordinator deleted the actor, so a future explicit open may
/// fetch it again.
pub async fn discard_remote_if_unreferenced_tx(
    conn: &mut PgConnection,
    session_id: i64,
) -> Result<bool, DbError> {
    let account_id = sqlx::query_scalar!(
        "DELETE FROM webxdc_sessions s
         USING accounts coordinator
         WHERE s.id = $1 AND coordinator.id = s.account_id
           AND coordinator.domain IS NOT NULL
           AND NOT EXISTS (
               SELECT 1 FROM webxdc_memberships m
               JOIN accounts participant ON participant.id = m.participant_account_id
               WHERE m.session_id = s.id AND participant.domain IS NULL
           )
         RETURNING s.account_id",
        session_id,
    )
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(account_id) = account_id {
        sqlx::query!(
            "DELETE FROM accounts
             WHERE id = $1 AND domain IS NOT NULL AND is_internal
               AND NOT EXISTS (
                   SELECT 1 FROM webxdc_sessions WHERE account_id = $1
               )",
            account_id,
        )
        .execute(&mut *conn)
        .await?;
        return Ok(true);
    }
    Ok(false)
}

/// Unconditionally drops one expired cached remote session and all its child
/// rows. A locally coordinated session is never selected by this query.
pub async fn discard_remote_cache(pool: &PgPool, session_id: i64) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let account_id = sqlx::query_scalar!(
        "DELETE FROM webxdc_sessions s
         USING accounts coordinator
         WHERE s.id = $1 AND coordinator.id = s.account_id
           AND coordinator.domain IS NOT NULL
         RETURNING s.account_id",
        session_id,
    )
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(account_id) = account_id {
        sqlx::query!(
            "DELETE FROM accounts
             WHERE id = $1 AND domain IS NOT NULL AND is_internal
               AND NOT EXISTS (
                   SELECT 1 FROM webxdc_sessions WHERE account_id = $1
               )",
            account_id,
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(account_id.is_some())
}

/// Removes retained coordinator accounts once their deletion fan-out has
/// drained. The protocol tombstone and reserved synthetic username survive;
/// only the generic account row and its cascading signing keys are reaped.
pub async fn prune_tombstoned_coordinator_accounts(
    pool: &PgPool,
    deleted_before: OffsetDateTime,
    limit: i64,
) -> Result<u64, DbError> {
    let result = sqlx::query!(
        r#"
        WITH candidates AS (
            SELECT a.id
            FROM webxdc_tombstones t
            JOIN accounts a ON a.id = t.coordinator_account_id
            WHERE t.deleted_at <= $1
              AND a.is_internal
              AND NOT EXISTS (
                  SELECT 1 FROM delivery_jobs j WHERE j.account_id = a.id
              )
              AND NOT EXISTS (
                  SELECT 1 FROM webxdc_sessions s WHERE s.account_id = a.id
              )
            ORDER BY t.deleted_at, a.id
            LIMIT $2
            FOR UPDATE OF a SKIP LOCKED
        )
        DELETE FROM accounts a
        USING candidates c
        WHERE a.id = c.id
        "#,
        deleted_before,
        limit,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Active local sessions idle past `active_cutoff`, plus any local or cached
/// remote sessions closed past `closed_cutoff`, in bounded maintenance order.
pub async fn lifecycle_candidates(
    pool: &PgPool,
    active_cutoff: OffsetDateTime,
    closed_cutoff: OffsetDateTime,
    limit: i64,
) -> Result<Vec<Session>, DbError> {
    Ok(sqlx::query_as!(
        Session,
        "SELECT s.id, s.account_id, s.coordinator_uri, s.creator_account_id,
                s.creator_uri, s.name, s.summary, s.bundle_id, s.bundle_url,
                s.bundle_name, s.bundle_media_type, s.digest_multibase,
                s.send_update_interval, s.send_update_max_size,
                s.membership_policy, s.last_serial, s.published_at, s.ended_at
         FROM webxdc_sessions s
         JOIN accounts coordinator ON coordinator.id = s.account_id
         WHERE NOT EXISTS (
                   SELECT 1 FROM webxdc_tombstones t WHERE t.session_id = s.id
               )
           AND (
               (s.ended_at IS NULL AND coordinator.domain IS NULL
                AND s.last_activity_at <= $1)
               OR s.ended_at <= $2
           )
         ORDER BY coalesce(s.ended_at, s.last_activity_at), s.id
         LIMIT $3",
        active_cutoff,
        closed_cutoff,
        limit,
    )
    .fetch_all(pool)
    .await?)
}

pub async fn close(pool: &PgPool, session_id: i64) -> Result<Option<Session>, DbError> {
    Ok(sqlx::query_as!(
        Session,
        "UPDATE webxdc_sessions SET ended_at = coalesce(ended_at, now()) WHERE id = $1
         RETURNING id, account_id, coordinator_uri, creator_account_id, creator_uri,
                   name, summary, bundle_id, bundle_url, bundle_name, bundle_media_type,
                   digest_multibase, send_update_interval,
                   send_update_max_size, membership_policy, last_serial,
                   published_at, ended_at",
        session_id,
    )
    .fetch_optional(pool)
    .await?)
}

pub async fn close_at(
    pool: &PgPool,
    session_id: i64,
    ended_at: OffsetDateTime,
) -> Result<Option<Session>, DbError> {
    Ok(sqlx::query_as!(
        Session,
        "UPDATE webxdc_sessions SET ended_at = coalesce(ended_at, $2) WHERE id = $1
         RETURNING id, account_id, coordinator_uri, creator_account_id, creator_uri,
                   name, summary, bundle_id, bundle_url, bundle_name, bundle_media_type,
                   digest_multibase, send_update_interval,
                   send_update_max_size, membership_policy, last_serial,
                   published_at, ended_at",
        session_id,
        ended_at,
    )
    .fetch_optional(pool)
    .await?)
}

pub async fn update_bundle_url(
    pool: &PgPool,
    session_id: i64,
    bundle_url: &str,
) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE webxdc_sessions SET bundle_url = $2 WHERE id = $1",
        session_id,
        bundle_url,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn close_at_tx(
    conn: &mut PgConnection,
    session_id: i64,
    ended_at: OffsetDateTime,
) -> Result<Option<Session>, DbError> {
    Ok(sqlx::query_as!(
        Session,
        "UPDATE webxdc_sessions SET ended_at = coalesce(ended_at, $2) WHERE id = $1
         RETURNING id, account_id, coordinator_uri, creator_account_id, creator_uri,
                   name, summary, bundle_id, bundle_url, bundle_name, bundle_media_type,
                   digest_multibase, send_update_interval,
                   send_update_max_size, membership_policy, last_serial,
                   published_at, ended_at",
        session_id,
        ended_at,
    )
    .fetch_optional(conn)
    .await?)
}

/// Resolve an already-known app link without loading its bundle or fetching it.
pub async fn invitation_in_urls(
    pool: &PgPool,
    urls: &[&str],
) -> Result<Option<(i64, String, String)>, DbError> {
    if urls.is_empty() {
        return Ok(None);
    }
    Ok(sqlx::query_as("SELECT id, coordinator_uri, name FROM webxdc_sessions WHERE coordinator_uri = ANY($1) AND ended_at IS NULL ORDER BY array_position($1::text[], coordinator_uri) LIMIT 1")
        .bind(urls).fetch_optional(pool).await?)
}

/// Lightweight display metadata for local and remote invitation posts.
pub async fn invitation_cards(
    pool: &PgPool,
    status_ids: &[i64],
) -> Result<HashMap<i64, (Option<i64>, String, String)>, DbError> {
    if status_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(i64, Option<i64>, String, String)> = sqlx::query_as(
        "SELECT i.status_id, s.id, s.coordinator_uri, s.name FROM webxdc_invitations i JOIN webxdc_sessions s ON s.id=i.session_id WHERE i.status_id=ANY($1)
         UNION ALL SELECT i.status_id, s.id, i.session_uri, i.session_name FROM webxdc_invitation_links i LEFT JOIN webxdc_sessions s ON s.coordinator_uri=i.session_uri WHERE i.status_id=ANY($1)")
        .bind(status_ids).fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|(id, session, uri, name)| (id, (session, uri, name)))
        .collect())
}

pub async fn set_remote_invitation(
    pool: &PgPool,
    status_id: i64,
    invitation: Option<(&str, &str)>,
) -> Result<(), DbError> {
    if let Some((uri, name)) = invitation {
        sqlx::query("INSERT INTO webxdc_invitation_links(status_id,session_uri,session_name) VALUES($1,$2,$3) ON CONFLICT(status_id) DO UPDATE SET session_uri=excluded.session_uri,session_name=excluded.session_name")
            .bind(status_id).bind(uri).bind(name).execute(pool).await?;
    } else {
        sqlx::query("DELETE FROM webxdc_invitation_links WHERE status_id=$1")
            .bind(status_id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

#[derive(Debug, FromRow)]
pub struct AdminSession {
    pub id: i64,
    pub name: String,
    pub creator_uri: String,
    pub creator_label: String,
    pub local: bool,
    pub ended_at: Option<OffsetDateTime>,
    pub package_bytes: i64,
    pub data_bytes: i64,
    pub package_sessions: i64,
}

pub async fn admin_sessions(
    pool: &PgPool,
    search: &str,
    kind: &str,
    max_id: Option<i64>,
    limit: i64,
) -> Result<Vec<AdminSession>, DbError> {
    Ok(sqlx::query_as(
        "SELECT s.id, s.name, s.creator_uri, coalesce('@' || creator.username || coalesce('@' || creator.domain,''),
                split_part(s.creator_uri,'/',3)) AS creator_label, a.domain IS NULL AS local, s.ended_at,
                p.storage_bytes AS package_bytes,
                greatest(s.storage_bytes-p.storage_bytes,0)::bigint AS data_bytes,
                (SELECT count(*) FROM webxdc_sessions other WHERE other.digest_multibase=s.digest_multibase) AS package_sessions
         FROM webxdc_sessions s JOIN accounts a ON a.id=s.account_id
         JOIN webxdc_packages p USING (digest_multibase)
         LEFT JOIN accounts creator ON creator.id=s.creator_account_id
         WHERE ($1='' OR s.name ILIKE '%' || $1 || '%' OR s.creator_uri ILIKE '%' || $1 || '%'
                OR ('@' || creator.username || coalesce('@' || creator.domain,'')) ILIKE '%' || $1 || '%')
           AND ($2='' OR ($2='local' AND a.domain IS NULL) OR ($2='remote' AND a.domain IS NOT NULL)
                OR ($2='ended' AND s.ended_at IS NOT NULL))
           AND ($3::bigint IS NULL OR s.id<$3)
         ORDER BY s.id DESC LIMIT $4")
        .bind(search).bind(kind).bind(max_id).bind(limit).fetch_all(pool).await?)
}

#[derive(Debug, FromRow)]
pub struct SessionStorage {
    pub package_bytes: i64,
    pub data_bytes: i64,
    pub package_sessions: i64,
    pub members: i64,
    pub guests: i64,
}

pub async fn session_storage(
    pool: &PgPool,
    session_id: i64,
) -> Result<Option<SessionStorage>, DbError> {
    Ok(sqlx::query_as(
        "SELECT p.storage_bytes AS package_bytes,
                greatest(s.storage_bytes-p.storage_bytes,0)::bigint AS data_bytes,
                (SELECT count(*) FROM webxdc_sessions other WHERE other.digest_multibase=s.digest_multibase) AS package_sessions,
                (SELECT count(*) FROM webxdc_memberships m WHERE m.session_id=s.id) AS members,
                (SELECT count(*) FROM webxdc_guests g WHERE g.session_id=s.id) AS guests
         FROM webxdc_sessions s JOIN webxdc_packages p USING (digest_multibase) WHERE s.id=$1")
        .bind(session_id).fetch_optional(pool).await?)
}
