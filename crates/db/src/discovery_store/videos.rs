//! Video evidence and pending actor dependencies share the ordinary store fence.
use super::{
    Admission, Completion, Lease, Operation, PgPool, Settings, Source, Tx, address, bounded,
    delete_claim, invalid, lock_settings, queue::enqueue_locked, valid_lease,
};
use crate::DbError;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct VideoHint {
    pub uuid: String,
    pub owner_uri: String,
    pub channel_uri: String,
}
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct VideoDetails {
    pub entry_id: i64,
    pub uuid: String,
    pub owner_uri: String,
    pub channel_uri: String,
    pub channel_title: Option<String>,
    pub duration: Option<i32>,
}
pub struct VerifiedVideo<'a> {
    pub hint: &'a VideoHint,
    pub channel_title: &'a str,
    pub duration: i32,
}
fn validate(hint: &VideoHint, target: &str) -> Result<(), DbError> {
    let origin = address(target)?.1;
    if hint.uuid.len() != 36
        || !hint.uuid.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
            }
        })
        || address(&hint.owner_uri)?.1 != origin
        || address(&hint.channel_uri)?.1 != origin
        || hint.owner_uri == hint.channel_uri
    {
        return Err(invalid("invalid video identities"));
    }
    Ok(())
}

pub async fn finish_video_page(
    pool: &PgPool,
    lease: &Lease,
    hints: &[(String, VideoHint)],
) -> Result<Completion, DbError> {
    finish_video_sample(pool, lease, hints, hints.len() == 8).await
}
pub async fn finish_video_sample(
    pool: &PgPool,
    lease: &Lease,
    hints: &[(String, VideoHint)],
    more: bool,
) -> Result<Completion, DbError> {
    if lease.operation != "inspect_source" || hints.len() > 8 {
        return Err(invalid("invalid video catalogue page"));
    }
    let mut tx = pool.begin().await?;
    let settings = lock_settings(&mut tx).await?;
    if !settings.runs("video_catalogue")
        || !settings.runs("video_indexing")
        || !valid_lease(&mut tx, &settings, lease).await?
    {
        return Ok(Completion::Stale);
    }
    let source = sqlx::query_as!(
        Source,
        "SELECT * FROM discovery_sources WHERE id=$1",
        lease.source_id
    )
    .fetch_one(&mut *tx)
    .await?;
    for (uri, hint) in hints {
        validate(hint, uri)?;
        if address(uri)?.1 != source.origin {
            return Err(invalid("video left its source"));
        }
    }
    let endpoint = format!("{}/api/v1/videos", source.origin);
    sqlx::query!("UPDATE discovery_sources SET canonical_uri=$2,capabilities=CASE WHEN 'peer_exploration'=ANY(capabilities) THEN ARRAY['source_inspection','video_catalogue','peer_exploration'] ELSE ARRAY['source_inspection','video_catalogue'] END,verified_at=clock_timestamp(),valid_until=clock_timestamp()+interval '24 hours',cooldown_until=NULL,last_success_at=clock_timestamp() WHERE id=$1",source.id,endpoint).execute(&mut *tx).await?;
    delete_claim(&mut tx, lease).await?;
    let mut novel = 0;
    for (uri, hint) in hints {
        if !super::recovery::new_candidate(&mut tx, uri).await? {
            continue;
        }
        novel += 1;
        let admission = enqueue_locked(
            &mut tx,
            &settings,
            source.id,
            uri,
            Operation::RefreshEntry,
            source.introduced_by,
        )
        .await?;
        if admission == Admission::Capacity {
            super::recovery::defer_candidate(&mut tx,source.id,uri,"video",Some(serde_json::json!({"uuid":hint.uuid,"owner_uri":hint.owner_uri,"channel_uri":hint.channel_uri}))).await?;
        }
        if let Admission::Added(id) = admission {
            sqlx::query!("UPDATE discovery_jobs SET cursor='video' WHERE id=$1", id)
                .execute(&mut *tx)
                .await?;
            sqlx::query!("INSERT INTO discovery_video_work(job_id,uuid,owner_uri,channel_uri) VALUES($1,$2,$3,$4)",id,hint.uuid,hint.owner_uri,hint.channel_uri).execute(&mut *tx).await?;
        }
    }
    super::sampling::advance_sampling(&mut tx, source.id, more, 8, novel).await?;
    tx.commit().await?;
    Ok(Completion::Committed)
}

