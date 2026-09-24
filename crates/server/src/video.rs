//! Native video publication, shared by first-party forms and client APIs.
//! Channel identity is distinct from both its owning person and communities.

use plamenu_ap::{activity, actor::Actor};
use plamenu_db::{
    account::{self, Account, NewLocalAccount},
    media::Media,
    video,
};
use serde_json::{Value, json};

use crate::{AppState, error::ApiError};

/// `PeerTube`'s supported description envelope, separate from short social posts.
pub const MAX_DESCRIPTION_CHARACTERS: i32 = 10_000;

fn validate_title(title: Option<&str>) -> Result<(), ApiError> {
    if title.is_none_or(|s| !(3..=120).contains(&s.trim().chars().count())) {
        return Err(ApiError::Unprocessable(
            "Give the video a title of 3–120 characters".into(),
        ));
    }
    Ok(())
}

pub async fn create_channel(
    state: &AppState,
    owner: &Account,
    name: &str,
    title: &str,
) -> Result<Account, ApiError> {
    let name = name.trim();
    let title = title.trim();
    if name.is_empty()
        || name.len() > 30
        || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
    {
        return Err(ApiError::Unprocessable(
            "Use 1–30 letters, numbers or underscores for the channel address".into(),
        ));
    }
    if title.is_empty() || title.chars().count() > 120 {
        return Err(ApiError::Unprocessable(
            "Give the channel a name of 1–120 characters".into(),
        ));
    }
    if !owner.is_local() || owner.is_group() || owner.suspended() {
        return Err(ApiError::Forbidden(
            "This account cannot create a channel".into(),
        ));
    }
    if plamenu_db::username_block::matches(&state.pool, name, false).await?
        || plamenu_db::username_block::matches(&state.pool, name, true).await?
    {
        return Err(ApiError::Unprocessable(
            "This channel address is reserved".into(),
        ));
    }
    let keys = crate::auth::generate_keypair_gated().await?;
    let ed = plamenu_ap::keys::generate_ed25519_keypair();
    let keyring = state.federation_keyring.as_deref().ok_or_else(|| {
        ApiError::Internal(Box::new(
            crate::crypto::KeyEncryptionError::MissingConfiguration,
        ))
    })?;
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    // Serialize the per-owner quota, including concurrent creation requests.
    sqlx::query!("SELECT id FROM accounts WHERE id = $1 FOR UPDATE", owner.id)
        .fetch_one(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?;
    if video::owned_channels(&mut *tx, owner.id).await?.len() >= 50 {
        return Err(ApiError::Unprocessable(
            "You have reached the limit of 50 video channels".into(),
        ));
    }
    let mut channel = account::create_local_immutable(
        &mut *tx,
        NewLocalAccount {
            username: name,
            display_name: title,
            note: "",
            public_key_pem: &keys.public_pem,
        },
        &state.config.domain,
    )
    .await
    .map_err(|error| match error {
        plamenu_db::DbError::UsernameTaken => {
            ApiError::Unprocessable("This channel address is already taken".into())
        }
        other => ApiError::from(other),
    })?;
    video::create_channel(&mut tx, channel.id, owner.id).await?;
    channel.actor_type = Some("Group".into());
    crate::key_store::provision_account_tx(
        &mut tx,
        keyring,
        &state.config.domain,
        &channel,
        &keys,
        &ed,
    )
    .await
    .map_err(|e| ApiError::Internal(Box::new(e)))?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(channel)
}

/// A channel advertises its owning Person, never a community's moderators
/// collection. Applied to the existing signed account document.
pub async fn channel_actor(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    account: &Account,
    actor: &mut Actor,
) -> Result<(), ApiError> {
    if !account.is_group() {
        return Ok(());
    }
    let Some(channel) = video::channel(&mut *conn, account.id).await? else {
        return Ok(());
    };
    let owner = account::find_by_id(&mut *conn, channel.owner_account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    actor.attributed_to = Some(crate::entities::account_uri(&state.config.domain, &owner));
    actor.affiliations = None;
    actor.posting_policy = None;
    actor.posting_restricted_to_mods = None;
    Ok(())
}

pub fn validate_publication(
    params: &crate::actions::PostParams<'_>,
    media: &[Media],
) -> Result<(), ApiError> {
    if !matches!(params.visibility, "public" | "unlisted") {
        return Err(ApiError::Unprocessable(
            "Native videos currently support public or unlisted visibility".into(),
        ));
    }
    if params.in_reply_to_id.is_some()
        || params.group_id.is_some()
        || params.poll.is_some()
        || params.event.is_some()
        || params.quoted_status_id.is_some()
        || params.external_url.is_some_and(|u| !u.trim().is_empty())
    {
        return Err(ApiError::Unprocessable(
            "Publish a video as its own post in a video channel".into(),
        ));
    }
    validate_title(params.title)?;
    if media.len() != 1
        || media[0].content_type != "video/mp4"
        || media[0].processing != "complete"
        || media[0].file_name.is_none()
        || media[0].small_file_name.is_none()
        || media[0].duration.is_none_or(|d| !d.is_finite() || d <= 0.0)
    {
        return Err(ApiError::Unprocessable(
            "Choose one fully processed video with a thumbnail".into(),
        ));
    }
    Ok(())
}

/// Cache a federation-compatible thumbnail once, not on each remote fetch.
/// The media row lock serializes competing publications and deletion. Native
/// clients keep the original AVIF; the derivative follows the source's lifetime.
pub async fn ensure_compatible_poster(state: &AppState, media: &Media) -> Result<(), ApiError> {
    let source = media.small_file_name.as_deref().ok_or(ApiError::NotFound)?;
    if matches!(
        crate::media_processing::content_type_for(source),
        "image/jpeg" | "image/png" | "image/webp" | "image/gif"
    ) && media
        .small_width
        .zip(media.small_height)
        .is_some_and(|(w, h)| i64::from(w) * 9 == i64::from(h) * 16)
    {
        return Ok(());
    }
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let found = sqlx::query_scalar!(
        "SELECT id FROM media_attachments WHERE id = $1 AND account_id = $2 AND remote_url IS NULL FOR UPDATE",
        media.id, media.account_id,
    ).fetch_optional(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
    if found.is_none() {
        return Err(ApiError::NotFound);
    }
    if sqlx::query_scalar!(
        "SELECT media_id FROM video_compatibility_posters WHERE media_id = $1",
        media.id
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(plamenu_db::DbError::from)?
    .is_some()
    {
        return Ok(());
    }
    let bytes = state
        .media
        .get(source)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let settings = state.settings_cache.get(&state.pool).await?;
    let params =
        crate::media_processing::EncodeParams::from_settings(&settings, &state.config.ffmpeg_path);
    let poster = crate::media_processing::process_video_poster_blocking(bytes, params).await?;
    let size = i64::try_from(poster.bytes.len()).unwrap_or(i64::MAX);
    let (key, quota_reserved) =
        compatibility_poster_key(&mut tx, media, poster.extension, size).await?;
    state
        .media
        .put(&key, poster.bytes)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let width = i32::try_from(poster.width).unwrap_or(640);
    let height = i32::try_from(poster.height).unwrap_or(640);
    let cleanup_id = plamenu_db::id::next();
    let result = sqlx::query!("INSERT INTO video_compatibility_posters (media_id, file_name, width, height, file_size, cleanup_job_id) VALUES ($1, $2, $3, $4, $5, $6)",
            media.id, key, width, height, size, cleanup_id)
            .execute(&mut *tx).await;
    if let Err(error) = result {
        // Unique keys make this safe: a competing publisher cannot own it.
        if let Err(cleanup_error) = state.media.delete(&key).await {
            tracing::warn!(%cleanup_error, file = %key, "poster rollback cleanup needs reconciliation");
        }
        return Err(plamenu_db::DbError::from(error).into());
    }
    if quota_reserved && !plamenu_db::video_storage::shrink(&mut tx, &key, size).await? {
        return Err(ApiError::Unprocessable(
            "Video poster storage reservation changed".into(),
        ));
    }
    // A commit transport failure has an uncertain outcome. Keep the bytes;
    // reconciliation can remove an orphan, but deleting could break a commit.
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

/// Select the already-budgeted native key while the caller holds the media
/// fence. Legacy non-draft attachments retain their existing storage path.
async fn compatibility_poster_key(
    conn: &mut sqlx::PgConnection,
    media: &Media,
    extension: &str,
    size: i64,
) -> Result<(String, bool), ApiError> {
    let Some(stem) = media
        .file_name
        .as_deref()
        .and_then(|file| file.strip_suffix(".draft.mp4"))
    else {
        return Ok((
            format!(
                "{}.draft.poster.{extension}",
                crate::media_processing::storage_stem(plamenu_db::id::next())
            ),
            false,
        ));
    };
    let key = format!("{stem}.draft.poster.jpg");
    let allowance = sqlx::query_scalar!(
        "SELECT reserved_bytes FROM video_storage_reservations
            WHERE file_name = $1 AND media_id = $2 AND account_id = $3 FOR UPDATE",
        key,
        media.id,
        media.account_id
    )
    .fetch_optional(conn)
    .await
    .map_err(plamenu_db::DbError::from)?
    .flatten();
    if extension != "jpg" || allowance.is_none_or(|allowance| size > allowance) {
        return Err(ApiError::Unprocessable(
            "Video compatibility poster has no sufficient storage reservation".into(),
        ));
    }
    Ok((key, true))
}

/// The canonical Video shape is used by Create, Update and dereferencing.
/// Keep an ordinary Document attachment as well for attachment-only clients.
pub fn enrich_object(
    domain: &str,
    note: &mut Value,
    publication: &video::Publication,
    media: &Media,
    hls: &[plamenu_db::native_video_hls::FileTag],
) -> Result<(), ApiError> {
    let file = media.file_name.as_deref().ok_or(ApiError::NotFound)?;
    let poster = publication
        .poster_file_name
        .as_deref()
        .or(media.small_file_name.as_deref())
        .ok_or(ApiError::NotFound)?;
    let author = note["attributedTo"].clone();
    let web = note["url"].clone();
    let published = note["published"].clone();
    let language = note["contentMap"]
        .as_object()
        .and_then(|m| m.keys().next())
        .cloned();
    note["type"] = json!("Video");
    // A scoped context survives both standalone dereferencing and nesting in
    // Create/Update without adding video terms to every ordinary Note.
    note["@context"] = context();
    note["uuid"] = json!(publication.uuid);
    note["attributedTo"] = json!([
        {"type":"Person", "id":author},
        {"type":"Group", "id":publication.channel_uri}
    ]);
    note["duration"] = json!(format!(
        "PT{:.0}S",
        media.duration.unwrap_or_default().ceil()
    ));
    if let Some((width, height)) = media
        .width
        .zip(media.height)
        .filter(|(w, h)| *w > 0 && *h > 0)
    {
        note["aspectRatio"] = json!(f64::from(width) / f64::from(height));
    }
    let mut urls = vec![json!({
        "type":"Link", "mediaType":"text/html", "href":web
    })];
    urls.push(json!(
        {"type":"Link", "mediaType":"video/mp4", "href":format!("https://{domain}/media/{file}"),
         "height":media.height.unwrap_or(0), "width":media.width.unwrap_or(0), "size":publication.file_size}
    ));
    if !hls.is_empty() {
        let files: Vec<Value> = hls
            .iter()
            .map(|rendition| {
                json!({
                    "type": "Link", "mediaType": "video/mp4",
                    "href": format!("https://{domain}/media/{}", rendition.file_name),
                    "height": rendition.height, "width": rendition.width,
                    "size": rendition.size_bytes,
                })
            })
            .collect();
        urls.push(json!({
            "type": "Link", "mediaType": "application/x-mpegURL",
            "href": format!("https://{domain}/media/hls/{}/master.m3u8", media.id),
            "tag": files,
        }));
    }
    note["url"] = json!(urls);
    let mut icon = json!({"type":"Image", "mediaType":crate::media_processing::content_type_for(poster),
        "url":format!("https://{domain}/media/{poster}"),
        "width":publication.poster_width.or(media.small_width).unwrap_or(0),
        "height":publication.poster_height.or(media.small_height).unwrap_or(0)});
    if let Some(alt) = publication.poster_alt.as_deref() {
        icon["name"] = json!(alt);
    }
    note["icon"] = json!([icon]);
    note["updated"] = note
        .get("updated")
        .cloned()
        .filter(|v| !v.is_null())
        .unwrap_or(published);
    note["views"] = json!(0);
    note["state"] = json!(1);
    note["isLiveBroadcast"] = json!(false);
    note["commentsPolicy"] = json!(1);
    note["downloadEnabled"] = json!(true);
    note["waitTranscoding"] = json!(false);
    // PeerTube accepts text/html and its description renderer sanitizes HTML.
    // Keep the canonical sanitized HTML instead of relabelling unescaped plain
    // text as Markdown (which changed literal asterisks and discarded links).
    note["mediaType"] = json!("text/html");
    if note["content"].as_str().unwrap_or_default().chars().count() > 10_000 {
        return Err(ApiError::Unprocessable(
            "The formatted video description exceeds 10000 characters; shorten it before publishing".into(),
        ));
    }
    if let Some(lang) = language.as_deref() {
        note["language"] = json!({"identifier":lang});
        note["contentMap"][lang] = note["content"].clone();
    }
    if media.caption_vtt.is_some() {
        note["subtitleLanguage"] = json!([{"identifier":language.as_deref().unwrap_or("und"),
            "name":language.as_deref().unwrap_or("Undetermined"),
            "url":format!("https://{domain}/media/{}/captions.vtt", media.id)}]);
    } else {
        note["subtitleLanguage"] = json!([]);
    }
    Ok(())
}

pub async fn validate_edit(
    state: &AppState,
    status: &plamenu_db::status::Status,
    media_ids: &[i64],
    title: Option<&str>,
) -> Result<(), ApiError> {
    if status.object_type.as_deref() != Some("Video") {
        return Ok(());
    }
    let publication = video::publications(&state.pool, &[status.id])
        .await?
        .pop()
        .ok_or(ApiError::NotFound)?;
    if media_ids != [publication.media_id] {
        return Err(ApiError::Unprocessable(
            "A published video's media cannot be removed or replaced".into(),
        ));
    }
    validate_title(title)
}

fn context() -> Value {
    let mut context = activity::quote_context();
    context
        .as_array_mut()
        .expect("quote context is an array")
        .push(json!({
            "sc":"http://schema.org/", "pt":"https://joinpeertube.org/ns#",
            "subtitleLanguage":"sc:subtitleLanguage", "language":"sc:inLanguage",
            "identifier":"sc:identifier", "isLiveBroadcast":"sc:isLiveBroadcast",
            "uuid":{"@type":"sc:identifier","@id":"pt:uuid"},
            "views":{"@type":"sc:Number","@id":"pt:views"},
            "aspectRatio":{"@type":"sc:Float","@id":"pt:aspectRatio"},
            "state":{"@type":"sc:Number","@id":"pt:state"},
            "size":{"@type":"sc:Number","@id":"pt:size"},
            "commentsPolicy":{"@type":"sc:Number","@id":"pt:commentsPolicy"},
            "downloadEnabled":{"@type":"sc:Boolean","@id":"pt:downloadEnabled"},
            "waitTranscoding":{"@type":"sc:Boolean","@id":"pt:waitTranscoding"}
        }));
    context
}

/// Include subscribers to the channel in subsequent owner-signed mutations.
/// Capture before deletion removes the publication sidecar.
pub async fn follower_inboxes(
    conn: &mut plamenu_db::PgConnection,
    status_id: i64,
) -> Result<Vec<String>, ApiError> {
    let Some(publication) = video::publications(&mut *conn, &[status_id]).await?.pop() else {
        return Ok(Vec::new());
    };
    Ok(plamenu_db::follow::follower_inboxes(conn, publication.channel_id).await?)
}

pub async fn announce(
    conn: &mut plamenu_db::PgConnection,
    publication: &video::Publication,
    object: &Value,
) -> Result<i64, ApiError> {
    let uri = format!(
        "{}/videos/{}/announce",
        publication.channel_uri, publication.uuid
    );
    let stream_id =
        video::create_announcement(conn, publication.channel_id, publication.status_id, &uri)
            .await?;
    let mut cc = object["cc"].as_array().cloned().unwrap_or_default();
    cc.push(json!(format!("{}/followers", publication.channel_uri)));
    let activity = json!({
        "@context": activity::quote_context(),
        "id":uri,
        "type":"Announce", "actor":publication.channel_uri,
        "to":object["to"], "cc":cc,
        "object":object["id"], "published":object["published"]
    });
    let inboxes = plamenu_db::follow::follower_inboxes(&mut *conn, publication.channel_id).await?;
    plamenu_db::job::enqueue_many_tx(&mut *conn, publication.channel_id, &inboxes, &activity)
        .await?;
    Ok(stream_id)
}
