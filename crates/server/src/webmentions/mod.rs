//! Verified website interactions retain their own identity and durable jobs.
mod aliases;
pub mod interactions;
pub mod outgoing;
pub mod parse;
pub mod photo;
mod target;
pub mod worker;
use crate::{AppState, error::ApiError};
use plamenu_db::{DbError, id};
use serde_json::{Value, json};
use sqlx::types::Json;
use time::OffsetDateTime;
use url::Url;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Mention {
    pub id: i64,
    pub source: String,
    pub target: String,
    pub owner_id: i64,
    pub entry: Option<Json<parse::Entry>>,
    pub active: bool,
    pub announced: bool,
    pub photo_bytes: Option<Vec<u8>>,
    pub photo_type: Option<String>,
    pub notification_id: Option<i64>,
    pub revision: i64,
    pub state: String,
    pub attempts: i32,
    pub next_attempt: OffsetDateTime,
    pub lease: Option<i64>,
    pub lease_until: Option<OffsetDateTime>,
    pub received_at: OffsetDateTime,
    pub verified_at: Option<OffsetDateTime>,
}
impl Mention {
    #[must_use]
    pub fn entity(&self) -> Value {
        let mut value = json!({"id":self.id.to_string(),"source":self.source,"target":self.target,"entry":self.entry,"received_at":crate::entities::rfc3339(self.received_at).ok(),"verified_at":self.verified_at.and_then(|t|crate::entities::rfc3339(t).ok())});
        if value["entry"].is_object() {
            value["entry"]["photo"] = self
                .photo_bytes
                .as_ref()
                .map(|_| format!("/webmentions/{}/photo", self.id))
                .into();
        }
        value
    }
}
fn valid_url(raw: &str) -> bool {
    raw.len() <= 2048
        && !raw.chars().any(char::is_control)
        && Url::parse(raw).is_ok_and(|u| {
            matches!(u.scheme(), "http" | "https")
                && u.username().is_empty()
                && u.password().is_none()
                && u.fragment().is_none()
        })
}
pub async fn receive(state: &AppState, source: &str, target: &str) -> Result<i64, ApiError> {
    if !valid_url(source) || !valid_url(target) || source == target {
        return Err(ApiError::BadRequest(
            "Invalid Webmention source or target".into(),
        ));
    }
    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, source).await? {
        return Err(ApiError::Forbidden("Website is blocked".into()));
    }
    let owner = target::owner(state, target).await?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    // Serialize admission quotas across workers/processes, including repeat updates.
    sqlx::query("SELECT pg_advisory_xact_lock(175,1)")
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM webmentions WHERE state IN ('queued','running')")
            .fetch_one(&mut *tx)
            .await
            .map_err(DbError::from)?;
    let recent:i64=sqlx::query_scalar("SELECT count(*) FROM webmentions WHERE owner_id=$1 AND received_at>now()-interval '1 hour'").bind(owner).fetch_one(&mut *tx).await.map_err(DbError::from)?;
    if count >= 5000 || recent >= 500 {
        return Err(ApiError::TooManyRequests);
    }
    let mention=sqlx::query_scalar("INSERT INTO webmentions(id,source,target,owner_id) VALUES($1,$2,$3,$4) ON CONFLICT(source,target) DO UPDATE SET revision=webmentions.revision+1,state='queued',attempts=0,next_attempt=now(),lease=NULL,lease_until=NULL,received_at=now() RETURNING id")
        .bind(id::next()).bind(source).bind(target).bind(owner).fetch_one(&mut *tx).await.map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(mention)
}
pub(crate) async fn visible(state: &AppState, row: &Mention) -> Result<bool, ApiError> {
    if !row.active
        || row.entry.is_none()
        || !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, &row.source)
            .await?
    {
        return Ok(false);
    }
    match target::owner(state, &row.target).await {
        Ok(owner) => Ok(owner == row.owner_id),
        Err(ApiError::NotFound | ApiError::Gone) => Ok(false),
        Err(e) => Err(e),
    }
}

pub struct Page {
    pub items: Vec<Mention>,
    pub before: Option<i64>,
}
pub async fn page(
    state: &AppState,
    owner: Option<i64>,
    target: Option<&str>,
    mut before: Option<i64>,
) -> Result<Page, ApiError> {
    if owner.is_none() && target.is_none() {
        return Err(ApiError::BadRequest("Choose a Webmention target".into()));
    }
    let targets = if let Some(target) = target {
        Some(aliases::for_target(state, target).await?)
    } else {
        None
    };
    let mut items = Vec::new();
    // Bound each request even when moderation hides an entire old archive.
    // The cursor follows scanned rows so hidden records never trap pagination.
    for _ in 0..10 {
        let rows:Vec<Mention>=sqlx::query_as("SELECT * FROM webmentions WHERE active AND ($1::bigint IS NULL OR owner_id=$1) AND ($2::text[] IS NULL OR target=ANY($2)) AND ($3::bigint IS NULL OR id<$3) ORDER BY id DESC LIMIT 51")
            .bind(owner).bind(&targets).bind(before).fetch_all(&state.pool).await.map_err(DbError::from)?;
        let full = rows.len() == 51;
        for row in rows.into_iter().take(50) {
            before = Some(row.id);
            if visible(state, &row).await? {
                items.push(row);
                if items.len() == 50 {
                    return Ok(Page { items, before });
                }
            }
        }
        if !full {
            return Ok(Page {
                items,
                before: None,
            });
        }
    }
    Ok(Page { items, before })
}
pub async fn list(
    state: &AppState,
    owner: Option<i64>,
    target: Option<&str>,
    before: Option<i64>,
) -> Result<Vec<Mention>, ApiError> {
    Ok(page(state, owner, target, before).await?.items)
}
pub async fn notification_sources(
    state: &AppState,
    ids: &[i64],
    owner: i64,
) -> Result<std::collections::HashMap<i64, Value>, ApiError> {
    if ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let rows: Vec<Mention> = sqlx::query_as(
        "SELECT * FROM webmentions WHERE notification_id=ANY($1) AND owner_id=$2 AND active",
    )
    .bind(ids)
    .bind(owner)
    .fetch_all(&state.pool)
    .await
    .map_err(DbError::from)?;
    let mut sources = std::collections::HashMap::new();
    for row in rows {
        if visible(state, &row).await?
            && let Some(id) = row.notification_id
        {
            sources.insert(
                id,
                json!({"url":"/webmentions/archive","mention":row.entity()}),
            );
        }
    }
    Ok(sources)
}
