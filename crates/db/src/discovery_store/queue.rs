use super::{
    Admission, Completion, PgPool, Settings, Tx, address, bounded, introducer_valid, invalid,
    lock_settings,
};
use crate::{DbError, id};
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    InspectSource,
    ListPeers,
    ListDirectory,
    ListCommunities,
    SampleOutbox,
    RefreshEntry,
    RevalidateEntry,
    RefreshPresentation,
}

impl Operation {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InspectSource => "inspect_source",
            Self::ListPeers => "list_peers",
            Self::ListDirectory => "list_directory",
            Self::ListCommunities => "list_communities",
            Self::SampleOutbox => "sample_outbox",
            Self::RefreshEntry => "refresh_entry",
            Self::RevalidateEntry => "revalidate_entry",
            Self::RefreshPresentation => "refresh_presentation",
        }
    }
    fn mechanism(self) -> &'static str {
        match self {
            Self::InspectSource => "source_inspection",
            Self::ListPeers => "peer_exploration",
            Self::ListDirectory => "directory",
            Self::ListCommunities => "community_directory",
            Self::SampleOutbox => "outbox",
            Self::RefreshEntry | Self::RevalidateEntry | Self::RefreshPresentation => {
                "metadata_cache"
            }
        }
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Lease {
    pub search_run_id: Option<i64>,
    pub exploration: bool,
    pub id: i64,
    pub source_id: i64,
    pub target_uri: String,
    pub operation: String,
    pub cursor: Option<String>,
    pub attempts: i32,
    pub lease_token: i64,
    pub leased_until: OffsetDateTime,
    pub policy_revision: i64,
    pub source_revision: i64,
    pub entry_revision: Option<i64>,
}

pub async fn enqueue(
    pool: &PgPool,
    source: i64,
    target: &str,
    operation: Operation,
    introduced_by: Option<i64>,
) -> Result<Admission, DbError> {
    let mut tx = pool.begin().await?;
    let settings = lock_settings(&mut tx).await?;
    let result =
        enqueue_locked(&mut tx, &settings, source, target, operation, introduced_by).await?;
    tx.commit().await?;
    Ok(result)
}

