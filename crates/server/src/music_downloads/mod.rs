//! Private, bounded ZIP jobs use current music source permissions throughout.
mod content;
pub mod file;
mod records;
mod selection;
pub mod worker;
mod writer;
use crate::{AppState, error::ApiError, music_assets, music_queues};
use plamenu_db::{DbError, id};
pub use selection::Request;
use serde_json::{Value, json};
use sqlx::types::Json;
use time::OffsetDateTime;
fn internal<E: std::error::Error + Send + Sync + 'static>(error: E) -> ApiError {
    ApiError::Internal(Box::new(error))
}
#[derive(Clone, sqlx::FromRow)]
pub struct Download {
    pub id: i64,
    pub owner_id: i64,
    pub label: String,
    #[sqlx(try_from = "selection::Selection")]
    selection: Json<selection::Selection>,
    pub state: String,
    pub revision: i64,
    pub completed_items: i32,
    pub included: Vec<i32>,
    #[sqlx(try_from = "records::Errors")]
    pub item_errors: Value,
    pub error: Option<String>,
    pub notification_id: Option<i64>,
    pub lease: Option<i64>,
    pub lease_until: Option<OffsetDateTime>,
    pub file_name: Option<String>,
    pub size_bytes: i64,
    pub budget_bytes: i64,
    pub cleanup_job_id: Option<i64>,
    pub created_at: OffsetDateTime,
    pub completed_at: Option<OffsetDateTime>,
    pub expires_at: OffsetDateTime,
}
impl Download {
    async fn container_available(&self, state: &AppState) -> Result<bool, ApiError> {
        if let Some(parent) = &self.selection.parent {
            match selection::parent(state, self.owner_id, parent).await {
                Ok(()) => Ok(true),
                Err(ApiError::NotFound) => Ok(false),
                Err(error) => Err(error),
            }
        } else {
            Ok(true)
        }
    }
    pub async fn display_label(&self, state: &AppState) -> Result<&str, ApiError> {
        Ok(if self.container_available(state).await? {
            &self.label
        } else {
            "Music download"
        })
    }
    pub async fn entity(&self, state: &AppState) -> Result<Value, ApiError> {
        let container = self.container_available(state).await?;
        let mut items = Vec::new();
        for (index, item) in self.selection.items.iter().enumerate() {
            let source = if container {
                match selection::playable(state, self.owner_id, item).await {
                    Ok(source) => Some(source),
                    Err(ApiError::NotFound) => None,
                    Err(error) => return Err(error),
                }
            } else {
                None
            };
            items.push(json!({"index":index,"source":source}));
        }
        Ok(
            json!({"id":self.id.to_string(),"label":if container {self.label.as_str()}else{"Music download"},"state":self.state,"revision":self.revision.to_string(),"completed_items":self.completed_items,"item_count":items.len(),"items":items,"item_errors":self.item_errors,"error":self.error,"size":self.size_bytes,"expires_at":crate::entities::rfc3339(self.expires_at)?,"download_url":(self.state=="ready").then(||format!("/music/downloads/{}/file",self.id))}),
        )
    }
}
pub async fn get(state: &AppState, owner: i64, download: i64) -> Result<Download, ApiError> {
    let mut connection = state.pool.acquire().await.map_err(DbError::from)?;
    music_queues::active_owner(&mut connection, owner).await?;
    sqlx::query_as("SELECT * FROM music_downloads WHERE id=$1 AND owner_id=$2 AND expires_at>now()")
        .bind(download)
        .bind(owner)
        .fetch_optional(&mut *connection)
        .await
        .map_err(DbError::from)?
        .ok_or(ApiError::NotFound)
}
pub async fn list(
    state: &AppState,
    owner: i64,
    before: Option<i64>,
) -> Result<Vec<Download>, ApiError> {
    if before.is_some_and(|id| id <= 0) {
        return Err(ApiError::Unprocessable("Invalid download cursor".into()));
    }
    let mut connection = state.pool.acquire().await.map_err(DbError::from)?;
    music_queues::active_owner(&mut connection, owner).await?;
    Ok(sqlx::query_as("SELECT * FROM music_downloads WHERE owner_id=$1 AND expires_at>now() AND ($2::bigint IS NULL OR id<$2) ORDER BY id DESC LIMIT 51")
        .bind(owner).bind(before).fetch_all(&mut *connection).await.map_err(DbError::from)?)
}
pub async fn create(state: &AppState, owner: i64, input: &Request) -> Result<Download, ApiError> {
    let (selection, label) = selection::resolve(state, owner, input).await?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, owner).await?;
    music_queues::active_owner(&mut tx, owner).await?;
    let (active,total):(i64,i64)=sqlx::query_as("SELECT count(*) FILTER(WHERE state IN ('queued','running')),count(*) FROM music_downloads WHERE owner_id=$1 AND expires_at>now()")
        .bind(owner).fetch_one(&mut *tx).await.map_err(DbError::from)?;
    if active >= 2 || total >= 100 {
        return Err(ApiError::Unprocessable(
            "Music download limit reached; remove old downloads or wait for active jobs".into(),
        ));
    }
    let expires = OffsetDateTime::now_utc()
        + time::Duration::days(i64::from(state.config.music.download_retention_days));
    let row=sqlx::query_as("INSERT INTO music_downloads(id,owner_id,label,selection,expires_at) VALUES($1,$2,$3,$4,$5) RETURNING *")
        .bind(id::next()).bind(owner).bind(label).bind(selection).bind(expires).fetch_one(&mut *tx).await.map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(row)
}
pub async fn remove(
    state: &AppState,
    owner: i64,
    download: i64,
    revision: i64,
    cancel: bool,
) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, owner).await?;
    music_queues::active_owner(&mut tx, owner).await?;
    let row: Download =
        sqlx::query_as("SELECT * FROM music_downloads WHERE id=$1 AND owner_id=$2 FOR UPDATE")
            .bind(download)
            .bind(owner)
            .fetch_optional(&mut *tx)
            .await
            .map_err(DbError::from)?
            .ok_or(ApiError::NotFound)?;
    if row.revision != revision {
        return Err(ApiError::Conflict(
            "Music download changed; reload the page".into(),
        ));
    }
    if cancel {
        if !matches!(row.state.as_str(), "queued" | "running") {
            return Err(ApiError::Conflict(
                "This download has already finished".into(),
            ));
        }
        sqlx::query("UPDATE music_downloads SET state='cancelled',revision=revision+1,lease=NULL,lease_until=NULL WHERE id=$1")
            .bind(download).execute(&mut *tx).await.map_err(DbError::from)?;
        sqlx::query("UPDATE media_cleanup_jobs SET run_at=now() WHERE file_name=$1")
            .bind(row.file_name)
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
    } else {
        sqlx::query("DELETE FROM music_downloads WHERE id=$1")
            .bind(download)
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
    }
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
pub async fn authorize_file(
    state: &AppState,
    owner: i64,
    download: i64,
) -> Result<Download, ApiError> {
    let row = get(state, owner, download).await?;
    if row.state != "ready" || row.file_name.is_none() {
        return Err(ApiError::NotFound);
    }
    if let Some(parent) = &row.selection.parent {
        selection::parent(state, owner, parent).await?;
    }
    for index in &row.included {
        let item = usize::try_from(*index)
            .ok()
            .and_then(|index| row.selection.items.get(index))
            .ok_or(ApiError::NotFound)?;
        selection::playable(state, owner, item).await?;
    }
    Ok(row)
}

