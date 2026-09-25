//! Native video publication, shared by first-party forms and client APIs.
//! Channel identity is distinct from both its owning person and communities.

use base64::Engine;
use plamenu_ap::{activity, actor::Actor};
use plamenu_db::{
    account::{self, Account, NewLocalAccount},
    media::Media,
    video,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{AppState, error::ApiError};

/// `PeerTube`'s supported description envelope, separate from short social posts.
pub const MAX_DESCRIPTION_CHARACTERS: i32 = 10_000;

/// A changed local caption body must change its AP Link, or `PeerTube` may keep
/// the former text when it receives an otherwise identical Video Update.
pub(crate) fn caption_public_url(
    domain: &str,
    media_id: i64,
    track: &plamenu_db::video_captions::Caption,
) -> String {
    let path = if track.legacy {
        format!("https://{domain}/media/{media_id}/captions.vtt")
    } else {
        format!(
            "https://{domain}/media/{media_id}/captions/{}.vtt",
            track.language
        )
    };
    if let Some(body) = &track.vtt_body {
        let digest = Sha256::digest(body.as_bytes());
        let revision = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&digest[..12]);
        format!("{path}?v={revision}")
    } else {
        path
    }
}

/// A content-derived collection URL prevents a remote AP cache from keeping
/// the prior chapter timeline after `Update(Video)`.
pub(crate) fn chapter_collection_url(
    object_id: &str,
    chapters: &[plamenu_db::video_chapters::Chapter],
) -> String {
    let mut digest = Sha256::new();
    for chapter in chapters {
        digest.update(chapter.start_seconds.to_be_bytes());
        digest.update((chapter.title.len() as u64).to_be_bytes());
        digest.update(chapter.title.as_bytes());
    }
    let hash = digest.finalize();
    let revision = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&hash[..12]);
    format!("{object_id}/chapters?v={revision}")
}

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
    let ready_file = media.first().is_some_and(|item| {
        item.content_type == "video/mp4"
            && item.processing == "complete"
            && item.file_name.is_some()
            && item.small_file_name.is_some()
            && item
                .duration
                .is_some_and(|duration| duration.is_finite() && duration > 0.0)
    });
    let waiting_live = media.first().is_some_and(|item| {
        item.content_type == "application/x-mpegURL"
            && item.kind_or_derived() == "video"
            && item.processing == "complete"
            && item.live_state.as_deref() == Some("waiting")
            && item.file_name.is_none()
    });
    if media.len() != 1 || !(ready_file || waiting_live) {
        return Err(ApiError::Unprocessable(
            "Choose one processed video with a thumbnail, or one waiting live broadcast".into(),
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
#[allow(
    clippy::too_many_lines,
    reason = "one canonical Video shape for Create, Update and dereferencing"
)]
pub fn enrich_object(
    domain: &str,
    note: &mut Value,
    publication: &video::Publication,
    media: &Media,
    hls: &[plamenu_db::native_video_hls::FileTag],
    captions: &[plamenu_db::video_captions::Caption],
    chapters: &[plamenu_db::video_chapters::Chapter],
) -> Result<(), ApiError> {
    let live = media.live_state.is_some();
    let file = if live {
        None
    } else {
        Some(media.file_name.as_deref().ok_or(ApiError::NotFound)?)
    };
    let poster = publication
        .poster_file_name
        .as_deref()
        .or(media.small_file_name.as_deref());
    if !live && poster.is_none() {
        return Err(ApiError::NotFound);
    }
    let author = note["attributedTo"].clone();
    let web = note["url"].clone();
    let published = note["published"].clone();
    let language = note["contentMap"]
        .as_object()
        .and_then(|m| m.keys().next())
        .cloned();
    note["type"] = json!("Video");
    // PeerTube's Video wire format uses dereferenceable collection URL
    // strings, whereas an ordinary Note embeds count-only collections.
    // Supplying the Note object makes its rate-sync request treat that object
    // as Got options instead of a URL and reject the Update. Both rates must
    // be present: PeerTube 8.3 passes missing counts to a required SQL bind.
    let object_id = note["id"].as_str().ok_or(ApiError::NotFound)?.to_owned();
    note["likes"] = json!(format!("{object_id}/likes"));
    note["dislikes"] = json!(format!("{object_id}/dislikes"));
    note["shares"] = json!(format!("{object_id}/shares"));
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
    if let Some(file) = file {
        urls.push(json!(
            {"type":"Link", "mediaType":"video/mp4", "href":format!("https://{domain}/media/{file}"),
             "height":media.height.unwrap_or(0), "width":media.width.unwrap_or(0), "size":publication.file_size}
        ));
    }
    if !hls.is_empty() {
        let mut files: Vec<Value> = hls
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
        if hls.iter().all(|rendition| rendition.has_segment_sha256) {
            for rendition in hls {
                files.push(json!({
                    "type": "Infohash",
                    "name": plamenu_db::native_video_hls::p2p_infohash(&publication.uuid, rendition.height),
                }));
            }
            files.push(json!({
                "type": "Link", "name": "sha256", "mediaType": "application/json",
                "href": format!("https://{domain}/media/hls/{}/sha256.json", media.id),
            }));
            urls.push(json!({
                "type": "Link", "name": "tracker-websocket",
                "rel": ["tracker", "websocket"],
                "href": format!("wss://{domain}/tracker/socket"),
            }));
        }
        urls.push(json!({
            "type": "Link", "mediaType": "application/x-mpegURL",
            "href": format!("https://{domain}/media/hls/{}/master.m3u8", media.id),
            "tag": files,
        }));
    } else if live && media.live_state.as_deref() == Some("live") {
        let mut tags = Vec::new();
        if publication.live_latency_mode != Some(3) {
            tags.push(json!({
                "type": "Infohash",
                "name": plamenu_db::native_video_hls::p2p_infohash(&publication.uuid, 0),
            }));
            tags.push(json!({
                "type": "Link", "name": "sha256", "mediaType": "application/json",
                "href": format!("https://{domain}/media/hls/{}/sha256.json", media.id),
            }));
            urls.push(json!({
                "type": "Link", "name": "tracker-websocket",
                "rel": ["tracker", "websocket"],
                "href": format!("wss://{domain}/tracker/socket"),
            }));
        }
        urls.push(json!({
            "type": "Link", "mediaType": "application/x-mpegURL",
            "href": format!("https://{domain}/media/hls/{}/master.m3u8", media.id),
            "tag": tags,
        }));
    }
    note["url"] = json!(urls);
    if let Some(poster) = poster {
        let mut icon = json!({"type":"Image", "mediaType":crate::media_processing::content_type_for(poster),
            "url":format!("https://{domain}/media/{poster}"),
            "width":publication.poster_width.or(media.small_width).unwrap_or(0),
            "height":publication.poster_height.or(media.small_height).unwrap_or(0)});
        if let Some(alt) = publication.poster_alt.as_deref() {
            icon["name"] = json!(alt);
        }
        note["icon"] = json!([icon]);
    } else if live {
        // PeerTube requires at least one image icon even before a live has
        // produced a frame. The bundled, always-available brand image keeps
        // waiting announcements valid without fabricating a video file.
        note["icon"] = json!([{
            "type": "Image", "mediaType": "image/png",
            "url": format!("https://{domain}/pwa/icon-512.png"),
            "width": 512, "height": 512,
            "name": "Live broadcast placeholder",
        }]);
    }
    note["updated"] = note
        .get("updated")
        .cloned()
        .filter(|v| !v.is_null())
        .unwrap_or(published);
    note["views"] = json!(0);
    note["state"] = json!(match media.live_state.as_deref() {
        Some("waiting" | "interrupted") => 4,
        Some("ended") => 5,
        _ => 1,
    });
    note["isLiveBroadcast"] = json!(live);
    if live {
        note["permanentLive"] = json!(media.live_permanent);
        if let Some(seconds) = publication.live_dvr_window_seconds {
            note["dvrWindow"] = json!(format!("PT{seconds}S"));
        }
        if let Some(mode) = publication.live_latency_mode {
            note["latencyMode"] = json!(mode);
        }
        note["schedules"] = json!(publication.live_scheduled_starts.as_deref().unwrap_or_default()
            .iter().map(|at| json!({"startDate": at.format(&time::format_description::well_known::Rfc3339).unwrap_or_default()}))
            .collect::<Vec<_>>());
        note["attachment"] = json!([]);
    }
    note["commentsPolicy"] = json!(1);
    note["downloadEnabled"] = json!(!live);
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
    note["subtitleLanguage"] = json!(
        captions
            .iter()
            .take(16)
            .map(|track| {
                let href = caption_public_url(domain, media.id, track);
                json!({
                    "identifier": track.language,
                    "name": track.language,
                    "automaticallyGenerated": track.automatically_generated,
                    "url": [{"type":"Link", "mediaType":"text/vtt", "href":href}],
                })
            })
            .collect::<Vec<_>>()
    );
    note["hasParts"] = json!(chapter_collection_url(&object_id, chapters));
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
            "hasParts":"sc:hasParts",
            "identifier":"sc:identifier", "isLiveBroadcast":"sc:isLiveBroadcast",
            "uuid":{"@type":"sc:identifier","@id":"pt:uuid"},
            "views":{"@type":"sc:Number","@id":"pt:views"},
            "aspectRatio":{"@type":"sc:Float","@id":"pt:aspectRatio"},
            "state":{"@type":"sc:Number","@id":"pt:state"},
            "latencyMode":{"@type":"sc:Number","@id":"pt:latencyMode"},
            "dvrWindow":{"@type":"sc:Duration","@id":"pt:dvrWindow"},
            "schedules":"sc:eventSchedule", "startDate":"sc:startDate",
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

#[cfg(test)]
mod tests {
    use super::*;
    use plamenu_db::video_chapters::Chapter;

    #[test]
    fn chapter_collection_revision_changes_with_content() {
        let base = "https://example.test/ap/accounts/1/statuses/2";
        let first = Chapter {
            media_id: 7,
            start_seconds: 0,
            title: "Opening".into(),
        };
        let initial = chapter_collection_url(base, std::slice::from_ref(&first));
        assert_eq!(
            initial,
            chapter_collection_url(base, std::slice::from_ref(&first))
        );
        assert!(initial.starts_with(&format!("{base}/chapters?v=")));
        let renamed = Chapter {
            title: "Changed".into(),
            ..first.clone()
        };
        assert_ne!(initial, chapter_collection_url(base, &[renamed]));
        assert_ne!(initial, chapter_collection_url(base, &[]));
    }
}
