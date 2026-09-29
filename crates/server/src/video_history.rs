//! Viewing history never widens access and never eagerly fetches remote media.
use crate::{AppState, entities, error::ApiError};
use plamenu_db::{media, status, video_history as db};
use serde_json::Value;

async fn visible_ids(state: &AppState, viewer: i64, ids: &[i64]) -> Result<Vec<i64>, ApiError> {
    Ok(sqlx::query_scalar("SELECT DISTINCT s.id FROM statuses s JOIN media_attachments m ON m.status_id=s.id LEFT JOIN video_publications v ON v.status_id=s.id AND v.media_id=m.id LEFT JOIN remote_video_channels r ON r.status_id=s.id LEFT JOIN video_channels c ON c.account_id=v.channel_id WHERE s.id=ANY($1) AND s.deleted_at IS NULL AND s.object_type='Video' AND NOT account_hidden($2,s.account_id) AND NOT account_hidden($2,COALESCE(v.channel_id,r.channel_id,s.account_id)) AND NOT account_hidden($2,COALESCE(c.owner_account_id,m.account_id))")
        .bind(ids).bind(viewer).fetch_all(&state.pool).await.map_err(plamenu_db::DbError::from)?)
}

pub async fn readable(
    state: &AppState,
    viewer: i64,
    media_id: i64,
) -> Result<(media::Media, status::Status), ApiError> {
    let m = media::find_by_ids(&state.pool, &[media_id])
        .await?
        .pop()
        .ok_or(ApiError::NotFound)?;
    if m.kind_or_derived() != "video"
        || m.live_state.is_some()
        || m.not_processed() && !m.download_on_demand
    {
        return Err(ApiError::NotFound);
    }
    let s = status::find_by_id(&state.pool, m.status_id.ok_or(ApiError::NotFound)?)
        .await?
        .ok_or(ApiError::NotFound)?;
    if s.object_type.as_deref() != Some("Video")
        || !entities::can_view(&state.pool, &s, Some(viewer)).await?
        || media::public_access_blocked(&state.pool, media_id).await?
        || visible_ids(state, viewer, &[s.id]).await?.is_empty()
    {
        return Err(ApiError::NotFound);
    }
    Ok((m, s))
}

/// One bounded history page, batch-loaded and re-authorized against today's
/// permissions. Stored positions never constitute an access capability.
pub async fn list(
    state: &AppState,
    viewer: i64,
    before: Option<time::OffsetDateTime>,
    before_media: i64,
) -> Result<(Vec<Value>, Option<db::Entry>), ApiError> {
    let mut entries = db::page(&state.pool, viewer, before, before_media).await?;
    let more = entries.len() > 20;
    entries.truncate(20);
    let cursor = more.then(|| entries.last().cloned()).flatten();
    let ids = entries.iter().map(|e| e.media_id).collect::<Vec<_>>();
    let media = media::find_by_ids(&state.pool, &ids).await?;
    let status_ids = media.iter().filter_map(|m| m.status_id).collect::<Vec<_>>();
    // Unlike stub-preserving context fetches, only live status rows may enter
    // a private history response. No deleted title or old media URL survives.
    let live_ids = visible_ids(state, viewer, &status_ids).await?;
    let statuses = status::find_by_ids(&state.pool, &live_ids).await?;
    let statuses = entities::filter_viewable(&state.pool, &statuses, Some(viewer)).await?;
    let values =
        entities::render_statuses(&state.pool, &state.config.domain, &statuses, Some(viewer))
            .await?;
    let mut result = Vec::new();
    for e in entries {
        let Some(m) = media.iter().find(|m| {
            m.id == e.media_id && m.kind_or_derived() == "video" && m.live_state.is_none()
        }) else {
            continue;
        };
        let Some(s) = values
            .iter()
            .find(|s| s["id"].as_str().and_then(|s| s.parse().ok()) == m.status_id)
        else {
            continue;
        };
        let attachment = s["media_attachments"].as_array().and_then(|items| {
            items
                .iter()
                .find(|m| m["id"].as_str().and_then(|s| s.parse().ok()) == Some(e.media_id))
        });
        let Some(attachment) = attachment else {
            continue;
        };
        let url = attachment["url"].as_str().unwrap_or_default();
        let permalink = format!("/web/statuses/{}", m.status_id.unwrap());
        result.push(serde_json::json!({"media_id":e.media_id.to_string(),"status_id":m.status_id.unwrap().to_string(),"title":s["title"],"position_seconds":e.position_seconds,"completed":e.completed,"updated_at":e.updated_at.format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),"permalink":permalink,"resume_url":if url.is_empty(){None}else{Some(format!("{url}#t={}",e.position_seconds))}}));
    }
    Ok((result, cursor))
}

pub async fn save(
    state: &AppState,
    viewer: i64,
    media_id: i64,
    seconds: i32,
    completed: bool,
) -> Result<bool, ApiError> {
    let (m, _) = readable(state, viewer, media_id).await?;
    if !(0..=604_800).contains(&seconds)
        || m.duration
            .is_some_and(|d| d.is_finite() && f64::from(seconds) > d.ceil())
    {
        return Err(ApiError::Unprocessable(
            "Position must be within this video's duration".into(),
        ));
    }
    // Completion is explicit, and must be within the final ten seconds when
    // duration is known; do not let an arbitrary seek mark a long film watched.
    if completed
        && m.duration
            .is_some_and(|d| d.is_finite() && f64::from(seconds) < (d - 10.0).max(0.0))
    {
        return Err(ApiError::Unprocessable(
            "Complete playback before marking it watched".into(),
        ));
    }
    Ok(db::save(&state.pool, viewer, media_id, seconds, completed).await?)
}

/// First-party focus page only: opt-in controls are not leaked to anonymous
/// entities, embeds, federation or another user's API response.
pub async fn annotate(state: &AppState, viewer: i64, entity: &mut Value) -> Result<(), ApiError> {
    let Some(items) = entity
        .get_mut("media_attachments")
        .and_then(Value::as_array_mut)
    else {
        return Ok(());
    };
    if !db::enabled(&state.pool, viewer).await? {
        return Ok(());
    }
    let ids: Vec<i64> = items
        .iter()
        .filter_map(|m| m["id"].as_str()?.parse().ok())
        .collect();
    let positions = db::for_media(&state.pool, viewer, &ids).await?;
    for item in items {
        if item["type"] != "video" || item.get("live").is_some() {
            continue;
        }
        let id = item["id"].as_str().and_then(|s| s.parse::<i64>().ok());
        let found = positions.iter().find(|e| Some(e.media_id) == id);
        item["viewer_history"] = serde_json::json!({"position_seconds":found.map_or(0,|e|e.position_seconds), "completed":found.is_some_and(|e|e.completed)});
    }
    Ok(())
}