pub async fn preview(
    state: &AppState,
    owner: i64,
    input: &Request,
) -> Result<(String, Vec<crate::music_collections::Playable>), ApiError> {
    let (selection, label) = selection::resolve(state, owner, input).await?;
    let mut items = Vec::new();
    for item in &selection.items {
        items.push(selection::playable(state, owner, item).await?);
    }
    Ok((label, items))
}

pub(crate) async fn notification_sources(
    pool: &sqlx::PgPool,
    ids: &[i64],
    viewer: i64,
) -> Result<std::collections::HashMap<i64, Value>, ApiError> {
    let mut result = std::collections::HashMap::new();
    for page in ids.chunks(100) {
        let rows:Vec<(i64,i64)>=sqlx::query_as("SELECT d.notification_id,d.id FROM music_downloads d JOIN accounts a ON a.id=d.owner_id JOIN notifications n ON n.id=d.notification_id WHERE d.notification_id=ANY($1) AND d.owner_id=$2 AND n.account_id=$2 AND n.kind='music.download' AND d.state='ready' AND d.expires_at>now() AND a.deleted_at IS NULL AND a.suspended_at IS NULL AND NOT EXISTS(SELECT 1 FROM users u WHERE u.account_id=a.id AND (NOT u.approved OR u.confirmed_at IS NULL))")
            .bind(page).bind(viewer).fetch_all(pool).await.map_err(DbError::from)?;
        for (notification, id) in rows {
            result.insert(notification,json!({"kind":"download","id":id.to_string(),"name":"Music download","url":format!("/music/downloads/{id}")}));
        }
    }
    Ok(result)
}
