use super::{
    Admission, Completion, Lease, PgPool, address, bounded, delete_claim, evidence_times,
    introducer_valid, lock_settings, valid_lease,
};
use crate::{DbError, id};
use time::OffsetDateTime;

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct Source {
    pub next_ordinal: i64,
    pub depth: i16,
    pub provenance: String,
    pub peer_next_at: OffsetDateTime,
    pub peer_offset: i32,
    pub peers_verified_at: Option<OffsetDateTime>,
    pub peer_error: Option<String>,
    pub id: i64,
    pub origin: String,
    pub endpoint: String,
    pub introduced_by: Option<i64>,
    pub enabled: bool,
    pub revision: i64,
    pub canonical_uri: Option<String>,
    pub capabilities: Vec<String>,
    pub verified_at: Option<OffsetDateTime>,
    pub valid_until: Option<OffsetDateTime>,
    pub cooldown_until: Option<OffsetDateTime>,
    /// Last failure reason; `last_success_at` can be newer after recovery.
    pub last_error: Option<String>,
    pub attempts_total: i64,
    pub created_at: OffsetDateTime,
    pub last_success_at: Option<OffsetDateTime>,
    pub last_failure_at: Option<OffsetDateTime>,
}

pub async fn source(pool: &PgPool, id: i64) -> Result<Option<Source>, DbError> {
    Ok(
        sqlx::query_as!(Source, "SELECT * FROM discovery_sources WHERE id=$1", id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn add_source(
    pool: &PgPool,
    endpoint: &str,
    introduced_by: Option<i64>,
) -> Result<Admission, DbError> {
    add_source_with_provenance(pool, endpoint, introduced_by, "operator").await
}

pub async fn add_source_with_provenance(
    pool: &PgPool,
    endpoint: &str,
    introduced_by: Option<i64>,
    provenance: &str,
) -> Result<Admission, DbError> {
    let mut tx = pool.begin().await?;
    let settings = lock_settings(&mut tx).await?;
    let result = add_source_locked(&mut tx, &settings, endpoint, introduced_by, provenance).await?;
    tx.commit().await?;
    Ok(result)
}

pub(super) async fn add_source_locked(
    tx: &mut super::Tx<'_>,
    settings: &super::Settings,
    endpoint: &str,
    introduced_by: Option<i64>,
    provenance: &str,
) -> Result<Admission, DbError> {
    let (endpoint, origin) = address(endpoint)?;
    if !["operator", "bundled", "import", "peer"].contains(&provenance) {
        return Err(super::invalid("invalid source provenance"));
    }
    let depth = if let Some(parent) = introduced_by {
        sqlx::query_scalar!("SELECT depth FROM discovery_sources WHERE id=$1", parent)
            .fetch_optional(&mut **tx)
            .await?
            .unwrap_or(2)
            + 1
    } else {
        0
    };
    if depth > 2 {
        return Ok(Admission::Suppressed);
    }
    if !settings.runs("source_inspection") {
        return Ok(Admission::Disabled);
    }
    if !introducer_valid(tx, introduced_by).await? {
        return Ok(Admission::Suppressed);
    }
    if let Some(existing) = sqlx::query!(
        "SELECT id,enabled,provenance FROM discovery_sources WHERE endpoint=$1",
        endpoint
    )
    .fetch_optional(&mut **tx)
    .await?
    {
        if !existing.enabled {
            return Ok(Admission::Suppressed);
        }
        if existing.provenance == "peer" && ["operator", "import"].contains(&provenance) {
            return super::expansion::adopt_source(tx, settings, existing.id, provenance).await;
        }
        return Ok(Admission::Existing(existing.id));
    }
    let counts = sqlx::query!(
        r#"SELECT count(*) AS "total!",
        count(*) FILTER (WHERE origin=$1) AS "origin!",
        count(*) FILTER (WHERE introduced_by IS NOT DISTINCT FROM $2) AS "introducer!"
        FROM discovery_sources"#,
        origin,
        introduced_by
    )
    .fetch_one(&mut **tx)
    .await?;
    if counts.total >= settings.max_sources
        || (provenance == "peer" && counts.total >= (settings.max_sources * 4 / 5).max(1))
        || counts.origin >= settings.sources_per_origin
        || counts.introducer >= settings.sources_per_introducer
    {
        return Ok(Admission::Capacity);
    }
    let id = id::next();
    sqlx::query!(
        "INSERT INTO discovery_sources(id,origin,endpoint,introduced_by,depth,provenance,cooldown_until,last_error,last_failure_at)
        SELECT $1,$2,$3,$4,$5,$6,max(cooldown_until),
            CASE WHEN max(cooldown_until) IS NOT NULL THEN 'rate_limited' END,
            max(last_failure_at)
        FROM discovery_sources WHERE origin=$2 AND last_error='rate_limited'
            AND cooldown_until>clock_timestamp()",
        id,
        origin,
        endpoint,
        introduced_by, depth, provenance
    )
    .execute(&mut **tx)
    .await?;
    Ok(Admission::Added(id))
}

/// Operator/policy suspension; a hint cannot re-enable a source. Both directions
/// increment the revision so toggling off/on cannot revive an earlier claim.
pub async fn set_source_enabled(pool: &PgPool, id: i64, enabled: bool) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    lock_settings(&mut tx).await?;
    let changed = sqlx::query!(
        "UPDATE discovery_sources SET enabled=$2,revision=revision+1 WHERE id=$1 AND enabled<>$2",
        id,
        enabled
    )
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if changed {
        sqlx::query!("UPDATE discovery_jobs SET lease_token=NULL,lease_owner=NULL,leased_until=NULL WHERE source_id=$1",id)
            .execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(changed)
}

pub struct VerifiedSource<'a> {
    pub canonical_uri: &'a str,
    pub capabilities: &'a [String],
    pub verified_at: OffsetDateTime,
    pub valid_until: OffsetDateTime,
}

/// The caller supplies facts verified at their authority, never directory hints.
pub async fn finish_source(
    pool: &PgPool,
    lease: &Lease,
    facts: &VerifiedSource<'_>,
) -> Result<Completion, DbError> {
    let (canonical, _) = address(facts.canonical_uri)?;
    evidence_times(facts.verified_at, facts.valid_until)?;
    if facts.capabilities.len() > 6 {
        return Err(super::invalid("too many discovery capabilities"));
    }
    let mut tx = pool.begin().await?;
    let settings = lock_settings(&mut tx).await?;
    if lease.operation != "inspect_source"
        || !valid_lease(&mut tx, &settings, lease).await?
        || facts.verified_at
            < lease.leased_until - time::Duration::seconds(i64::from(settings.lease_seconds))
    {
        return Ok(Completion::Stale);
    }
    sqlx::query!("UPDATE discovery_sources SET canonical_uri=$2,capabilities=$3,verified_at=$4,valid_until=$5,cooldown_until=NULL,last_success_at=clock_timestamp() WHERE id=$1",
        lease.source_id,canonical,facts.capabilities,facts.verified_at,facts.valid_until).execute(&mut *tx).await?;
    delete_claim(&mut tx, lease).await?;
    tx.commit().await?;
    Ok(Completion::Committed)
}

pub async fn set_source_cooldown(
    pool: &PgPool,
    id: i64,
    until: OffsetDateTime,
    reason: &str,
) -> Result<bool, DbError> {
    bounded(reason, 128)?;
    let mut tx = pool.begin().await?;
    lock_settings(&mut tx).await?;
    let changed = sqlx::query!("UPDATE discovery_sources SET cooldown_until=$2,last_error=$3,revision=revision+1 WHERE id=$1",id,until,reason)
        .execute(&mut *tx).await?.rows_affected() == 1;
    sqlx::query!("UPDATE discovery_jobs SET lease_token=NULL,lease_owner=NULL,leased_until=NULL WHERE source_id=$1",id)
        .execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(changed)
}
