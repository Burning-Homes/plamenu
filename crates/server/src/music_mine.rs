//! Personal music membership is a live union, independently of named collections.
use crate::{
    AppState,
    error::ApiError,
    music_assets, music_entities,
    music_saved::{self, Kind, Reference},
};
use plamenu_db::DbError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// Preserve the common metadata rule when deriving artists from collected albums.
const CANDIDATES:&str=concat!(
    "-- STUBKEEP: candidate identity union; current resource reads filter authority\nWITH remote_status_aliases AS (SELECT s.id,COALESCE(track.id,audio.id) AS remote_id FROM statuses s JOIN accounts a ON a.id=s.account_id JOIN music_remote_resources audio ON audio.uri=s.uri AND audio.controller_id=s.account_id AND audio.kind='audio' LEFT JOIN music_remote_resources track ON track.uri=audio.media_of AND track.kind='track' WHERE s.object_type='Audio' AND s.reblog_of_id IS NULL AND a.domain IS NOT NULL), readable_assets AS (SELECT id,owner_id,artist_id,album_artist_id FROM (",
    music_assets::asset_query!(
        " WHERE (t.owner_id=$1 OR EXISTS(SELECT 1 FROM music_saved_resources b WHERE b.account_id=$1 AND b.asset_id=t.id) OR EXISTS(SELECT 1 FROM music_favourites f WHERE f.account_id=$1 AND f.asset_id=t.id) OR EXISTS(SELECT 1 FROM music_publications p JOIN statuses s ON s.id=p.status_id JOIN accounts a ON a.id=s.account_id WHERE p.asset_id=t.id AND s.account_id=t.owner_id AND a.domain IS NULL AND s.object_type='Audio' AND s.deleted_at IS NULL AND s.reblog_of_id IS NULL AND (s.account_id=$1 OR s.visibility IN('public','unlisted')) AND (EXISTS(SELECT 1 FROM music_favourites f WHERE f.account_id=$1 AND f.status_id=s.id) OR EXISTS(SELECT 1 FROM music_saved_resources b WHERE b.account_id=$1 AND b.status_id=s.id)))) ",""),
    ") readable), aliases AS (SELECT p.status_id,t.id AS asset_id FROM music_publications p JOIN readable_assets t ON t.id=p.asset_id JOIN statuses s ON s.id=p.status_id JOIN accounts a ON a.id=s.account_id WHERE s.account_id=t.owner_id AND a.domain IS NULL AND s.object_type='Audio' AND s.deleted_at IS NULL AND s.reblog_of_id IS NULL AND (s.account_id=$1 OR s.visibility IN('public','unlisted'))), collected_albums AS (SELECT id,artist_id FROM (",
    music_entities::entity_batch!(
    music_entities::album_select!(
        "WHERE a.owner_id=$1 OR EXISTS(SELECT 1 FROM music_saved_resources b WHERE b.account_id=$1 AND b.album_id=a.id)"
    ),
    music_assets::asset_query!(
        " WHERE t.album_id IN(SELECT selected.id FROM music_albums selected WHERE selected.owner_id=$1 OR EXISTS(SELECT 1 FROM music_saved_resources b WHERE b.account_id=$1 AND b.album_id=selected.id)) ",
        ""
    ),
    "SELECT album_id"
),
    ") collected), raw AS (
      SELECT id,'track'::text AS kind,true AS owned,false AS saved,false AS favourite,false AS implicit FROM readable_assets WHERE owner_id=$1
      UNION ALL SELECT s.id,'artist',true,false,false,false FROM music_artists s WHERE s.owner_id=$1
      UNION ALL SELECT s.id,'album',true,false,false,false FROM music_albums s WHERE s.owner_id=$1
      UNION ALL SELECT s.id,s.kind,true,false,false,false FROM music_collections s WHERE s.owner_id=$1
      UNION ALL SELECT s.id,'radio',true,false,false,false FROM music_radios s WHERE s.owner_id=$1
      UNION ALL SELECT COALESCE(p.asset_id,s.id),CASE WHEN p.asset_id IS NULL THEN 'audio' ELSE 'track' END,true,false,false,false FROM statuses s LEFT JOIN aliases p ON p.status_id=s.id WHERE s.account_id=$1 AND s.object_type='Audio' AND s.reblog_of_id IS NULL AND s.deleted_at IS NULL
      UNION ALL SELECT COALESCE(p.asset_id,s.target_id),CASE WHEN p.asset_id IS NULL THEN s.kind ELSE 'track' END,false,true,false,false FROM music_saved_resources s LEFT JOIN aliases p ON s.kind='audio' AND p.status_id=s.target_id WHERE s.account_id=$1
      UNION ALL SELECT COALESCE(s.asset_id,p.asset_id,s.status_id),CASE WHEN COALESCE(s.asset_id,p.asset_id) IS NULL THEN 'audio' ELSE 'track' END,false,false,true,false FROM music_favourites s LEFT JOIN aliases p ON p.status_id=s.status_id WHERE s.account_id=$1 AND s.remote_id IS NULL
      UNION ALL SELECT resource_id,'remote',false,true,false,false FROM music_saved_remote WHERE account_id=$1
      UNION ALL SELECT remote_id,'remote',false,false,true,false FROM music_favourites WHERE account_id=$1 AND remote_id IS NOT NULL
      UNION ALL SELECT unnest(ARRAY[artist_id,album_artist_id]),'artist',false,false,false,true FROM readable_assets
      UNION ALL SELECT artist_id,'artist',false,false,false,true FROM collected_albums
    ), folded AS (
      SELECT COALESCE(legacy.remote_id,track.id,raw.id) AS id,
        CASE WHEN legacy.remote_id IS NOT NULL THEN 'remote' ELSE raw.kind END AS kind,
        raw.owned,raw.saved,raw.favourite,raw.implicit
      FROM raw
      LEFT JOIN remote_status_aliases legacy ON raw.kind='audio' AND legacy.id=raw.id
      LEFT JOIN music_remote_resources audio ON raw.kind='remote' AND audio.id=raw.id AND audio.kind='audio'
      LEFT JOIN music_remote_resources track ON track.uri=audio.media_of AND track.kind='track'
      WHERE legacy.id IS NULL OR legacy.id=ANY($6)
    ), candidates AS (SELECT id,kind,bool_or(owned) AS owned,bool_or(saved) AS saved,bool_or(favourite) AS favourite,bool_or(implicit) AS implicit FROM folded WHERE id IS NOT NULL GROUP BY id,kind)
    SELECT * FROM candidates WHERE ($3='all' OR ($3='tracks' AND kind IN('track','audio')) OR (kind='remote' AND EXISTS(SELECT 1 FROM music_remote_resources r WHERE r.id=candidates.id AND (($3='tracks' AND r.kind IN('track','audio')) OR ($3='artists' AND r.kind='artist') OR ($3='albums' AND r.kind='album') OR ($3='libraries' AND r.kind='library') OR ($3='playlists' AND r.kind='playlist')))) OR ($3='artists' AND kind='artist') OR ($3='albums' AND kind='album') OR ($3='libraries' AND kind='library') OR ($3='playlists' AND kind='playlist') OR ($3='radios' AND kind='radio')) AND ($4::bigint IS NULL OR id<$4 OR (id=$4 AND kind>$5)) ORDER BY id DESC,kind LIMIT 100
    -- STUBFILTER: owned Audio and verified publication aliases exclude deleted or boosted statuses
    "
);
#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Filter {
    #[default]
    All,
    Tracks,
    Artists,
    Albums,
    Libraries,
    Playlists,
    Radios,
}
impl Filter {
    #[must_use]
    pub fn text(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Tracks => "tracks",
            Self::Artists => "artists",
            Self::Albums => "albums",
            Self::Libraries => "libraries",
            Self::Playlists => "playlists",
            Self::Radios => "radios",
        }
    }
}
#[derive(Default, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub kind: Filter,
    pub before: Option<String>,
    #[serde(default)]
    pub q: String,
}
#[derive(sqlx::FromRow)]
struct Candidate {
    id: i64,
    kind: String,
    #[sqlx(flatten)]
    membership: Membership,
}
// Memberships overlap independently, including all four at the same time.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, sqlx::FromRow, Serialize)]
pub struct Membership {
    pub owned: bool,
    pub saved: bool,
    pub favourite: bool,
    pub implicit: bool,
}
#[derive(Serialize)]
pub struct Item {
    pub reference: Reference,
    pub membership: Membership,
    pub source: Value,
}
#[derive(Serialize)]
pub struct Page {
    pub items: Vec<Item>,
    pub before: Option<String>,
}
fn cursor(value: &str) -> Result<(i64, String), ApiError> {
    let invalid = || ApiError::Unprocessable("Invalid personal music cursor".into());
    if value.len() > 64 {
        return Err(invalid());
    }
    let (id, kind) = value.split_once(':').ok_or_else(invalid)?;
    let id = id
        .parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(invalid)?;
    Kind::parse(kind).map_err(|_| invalid())?;
    Ok((id, kind.to_owned()))
}
fn contains(value: &Value, search: &str) -> bool {
    match value {
        Value::String(text) => text.to_lowercase().contains(search),
        Value::Array(items) => items.iter().any(|item| contains(item, search)),
        Value::Object(fields) => fields.iter().any(|(key, item)| {
            matches!(
                key.as_str(),
                "name"
                    | "title"
                    | "artist"
                    | "album"
                    | "genre"
                    | "genres"
                    | "tags"
                    | "description"
                    | "extra_artists"
                    | "music"
            ) && contains(item, search)
        }),
        _ => false,
    }
}
/// Filter current authorized resources before a visible cursor or search result.
pub async fn page(state: &AppState, owner: i64, input: &Request) -> Result<Page, ApiError> {
    if input.q.chars().count() > 200 || input.q.contains('\0') {
        return Err(ApiError::Unprocessable(
            "Invalid personal music search".into(),
        ));
    }
    let mut conn = state.pool.acquire().await.map_err(DbError::from)?;
    crate::music_queues::active_owner(&mut conn, owner).await?;
    drop(conn);
    let legacy = readable_legacy_members(state, owner).await?;
    let mut next = input.before.as_deref().map(cursor).transpose()?;
    let search = input.q.trim().to_lowercase();
    let mut items = Vec::new();
    let mut visible_cursor = None;
    loop {
        let rows: Vec<Candidate> = sqlx::query_as(CANDIDATES)
            .bind(owner)
            .bind(Some(owner))
            .bind(input.kind.text())
            .bind(next.as_ref().map(|value| value.0))
            .bind(next.as_ref().map(|value| value.1.as_str()))
            .bind(&legacy)
            .fetch_all(&state.pool)
            .await
            .map_err(DbError::from)?;
        let references = rows
            .iter()
            .map(|row| {
                Ok(Reference {
                    kind: Kind::parse(&row.kind)?,
                    id: row.id.to_string(),
                })
            })
            .collect::<Result<Vec<_>, ApiError>>()?;
        let mut sources = music_saved::sources(state, &references, owner).await?;
        for row in &rows {
            let kind = Kind::parse(&row.kind)?;
            if let Some(source) = sources.remove(&(kind, row.id))
                && (search.is_empty() || contains(&source, &search))
            {
                if items.len() == 50 {
                    return Ok(Page {
                        items,
                        before: visible_cursor,
                    });
                }
                items.push(Item {
                    reference: Reference {
                        kind,
                        id: row.id.to_string(),
                    },
                    membership: row.membership,
                    source,
                });
                visible_cursor = Some(format!("{}:{}", row.id, row.kind));
            }
        }
        if rows.len() < 100 {
            return Ok(Page {
                items,
                before: None,
            });
        }
        next = rows.last().map(|row| (row.id, row.kind.clone()));
    }
}

