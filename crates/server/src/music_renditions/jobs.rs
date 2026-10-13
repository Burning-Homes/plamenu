//! Persistent single-flight jobs. Each lease writes a fresh private key;
//! expired or cancelled encoders cannot overwrite a successor's artifact.
use super::{Format, Selection};
use crate::{AppState, error::ApiError, music_assets};
use plamenu_db::{DbError, id};
use serde_json::{Value, json};

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Rendition {
    pub id: i64,
    pub asset_id: i64,
    pub owner_id: i64,
    pub source_hash: String,
    pub format: String,
    pub bitrate: i32,
    pub actual_bitrate: Option<i32>,
    pub requester_id: Option<i64>,
    pub principal_kind: String,
    pub share_link_id: Option<i64>,
    pub state: String,
    pub file_name: Option<String>,
    pub size_bytes: i64,
    pub budget_bytes: i64,
    pub lease: Option<i64>,
    pub cleanup_job_id: Option<i64>,
    pub error: Option<String>,
    pub progress_dir: Option<String>,
    pub keep_running: bool,
}
pub(super) async fn current_source(
    state: &AppState,
    asset: i64,
    viewer: Option<i64>,
    share: Option<i64>,
) -> Result<music_assets::Asset, ApiError> {
    let source = if let Some(link) = share {
        let scope = crate::music_shares::resolve_job(state, link, viewer).await?;
        crate::music_shares::asset(state, &scope, asset, viewer).await?
    } else {
        music_assets::get(state, asset, viewer).await?
    };
    if source.indexed_original {
        crate::music_original::open(state, &source).await?;
    }
    Ok(source)
}
async fn job_source(state: &AppState, job: &Rendition) -> Result<music_assets::Asset, ApiError> {
    if (job.principal_kind == "account" && job.requester_id.is_none())
        || (job.principal_kind == "share" && job.share_link_id.is_none())
    {
        return Err(ApiError::NotFound);
    }
    current_source(state, job.asset_id, job.requester_id, job.share_link_id).await
}
pub async fn get_shared(
    state: &AppState,
    asset: i64,
    job: i64,
    viewer: Option<i64>,
    scope: &crate::music_shares::Scope,
) -> Result<Rendition, ApiError> {
    let source = current_source(state, asset, viewer, Some(scope.id)).await?;
    sqlx::query_as("SELECT * FROM music_renditions WHERE id=$1 AND asset_id=$2 AND source_hash=$3 AND NOT EXISTS(SELECT 1 FROM media_cleanup_jobs c WHERE c.file_name=music_renditions.file_name AND music_renditions.state='ready')")
        .bind(job).bind(asset).bind(source.audio_hash).fetch_optional(&state.pool).await.map_err(DbError::from)?.ok_or(ApiError::NotFound)
}
pub async fn request_shared(
    state: &AppState,
    asset: i64,
    viewer: Option<i64>,
    selection: Selection,
    scope: &crate::music_shares::Scope,
) -> Result<Rendition, ApiError> {
    request_for(state, asset, viewer, selection, Some(scope.id), None).await
}
impl Rendition {
    #[must_use]
    pub fn entity(&self) -> Value {
        json!({"id":self.id.to_string(),"asset_id":self.asset_id.to_string(),"format":self.format,"bitrate":self.bitrate,"actual_bitrate":if self.format=="flac"{None}else{self.actual_bitrate},"lossless":self.format=="flac","state":self.state,"bytes":self.size_bytes,"error":self.error,"stream_url":format!("/music/assets/{}/renditions/{}/stream",self.asset_id,self.id)})
    }
}
pub async fn negotiate(
    state: &AppState,
    viewer: Option<i64>,
    format: &str,
    bitrate: Option<u32>,
) -> Result<Selection, ApiError> {
    let role = if let Some(viewer) = viewer {
        let mut conn = state.pool.acquire().await.map_err(DbError::from)?;
        crate::music_queues::active_owner(&mut conn, viewer).await?;
        drop(conn);
        if let Some(user) = plamenu_db::user::find_by_account_id(&state.pool, viewer).await? {
            plamenu_db::role::for_user(&state.pool, user.id).await?
        } else {
            None
        }
    } else {
        None
    };
    super::selection(state.config.music, format, bitrate, role.as_ref())
}
pub async fn get(
    state: &AppState,
    asset: i64,
    job: i64,
    viewer: Option<i64>,
) -> Result<Rendition, ApiError> {
    let source = current_source(state, asset, viewer, None).await?;
    sqlx::query_as("SELECT * FROM music_renditions WHERE id=$1 AND asset_id=$2 AND source_hash=$3 AND NOT EXISTS(SELECT 1 FROM media_cleanup_jobs c WHERE c.file_name=music_renditions.file_name AND music_renditions.state='ready')")
        .bind(job)
        .bind(asset)
        .bind(source.audio_hash)
        .fetch_optional(&state.pool)
        .await
        .map_err(DbError::from)?
        .ok_or(ApiError::NotFound)
}
pub async fn list(
    state: &AppState,
    asset: i64,
    viewer: Option<i64>,
) -> Result<Vec<Rendition>, ApiError> {
    let source = current_source(state, asset, viewer, None).await?;
    let ceiling = negotiate(state, viewer, "flac", Some(512)).await?.bitrate;
    Ok(sqlx::query_as("SELECT * FROM music_renditions WHERE asset_id=$1 AND source_hash=$2 AND (format='flac' OR COALESCE(actual_bitrate,bitrate)<=$3) AND NOT EXISTS(SELECT 1 FROM media_cleanup_jobs c WHERE c.file_name=music_renditions.file_name AND music_renditions.state='ready') ORDER BY id DESC LIMIT 60")
        .bind(asset).bind(source.audio_hash).bind(i32::try_from(ceiling).unwrap()).fetch_all(&state.pool).await.map_err(DbError::from)?)
}
pub async fn list_shared(
    state: &AppState,
    asset: i64,
    viewer: Option<i64>,
    scope: &crate::music_shares::Scope,
) -> Result<Vec<Rendition>, ApiError> {
    let source = current_source(state, asset, viewer, Some(scope.id)).await?;
    let ceiling = negotiate(state, viewer, "flac", Some(512)).await?.bitrate;
    Ok(sqlx::query_as("SELECT * FROM music_renditions WHERE asset_id=$1 AND source_hash=$2 AND (format='flac' OR COALESCE(actual_bitrate,bitrate)<=$3) AND NOT EXISTS(SELECT 1 FROM media_cleanup_jobs c WHERE c.file_name=music_renditions.file_name AND music_renditions.state='ready') ORDER BY id DESC LIMIT 60")
        .bind(asset).bind(source.audio_hash).bind(i32::try_from(ceiling).unwrap()).fetch_all(&state.pool).await.map_err(DbError::from)?)
}
pub async fn authorize_playback(
    state: &AppState,
    row: &Rendition,
    viewer: Option<i64>,
) -> Result<(), ApiError> {
    let requested = u32::try_from(row.bitrate).unwrap();
    let allowed = negotiate(state, viewer, &row.format, Some(requested)).await?;
    if row.format != "flac"
        && row.actual_bitrate.unwrap_or(row.bitrate) > i32::try_from(allowed.bitrate).unwrap()
    {
        return Err(ApiError::Forbidden(
            "Prepare a copy within your allowed bitrate".into(),
        ));
    }
    Ok(())
}
pub async fn request(
    state: &AppState,
    asset: i64,
    viewer: Option<i64>,
    selection: Selection,
) -> Result<Rendition, ApiError> {
    request_for(state, asset, viewer, selection, None, None).await
}
async fn request_for(
    state: &AppState,
    asset: i64,
    viewer: Option<i64>,
    selection: Selection,
    share: Option<i64>,
    listener: Option<i64>,
) -> Result<Rendition, ApiError> {
    let selection = negotiate(
        state,
        viewer,
        selection.format.name(),
        Some(selection.bitrate),
    )
    .await?;
    let source = current_source(state, asset, viewer, share).await?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, source.owner_id).await?;
    // Recheck under the source-owner fence before admitting any new work.
    let source = current_source(state, asset, viewer, share).await?;
    sqlx::query("SELECT pg_advisory_xact_lock(7410162)")
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    let previous: Option<Rendition> = sqlx::query_as("SELECT * FROM music_renditions WHERE asset_id=$1 AND source_hash=$2 AND format=$3 AND bitrate=$4 FOR UPDATE")
        .bind(asset).bind(&source.audio_hash).bind(selection.format.name()).bind(i32::try_from(selection.bitrate).unwrap())
        .fetch_optional(&mut *tx).await.map_err(DbError::from)?;
    if let Some(previous) = previous {
        if matches!(previous.state.as_str(), "queued" | "running" | "ready") {
            if let Some(listener) = listener {
                admit_listener(&mut tx, previous.id, listener).await?;
            } else if !previous.keep_running {
                sqlx::query("UPDATE music_renditions SET keep_running=true WHERE id=$1")
                    .bind(previous.id)
                    .execute(&mut *tx)
                    .await
                    .map_err(DbError::from)?;
            }
            tx.commit().await.map_err(DbError::from)?;
            return Ok(previous);
        }
        sqlx::query("DELETE FROM music_renditions WHERE id=$1")
            .bind(previous.id)
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
    }
    let (owner,total):(i64,i64) = sqlx::query_as("SELECT count(*) FILTER(WHERE owner_id=$1),count(*) FROM music_renditions WHERE state IN ('queued','running')")
        .bind(source.owner_id).fetch_one(&mut *tx).await.map_err(DbError::from)?;
    if owner >= 20 || total >= 200 {
        return Err(ApiError::TooManyRequests);
    }
    let job: Rendition = sqlx::query_as("INSERT INTO music_renditions(id,asset_id,owner_id,source_hash,format,bitrate,requester_id,principal_kind,share_link_id,keep_running) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) RETURNING *")
        .bind(id::next()).bind(asset).bind(source.owner_id).bind(source.audio_hash).bind(selection.format.name())
        .bind(i32::try_from(selection.bitrate).unwrap()).bind(viewer).bind(if share.is_some(){"share"}else if viewer.is_some(){"account"}else{"anonymous"}).bind(share).bind(listener.is_none()).fetch_one(&mut *tx).await.map_err(DbError::from)?;
    if let Some(listener) = listener {
        admit_listener(&mut tx, job.id, listener).await?;
    }
    tx.commit().await.map_err(DbError::from)?;
    Ok(job)
}
async fn admit_listener(
    conn: &mut sqlx::PgConnection,
    job: i64,
    listener: i64,
) -> Result<(), ApiError> {
    sqlx::query("DELETE FROM music_rendition_listeners WHERE lease_until<now()")
        .execute(&mut *conn)
        .await
        .map_err(DbError::from)?;
    let (per_job, total): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER(WHERE rendition_id=$1),count(*) FROM music_rendition_listeners",
    )
    .bind(job)
    .fetch_one(&mut *conn)
    .await
    .map_err(DbError::from)?;
    if per_job >= 16 || total >= 256 {
        return Err(ApiError::TooManyRequests);
    }
    sqlx::query("INSERT INTO music_rendition_listeners(id,rendition_id) VALUES($1,$2)")
        .bind(listener)
        .bind(job)
        .execute(conn)
        .await
        .map_err(DbError::from)?;
    Ok(())
}
/// The guard removes its own listener on body drop. A process crash instead
/// lets its finite durable lease expire; no disconnected client lives forever.
pub struct Listener {
    pool: sqlx::PgPool,
    pub id: i64,
    pub job: i64,
}
impl Drop for Listener {
    fn drop(&mut self) {
        let pool = self.pool.clone();
        let listener = self.id;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Err(error) = sqlx::query("DELETE FROM music_rendition_listeners WHERE id=$1")
                    .bind(listener)
                    .execute(&pool)
                    .await
                {
                    tracing::warn!(%error,"Rendition listener cleanup deferred to lease expiry");
                }
            });
        }
    }
}
impl Listener {
    pub async fn renew(&self) -> Result<(), ApiError> {
        let changed=sqlx::query("UPDATE music_rendition_listeners SET lease_until=now()+interval '45 seconds' WHERE id=$1 AND rendition_id=$2 AND lease_until>now()")
            .bind(self.id).bind(self.job).execute(&self.pool).await.map_err(DbError::from)?.rows_affected();
        if changed != 1 {
            return Err(ApiError::Conflict("Playback listener expired".into()));
        }
        Ok(())
    }
}
pub async fn request_cold(
    state: &AppState,
    asset: i64,
    viewer: Option<i64>,
    selection: Selection,
    scope: Option<&crate::music_shares::Scope>,
) -> Result<(Rendition, Listener), ApiError> {
    let listener = id::next();
    let row = request_for(
        state,
        asset,
        viewer,
        selection,
        scope.map(|s| s.id),
        Some(listener),
    )
    .await?;
    Ok((
        row.clone(),
        Listener {
            pool: state.pool.clone(),
            id: listener,
            job: row.id,
        },
    ))
}
pub async fn cancel(state: &AppState, asset: i64, job: i64, owner: i64) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, owner).await?;
    let key:Option<Option<String>>=sqlx::query_scalar("DELETE FROM music_renditions WHERE id=$1 AND asset_id=$2 AND owner_id=$3 RETURNING file_name")
        .bind(job).bind(asset).bind(owner).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
    let key = key.ok_or(ApiError::NotFound)?;
    if let Some(key) = key {
        sqlx::query("UPDATE media_cleanup_jobs SET run_at=now() WHERE file_name=$1")
            .bind(key)
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
    }
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
async fn claim(state: &AppState, job: i64) -> Result<Option<Rendition>, ApiError> {
    let owner: Option<i64> =
        sqlx::query_scalar("SELECT owner_id FROM music_renditions WHERE id=$1")
            .bind(job)
            .fetch_optional(&state.pool)
            .await
            .map_err(DbError::from)?;
    let Some(owner) = owner else { return Ok(None) };
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, owner).await?;
    sqlx::query("SELECT pg_advisory_xact_lock(7410162)")
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    let row:Option<Rendition>=sqlx::query_as("SELECT * FROM music_renditions WHERE id=$1 AND (state='queued' OR (state='running' AND lease_until<now())) FOR UPDATE SKIP LOCKED")
        .bind(job).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
    let Some(row) = row else { return Ok(None) };
    let wanted:bool=sqlx::query_scalar("SELECT keep_running OR EXISTS(SELECT 1 FROM music_rendition_listeners l WHERE l.rendition_id=music_renditions.id AND l.lease_until>now()) FROM music_renditions WHERE id=$1")
        .bind(row.id).fetch_one(&mut *tx).await.map_err(DbError::from)?;
    if !wanted {
        sqlx::query("UPDATE music_renditions SET state='cancelled',lease=NULL,lease_until=NULL,progress_dir=NULL,error='Playback listeners disconnected' WHERE id=$1").bind(row.id).execute(&mut *tx).await.map_err(DbError::from)?;
        tx.commit().await.map_err(DbError::from)?;
        return Ok(None);
    }
    if let Some(key) = &row.file_name {
        sqlx::query("UPDATE media_cleanup_jobs SET run_at=now() WHERE file_name=$1")
            .bind(key)
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
    }
    let source = job_source(state, &row).await?;
    if source.audio_hash != row.source_hash {
        return Err(ApiError::NotFound);
    }
    let (running,scratch):(i64,i64)=sqlx::query_as("SELECT count(*),COALESCE(sum(budget_bytes*2),0)::bigint FROM music_renditions WHERE state='running' AND lease_until>=now()")
        .fetch_one(&mut *tx).await.map_err(DbError::from)?;
    let policy = state.config.music;
    let budget = i64::try_from(policy.max_rendition_mib * 1024 * 1024).unwrap();
    if running >= i64::try_from(policy.rendition_jobs).unwrap()
        || scratch.saturating_add(budget * 2)
            > i64::try_from(policy.rendition_scratch_mib * 1024 * 1024).unwrap()
    {
        return Ok(None);
    }
    let lease = id::next();
    let key = format!(
        "musicrend{lease}.{}",
        Format::parse(&row.format)?.extension()
    );
    music_assets::reserve_conn(state, &mut tx, owner, std::slice::from_ref(&key), &[budget])
        .await?;
    let cleanup: i64 = sqlx::query_scalar("SELECT id FROM media_cleanup_jobs WHERE file_name=$1")
        .bind(&key)
        .fetch_one(&mut *tx)
        .await
        .map_err(DbError::from)?;
    sqlx::query(
        "UPDATE media_cleanup_jobs SET run_at=now()+interval '3 minutes' WHERE file_name=$1",
    )
    .bind(&key)
    .execute(&mut *tx)
    .await
    .map_err(DbError::from)?;
    let claimed=sqlx::query_as("UPDATE music_renditions SET state='running',lease=$2,lease_until=now()+interval '2 minutes',file_name=$3,budget_bytes=$4,cleanup_job_id=$5,error=NULL,progress_dir=NULL,actual_bitrate=NULL,updated_at=now() WHERE id=$1 RETURNING *")
        .bind(job).bind(lease).bind(key).bind(budget).bind(cleanup).fetch_one(&mut *tx).await.map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(Some(claimed))
}
async fn heartbeat(state: &AppState, job: &Rendition) -> Result<(), ApiError> {
    let wanted:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM music_renditions r WHERE r.id=$1 AND (r.keep_running OR EXISTS(SELECT 1 FROM music_rendition_listeners l WHERE l.rendition_id=r.id AND l.lease_until>now())))")
        .bind(job.id).fetch_one(&state.pool).await.map_err(DbError::from)?;
    if !wanted {
        return Err(ApiError::Conflict("Playback listeners disconnected".into()));
    }
    let current = negotiate(
        state,
        job.requester_id,
        &job.format,
        Some(u32::try_from(job.bitrate).unwrap()),
    )
    .await?;
    if current.bitrate < u32::try_from(job.bitrate).unwrap() {
        return Err(ApiError::Conflict(
            "Rendition bitrate permission changed".into(),
        ));
    }
    let source = job_source(state, job).await?;
    if source.audio_hash != job.source_hash {
        return Err(ApiError::NotFound);
    }
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    let changed=sqlx::query("UPDATE music_renditions SET lease_until=now()+interval '2 minutes' WHERE id=$1 AND state='running' AND lease=$2 AND lease_until>now()")
        .bind(job.id).bind(job.lease).execute(&mut *tx).await.map_err(DbError::from)?.rows_affected();
    if changed != 1 {
        return Err(ApiError::Conflict(
            "Rendition cancelled or lease expired".into(),
        ));
    }
    sqlx::query("UPDATE media_cleanup_jobs SET run_at=now()+interval '3 minutes' WHERE file_name=$1 AND attempts=0")
        .bind(&job.file_name).execute(&mut *tx).await.map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
