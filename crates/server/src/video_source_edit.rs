//! Durable private preparation of published-film edits. Reuse the bounded
//! receiving/import lane; never encode or copy while publication locks are held.
use crate::{AppState, error::ApiError, video_clip::Window};
use plamenu_db::{video_captions, video_draft, video_source_import};
use sqlx::{FromRow, PgConnection};

#[derive(Clone, Copy, Debug)]
pub enum Operation {
    Reprocess,
    Trim(Window),
}
impl Operation {
    fn name(self) -> &'static str {
        match self {
            Self::Reprocess => "reprocess",
            Self::Trim(_) => "trim",
        }
    }
}

#[derive(FromRow)]
pub struct Prepared {
    pub target_media_id: Option<i64>,
    pub target_revision: i64,
    pub operation: String,
    pub phase: String,
}
pub async fn prepared(state: &AppState, id: i64, actor: i64) -> Result<Option<Prepared>, ApiError> {
    Ok(sqlx::query_as(
        "SELECT e.target_media_id,e.target_revision,e.operation,
        CASE WHEN d.expires_at<=clock_timestamp() THEN 'expired'
             WHEN d.state='receiving' THEN i.state ELSE d.state END AS phase
        FROM video_source_edits e JOIN video_source_imports i ON i.media_id=e.media_id
        JOIN video_drafts d ON d.media_id=e.media_id JOIN media_attachments m ON m.id=d.media_id
        WHERE d.media_id=$1 AND d.account_id=$2 AND m.account_id=$2 AND m.status_id IS NULL",
    )
    .bind(id)
    .bind(actor)
    .fetch_optional(&state.pool)
    .await
    .map_err(plamenu_db::DbError::from)?)
}
pub async fn check_retry(state: &AppState, id: i64, actor: i64) -> Result<(), ApiError> {
    if let Some(edit) = prepared(state, id, actor).await? {
        let mut tx = state
            .pool
            .begin()
            .await
            .map_err(plamenu_db::DbError::from)?;
        let original = source(
            &mut tx,
            edit.target_media_id.ok_or(ApiError::NotFound)?,
            actor,
        )
        .await?;
        current(&original, edit.target_revision)?;
        tx.commit().await.map_err(plamenu_db::DbError::from)?;
    }
    Ok(())
}
pub async fn processing(state: &AppState, id: i64) -> Result<(bool, Option<Window>), ApiError> {
    let (force, start, end): (bool, i64, Option<i64>) = sqlx::query_as(
        "SELECT force_reencode,clip_start_ms,clip_end_ms FROM video_drafts WHERE media_id=$1",
    )
    .bind(id)
    .fetch_one(&state.pool)
    .await
    .map_err(plamenu_db::DbError::from)?;
    Ok((force, end.map(|end| Window::new(start, end)).transpose()?))
}

