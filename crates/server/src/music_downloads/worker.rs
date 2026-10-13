//! Archive leases reserve storage before writing and fence cancellation/finalize.
use super::Download;
use crate::{AppState, error::ApiError, music_assets, music_queues};
use plamenu_db::{DbError, id};
pub async fn heartbeat(state: &AppState, job: &Download) -> Result<(), ApiError> {
    if state.shutdown.is_cancelled() {
        return Err(ApiError::Conflict("Archive worker stopped".into()));
    }
    if let Some(parent) = &job.selection.parent {
        super::selection::parent(state, job.owner_id, parent).await?;
    }
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, job.owner_id).await?;
    music_queues::active_owner(&mut tx, job.owner_id).await?;
    let renewed=sqlx::query("UPDATE music_downloads SET lease_until=now()+interval '30 seconds' WHERE id=$1 AND lease=$2 AND lease_until>now() AND state='running' AND expires_at>now()")
        .bind(job.id).bind(job.lease).execute(&mut *tx).await.map_err(DbError::from)?;
    if renewed.rows_affected() != 1 {
        return Err(ApiError::Conflict(
            "Music download cancelled or lease expired".into(),
        ));
    }
    let journal=sqlx::query("UPDATE media_cleanup_jobs SET run_at=now()+interval '3 minutes' WHERE file_name=$1 AND attempts=0 AND run_at>now()")
        .bind(&job.file_name).execute(&mut *tx).await.map_err(DbError::from)?;
    if journal.rows_affected() != 1 {
        return Err(ApiError::Conflict(
            "Music download storage reservation expired".into(),
        ));
    }
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
pub async fn claim(state: &AppState, download: i64) -> Result<Option<Download>, ApiError> {
    let owner: Option<i64> = sqlx::query_scalar("SELECT owner_id FROM music_downloads WHERE id=$1")
        .bind(download)
        .fetch_optional(&state.pool)
        .await
        .map_err(DbError::from)?;
    let Some(owner) = owner else {
        return Ok(None);
    };
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, owner).await?;
    music_queues::active_owner(&mut tx, owner).await?;
    sqlx::query("SELECT pg_advisory_xact_lock(7410172)")
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM music_downloads WHERE state='running' AND lease_until>now()",
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(DbError::from)?;
    if usize::try_from(active).unwrap_or(usize::MAX) >= state.config.music.download_jobs {
        return Ok(None);
    }
    let row:Option<Download>=sqlx::query_as("SELECT * FROM music_downloads WHERE id=$1 AND expires_at>now() AND (state='queued' OR (state='running' AND lease_until<now())) FOR UPDATE SKIP LOCKED")
        .bind(download).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
    let Some(row) = row else {
        return Ok(None);
    };
    if let Some(key) = &row.file_name {
        sqlx::query("UPDATE media_cleanup_jobs SET run_at=now() WHERE file_name=$1")
            .bind(key)
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
    }
    if let Some(parent) = &row.selection.parent {
        super::selection::parent(state, owner, parent).await?;
    }
    let lease = id::next();
    let key = format!("musicdownload{}-{lease}.zip", row.id);
    let budget = i64::try_from(super::content::estimate(state, owner, &row.selection.items).await?)
        .map_err(|_| ApiError::Unprocessable("Download size exceeds its limit".into()))?;
    music_assets::reserve_conn(state, &mut tx, owner, std::slice::from_ref(&key), &[budget])
        .await?;
    let cleanup: i64 = sqlx::query_scalar("SELECT id FROM media_cleanup_jobs WHERE file_name=$1")
        .bind(&key)
        .fetch_one(&mut *tx)
        .await
        .map_err(DbError::from)?;
    let row=sqlx::query_as("UPDATE music_downloads SET state='running',revision=revision+1,lease=$2,lease_until=now()+interval '30 seconds',file_name=$3,budget_bytes=$4,cleanup_job_id=$5,completed_items=0,item_errors='{}'::music_download_item_error[],error=NULL WHERE id=$1 RETURNING *")
        .bind(download).bind(lease).bind(key).bind(budget).bind(cleanup).fetch_one(&mut *tx).await.map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(Some(row))
}
async fn finalize(
    state: &AppState,
    job: &Download,
    built: super::writer::Built,
) -> Result<(), ApiError> {
    heartbeat(state, job).await?;
    for index in &built.included {
        super::selection::playable(
            state,
            job.owner_id,
            &job.selection.items[usize::try_from(*index).expect("bounded member")],
        )
        .await?;
    }
    let size = i64::try_from(
        built
            .file
            .as_file()
            .metadata()
            .map_err(super::internal)?
            .len(),
    )
    .map_err(|_| ApiError::Unprocessable("Archive size exceeded".into()))?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, job.owner_id).await?;
    music_queues::active_owner(&mut tx, job.owner_id).await?;
    let current:Option<i64>=sqlx::query_scalar("SELECT id FROM music_downloads WHERE id=$1 AND lease=$2 AND state='running' AND lease_until>now() AND expires_at>now() FOR UPDATE")
        .bind(job.id).bind(job.lease).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
    current.ok_or_else(|| ApiError::Conflict("Music download cancelled".into()))?;
    let key = job.file_name.as_ref().ok_or(ApiError::NotFound)?;
    let journal:Option<i64>=sqlx::query_scalar("SELECT id FROM media_cleanup_jobs WHERE file_name=$1 AND attempts=0 AND run_at>now() FOR UPDATE")
        .bind(key).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
    journal
        .ok_or_else(|| ApiError::Conflict("Music download storage reservation expired".into()))?;
    state
        .media
        .put_file(key, built.file.path())
        .await
        .map_err(super::internal)?;
    sqlx::query("UPDATE music_downloads SET state='ready',revision=revision+1,lease=NULL,lease_until=NULL,size_bytes=$3,included=$4,item_errors=$5,completed_at=now() WHERE id=$1 AND lease=$2")
        .bind(job.id).bind(job.lease).bind(size).bind(built.included).bind(super::records::Errors::from_value(built.errors)?).execute(&mut *tx).await.map_err(DbError::from)?;
    let notification = id::next();
    sqlx::query("INSERT INTO notifications(id,account_id,from_account_id,kind,reasons) VALUES($1,$2,$2,'music.download',ARRAY['music.download'])")
        .bind(notification).bind(job.owner_id).execute(&mut *tx).await.map_err(DbError::from)?;
    sqlx::query("UPDATE music_downloads SET notification_id=$2 WHERE id=$1")
        .bind(job.id)
        .bind(notification)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    sqlx::query("DELETE FROM media_cleanup_jobs WHERE file_name=$1")
        .bind(key)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
