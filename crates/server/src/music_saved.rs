//! Private exact references reuse current source permissions on every read.
use crate::{
    AppState,
    error::ApiError,
    music_assets,
    music_collections::{self, Entry},
    music_entities::{self, Kind as EntityKind},
    music_radios,
};
use plamenu_db::{DbError, id};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use time::OffsetDateTime;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Track,
    Audio,
    Artist,
    Album,
    Library,
    Playlist,
    Radio,
    Remote,
}
impl Kind {
    pub(crate) fn text(self) -> &'static str {
        match self {
            Self::Track => "track",
            Self::Audio => "audio",
            Self::Artist => "artist",
            Self::Album => "album",
            Self::Library => "library",
            Self::Playlist => "playlist",
            Self::Radio => "radio",
            Self::Remote => "remote",
        }
    }
    pub(crate) fn parse(text: &str) -> Result<Self, ApiError> {
        match text {
            "track" => Ok(Self::Track),
            "audio" => Ok(Self::Audio),
            "artist" => Ok(Self::Artist),
            "album" => Ok(Self::Album),
            "library" => Ok(Self::Library),
            "playlist" => Ok(Self::Playlist),
            "radio" => Ok(Self::Radio),
            "remote" => Ok(Self::Remote),
            _ => Err(ApiError::NotFound),
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Reference {
    pub kind: Kind,
    pub id: String,
}
impl Reference {
    pub(crate) fn key(&self) -> Result<(Kind, i64), ApiError> {
        let id = self
            .id
            .parse::<i64>()
            .ok()
            .filter(|id| *id > 0)
            .ok_or_else(|| ApiError::Unprocessable("Invalid music reference".into()))?;
        Ok((self.kind, id))
    }
}
#[derive(sqlx::FromRow)]
struct Stored {
    id: i64,
    kind: String,
    target_id: i64,
    created_at: OffsetDateTime,
}
impl Stored {
    fn reference(&self) -> Result<Reference, ApiError> {
        Ok(Reference {
            kind: Kind::parse(&self.kind)?,
            id: self.target_id.to_string(),
        })
    }
}
#[derive(Serialize)]
pub struct Saved {
    pub id: String,
    pub reference: Reference,
    pub created_at: String,
    /// Missing when current permissions no longer allow reading the target.
    pub source: Option<Value>,
}
#[derive(Serialize)]
pub struct Page {
    pub items: Vec<Saved>,
    pub before: Option<String>,
}
fn saved(row: &Stored, source: Option<Value>) -> Result<Saved, ApiError> {
    Ok(Saved {
        id: row.id.to_string(),
        reference: row.reference()?,
        created_at: crate::entities::rfc3339(row.created_at)?,
        source,
    })
}
fn insert_entity(result: &mut HashMap<(Kind, i64), Value>, kind: Kind, id: i64, mut value: Value) {
    value["kind"] = json!(kind);
    value["id"] = json!(id.to_string());
    if kind == Kind::Track {
        value["url"] = json!(format!("/music/assets/{id}"));
    }
    result.insert((kind, id), value);
}
/// Bounded metadata resolution; saved references never create access or copies.
pub async fn sources(
    state: &AppState,
    references: &[Reference],
    viewer: i64,
) -> Result<HashMap<(Kind, i64), Value>, ApiError> {
    if references.len() > 100 {
        return Err(ApiError::Unprocessable(
            "Saved music batch is too large".into(),
        ));
    }
    let keys = references
        .iter()
        .map(Reference::key)
        .collect::<Result<Vec<_>, _>>()?;
    let ids = |kind| {
        keys.iter()
            .filter_map(|(k, id)| (*k == kind).then_some(*id))
            .collect::<Vec<_>>()
    };
    let mut result = HashMap::new();
    for row in music_assets::for_ids(state, &ids(Kind::Track), Some(viewer)).await? {
        insert_entity(&mut result, Kind::Track, row.id, row.entity());
    }
    let mut entries = ids(Kind::Audio)
        .into_iter()
        .map(|id| Entry {
            remote_id: None,
            episode_id: None,
            id,
            collection_id: 0,
            asset_id: None,
            status_id: Some(id),
            position: 1,
        })
        .collect::<Vec<_>>();
    let legacy_ids = ids(Kind::Audio);
    let mappings: Vec<(i64,i64)> = sqlx::query_as("-- STUBKEEP: identity mapping; subsequent deleted_ids and audience checks govern reads\nSELECT s.id,COALESCE(track.id,audio.id) FROM statuses s JOIN accounts a ON a.id=s.account_id JOIN music_remote_resources audio ON audio.uri=s.uri AND audio.controller_id=s.account_id AND audio.kind='audio' LEFT JOIN music_remote_resources track ON track.uri=audio.media_of AND track.kind='track' WHERE s.id=ANY($1) AND s.object_type='Audio' AND s.reblog_of_id IS NULL AND a.domain IS NOT NULL")
        .bind(&legacy_ids).fetch_all(&state.pool).await.map_err(DbError::from)?;
    let mapped = mappings
        .iter()
        .map(|(status, _)| *status)
        .collect::<std::collections::HashSet<_>>();
    entries.retain(|entry| !mapped.contains(&entry.id));
    legacy_remote_sources(state, &mappings, viewer, &mut result).await?;
    let music = plamenu_db::music::for_statuses(&state.pool, &ids(Kind::Audio)).await?;
    for row in music_collections::sources(state, &entries, Some(viewer)).await? {
        let id = row
            .entry_id
            .parse::<i64>()
            .map_err(|error| ApiError::Internal(Box::new(error)))?;
        let mut value =
            serde_json::to_value(row).map_err(|error| ApiError::Internal(Box::new(error)))?;
        value["music"] = serde_json::to_value(music.get(&id))
            .map_err(|error| ApiError::Internal(Box::new(error)))?;
        insert_entity(&mut result, Kind::Audio, id, value);
    }
    for (kind, entity_kind) in [
        (Kind::Artist, EntityKind::Artist),
        (Kind::Album, EntityKind::Album),
    ] {
        for row in music_entities::for_ids(state, entity_kind, &ids(kind), Some(viewer)).await? {
            insert_entity(&mut result, kind, row.id, row.entity(entity_kind));
        }
    }
    let mut collections = ids(Kind::Library);
    collections.extend(ids(Kind::Playlist));
    for row in music_collections::for_ids(state, &collections, Some(viewer)).await? {
        let kind = Kind::parse(&row.kind)?;
        if keys.contains(&(kind, row.id)) {
            insert_entity(&mut result, kind, row.id, row.entity());
        }
    }
    for row in music_radios::for_ids(state, &ids(Kind::Radio), Some(viewer)).await? {
        insert_entity(&mut result, Kind::Radio, row.id, row.entity(Some(viewer)));
    }
    for row in crate::music_remote::for_ids(state, &ids(Kind::Remote), viewer).await? {
        let key = row.id;
        insert_entity(&mut result, Kind::Remote, key, row.entity());
    }
    artwork(state, &mut result).await?;
    Ok(result)
}
async fn legacy_remote_sources(
    state: &AppState,
    mappings: &[(i64, i64)],
    viewer: i64,
    result: &mut HashMap<(Kind, i64), Value>,
) -> Result<(), ApiError> {
    let ids = mappings
        .iter()
        .map(|(status, _)| *status)
        .collect::<Vec<_>>();
    let rows = plamenu_db::status::find_by_ids(&state.pool, &ids).await?;
    let deleted = plamenu_db::status::deleted_ids(&state.pool, &ids).await?;
    let visible = crate::entities::filter_viewable(&state.pool, &rows, Some(viewer))
        .await?
        .into_iter()
        .filter(|row| !deleted.contains(&row.id))
        .map(|row| row.id)
        .collect::<std::collections::HashSet<_>>();
    let remote_ids = mappings
        .iter()
        .filter(|(status, _)| visible.contains(status))
        .map(|(_, remote)| *remote)
        .collect::<Vec<_>>();
    let resources = crate::music_remote::for_ids(state, &remote_ids, viewer)
        .await?
        .into_iter()
        .map(|row| (row.id, row))
        .collect::<HashMap<_, _>>();
    for (status, remote) in mappings {
        if visible.contains(status)
            && let Some(resource) = resources.get(remote)
        {
            result.insert((Kind::Audio, *status), resource.entity());
        }
    }
    Ok(())
}

#[derive(sqlx::FromRow)]
struct Preview {
    id: i64,
    target_kind: String,
    target_id: i64,
    width: i32,
    height: i32,
    content_type: String,
}
async fn artwork(
    state: &AppState,
    result: &mut HashMap<(Kind, i64), Value>,
) -> Result<(), ApiError> {
    let ids = |kind| {
        result
            .keys()
            .filter_map(|(key, id)| (*key == kind).then_some(*id))
            .collect::<Vec<_>>()
    };
    let artists = ids(Kind::Artist);
    let albums = ids(Kind::Album);
    let mut collections = ids(Kind::Library);
    collections.extend(ids(Kind::Playlist));
    if artists.is_empty() && albums.is_empty() && collections.is_empty() {
        return Ok(());
    }
    let rows:Vec<Preview>=sqlx::query_as("SELECT DISTINCT ON (target_kind,target_id) id,target_kind,target_id,width,height,content_type FROM music_artworks WHERE ((target_kind='artists' AND target_id=ANY($1)) OR (target_kind='albums' AND target_id=ANY($2)) OR (target_kind='collections' AND target_id=ANY($3))) AND NOT EXISTS(SELECT 1 FROM media_cleanup_jobs c WHERE c.file_name=music_artworks.file_name) ORDER BY target_kind,target_id,CASE WHEN (target_kind='artists' AND slot='image') OR (target_kind<>'artists' AND slot='cover') THEN 0 ELSE 1 END,id DESC")
        .bind(artists).bind(albums).bind(collections).fetch_all(&state.pool).await.map_err(DbError::from)?;
    for row in rows {
        let kinds = match row.target_kind.as_str() {
            "artists" => &[Kind::Artist][..],
            "albums" => &[Kind::Album][..],
            "collections" => &[Kind::Library, Kind::Playlist][..],
            _ => continue,
        };
        for kind in kinds {
            if let Some(value) = result.get_mut(&(*kind, row.target_id)) {
                value["image"] = json!({"id":row.id.to_string(),"url":format!("/music/artwork/{}",row.id),"width":row.width,"height":row.height,"content_type":row.content_type});
            }
        }
    }
    Ok(())
}
async fn readable(state: &AppState, reference: &Reference, viewer: i64) -> Result<Value, ApiError> {
    sources(state, std::slice::from_ref(reference), viewer)
        .await?
        .remove(&reference.key()?)
        .ok_or(ApiError::NotFound)
}
/// Canonical local publications fold only through their proven retained source.
async fn normalize(
    state: &AppState,
    reference: &Reference,
    viewer: i64,
) -> Result<Reference, ApiError> {
    readable(state, reference, viewer).await?;
    if reference.kind == Kind::Audio {
        let (_, status) = reference.key()?;
        let asset:Option<i64>=sqlx::query_scalar("SELECT t.id FROM music_publications p JOIN music_assets t ON t.id=p.asset_id JOIN statuses s ON s.id=p.status_id JOIN accounts a ON a.id=s.account_id WHERE p.status_id=$1 AND s.account_id=t.owner_id AND a.domain IS NULL AND s.object_type='Audio' AND s.reblog_of_id IS NULL AND s.deleted_at IS NULL\n-- STUBFILTER: aliases require a live origin-owned native Audio publication")
            .bind(status).fetch_optional(&state.pool).await.map_err(DbError::from)?;
        if let Some(asset) = asset {
            let target = Reference {
                kind: Kind::Track,
                id: asset.to_string(),
            };
            match readable(state, &target, viewer).await {
                Ok(_) => return Ok(target),
                Err(ApiError::NotFound) => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(reference.clone())
}
/// Save is independent from ownership, music favourites and `ActivityPub` follows.
pub async fn save(state: &AppState, owner: i64, reference: &Reference) -> Result<Saved, ApiError> {
    if reference.kind == Kind::Remote {
        return crate::music_remote::save(state, owner, reference).await;
    }
    if reference.kind == Kind::Audio
        && let Some(remote) =
            crate::music_remote::status_reference(state, reference.key()?.1, owner).await?
    {
        return crate::music_remote::save(
            state,
            owner,
            &Reference {
                kind: Kind::Remote,
                id: remote.to_string(),
            },
        )
        .await;
    }
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, owner).await?;
    crate::music_queues::active_owner(&mut tx, owner).await?;
    let reference = normalize(state, reference, owner).await?;
    let (kind, target) = reference.key()?;
    let source = readable(state, &reference, owner).await?;
    sqlx::query("INSERT INTO music_saved_resources(id,account_id,kind,asset_id,status_id,artist_id,album_id,collection_id,radio_id) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9) ON CONFLICT(account_id,kind,target_id) DO NOTHING")
        .bind(id::next()).bind(owner).bind(kind.text())
        .bind((kind==Kind::Track).then_some(target)).bind((kind==Kind::Audio).then_some(target))
        .bind((kind==Kind::Artist).then_some(target)).bind((kind==Kind::Album).then_some(target))
        .bind(matches!(kind,Kind::Library|Kind::Playlist).then_some(target)).bind((kind==Kind::Radio).then_some(target))
        .execute(&mut *tx).await.map_err(DbError::from)?;
    let row: Stored = sqlx::query_as(
        "SELECT * FROM music_saved_resources WHERE account_id=$1 AND kind=$2 AND target_id=$3",
    )
    .bind(owner)
    .bind(kind.text())
    .bind(target)
    .fetch_one(&mut *tx)
    .await
    .map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    saved(&row, Some(source))
}
/// The caller can remove their own reference even after source access is lost.
pub async fn remove(state: &AppState, owner: i64, saved_id: i64) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, owner).await?;
    crate::music_queues::active_owner(&mut tx, owner).await?;
    sqlx::query("DELETE FROM music_saved_resources WHERE id=$1 AND account_id=$2")
        .bind(saved_id)
        .bind(owner)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    sqlx::query("DELETE FROM music_saved_remote WHERE id=$1 AND account_id=$2")
        .bind(saved_id)
        .bind(owner)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
/// Native controls identify the current source while the manager uses row IDs.
pub async fn contains(
    state: &AppState,
    owner: i64,
    reference: &Reference,
) -> Result<bool, ApiError> {
    if reference.kind == Kind::Remote {
        return crate::music_remote::contains(state, owner, reference).await;
    }
    if reference.kind == Kind::Audio
        && let Some(remote) =
            crate::music_remote::status_reference(state, reference.key()?.1, owner).await?
    {
        return crate::music_remote::contains(
            state,
            owner,
            &Reference {
                kind: Kind::Remote,
                id: remote.to_string(),
            },
        )
        .await;
    }
    let reference = normalize(state, reference, owner).await?;
    let (kind, target) = reference.key()?;
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM music_saved_resources b WHERE b.account_id=$1 AND ((b.kind=$2 AND b.target_id=$3) OR ($2='track' AND b.kind='audio' AND EXISTS(SELECT 1 FROM music_publications p JOIN music_assets t ON t.id=p.asset_id JOIN statuses s ON s.id=p.status_id JOIN accounts a ON a.id=s.account_id WHERE p.asset_id=$3 AND p.status_id=b.target_id AND s.account_id=t.owner_id AND a.domain IS NULL AND s.object_type='Audio' AND s.reblog_of_id IS NULL AND s.deleted_at IS NULL))))\n-- STUBFILTER: saved controls fold only verified live native publication aliases")
        .bind(owner).bind(kind.text()).bind(target).fetch_one(&state.pool).await.map_err(DbError::from)?)
}
/// Removing an available source preserves ownership and favourite membership.
pub async fn remove_reference(
    state: &AppState,
    owner: i64,
    reference: &Reference,
) -> Result<(), ApiError> {
    if reference.kind == Kind::Remote {
        return crate::music_remote::remove_reference(state, owner, reference).await;
    }
    let (kind, target) = reference.key()?;
    let remote = if kind == Kind::Audio {
        crate::music_remote::status_identity(&state.pool, target).await?
    } else {
        None
    };
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, owner).await?;
    crate::music_queues::active_owner(&mut tx, owner).await?;
    sqlx::query("DELETE FROM music_saved_resources b WHERE b.account_id=$1 AND ((b.kind=$2 AND b.target_id=$3) OR ($2='track' AND b.kind='audio' AND EXISTS(SELECT 1 FROM music_publications p JOIN music_assets t ON t.id=p.asset_id JOIN statuses s ON s.id=p.status_id JOIN accounts a ON a.id=s.account_id WHERE p.asset_id=$3 AND p.status_id=b.target_id AND s.account_id=t.owner_id AND a.domain IS NULL AND s.object_type='Audio' AND s.reblog_of_id IS NULL AND s.deleted_at IS NULL)))\n-- STUBFILTER: alias removal only follows a live origin-owned native publication")
        .bind(owner).bind(kind.text()).bind(target).execute(&mut *tx).await.map_err(DbError::from)?;
    if let Some(remote) = remote {
        sqlx::query("DELETE FROM music_saved_remote WHERE account_id=$1 AND resource_id=$2")
            .bind(owner)
            .bind(remote)
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
    }
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
/// This owner-only manager includes unavailable references without their metadata.
pub async fn page(state: &AppState, owner: i64, before: Option<i64>) -> Result<Page, ApiError> {
    let mut conn = state.pool.acquire().await.map_err(DbError::from)?;
    crate::music_queues::active_owner(&mut conn, owner).await?;
    drop(conn);
    let mut rows:Vec<Stored>=sqlx::query_as("SELECT * FROM (SELECT id,kind,target_id,created_at FROM music_saved_resources WHERE account_id=$1 UNION ALL SELECT id,'remote'::text AS kind,resource_id AS target_id,created_at FROM music_saved_remote WHERE account_id=$1) saved WHERE ($2::bigint IS NULL OR id<$2) ORDER BY id DESC LIMIT 51")
        .bind(owner).bind(before).fetch_all(&state.pool).await.map_err(DbError::from)?;
    let before = if rows.len() > 50 {
        rows.truncate(50);
        rows.last().map(|row| row.id.to_string())
    } else {
        None
    };
    let references = rows
        .iter()
        .map(Stored::reference)
        .collect::<Result<Vec<_>, _>>()?;
    let mut resolved = sources(state, &references, owner).await?;
    let items = rows
        .iter()
        .map(|row| {
            saved(
                row,
                resolved.remove(&(Kind::parse(&row.kind)?, row.target_id)),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Page { items, before })
}
