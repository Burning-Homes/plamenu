//! Leases and revisions fence repeat deliveries; network never holds a transaction.
use super::{Mention, parse, target};
use crate::{AppState, error::ApiError};
use plamenu_db::{DbError, id};
use plamenu_federation::FederationError;
use sqlx::types::Json;
use std::time::Duration;

async fn claim(state: &AppState, mention: i64) -> Result<Option<Mention>, ApiError> {
    Ok(sqlx::query_as("UPDATE webmentions SET state='running',lease=$2,lease_until=now()+interval '2 minutes' WHERE id=$1 AND next_attempt<=now() AND (state='queued' OR (state='running' AND lease_until<now())) RETURNING *")
        .bind(mention).bind(id::next()).fetch_optional(&state.pool).await.map_err(DbError::from)?)
}
async fn verify(state: &AppState, row: &Mention) -> Result<Option<parse::Entry>, FederationError> {
    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, &row.source)
        .await
        .map_err(|e| FederationError::Admission(e.to_string()))?
    {
        return Ok(None);
    }
    let page = match state.federation.fetch_mention_page(&row.source).await {
        Ok(page) => page,
        Err(FederationError::Status(404 | 410)) => return Ok(None),
        Err(e) => return Err(e),
    };
    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, &page.final_url)
        .await
        .map_err(|e| FederationError::Admission(e.to_string()))?
    {
        return Ok(None);
    }
    if !matches!(
        page.content_type.split(';').next().unwrap_or("").trim(),
        "text/html" | "application/xhtml+xml"
    ) {
        return Ok(None);
    }
    let base = url::Url::parse(&page.final_url)
        .map_err(|_| FederationError::InvalidUrl(page.final_url.clone()))?;
    parse::entry(&page.body, &base, &row.target)
        .map_err(|_| FederationError::TooLarge(row.source.clone()))
}
pub async fn run(state: &AppState, mention: i64) -> Result<(), ApiError> {
    let Some(row) = claim(state, mention).await? else {
        return Ok(());
    };
    let result = verify(state, &row).await;
    let entry = match result {
        Ok(entry) => entry,
        Err(error) => {
            tracing::debug!(error=%crate::error::ErrorChain(&error),mention,"Webmention verification deferred");
            let retry = !matches!(
                error,
                FederationError::PrivateAddress(_)
                    | FederationError::InvalidUrl(_)
                    | FederationError::Admission(_)
            ) && row.attempts < 5;
            sqlx::query("UPDATE webmentions SET state=$4,attempts=attempts+1,next_attempt=now()+make_interval(secs=>$5),lease=NULL,lease_until=NULL WHERE id=$1 AND revision=$2 AND lease=$3")
                .bind(row.id).bind(row.revision).bind(row.lease).bind(if retry{"queued"}else{"failed"}).bind(30.0*f64::from(1_i32<<row.attempts.min(5))).execute(&state.pool).await.map_err(DbError::from)?;
            return Ok(());
        }
    };
    let entry = match target::owner(state, &row.target).await {
        Ok(owner) if owner == row.owner_id => entry,
        Ok(_) | Err(ApiError::NotFound | ApiError::Gone) => None,
        Err(e) => return Err(e),
    };
    let photo = if let Some(entry) = &entry {
        super::photo::cache(state, entry.photo.as_deref()).await
    } else {
        None
    };
    let self_mention = if let Some(author) = entry.as_ref().and_then(|e| e.author_url.as_deref()) {
        let owner = plamenu_db::account::find_by_id(&state.pool, row.owner_id)
            .await?
            .ok_or(ApiError::NotFound)?;
        [
            crate::entities::account_uri(&state.config.domain, &owner),
            crate::entities::account_web_url(&state.config.domain, &owner),
        ]
        .iter()
        .any(|uri| uri.trim_end_matches('/') == author.trim_end_matches('/'))
    } else {
        false
    };
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    let current:Option<Mention>=sqlx::query_as("SELECT * FROM webmentions WHERE id=$1 AND revision=$2 AND lease=$3 AND lease_until>now() FOR UPDATE")
        .bind(row.id).bind(row.revision).bind(row.lease).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
    let Some(current) = current else {
        return Ok(());
    };
    let mut notification = current.notification_id;
    if entry.is_none() {
        if let Some(id) = notification.take() {
            sqlx::query(
                "DELETE FROM notifications WHERE id=$1 AND account_id=$2 AND kind='webmention'",
            )
            .bind(id)
            .bind(row.owner_id)
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
        }
    } else if notification.is_none() && !current.announced && !self_mention {
        let next = id::next();
        sqlx::query("INSERT INTO notifications(id,account_id,from_account_id,kind,reasons) VALUES($1,$2,$2,'webmention',ARRAY['webmention'])").bind(next).bind(row.owner_id).execute(&mut *tx).await.map_err(DbError::from)?;
        notification = Some(next);
    }
    sqlx::query("UPDATE webmentions SET entry=$2,active=$3,announced=announced OR $3,notification_id=$4,photo_bytes=$5,photo_type=$6,state='done',verified_at=now(),lease=NULL,lease_until=NULL WHERE id=$1")
        .bind(row.id).bind(entry.as_ref().map(Json)).bind(entry.is_some()).bind(notification).bind(photo.as_ref().map(|p|&p.0)).bind(photo.as_ref().map(|p|p.1.as_str())).execute(&mut *tx).await.map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
#[must_use]
pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                            ()=state.shutdown.cancelled()=>break,
                            _=tick.tick()=>{
                                let rows=sqlx::query_scalar::<_,i64>("SELECT id FROM webmentions WHERE next_attempt<=now() AND (state='queued' OR (state='running' AND lease_until<now())) ORDER BY next_attempt,id LIMIT 5").fetch_all(&state.pool).await;
                                if let Err(e)=super::outgoing::pass(&state).await {tracing::warn!(error=%e.chain(),"Outgoing Webmention worker failed");}
                                match rows {Ok(ids)=>for id in ids {if state.shutdown.is_cancelled(){break;}
            if let Err(e)=run(&state,id).await {tracing::warn!(error=%e.chain(),"Webmention worker failed");}},Err(e)=>tracing::warn!(error=%e,"Webmention queue unavailable")}
                            }
                        }
        }
    })
}
