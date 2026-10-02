//! Native ordered Video collections, consent-scoped channel authority and
//! metadata-only, bounded `PeerTube` Playlist interoperability.
use crate::{AppState, entities, error::ApiError};
use plamenu_db::{
    account::{self, Account},
    id, status, video_channel_access as access,
    video_playlists::{self as db, Entry, Playlist},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;

fn invalid(message: &str) -> ApiError {
    ApiError::Unprocessable(message.into())
}
#[must_use]
pub fn uri(state: &AppState, p: &Playlist) -> String {
    p.remote_uri.clone().unwrap_or_else(|| {
        format!(
            "https://{}/ap/video-playlists/{}",
            state.config.domain, p.id
        )
    })
}
fn entry_uri(state: &AppState, p: &Playlist, entry: &Entry) -> String {
    entry
        .remote_uri
        .clone()
        .unwrap_or_else(|| format!("{}/entries/{}", uri(state, p), entry.id))
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Change {
    #[serde(default)]
    pub operation: String,
    pub channel_id: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub visibility: Option<String>,
    pub video_url: Option<String>,
    pub status_id: Option<String>,
    pub entry_id: Option<String>,
    /// One-based target position, usable as a native numeric input.
    pub position: Option<i32>,
    pub start_seconds: Option<i32>,
    pub stop_seconds: Option<i32>,
    /// Every mutation requires the currently displayed revision.
    pub revision: Option<i64>,
    pub confirm: Option<String>,
}

fn fields(
    input: &Change,
    previous: Option<&Playlist>,
) -> Result<(String, String, String), ApiError> {
    let name = input
        .name
        .as_deref()
        .or_else(|| previous.map(|p| p.name.as_str()))
        .unwrap_or("")
        .trim();
    let description = input
        .description
        .as_deref()
        .or_else(|| previous.map(|p| p.description.as_str()))
        .unwrap_or("");
    let visibility = input
        .visibility
        .as_deref()
        .or_else(|| previous.map(|p| p.visibility.as_str()))
        .unwrap_or("private");
    if name.is_empty()
        || name.chars().count() > 120
        || description.chars().count() > 4000
        || !matches!(visibility, "public" | "unlisted" | "private")
    {
        return Err(invalid(
            "Use a title of 1–120 characters, a description up to 4000 characters, and public, unlisted or private visibility",
        ));
    }
    Ok((name.into(), description.into(), visibility.into()))
}
pub async fn can_edit(state: &AppState, p: &Playlist, actor: i64) -> Result<bool, ApiError> {
    if p.remote_uri.is_some() || p.deleted_at.is_some() {
        return Ok(false);
    }
    Ok(matches!(
        access::role(&state.pool, p.channel_id, actor)
            .await?
            .as_deref(),
        Some("owner" | "editor")
    ) || (p.creator_id == actor
        && access::role(&state.pool, p.channel_id, actor)
            .await?
            .as_deref()
            == Some("publisher")))
}
pub async fn can_read(
    state: &AppState,
    p: &Playlist,
    viewer: Option<i64>,
) -> Result<bool, ApiError> {
    if p.deleted_at.is_some()
        || !crate::instance_policy::account_id_visible(&state.pool, p.channel_id).await?
    {
        return Ok(false);
    }
    let owner: i64 = sqlx::query_scalar(
        "SELECT COALESCE((SELECT owner_account_id FROM video_channels WHERE account_id=$1),$2)",
    )
    .bind(p.channel_id)
    .bind(p.creator_id)
    .fetch_one(&state.pool)
    .await
    .map_err(plamenu_db::DbError::from)?;
    if !crate::instance_policy::account_id_visible(&state.pool, owner).await? {
        return Ok(false);
    }
    if let Some(viewer) = viewer
        && (plamenu_db::block::exists(&state.pool, p.channel_id, viewer).await?
            || plamenu_db::block::exists(&state.pool, owner, viewer).await?
            || plamenu_db::account_domain_block::blocks_account_domain(&state.pool, owner, viewer)
                .await?)
    {
        return Ok(false);
    }
    if p.visibility == "private" {
        return match viewer {
            Some(actor) => can_edit(state, p, actor).await,
            None => Ok(false),
        };
    }
    Ok(true)
}
pub async fn readable(
    state: &AppState,
    id: i64,
    viewer: Option<i64>,
) -> Result<Playlist, ApiError> {
    let p = db::find(&state.pool, id).await?.ok_or(ApiError::NotFound)?;
    if !can_read(state, &p, viewer).await? {
        return Err(ApiError::NotFound);
    }
    Ok(p)
}
pub async fn list(
    state: &AppState,
    viewer: Option<i64>,
    mine: bool,
    before: Option<i64>,
) -> Result<Vec<Playlist>, ApiError> {
    if mine && viewer.is_none() {
        return Err(ApiError::NotFound);
    }
    let rows:Vec<Playlist>=sqlx::query_as("SELECT p.* FROM video_playlists p WHERE p.deleted_at IS NULL AND ($3::bigint IS NULL OR p.id<$3) AND ((NOT $2 AND p.visibility='public') OR ($2 AND p.remote_uri IS NULL AND EXISTS (SELECT 1 FROM video_channels c LEFT JOIN video_channel_grants g ON g.channel_id=c.account_id AND g.account_id=$1 AND g.accepted WHERE c.account_id=p.channel_id AND (c.owner_account_id=$1 OR g.role='editor' OR (g.role='publisher' AND p.creator_id=$1))))) ORDER BY p.id DESC LIMIT 21")
        .bind(viewer).bind(mine).bind(before).fetch_all(&state.pool).await.map_err(plamenu_db::DbError::from)?;
    let mut result = Vec::new();
    for p in rows {
        if can_read(state, &p, viewer).await? {
            result.push(p);
        }
    }
    Ok(result)
}
pub async fn create(
    state: &AppState,
    actor: &Account,
    input: &Change,
) -> Result<Playlist, ApiError> {
    let channel = input
        .channel_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok())
        .ok_or_else(|| invalid("Choose a video channel"))?;
    let (name, description, visibility) = fields(input, None)?;
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    sqlx::query("SELECT account_id FROM video_channels WHERE account_id=$1 FOR UPDATE")
        .bind(channel)
        .fetch_optional(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?
        .ok_or(ApiError::NotFound)?;
    if access::lock_access(&mut tx, channel, actor.id)
        .await?
        .is_none()
    {
        return Err(ApiError::NotFound);
    }
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM video_playlists WHERE channel_id=$1 AND remote_uri IS NULL AND deleted_at IS NULL").bind(channel).fetch_one(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
    if count >= db::MAX_PLAYLISTS {
        return Err(invalid(
            "This channel has reached the limit of 100 playlists",
        ));
    }
    let p:Playlist=sqlx::query_as("INSERT INTO video_playlists(id,creator_id,channel_id,name,description,visibility) VALUES($1,$2,$3,$4,$5,$6) RETURNING *")
        .bind(id::next()).bind(actor.id).bind(channel).bind(name).bind(description).bind(visibility).fetch_one(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
    if p.visibility != "private" {
        publish(state, &mut tx, &p, "Create").await?;
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(p)
}

async fn video_for_add(
    state: &AppState,
    actor: i64,
    input: &Change,
) -> Result<Option<status::Status>, ApiError> {
    if input.operation != "add" {
        return Ok(None);
    }
    let stored = if let Some(id) = input.status_id.as_deref() {
        status::find_by_id(
            &state.pool,
            id.parse().map_err(|_| invalid("Invalid Video id"))?,
        )
        .await?
    } else {
        let url = input
            .video_url
            .as_deref()
            .ok_or_else(|| invalid("Enter a Video URL or id"))?
            .trim();
        // The URL a reader copies from the first-party page is a human
        // permalink, not necessarily the canonical ActivityPub identifier.
        let local = if let Some(path) = url
            .split(['?', '#'])
            .next()
            .unwrap_or(url)
            .strip_prefix(&format!("https://{}/@", state.config.domain))
        {
            if let Some((handle, id)) = path.split_once('/') {
                if let Ok(id) = id.parse::<i64>() {
                    if let Some(stored) = status::find_by_id(&state.pool, id).await? {
                        let account = account::find_by_id(&state.pool, stored.account_id)
                            .await?
                            .ok_or(ApiError::NotFound)?;
                        let expected = entities::account_acct(&state.config.domain, &account);
                        (handle == expected).then_some(stored)
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        if local.is_some() {
            local
        } else {
            crate::ingest::resolve_or_fetch_status_for_account(state, url, Some(actor)).await?
        }
    }
    .ok_or_else(|| invalid("Video unavailable"))?;
    if stored.object_type.as_deref() != Some("Video")
        || status::is_deleted(&state.pool, stored.id).await?
        || !entities::can_view(&state.pool, &stored, Some(actor)).await?
    {
        return Err(invalid("Choose a Video you can view"));
    }
    Ok(Some(stored))
}

#[allow(
    clippy::too_many_lines,
    reason = "one locked playlist mutation state machine"
)]
pub async fn change(
    state: &AppState,
    actor: &Account,
    id: i64,
    input: &Change,
) -> Result<Playlist, ApiError> {
    // Network dereference, if requested, happens before acquiring authority locks.
    let initial = db::find(&state.pool, id).await?.ok_or(ApiError::NotFound)?;
    if !can_edit(state, &initial, actor.id).await? {
        return Err(ApiError::NotFound);
    }
    let add = video_for_add(state, actor.id, input).await?;
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let role = access::lock_access(&mut tx, initial.channel_id, actor.id).await?;
    let p: Playlist = sqlx::query_as("SELECT * FROM video_playlists WHERE id=$1 FOR UPDATE")
        .bind(id)
        .fetch_one(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?;
    if p.deleted_at.is_some()
        || p.remote_uri.is_some()
        || !(matches!(role.as_deref(), Some("owner" | "editor"))
            || (role.as_deref() == Some("publisher") && p.creator_id == actor.id))
    {
        return Err(ApiError::NotFound);
    }
    if input.revision != Some(p.revision) {
        return Err(ApiError::Conflict("This playlist changed. Reload it before submitting again; your saved entries have not been overwritten".into()));
    }
    let mut entries = db::entries(&mut *tx, id).await?;
    match input.operation.as_str() {
        "profile" => {
            let (name, description, visibility) = fields(input, Some(&p))?;
            if visibility != "private" {
                let ids = entries
                    .iter()
                    .filter_map(|e| e.status_id)
                    .collect::<Vec<_>>();
                sqlx::query("SELECT id FROM statuses WHERE id=ANY($1) ORDER BY id FOR SHARE\n-- STUBKEEP: lock all candidate identities before validate_entries rechecks deletion and audience")
                    .bind(&ids)
                    .fetch_all(&mut *tx)
                    .await
                    .map_err(plamenu_db::DbError::from)?;
                validate_entries(state, &entries, actor.id, false).await?;
            }
            sqlx::query(
                "UPDATE video_playlists SET name=$2,description=$3,visibility=$4 WHERE id=$1",
            )
            .bind(id)
            .bind(name)
            .bind(description)
            .bind(visibility)
            .execute(&mut *tx)
            .await
            .map_err(plamenu_db::DbError::from)?;
        }
        "add" => {
            if entries.len() >= db::MAX_ENTRIES {
                return Err(invalid("A playlist can contain at most 1000 entries"));
            }
            let video = add.ok_or(ApiError::NotFound)?;
            // Lock the publication row: a concurrent audience edit or deletion
            // cannot race this transaction's public-collection validation.
            sqlx::query("SELECT id FROM statuses WHERE id=$1 FOR SHARE\n-- STUBKEEP: lock canonical identity before explicit is_deleted and audience checks")
                .bind(video.id)
                .fetch_one(&mut *tx)
                .await
                .map_err(plamenu_db::DbError::from)?;
            let video = status::find_by_id(&mut *tx, video.id)
                .await?
                .ok_or(ApiError::NotFound)?;
            if status::is_deleted(&mut *tx, video.id).await?
                || !entities::can_view(&state.pool, &video, Some(actor.id)).await?
                || (p.visibility != "private"
                    && !matches!(video.visibility.as_str(), "public" | "unlisted"))
            {
                return Err(invalid(
                    "A shared playlist can only contain public or unlisted Videos",
                ));
            }
            let duration: Option<f64> = sqlx::query_scalar(
                "SELECT max(duration) FROM media_attachments WHERE status_id=$1",
            )
            .bind(video.id)
            .fetch_one(&mut *tx)
            .await
            .map_err(plamenu_db::DbError::from)?;
            let start = input.start_seconds.unwrap_or(0);
            if !db::valid_clip(start, input.stop_seconds, duration)
                || (duration.is_none() && (start > 0 || input.stop_seconds.is_some()))
            {
                return Err(invalid(
                    "Clip times must fit a known Video duration; end must be later than start",
                ));
            }
            let author = account::find_by_id(&mut *tx, video.account_id)
                .await?
                .ok_or(ApiError::NotFound)?;
            let position = i32::try_from(entries.len() + 1).unwrap_or(1000);
            sqlx::query("INSERT INTO video_playlist_entries(id,playlist_id,status_id,video_uri,position,start_seconds,stop_seconds) VALUES($1,$2,$3,$4,$5,$6,$7)")
                .bind(id::next()).bind(id).bind(video.id).bind(entities::status_uri_for_account(&state.config.domain,&video,&author)).bind(position).bind(start).bind(input.stop_seconds).execute(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
        }
        "remove" | "move" | "clip" => {
            let entry = input
                .entry_id
                .as_deref()
                .and_then(|s| s.parse::<i64>().ok())
                .ok_or_else(|| invalid("Choose a playlist entry"))?;
            let index = entries
                .iter()
                .position(|e| e.id == entry)
                .ok_or(ApiError::NotFound)?;
            match input.operation.as_str() {
                "remove" => {
                    sqlx::query(
                        "DELETE FROM video_playlist_entries WHERE id=$1 AND playlist_id=$2",
                    )
                    .bind(entry)
                    .bind(id)
                    .execute(&mut *tx)
                    .await
                    .map_err(plamenu_db::DbError::from)?;
                    entries.remove(index);
                }
                "move" => {
                    let position = input.position.ok_or_else(|| invalid("Choose a position"))?;
                    if position < 1
                        || usize::try_from(position).unwrap_or(usize::MAX) > entries.len()
                    {
                        return Err(invalid("Position must be within the playlist"));
                    }
                    let moving = entries.remove(index);
                    entries.insert(usize::try_from(position - 1).unwrap_or(0), moving);
                }
                _ => {
                    let duration = if let Some(status) = entries[index].status_id {
                        let video = status::find_by_id(&mut *tx, status)
                            .await?
                            .ok_or(ApiError::NotFound)?;
                        if status::is_deleted(&mut *tx, status).await?
                            || !entities::can_view(&state.pool, &video, Some(actor.id)).await?
                        {
                            return Err(ApiError::NotFound);
                        }
                        sqlx::query_scalar::<_, Option<f64>>(
                            "SELECT max(duration) FROM media_attachments WHERE status_id=$1",
                        )
                        .bind(status)
                        .fetch_one(&mut *tx)
                        .await
                        .map_err(plamenu_db::DbError::from)?
                    } else {
                        None
                    };
                    let start = input.start_seconds.unwrap_or(0);
                    if !db::valid_clip(start, input.stop_seconds, duration)
                        || (duration.is_none() && (start > 0 || input.stop_seconds.is_some()))
                    {
                        return Err(invalid(
                            "Clip times must fit a known Video duration; end must be later than start",
                        ));
                    }
                    sqlx::query("UPDATE video_playlist_entries SET start_seconds=$2,stop_seconds=$3 WHERE id=$1").bind(entry).bind(start).bind(input.stop_seconds).execute(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
                }
            }
            // A single bounded batch, not one write per move/entry.
            let ids = entries.iter().map(|e| e.id).collect::<Vec<_>>();
            sqlx::query("UPDATE video_playlist_entries e SET position=r.position::integer FROM unnest($1::bigint[]) WITH ORDINALITY r(id,position) WHERE e.id=r.id AND e.playlist_id=$2").bind(&ids).bind(id).execute(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
        }
        "delete" => {
            if input.confirm.as_deref() != Some(p.name.as_str()) {
                return Err(invalid("Type the playlist title to confirm deletion"));
            }
            sqlx::query("UPDATE video_playlists SET deleted_at=now() WHERE id=$1")
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(plamenu_db::DbError::from)?;
            sqlx::query("DELETE FROM video_playlist_entries WHERE playlist_id=$1")
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(plamenu_db::DbError::from)?;
        }
        _ => return Err(invalid("Unknown playlist operation")),
    }
    let updated:Playlist=sqlx::query_as("UPDATE video_playlists SET revision=revision+1,updated_at=GREATEST(now(),updated_at+interval '1 millisecond') WHERE id=$1 RETURNING *").bind(id).fetch_one(&mut *tx).await.map_err(plamenu_db::DbError::from)?;
    if p.visibility != "private"
        && (updated.visibility == "private" || updated.deleted_at.is_some())
    {
        publish(state, &mut tx, &p, "Delete").await?;
    } else if updated.visibility != "private" && updated.deleted_at.is_none() {
        publish(
            state,
            &mut tx,
            &updated,
            if p.visibility == "private" {
                "Create"
            } else {
                "Update"
            },
        )
        .await?;
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(updated)
}

async fn validate_entries(
    state: &AppState,
    entries: &[Entry],
    actor: i64,
    private: bool,
) -> Result<(), ApiError> {
    let ids = entries
        .iter()
        .filter_map(|e| e.status_id)
        .collect::<Vec<_>>();
    let live: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM statuses WHERE id=ANY($1) AND deleted_at IS NULL\n-- STUBFILTER: exclude deleted statuses directly or through public_native_videos")
            .bind(&ids)
            .fetch_all(&state.pool)
            .await
            .map_err(plamenu_db::DbError::from)?;
    let stored = status::find_by_ids(&state.pool, &live).await?;
    let visible = entities::filter_viewable(&state.pool, &stored, Some(actor)).await?;
    if visible.len() != stored.len()
        || visible
            .iter()
            .any(|s| !private && !matches!(s.visibility.as_str(), "public" | "unlisted"))
    {
        return Err(invalid(
            "Remove restricted Videos before sharing this playlist",
        ));
    }
    Ok(())
}

pub async fn object(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    p: &Playlist,
    page: Option<usize>,
) -> Result<Value, ApiError> {
    let entries = db::entries(&mut *conn, p.id).await?;
    let channel = account::find_by_id(&mut *conn, p.channel_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let url = uri(state, p);
    let to = if p.visibility == "public" {
        vec![plamenu_ap::activity::PUBLIC.to_owned()]
    } else {
        Vec::new()
    };
    let cc = vec![plamenu_ap::activity::PUBLIC.to_owned()];
    let mut context = crate::video::context();
    if let Some(terms) = context
        .as_array_mut()
        .and_then(|a| a.last_mut())
        .and_then(Value::as_object_mut)
    {
        terms.extend(json!({"Playlist":"pt:Playlist","PlaylistElement":"pt:PlaylistElement","position":"pt:position","startTimestamp":"pt:startTimestamp","stopTimestamp":"pt:stopTimestamp","videoChannelPosition":"pt:videoChannelPosition"}).as_object().unwrap().clone());
    }
    let channel_uri = entities::account_uri(&state.config.domain, &channel);
    let mut value = json!({"@context":context,"id":url,"type":"Playlist","uuid":p.uuid,"name":p.name,"content":p.description,"mediaType":"text/plain","published":p.created_at.format(&Rfc3339).unwrap_or_default(),"updated":p.updated_at.format(&Rfc3339).unwrap_or_default(),"audience":channel_uri,"attributedTo":[channel_uri],"to":to,"cc":cc,"totalItems":entries.len(),"first":format!("{url}/pages/1"),"videoChannelPosition":1});
    // PeerTube represents a cleared optional description as null and rejects
    // an empty string. Retain the empty native/API source without inventing text.
    if p.description.is_empty() {
        value["content"] = Value::Null;
    }
    if let Some(page) = page {
        let begin = page.saturating_sub(1).saturating_mul(25);
        if page == 0 || begin >= db::MAX_ENTRIES {
            return Err(ApiError::NotFound);
        }
        value["id"] = json!(format!("{url}/pages/{page}"));
        value["type"] = json!("OrderedCollectionPage");
        value["partOf"] = json!(url);
        value["orderedItems"] = json!(
            entries
                .iter()
                .skip(begin)
                .take(25)
                .map(|e| entry_uri(state, p, e))
                .collect::<Vec<_>>()
        );
        if begin + 25 < entries.len() {
            value["next"] = json!(format!("{url}/pages/{}", page + 1));
        }
    }
    Ok(value)
}
#[must_use]
pub fn element(state: &AppState, p: &Playlist, e: &Entry) -> Value {
    let mut value = json!({"@context":crate::video::context(),"type":"PlaylistElement","id":entry_uri(state,p,e),"url":e.video_uri,"position":e.position});
    if e.start_seconds > 0 {
        value["startTimestamp"] = json!(e.start_seconds);
    }
    if let Some(end) = e.stop_seconds {
        value["stopTimestamp"] = json!(end);
    }
    value
}
async fn publish(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    p: &Playlist,
    kind: &str,
) -> Result<(), ApiError> {
    let owner: i64 =
        sqlx::query_scalar("SELECT owner_account_id FROM video_channels WHERE account_id=$1")
            .bind(p.channel_id)
            .fetch_one(&mut *conn)
            .await
            .map_err(plamenu_db::DbError::from)?;
    let signer = account::find_by_id(&mut *conn, owner)
        .await?
        .ok_or(ApiError::NotFound)?;
    let object = if kind == "Delete" {
        json!(uri(state, p))
    } else {
        object(state, conn, p, None).await?
    };
    // PeerTube derives visibility from the activity's `to` when delivered,
    // rather than the object's `to`. Keep both representations aligned.
    let to = if p.visibility == "public" {
        vec![plamenu_ap::activity::PUBLIC]
    } else {
        Vec::new()
    };
    let activity = json!({"@context":crate::video::context(),"id":format!("{}/activities/{}/{}",uri(state,p),p.revision,kind.to_ascii_lowercase()),"type":kind,"actor":entities::account_uri(&state.config.domain,&signer),"to":to,"cc":[plamenu_ap::activity::PUBLIC],"object":object});
    let inboxes = plamenu_db::follow::follower_inboxes(&mut *conn, p.channel_id).await?;
    crate::actions::fan_out_conn(state, conn, &signer, &activity, &inboxes).await?;
    Ok(())
}

/// Current channel authority signs deletion, not the now-unprivileged creator.
pub async fn delete_for_account_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    account: i64,
) -> Result<(), ApiError> {
    // Match mutation lock order: channel before playlist, including creator
    // deletion after transfer. Never wait on a channel while holding its list.
    sqlx::query("SELECT c.account_id FROM video_channels c WHERE EXISTS(SELECT 1 FROM video_playlists p WHERE p.channel_id=c.account_id AND p.remote_uri IS NULL AND p.deleted_at IS NULL AND (p.channel_id=$1 OR p.creator_id=$1)) ORDER BY c.account_id FOR UPDATE")
        .bind(account).fetch_all(&mut *conn).await.map_err(plamenu_db::DbError::from)?;
    let rows:Vec<Playlist>=sqlx::query_as("SELECT * FROM video_playlists WHERE remote_uri IS NULL AND deleted_at IS NULL AND (channel_id=$1 OR creator_id=$1) ORDER BY channel_id,id FOR UPDATE")
        .bind(account).fetch_all(&mut *conn).await.map_err(plamenu_db::DbError::from)?;
    for p in rows {
        if p.visibility != "private" {
            publish(state, conn, &p, "Delete").await?;
        }
        sqlx::query("UPDATE video_playlists SET deleted_at=now(),revision=revision+1 WHERE id=$1")
            .bind(p.id)
            .execute(&mut *conn)
            .await
            .map_err(plamenu_db::DbError::from)?;
        sqlx::query("DELETE FROM video_playlist_entries WHERE playlist_id=$1")
            .bind(p.id)
            .execute(&mut *conn)
            .await
            .map_err(plamenu_db::DbError::from)?;
    }
    Ok(())
}