async fn process(
    state: &AppState,
    job: &Rendition,
    launcher: std::path::PathBuf,
) -> Result<(), ApiError> {
    let source = job_source(state, job).await?;
    let original = crate::music_original::process_input(state, &source).await?;
    let path = original.path.clone();
    let selection = Selection {
        format: Format::parse(&job.format)?,
        bitrate: u32::try_from(job.bitrate).unwrap(),
    };
    let selection = super::for_source(&state.config, &path, selection).await?;
    let directory = crate::media_transcode::scratch_dir(&state.config).await?;
    let name = directory
        .path()
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| ApiError::Internal("Invalid scratch directory".into()))?;
    let changed=sqlx::query("UPDATE music_renditions SET progress_dir=$3,actual_bitrate=$4 WHERE id=$1 AND lease=$2 AND state='running' AND lease_until>now()")
        .bind(job.id).bind(job.lease).bind(name).bind(i32::try_from(selection.bitrate).unwrap()).execute(&state.pool).await.map_err(DbError::from)?.rows_affected();
    if changed != 1 {
        return Err(ApiError::Conflict("Rendition lease expired".into()));
    }
    let limit = crate::media_limits::FileSizeLimit {
        launcher,
        bytes: u64::try_from(job.budget_bytes).unwrap(),
    };
    let encoded = {
        let encoding = super::encode_in(&state.config, &path, selection, &limit, directory);
        tokio::pin!(encoding);
        let mut clock = tokio::time::interval(std::time::Duration::from_secs(2));
        let timeout = tokio::time::sleep(std::time::Duration::from_mins(32));
        tokio::pin!(timeout);
        loop {
            tokio::select! {
                result=&mut encoding=>break result?,
                ()=state.shutdown.cancelled()=>return Err(ApiError::Conflict("Server stopping".into())),
                ()=&mut timeout=>return Err(ApiError::Unprocessable("Rendition timed out".into())),
                _=clock.tick()=>heartbeat(state,job).await?,
            }
        }
    };
    heartbeat(state, job).await?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, job.owner_id).await?;
    let source = job_source(state, job).await?;
    if source.audio_hash != job.source_hash {
        return Err(ApiError::NotFound);
    }
    let valid:Option<i64>=sqlx::query_scalar("SELECT id FROM music_renditions WHERE id=$1 AND state='running' AND lease=$2 AND lease_until>now() FOR UPDATE")
        .bind(job.id).bind(job.lease).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
    valid.ok_or_else(|| ApiError::Conflict("Rendition cancelled or lease expired".into()))?;
    let key = job.file_name.as_ref().expect("running job owns key");
    let journal:Option<i64>=sqlx::query_scalar("SELECT id FROM media_cleanup_jobs WHERE file_name=$1 AND attempts=0 AND run_at>now() FOR UPDATE")
        .bind(key).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
    journal.ok_or_else(|| ApiError::Conflict("Rendition storage reservation expired".into()))?;
    state
        .media
        .put_file(key, &encoded.file)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    sqlx::query("UPDATE music_renditions SET state='ready',size_bytes=$3,actual_bitrate=$4,lease=NULL,lease_until=NULL,progress_dir=NULL,updated_at=now() WHERE id=$1 AND lease=$2")
        .bind(job.id).bind(job.lease).bind(i64::try_from(encoded.bytes).unwrap()).bind(i32::try_from(encoded.selection.bitrate).unwrap())
        .execute(&mut *tx).await.map_err(DbError::from)?;
    sqlx::query("DELETE FROM media_cleanup_jobs WHERE file_name=$1")
        .bind(key)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
