//! Outbox scheduling, canonical-author fencing and post erasure share the store lock.
use super::{
    Admission, Completion, Lease, Operation, PgPool, Settings, Tx, VerifiedEntry, address, bounded,
    delete_claim, invalid, lock_settings, queue::enqueue_locked, valid_lease,
};
use crate::DbError;

pub(super) async fn author_current(
    tx: &mut Tx<'_>,
    settings: &Settings,
    source: i64,
    facts: &VerifiedEntry<'_>,
) -> Result<bool, DbError> {
    if !settings.runs("outbox") || !settings.runs("public_post_indexing") {
        return Ok(false);
    }
    let Some(revision) = facts.author_revision else {
        return Ok(false);
    };
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS(SELECT 1 FROM discovery_entries
        WHERE canonical_uri=$1 AND source_id=$2 AND revision=$3 AND kind='profile'
          AND (state='active' OR (state='denied' AND invalidation_reason='ineligible'))
          AND visibility='public' AND verified_at<=clock_timestamp() AND verified_at<=$4
          AND (indexable='allow' OR (indexable='missing' AND $5))) AS "current!""#,
        facts.actor_uri,
        source,
        revision,
        facts.valid_until,
        settings.allow_missing_indexable
    )
    .fetch_one(&mut **tx)
    .await?)
}

