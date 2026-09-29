//! Adopt a fully processed private candidate without changing social or media
//! identity. No encoding, file copying or network I/O occurs under these locks.
//! Old keys stay charged until acknowledged cleanup; failed transactions leave
//! both the publication and the private candidate unchanged.
use crate::{AppState, error::ApiError};
use plamenu_db::{media_cleanup, video_captions, video_storage};
use sqlx::FromRow;

#[derive(FromRow)]
pub struct Editable {
    pub media_id: i64,
    pub status_id: i64,
    pub title: String,
    pub revision: i64,
}

pub async fn editable(state: &AppState, media: i64, actor: i64) -> Result<Editable, ApiError> {
    video_captions::owned_publication(&state.pool, media, actor)
        .await?
        .ok_or(ApiError::NotFound)?;
    sqlx::query_as(
        "SELECT m.id AS media_id, m.status_id, COALESCE(s.title,'Video') AS title,
        m.video_content_revision AS revision FROM media_attachments m
        JOIN statuses s ON s.id=m.status_id JOIN video_drafts d ON d.media_id=m.id
        WHERE m.id=$1 AND m.processing='complete' AND d.state='ready'
          AND m.live_state IS NULL AND s.deleted_at IS NULL",
    )
    .bind(media)
    .fetch_optional(&state.pool)
    .await
    .map_err(plamenu_db::DbError::from)?
    .ok_or(ApiError::NotFound)
}

#[derive(FromRow)]
pub struct Candidate {
    pub media_id: i64,
    pub title: String,
}

/// UI eligibility is batch-loaded after publication authority was checked.
/// Legacy short-attachment Videos and live sessions are not this VOD workflow.
pub async fn ready_native_ids(state: &AppState, ids: &[i64]) -> Result<Vec<i64>, ApiError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_scalar("SELECT m.id FROM media_attachments m JOIN video_drafts d ON d.media_id=m.id
        WHERE m.id=ANY($1) AND m.processing='complete' AND d.state='ready' AND m.live_state IS NULL")
        .bind(ids).fetch_all(&state.pool).await.map_err(plamenu_db::DbError::from)?)
}

