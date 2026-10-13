//! Retained music and social Audio publications share one transactional outbox.
use crate::{
    AppState,
    actions::{self, DeleteMode, EditParams, PostParams},
    error::ApiError,
};
use plamenu_ap::activity::PostKind;
use plamenu_db::{
    DbError, PgConnection,
    account::{self, Account},
    id,
    media::{self, NewLocalMedia},
    status::Status,
};

/// Only a generated playback key can become an attachment reference.
#[must_use]
pub(crate) fn asset_from_key(key: &str) -> Option<i64> {
    let id = key
        .strip_prefix("music")?
        .strip_suffix(".mp3")?
        .parse::<i64>()
        .ok()?;
    (id > 0 && key == format!("music{id}.mp3")).then_some(id)
}
pub(crate) async fn link(
    conn: &mut PgConnection,
    asset: i64,
    status: i64,
    media: i64,
) -> Result<(), ApiError> {
    let inserted=sqlx::query("INSERT INTO music_publications(asset_id,status_id,media_id) SELECT t.id,s.id,m.id FROM music_assets t JOIN statuses s ON s.id=$2 AND s.account_id=t.owner_id AND s.object_type='Audio' JOIN media_attachments m ON m.id=$3 AND m.account_id=t.owner_id AND m.file_name=t.stream_file AND m.processing='complete' WHERE t.id=$1 AND t.visibility='public'")
        .bind(asset).bind(status).bind(media).execute(conn).await.map_err(DbError::from)?.rows_affected();
    if inserted != 1 {
        return Err(ApiError::Unprocessable(
            "Stored track is unavailable for publication".into(),
        ));
    }
    Ok(())
}
pub(crate) async fn unlink(conn: &mut PgConnection, status: i64) -> Result<(), DbError> {
    sqlx::query("DELETE FROM music_publications WHERE status_id=$1")
        .bind(status)
        .execute(conn)
        .await?;
    Ok(())
}
pub async fn published(
    state: &AppState,
    owner: i64,
    asset: i64,
) -> Result<Option<Status>, ApiError> {
    let id:Option<i64>=sqlx::query_scalar("SELECT p.status_id FROM music_publications p JOIN music_assets t ON t.id=p.asset_id JOIN statuses s ON s.id=p.status_id WHERE p.asset_id=$1 AND t.owner_id=$2 AND s.deleted_at IS NULL\n-- STUBFILTER: withdrawn publications are not active").bind(asset).bind(owner).fetch_optional(&state.pool).await.map_err(DbError::from)?;
    match id {
        Some(id) => Ok(plamenu_db::status::find_by_id(&state.pool, id).await?),
        None => Ok(None),
    }
}
pub async fn publish(state: &AppState, owner: &Account, asset: i64) -> Result<Status, ApiError> {
    crate::music_local::playback::prepare_publication(state, owner.id, asset).await?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    let version:Option<i64>=sqlx::query_scalar("SELECT revision FROM music_assets WHERE id=$1 AND owner_id=$2 AND visibility='public' FOR UPDATE").bind(asset).bind(owner.id).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
    version.ok_or_else(|| {
        ApiError::Unprocessable("Make the stored track public before publishing Audio".into())
    })?;
    let existing:Option<i64>=sqlx::query_scalar("SELECT p.status_id FROM music_publications p JOIN statuses s ON s.id=p.status_id WHERE p.asset_id=$1 AND s.deleted_at IS NULL\n-- STUBFILTER: only live publications satisfy idempotent publishing").bind(asset).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
    if let Some(existing) = existing {
        return plamenu_db::status::find_by_id(&mut *tx, existing)
            .await?
            .ok_or(ApiError::NotFound);
    }
    let asset = crate::music_assets::owned_conn(&mut tx, owner.id, asset).await?;
    let mut input = NewLocalMedia::new(owner.id, id::next(), &asset.stream_file, "audio/mpeg");
    input.kind = "audio";
    input.duration = Some(asset.duration);
    let media = media::create_local(&mut *tx, input).await?;
    media::set_file_sizes(&mut *tx, media.id, asset.stream_bytes, None).await?;
    let metadata = asset.metadata();
    let (status, _) = actions::post_music_audio_conn(
        state,
        &mut tx,
        PostParams {
            username: &owner.username,
            title: Some(&asset.title),
            text: &asset.description,
            content_type: crate::compose::PostFormat::from_media_type(&asset.description_type),
            visibility: "public",
            kind: PostKind::Audio,
            media_ids: &[media.id],
            music: Some(&metadata),
            ..Default::default()
        },
        media,
        asset.id,
    )
    .await?;
    tx.commit().await.map_err(DbError::from)?;
    after_create(state, &status).await;
    Ok(status)
}
async fn after_create(state: &AppState, status: &Status) {
    if let Err(error) = actions::notify_new_status_followers(state, status).await {
        tracing::warn!(%error,"music publication notification failed after commit");
    }
    crate::streaming::status_created(state, status.id).await;
    crate::webhooks::status_event(state, plamenu_db::webhook::STATUS_CREATED, status).await;
}
pub async fn withdraw(state: &AppState, owner: &Account, asset: i64) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    let locked: Option<i64> =
        sqlx::query_scalar("SELECT id FROM music_assets WHERE id=$1 AND owner_id=$2 FOR UPDATE")
            .bind(asset)
            .bind(owner.id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(DbError::from)?;
    locked.ok_or(ApiError::NotFound)?;
    let events = withdraw_conn(state, &mut tx, owner.id, asset).await?;
    tx.commit().await.map_err(DbError::from)?;
    publish_deletes(state, &events).await;
    Ok(())
}
pub(crate) async fn withdraw_conn(
    state: &AppState,
    conn: &mut PgConnection,
    owner: i64,
    asset: i64,
) -> Result<Vec<plamenu_db::streaming::Event>, ApiError> {
    let actor = account::find_by_id(&mut *conn, owner)
        .await?
        .ok_or(ApiError::NotFound)?;
    let status: Option<i64> =
        sqlx::query_scalar("SELECT status_id FROM music_publications WHERE asset_id=$1")
            .bind(asset)
            .fetch_optional(&mut *conn)
            .await
            .map_err(DbError::from)?;
    let mut events = Vec::new();
    if let Some(id) = status
        && let Some(stored) = plamenu_db::status::find_by_id(&mut *conn, id).await?
    {
        events
            .push(crate::streaming::prepare_delete_event_for_author(state, &stored, &actor).await?);
        actions::delete_status_conn(state, conn, &actor, id, DeleteMode::Stub).await?;
    }
    Ok(events)
}
pub(crate) async fn publish_deletes(state: &AppState, events: &[plamenu_db::streaming::Event]) {
    for event in events {
        crate::streaming::publish(state, event).await;
    }
}
pub(crate) async fn synchronize_conn(
    state: &AppState,
    conn: &mut PgConnection,
    owner: i64,
    asset: i64,
) -> Result<Option<Status>, ApiError> {
    let status: Option<i64> =
        sqlx::query_scalar("SELECT status_id FROM music_publications WHERE asset_id=$1")
            .bind(asset)
            .fetch_optional(&mut *conn)
            .await
            .map_err(DbError::from)?;
    let Some(status) = status else {
        return Ok(None);
    };
    let asset = crate::music_assets::owned_conn(conn, owner, asset).await?;
    let actor = account::find_by_id(&mut *conn, owner)
        .await?
        .ok_or(ApiError::NotFound)?;
    let updated = actions::edit_status_conn(
        state,
        conn,
        &actor,
        status,
        EditParams {
            title: Some(&asset.title),
            text: Some(&asset.description),
            content_type: Some(crate::compose::PostFormat::from_media_type(
                &asset.description_type,
            )),
            music: Some(asset.metadata()),
            ..Default::default()
        },
    )
    .await?;
    Ok(Some(updated))
}
pub(crate) async fn publish_update(state: &AppState, status: Option<&Status>) {
    if let Some(status) = status {
        if let Err(error) = actions::notify_status_edited(state, status).await {
            tracing::warn!(%error,"music edit notification failed after commit");
        }
        crate::streaming::status_edited(state, status.id).await;
        crate::webhooks::status_event(state, plamenu_db::webhook::STATUS_UPDATED, status).await;
    }
}