pub(super) async fn invalidate_author_locked(
    tx: &mut Tx<'_>,
    actor: &str,
    reason: &str,
) -> Result<(), DbError> {
    sqlx::query!("UPDATE discovery_entries SET state=CASE WHEN state='withdrawn' THEN state ELSE 'denied' END,
        revision=revision+1,title=NULL,summary=NULL,language=NULL,published_at=NULL,sensitive=false,
        invalidation_reason=$2,updated_at=clock_timestamp() WHERE kind='post' AND actor_uri=$1",actor,reason)
        .execute(&mut **tx).await?;
    // Pending canonical post checks keep the authoritative actor URI as their
    // cursor. Cancellation plus the actor revision fences work already in flight.
    sqlx::query!(
        "DELETE FROM discovery_jobs WHERE NOT withdrawn AND
        ((operation='sample_outbox' AND target_uri=$1) OR cursor=$1 OR target_uri IN
         (SELECT canonical_uri FROM discovery_entries WHERE kind='post' AND actor_uri=$1))",
        actor
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Authenticated negative actor evidence applies independently to each use.
/// A profile opt-out alone does not revoke public-post indexing permission.
pub async fn revoke_actor(
    pool: &PgPool,
    actor: &str,
    discoverable: &str,
    indexable: &str,
    public: bool,
) -> Result<(), DbError> {
    let (actor, _) = address(actor)?;
    let profile_refused = !public || ["deny", "invalid"].contains(&discoverable);
    let posts_refused = !public || ["deny", "invalid"].contains(&indexable);
    if !profile_refused && !posts_refused {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    lock_settings(&mut tx).await?;
    super::videos::invalidate_video_actor_locked(&mut tx, &actor, false, "actor_policy_changed")
        .await?;
    sqlx::query!("UPDATE discovery_entries SET revision=revision+1,
        discoverable=CASE WHEN $2 IN ('deny','invalid') THEN $2 ELSE discoverable END,
        indexable=CASE WHEN $3 IN ('deny','invalid') THEN $3 ELSE indexable END,
        visibility=CASE WHEN $4 THEN visibility ELSE 'unknown' END,
        state=CASE WHEN state='withdrawn' THEN state WHEN $5 THEN 'denied' ELSE state END,
        title=CASE WHEN $5 THEN NULL ELSE title END,summary=CASE WHEN $5 THEN NULL ELSE summary END,
        language=CASE WHEN $5 THEN NULL ELSE language END,
        outbox_uri=CASE WHEN $6 THEN NULL ELSE outbox_uri END,outbox_next_at=CASE WHEN $6 THEN NULL ELSE outbox_next_at END,
        invalidation_reason=CASE WHEN $5 AND state<>'withdrawn' THEN 'ineligible' ELSE invalidation_reason END,
        updated_at=clock_timestamp() WHERE canonical_uri=$1 AND kind IN ('profile','community')",
        actor,discoverable,indexable,public,profile_refused,posts_refused).execute(&mut *tx).await?;
    if posts_refused {
        invalidate_author_locked(&mut tx, &actor, "author_indexing_refused").await?;
    }
    // A first fetch can be in flight before the actor's catalogue row exists.
    sqlx::query!("DELETE FROM discovery_jobs WHERE target_uri=$1 AND NOT withdrawn AND operation IN ('refresh_entry','revalidate_entry')",actor).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// At most eight author rounds are admitted per pass, once per 24 hours. The
/// existing queue/source/request/byte ceilings also cover these rounds.
pub async fn schedule_outboxes(pool: &PgPool) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    let settings = lock_settings(&mut tx).await?;
    if !settings.runs("outbox")
        || !settings.runs("metadata_cache")
        || !settings.runs("public_post_indexing")
    {
        return Ok(());
    }
    let actors=sqlx::query!("SELECT e.id,e.source_id,e.canonical_uri,s.introduced_by FROM discovery_entries e
        JOIN discovery_sources s ON s.id=e.source_id WHERE e.kind='profile' AND e.outbox_uri IS NOT NULL
        AND e.outbox_next_at<=clock_timestamp() AND e.visibility='public'
        AND (e.state='active' OR (e.state='denied' AND e.invalidation_reason='ineligible'))
        AND e.valid_until>clock_timestamp() AND s.enabled AND s.verified_at IS NOT NULL
        AND (e.indexable='allow' OR (e.indexable='missing' AND $1)) ORDER BY e.outbox_next_at,e.id LIMIT 8",settings.allow_missing_indexable)
        .fetch_all(&mut *tx).await?;
    for actor in actors {
        if matches!(
            enqueue_locked(
                &mut tx,
                &settings,
                actor.source_id,
                &actor.canonical_uri,
                Operation::SampleOutbox,
                actor.introduced_by
            )
            .await?,
            Admission::Added(_) | Admission::Existing(_)
        ) {
            sqlx::query!("UPDATE discovery_entries SET outbox_next_at=clock_timestamp()+interval '24 hours' WHERE id=$1",actor.id).execute(&mut *tx).await?;
        }
    }
    // Posts are checked on authenticated change evidence or explicit resolution,
    // not repeatedly reread simply because another day passed.
    tx.commit().await?;
    Ok(())
}

async fn enqueue_post(
    tx: &mut Tx<'_>,
    settings: &Settings,
    source: i64,
    uri: &str,
    actor: &str,
    introducer: Option<i64>,
) -> Result<(), DbError> {
    let admission = enqueue_locked(
        tx,
        settings,
        source,
        uri,
        Operation::RefreshEntry,
        introducer,
    )
    .await?;
    if admission == Admission::Capacity {
        super::recovery::defer_candidate(tx, source, uri, actor, None).await?;
    }
    if let Admission::Added(id) = admission {
        sqlx::query!(
            "UPDATE discovery_jobs SET cursor=$2,exploration=false WHERE id=$1",
            id,
            actor
        )
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// URI hints from an outbox carry no trusted text. Only four distinct targets
/// from a round are queued, and all need a separate canonical object fetch.
pub async fn finish_outbox(
    pool: &PgPool,
    lease: &Lease,
    hints: &[String],
) -> Result<Completion, DbError> {
    if lease.operation != "sample_outbox" || hints.len() > 32 {
        return Err(invalid("invalid outbox sample"));
    }
    let mut tx = pool.begin().await?;
    let settings = lock_settings(&mut tx).await?;
    if !settings.runs("outbox")
        || !settings.runs("public_post_indexing")
        || !valid_lease(&mut tx, &settings, lease).await?
    {
        return Ok(Completion::Stale);
    }
    let actor = sqlx::query!(
        "SELECT e.revision,e.outbox_uri,e.indexable,e.visibility,e.valid_until,s.introduced_by
        FROM discovery_entries e JOIN discovery_sources s ON s.id=e.source_id
        WHERE e.canonical_uri=$1 AND e.source_id=$2 AND e.kind='profile'",
        lease.target_uri,
        lease.source_id
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(actor) = actor else {
        return Ok(Completion::Stale);
    };
    if Some(actor.revision) != lease.entry_revision
        || actor.outbox_uri.is_none()
        || actor.visibility != "public"
        || actor.valid_until <= time::OffsetDateTime::now_utc()
        || !super::catalogue::permitted(&actor.indexable, settings.allow_missing_indexable)
    {
        return Ok(Completion::Stale);
    }
    let origin = address(&lease.target_uri)?.1;
    let mut novel = 0;
    for hint in hints {
        if address(hint)?.1 != origin || hint == &lease.target_uri {
            return Err(invalid("post hint left its author origin"));
        }
        if super::recovery::new_candidate(&mut tx, hint).await? {
            novel += 1;
            enqueue_post(
                &mut tx,
                &settings,
                lease.source_id,
                hint,
                &lease.target_uri,
                actor.introduced_by,
            )
            .await?;
        }
    }
    sqlx::query("UPDATE discovery_outbox_walk SET zero_yield=CASE WHEN $2=0 THEN LEAST(zero_yield+1,7) ELSE 0 END WHERE actor_uri=$1")
        .bind(&lease.target_uri).bind(novel).execute(&mut *tx).await?;
    sqlx::query("UPDATE discovery_entries SET outbox_next_at=now()+COALESCE((SELECT CASE WHEN zero_yield>=2 THEN LEAST(interval '7 days',interval '12 hours'*zero_yield) ELSE interval '12 hours' END FROM discovery_outbox_walk WHERE actor_uri=$1),interval '24 hours') WHERE canonical_uri=$1")
        .bind(&lease.target_uri).execute(&mut *tx).await?;
    sqlx::query!(
        "UPDATE discovery_entries SET outbox_round=outbox_round+1 WHERE canonical_uri=$1",
        lease.target_uri
    )
    .execute(&mut *tx)
    .await?;
    delete_claim(&mut tx, lease).await?;
    tx.commit().await?;
    Ok(Completion::Committed)
}

/// An authenticated author's Update clears old text pending canonical refresh;
/// Delete retains a withdrawal guard. Foreign actors cannot invalidate a post.
pub async fn invalidate_owned_post(
    pool: &PgPool,
    actor: &str,
    uri: &str,
    withdrawn: bool,
    reason: &str,
) -> Result<bool, DbError> {
    bounded(reason, 128)?;
    let (actor, origin) = address(actor)?;
    let (uri, post_origin) = address(uri)?;
    if origin != post_origin || uri == actor {
        return Ok(false);
    }
    let known=sqlx::query_scalar!(r#"SELECT EXISTS(SELECT 1 FROM discovery_entries WHERE canonical_uri=$1 AND actor_uri=$2 AND kind='post')
        OR EXISTS(SELECT 1 FROM discovery_jobs WHERE target_uri=$1 AND cursor=$2) AS "known!""#,uri,actor).fetch_one(pool).await?;
    if !known {
        return Ok(false);
    }
    let mut tx = pool.begin().await?;
    lock_settings(&mut tx).await?;
    let changed=sqlx::query!("UPDATE discovery_entries SET state=CASE WHEN state='withdrawn' OR $3 THEN 'withdrawn' ELSE 'denied' END,
        revision=revision+1,title=NULL,summary=NULL,language=NULL,published_at=NULL,sensitive=false,
        invalidation_reason=$4,updated_at=clock_timestamp()
        WHERE canonical_uri=$1 AND actor_uri=$2 AND kind='post'",uri,actor,withdrawn,reason).execute(&mut *tx).await?.rows_affected();
    if withdrawn {
        sqlx::query!("UPDATE discovery_jobs SET withdrawn=true,search_run_id=NULL,cursor=NULL,lease_token=NULL,lease_owner=NULL,leased_until=NULL,last_error=$3
            WHERE target_uri=$1 AND cursor=$2 AND operation='refresh_entry'",uri,actor,reason).execute(&mut *tx).await?;
    } else {
        sqlx::query!(
            "DELETE FROM discovery_jobs WHERE target_uri=$1 AND cursor=$2 AND NOT withdrawn",
            uri,
            actor
        )
        .execute(&mut *tx)
        .await?;
    }
    if changed > 0 {
        sqlx::query!(
            "DELETE FROM discovery_jobs WHERE target_uri=$1 AND operation='revalidate_entry'",
            uri
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(changed > 0)
}
