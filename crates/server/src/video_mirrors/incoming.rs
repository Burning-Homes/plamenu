//! Authenticated `CacheFile` registration. The mirror never grants source access.
use crate::{AppState, error::ApiError};
use plamenu_ap::{activity::Activity, cache_file::CacheFile};
use plamenu_db::{account::Account, media, video_distribution};

#[allow(
    clippy::too_many_lines,
    reason = "one ordered actor/source/consent registration boundary"
)]
pub async fn handle(
    state: &AppState,
    sender: &Account,
    activity: &Activity,
) -> Result<bool, ApiError> {
    if !matches!(activity.kind.as_str(), "Create" | "Update" | "Undo") {
        return Ok(false);
    }
    let undo = activity.kind == "Undo";
    let mut object = if undo && activity.object["type"] == "Create" {
        activity.object["object"].clone()
    } else {
        activity.object.clone()
    };
    if object.is_string() {
        if sender.actor_type.as_deref() != Some("Application")
            || !approved(&state.pool, sender).await?
        {
            return Ok(false);
        }
        let uri = object.as_str().unwrap();
        let requested = super::https(uri)?;
        let actor = super::https(sender.uri.as_deref().ok_or(ApiError::NotFound)?)?;
        if requested.origin() != actor.origin() {
            return Err(ApiError::Forbidden(
                "Mirror document is not owned by its actor".into(),
            ));
        }
        object = state
            .federation
            .fetch_object(uri)
            .await
            .map_err(|error| ApiError::Internal(Box::new(error)))?;
    }
    if object["type"] != "CacheFile" {
        return Ok(false);
    }
    let offer: CacheFile = serde_json::from_value(object.clone())
        .map_err(|_| ApiError::BadRequest("Invalid CacheFile object".into()))?;
    let actor = super::https(sender.uri.as_deref().ok_or(ApiError::NotFound)?)?;
    let offered = super::https(&offer.id)?;
    if offered.origin() != actor.origin() {
        return Err(ApiError::Forbidden(
            "CacheFile URI is owned by another actor".into(),
        ));
    }
    super::https(&offer.object)?;
    if super::https(&offer.url.href)?.origin() != actor.origin() {
        return Err(ApiError::Forbidden(
            "Mirror delivery URL is not owned by its actor".into(),
        ));
    }
    if undo {
        record_undo(state, sender, &offer.id).await?;
        return Ok(true);
    }
    if !approved(&state.pool, sender).await? {
        return Ok(true);
    }
    let expires = offer
        .bounded_expiry(time::OffsetDateTime::now_utc())
        .map_err(|error| ApiError::BadRequest(error.into()))?;
    let Some(post) = crate::ingest::resolve_status_ref(state, &offer.object).await? else {
        return Ok(true);
    };
    if !matches!(post.visibility.as_str(), "public" | "unlisted")
        || post.object_type.as_deref() != Some("Video")
    {
        return Ok(true);
    }
    let attachments = media::for_statuses(&state.pool, &[post.id]).await?;
    let Some(item) = attachments.get(&post.id).and_then(|items| {
        items
            .iter()
            .find(|item| item.kind_or_derived() == "video" && item.live_state.is_none())
    }) else {
        return Ok(true);
    };
    if media::public_access_blocked(&state.pool, item.id).await? {
        return Ok(true);
    }
    // Foreign PeerTube permission needs an explicit operator attestation. A
    // plain downloadable/public Video is not inferred to authorize mirrors.
    if !video_distribution::for_media(&state.pool, &[item.id])
        .await?
        .get(&item.id)
        .is_some_and(|policy| policy.mirror_allowed)
    {
        return Ok(true);
    }
    let canonical = object[super::CANONICAL_PLAYLISTS]
        .as_bool()
        .unwrap_or(false);
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    // Serialize each actor's capacity check with its insert, not just its URI.
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(sender.id)
        .execute(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?;
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM (SELECT uri FROM remote_video_mirrors WHERE actor_id=$1 UNION SELECT uri FROM remote_video_mirror_tombstones WHERE actor_id=$1) known")
            .bind(sender.id)
            .fetch_one(&mut *tx)
            .await
            .map_err(plamenu_db::DbError::from)?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM remote_video_mirrors WHERE uri=$1)")
            .bind(&offer.id)
            .fetch_one(&mut *tx)
            .await
            .map_err(plamenu_db::DbError::from)?;
    if count >= 256 && !exists {
        return Err(ApiError::ServiceUnavailable(
            "Mirror offer capacity reached".into(),
        ));
    }
    sqlx::query("INSERT INTO remote_video_mirrors(uri,actor_id,media_id,source_uri,playlist_url,expires_at,canonical_playlists)
        SELECT $1,$2,$3,$4,$5,$6,$7 WHERE NOT EXISTS(SELECT 1 FROM remote_video_mirror_tombstones WHERE uri=$1)
        ON CONFLICT(uri) DO UPDATE SET playlist_url=EXCLUDED.playlist_url,
        expires_at=EXCLUDED.expires_at,canonical_playlists=EXCLUDED.canonical_playlists,updated_at=clock_timestamp()
        WHERE remote_video_mirrors.actor_id=EXCLUDED.actor_id AND remote_video_mirrors.media_id=EXCLUDED.media_id
        AND remote_video_mirrors.source_uri=EXCLUDED.source_uri AND remote_video_mirrors.withdrawn_at IS NULL")
        .bind(&offer.id).bind(sender.id).bind(item.id).bind(&offer.object).bind(&offer.url.href).bind(expires).bind(canonical)
        .execute(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(true)
}

async fn record_undo(state: &AppState, sender: &Account, uri: &str) -> Result<(), ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(sender.id)
        .execute(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?;
    let owner:Option<i64>=sqlx::query_scalar("SELECT actor_id FROM remote_video_mirrors WHERE uri=$1 UNION SELECT actor_id FROM remote_video_mirror_tombstones WHERE uri=$1 LIMIT 1")
        .bind(uri).fetch_optional(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
    if owner.is_some_and(|id| id != sender.id) {
        return Err(ApiError::Forbidden(
            "Mirror withdrawal belongs to another actor".into(),
        ));
    }
    if owner.is_none() {
        if !approved(&mut *tx, sender).await? {
            return Ok(());
        }
        let count:i64=sqlx::query_scalar("SELECT count(*) FROM (SELECT uri FROM remote_video_mirrors WHERE actor_id=$1 UNION SELECT uri FROM remote_video_mirror_tombstones WHERE actor_id=$1) known")
            .bind(sender.id).fetch_one(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
        if count >= 256 {
            return Err(ApiError::ServiceUnavailable(
                "Mirror offer capacity reached".into(),
            ));
        }
    }
    sqlx::query("INSERT INTO remote_video_mirror_tombstones(uri,actor_id) VALUES($1,$2) ON CONFLICT(uri) DO NOTHING")
        .bind(uri).bind(sender.id).execute(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
    sqlx::query("UPDATE remote_video_mirrors SET withdrawn_at=clock_timestamp(),updated_at=clock_timestamp() WHERE uri=$1 AND actor_id=$2")
        .bind(uri).bind(sender.id).execute(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

async fn approved<'e>(
    executor: impl plamenu_db::PgExecutor<'e>,
    sender: &Account,
) -> Result<bool, ApiError> {
    if sender.suspended() || sender.actor_type.as_deref() != Some("Application") {
        return Ok(false);
    }
    let uri = super::https(sender.uri.as_deref().ok_or(ApiError::NotFound)?)?;
    Ok(plamenu_db::video_mirrors::approved(executor, uri.host_str().unwrap(), false).await?)
}
