//! Local live-broadcast authoring. The secret exists only in this request and
//! its one-time HTML response; the database retains only a SHA-256 digest.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use plamenu_db::{account::Account, id, native_live, status::Status};
use sha2::{Digest as _, Sha256};
use time::OffsetDateTime;

use crate::{AppState, actions::PostParams, error::ApiError};

pub struct Created {
    pub status: Status,
    pub broadcast_uuid: String,
    pub ingest_key: String,
}

fn new_ingest_key() -> (String, [u8; 32]) {
    let mut secret = [0_u8; 32];
    getrandom::fill(&mut secret).expect("operating-system entropy is available");
    let key = URL_SAFE_NO_PAD.encode(secret);
    let mut hash = [0_u8; 32];
    hash.copy_from_slice(&Sha256::digest(key.as_bytes()));
    (key, hash)
}

/// Announce a waiting Video in an owned channel. A private allocation is made
/// first, then the existing post path atomically binds its media, publication
/// and federation jobs. Failure discards the private allocation. A crash in
/// between leaves only unattached, unaddressable media for orphan cleanup.
#[allow(
    clippy::too_many_arguments,
    reason = "HTML form fields mirror the shared Video publication path"
)]
pub async fn create(
    state: &AppState,
    owner: &Account,
    channel_id: i64,
    title: &str,
    description: &str,
    language: Option<&str>,
    visibility: &str,
    permanent: bool,
    playback: native_live::PlaybackOptions,
    scheduled_starts: &[OffsetDateTime],
    metadata: &crate::video_metadata::Input,
) -> Result<Created, ApiError> {
    validate_schedules(scheduled_starts)?;
    crate::video_metadata::authoring(metadata)?;
    if !(3..=120).contains(&title.trim().chars().count())
        || description.chars().count() > 10_000
        || !matches!(visibility, "public" | "unlisted")
        || !playback.valid()
    {
        return Err(ApiError::Unprocessable(
            "Check the live broadcast title, description and visibility".into(),
        ));
    }
    let media_id = id::next();
    let (ingest_key, hash) = new_ingest_key();
    let allocation = native_live::allocate(
        &state.pool,
        media_id,
        owner.id,
        channel_id,
        &hash,
        permanent,
        scheduled_starts,
        playback,
    )
    .await?
    .ok_or(ApiError::Forbidden(
        "This channel cannot create another live broadcast".into(),
    ))?;
    let result = async {
        let metadata = crate::video_metadata::draft(state, owner.id, media_id, metadata).await?;
        crate::actions::post_video(
            state,
            PostParams {
                username: &owner.username,
                media_ids: &[media_id],
                title: Some(title.trim()),
                text: description,
                language,
                visibility,
                spoiler_text: &metadata.spoiler_text,
                sensitive: metadata.sensitive,
                ..Default::default()
            },
            channel_id,
        )
        .await
    }
    .await;
    let status = match result {
        Ok((status, _)) => status,
        Err(error) => {
            if let Err(cleanup_error) =
                native_live::discard_unpublished(&state.pool, media_id, owner.id).await
            {
                tracing::warn!(%cleanup_error, media_id, "failed to discard unpublished live allocation");
            }
            return Err(error);
        }
    };
    Ok(Created {
        status,
        broadcast_uuid: allocation.broadcast_uuid,
        ingest_key,
    })
}

/// A schedule is an announcement, not an ingest gate. Give federation at
/// least a minute to deliver it and cap accidental far-future dates.
pub fn validate_schedule(at: Option<OffsetDateTime>) -> Result<(), ApiError> {
    if let Some(at) = at {
        let now = OffsetDateTime::now_utc();
        if at <= now + time::Duration::minutes(1) || at > now + time::Duration::days(366) {
            return Err(ApiError::Unprocessable(
                "Choose a planned start between one minute and one year from now".into(),
            ));
        }
    }
    Ok(())
}

pub fn validate_schedules(starts: &[OffsetDateTime]) -> Result<(), ApiError> {
    if starts.len() > 16 || starts.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(ApiError::Unprocessable(
            "Choose no more than 16 distinct planned starts".into(),
        ));
    }
    for at in starts {
        validate_schedule(Some(*at))?;
    }
    Ok(())
}

pub async fn rotate_ingest_key(
    state: &AppState,
    status_id: i64,
    owner: i64,
) -> Result<Option<String>, ApiError> {
    let (key, hash) = new_ingest_key();
    Ok(
        native_live::rotate_key(&state.pool, status_id, owner, &hash)
            .await?
            .then_some(key),
    )
}

/// Announce waiting/live/ended transitions to the channel's followers. The
/// canonical Video is rebuilt after the state commit, so dereferencing and
/// delivery agree about the current session.
pub async fn federate_state(state: &AppState, status_id: i64) -> Result<(), ApiError> {
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let status = plamenu_db::status::find_by_id(&mut *conn, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let channel = plamenu_db::account::find_by_id(&mut *conn, status.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    crate::actions::federate_note_update(
        state,
        &channel,
        &status,
        time::OffsetDateTime::now_utc(),
        Vec::new(),
    )
    .await?;
    crate::streaming::status_edited(state, status.id).await;
    Ok(())
}