pub async fn candidates(
    state: &AppState,
    media: i64,
    actor: i64,
) -> Result<Vec<Candidate>, ApiError> {
    editable(state, media, actor).await?;
    Ok(sqlx::query_as("SELECT d.media_id,d.title FROM video_drafts d
        JOIN media_attachments m ON m.id=d.media_id JOIN video_publications v ON v.channel_id=d.channel_id
        LEFT JOIN video_source_edits e ON e.media_id=d.media_id
        WHERE v.media_id=$1 AND d.account_id=$2 AND m.account_id=$2 AND m.status_id IS NULL
          AND m.scheduled_status_id IS NULL AND m.live_state IS NULL AND m.processing='complete'
          AND d.state='ready' AND d.expires_at>clock_timestamp()
          AND (e.media_id IS NULL OR (e.target_media_id=$1 AND
              e.target_revision=(SELECT video_content_revision FROM media_attachments WHERE id=$1)
              AND EXISTS(SELECT 1 FROM video_source_imports i WHERE i.media_id=e.media_id AND i.state='done')))
        ORDER BY d.media_id DESC LIMIT 100")
        .bind(media).bind(actor).fetch_all(&state.pool).await.map_err(plamenu_db::DbError::from)?)
}

#[derive(FromRow)]
struct Ready {
    media_id: i64,
    account_id: i64,
    channel_id: i64,
    source_file: String,
    source_bytes: i64,
    reserved_bytes: i64,
    generation: i64,
    revision: i64,
}

/// `revision` comes from the review page/API, not a fresh server-side lookup:
/// two editors cannot unknowingly apply a candidate over a newer film.
pub async fn replace(
    state: &AppState,
    media: i64,
    actor: i64,
    candidate: i64,
    revision: i64,
) -> Result<(), ApiError> {
    if media == candidate || revision < 0 {
        return Err(ApiError::BadRequest(
            "Choose a private replacement draft".into(),
        ));
    }
    // The detached task owns the whole short transaction if its HTTP caller
    // disappears. An uncertain commit must never trigger eager file deletion.
    let state = state.clone();
    tokio::spawn(async move {
        let current = editable(&state, media, actor).await?;
        if current.revision != revision
            || !candidates(&state, media, actor)
                .await?
                .iter()
                .any(|c| c.media_id == candidate)
        {
            return Err(ApiError::Conflict(
                "Review a ready replacement for the current film".into(),
            ));
        }
        let incoming = plamenu_db::media::find_by_ids(&state.pool, &[candidate])
            .await?
            .pop()
            .ok_or(ApiError::NotFound)?;
        // Native private drafts only have the efficient AVIF preview. Ordinary
        // publication prepares the reserved JPEG; replacement must do the same
        // before its adoption transaction, or PeerTube silently rejects Update.
        crate::video::ensure_compatible_poster(&state, &incoming).await?;
        adopt(&state, media, actor, candidate, revision).await
    })
    .await
    .map_err(|e| ApiError::Internal(Box::new(e)))?
}

#[expect(
    clippy::too_many_lines,
    reason = "one atomic adoption and signed Update transaction"
)]
async fn adopt(
    state: &AppState,
    media: i64,
    actor: i64,
    candidate: i64,
    revision: i64,
) -> Result<(), ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    video_storage::lock(&mut tx).await?;
    let publication = video_captions::lock_owned_publication(&mut tx, media, actor)
        .await?
        .ok_or(ApiError::NotFound)?;
    // Serialize social deletion/edit and content application. Authority's
    // channel/account locks precede these fences; ids order paired media locks.
    sqlx::query("SELECT id FROM statuses WHERE id=$1 FOR UPDATE")
        .bind(publication.status_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?;
    sqlx::query("SELECT id FROM media_attachments WHERE id=ANY($1) ORDER BY id FOR UPDATE")
        .bind([media, candidate].as_slice())
        .fetch_all(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?;
    let rows:Vec<Ready>=sqlx::query_as("SELECT d.media_id,d.account_id,d.channel_id,d.source_file,d.source_bytes,d.reserved_bytes,d.generation,m.video_content_revision AS revision
        FROM video_drafts d JOIN media_attachments m ON m.id=d.media_id
        JOIN accounts a ON a.id=d.account_id AND a.suspended_at IS NULL
        WHERE m.id=ANY($1) AND m.account_id=d.account_id AND d.state='ready' AND m.processing='complete'
          AND m.live_state IS NULL AND m.scheduled_status_id IS NULL
          AND ((m.id=$2 AND m.status_id=$4) OR (m.id=$3 AND m.account_id=$5 AND m.status_id IS NULL AND d.expires_at>clock_timestamp()))
          AND EXISTS(SELECT 1 FROM video_draft_outputs o WHERE o.media_id=m.id AND o.generation=d.generation AND NOT o.retired AND o.file_name=m.file_name)
          AND EXISTS(SELECT 1 FROM video_draft_outputs o WHERE o.media_id=m.id AND o.generation=d.generation AND NOT o.retired AND o.file_name=m.small_file_name)
        ORDER BY m.id FOR UPDATE OF d")
        .bind([media,candidate].as_slice()).bind(media).bind(candidate).bind(publication.status_id).bind(actor)
        .fetch_all(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
    let old = rows
        .iter()
        .find(|r| r.media_id == media)
        .ok_or(ApiError::Conflict(
            "The published film is no longer editable".into(),
        ))?;
    let new = rows
        .iter()
        .find(|r| r.media_id == candidate)
        .ok_or(ApiError::Conflict(
            "The replacement draft is no longer ready".into(),
        ))?;
    if old.revision != revision || old.channel_id != new.channel_id {
        return Err(ApiError::Conflict(
            "The film changed, or the draft belongs to another channel; review it again".into(),
        ));
    }
    // A derived edit is tied to the film it was reviewed against, even if a
    // client fetches a newer revision token afterwards. Never apply a stale
    // trim/reprocess to another publication or silently discard newer footage.
    let derived:Option<(Option<i64>,i64,String,String)>=sqlx::query_as("SELECT e.target_media_id,e.target_revision,e.operation,i.state
        FROM video_source_edits e JOIN video_source_imports i ON i.media_id=e.media_id WHERE e.media_id=$1")
        .bind(candidate).fetch_optional(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
    if derived
        .as_ref()
        .is_some_and(|(target, expected, _, phase)| {
            *target != Some(media) || *expected != revision || phase != "done"
        })
    {
        return Err(ApiError::Conflict(
            "This edit was prepared from another or older film; prepare it again".into(),
        ));
    }
    let generation = old
        .generation
        .checked_add(1)
        .ok_or(ApiError::Conflict("Video generation exhausted".into()))?;
    let next_revision = revision
        .checked_add(1)
        .ok_or(ApiError::Conflict("Video revision exhausted".into()))?;
    let keys:Vec<String>=sqlx::query_scalar("SELECT file_name FROM video_draft_outputs WHERE media_id=$1 AND generation=$2 AND NOT retired UNION SELECT source_file FROM video_drafts WHERE media_id=$1")
        .bind(candidate).bind(new.generation).fetch_all(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
    // Adoption by a collaborator charges the immutable uploader, including
    // still-retained old files. Global bytes are unchanged. Unknown debt fails
    // closed, just as fresh admission does. Never refund during pointer swaps.
    let (unknown, owned, additional):(i64,i64,i64)=sqlx::query_as("SELECT count(*) FILTER(WHERE reserved_bytes IS NULL),
        COALESCE(sum(reserved_bytes) FILTER(WHERE account_id=$1),0)::bigint,
        COALESCE(sum(reserved_bytes) FILTER(WHERE file_name=ANY($2) AND account_id<>$1),0)::bigint FROM video_storage_reservations")
        .bind(old.account_id).bind(&keys).fetch_one(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM video_storage_reservations WHERE file_name=ANY($1)",
    )
    .bind(&keys)
    .fetch_one(&mut *tx)
    .await
    .map_err(plamenu_db::DbError::from)?;
    if unknown > 0
        || count != i64::try_from(keys.len()).unwrap_or(i64::MAX)
        || (additional > 0
            && additional
                > state
                    .config
                    .native_video
                    .storage_limits()
                    .per_account
                    .saturating_sub(owned))
    {
        return Err(ApiError::Unprocessable("The original uploader's retained-video quota cannot accept this replacement; neither film was changed".into()));
    }
    let old_keys:Vec<String>=sqlx::query_scalar("UPDATE video_draft_outputs SET retired=true WHERE media_id=$1 AND NOT retired RETURNING file_name")
        .bind(media).fetch_all(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
    media_cleanup::enqueue_many(&mut *tx, &old_keys).await?;
    sqlx::query("DELETE FROM native_video_hls_renditions WHERE media_id=$1")
        .bind(media)
        .execute(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?;
    sqlx::query("DELETE FROM video_storyboards WHERE media_id=$1")
        .bind(media)
        .execute(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?;
    // This DELETE queues only the old JPEG. The candidate row is MOVED rather
    // than deleted, so its adopted JPEG cannot be enqueued by the trigger.
    sqlx::query("DELETE FROM video_compatibility_posters WHERE media_id=$1")
        .bind(media)
        .execute(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?;
    sqlx::query("UPDATE video_draft_outputs SET media_id=$2,generation=$3 WHERE media_id=$1 AND generation=$4 AND NOT retired")
        .bind(candidate).bind(media).bind(generation).bind(new.generation).execute(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
    for query in [
        "UPDATE native_video_hls_renditions SET media_id=$2,generation=$3 WHERE media_id=$1",
        "UPDATE video_storyboards SET media_id=$2,generation=$3 WHERE media_id=$1",
    ] {
        sqlx::query(query)
            .bind(candidate)
            .bind(media)
            .bind(generation)
            .execute(&mut *tx)
            .await
            .map_err(plamenu_db::DbError::from)?;
    }
    let uuid: String = sqlx::query_scalar("SELECT uuid FROM video_publications WHERE media_id=$1")
        .bind(media)
        .fetch_one(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?;
    let heights: Vec<i32> =
        sqlx::query_scalar("SELECT height FROM native_video_hls_renditions WHERE media_id=$1")
            .bind(media)
            .fetch_all(&mut *tx)
            .await
            .map_err(plamenu_db::DbError::from)?;
    for height in heights {
        sqlx::query("UPDATE native_video_hls_renditions SET p2p_infohash=$3 WHERE media_id=$1 AND height=$2")
            .bind(media).bind(height).bind(plamenu_db::native_video_hls::p2p_infohash(&uuid,height))
            .execute(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
    }
    sqlx::query("UPDATE video_compatibility_posters SET media_id=$2 WHERE media_id=$1")
        .bind(candidate)
        .bind(media)
        .execute(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?;
    // Legacy single-track captions get their candidate language explicitly:
    // the publication's language is intentionally not changed by replacement.
    sqlx::query("INSERT INTO video_captions(media_id,language,vtt_body)
        SELECT m.id,COALESCE(d.language,'und'),m.caption_vtt FROM media_attachments m JOIN video_drafts d ON d.media_id=m.id
        WHERE m.id=$1 AND m.caption_vtt IS NOT NULL ON CONFLICT(media_id,language) DO NOTHING")
        .bind(candidate).execute(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
    for (remove, move_rows) in [
        (
            "DELETE FROM video_captions WHERE media_id=$1",
            "UPDATE video_captions SET media_id=$2 WHERE media_id=$1",
        ),
        (
            "DELETE FROM video_chapters WHERE media_id=$1",
            "UPDATE video_chapters SET media_id=$2 WHERE media_id=$1",
        ),
    ] {
        sqlx::query(remove)
            .bind(media)
            .execute(&mut *tx)
            .await
            .map_err(plamenu_db::DbError::from)?;
        sqlx::query(move_rows)
            .bind(candidate)
            .bind(media)
            .execute(&mut *tx)
            .await
            .map_err(plamenu_db::DbError::from)?;
    }
    sqlx::query("UPDATE media_attachments m SET file_name=n.file_name,small_file_name=n.small_file_name,
        width=n.width,height=n.height,small_width=n.small_width,small_height=n.small_height,
        file_size=n.file_size,thumbnail_file_size=n.thumbnail_file_size,duration=n.duration,frame_rate=n.frame_rate,bitrate=n.bitrate,
        content_type=n.content_type,kind=n.kind,blurhash=n.blurhash,focus_x=n.focus_x,focus_y=n.focus_y,
        description=n.description,transcript=n.transcript,caption_vtt=NULL,
        audio_described=n.audio_described,visuals_conveyed_in_audio=n.visuals_conveyed_in_audio,video_content_revision=$3
        FROM media_attachments n WHERE m.id=$1 AND n.id=$2")
        .bind(media).bind(candidate).bind(next_revision).execute(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
    // Swap the source pointers atomically. Deleting the candidate now queues
    // the OLD source, not the freshly adopted one. No placeholder key or bytes.
    sqlx::query("SET CONSTRAINTS video_drafts_source_file_key DEFERRED")
        .execute(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?;
    sqlx::query(
        "UPDATE video_drafts d SET source_file=CASE WHEN d.media_id=$1 THEN $3 ELSE $4 END,
        source_bytes=CASE WHEN d.media_id=$1 THEN $5 ELSE $6 END,
        reserved_bytes=CASE WHEN d.media_id=$1 THEN $7 ELSE $8 END
        WHERE d.media_id=ANY($2)",
    )
    .bind(media)
    .bind([media, candidate].as_slice())
    .bind(&new.source_file)
    .bind(&old.source_file)
    .bind(new.source_bytes)
    .bind(old.source_bytes)
    .bind(new.reserved_bytes)
    .bind(old.reserved_bytes)
    .execute(&mut *tx)
    .await
    .map_err(plamenu_db::DbError::from)?;
    sqlx::query(
        "UPDATE video_drafts d SET generation=$3,source_fingerprint=n.source_fingerprint,
        poster_time_seconds=n.poster_time_seconds,poster_alt=n.poster_alt,
        force_reencode=n.force_reencode,clip_start_ms=n.clip_start_ms,clip_end_ms=n.clip_end_ms
        FROM video_drafts n WHERE d.media_id=$1 AND n.media_id=$2",
    )
    .bind(media)
    .bind(candidate)
    .bind(generation)
    .execute(&mut *tx)
    .await
    .map_err(plamenu_db::DbError::from)?;
    // Only verified same-source reprocessing preserves positions. The media
    // row fence blocks concurrent history writes; old players still fail their
    // content-revision check. Trims/replacements do not carry old positions.
    if derived
        .as_ref()
        .is_some_and(|(_, _, operation, _)| operation == "reprocess")
    {
        sqlx::query("UPDATE video_watch_history SET content_revision=$3 WHERE media_id=$1 AND content_revision=$2")
            .bind(media).bind(revision).bind(next_revision).execute(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
    }
    sqlx::query(
        "UPDATE video_storage_reservations SET media_id=$2,account_id=$3 WHERE file_name=ANY($1)",
    )
    .bind(&keys)
    .bind(media)
    .bind(old.account_id)
    .execute(&mut *tx)
    .await
    .map_err(plamenu_db::DbError::from)?;
    // Candidate metadata is intentionally not substituted for the post's
    // title/audience/licence/warning. Those have independent authorized editors.
    sqlx::query("DELETE FROM media_attachments WHERE id=$1")
        .bind(candidate)
        .execute(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?;
    let edited_at = time::OffsetDateTime::now_utc();
    sqlx::query("UPDATE statuses SET edited_at=$2,updated_at=$2 WHERE id=$1")
        .bind(publication.status_id)
        .bind(edited_at)
        .execute(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?;
    let updated = plamenu_db::status::find_by_id(&mut *tx, publication.status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let author = plamenu_db::account::find_by_id(&mut *tx, publication.actor_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    crate::actions::federate_note_update_conn(
        state,
        &mut tx,
        &author,
        &updated,
        edited_at,
        Vec::new(),
    )
    .await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    crate::streaming::status_edited(state, updated.id).await;
    crate::webhooks::status_event(state, plamenu_db::webhook::STATUS_UPDATED, &updated).await;
    Ok(())
}
