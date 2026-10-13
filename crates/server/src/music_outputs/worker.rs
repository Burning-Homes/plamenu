//! Generation-fenced leases keep a native broadcast alive independently of tabs.
use super::Output;
use crate::{AppState, error::ApiError};
use plamenu_db::{DbError, id};

pub async fn claim(state: &AppState, output: i64) -> Result<Option<Output>, ApiError> {
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    sqlx::query("SELECT pg_advisory_xact_lock(7410170)")
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM music_outputs WHERE worker_lease IS NOT NULL AND lease_until>now()",
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(DbError::from)?;
    if count >= 8 {
        tx.commit().await.map_err(DbError::from)?;
        return Ok(None);
    }
    let row:Option<Output>=sqlx::query_as("UPDATE music_outputs o SET worker_lease=$2,lease_until=now()+interval '10 seconds',generation=generation+1,last_error=NULL,codec_header=''::bytea WHERE id=$1 AND enabled AND transport='playing' AND (worker_lease IS NULL OR lease_until<now()) AND EXISTS(SELECT 1 FROM accounts a WHERE a.id=o.owner_id AND a.domain IS NULL AND a.deleted_at IS NULL AND a.suspended_at IS NULL AND NOT EXISTS(SELECT 1 FROM users u WHERE u.account_id=a.id AND (NOT u.approved OR u.confirmed_at IS NULL))) RETURNING o.*")
        .bind(output).bind(id::next()).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
    if let Some(row) = &row {
        sqlx::query("DELETE FROM music_output_chunks WHERE output_id=$1")
            .bind(row.id)
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
    }
    tx.commit().await.map_err(DbError::from)?;
    Ok(row)
}
pub async fn heartbeat(state: &AppState, row: &Output) -> Result<bool, ApiError> {
    let allowed = crate::music_renditions::negotiate(
        state,
        Some(row.owner_id),
        &row.format,
        u32::try_from(row.bitrate).ok(),
    )
    .await?;
    if i32::try_from(allowed.bitrate).ok() != Some(row.bitrate) {
        return Err(ApiError::Conflict(
            "The permitted stream bitrate changed; restart playback".into(),
        ));
    }
    Ok(sqlx::query("UPDATE music_outputs o SET lease_until=now()+interval '10 seconds' WHERE id=$1 AND generation=$2 AND worker_lease=$3 AND lease_until>now() AND enabled AND transport='playing' AND EXISTS(SELECT 1 FROM accounts a WHERE a.id=o.owner_id AND a.deleted_at IS NULL AND a.suspended_at IS NULL AND NOT EXISTS(SELECT 1 FROM users u WHERE u.account_id=a.id AND (NOT u.approved OR u.confirmed_at IS NULL)))")
        .bind(row.id).bind(row.generation).bind(row.worker_lease).execute(&state.pool).await.map_err(DbError::from)?.rows_affected()==1)
}

/// Resolve the current queue entry immediately before a decoder is opened.
pub async fn selected(
    state: &AppState,
    row: &Output,
) -> Result<crate::music_collections::Entry, ApiError> {
    let queue = crate::music_queues::get(state, row.owner_id, &row.device).await?;
    let entry = queue
        .current_entry_id
        .as_deref()
        .and_then(|id| id.parse::<i64>().ok())
        .ok_or(ApiError::NotFound)?;
    let selected =
        crate::music_queues::output_entry(state, row.owner_id, &row.device, entry).await?;
    if crate::music_collections::sources(state, std::slice::from_ref(&selected), Some(row.owner_id))
        .await?
        .is_empty()
    {
        return Err(ApiError::NotFound);
    }
    Ok(selected)
}

