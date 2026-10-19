//! Bounded source expansion and rotation. Negative evidence is never evicted.
use super::{
    Admission, Completion, Lease, Operation, PgPool, Settings, Tx, delete_claim, invalid,
    lock_settings, queue::enqueue_locked, sources::add_source_locked, valid_lease,
};
use crate::DbError;

pub(super) async fn rotate_entry(
    tx: &mut Tx<'_>,
    source: i64,
    origin_full: bool,
    introducer_full: bool,
) -> Result<bool, DbError> {
    let victim = sqlx::query!(
        r#"SELECT e.id,e.canonical_uri FROM discovery_entries e
        JOIN discovery_sources s ON s.id=e.source_id CROSS JOIN discovery_sources target
        WHERE target.id=$1 AND e.state='active'
          AND NOT EXISTS(SELECT 1 FROM discovery_entries dependent WHERE dependent.kind='post' AND dependent.state='active' AND dependent.actor_uri=e.canonical_uri)
          AND e.valid_until<=clock_timestamp() AND e.kind IN ('post','video')
          AND (NOT $2 OR s.origin=target.origin)
          AND (NOT $3 OR s.introduced_by IS NOT DISTINCT FROM target.introduced_by)
          AND NOT EXISTS(SELECT 1 FROM discovery_dismissals d WHERE d.entry_id=e.id)
          AND NOT EXISTS(SELECT 1 FROM discovery_jobs j WHERE (j.target_uri=e.canonical_uri OR j.cursor=e.canonical_uri)
            AND (j.withdrawn OR j.leased_until>clock_timestamp()))
        ORDER BY e.verified_at,e.id LIMIT 1"#,
        source,
        origin_full,
        introducer_full
    )
    .fetch_optional(&mut **tx)
    .await?;
    let Some(victim) = victim else {
        return Ok(false);
    };
    sqlx::query!(
        "DELETE FROM discovery_jobs WHERE target_uri=$1 AND NOT withdrawn",
        victim.canonical_uri
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query!("DELETE FROM discovery_entries WHERE id=$1", victim.id)
        .execute(&mut **tx)
        .await?;
    Ok(true)
}

/// Probe at most eight sources per scheduling pass and two hops from an
/// operator root. The next round and list window survive process restarts.
pub async fn schedule_peer_exploration(pool: &PgPool) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    let settings = lock_settings(&mut tx).await?;
    if !settings.runs("peer_exploration") || !settings.runs("source_inspection") {
        return Ok(());
    }
    let sources = sqlx::query_as::<_, (i64, String, Option<i64>)>(
        "SELECT id,origin,introduced_by FROM discovery_sources
        WHERE enabled AND depth<2 AND peer_next_at<=clock_timestamp()
          AND verified_at IS NOT NULL AND valid_until>clock_timestamp()
          AND ('directory'=ANY(capabilities) OR 'peer_exploration'=ANY(capabilities))
          AND (last_error IS DISTINCT FROM 'rate_limited' OR cooldown_until<=clock_timestamp())
        ORDER BY peer_next_at,id LIMIT 8",
    )
    .fetch_all(&mut *tx)
    .await?;
    for (source_id, origin, introduced_by) in sources {
        let endpoint = format!("{origin}/api/v1/instance/peers");
        if matches!(
            enqueue_locked(
                &mut tx,
                &settings,
                source_id,
                &endpoint,
                Operation::ListPeers,
                introduced_by
            )
            .await?,
            Admission::Added(_) | Admission::Existing(_)
        ) {
            sqlx::query!("UPDATE discovery_sources SET peer_next_at=clock_timestamp()+interval '24 hours' WHERE id=$1",source_id)
                .execute(&mut *tx).await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

/// A peer list proves only its own capability. Every address gets independent
/// source admission and subsequent canonical verification; no imported trust.
pub async fn finish_peer_page(
    pool: &PgPool,
    lease: &Lease,
    endpoints: &[String],
    next_offset: i32,
) -> Result<Completion, DbError> {
    if lease.operation != "list_peers"
        || endpoints.len() > 8
        || !(0..=262_144).contains(&next_offset)
    {
        return Err(invalid("invalid discovery peer page"));
    }
    let mut tx = pool.begin().await?;
    let settings = lock_settings(&mut tx).await?;
    if !settings.runs("source_inspection") || !valid_lease(&mut tx, &settings, lease).await? {
        return Ok(Completion::Stale);
    }
    delete_claim(&mut tx, lease).await?;
    sqlx::query!("UPDATE discovery_sources SET peers_verified_at=clock_timestamp(),peer_error=NULL,
        peer_offset=$2,peer_next_at=clock_timestamp()+interval '24 hours',
        capabilities=CASE WHEN 'peer_exploration'=ANY(capabilities) THEN capabilities ELSE array_append(capabilities,'peer_exploration') END
        WHERE id=$1",lease.source_id,next_offset).execute(&mut *tx).await?;
    for endpoint in endpoints {
        let (endpoint, _) = super::address(endpoint)?;
        sqlx::query("INSERT INTO discovery_source_hints(endpoint,introduced_by)
            SELECT $1,$2 WHERE (SELECT count(*) FROM discovery_source_hints)<1000
            AND NOT EXISTS(SELECT 1 FROM discovery_sources WHERE endpoint=$1) ON CONFLICT DO NOTHING")
            .bind(endpoint).bind(lease.source_id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(Completion::Committed)
}

pub async fn fail_peer_page(
    pool: &PgPool,
    lease: &Lease,
    reason: &str,
) -> Result<Completion, DbError> {
    super::bounded(reason, 128)?;
    let mut tx = pool.begin().await?;
    let settings = lock_settings(&mut tx).await?;
    if lease.operation != "list_peers" || !valid_lease(&mut tx, &settings, lease).await? {
        return Ok(Completion::Stale);
    }
    delete_claim(&mut tx, lease).await?;
    sqlx::query!("UPDATE discovery_sources SET peer_error=$2,peer_next_at=clock_timestamp()+interval '24 hours' WHERE id=$1",
        lease.source_id,reason).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Completion::Committed)
}

#[derive(Default, Debug, serde::Serialize)]
pub struct ImportResult {
    pub admitted: usize,
    pub existing: usize,
    pub refused: usize,
}

pub async fn import_sources(
    pool: &PgPool,
    endpoints: &[String],
    provenance: &str,
    replace_bundled: bool,
) -> Result<ImportResult, DbError> {
    if endpoints.len() > 100 || !["bundled", "import"].contains(&provenance) {
        return Err(invalid("invalid discovery source import"));
    }
    let mut tx = pool.begin().await?;
    let settings = lock_settings(&mut tx).await?;
    let mut result = ImportResult::default();
    if !settings.runs("source_inspection") {
        result.refused = endpoints.len();
        return Ok(result);
    }
    if replace_bundled {
        sqlx::query!(
            "UPDATE discovery_sources SET enabled=false,revision=revision+1
            WHERE provenance='bundled' AND enabled AND NOT(endpoint=ANY($1))",
            endpoints
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "UPDATE discovery_jobs SET lease_token=NULL,lease_owner=NULL,leased_until=NULL
            WHERE source_id IN (SELECT id FROM discovery_sources WHERE NOT enabled)"
        )
        .execute(&mut *tx)
        .await?;
    }
    for endpoint in endpoints {
        let admitted = add_source_locked(&mut tx, &settings, endpoint, None, provenance).await?;
        match admitted {
            Admission::Added(id) => {
                enqueue_locked(
                    &mut tx,
                    &settings,
                    id,
                    endpoint,
                    Operation::InspectSource,
                    None,
                )
                .await?;
                result.admitted += 1;
            }
            Admission::Existing(_) => result.existing += 1,
            _ => result.refused += 1,
        }
    }
    tx.commit().await?;
    Ok(result)
}

/// An explicit operator choice can sponsor an already discovered peer as a
/// root. It changes provenance, not verified facts, refusal guards or backoff.
pub(super) async fn adopt_source(
    tx: &mut Tx<'_>,
    settings: &Settings,
    id: i64,
    provenance: &str,
) -> Result<Admission, DbError> {
    let fits = sqlx::query_scalar!(
        r#"SELECT
        (SELECT count(*) FROM discovery_sources WHERE introduced_by IS NULL)<$2
        AND (SELECT count(*) FROM discovery_jobs WHERE introduced_by IS NULL OR source_id=$1)<=$3
        AND (SELECT count(*) FROM discovery_entries e JOIN discovery_sources s ON s.id=e.source_id
            WHERE s.introduced_by IS NULL OR s.id=$1)<=$4 AS "fits!""#,
        id,
        settings.sources_per_introducer,
        settings.jobs_per_introducer,
        settings.entries_per_introducer
    )
    .fetch_one(&mut **tx)
    .await?;
    if !fits {
        return Ok(Admission::Capacity);
    }
    sqlx::query!("UPDATE discovery_sources SET introduced_by=NULL,depth=0,provenance=$2,revision=revision+1 WHERE id=$1",id,provenance)
        .execute(&mut **tx).await?;
    sqlx::query!("UPDATE discovery_jobs SET introduced_by=NULL,lease_token=NULL,lease_owner=NULL,leased_until=NULL WHERE source_id=$1",id)
        .execute(&mut **tx).await?;
    Ok(Admission::Existing(id))
}

pub(super) async fn entry_room(
    tx: &mut Tx<'_>,
    settings: &Settings,
    source: i64,
) -> Result<bool, DbError> {
    let counts = sqlx::query!(r#"SELECT count(*) AS "total!",
            count(*) FILTER (WHERE s.origin=target.origin) AS "origin!",
            count(*) FILTER (WHERE s.introduced_by IS NOT DISTINCT FROM target.introduced_by) AS "introducer!"
            FROM discovery_entries e JOIN discovery_sources s ON s.id=e.source_id
            CROSS JOIN discovery_sources target WHERE target.id=$1"#,source)
            .fetch_one(&mut **tx).await?;
    if counts.total < settings.max_entries
        && counts.origin < settings.entries_per_origin
        && counts.introducer < settings.entries_per_introducer
    {
        return Ok(true);
    }
    rotate_entry(
        tx,
        source,
        counts.origin >= settings.entries_per_origin,
        counts.introducer >= settings.entries_per_introducer,
    )
    .await
}