pub(super) async fn enqueue_locked(
    tx: &mut Tx<'_>,
    settings: &Settings,
    source: i64,
    target: &str,
    operation: Operation,
    introduced_by: Option<i64>,
) -> Result<Admission, DbError> {
    let (target, origin) = address(target)?;
    if !settings.runs(operation.mechanism()) {
        return Ok(Admission::Disabled);
    }
    if !introducer_valid(tx, introduced_by).await? {
        return Ok(Admission::Suppressed);
    }
    let source_row = sqlx::query!(
        "SELECT origin,enabled,verified_at,provenance FROM discovery_sources WHERE id=$1",
        source
    )
    .fetch_optional(&mut **tx)
    .await?;
    let Some(source_row) = source_row else {
        return Err(invalid("unknown discovery source"));
    };
    if source_row.origin != origin {
        return Err(invalid("discovery target is outside its source origin"));
    }
    if !source_row.enabled {
        return Ok(Admission::Suppressed);
    }
    if operation == Operation::RefreshEntry && sqlx::query_scalar!(
        r#"SELECT state='withdrawn' AS "withdrawn!" FROM discovery_entries WHERE canonical_uri=$1"#,target)
        .fetch_optional(&mut **tx).await?.unwrap_or(false)
    { return Ok(Admission::Suppressed); }
    if operation == Operation::RevalidateEntry && !sqlx::query_scalar!(
        r#"SELECT EXISTS(SELECT 1 FROM discovery_entries WHERE canonical_uri=$1 AND source_id=$2)
        OR EXISTS(SELECT 1 FROM discovery_jobs WHERE target_uri=$1 AND source_id=$2 AND withdrawn) OR EXISTS(SELECT 1 FROM discovery_identity WHERE canonical_uri=$1 AND source_id=$2 AND outcome IN ('denied','withdrawn')) AS "known!""#, target, source
    ).fetch_one(&mut **tx).await? { return Ok(Admission::Suppressed); }
    let op = operation.as_str();
    if let Some(existing) = sqlx::query!(
        "SELECT id,withdrawn FROM discovery_jobs WHERE target_uri=$1 AND operation=$2",
        target,
        op
    )
    .fetch_optional(&mut **tx)
    .await?
    {
        return Ok(if existing.withdrawn {
            Admission::Suppressed
        } else {
            Admission::Existing(existing.id)
        });
    }
    let counts = sqlx::query!(
        r#"SELECT count(*) AS "total!",
        count(*) FILTER (WHERE origin=$1) AS "origin!",
        count(*) FILTER (WHERE introduced_by IS NOT DISTINCT FROM $2) AS "introducer!"
        FROM discovery_jobs"#,
        origin,
        introduced_by
    )
    .fetch_one(&mut **tx)
    .await?;
    if counts.total >= settings.max_jobs
        || (source_row.provenance == "peer" && counts.total >= (settings.max_jobs * 4 / 5).max(1))
        || counts.origin >= settings.jobs_per_origin
        || counts.introducer >= settings.jobs_per_introducer
    {
        return Ok(Admission::Capacity);
    }
    let exploration = operation == Operation::ListPeers
        || (operation == Operation::InspectSource && source_row.verified_at.is_none());
    let id = id::next();
    sqlx::query!("INSERT INTO discovery_jobs(id,source_id,target_uri,origin,introduced_by,operation,exploration) VALUES($1,$2,$3,$4,$5,$6,$7)",
        id,source,target,origin,introduced_by,op,exploration).execute(&mut **tx).await?;
    Ok(Admission::Added(id))
}

/// Claim a bounded batch, at most one live lease per origin across processes.
/// Expired leases retain cursors and attempts. A new token fences the old worker.
pub async fn claim_due(pool: &PgPool, worker: &str) -> Result<Vec<Lease>, DbError> {
    claim_due_for(
        pool,
        worker,
        &[
            Operation::InspectSource,
            Operation::ListPeers,
            Operation::ListDirectory,
            Operation::ListCommunities,
            Operation::SampleOutbox,
            Operation::RefreshEntry,
            Operation::RevalidateEntry,
            Operation::RefreshPresentation,
        ],
    )
    .await
}

/// Workers claim only operations with an installed adapter.
pub async fn claim_due_for(
    pool: &PgPool,
    worker: &str,
    operations: &[Operation],
) -> Result<Vec<Lease>, DbError> {
    let operations: Vec<String> = operations.iter().map(|op| op.as_str().to_owned()).collect();
    bounded(worker, 128)?;
    if worker.is_empty() {
        return Err(invalid("discovery worker name is empty"));
    }
    let mut tx = pool.begin().await?;
    let settings = lock_settings(&mut tx).await?;
    if !settings.enabled || settings.paused {
        return Ok(Vec::new());
    }
    sqlx::query!("UPDATE discovery_jobs SET lease_token=NULL,lease_owner=NULL,leased_until=NULL WHERE leased_until<=clock_timestamp()")
        .execute(&mut *tx).await?;
    sqlx::query!("DELETE FROM discovery_jobs WHERE NOT withdrawn AND (expires_at<=clock_timestamp() OR (lease_token IS NULL AND attempts >= $1))",settings.max_attempts)
        .execute(&mut *tx).await?;
    let active = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM discovery_jobs WHERE lease_token IS NOT NULL"#
    )
    .fetch_one(&mut *tx)
    .await?;
    let mut leases = Vec::new();
    for _ in active..i64::from(settings.max_inflight) {
        let explore =
            sqlx::query_scalar!(r#"SELECT turn=0 AS "explore!" FROM discovery_scheduler"#)
                .fetch_one(&mut *tx)
                .await?;
        let candidate = sqlx::query!(r#"WITH request_usage AS MATERIALIZED (
                SELECT family,origin,count(*) AS attempts FROM discovery_requests
                WHERE started_at>clock_timestamp()-interval '24 hours' AND purpose IN ('acquisition','maintenance') GROUP BY family,origin
            ), family_usage AS MATERIALIZED (
                SELECT family,sum(attempts)/discovery_cost(family) AS cost FROM request_usage GROUP BY family
            ), origin_usage AS MATERIALIZED (
                SELECT origin,sum(attempts) AS attempts FROM request_usage GROUP BY origin
            ), busy_usage AS MATERIALIZED (
                SELECT discovery_family(busy.operation,busy.target_uri,busy.cursor,bs.capabilities,be.kind) AS family,
                    count(*) AS jobs FROM discovery_jobs busy JOIN discovery_sources bs ON bs.id=busy.source_id
                    LEFT JOIN discovery_entries be ON be.canonical_uri=busy.target_uri
                WHERE busy.lease_token IS NOT NULL GROUP BY 1
            ) SELECT j.id,s.revision,
            (SELECT e.revision FROM discovery_entries e WHERE e.canonical_uri=j.target_uri) AS entry_revision
            FROM discovery_jobs j JOIN discovery_sources s ON s.id=j.source_id
            CROSS JOIN LATERAL (SELECT discovery_family(j.operation,j.target_uri,j.cursor,s.capabilities,
                (SELECT kind FROM discovery_entries WHERE canonical_uri=j.target_uri)) AS family) f
            LEFT JOIN family_usage fu ON fu.family=f.family
            LEFT JOIN origin_usage ou ON ou.origin=j.origin
            LEFT JOIN busy_usage bu ON bu.family=f.family
            WHERE j.search_run_id IS NULL AND j.operation=ANY($2) AND NOT j.withdrawn AND j.lease_token IS NULL AND j.run_at<=clock_timestamp() AND s.enabled
              AND (s.cooldown_until IS NULL OR s.cooldown_until<=clock_timestamp() OR (j.operation='list_peers' AND s.last_error IS DISTINCT FROM 'rate_limited'))
              AND (j.operation IN ('inspect_source','list_peers') OR s.verified_at IS NOT NULL)
              AND (j.operation<>'list_peers' OR NOT('source_inspection'=ANY($1)))
              AND NOT (CASE j.operation WHEN 'inspect_source' THEN 'source_inspection'
                WHEN 'list_peers' THEN 'peer_exploration' WHEN 'list_directory' THEN 'directory'
                WHEN 'list_communities' THEN 'community_directory' WHEN 'sample_outbox' THEN 'outbox' ELSE 'metadata_cache' END = ANY($1))
              AND (NOT (j.operation IN ('refresh_entry','revalidate_entry') AND
                (j.cursor LIKE 'http%' OR EXISTS(SELECT 1 FROM discovery_entries p WHERE p.canonical_uri=j.target_uri AND p.kind='post')))
                OR NOT($1 && ARRAY['outbox','public_post_indexing']::text[]))
              AND (NOT('community_directory'=ANY($1)) OR NOT(COALESCE(j.cursor='community',false) OR EXISTS(SELECT 1 FROM discovery_entries c WHERE c.canonical_uri=j.target_uri AND c.kind='community')))
              AND (NOT($1 && ARRAY['video_catalogue','video_indexing']::text[]) OR NOT(COALESCE(j.cursor='video',false) OR EXISTS(SELECT 1 FROM discovery_entries v WHERE v.canonical_uri=j.target_uri AND v.kind='video') OR EXISTS(SELECT 1 FROM discovery_video_work w WHERE w.job_id=j.id)))
              AND NOT EXISTS (SELECT 1 FROM discovery_jobs busy WHERE busy.origin=j.origin AND busy.lease_token IS NOT NULL)
              AND NOT EXISTS (SELECT 1 FROM discovery_entries e WHERE e.canonical_uri=j.target_uri
                AND e.state='withdrawn' AND j.operation='refresh_entry')
            ORDER BY (j.exploration=$3) DESC,
                ((SELECT turn=1 FROM discovery_scheduler) AND (j.operation='refresh_presentation' OR
                    (j.operation IN ('refresh_entry','revalidate_entry') AND EXISTS(SELECT 1 FROM discovery_entries e WHERE e.canonical_uri=j.target_uri)))) DESC,
                ((SELECT turn=1 FROM discovery_scheduler) AND (
                    (j.operation='refresh_presentation' AND NOT EXISTS(SELECT 1 FROM discovery_server_icons i WHERE i.source_id=j.source_id AND i.image IS NOT NULL)) OR
                    EXISTS(SELECT 1 FROM discovery_entries e WHERE e.canonical_uri=j.target_uri AND e.kind IN ('profile','community')
                        AND NOT EXISTS(SELECT 1 FROM discovery_actor_labels l WHERE l.uri=e.canonical_uri)))) DESC,
                ((SELECT turn<>1 FROM discovery_scheduler) AND j.operation IN ('refresh_entry','revalidate_entry') AND j.entry_revision IS NULL) DESC,
                COALESCE(fu.cost,0)+COALESCE(bu.jobs,0),COALESCE(ou.attempts,0),
                j.run_at,j.id LIMIT 1"#,&settings.disabled_mechanisms,&operations,explore)
            .fetch_optional(&mut *tx).await?;
        let Some(candidate) = candidate else {
            break;
        };
        let token = id::next();
        let lease = sqlx::query_as!(Lease,r#"UPDATE discovery_jobs SET lease_token=$2,lease_owner=$3,
            leased_until=clock_timestamp()+make_interval(secs=>$4),attempts=attempts+1,
            policy_revision=$5,source_revision=$6,entry_revision=$7
            WHERE id=$1 RETURNING id,source_id,target_uri,operation,cursor,attempts,exploration,search_run_id,
            lease_token AS "lease_token!",leased_until AS "leased_until!",
            policy_revision AS "policy_revision!",source_revision AS "source_revision!",entry_revision"#,
            candidate.id,token,worker,f64::from(settings.lease_seconds),settings.revision,
            candidate.revision,candidate.entry_revision).fetch_one(&mut *tx).await?;
        sqlx::query!(
            "UPDATE discovery_sources SET attempts_total=attempts_total+1 WHERE id=$1",
            lease.source_id
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query("SELECT discovery_count(origin,'job',CASE WHEN $2 THEN 'exploration' WHEN $3 THEN 'maintenance' ELSE 'acquisition' END,'claim') FROM discovery_sources WHERE id=$1")
            .bind(lease.source_id).bind(lease.exploration).bind(lease.operation=="refresh_presentation" || lease.entry_revision.is_some()).execute(&mut *tx).await?;
        leases.push(lease);
    }
    tx.commit().await?;
    Ok(leases)
}

pub(super) async fn valid_lease(
    tx: &mut Tx<'_>,
    settings: &Settings,
    lease: &Lease,
) -> Result<bool, DbError> {
    if !settings.enabled || settings.paused || settings.revision != lease.policy_revision {
        return Ok(false);
    }
    if let Some(run) = lease.search_run_id
        && !super::wider::search_lease_allowed(tx, lease, run).await?
    {
        return Ok(false);
    }
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS(
        SELECT 1 FROM discovery_jobs j JOIN discovery_sources s ON s.id=j.source_id
        WHERE NOT j.withdrawn AND j.id=$1 AND j.lease_token=$2 AND j.source_id=$3 AND j.target_uri=$4
          AND j.search_run_id IS NOT DISTINCT FROM $10 AND j.exploration=$9 AND j.operation=$5 AND j.policy_revision=$6 AND j.source_revision=$7
          AND j.entry_revision IS NOT DISTINCT FROM $8
          AND s.revision=j.source_revision AND s.enabled
          AND (s.cooldown_until IS NULL OR s.cooldown_until<=clock_timestamp() OR (j.operation='list_peers' AND s.last_error IS DISTINCT FROM 'rate_limited'))
          AND j.leased_until>clock_timestamp() AND j.expires_at>clock_timestamp()
          AND (j.operation IN ('inspect_source','list_peers') OR s.verified_at IS NOT NULL)
    ) AS "valid!""#,
        lease.id,
        lease.lease_token,
        lease.source_id,
        &lease.target_uri,
        &lease.operation,
        lease.policy_revision,
        lease.source_revision,
        lease.entry_revision,
        lease.exploration,
        lease.search_run_id
    )
    .fetch_one(&mut **tx)
    .await?)
}