pub async fn run(state: &AppState, download: i64) -> Result<(), ApiError> {
    let job = match claim(state, download).await {
        Ok(Some(job)) => job,
        Ok(None) => return Ok(()),
        Err(error) => {
            sqlx::query("UPDATE music_downloads SET state='failed',revision=revision+1,lease=NULL,lease_until=NULL,error='Music download could not start; check current access and storage limits' WHERE id=$1 AND (state='queued' OR (state='running' AND lease_until<now()))")
                .bind(download).execute(&state.pool).await.map_err(DbError::from)?;
            return Err(error);
        }
    };
    let result = tokio::time::timeout(std::time::Duration::from_mins(30), async {
        let built = super::writer::build(state, &job).await?;
        finalize(state, &job, built).await
    })
    .await
    .unwrap_or_else(|_| {
        Err(ApiError::ServiceUnavailable(
            "Music download timed out".into(),
        ))
    });
    if result.is_err() && !state.shutdown.is_cancelled() {
        let mut tx = state.pool.begin().await.map_err(DbError::from)?;
        sqlx::query("UPDATE music_downloads SET state='failed',revision=revision+1,lease=NULL,lease_until=NULL,error='Music download could not finish; check current access and try again' WHERE id=$1 AND lease=$2")
            .bind(job.id).bind(job.lease).execute(&mut *tx).await.map_err(DbError::from)?;
        sqlx::query("UPDATE media_cleanup_jobs SET run_at=now() WHERE file_name=$1")
            .bind(job.file_name)
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
        tx.commit().await.map_err(DbError::from)?;
    }
    result
}
pub async fn expire(state: &AppState) -> Result<(), ApiError> {
    let rows: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT id,owner_id FROM music_downloads WHERE expires_at<now() ORDER BY id LIMIT 100",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(DbError::from)?;
    for (id, owner) in rows {
        let mut tx = state.pool.begin().await.map_err(DbError::from)?;
        match music_assets::lock_owner(&mut tx, owner).await {
            Ok(()) => {}
            Err(ApiError::NotFound) => continue,
            Err(error) => return Err(error),
        }
        sqlx::query("DELETE FROM music_downloads WHERE id=$1 AND owner_id=$2 AND expires_at<now()")
            .bind(id)
            .bind(owner)
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
        tx.commit().await.map_err(DbError::from)?;
    }
    Ok(())
}
#[must_use]
pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tasks = tokio::task::JoinSet::new();
        let mut active = std::collections::HashSet::new();
        let mut identities = std::collections::HashMap::new();
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            tokio::select! {
                ()=state.shutdown.cancelled()=>break,
                complete=tasks.join_next(),if !tasks.is_empty()=>match complete {
                    Some(Ok((id,result)))=>{active.remove(&id);identities.retain(|_,value|*value!=id);if let Err(error)=result {tracing::warn!(download=id,%error,"Music download failed");}},
                    Some(Err(error))=>{if let Some(id)=identities.remove(&error.id()) {active.remove(&id);}tracing::error!(%error,"Music download worker failed");},None=>{},
                },
                _=tick.tick()=>{
                    let expired=expire(&state).await;
                    if let Err(error)=expired {tracing::warn!(%error,"Music download expiry failed");}
                    let ids=sqlx::query_scalar::<_,i64>("SELECT id FROM music_downloads WHERE expires_at>now() AND (state='queued' OR (state='running' AND lease_until<now())) ORDER BY id LIMIT 20").fetch_all(&state.pool).await;
                    match ids {Ok(ids)=>for id in ids {
                        if active.len()>=state.config.music.download_jobs {break;}
                        if !active.insert(id){continue;}
                        let state=state.clone();
                        let task=tasks.spawn(async move {(id,run(&state,id).await)});
                        identities.insert(task.id(),id);
                    },Err(error)=>tracing::warn!(%error,"Music download queue failed"),}
                },
            }
        }
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    })
}