/// Each chunk is committed only while this exact encoder generation owns the row.
pub async fn publish(
    state: &AppState,
    row: &Output,
    sequence: i64,
    bytes: &[u8],
) -> Result<(), ApiError> {
    if bytes.is_empty() || bytes.len() > 16384 || sequence <= 0 {
        return Err(ApiError::Unprocessable("Invalid HTTP audio chunk".into()));
    }
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    let current:Option<i64>=sqlx::query_scalar("SELECT id FROM music_outputs WHERE id=$1 AND generation=$2 AND worker_lease=$3 AND lease_until>now() AND enabled AND transport='playing' FOR UPDATE")
        .bind(row.id).bind(row.generation).bind(row.worker_lease).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
    if current.is_none() {
        return Err(ApiError::Conflict(
            "HTTP stream worker lease expired".into(),
        ));
    }
    sqlx::query(
        "INSERT INTO music_output_chunks(output_id,generation,sequence,bytes,title) SELECT $1,$2,$3,$4,now_playing FROM music_outputs WHERE id=$1",
    )
    .bind(row.id)
    .bind(row.generation)
    .bind(sequence)
    .bind(bytes)
    .execute(&mut *tx)
    .await
    .map_err(DbError::from)?;
    sqlx::query("DELETE FROM music_output_chunks WHERE output_id=$1 AND (generation<>$2 OR sequence<=$3-512)")
        .bind(row.id).bind(row.generation).bind(sequence).execute(&mut *tx).await.map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}

async fn feed(
    state: &AppState,
    row: &Output,
    input: &mut tokio::process::ChildStdin,
    launcher: &std::path::Path,
) -> Result<(), ApiError> {
    let mut listening: Option<(i64, super::playback::Listen)> = None;
    loop {
        if state.shutdown.is_cancelled() || !heartbeat(state, row).await? {
            return Ok(());
        }
        let mut queue = crate::music_queues::get(state, row.owner_id, &row.device).await?;
        if queue.current_entry_id.is_none() {
            if queue.entries.is_empty() {
                return Ok(());
            }
            queue = crate::music_queues::change_output(
                state,
                row,
                &crate::music_queues::Change {
                    revision: queue.revision,
                    action: crate::music_queues::Action::Next { ended: false },
                },
            )
            .await?;
        }
        let entry = selected(state, row).await?;
        let playable = queue
            .entries
            .iter()
            .find(|item| item.entry_id == entry.id.to_string())
            .ok_or(ApiError::NotFound)?;
        let title = if playable.artist.is_empty() {
            playable.title.clone()
        } else {
            format!("{} - {}", playable.artist, playable.title)
        };
        sqlx::query("UPDATE music_outputs SET now_playing=$4,current_entry_id=$5 WHERE id=$1 AND generation=$2 AND worker_lease=$3 AND lease_until>now()")
            .bind(row.id).bind(row.generation).bind(row.worker_lease).bind(title.chars().take(1000).collect::<String>()).bind(entry.id).execute(&state.pool).await.map_err(DbError::from)?;
        let listen = listening_session(state, row, &queue, entry.id, &mut listening).await?;
        if !super::pipeline::track(state, row, &entry, input, launcher, &queue, listen).await? {
            continue;
        }
        listening = None;
        let queue = crate::music_queues::get(state, row.owner_id, &row.device).await?;
        if queue.current_entry_id.as_deref() != Some(entry.id.to_string().as_str()) {
            return Err(ApiError::Conflict("HTTP stream queue changed".into()));
        }
        let next = crate::music_queues::change_output(
            state,
            row,
            &crate::music_queues::Change {
                revision: queue.revision,
                action: crate::music_queues::Action::Next { ended: true },
            },
        )
        .await?;
        if next.current_entry_id.is_none() {
            return Ok(());
        }
    }
}
async fn listening_session<'a>(
    state: &AppState,
    row: &Output,
    queue: &crate::music_queues::Queue,
    entry: i64,
    current: &'a mut Option<(i64, super::playback::Listen)>,
) -> Result<&'a mut super::playback::Listen, ApiError> {
    if current.as_ref().is_none_or(|(id, _)| *id != entry) {
        if let Some((_, listen)) = current {
            listen.report(state, row, 0.0, true).await?;
        }
        *current = Some((
            entry,
            super::playback::Listen::begin(state, row, queue).await?,
        ));
    }
    Ok(&mut current
        .as_mut()
        .expect("current listening session was initialized")
        .1)
}
/// Run one claimed generation; dropping either branch kills its child pipelines.
pub async fn run(
    state: &AppState,
    output: i64,
    launcher: std::path::PathBuf,
) -> Result<(), ApiError> {
    let Some(row) = claim(state, output).await? else {
        return Ok(());
    };
    let result = run_claimed(state, &row, &launcher).await;
    let error = result
        .as_ref()
        .err()
        .map(|error| error.to_string().chars().take(2000).collect::<String>());
    sqlx::query("UPDATE music_outputs SET worker_lease=NULL,lease_until=NULL,transport=CASE WHEN $5 THEN transport ELSE 'paused' END,last_error=$4,updated_at=now() WHERE id=$1 AND generation=$2 AND worker_lease=$3")
        .bind(row.id).bind(row.generation).bind(row.worker_lease).bind(error).bind(state.shutdown.is_cancelled()).execute(&state.pool).await.map_err(DbError::from)?;
    result
}

