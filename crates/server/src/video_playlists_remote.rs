//! Only canonical, same-origin auxiliary metadata is imported. No Video media
//! is downloaded here, and no database authority lock spans a network request.
use crate::{AppState, entities, error::ApiError};
use futures_util::{FutureExt, StreamExt, TryStreamExt, stream};
use plamenu_db::{
    account::Account,
    id,
    video_playlists::{self as db, Entry, Playlist},
};
use serde_json::Value;
use std::collections::HashSet;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

fn invalid(message: &str) -> ApiError {
    ApiError::Unprocessable(message.into())
}
fn value_id(v: &Value) -> Option<&str> {
    v.as_str().or_else(|| v.get("id").and_then(Value::as_str))
}
fn same_origin(root: &str, target: &str) -> bool {
    let (Ok(a), Ok(b)) = (url::Url::parse(root), url::Url::parse(target)) else {
        return false;
    };
    plamenu_federation::is_federation_url(target)
        && target.len() <= 2048
        && a.origin() == b.origin()
        && b.username().is_empty()
        && b.password().is_none()
}
async fn fetch(state: &AppState, root: &str, target: &str) -> Result<(Value, usize), ApiError> {
    if !same_origin(root, target)
        || !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, target)
            .await?
    {
        return Err(invalid(
            "Playlist metadata must stay on its publishing origin",
        ));
    }
    let fetched = state
        .federation
        .fetch_activitypub_same_origin(target)
        .await
        .map_err(|_| invalid("Playlist metadata could not be fetched safely"))?;
    if fetched.body_bytes > 2 * 1024 * 1024 || !same_origin(root, &fetched.final_url) {
        return Err(invalid("Playlist response exceeded its safety limit"));
    }
    let value = fetched
        .document
        .ok_or_else(|| invalid("Playlist metadata is missing"))?;
    if value.get("id").and_then(Value::as_str) != Some(target) {
        return Err(invalid("Playlist metadata identity does not match its URL"));
    }
    Ok((value, fetched.body_bytes))
}

struct Imported {
    root: Value,
    owner: Account,
    channel: Account,
    entries: Vec<Entry>,
    updated: OffsetDateTime,
    published: OffsetDateTime,
    visibility: &'static str,
}