pub(super) async fn delete_claim(tx: &mut Tx<'_>, lease: &Lease) -> Result<(), DbError> {
    sqlx::query!(
        "DELETE FROM discovery_jobs WHERE id=$1 AND lease_token=$2",
        lease.id,
        lease.lease_token
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn finish_job(pool: &PgPool, lease: &Lease) -> Result<Completion, DbError> {
    let mut tx = pool.begin().await?;
    let settings = lock_settings(&mut tx).await?;
    if !valid_lease(&mut tx, &settings, lease).await? {
        return Ok(Completion::Stale);
    }
    delete_claim(&mut tx, lease).await?;
    tx.commit().await?;
    Ok(Completion::Committed)
}

/// Persist a bounded continuation/backoff and release the lease. Duplicate hints
/// cannot shorten this delay or reset attempts; exhausted jobs are removed.
pub async fn retry_job(
    pool: &PgPool,
    lease: &Lease,
    cursor: Option<&str>,
    delay_seconds: i32,
    error: &str,
) -> Result<Completion, DbError> {
    if let Some(cursor) = cursor {
        bounded(cursor, 2048)?;
    }
    bounded(error, 128)?;
    if !(0..=604_800).contains(&delay_seconds) {
        return Err(invalid("invalid discovery retry delay"));
    }
    let mut tx = pool.begin().await?;
    let settings = lock_settings(&mut tx).await?;
    if !valid_lease(&mut tx, &settings, lease).await? {
        return Ok(Completion::Stale);
    }
    if lease.attempts >= settings.max_attempts {
        delete_claim(&mut tx, lease).await?;
    } else {
        sqlx::query!("UPDATE discovery_jobs SET cursor=$3,run_at=clock_timestamp()+make_interval(secs=>$4),last_error=$5,lease_token=NULL,lease_owner=NULL,leased_until=NULL WHERE id=$1 AND lease_token=$2",
            lease.id,lease.lease_token,cursor,f64::from(delay_seconds),error).execute(&mut *tx).await?;
    }
    sqlx::query!(
        "UPDATE discovery_sources SET last_error=$2 WHERE id=$1",
        lease.source_id,
        error
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Completion::Committed)
}

/// This is queue/storage maintenance, not network polling. It works while paused
/// or disabled and never removes catalogue withdrawal guards.
pub async fn prune_jobs(pool: &PgPool) -> Result<u64, DbError> {
    let mut tx = pool.begin().await?;
    let settings = lock_settings(&mut tx).await?;
    sqlx::query!("DELETE FROM discovery_search_runs WHERE expires_at<=clock_timestamp()")
        .execute(&mut *tx)
        .await?;
    let removed = sqlx::query!("DELETE FROM discovery_jobs WHERE NOT withdrawn AND (expires_at<=clock_timestamp() OR (attempts >= $1 AND (leased_until IS NULL OR leased_until<=clock_timestamp())))",settings.max_attempts)
        .execute(&mut *tx).await?.rows_affected();
    tx.commit().await?;
    Ok(removed)
}
