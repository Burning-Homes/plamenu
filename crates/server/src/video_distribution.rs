//! Current-authority, atomic native Video download/embed choices.
use crate::{AppState, error::ApiError};
use plamenu_db::{account, status, video_captions, video_distribution as db};
use serde_json::{Value, json};

pub async fn owned(
    state: &AppState,
    owner: i64,
    media: i64,
) -> Result<(db::Policy, Option<video_captions::OwnedPublication>), ApiError> {
    let publication = video_captions::owned_publication(&state.pool, media, owner).await?;
    if publication.is_none() {
        let unattached:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM media_attachments WHERE id=$1 AND account_id=$2 AND status_id IS NULL AND scheduled_status_id IS NULL AND remote_url IS NULL)").bind(media).bind(owner).fetch_one(&state.pool).await.map_err(plamenu_db::DbError::from)?;
        if !unattached {
            return Err(ApiError::NotFound);
        }
        let draft = plamenu_db::video_draft::find_owned(&state.pool, media, owner)
            .await?
            .ok_or(ApiError::NotFound)?;
        if plamenu_db::video_channel_access::role(&state.pool, draft.channel_id, owner)
            .await?
            .is_none()
        {
            return Err(ApiError::NotFound);
        }
        if draft.expires_at <= time::OffsetDateTime::now_utc()
            || !matches!(
                draft.state.as_str(),
                "receiving" | "queued" | "processing" | "ready" | "failed"
            )
        {
            return Err(ApiError::NotFound);
        }
    }
    let policy = db::for_media(&state.pool, &[media])
        .await?
        .remove(&media)
        .unwrap_or(db::Policy::default_for(media));
    Ok((policy, publication))
}

pub async fn save(
    state: &AppState,
    owner: i64,
    policy: db::Policy,
    may_edit_published: bool,
) -> Result<(), ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let published = video_captions::lock_owned_publication(&mut tx, policy.media_id, owner).await?;
    if published.is_some() && !may_edit_published {
        return Err(ApiError::Forbidden(
            "Published sharing choices require write:statuses".into(),
        ));
    }
    if published.is_none() {
        let draft = plamenu_db::video_draft::find_owned(&mut *tx, policy.media_id, owner)
            .await?
            .ok_or(ApiError::NotFound)?;
        if !plamenu_db::video::lock_owned_channel(&mut tx, draft.channel_id, owner).await?
            || !plamenu_db::video_metadata::lock_owned_draft(&mut tx, owner, policy.media_id)
                .await?
            || draft.expires_at <= time::OffsetDateTime::now_utc()
            || matches!(draft.state.as_str(), "cancelled" | "published" | "expired")
        {
            return Err(ApiError::NotFound);
        }
        // Check again after the media fence: concurrent scheduling/adoption
        // cannot silently carry a change made against a different state.
        let current = plamenu_db::video_draft::find_owned(&mut *tx, policy.media_id, owner)
            .await?
            .ok_or(ApiError::NotFound)?;
        if current.expires_at <= time::OffsetDateTime::now_utc()
            || matches!(
                current.state.as_str(),
                "cancelled" | "published" | "expired"
            )
        {
            return Err(ApiError::NotFound);
        }
    }
    let before = db::for_media(&mut *tx, &[policy.media_id])
        .await?
        .remove(&policy.media_id)
        .unwrap_or(db::Policy::default_for(policy.media_id));
    if before == policy {
        return Ok(());
    }
    db::upsert(&mut *tx, policy).await?;
    let mut edited = None;
    if let Some(publication) = published {
        let at = time::OffsetDateTime::now_utc();
        sqlx::query("UPDATE statuses SET edited_at=$2,updated_at=$2 WHERE id=$1")
            .bind(publication.status_id)
            .bind(at)
            .execute(&mut *tx)
            .await
            .map_err(plamenu_db::DbError::from)?;
        let item = status::find_by_id(&mut *tx, publication.status_id)
            .await?
            .ok_or(ApiError::NotFound)?;
        let actor = account::find_by_id(&mut *tx, publication.actor_id)
            .await?
            .ok_or(ApiError::NotFound)?;
        crate::actions::federate_note_update_conn(state, &mut tx, &actor, &item, at, Vec::new())
            .await?;
        edited = Some(item);
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    if let Some(item) = edited {
        crate::streaming::status_edited(state, item.id).await;
        crate::webhooks::status_event(state, plamenu_db::webhook::STATUS_UPDATED, &item).await;
    }
    Ok(())
}
pub fn enrich(object: &mut Value, policy: db::Policy) {
    // The choice is retained for recordings, but a waiting/on-air live stream
    // still has no finite downloadable file in PeerTube's Video contract.
    object["downloadEnabled"] = json!(policy.download_enabled && object["isLiveBroadcast"] != true);
    object[format!("{}mirrorAllowed", plamenu_ap::PLAMENU_NS)] =
        json!(policy.mirror_allowed && object["isLiveBroadcast"] != true);
}

#[cfg(test)]
mod tests {
    #[test]
    fn live_choices_do_not_advertise_a_finite_download_until_replay() {
        let mut live = serde_json::json!({"isLiveBroadcast":true});
        super::enrich(
            &mut live,
            plamenu_db::video_distribution::Policy::default_for(1),
        );
        assert_eq!(live["downloadEnabled"], false);
        live["isLiveBroadcast"] = serde_json::json!(false);
        super::enrich(
            &mut live,
            plamenu_db::video_distribution::Policy::default_for(1),
        );
        assert_eq!(live["downloadEnabled"], true);
    }

    #[test]
    fn mirrors_require_explicit_creator_consent_and_never_offer_live_recording_bytes() {
        let permission = format!("{}mirrorAllowed", plamenu_ap::PLAMENU_NS);
        let mut video = serde_json::json!({"isLiveBroadcast":false});
        let mut policy = plamenu_db::video_distribution::Policy::default_for(1);
        super::enrich(&mut video, policy);
        assert_eq!(video[&permission], false);
        policy.mirror_allowed = true;
        super::enrich(&mut video, policy);
        assert_eq!(video[&permission], true);
        video["isLiveBroadcast"] = serde_json::json!(true);
        super::enrich(&mut video, policy);
        assert_eq!(video[&permission], false);
    }
}