/// Execute one due job. Integration tests provide the actual CLI launcher.
pub async fn run_one(
    state: &AppState,
    job: i64,
    launcher: std::path::PathBuf,
) -> Result<bool, ApiError> {
    let claim = claim(state, job).await;
    let claimed = match claim {
        Ok(Some(row)) => row,
        Ok(None) => return Ok(false),
        Err(error) => {
            sqlx::query("UPDATE music_renditions SET state='failed',error=$2,lease=NULL,lease_until=NULL,updated_at=now() WHERE id=$1 AND (state='queued' OR lease_until<now())")
                .bind(job).bind(error.to_string()).execute(&state.pool).await.map_err(DbError::from)?;
            return Err(error);
        }
    };
    if let Err(error) = process(state, &claimed, launcher).await {
        let mut tx = state.pool.begin().await.map_err(DbError::from)?;
        sqlx::query("UPDATE music_renditions SET state='failed',error=$3,lease=NULL,lease_until=NULL,updated_at=now() WHERE id=$1 AND state='running' AND lease=$2")
            .bind(job).bind(claimed.lease).bind(error.to_string()).execute(&mut *tx).await.map_err(DbError::from)?;
        sqlx::query("UPDATE media_cleanup_jobs SET run_at=now() WHERE file_name=$1")
            .bind(&claimed.file_name)
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
        tx.commit().await.map_err(DbError::from)?;
        return Err(error);
    }
    Ok(true)
}
#[must_use]
pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let due:Result<Vec<i64>,_>=sqlx::query_scalar("SELECT id FROM music_renditions WHERE state='queued' OR (state='running' AND lease_until<now()) ORDER BY id LIMIT 20").fetch_all(&state.pool).await;
            match due {
                Ok(ids) => {
                    let mut tasks = tokio::task::JoinSet::new();
                    for job in ids {
                        if state.shutdown.is_cancelled() {
                            return;
                        }
                        let state = state.clone();
                        tasks.spawn(async move {
                            let launcher = crate::media_limits::running_executable()
                                .map_err(|e| ApiError::Internal(Box::new(e)))?;
                            run_one(&state, job, launcher).await
                        });
                    }
                    while let Some(result) = tasks.join_next().await {
                        if let Err(error) =
                            result.unwrap_or_else(|e| Err(ApiError::Internal(Box::new(e))))
                        {
                            tracing::warn!(error=%error.chain(),"Music rendition failed");
                        }
                    }
                }
                Err(error) => tracing::error!(%error,"Music rendition scan failed"),
            }
            if !crate::workers::pause(&state, std::time::Duration::from_secs(3)).await {
                break;
            }
        }
    })
}
