//! Persistent, bounded catalogue walks, independent of catalogue freshness.
use super::{Lease, PgPool, Tx, invalid, lock_settings, valid_lease};
use crate::DbError;

#[derive(Debug, Clone, Copy, Default, sqlx::FromRow)]
pub struct SamplingPlan {
    pub round: i64,
    pub offset: i32,
    pub seed: i64,
}
impl SamplingPlan {
    /// Four active/deeper pages, then a page giving newcomers a chance.
    #[must_use]
    pub fn newcomers(self) -> bool {
        self.round % 5 == 4
    }
}

pub async fn sampling_plan(pool: &PgPool, lease: &Lease) -> Result<SamplingPlan, DbError> {
    let mut tx = pool.begin().await?;
    let settings = lock_settings(&mut tx).await?;
    if !valid_lease(&mut tx, &settings, lease).await? {
        return Err(invalid("stale sampling lease"));
    }
    sqlx::query!(
        "INSERT INTO discovery_sampling(source_id) VALUES($1) ON CONFLICT DO NOTHING",
        lease.source_id
    )
    .execute(&mut *tx)
    .await?;
    let plan = sqlx::query_as!(
        SamplingPlan,
        r#"SELECT p.round,CASE WHEN p.round%5=4 AND NOT('video_catalogue'=ANY(s.capabilities)
            OR s.endpoint LIKE '%/api/v1/videos') THEN p.new_offset ELSE p.active_offset END AS "offset!",p.seed
        FROM discovery_sampling p JOIN discovery_sources s ON s.id=p.source_id WHERE p.source_id=$1"#,
        lease.source_id
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(plan)
}

pub(super) async fn advance_sampling(
    tx: &mut Tx<'_>,
    source: i64,
    more: bool,
    page_size: i32,
    candidates: usize,
) -> Result<(), DbError> {
    let count = i64::try_from(candidates).map_err(|_| invalid("invalid sample size"))?;
    sqlx::query!(
        "INSERT INTO discovery_sampling(source_id) VALUES($1) ON CONFLICT DO NOTHING",
        source
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query("UPDATE discovery_sampling SET
        active_offset=CASE WHEN $3=8 OR round%5<>4 THEN CASE WHEN $2 AND active_offset+$3<400 THEN active_offset+$3 ELSE 0 END ELSE active_offset END,
        new_offset=CASE WHEN $3<>8 AND round%5=4 THEN CASE WHEN $2 AND new_offset+$3<400 THEN new_offset+$3 ELSE 0 END ELSE new_offset END,
        round=round+1,pages=pages+1,candidates=candidates+$4,
        zero_yield=CASE WHEN $4=0 THEN LEAST(zero_yield+1,7) ELSE 0 END,
        next_at=clock_timestamp()+CASE WHEN $4>0 THEN interval '2 hours'
            WHEN zero_yield>=1 THEN LEAST(interval '7 days',interval '12 hours'*(zero_yield+1)) ELSE interval '2 hours' END
        WHERE source_id=$1").bind(source).bind(more).bind(page_size).bind(count).execute(&mut **tx).await?;
    Ok(())
}

#[derive(Debug, serde::Serialize)]
pub struct FamilyDiagnostic {
    pub family: String,
    pub operation: String,
    pub requests: i64,
    pub charged_bytes: i64,
}

#[derive(Debug, serde::Serialize)]
pub struct CoverageDiagnostic {
    pub kind: String,
    pub fresh_entries: i64,
    pub authors: i64,
    pub origins: i64,
}
pub async fn coverage_diagnostics(pool: &PgPool) -> Result<Vec<CoverageDiagnostic>, DbError> {
    Ok(sqlx::query_as!(
        CoverageDiagnostic,
        r#"SELECT e.kind,count(*) AS "fresh_entries!",
        count(DISTINCT e.actor_uri) AS "authors!",count(DISTINCT s.origin) AS "origins!"
        FROM discovery_entries e JOIN discovery_sources s ON s.id=e.source_id
        WHERE e.state='active' AND e.valid_until>clock_timestamp() AND s.enabled
        AND s.verified_at IS NOT NULL GROUP BY e.kind ORDER BY e.kind"#
    )
    .fetch_all(pool)
    .await?)
}
pub async fn family_diagnostics(pool: &PgPool) -> Result<Vec<FamilyDiagnostic>, DbError> {
    Ok(sqlx::query_as!(FamilyDiagnostic,r#"SELECT family,operation,count(*) AS "requests!",
        COALESCE(sum(bytes),0)::bigint AS "charged_bytes!" FROM discovery_requests
        WHERE started_at>clock_timestamp()-interval '24 hours' GROUP BY family,operation ORDER BY family,operation"#)
        .fetch_all(pool).await?)
}
