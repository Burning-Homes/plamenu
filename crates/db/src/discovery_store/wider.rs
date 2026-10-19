//! Interactive search leases use the ordinary budgets and catalogue fences.
//! They carry no query text and are never claimed by the background worker.
use super::{Lease, PgPool, Tx, address, invalid, lock_settings};
use crate::{DbError, id};
use time::OffsetDateTime;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SearchRun {
    pub id: i64,
    pub account_id: i64,
    pub query_digest: String,
    pub kind: String,
    pub source_ids: Vec<i64>,
    pub created_at: OffsetDateTime,
    pub deadline: OffsetDateTime,
    pub expires_at: OffsetDateTime,
    pub requests: i32,
    pub finished: bool,
    pub results: serde_json::Value,
}

pub enum SearchStart {
    Started(SearchRun),
    Cached(SearchRun),
    Busy,
    Disabled,
}

pub async fn start_search(
    pool: &PgPool,
    viewer: i64,
    digest: &str,
    kind: &str,
    sources: &[i64],
) -> Result<SearchStart, DbError> {
    if digest.len() != 64
        || !digest.bytes().all(|b| b.is_ascii_hexdigit())
        || !["profile", "community"].contains(&kind)
        || sources.is_empty()
        || sources.len() > 3
    {
        return Err(invalid("invalid wider search"));
    }
    let mut tx = pool.begin().await?;
    let settings = lock_settings(&mut tx).await?;
    if !settings.runs("wider_search")
        || !settings.runs("metadata_cache")
        || !settings.runs("profile_listing")
    {
        return Ok(SearchStart::Disabled);
    }
    sqlx::query!("DELETE FROM discovery_search_runs WHERE expires_at<=clock_timestamp()")
        .execute(&mut *tx)
        .await?;
    let old = sqlx::query_as!(
        SearchRun,
        "SELECT * FROM discovery_search_runs WHERE account_id=$1",
        viewer
    )
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(old) = old {
        if old.query_digest == digest
            && old.kind == kind
            && old.source_ids == sources
            && old.finished
        {
            return Ok(SearchStart::Cached(old));
        }
        if old.created_at > OffsetDateTime::now_utc() - time::Duration::seconds(30) {
            return Ok(SearchStart::Busy);
        }
        sqlx::query!("DELETE FROM discovery_search_runs WHERE id=$1", old.id)
            .execute(&mut *tx)
            .await?;
    }
    let run=sqlx::query_as!(SearchRun,"INSERT INTO discovery_search_runs(id,account_id,query_digest,kind,source_ids) VALUES($1,$2,$3,$4,$5) RETURNING *",id::next(),viewer,digest,kind,sources).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(SearchStart::Started(run))
}

pub async fn search_run(
    pool: &PgPool,
    viewer: i64,
    id: i64,
    digest: &str,
) -> Result<Option<SearchRun>, DbError> {
    Ok(sqlx::query_as!(SearchRun,"SELECT * FROM discovery_search_runs WHERE id=$1 AND account_id=$2 AND query_digest=$3 AND expires_at>clock_timestamp()",id,viewer,digest).fetch_optional(pool).await?)
}