/// Check only this account's historical memberships, in bounded source batches.
async fn readable_legacy_members(state: &AppState, owner: i64) -> Result<Vec<i64>, ApiError> {
    let mut visible = Vec::new();
    let mut after = None;
    loop {
        let ids: Vec<i64> = sqlx::query_scalar("-- STUBKEEP: candidate saved identities; current resource reads filter authority\nSELECT s.id FROM statuses s JOIN accounts a ON a.id=s.account_id JOIN music_remote_resources r ON r.uri=s.uri AND r.controller_id=s.account_id AND r.kind='audio' WHERE s.object_type='Audio' AND s.reblog_of_id IS NULL AND a.domain IS NOT NULL AND ($2::bigint IS NULL OR s.id>$2) AND (EXISTS(SELECT 1 FROM music_saved_resources b WHERE b.account_id=$1 AND b.status_id=s.id) OR EXISTS(SELECT 1 FROM music_favourites f WHERE f.account_id=$1 AND f.status_id=s.id)) ORDER BY s.id LIMIT 100")
            .bind(owner).bind(after).fetch_all(&state.pool).await.map_err(DbError::from)?;
        let entries = ids
            .iter()
            .map(|id| crate::music_collections::Entry {
                id: *id,
                status_id: Some(*id),
                asset_id: None,
                remote_id: None,
                episode_id: None,
                collection_id: 0,
                position: 1,
            })
            .collect::<Vec<_>>();
        visible.extend(
            crate::music_remote::readable_entries(&state.pool, &entries, Some(owner))
                .await?
                .into_iter()
                .map(|entry| entry.id),
        );
        if ids.len() < 100 {
            return Ok(visible);
        }
        after = ids.last().copied();
    }
}