/// Bind cached identities before any revalidation I/O, including an explicit
/// operator recheck of a first-fetch withdrawal guard. Actor events can then
/// cancel the current job even while the video document is still in flight.
pub async fn video_hint(pool: &PgPool, lease: &Lease) -> Result<Option<VideoHint>, DbError> {
    let mut tx = pool.begin().await?;
    let settings = lock_settings(&mut tx).await?;
    if !settings.runs("video_catalogue")
        || !settings.runs("video_indexing")
        || !valid_lease(&mut tx, &settings, lease).await?
    {
        return Ok(None);
    }
    let hint=sqlx::query_as!(VideoHint,r#"SELECT uuid AS "uuid!",owner_uri AS "owner_uri!",channel_uri AS "channel_uri!" FROM discovery_video_work WHERE job_id=$1 UNION ALL SELECT v.uuid,v.owner_uri,v.channel_uri FROM discovery_video_details v JOIN discovery_entries e ON e.id=v.entry_id WHERE e.canonical_uri=$2 UNION ALL SELECT v.uuid,v.owner_uri,v.channel_uri FROM discovery_video_work v JOIN discovery_jobs j ON j.id=v.job_id WHERE j.target_uri=$2 AND j.withdrawn LIMIT 1"#,lease.id,lease.target_uri).fetch_optional(&mut *tx).await?;
    if let Some(h) = &hint {
        sqlx::query!("INSERT INTO discovery_video_work(job_id,uuid,owner_uri,channel_uri) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING",lease.id,h.uuid,h.owner_uri,h.channel_uri).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(hint)
}

pub async fn video_details(pool: &PgPool, id: i64) -> Result<Option<VideoDetails>, DbError> {
    Ok(sqlx::query_as!(
        VideoDetails,
        "SELECT * FROM discovery_video_details WHERE entry_id=$1",
        id
    )
    .fetch_optional(pool)
    .await?)
}

pub(super) async fn video_current(
    tx: &mut Tx<'_>,
    settings: &Settings,
    lease: &Lease,
    video: &VerifiedVideo<'_>,
) -> Result<bool, DbError> {
    validate(video.hint, &lease.target_uri)?;
    bounded(video.channel_title, 1024)?;
    if !(0..=604_800).contains(&video.duration) {
        return Err(invalid("invalid video duration"));
    }
    if !settings.runs("video_catalogue") || !settings.runs("video_indexing") {
        return Ok(false);
    }
    Ok(sqlx::query_scalar!(r#"SELECT EXISTS(SELECT 1 FROM discovery_video_work WHERE job_id=$1 AND uuid=$2 AND owner_uri=$3 AND channel_uri=$4) AS "current!""#,lease.id,video.hint.uuid,video.hint.owner_uri,video.hint.channel_uri).fetch_one(&mut **tx).await?)
}
pub(super) async fn save_video(
    tx: &mut Tx<'_>,
    id: i64,
    video: &VerifiedVideo<'_>,
    allowed: bool,
) -> Result<(), DbError> {
    sqlx::query!("INSERT INTO discovery_video_details(entry_id,uuid,owner_uri,channel_uri,channel_title,duration) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(entry_id) DO UPDATE SET uuid=excluded.uuid,owner_uri=excluded.owner_uri,channel_uri=excluded.channel_uri,channel_title=excluded.channel_title,duration=excluded.duration",id,video.hint.uuid,video.hint.owner_uri,video.hint.channel_uri,allowed.then_some(video.channel_title),allowed.then_some(video.duration)).execute(&mut **tx).await?;
    Ok(())
}

// Shared configurable catalogue/origin limits are checked by entry_room.
// Per-author diversity belongs to browsing, never destructive storage limits.
pub(super) async fn invalidate_video_actor_locked(
    tx: &mut Tx<'_>,
    actor: &str,
    withdrawn: bool,
    reason: &str,
) -> Result<(), DbError> {
    sqlx::query("DELETE FROM discovery_actor_evidence WHERE uri=$1")
        .bind(actor)
        .execute(&mut **tx)
        .await?;
    sqlx::query!("UPDATE discovery_entries e SET state=CASE WHEN e.state='withdrawn' OR $2 THEN 'withdrawn' ELSE 'denied' END,revision=revision+1,title=NULL,summary=NULL,language=NULL,published_at=NULL,sensitive=false,invalidation_reason=$3,updated_at=clock_timestamp() FROM discovery_video_details v WHERE e.id=v.entry_id AND (v.owner_uri=$1 OR v.channel_uri=$1)",actor,withdrawn,reason).execute(&mut **tx).await?;
    invalidate_work(tx, actor, None, withdrawn, reason).await
}
async fn invalidate_work(
    tx: &mut Tx<'_>,
    actor: &str,
    uri: Option<&str>,
    withdrawn: bool,
    reason: &str,
) -> Result<(), DbError> {
    if withdrawn {
        sqlx::query!("UPDATE discovery_jobs j SET withdrawn=true,search_run_id=NULL,lease_token=NULL,lease_owner=NULL,leased_until=NULL,cursor=NULL,last_error=$3 FROM discovery_video_work w WHERE w.job_id=j.id AND j.operation='refresh_entry' AND (w.owner_uri=$1 OR ($2::text IS NULL AND w.channel_uri=$1)) AND ($2::text IS NULL OR j.target_uri=$2)",actor,uri,reason).execute(&mut **tx).await?;
    }
    sqlx::query!("DELETE FROM discovery_jobs j USING discovery_video_work w WHERE w.job_id=j.id AND NOT j.withdrawn AND (w.owner_uri=$1 OR ($2::text IS NULL AND w.channel_uri=$1)) AND ($2::text IS NULL OR j.target_uri=$2)",actor,uri).execute(&mut **tx).await?;
    Ok(())
}

pub async fn invalidate_owned_video(
    pool: &PgPool,
    actor: &str,
    uri: &str,
    withdrawn: bool,
    reason: &str,
) -> Result<(), DbError> {
    let (actor, origin) = address(actor)?;
    let (uri, target_origin) = address(uri)?;
    bounded(reason, 128)?;
    if origin != target_origin || actor == uri {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    lock_settings(&mut tx).await?;
    sqlx::query!("UPDATE discovery_entries SET state=CASE WHEN state='withdrawn' OR $3 THEN 'withdrawn' ELSE 'denied' END,revision=revision+1,title=NULL,summary=NULL,language=NULL,published_at=NULL,sensitive=false,invalidation_reason=$4,updated_at=clock_timestamp() WHERE kind='video' AND canonical_uri=$1 AND actor_uri=$2",uri,actor,withdrawn,reason).execute(&mut *tx).await?;
    invalidate_work(&mut tx, &actor, Some(&uri), withdrawn, reason).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn video_actor_hidden(pool: &PgPool, viewer: i64, uri: &str) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar!(r#"SELECT EXISTS(SELECT 1 FROM accounts a WHERE a.uri=$2 AND (a.id=$1 OR account_hidden($1,a.id) OR EXISTS(SELECT 1 FROM follow_recommendation_mutes m WHERE m.account_id=$1 AND m.target_account_id=a.id))) AS "hidden!""#,viewer,uri).fetch_one(pool).await?)
}
