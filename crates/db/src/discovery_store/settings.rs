use super::{PgPool, Tx, invalid};
use crate::DbError;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, sqlx::FromRow)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent persisted discovery controls"
)]
pub struct Settings {
    pub singleton: bool,
    pub enabled: bool,
    pub paused: bool,
    pub allow_missing_discoverable: bool,
    pub allow_missing_indexable: bool,
    pub disabled_mechanisms: Vec<String>,
    pub revision: i64,
    pub max_sources: i64,
    pub sources_per_origin: i64,
    pub sources_per_introducer: i64,
    pub max_jobs: i64,
    pub jobs_per_origin: i64,
    pub jobs_per_introducer: i64,
    pub max_entries: i64,
    pub entries_per_origin: i64,
    pub entries_per_introducer: i64,
    pub max_inflight: i32,
    pub max_attempts: i32,
    pub lease_seconds: i32,
    pub requests_per_day: i64,
    pub requests_per_origin: i64,
    pub bytes_per_day: i64,
}

pub async fn settings(pool: &PgPool) -> Result<Settings, DbError> {
    Ok(
        sqlx::query_as!(Settings, "SELECT * FROM discovery_settings WHERE singleton")
            .fetch_one(pool)
            .await?,
    )
}

pub(super) async fn lock_settings(tx: &mut Tx<'_>) -> Result<Settings, DbError> {
    Ok(sqlx::query_as!(
        Settings,
        "SELECT * FROM discovery_settings WHERE singleton FOR UPDATE"
    )
    .fetch_one(&mut **tx)
    .await?)
}

/// Compare-and-swap the policy and limits. A changed policy fences and releases
/// old claims immediately, but preserves attempts, cursors and retry times.
/// Limits cannot be lowered below retained usage, including withdrawal guards.
pub async fn save_settings(pool: &PgPool, next: &Settings) -> Result<bool, DbError> {
    if !next.singleton || next.disabled_mechanisms.len() > 13 {
        return Err(invalid("invalid discovery settings"));
    }
    let mut tx = pool.begin().await?;
    let current = lock_settings(&mut tx).await?;
    if current.revision != next.revision {
        return Ok(false);
    }
    if current == *next {
        return Ok(true);
    }
    let fits = sqlx::query_scalar!(r#"SELECT
        (SELECT count(*) FROM discovery_sources) <= $1
        AND NOT EXISTS (SELECT 1 FROM discovery_sources GROUP BY origin HAVING count(*) > $2)
        AND NOT EXISTS (SELECT 1 FROM discovery_sources GROUP BY introduced_by HAVING count(*) > $3)
        AND (SELECT count(*) FROM discovery_jobs) <= $4
        AND NOT EXISTS (SELECT 1 FROM discovery_jobs GROUP BY origin HAVING count(*) > $5)
        AND NOT EXISTS (SELECT 1 FROM discovery_jobs GROUP BY introduced_by HAVING count(*) > $6)
        AND (SELECT count(*) FROM discovery_entries) <= $7
        AND NOT EXISTS (SELECT 1 FROM discovery_entries e JOIN discovery_sources s ON s.id=e.source_id
            GROUP BY s.origin HAVING count(*) > $8)
        AND NOT EXISTS (SELECT 1 FROM discovery_entries e JOIN discovery_sources s ON s.id=e.source_id
            GROUP BY s.introduced_by HAVING count(*) > $9) AS "fits!""#,
        next.max_sources, next.sources_per_origin, next.sources_per_introducer,
        next.max_jobs, next.jobs_per_origin, next.jobs_per_introducer,
        next.max_entries, next.entries_per_origin, next.entries_per_introducer,
    ).fetch_one(&mut *tx).await?;
    if !fits {
        return Err(invalid("discovery limits are below retained usage"));
    }
    sqlx::query!(
        r#"UPDATE discovery_settings SET enabled=$1,paused=$2,
        allow_missing_discoverable=$3,allow_missing_indexable=$4,disabled_mechanisms=$5,
        max_sources=$6,sources_per_origin=$7,sources_per_introducer=$8,
        max_jobs=$9,jobs_per_origin=$10,jobs_per_introducer=$11,
        max_entries=$12,entries_per_origin=$13,entries_per_introducer=$14,
        max_inflight=$15,max_attempts=$16,lease_seconds=$17,requests_per_day=$18,requests_per_origin=$19,bytes_per_day=$20,revision=revision+1
        WHERE singleton"#,
        next.enabled,
        next.paused,
        next.allow_missing_discoverable,
        next.allow_missing_indexable,
        &next.disabled_mechanisms,
        next.max_sources,
        next.sources_per_origin,
        next.sources_per_introducer,
        next.max_jobs,
        next.jobs_per_origin,
        next.jobs_per_introducer,
        next.max_entries,
        next.entries_per_origin,
        next.entries_per_introducer,
        next.max_inflight,
        next.max_attempts,
        next.lease_seconds,
        next.requests_per_day,
        next.requests_per_origin,
        next.bytes_per_day
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!("UPDATE discovery_jobs SET lease_token=NULL,lease_owner=NULL,leased_until=NULL")
        .execute(&mut *tx)
        .await?;
    if !next.allow_missing_indexable {
        sqlx::query!("UPDATE discovery_entries SET state='denied',revision=revision+1,title=NULL,summary=NULL,language=NULL,published_at=NULL,sensitive=false,invalidation_reason='missing_indexing_denied',updated_at=clock_timestamp() WHERE kind IN ('post','video') AND state='active' AND (indexable='missing' OR EXISTS(SELECT 1 FROM discovery_entries a WHERE a.canonical_uri=discovery_entries.actor_uri AND a.kind='profile' AND a.indexable='missing'))").execute(&mut *tx).await?;
    }
    if !next.allow_missing_discoverable {
        sqlx::query!("UPDATE discovery_entries SET state='denied',revision=revision+1,title=NULL,summary=NULL,language=NULL,published_at=NULL,sensitive=false,invalidation_reason='missing_discovery_denied',updated_at=clock_timestamp() WHERE kind='community' AND state='active' AND discoverable='missing'").execute(&mut *tx).await?;
    }
    if next.requests_per_day > current.requests_per_day
        || next.bytes_per_day > current.bytes_per_day
    {
        sqlx::query("UPDATE discovery_jobs SET run_at=now() WHERE last_error='budget_exhausted' AND lease_token IS NULL AND NOT withdrawn")
            .execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(true)
}

impl Settings {
    pub(super) fn runs(&self, mechanism: &str) -> bool {
        self.enabled && !self.paused && !self.disabled_mechanisms.iter().any(|m| m == mechanism)
    }
}