#[allow(
    clippy::too_many_lines,
    reason = "bounded remote collection traversal with identity checks"
)]
async fn collect(state: &AppState, url: &str) -> Result<Imported, ApiError> {
    if url.starts_with(&format!("https://{}/", state.config.domain)) {
        return Err(invalid(
            "Open local playlists directly instead of importing them",
        ));
    }
    let (root, mut bytes) = fetch(state, url, url).await?;
    if root["type"] != "Playlist" {
        return Err(invalid("This URL is not an ActivityPub Video Playlist"));
    }
    let total = root["totalItems"]
        .as_u64()
        .and_then(|v| usize::try_from(v).ok())
        .filter(|v| *v <= db::MAX_ENTRIES)
        .ok_or_else(|| invalid("Playlist exceeds the 1000-entry import limit"))?;
    let public = |key: &str| {
        root[key].as_array().is_some_and(|a| {
            a.iter()
                .any(|v| v.as_str() == Some(plamenu_ap::activity::PUBLIC))
        })
    };
    let visibility = if public("to") {
        "public"
    } else if public("cc") {
        "unlisted"
    } else {
        return Err(invalid(
            "Only public and unlisted playlists may be imported",
        ));
    };
    let channel_url = root
        .get("audience")
        .and_then(value_id)
        .or_else(|| {
            root["attributedTo"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(value_id)
        })
        .ok_or_else(|| invalid("Playlist has no publishing channel"))?;
    if !same_origin(url, channel_url)
        || !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, channel_url)
            .await?
    {
        return Err(invalid("Playlist channel origin is invalid"));
    }
    let channel_actor = state
        .federation
        .fetch_actor(channel_url)
        .await
        .map_err(|_| invalid("Playlist channel unavailable"))?;
    if channel_actor.id != channel_url || channel_actor.kind != "Group" {
        return Err(invalid("Playlist channel is not a verified Group"));
    }
    let attributed = channel_actor
        .attributed_to
        .as_ref()
        .ok_or_else(|| invalid("Playlist channel has no verified owner"))?;
    let owner_url = if let Some(a) = attributed.as_array() {
        a.iter().find_map(value_id)
    } else {
        value_id(attributed)
    }
    .ok_or_else(|| invalid("Playlist channel has no verified owner"))?;
    if !same_origin(url, owner_url)
        || !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, owner_url)
            .await?
    {
        return Err(invalid("Playlist owner origin is invalid"));
    }
    let owner_actor = state
        .federation
        .fetch_actor(owner_url)
        .await
        .map_err(|_| invalid("Playlist owner unavailable"))?;
    if owner_actor.id != owner_url
        || !matches!(
            owner_actor.kind.as_str(),
            "Person" | "Service" | "Application"
        )
    {
        return Err(invalid("Playlist owner identity is invalid"));
    }
    let owner = crate::remote::refresh_remote_actor(state, &owner_actor).await?;
    let channel = crate::remote::refresh_remote_actor(state, &channel_actor).await?;
    if owner.suspended() || channel.suspended() {
        return Err(ApiError::NotFound);
    }
    let date = |key: &str| {
        root[key]
            .as_str()
            .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
            .ok_or_else(|| invalid("Playlist dates are invalid"))
    };
    let published = date("published")?;
    let updated = if root.get("updated").is_some() {
        date("updated")?
    } else {
        published
    };
    let mut urls = Vec::<String>::new();
    let mut page = root.clone();
    let mut visited = HashSet::new();
    for _ in 0..=40 {
        if let Some(items) = page.get("orderedItems").or_else(|| page.get("items")) {
            let items = items
                .as_array()
                .ok_or_else(|| invalid("Playlist page entries are invalid"))?;
            for item in items {
                let entry = value_id(item)
                    .filter(|u| same_origin(url, u))
                    .ok_or_else(|| invalid("Playlist entry origin is invalid"))?;
                if urls.len() >= db::MAX_ENTRIES || urls.iter().any(|u| u == entry) {
                    return Err(invalid(
                        "Playlist exceeds its limit or repeats an entry identity",
                    ));
                }
                urls.push(entry.to_owned());
            }
        }
        let next = page
            .get("next")
            .or_else(|| {
                if visited.is_empty() {
                    page.get("first")
                } else {
                    None
                }
            })
            .and_then(value_id);
        let Some(next) = next else {
            break;
        };
        if !visited.insert(next.to_owned()) || visited.len() > 40 {
            return Err(invalid("Playlist pagination loops or exceeds 40 pages"));
        }
        let fetched = fetch(state, url, next).await?;
        bytes = bytes.saturating_add(fetched.1);
        if bytes > 8 * 1024 * 1024 {
            return Err(invalid("Playlist exceeds its aggregate metadata budget"));
        }
        page = fetched.0;
        if page.get("partOf").and_then(value_id) != Some(url)
            || !matches!(
                page.get("type").and_then(Value::as_str),
                Some("Playlist" | "OrderedCollectionPage" | "CollectionPage")
            )
        {
            return Err(invalid("Playlist page belongs to another collection"));
        }
    }
    if urls.len() != total {
        return Err(invalid(
            "Playlist entry count does not match its collection",
        ));
    }
    let requests = urls
        .iter()
        .cloned()
        .map(|target| async move { fetch(state, url, &target).await }.boxed())
        .collect::<Vec<_>>();
    let mut requests = stream::iter(requests).buffered(8);
    let mut positions = HashSet::new();
    let mut entries = Vec::with_capacity(urls.len());
    for url in urls {
        let (element, size) = requests
            .try_next()
            .await?
            .ok_or_else(|| invalid("Playlist entry is missing"))?;
        bytes = bytes.saturating_add(size);
        if bytes > 8 * 1024 * 1024 || element["type"] != "PlaylistElement" {
            return Err(invalid(
                "Playlist entries exceed their metadata limit or have an invalid type",
            ));
        }
        let position = element["position"]
            .as_i64()
            .and_then(|v| i32::try_from(v).ok())
            .filter(|v| *v >= 1 && *v <= 1000)
            .ok_or_else(|| invalid("Playlist position is invalid"))?;
        if !positions.insert(position) {
            return Err(invalid("Playlist entry positions conflict"));
        }
        let timestamp = |key: &str| -> Result<Option<i32>, ApiError> {
            element
                .get(key)
                .map(|v| {
                    v.as_i64()
                        .and_then(|v| i32::try_from(v).ok())
                        .ok_or_else(|| invalid("Playlist clip boundary is invalid"))
                })
                .transpose()
        };
        let start = timestamp("startTimestamp")?.unwrap_or(0);
        let stop = timestamp("stopTimestamp")?;
        if !db::valid_clip(start, stop, None) {
            return Err(invalid("Playlist clip boundary is invalid"));
        }
        let video = element.get("url").and_then(Value::as_str).unwrap_or("");
        if !video.is_empty()
            && (video.len() > 2048 || !plamenu_federation::is_federation_url(video))
        {
            return Err(invalid("Playlist Video URL is invalid"));
        }
        // Resolve existing metadata only; never fetch a thousand Videos eagerly.
        let status = if video.is_empty() {
            None
        } else {
            crate::ingest::resolve_status_ref(state, video)
                .await?
                .filter(|s| s.object_type.as_deref() == Some("Video"))
        };
        entries.push(Entry {
            id: id::next(),
            playlist_id: 0,
            remote_uri: Some(url),
            status_id: status.map(|s| s.id),
            video_uri: video.to_owned(),
            position,
            start_seconds: start,
            stop_seconds: stop,
        });
    }
    entries.sort_by_key(|e| e.position);
    Ok(Imported {
        root,
        owner,
        channel,
        entries,
        updated,
        published,
        visibility,
    })
}