#[derive(FromRow)]
struct Retained {
    channel_id: i64,
    source_file: String,
    source_bytes: i64,
    clip_start_ms: i64,
    clip_end_ms: Option<i64>,
    duration: Option<f64>,
    revision: i64,
    title: String,
    description: String,
    language: Option<String>,
    visibility: String,
    media_description: Option<String>,
    transcript: Option<String>,
    audio_described: bool,
    visuals_conveyed_in_audio: bool,
}
async fn source(conn: &mut PgConnection, media: i64, actor: i64) -> Result<Retained, ApiError> {
    video_captions::lock_owned_publication(conn, media, actor)
        .await?
        .ok_or(ApiError::NotFound)?;
    sqlx::query_as("SELECT d.channel_id,d.source_file,d.source_bytes,d.clip_start_ms,d.clip_end_ms,
        m.duration,m.video_content_revision AS revision,COALESCE(s.title,'Video') AS title,
        d.description,s.language,s.visibility,m.description AS media_description,m.transcript,
        m.audio_described,m.visuals_conveyed_in_audio
        FROM video_drafts d JOIN media_attachments m ON m.id=d.media_id JOIN statuses s ON s.id=m.status_id
        WHERE m.id=$1 AND d.state='ready' AND m.processing='complete' AND m.live_state IS NULL AND d.source_bytes>0
        FOR UPDATE OF d")
        .bind(media).fetch_optional(conn).await.map_err(plamenu_db::DbError::from)?.ok_or(ApiError::NotFound)
}
fn current(source: &Retained, revision: i64) -> Result<(), ApiError> {
    if source.revision != revision {
        return Err(ApiError::Conflict(
            "The published film changed; review it before preparing an edit".into(),
        ));
    }
    Ok(())
}
fn raw_window(source: &Retained, operation: Operation) -> Result<Option<Window>, ApiError> {
    let parent = source
        .clip_end_ms
        .map(|end| Window::new(source.clip_start_ms, end))
        .transpose()?;
    match operation {
        Operation::Reprocess => Ok(parent),
        Operation::Trim(relative) => {
            let duration = source
                .duration
                .filter(|v| v.is_finite() && *v > 0.0)
                .ok_or(ApiError::Unprocessable(
                    "This film has no reliable duration for trimming".into(),
                ))?;
            if relative.duration_seconds() + seconds(relative.start_ms()) > duration + 0.001 {
                return Err(ApiError::Unprocessable(
                    "The cut must fit within the currently published film".into(),
                ));
            }
            Ok(Some(
                parent.map_or(Ok(relative), |parent| relative.within(parent))?,
            ))
        }
    }
}
#[expect(
    clippy::cast_precision_loss,
    reason = "bounded seven-day millisecond window"
)]
fn seconds(ms: i64) -> f64 {
    ms as f64 / 1000.0
}

