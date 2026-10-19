//! Atomic directory-page admission and maintenance of already admitted sources.
use super::{
    Admission, Completion, Entry, Lease, Operation, PgPool, Source, address, delete_claim, invalid,
    lock_settings, queue::enqueue_locked, valid_lease,
};
use crate::DbError;

pub struct ProfileHint<'a> {
    pub uri: &'a str,
    /// A directory's actor-URI field still needs its own canonical fetch.
    pub canonical: bool,
}

/// A successful bounded page proves only the directory capability. Candidate
/// addresses enter the ordinary quota/withdrawal gates in the same transaction;
/// no profile fields from the directory are retained.
pub async fn finish_directory_page(
    pool: &PgPool,
    lease: &Lease,
    hints: &[ProfileHint<'_>],
    next_page: bool,
) -> Result<Completion, DbError> {
    finish_listing_page(pool, lease, hints, next_page, false).await
}

pub async fn finish_community_page(
    pool: &PgPool,
    lease: &Lease,
    hints: &[ProfileHint<'_>],
    next_page: bool,
) -> Result<Completion, DbError> {
    finish_listing_page(pool, lease, hints, next_page, true).await
}

async fn finish_listing_page(
    pool: &PgPool,
    lease: &Lease,
    hints: &[ProfileHint<'_>],
    next_page: bool,
    communities: bool,
) -> Result<Completion, DbError> {
    let mechanism = if communities {
        "community_directory"
    } else {
        "directory"
    };
    let operation = if communities {
        Operation::ListCommunities
    } else {
        Operation::ListDirectory
    };
    if hints.len() > 20
        || !["inspect_source", operation.as_str()].contains(&lease.operation.as_str())
    {
        return Err(invalid("invalid discovery directory page"));
    }
    let mut tx = pool.begin().await?;
    let settings = lock_settings(&mut tx).await?;
    if !settings.runs(mechanism) || !valid_lease(&mut tx, &settings, lease).await? {
        return Ok(Completion::Stale);
    }
    let source = sqlx::query_as!(
        Source,
        "SELECT * FROM discovery_sources WHERE id=$1",
        lease.source_id
    )
    .fetch_one(&mut *tx)
    .await?;
    let mut addresses = Vec::new();
    for hint in hints {
        let (uri, origin) = address(hint.uri)?;
        if origin != source.origin {
            return Err(invalid("directory hint is not local to its source"));
        }
        addresses.push((uri, hint.canonical));
    }
    let mut canonical =
        url::Url::parse(&source.endpoint).map_err(|_| invalid("invalid endpoint"))?;
    if communities {
        canonical.set_path("/api/v3/community/list");
    }
    sqlx::query!("UPDATE discovery_sources SET canonical_uri=$3,capabilities=CASE WHEN 'peer_exploration'=ANY(capabilities) THEN ARRAY['source_inspection',$2,'peer_exploration'] ELSE ARRAY['source_inspection',$2] END,verified_at=clock_timestamp(),valid_until=clock_timestamp()+interval '24 hours',cooldown_until=NULL,last_success_at=clock_timestamp() WHERE id=$1",lease.source_id,mechanism,canonical.as_str()).execute(&mut *tx).await?;
    delete_claim(&mut tx, lease).await?;
    let mut novel = 0;
    for (uri, canonical) in addresses {
        // A repeated directory hint cannot force an early refresh of evidence
        // that was just verified, or override a retained refusal/withdrawal.
        if !super::recovery::new_candidate(&mut tx, &uri).await? {
            continue;
        }
        novel += 1;
        let admission = enqueue_locked(
            &mut tx,
            &settings,
            source.id,
            &uri,
            Operation::RefreshEntry,
            source.introduced_by,
        )
        .await?;
        if admission == Admission::Capacity {
            super::recovery::defer_candidate(
                &mut tx,
                source.id,
                &uri,
                if communities {
                    "community"
                } else if canonical {
                    "canonical"
                } else {
                    ""
                },
                None,
            )
            .await?;
        }
        if let Admission::Added(id) = admission
            && canonical
        {
            sqlx::query!(
                "UPDATE discovery_jobs SET cursor=$2 WHERE id=$1",
                id,
                if communities {
                    "community"
                } else {
                    "canonical"
                }
            )
            .execute(&mut *tx)
            .await?;
        }
    }
    super::sampling::advance_sampling(&mut tx, source.id, next_page, 20, novel).await?;
    tx.commit().await?;
    Ok(Completion::Committed)
}

/// An AP representation reached from a profile permalink only supplies a
/// canonical-address hint. Refetch that address in a distinct, fenced job.
/// A second identity hop is refused by the canonical cursor marker.
pub async fn finish_profile_alias(
    pool: &PgPool,
    lease: &Lease,
    canonical: &str,
) -> Result<Completion, DbError> {
    let (canonical, origin) = address(canonical)?;
    if lease.operation != "refresh_entry"
        || lease.cursor.is_some()
        || lease.entry_revision.is_some()
        || origin != address(&lease.target_uri)?.1
    {
        return Err(invalid("invalid canonical discovery handoff"));
    }
    let mut tx = pool.begin().await?;
    let settings = lock_settings(&mut tx).await?;
    if !valid_lease(&mut tx, &settings, lease).await? {
        return Ok(Completion::Stale);
    }
    let source = sqlx::query!(
        "SELECT introduced_by FROM discovery_sources WHERE id=$1",
        lease.source_id
    )
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO discovery_identity(canonical_uri,kind,source_id) VALUES($1,'profile',$2) ON CONFLICT DO NOTHING")
        .bind(&canonical).bind(lease.source_id).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO discovery_aliases(alias,canonical_uri) VALUES($1,$2) ON CONFLICT(alias) DO UPDATE SET canonical_uri=excluded.canonical_uri")
        .bind(&lease.target_uri).bind(&canonical).execute(&mut *tx).await?;
    delete_claim(&mut tx, lease).await?;
    if !super::recovery::new_candidate(&mut tx, &canonical).await? {
        tx.commit().await?;
        return Ok(Completion::Committed);
    }
    let admission = enqueue_locked(
        &mut tx,
        &settings,
        lease.source_id,
        &canonical,
        Operation::RefreshEntry,
        source.introduced_by,
    )
    .await?;
    if let Admission::Added(id) = admission {
        sqlx::query!(
            "UPDATE discovery_jobs SET cursor='canonical' WHERE id=$1",
            id
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(if admission == Admission::Capacity {
        Completion::Capacity
    } else {
        Completion::Committed
    })
}

/// Refresh existing evidence before expiry. Withdrawn identities are only
/// rechecked by an explicit operator request; new hints cannot resurrect them.
/// Runs while enabled/unpaused and admits at most 32 sources + 64 profiles.
pub async fn schedule_directory_maintenance(pool: &PgPool) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    let settings = lock_settings(&mut tx).await?;
    if !settings.enabled || settings.paused {
        return Ok(());
    }
    if settings.runs("source_inspection") {
        let sources=sqlx::query!("SELECT id,endpoint,introduced_by FROM discovery_sources WHERE enabled AND (CASE WHEN 'video_catalogue'=ANY(capabilities) OR endpoint LIKE '%/api/v1/videos' THEN $3 WHEN 'community_directory'=ANY(capabilities) OR endpoint LIKE '%/api/v3/community/list' THEN $2 ELSE $1 AND endpoint LIKE '%/api/v1/directory' END) AND (cooldown_until IS NULL OR cooldown_until<=clock_timestamp()) AND (verified_at IS NULL OR NOT EXISTS(SELECT 1 FROM discovery_sampling p WHERE p.source_id=discovery_sources.id) OR EXISTS(SELECT 1 FROM discovery_sampling p WHERE p.source_id=discovery_sources.id AND p.next_at<=clock_timestamp())) ORDER BY valid_until NULLS FIRST,id LIMIT 32",settings.runs("directory"),settings.runs("community_directory"),settings.runs("video_catalogue") && settings.runs("video_indexing")).fetch_all(&mut *tx).await?;
        for source in sources {
            enqueue_locked(
                &mut tx,
                &settings,
                source.id,
                &source.endpoint,
                Operation::InspectSource,
                source.introduced_by,
            )
            .await?;
        }
    }
    if settings.runs("metadata_cache") {
        let entries=sqlx::query!("SELECT e.canonical_uri,e.source_id,s.introduced_by FROM discovery_entries e JOIN discovery_sources s ON s.id=e.source_id WHERE e.kind IN ('profile','community','video') AND (e.kind<>'community' OR $1) AND (e.kind<>'video' OR $2) AND e.state<>'withdrawn' AND s.enabled AND s.verified_at IS NOT NULL AND e.valid_until>clock_timestamp() AND EXISTS(SELECT 1 FROM discovery_identity i WHERE i.canonical_uri=e.canonical_uri AND i.next_check_at<=clock_timestamp()) AND NOT EXISTS(SELECT 1 FROM discovery_jobs j WHERE j.target_uri=e.canonical_uri AND j.operation='refresh_entry') ORDER BY NOT EXISTS(SELECT 1 FROM discovery_actor_labels l WHERE l.uri=e.canonical_uri) DESC,e.valid_until,e.id LIMIT 64",settings.runs("community_directory"),settings.runs("video_catalogue") && settings.runs("video_indexing")).fetch_all(&mut *tx).await?;
        for entry in entries {
            if let Admission::Added(id) = enqueue_locked(
                &mut tx,
                &settings,
                entry.source_id,
                &entry.canonical_uri,
                Operation::RefreshEntry,
                entry.introduced_by,
            )
            .await?
            {
                sqlx::query!(
                    "UPDATE discovery_jobs SET cursor='canonical' WHERE id=$1",
                    id
                )
                .execute(&mut *tx)
                .await?;
            }
        }
    }
    tx.commit().await?;
    super::schedule_candidates(pool).await?;
    Ok(())
}

/// Current canonical addresses for the operator's bounded inspection command.
/// Server-side eligibility and current instance/account moderation still apply.
pub async fn profile_candidates(pool: &PgPool) -> Result<Vec<Entry>, DbError> {
    Ok(sqlx::query_as!(
        Entry,
        "SELECT * FROM discovery_entries WHERE kind='profile' ORDER BY id LIMIT 200"
    )
    .fetch_all(pool)
    .await?)
}

pub async fn request_profile_recheck(pool: &PgPool, uri: &str) -> Result<Admission, DbError> {
    let (uri, _) = address(uri)?;
    let mut tx = pool.begin().await?;
    let settings = lock_settings(&mut tx).await?;
    let known=sqlx::query!("SELECT s.id,s.introduced_by FROM discovery_sources s WHERE s.id IN (SELECT source_id FROM discovery_entries WHERE canonical_uri=$1 UNION SELECT source_id FROM discovery_jobs WHERE target_uri=$1 AND withdrawn UNION SELECT source_id FROM discovery_identity WHERE canonical_uri=$1 AND outcome IN ('denied','withdrawn')) ORDER BY s.id LIMIT 1",uri).fetch_optional(&mut *tx).await?;
    let Some(source) = known else {
        return Ok(Admission::Suppressed);
    };
    let result = enqueue_locked(
        &mut tx,
        &settings,
        source.id,
        &uri,
        Operation::RevalidateEntry,
        source.introduced_by,
    )
    .await?;
    tx.commit().await?;
    Ok(result)
}
