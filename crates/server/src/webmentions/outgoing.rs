//! Source changes discover new targets and reverify previously delivered links.
use super::parse;
use crate::{AppState, error::ApiError};
use plamenu_db::{DbError, account, status};

pub async fn prepare(state: &AppState, id: i64, revision: i64) -> Result<(), ApiError> {
    let base = url::Url::parse(&format!("https://{}", state.config.domain))
        .map_err(|_| ApiError::NotFound)?;
    let links = match public_status(state, id).await {
        Ok((item, _)) => parse::targets(&item.content, &base),
        Err(ApiError::NotFound | ApiError::Gone) => Vec::new(),
        Err(e) => return Err(e),
    };
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    let current: Option<i64> = sqlx::query_scalar(
        "SELECT revision FROM webmention_status_jobs WHERE status_id=$1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::from)?;
    if current != Some(revision) {
        return Ok(());
    }
    // Old endpoints are retained so removed/private/deleted sources retract remotely.
    sqlx::query("UPDATE webmention_deliveries SET pending=true,attempts=0,revision=revision+1,next_attempt=now() WHERE status_id=$1").bind(id).execute(&mut *tx).await.map_err(DbError::from)?;
    for link in links {
        sqlx::query("INSERT INTO webmention_deliveries(status_id,target) VALUES($1,$2) ON CONFLICT(status_id,target) DO NOTHING").bind(id).bind(link).execute(&mut *tx).await.map_err(DbError::from)?;
    }
    sqlx::query("DELETE FROM webmention_status_jobs WHERE status_id=$1 AND revision=$2")
        .bind(id)
        .bind(revision)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
pub async fn public_status(
    state: &AppState,
    id: i64,
) -> Result<(status::Status, account::Account), ApiError> {
    let item = status::find_by_id(&state.pool, id)
        .await?
        .ok_or(ApiError::Gone)?;
    if status::is_deleted(&state.pool, id).await? {
        return Err(ApiError::Gone);
    }
    let author = account::find_by_id(&state.pool, item.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if item.uri.is_some()
        || item.reblog_of_id.is_some()
        || !author.is_local()
        || !matches!(item.visibility.as_str(), "public" | "unlisted")
        || !crate::instance_policy::public_account_visible(
            &state.pool,
            &state.config.domain,
            &author,
        )
        .await?
    {
        return Err(ApiError::NotFound);
    }
    let active:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts a WHERE a.id=$1 AND a.deleted_at IS NULL AND a.suspended_at IS NULL AND NOT EXISTS(SELECT 1 FROM users u WHERE u.account_id=a.id AND (NOT u.approved OR u.confirmed_at IS NULL)))").bind(author.id).fetch_one(&state.pool).await.map_err(DbError::from)?;
    if !active {
        return Err(ApiError::NotFound);
    }
    Ok((item, author))
}
#[derive(sqlx::FromRow)]
struct Delivery {
    status_id: i64,
    target: String,
    endpoint: Option<String>,
    attempts: i32,
    revision: i64,
}
pub async fn pass(state: &AppState) -> Result<(), ApiError> {
    let jobs:Vec<(i64,i64)>=sqlx::query_as("SELECT status_id,revision FROM webmention_status_jobs WHERE next_attempt<=now() ORDER BY next_attempt,status_id LIMIT 10").fetch_all(&state.pool).await.map_err(DbError::from)?;
    for (id, revision) in jobs {
        prepare(state, id, revision).await?;
    }
    let jobs:Vec<Delivery>=sqlx::query_as("SELECT * FROM webmention_deliveries WHERE pending AND next_attempt<=now() ORDER BY next_attempt,status_id,target LIMIT 5").fetch_all(&state.pool).await.map_err(DbError::from)?;
    for job in jobs {
        if state.shutdown.is_cancelled() {
            break;
        }
        let outcome = deliver(state, &job).await;
        let pending = outcome.is_err() && job.attempts < 5;
        if let Err(error) = outcome {
            tracing::debug!(error=%crate::error::ErrorChain(&error),"Outgoing Webmention deferred");
        }
        sqlx::query("UPDATE webmention_deliveries SET pending=$4,attempts=attempts+1,next_attempt=now()+make_interval(secs=>$5) WHERE status_id=$1 AND target=$2 AND revision=$3")
            .bind(job.status_id).bind(&job.target).bind(job.revision).bind(pending).bind(30.0*f64::from(1_i32<<job.attempts.min(5))).execute(&state.pool).await.map_err(DbError::from)?;
    }
    Ok(())
}
async fn deliver(
    state: &AppState,
    job: &Delivery,
) -> Result<(), plamenu_federation::FederationError> {
    use plamenu_federation::FederationError;
    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, &job.target)
        .await
        .map_err(|e| FederationError::Admission(e.to_string()))?
    {
        return Ok(());
    }
    let endpoint = if let Some(endpoint) = &job.endpoint {
        endpoint.clone()
    } else {
        let page = state.federation.fetch_mention_page(&job.target).await?;
        let base = url::Url::parse(&page.final_url)
            .map_err(|_| FederationError::InvalidUrl(page.final_url.clone()))?;
        let Some(endpoint) = parse::endpoint(&page.body, &page.links, &base) else {
            return Ok(());
        };
        sqlx::query("UPDATE webmention_deliveries SET endpoint=$4 WHERE status_id=$1 AND target=$2 AND revision=$3").bind(job.status_id).bind(&job.target).bind(job.revision).bind(&endpoint).execute(&state.pool).await.map_err(|e|FederationError::Admission(e.to_string()))?;
        endpoint
    };
    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, &endpoint)
        .await
        .map_err(|e| FederationError::Admission(e.to_string()))?
    {
        return Ok(());
    }
    let source = format!(
        "https://{}/webmentions/posts/{}",
        state.config.domain, job.status_id
    );
    state
        .federation
        .send_webmention(&endpoint, &source, &job.target)
        .await
}
