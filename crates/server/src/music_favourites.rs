//! Private music saves do not create social Likes or new publications.
use crate::{
    AppState,
    error::ApiError,
    music_collections::{self, Entry, Playable},
    music_queues::Source,
};
use plamenu_db::{
    DbError, id,
    notification_policy::{self, Disposition},
};
use serde::Serialize;
use time::OffsetDateTime;

#[derive(sqlx::FromRow)]
struct Stored {
    id: i64,
    asset_id: Option<i64>,
    status_id: Option<i64>,
    remote_id: Option<i64>,
    created_at: OffsetDateTime,
}
impl Stored {
    fn entry(&self) -> Entry {
        Entry {
            remote_id: self.remote_id,
            episode_id: None,
            id: self.id,
            collection_id: 0,
            asset_id: self.asset_id,
            status_id: self.status_id,
            position: 1,
        }
    }
}
#[derive(Serialize)]
pub struct Favourite {
    pub id: String,
    pub asset_id: Option<String>,
    pub status_id: Option<String>,
    pub remote_id: Option<String>,
    pub created_at: String,
    pub track: Playable,
}
#[derive(Serialize)]
pub struct Page {
    pub items: Vec<Favourite>,
    pub before: Option<String>,
}
fn entity(row: &Stored, track: Playable, entry: &Entry) -> Result<Favourite, ApiError> {
    Ok(Favourite {
        id: row.id.to_string(),
        asset_id: entry.asset_id.map(|v| v.to_string()),
        status_id: entry.status_id.map(|v| v.to_string()),
        remote_id: entry.remote_id.map(|v| v.to_string()),
        created_at: crate::entities::rfc3339(row.created_at)?,
        track,
    })
}
/// Idempotent under the same account fence used by retirement and queues.
pub async fn save(state: &AppState, owner: i64, source: &Source) -> Result<Favourite, ApiError> {
    let mut entry = source.entry(1, 0, 1)?;
    crate::music_remote::normalize_sources(state, std::slice::from_mut(&mut entry), owner).await?;
    if entry.episode_id.is_some() {
        return Err(ApiError::Unprocessable(
            "Podcast episodes use played state rather than track favourites".into(),
        ));
    }
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    crate::music_assets::lock_owner(&mut tx, owner).await?;
    crate::music_queues::active_owner(&mut tx, owner).await?;
    let mut track = music_collections::sources(state, std::slice::from_ref(&entry), Some(owner))
        .await?
        .into_iter()
        .next()
        .ok_or(ApiError::NotFound)?;
    let previous:Option<Stored>=sqlx::query_as("-- STUBKEEP: saved identity lookup; current playable reads enforce source authority\nSELECT * FROM music_favourites WHERE account_id=$1 AND ((asset_id IS NOT DISTINCT FROM $2 AND status_id IS NOT DISTINCT FROM $3 AND remote_id IS NOT DISTINCT FROM $4) OR ($4::bigint IS NOT NULL AND status_id IN(SELECT s.id FROM statuses s JOIN accounts a ON a.id=s.account_id JOIN music_remote_resources audio ON audio.uri=s.uri AND audio.controller_id=s.account_id AND audio.kind='audio' LEFT JOIN music_remote_resources track ON track.uri=audio.media_of AND track.kind='track' WHERE s.object_type='Audio' AND s.reblog_of_id IS NULL AND a.domain IS NOT NULL AND COALESCE(track.id,audio.id)=$4))) ORDER BY id DESC LIMIT 1").bind(owner).bind(entry.asset_id).bind(entry.status_id).bind(entry.remote_id).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
    let row = if let Some(row) = previous {
        row
    } else {
        let recipient: Option<i64> = if let Some(asset) = entry.asset_id {
            sqlx::query_scalar("SELECT owner_id FROM music_assets WHERE id=$1")
                .bind(asset)
                .fetch_optional(&mut *tx)
                .await
                .map_err(DbError::from)?
        } else if entry.remote_id.is_some() {
            None
        } else {
            sqlx::query_scalar("SELECT s.account_id FROM statuses s JOIN accounts a ON a.id=s.account_id WHERE s.id=$1 AND s.deleted_at IS NULL AND a.domain IS NULL AND a.deleted_at IS NULL AND a.suspended_at IS NULL\n-- STUBFILTER: saved music only notifies the owner of a live Audio publication").bind(entry.status_id).fetch_optional(&mut *tx).await.map_err(DbError::from)?
        };
        let notification = if let Some(recipient) = recipient.filter(|v| *v != owner) {
            // Use the ordinary favourite policy; the event has a separate
            // extension type and never manufactures a status for a retained track.
            let disposition = notification_policy::evaluate(
                &state.pool,
                recipient,
                owner,
                "music.favourite",
                entry.status_id,
            )
            .await?;
            let muted = if let Some(status) = entry.status_id {
                plamenu_db::conversation::status_muted(&state.pool, recipient, status).await?
            } else {
                false
            };
            if disposition == Disposition::Drop || muted {
                None
            } else {
                let notification = id::next();
                sqlx::query("INSERT INTO notifications(id,account_id,from_account_id,kind,filtered,reasons) VALUES($1,$2,$3,'music.favourite',$4,ARRAY['music.favourite'])").bind(notification).bind(recipient).bind(owner).bind(disposition==Disposition::Filter).execute(&mut *tx).await.map_err(DbError::from)?;
                Some(notification)
            }
        } else {
            None
        };
        sqlx::query_as("INSERT INTO music_favourites(id,account_id,asset_id,status_id,notification_id,remote_id) VALUES($1,$2,$3,$4,$5,$6) RETURNING *").bind(id::next()).bind(owner).bind(entry.asset_id).bind(entry.status_id).bind(notification).bind(entry.remote_id).fetch_one(&mut *tx).await.map_err(DbError::from)?
    };
    tx.commit().await.map_err(DbError::from)?;
    track.entry_id = row.id.to_string();
    entity(&row, track, &entry)
}
/// Removing a stale save remains possible after its source access was revoked.
pub async fn remove(state: &AppState, owner: i64, source: &Source) -> Result<(), ApiError> {
    let mut entry = source.entry(1, 0, 1)?;
    crate::music_remote::normalize_entries(&state.pool, std::slice::from_mut(&mut entry)).await?;
    let remote = if let Some(status) = entry.status_id {
        crate::music_remote::status_identity(&state.pool, status).await?
    } else {
        entry.remote_id
    };
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    crate::music_assets::lock_owner(&mut tx, owner).await?;
    crate::music_queues::active_owner(&mut tx, owner).await?;
    sqlx::query("DELETE FROM music_favourites WHERE account_id=$1 AND ((asset_id IS NOT DISTINCT FROM $2 AND status_id IS NOT DISTINCT FROM $3 AND remote_id IS NOT DISTINCT FROM $4) OR ($5::bigint IS NOT NULL AND (remote_id=$5 OR status_id IN(SELECT s.id FROM statuses s JOIN accounts a ON a.id=s.account_id JOIN music_remote_resources audio ON audio.uri=s.uri AND audio.controller_id=s.account_id AND audio.kind='audio' LEFT JOIN music_remote_resources track ON track.uri=audio.media_of AND track.kind='track' WHERE s.object_type='Audio' AND s.reblog_of_id IS NULL AND a.domain IS NOT NULL AND COALESCE(track.id,audio.id)=$5))))").bind(owner).bind(entry.asset_id).bind(entry.status_id).bind(entry.remote_id).bind(remote).execute(&mut *tx).await.map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
pub async fn contains(state: &AppState, owner: i64, source: &Source) -> Result<bool, ApiError> {
    let mut entry = source.entry(1, 0, 1)?;
    crate::music_remote::normalize_entries(&state.pool, std::slice::from_mut(&mut entry)).await?;
    let remote = if let Some(status) = entry.status_id {
        crate::music_remote::status_identity(&state.pool, status).await?
    } else {
        entry.remote_id
    };
    Ok(sqlx::query_scalar("-- STUBKEEP: saved identity membership, without granting source access\nSELECT EXISTS(SELECT 1 FROM music_favourites WHERE account_id=$1 AND ((asset_id IS NOT DISTINCT FROM $2 AND status_id IS NOT DISTINCT FROM $3 AND remote_id IS NOT DISTINCT FROM $4) OR ($5::bigint IS NOT NULL AND (remote_id=$5 OR status_id IN(SELECT s.id FROM statuses s JOIN accounts a ON a.id=s.account_id JOIN music_remote_resources audio ON audio.uri=s.uri AND audio.controller_id=s.account_id AND audio.kind='audio' LEFT JOIN music_remote_resources track ON track.uri=audio.media_of AND track.kind='track' WHERE s.object_type='Audio' AND s.reblog_of_id IS NULL AND a.domain IS NOT NULL AND COALESCE(track.id,audio.id)=$5)))))").bind(owner).bind(entry.asset_id).bind(entry.status_id).bind(entry.remote_id).bind(remote).fetch_one(&state.pool).await.map_err(DbError::from)?)
}
pub async fn page(state: &AppState, owner: i64, before: Option<i64>) -> Result<Page, ApiError> {
    let mut conn = state.pool.acquire().await.map_err(DbError::from)?;
    crate::music_queues::active_owner(&mut conn, owner).await?;
    drop(conn);
    let mut cursor = before;
    let mut items = Vec::new();
    loop {
        let rows:Vec<Stored>=sqlx::query_as(r"-- STUBKEEP: identity folding; current playable reads enforce source authority
WITH aliases AS (
 SELECT f.*,COALESCE(direct_track.id,direct.id,legacy_track.id,legacy.id) AS canonical
 FROM music_favourites f
 LEFT JOIN music_remote_resources direct ON direct.id=f.remote_id
 LEFT JOIN music_remote_resources direct_track ON direct_track.uri=direct.media_of AND direct_track.kind='track'
 LEFT JOIN statuses s ON s.id=f.status_id AND s.object_type='Audio' AND s.reblog_of_id IS NULL
 LEFT JOIN accounts a ON a.id=s.account_id AND a.domain IS NOT NULL
 LEFT JOIN music_remote_resources legacy ON legacy.uri=s.uri AND legacy.controller_id=a.id AND legacy.kind='audio'
 LEFT JOIN music_remote_resources legacy_track ON legacy_track.uri=legacy.media_of AND legacy_track.kind='track'
 WHERE f.account_id=$1
), representatives AS (
 SELECT DISTINCT ON(asset_id,CASE WHEN canonical IS NULL THEN status_id END,canonical)
 id,asset_id,status_id,remote_id,created_at
 FROM aliases
 ORDER BY asset_id,CASE WHEN canonical IS NULL THEN status_id END,canonical,(remote_id IS NOT NULL) DESC,id DESC
)
SELECT * FROM representatives WHERE ($2::bigint IS NULL OR id<$2) ORDER BY id DESC LIMIT 100").bind(owner).bind(cursor).fetch_all(&state.pool).await.map_err(DbError::from)?;
        cursor = rows.last().map(|r| r.id);
        let entries = rows.iter().map(Stored::entry).collect::<Vec<_>>();
        let entries =
            crate::music_remote::readable_entries(&state.pool, &entries, Some(owner)).await?;
        let normalized = entries
            .iter()
            .map(|entry| (entry.id, entry))
            .collect::<std::collections::HashMap<_, _>>();
        let sources = music_collections::sources(state, &entries, Some(owner)).await?;
        let rows_by_id = rows
            .iter()
            .map(|r| (r.id.to_string(), r))
            .collect::<std::collections::HashMap<_, _>>();
        for track in sources {
            let row = rows_by_id[&track.entry_id];
            let entry = normalized[&row.id];
            items.push(entity(row, track, entry)?);
            if items.len() > 50 {
                items.truncate(50);
                return Ok(Page {
                    before: items.last().map(|item| item.id.clone()),
                    items,
                });
            }
        }
        if rows.len() < 100 {
            return Ok(Page {
                items,
                before: None,
            });
        }
    }
}
/// Current authorized targets, fetched in batches with no stored title snapshots.
pub(crate) async fn notification_tracks(
    pool: &sqlx::PgPool,
    domain: &str,
    ids: &[i64],
    viewer: i64,
) -> Result<std::collections::HashMap<i64, Playable>, ApiError> {
    if ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let entries:Vec<Entry>=sqlx::query_as("SELECT f.notification_id AS id,0::bigint AS collection_id,f.asset_id,f.status_id,NULL::bigint AS episode_id,f.remote_id,1::integer AS position FROM music_favourites f JOIN notifications n ON n.id=f.notification_id WHERE f.notification_id=ANY($1) AND n.account_id=$2").bind(ids).bind(viewer).fetch_all(pool).await.map_err(DbError::from)?;
    Ok(
        music_collections::sources_pool(pool, domain, &entries, Some(viewer))
            .await?
            .into_iter()
            .filter_map(|p| Some((p.entry_id.parse().ok()?, p)))
            .collect(),
    )
}
