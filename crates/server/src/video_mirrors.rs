//! Optional, native federation mirrors. Consent is not playback authority.
pub mod http;
pub mod incoming;
pub mod manifest;
pub mod source;
pub mod worker;

use crate::{AppState, error::ApiError};
use plamenu_db::{media, status, video_mirrors as db};
use time::OffsetDateTime;
use url::Url;

pub const CANONICAL_PLAYLISTS: &str = "https://codefloe.com/plamenu/plamenu/ns#canonicalPlaylists";

/// Withdrawal and the signed Undo share one transaction. The private asset
/// retirement trigger retains physical debt until both copies acknowledge.
pub async fn withdraw(state: &AppState, id: i64) -> Result<(), ApiError> {
    let item = db::find(&state.pool, id).await?.ok_or(ApiError::NotFound)?;
    let source = status::find_by_id(&state.pool, item.status_id).await?;
    let target = if let Some(source) = source {
        plamenu_db::account::find_by_id(&state.pool, source.account_id).await?
    } else {
        None
    };
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    if let Some(create) = db::withdraw_conn(&mut tx, id).await?
        && let Some(target) = target
    {
        if let Some(uri) = create["id"].as_str() {
            // Do not leave an unsent/retry Create queued behind its Undo.
            plamenu_db::job::cancel(&mut *tx, target.preferred_inbox(), uri).await?;
        }
        let actor = format!("https://{}/actor", state.config.domain);
        let undo = serde_json::json!({"@context":cache_context(),"type":"Undo",
                "id":format!("https://{}/cachefiles/{id}/undo",state.config.domain),"actor":actor,"object":create});
        plamenu_db::job::enqueue_many_from_instance(
            &mut *tx,
            &[target.preferred_inbox().to_owned()],
            &undo,
        )
        .await?;
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

#[must_use]
pub fn cache_context() -> serde_json::Value {
    serde_json::json!([plamenu_ap::AS_CONTEXT,{
        "CacheFile":"https://joinpeertube.org/ns#CacheFile",
        "expires":"https://schema.org/expires"
    }])
}

fn https(value: &str) -> Result<Url, ApiError> {
    let url =
        Url::parse(value).map_err(|_| ApiError::BadRequest("Invalid mirror source URI".into()))?;
    if value.len() > 2048
        || url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(ApiError::BadRequest(
            "Mirrors require credential-free HTTPS URIs".into(),
        ));
    }
    Ok(url)
}

/// Exact approved domains only. This does not override instance blocking or
/// the outbound DNS/IP/redirect guard, and grants no creator permission.
pub async fn approve(
    state: &AppState,
    operator: i64,
    domain: &str,
    source: bool,
    mirror: bool,
) -> Result<(), ApiError> {
    let domain = crate::instance_policy::normalize_domain(Some(domain))?;
    sqlx::query("INSERT INTO video_mirror_approvals(domain,allow_source,allow_mirror,approved_by)
        VALUES($1,$2,$3,$4) ON CONFLICT(domain) DO UPDATE SET allow_source=EXCLUDED.allow_source,
        allow_mirror=EXCLUDED.allow_mirror,approved_by=EXCLUDED.approved_by,updated_at=clock_timestamp()")
        .bind(domain).bind(source).bind(mirror).bind(operator).execute(&state.pool).await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

/// Operator-authorized, explicitly consented public VOD only. The attestation
/// is private audit evidence for external creators that lack our permission
/// vocabulary; downloadEnabled is deliberately NOT treated as consent.
pub async fn request(
    state: &AppState,
    operator: i64,
    status_id: i64,
    attestation: &str,
) -> Result<db::Mirror, ApiError> {
    let config = state
        .config
        .video_redundancy
        .ok_or_else(|| ApiError::Forbidden("Federated mirrors are not enabled".into()))?;
    if attestation.len() > 2000 {
        return Err(ApiError::BadRequest(
            "Creator consent attestation is too long".into(),
        ));
    }
    let post = status::find_by_id(&state.pool, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !matches!(post.visibility.as_str(), "public" | "unlisted")
        || post.object_type.as_deref() != Some("Video")
    {
        return Err(ApiError::NotFound);
    }
    let source_uri = post.uri.as_deref().ok_or(ApiError::NotFound)?;
    let source = https(source_uri)?;
    if !db::approved(&state.pool, source.host_str().unwrap(), true).await? {
        return Err(ApiError::Forbidden(
            "Source instance has not been approved for mirrors".into(),
        ));
    }
    let attachments = media::for_statuses(&state.pool, &[status_id]).await?;
    let item = attachments
        .get(&status_id)
        .and_then(|items| {
            items.iter().find(|item| {
                item.hls_master_url.is_some()
                    && item.live_state.is_none()
                    && item.remote_url.is_some()
            })
        })
        .ok_or(ApiError::NotFound)?;
    if media::public_access_blocked(&state.pool, item.id).await? {
        return Err(ApiError::NotFound);
    }
    let permitted = plamenu_db::video_distribution::for_media(&state.pool, &[item.id])
        .await?
        .get(&item.id)
        .is_some_and(|policy| policy.mirror_allowed);
    if !permitted && attestation.trim().len() < 20 {
        return Err(ApiError::Forbidden(
            "Explicit creator permission or an operator's creator-consent attestation is required"
                .into(),
        ));
    }
    let master = item.hls_master_url.as_deref().unwrap();
    let master_url = https(master)?;
    if !db::approved(&state.pool, master_url.host_str().unwrap(), true).await? {
        return Err(ApiError::Forbidden(
            "Media source domain has not been approved for mirrors".into(),
        ));
    }
    let updated: OffsetDateTime = sqlx::query_scalar("SELECT updated_at FROM statuses WHERE id=$1")
        .bind(status_id)
        .fetch_one(&state.pool)
        .await
        .map_err(plamenu_db::DbError::from)?;
    db::enqueue(
        &state.pool,
        db::Request {
            media_id: item.id,
            status_id,
            requested_by: operator,
            source_uri,
            source_master_url: master,
            source_updated_at: updated,
            consent_attestation: attestation.trim(),
            expires_at: OffsetDateTime::now_utc()
                + time::Duration::hours(i64::try_from(config.retention_hours).unwrap_or(24)),
            reserved_bytes: i64::try_from(config.max_mirror_mib * 1024 * 1024).unwrap_or(i64::MAX),
        },
        i64::try_from(config.quota_gib * 1024 * 1024 * 1024).unwrap_or(i64::MAX),
    )
    .await?
    .ok_or_else(|| {
        ApiError::Conflict(
            "Mirror quota is reserved or this video already has an active mirror".into(),
        )
    })
}

pub async fn authorized(state: &AppState, item: &db::Mirror) -> Result<bool, ApiError> {
    let mut connection = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    authorized_conn(state, &mut connection, item).await
}

/// Reuse the owned writer's transaction connection. Acquiring another pool
/// connection while holding its journal/job locks deadlocks a one-slot pool.
pub(crate) async fn authorized_conn(
    state: &AppState,
    connection: &mut sqlx::PgConnection,
    item: &db::Mirror,
) -> Result<bool, ApiError> {
    if state.config.video_redundancy.is_none()
        || item.expires_at <= OffsetDateTime::now_utc()
        || !matches!(item.state.as_str(), "queued" | "copying" | "ready")
        || media::public_access_blocked(&mut *connection, item.media_id).await?
    {
        return Ok(false);
    }
    // A canonical source Update invalidates the copied snapshot, even when
    // its master URL is reused. Never advertise an old immutable generation
    // as a replacement for newly published content.
    let source = https(&item.source_uri)?;
    let master = https(&item.source_master_url)?;
    sqlx::query_scalar(
        "SELECT EXISTS(
        SELECT 1 FROM media_attachments m JOIN statuses s ON s.id=m.status_id
        LEFT JOIN video_distribution d ON d.media_id=m.id
        WHERE m.id=$1 AND s.id=$2 AND s.updated_at=$3 AND s.uri=$4
          AND s.object_type='Video' AND s.visibility IN ('public','unlisted')
          AND m.hls_master_url=$5 AND m.live_state IS NULL
          AND EXISTS(SELECT 1 FROM video_mirror_approvals WHERE domain=$6 AND allow_source)
          AND EXISTS(SELECT 1 FROM video_mirror_approvals WHERE domain=$7 AND allow_source)
          AND ($7=$8 OR instance_domain_allowed($7))
          AND NOT instance_domain_rejects_media($7)
          AND ($9 OR COALESCE(d.mirror_allowed,false)))",
    )
    .bind(item.media_id)
    .bind(item.status_id)
    .bind(item.source_updated_at)
    .bind(&item.source_uri)
    .bind(&item.source_master_url)
    .bind(source.host_str().unwrap())
    .bind(master.host_str().unwrap())
    .bind(&state.config.domain)
    .bind(item.consent_attestation.trim().len() >= 20)
    .fetch_one(connection)
    .await
    .map_err(plamenu_db::DbError::from)
    .map_err(ApiError::from)
}