async fn run_claimed(
    state: &AppState,
    row: &Output,
    launcher: &std::path::Path,
) -> Result<(), ApiError> {
    let allowed = crate::music_renditions::negotiate(
        state,
        Some(row.owner_id),
        &row.format,
        u32::try_from(row.bitrate).ok(),
    )
    .await?;
    let mut row = row.clone();
    if i32::try_from(allowed.bitrate).ok() != Some(row.bitrate) {
        row.bitrate = i32::try_from(allowed.bitrate).map_err(|_| ApiError::NotFound)?;
        sqlx::query("UPDATE music_outputs SET bitrate=$4,revision=revision+1 WHERE id=$1 AND generation=$2 AND worker_lease=$3 AND lease_until>now()")
            .bind(row.id).bind(row.generation).bind(row.worker_lease).bind(row.bitrate).execute(&state.pool).await.map_err(DbError::from)?;
    }
    let super::pipeline::Encoder {
        mut child,
        mut input,
        output,
    } = super::pipeline::encoder(state, &row, launcher)?;
    let mut pump = Box::pin(super::pipeline::pump(state.clone(), row.clone(), output));
    let (result, drain) = tokio::select! {
        result=&mut pump=>(result,false),
        result=feed(state,&row,&mut input,launcher)=>(result,true),
    };
    drop(input);
    let result = if drain && result.is_ok() {
        tokio::time::timeout(std::time::Duration::from_secs(5), &mut pump)
            .await
            .unwrap_or_else(|_| {
                Err(ApiError::ServiceUnavailable(
                    "HTTP audio encoder did not finish".into(),
                ))
            })
    } else {
        result
    };
    let result = if result.is_ok() && !state.shutdown.is_cancelled() {
        match tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await {
            Ok(Ok(status)) if status.success() => result,
            Ok(Ok(_)) => Err(ApiError::BadGateway("HTTP audio encoder failed".into())),
            Ok(Err(error)) => Err(ApiError::Internal(Box::new(error))),
            Err(_) => Err(ApiError::ServiceUnavailable(
                "HTTP audio encoder stalled".into(),
            )),
        }
    } else {
        result
    };
    let _ = child.kill().await;
    result
}

#[must_use]
pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tasks = tokio::task::JoinSet::new();
        let mut active = std::collections::HashSet::new();
        let mut task_outputs = std::collections::HashMap::new();
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tokio::select! {
                ()=state.shutdown.cancelled()=>break,
                completed=tasks.join_next(),if !tasks.is_empty()=>{
                    match completed {
                        Some(Ok((id,result)))=>{
                            active.remove(&id);
                            task_outputs.retain(|_,output|*output!=id);
                            if let Err(error)=result {tracing::warn!(output=id,error=%error,"HTTP music output stopped");}
                        },
                        Some(Err(error))=>{
                            if let Some(output)=task_outputs.remove(&error.id()) {active.remove(&output);}
                            tracing::error!(%error,"HTTP music output task failed");
                        },
                        None=>{},
                    }
                },
                _=tick.tick()=>{
                    let ids=sqlx::query_scalar::<_,i64>("SELECT id FROM music_outputs WHERE enabled AND transport='playing' AND (worker_lease IS NULL OR lease_until<now()) ORDER BY id LIMIT 20")
                        .fetch_all(&state.pool).await;
                    match ids {
                        Ok(ids)=>for id in ids {
                            if active.len()>=8 {break;}
                            if !active.insert(id) {continue;}
                            let state=state.clone();
                            let task=tasks.spawn(async move {
                                let result=match crate::media_limits::running_executable() {
                                    Ok(launcher)=>run(&state,id,launcher).await,
                                    Err(error)=>Err(ApiError::Internal(Box::new(error))),
                                };
                                (id,result)
                            });
                            task_outputs.insert(task.id(),id);
                        },
                        Err(error)=>tracing::error!(%error,"HTTP music output scan failed"),
                    }
                },
            }
        }
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    })
}