/// Allocate a private, quota-charged receiving draft, then enqueue a restartable
/// copy with its source revision and annotation snapshot. Applying is separate.
pub async fn prepare(
    state: &AppState,
    media: i64,
    actor: i64,
    revision: i64,
    operation: Operation,
) -> Result<i64, ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let original = source(&mut tx, media, actor).await?;
    current(&original, revision)?;
    let window = raw_window(&original, operation)?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    let id = plamenu_db::id::next();
    let key = format!("{id}.orig");
    let config = state.config.native_video;
    if u64::try_from(original.source_bytes).unwrap_or(u64::MAX) > config.max_upload_bytes() {
        return Err(ApiError::Unprocessable(
            "The retained source exceeds the current native-video processing limit".into(),
        ));
    }
    let limits = video_draft::AdmissionLimits {
        unfinished_per_account: i64::from(config.unfinished_drafts),
        retained_source_bytes_per_account: config.storage_limits().per_account,
        pending_source_bytes_global: i64::try_from(
            config.pending_gib * crate::config::NativeVideoConfig::GIB,
        )
        .unwrap_or(i64::MAX),
        storage: config.storage_limits(),
        disk_available_bytes: crate::storage::available_bytes(&state.config.media_dir)
            .map_err(|e| ApiError::Internal(Box::new(e)))?,
        min_free_bytes: config.min_free_gib * crate::config::NativeVideoConfig::GIB,
        processing_reserved_bytes: state.video_resources.usage().1,
    };
    let trim = matches!(operation, Operation::Trim(_));
    // Untimed text cannot be automatically trimmed without leaking removed
    // passages or claiming to describe different footage. The review explains
    // this and offers ordinary accessible alternatives before application.
    let title: String = original.title.chars().take(120).collect();
    let new = video_draft::NewDraft {
        media_id: id,
        account_id: actor,
        channel_id: original.channel_id,
        title: &title,
        description: &original.description,
        language: original.language.as_deref(),
        visibility: &original.visibility,
        source_file: &key,
        source_fingerprint: None,
        poster_time_seconds: 0.0,
        media_description: (!trim)
            .then_some(original.media_description.as_deref())
            .flatten(),
        media_transcript: (!trim).then_some(original.transcript.as_deref()).flatten(),
        caption_vtt: None,
        reserved_bytes: original.source_bytes,
    };
    match video_draft::reserve(&state.pool, &new, &limits).await? {
        video_draft::Admission::Accepted => {}
        video_draft::Admission::Forbidden => return Err(ApiError::NotFound),
        video_draft::Admission::AccountFull => {
            return Err(ApiError::Unprocessable(
                "Publish or cancel an unfinished video draft before preparing another edit".into(),
            ));
        }
        video_draft::Admission::StorageFull => return Err(ApiError::Unprocessable(
            "Video storage cannot reserve an independent edit; the published film was not changed"
                .into(),
        )),
    }
    let result = enqueue(state, media, actor, revision, id, operation, window).await;
    if result.is_err() {
        video_draft::cancel(&state.pool, id, actor).await?;
    }
    result.map(|()| id)
}
async fn enqueue(
    state: &AppState,
    media: i64,
    actor: i64,
    revision: i64,
    id: i64,
    operation: Operation,
    window: Option<Window>,
) -> Result<(), ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let original = source(&mut tx, media, actor).await?;
    current(&original, revision)?;
    if !video_draft::lock_receiving(&mut tx, id, actor).await? {
        return Err(ApiError::Conflict(
            "The private edit draft was cancelled".into(),
        ));
    }
    let tracks = video_captions::for_media(&mut *tx, &[media]).await?;
    for track in tracks.get(&media).into_iter().flatten() {
        let body = track.vtt_body.as_deref().ok_or(ApiError::Unprocessable(
            "Local editing requires local caption tracks; correct this track before editing".into(),
        ))?;
        let body = match operation {
            Operation::Reprocess => Some(body.to_owned()),
            Operation::Trim(relative) => crate::video_clip::captions(body, relative)?,
        };
        if let Some(body) = body {
            sqlx::query("INSERT INTO video_captions(media_id,language,vtt_body,automatically_generated) VALUES($1,$2,$3,$4)")
                .bind(id).bind(&track.language).bind(body).bind(track.automatically_generated).execute(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
        }
    }
    let chapters = plamenu_db::video_chapters::for_media(&mut *tx, &[media]).await?;
    let chapters = chapters.get(&media).cloned().unwrap_or_default();
    let chapters = match operation {
        Operation::Reprocess => chapters,
        Operation::Trim(relative) => crate::video_clip::chapters(&chapters, relative),
    };
    plamenu_db::video_chapters::replace_conn(&mut tx, id, &chapters).await?;
    sqlx::query("UPDATE video_drafts SET force_reencode=true,clip_start_ms=$2,clip_end_ms=$3 WHERE media_id=$1")
        .bind(id).bind(window.map_or(0,Window::start_ms)).bind(window.map(Window::end_ms)).execute(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
    if matches!(operation, Operation::Reprocess) {
        sqlx::query("UPDATE media_attachments SET audio_described=$2,visuals_conveyed_in_audio=$3 WHERE id=$1")
            .bind(id).bind(original.audio_described).bind(original.visuals_conveyed_in_audio).execute(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
    }
    sqlx::query("INSERT INTO video_source_imports(media_id,source_url,source_kind) VALUES($1,'native','native')")
        .bind(id).execute(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
    sqlx::query("INSERT INTO video_source_edits(media_id,target_media_id,target_revision,operation) VALUES($1,$2,$3,$4)")
        .bind(id).bind(media).bind(revision).bind(operation.name()).execute(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

/// A short authority/revision fence before opening a copy and again before
/// making that copy processable. The private source key is never sent to clients.
pub async fn lock_source(
    conn: &mut PgConnection,
    claim: &video_source_import::Claim,
) -> Result<(String, i64), ApiError> {
    let media = claim.target_media_id.ok_or(ApiError::NotFound)?;
    let revision = claim.target_revision.ok_or(ApiError::NotFound)?;
    let original = source(conn, media, claim.account_id).await?;
    current(&original, revision)?;
    if original.channel_id != claim.channel_id || original.source_bytes != claim.reserved_bytes {
        return Err(ApiError::Conflict(
            "The retained source changed; prepare a new edit".into(),
        ));
    }
    Ok((original.source_file, original.source_bytes))
}