pub async fn import(
    state: &AppState,
    url: &str,
    sender: Option<&Account>,
) -> Result<Playlist, ApiError> {
    // Aggregate timeout also bounds a malicious origin that responds slowly to
    // many otherwise-small metadata requests; eight auxiliary fetches at most.
    let incoming = tokio::time::timeout(std::time::Duration::from_secs(45), collect(state, url))
        .await
        .map_err(|_| {
            invalid("Playlist import timed out without changing the saved collection")
        })??;
    if sender.is_some_and(|a| a.id != incoming.owner.id) {
        return Err(ApiError::NotFound);
    }
    let name = incoming.root["name"].as_str().unwrap_or("").trim();
    let description = incoming.root["content"].as_str().unwrap_or("");
    let uuid = incoming.root["uuid"]
        .as_str()
        .filter(|v| {
            v.len() == 36
                && v.bytes().enumerate().all(|(i, b)| {
                    if matches!(i, 8 | 13 | 18 | 23) {
                        b == b'-'
                    } else {
                        b.is_ascii_hexdigit()
                    }
                })
        })
        .ok_or_else(|| invalid("Playlist UUID is invalid"))?;
    if name.is_empty() || name.chars().count() > 120 || description.chars().count() > 4000 {
        return Err(invalid("Playlist title or description exceeds its limit"));
    }
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    // Serialize an initially-unknown URI with its Delete as well as other imports.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,94409))")
        .bind(url)
        .execute(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?;
    let tomb: Option<OffsetDateTime> = sqlx::query_scalar(
        "SELECT deleted_at FROM video_playlist_tombstones WHERE uri=$1 AND actor_uri=$2",
    )
    .bind(url)
    .bind(entities::account_uri(&state.config.domain, &incoming.owner))
    .fetch_optional(&mut *tx)
    .await
    .map_err(plamenu_db::DbError::from)?;
    if tomb.is_some_and(|time| incoming.updated <= time) {
        return Err(ApiError::NotFound);
    }
    let previous = db::by_uri(&mut *tx, url).await?;
    if let Some(p) = &previous
        && incoming.updated <= p.updated_at
    {
        if p.deleted_at.is_none()
            && incoming.owner.id != p.creator_id
            && incoming.channel.id == p.channel_id
        {
            let updated:Playlist=sqlx::query_as("UPDATE video_playlists SET creator_id=$2,revision=revision+1 WHERE id=$1 RETURNING *").bind(p.id).bind(incoming.owner.id).fetch_one(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
            tx.commit().await.map_err(plamenu_db::DbError::from)?;
            return Ok(updated);
        }
        return if p.deleted_at.is_some() {
            Err(ApiError::NotFound)
        } else {
            Ok(p.clone())
        };
    }
    let playlist_id = previous.as_ref().map_or_else(id::next, |p| p.id);
    let playlist:Playlist=sqlx::query_as("INSERT INTO video_playlists(id,uuid,creator_id,channel_id,remote_uri,name,description,visibility,created_at,updated_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) ON CONFLICT(remote_uri) DO UPDATE SET creator_id=EXCLUDED.creator_id,channel_id=EXCLUDED.channel_id,name=EXCLUDED.name,description=EXCLUDED.description,visibility=EXCLUDED.visibility,updated_at=EXCLUDED.updated_at,deleted_at=NULL,revision=video_playlists.revision+1 RETURNING *")
        .bind(playlist_id).bind(uuid).bind(incoming.owner.id).bind(incoming.channel.id).bind(url).bind(name).bind(description).bind(incoming.visibility).bind(incoming.published).bind(incoming.updated).fetch_one(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
    let old = db::entries(&mut *tx, playlist.id).await?;
    let mut entries = incoming.entries;
    for entry in &mut entries {
        entry.id = old
            .iter()
            .find(|e| e.remote_uri == entry.remote_uri)
            .map_or(entry.id, |e| e.id);
    }
    sqlx::query("DELETE FROM video_playlist_entries WHERE playlist_id=$1")
        .bind(playlist.id)
        .execute(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?;
    // Bulk insert keeps a large collection from holding a transaction for one
    // database round trip per entry.
    sqlx::query("INSERT INTO video_playlist_entries(id,playlist_id,remote_uri,status_id,video_uri,position,start_seconds,stop_seconds) SELECT id,$1,uri,status,video,position,start,stop FROM unnest($2::bigint[],$3::text[],$4::bigint[],$5::text[],$6::integer[],$7::integer[],$8::integer[]) AS e(id,uri,status,video,position,start,stop)")
        .bind(playlist.id).bind(entries.iter().map(|e|e.id).collect::<Vec<_>>()).bind(entries.iter().map(|e|e.remote_uri.clone()).collect::<Vec<_>>()).bind(entries.iter().map(|e|e.status_id).collect::<Vec<_>>()).bind(entries.iter().map(|e|e.video_uri.clone()).collect::<Vec<_>>()).bind(entries.iter().map(|e|e.position).collect::<Vec<_>>()).bind(entries.iter().map(|e|e.start_seconds).collect::<Vec<_>>()).bind(entries.iter().map(|e|e.stop_seconds).collect::<Vec<_>>()).execute(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(playlist)
}

pub async fn delete(state: &AppState, sender: &Account, url: &str) -> Result<bool, ApiError> {
    let actor = entities::account_uri(&state.config.domain, sender);
    if sender.is_local() || !same_origin(&actor, url) {
        return Ok(false);
    }
    let initial = db::by_uri(&state.pool, url).await?;
    let mut current_owner_matches = None;
    if let Some(p) = initial.as_ref() {
        let channel = plamenu_db::account::find_by_id(&state.pool, p.channel_id)
            .await?
            .ok_or(ApiError::NotFound)?;
        let channel_uri = entities::account_uri(&state.config.domain, &channel);
        current_owner_matches = Some(false);
        if same_origin(url, &channel_uri) {
            let fresh = state
                .federation
                .fetch_actor(&channel_uri)
                .await
                .map_err(|error| ApiError::Internal(Box::new(error)))?;
            // A cached creator is not authority after a channel handoff. A
            // successfully refreshed Group overrides that cache in both
            // directions. An unavailable origin must fail closed, rather than
            // restore a former owner's authority from stale cached metadata.
            if fresh.id == channel_uri && fresh.kind == "Group" {
                current_owner_matches = Some(fresh.attributed_to.as_ref().is_some_and(|v| {
                    if let Some(a) = v.as_array() {
                        a.iter().any(|v| value_id(v) == Some(actor.as_str()))
                    } else {
                        value_id(v) == Some(actor.as_str())
                    }
                }));
            }
        }
    }
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,94409))")
        .bind(url)
        .execute(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?;
    let previous = db::by_uri(&mut *tx, url).await?;
    if let Some(p) = previous {
        if !current_owner_matches.unwrap_or(p.creator_id == sender.id) {
            return Ok(true);
        }
        sqlx::query("UPDATE video_playlists SET deleted_at=now(),creator_id=$2 WHERE id=$1")
            .bind(p.id)
            .bind(sender.id)
            .execute(&mut *tx)
            .await
            .map_err(plamenu_db::DbError::from)?;
        sqlx::query("DELETE FROM video_playlist_entries WHERE playlist_id=$1")
            .bind(p.id)
            .execute(&mut *tx)
            .await
            .map_err(plamenu_db::DbError::from)?;
    } else {
        // Unknown-object Deletes share the generic status path too, but this
        // small URI fence is checked only against a subsequently verified owner.
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM video_playlist_tombstones WHERE actor_uri=$1")
                .bind(&actor)
                .fetch_one(&mut *tx)
                .await
                .map_err(plamenu_db::DbError::from)?;
        if count >= 1000 {
            return Ok(false);
        }
    }
    sqlx::query("INSERT INTO video_playlist_tombstones(uri,actor_uri) VALUES($1,$2) ON CONFLICT(uri) DO UPDATE SET actor_uri=EXCLUDED.actor_uri,deleted_at=now()")
        .bind(url).bind(actor).execute(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(db::by_uri(&state.pool, url).await?.is_some())
}

/// Resolve only the entry being opened. Save metadata identity conditionally:
/// an import/reorder/delete racing the fetch must not resurrect an old entry.
pub async fn resolve_entry(
    state: &AppState,
    p: &Playlist,
    e: &Entry,
    viewer: Option<i64>,
) -> Result<Option<plamenu_db::status::Status>, ApiError> {
    if !crate::video_playlists::can_read(state, p, viewer).await? {
        return Err(ApiError::NotFound);
    }
    let stored = if let Some(id) = e.status_id {
        plamenu_db::status::find_by_id(&state.pool, id).await?
    } else if e.video_uri.is_empty() {
        None
    } else {
        crate::ingest::resolve_or_fetch_status_for_account(state, &e.video_uri, viewer)
            .await
            .unwrap_or(None)
    };
    let Some(stored) = stored.filter(|s| s.object_type.as_deref() == Some("Video")) else {
        return Ok(None);
    };
    if plamenu_db::status::is_deleted(&state.pool, stored.id).await? {
        return Ok(None);
    }
    if !entities::can_view(&state.pool, &stored, viewer).await?
        || (p.visibility != "private"
            && !matches!(stored.visibility.as_str(), "public" | "unlisted"))
    {
        return Ok(None);
    }
    let duration: Option<f64> =
        sqlx::query_scalar("SELECT max(duration) FROM media_attachments WHERE status_id=$1")
            .bind(stored.id)
            .fetch_one(&state.pool)
            .await
            .map_err(plamenu_db::DbError::from)?;
    if !db::valid_clip(e.start_seconds, e.stop_seconds, duration) {
        return Ok(None);
    }
    if e.status_id.is_none() {
        sqlx::query("UPDATE video_playlist_entries SET status_id=$3 WHERE id=$1 AND playlist_id=$2 AND video_uri=$4 AND EXISTS(SELECT 1 FROM video_playlists WHERE id=$2 AND deleted_at IS NULL)").bind(e.id).bind(p.id).bind(stored.id).bind(&e.video_uri).execute(&state.pool).await.map_err(plamenu_db::DbError::from)?;
    }
    Ok(Some(stored))
}