pub async fn finish_search(
    pool: &PgPool,
    id: i64,
    results: &serde_json::Value,
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    lock_settings(&mut tx).await?;
    sqlx::query!("DELETE FROM discovery_jobs WHERE search_run_id=$1", id)
        .execute(&mut *tx)
        .await?;
    sqlx::query!(
        "UPDATE discovery_search_runs SET finished=true,results=$2 WHERE id=$1",
        id,
        results
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn search_source_ready(pool: &PgPool, source: i64, kind: &str) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar!(r#"SELECT NOT EXISTS(SELECT 1 FROM discovery_search_sources WHERE source_id=$1 AND kind=$2 AND retry_at>clock_timestamp()) AS "ready!""#,source,kind).fetch_one(pool).await?)
}

pub async fn search_source_result(
    pool: &PgPool,
    source: i64,
    kind: &str,
    outcome: &str,
    seconds: i32,
) -> Result<(), DbError> {
    if !(0..=86400).contains(&seconds) {
        return Err(invalid("invalid search cooldown"));
    }
    sqlx::query!("INSERT INTO discovery_search_sources(source_id,kind,outcome,retry_at) VALUES($1,$2,$3,clock_timestamp()+make_interval(secs=>$4)) ON CONFLICT(source_id,kind) DO UPDATE SET outcome=excluded.outcome,retry_at=excluded.retry_at,checked_at=clock_timestamp()",source,kind,outcome,f64::from(seconds)).execute(pool).await?;
    Ok(())
}

/// One source request, or one canonical verification, inside the caller's
/// original deadline. Existing background work and withdrawal guards win.
pub async fn search_lease(
    pool: &PgPool,
    run: &SearchRun,
    source: i64,
    target: &str,
    canonical: bool,
) -> Result<Option<Lease>, DbError> {
    let (target, origin) = address(target)?;
    let mut tx = pool.begin().await?;
    let settings = lock_settings(&mut tx).await?;
    if !settings.runs("wider_search")
        || !settings.runs("metadata_cache")
        || !settings.runs("profile_listing")
    {
        return Ok(None);
    }
    let Some(source_row)=sqlx::query!("SELECT revision,origin,enabled,valid_until,capabilities,cooldown_until,introduced_by FROM discovery_sources WHERE id=$1",source).fetch_optional(&mut *tx).await? else {return Ok(None);};
    let now = OffsetDateTime::now_utc();
    let capability = if run.kind == "community" {
        "community_directory"
    } else {
        "directory"
    };
    if !run.source_ids.contains(&source)
        || source_row.origin != origin
        || !source_row.enabled
        || source_row.valid_until.is_none_or(|v| v <= now)
        || source_row.cooldown_until.is_some_and(|v| v > now)
        || !source_row.capabilities.iter().any(|v| v == capability)
    {
        return Ok(None);
    }
    let room=sqlx::query_scalar!(r#"SELECT
        (SELECT count(*) FROM discovery_jobs)<$2
        AND (SELECT count(*) FROM discovery_jobs WHERE origin=$1)<$3
        AND (SELECT count(*) FROM discovery_jobs WHERE introduced_by IS NOT DISTINCT FROM $5)<$6
        AND (SELECT count(*) FROM discovery_jobs WHERE lease_token IS NOT NULL AND leased_until>clock_timestamp())<$4
        AND NOT EXISTS(SELECT 1 FROM discovery_jobs WHERE origin=$1 AND lease_token IS NOT NULL AND leased_until>clock_timestamp()) AS "room!""#,
        origin,settings.max_jobs,settings.jobs_per_origin,i64::from(settings.max_inflight),source_row.introduced_by,settings.jobs_per_introducer).fetch_one(&mut *tx).await?;
    if !room || sqlx::query_scalar!(r#"SELECT EXISTS(SELECT 1 FROM discovery_jobs WHERE target_uri=$1)
        OR EXISTS(SELECT 1 FROM discovery_entries WHERE canonical_uri=$1 AND state='withdrawn') AS "busy!""#,target).fetch_one(&mut *tx).await? {return Ok(None);}
    let revision = sqlx::query_scalar!(
        "SELECT revision FROM discovery_entries WHERE canonical_uri=$1",
        target
    )
    .fetch_optional(&mut *tx)
    .await?;
    let op = if canonical {
        "refresh_entry"
    } else {
        "search_source"
    };
    let cursor = if canonical {
        Some(if run.kind == "community" {
            "community"
        } else {
            "canonical"
        })
    } else {
        None
    };
    let lease=sqlx::query_as!(Lease,r#"INSERT INTO discovery_jobs(id,source_id,target_uri,origin,introduced_by,operation,cursor,search_run_id,
        expires_at,lease_token,lease_owner,leased_until,policy_revision,source_revision,entry_revision,attempts,exploration)
        SELECT $1,$2,$3,$4,$5,$6,$7,r.id,r.deadline,$8,'interactive-search',r.deadline,$9,$10,$11,1,false
        FROM discovery_search_runs r WHERE r.id=$12 AND NOT r.finished AND r.deadline>clock_timestamp()
        RETURNING id,source_id,target_uri,operation,cursor,attempts,exploration,search_run_id,
        lease_token AS "lease_token!",leased_until AS "leased_until!",policy_revision AS "policy_revision!",
        source_revision AS "source_revision!",entry_revision"#,id::next(),source,target,origin,source_row.introduced_by,op,cursor,id::next(),settings.revision,source_row.revision,revision,run.id).fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    Ok(lease)
}

pub async fn close_search_lease(pool: &PgPool, lease: &Lease) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    lock_settings(&mut tx).await?;
    super::delete_claim(&mut tx, lease).await?;
    tx.commit().await?;
    Ok(())
}

pub(super) async fn search_lease_allowed(
    tx: &mut Tx<'_>,
    lease: &Lease,
    run: i64,
) -> Result<bool, DbError> {
    let host = url::Url::parse(&lease.target_uri)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_default();
    Ok(sqlx::query_scalar!(r#"SELECT EXISTS(SELECT 1 FROM discovery_search_runs r
        WHERE r.id=$1 AND NOT r.finished AND r.deadline>clock_timestamp() AND $2=ANY(r.source_ids)
        AND NOT EXISTS(SELECT 1 FROM account_domain_blocks b WHERE b.account_id=r.account_id AND b.domain=$3)
        AND NOT EXISTS(SELECT 1 FROM accounts a WHERE a.uri=$4 AND (a.id=r.account_id OR account_hidden(r.account_id,a.id)
            OR EXISTS(SELECT 1 FROM follow_recommendation_mutes m WHERE m.account_id=r.account_id AND m.target_account_id=a.id)))
        AND NOT EXISTS(SELECT 1 FROM discovery_entries e JOIN discovery_dismissals d ON d.entry_id=e.id
            WHERE e.canonical_uri=$4 AND d.account_id=r.account_id)) AS "allowed!""#,run,lease.source_id,host,lease.target_uri).fetch_one(&mut **tx).await?)
}

pub(super) async fn charge_search(tx: &mut Tx<'_>, run: i64) -> Result<bool, DbError> {
    Ok(sqlx::query!("UPDATE discovery_search_runs SET requests=requests+1 WHERE id=$1 AND requests<24 AND NOT finished AND deadline>clock_timestamp()",run).execute(&mut **tx).await?.rows_affected()==1)
}
