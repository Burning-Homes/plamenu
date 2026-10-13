//! Reusable owner-scoped artists and albums retain source authorization.
use crate::{AppState, entities, error::ApiError, music_assets};
use plamenu_ap::activity::PUBLIC;
use plamenu_db::{
    DbError,
    account::{self, Account},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::FromRow;
use std::collections::HashMap;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Artist,
    Album,
}
impl Kind {
    #[must_use]
    pub fn path(self) -> &'static str {
        match self {
            Self::Artist => "artists",
            Self::Album => "albums",
        }
    }
    #[must_use]
    pub fn ap_type(self) -> &'static str {
        match self {
            Self::Artist => "Artist",
            Self::Album => "Album",
        }
    }
}
#[must_use]
pub fn uri(domain: &str, kind: Kind, id: i64) -> String {
    format!("https://{domain}/ap/music/{}/{id}", kind.path())
}
#[must_use]
pub fn local_id(domain: &str, target: &str) -> Option<(Kind, i64)> {
    for kind in [Kind::Artist, Kind::Album] {
        if let Some(id) = target
            .strip_prefix(&format!("https://{domain}/ap/music/{}/", kind.path()))
            .and_then(|s| s.parse::<i64>().ok())
            .filter(|id| *id > 0)
            && target == uri(domain, kind, id)
        {
            return Some((kind, id));
        }
    }
    None
}

#[derive(Debug, Clone, FromRow)]
pub struct Entity {
    pub id: i64,
    pub owner_id: i64,
    pub name: String,
    pub artist_id: Option<i64>,
    pub artist: String,
    pub release_year: Option<i32>,
    pub description: String,
    pub musicbrainz_id: Option<String>,
    pub tags: Vec<String>,
    pub genres: Vec<String>,
    pub revision: i64,
    pub visibility: String,
    pub share_tracks: bool,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}
impl Entity {
    #[must_use]
    pub fn entity(&self, kind: Kind) -> Value {
        let mut value = json!({"id":self.id.to_string(),"owner_id":self.owner_id.to_string(),"kind":kind,"name":self.name,"description":self.description,"artist_id":self.artist_id.map(|id|id.to_string()),"artist":self.artist,"release_year":self.release_year,"musicbrainz_id":self.musicbrainz_id,"tags":self.tags,"genres":self.genres,"revision":self.revision.to_string(),"created_at":self.created_at.format(&Rfc3339).ok(),"updated_at":self.updated_at.format(&Rfc3339).ok(),"url":format!("/music/{}/{}",kind.path(),self.id)});
        if kind == Kind::Album {
            value["visibility"] = json!(self.visibility);
            value["share_tracks"] = json!(self.share_tracks);
        }
        value
    }
}
macro_rules! artist_select {($tail:literal)=>{concat!("SELECT r.id,r.owner_id,r.name,NULL::bigint AS artist_id,r.name AS artist,NULL::integer AS release_year,r.description,r.musicbrainz_id,r.tags,'{}'::text[] AS genres,r.revision,'artist'::text AS entity_kind,'source'::text AS visibility,false AS share_tracks,r.created_at,r.updated_at FROM music_artists r ",$tail)};}
macro_rules! album_select {($tail:literal)=>{concat!("SELECT a.id,a.owner_id,a.name,a.artist_id,r.name AS artist,a.release_year,a.description,a.musicbrainz_id,a.tags,a.genres,a.revision,'album'::text AS entity_kind,a.visibility,a.share_tracks,a.created_at,a.updated_at FROM music_albums a JOIN music_artists r ON r.id=a.artist_id ",$tail)};}
pub(crate) use album_select;
pub(crate) use artist_select;
const ARTIST_GET: &str = artist_select!("WHERE r.id=$1");
const ALBUM_GET: &str = album_select!("WHERE a.id=$1");
macro_rules! entity_batch {
    ($selection:expr, $sources:expr, $references:literal) => {
        concat!("SELECT e.* FROM (",$selection,
            ") e JOIN accounts controller ON controller.id=e.owner_id WHERE controller.domain IS NULL AND NOT EXISTS(SELECT 1 FROM users u WHERE u.account_id=controller.id AND (NOT u.approved OR u.confirmed_at IS NULL)) AND (e.owner_id=$2 OR (controller.suspended_at IS NULL AND controller.deleted_at IS NULL AND NOT EXISTS(SELECT 1 FROM blocks b WHERE (b.account_id=$2 AND b.target_account_id=e.owner_id) OR (b.account_id=e.owner_id AND b.target_account_id=$2)) AND (e.visibility='public' OR (e.visibility='local' AND EXISTS(SELECT 1 FROM accounts v WHERE v.id=$2 AND v.domain IS NULL)) OR (e.entity_kind='album' AND EXISTS(SELECT 1 FROM music_album_grants g WHERE g.album_id=e.id AND g.account_id=$2)) OR (e.visibility='source' AND e.id IN (",
            $references," FROM (",$sources,
            ") readable)))))")
    };
}
pub(crate) use entity_batch;
macro_rules! artist_batch_query {
    ($selection:literal,$tracks:literal,$albums:literal,$album_tracks:literal) => {
        crate::music_entities::entity_batch!(
            crate::music_entities::artist_select!($selection),
            concat!(
                "SELECT artist_id,album_artist_id FROM (",
                crate::music_assets::asset_query!($tracks, ""),
                ") tracks UNION ALL SELECT artist_id,NULL::bigint AS album_artist_id FROM (",
                crate::music_entities::entity_batch!(
                    crate::music_entities::album_select!($albums),
                    crate::music_assets::asset_query!($album_tracks, ""),
                    "SELECT album_id"
                ),
                ") albums"
            ),
            "SELECT unnest(ARRAY[artist_id,album_artist_id])"
        )
    };
}
pub(crate) use artist_batch_query;
const ARTIST_BATCH: &str = artist_batch_query!(
    "WHERE r.id=ANY($1)",
    " WHERE (t.artist_id=ANY($1) OR a.artist_id=ANY($1)) ",
    "WHERE a.artist_id=ANY($1)",
    " WHERE a.artist_id=ANY($1) "
);

const ALBUM_BATCH: &str = entity_batch!(
    album_select!("WHERE a.id=ANY($1)"),
    music_assets::asset_query!(" WHERE t.album_id=ANY($1) ", ""),
    "SELECT album_id"
);
const ARTIST_COUNT: &str = concat!(
    "WITH readable AS (",
    music_assets::asset_query!(" WHERE (t.artist_id=$1 OR a.artist_id=$1) ", ""),
    "), readable_albums AS (",
    entity_batch!(
        album_select!("WHERE a.artist_id=$1"),
        music_assets::asset_query!(" WHERE a.artist_id=$1 ", ""),
        "SELECT album_id"
    ),
    ") SELECT count(*) AS track_count,(SELECT count(*) FROM readable_albums) AS album_count,COALESCE(sum(duration),0)::float8 AS total_duration FROM readable"
);
const ALBUM_COUNT: &str = concat!(
    "SELECT count(*) AS track_count,count(DISTINCT album_id) AS album_count,COALESCE(sum(duration),0)::float8 AS total_duration FROM (",
    music_assets::asset_query!(" WHERE t.album_id=$1 ", ""),
    ") readable"
);
#[derive(Debug, Clone, Copy, FromRow, Serialize)]
pub struct Statistics {
    pub track_count: i64,
    pub album_count: i64,
    pub total_duration: f64,
}
pub(crate) async fn statistics_conn(
    conn: &mut sqlx::PgConnection,
    kind: Kind,
    id: i64,
    viewer: Option<i64>,
) -> Result<Statistics, ApiError> {
    let query = match kind {
        Kind::Artist => ARTIST_COUNT,
        Kind::Album => ALBUM_COUNT,
    };
    Ok(sqlx::query_as(query)
        .bind(id)
        .bind(viewer)
        .fetch_one(conn)
        .await
        .map_err(DbError::from)?)
}
pub(crate) async fn find_conn(
    conn: &mut sqlx::PgConnection,
    kind: Kind,
    id: i64,
) -> Result<Option<Entity>, ApiError> {
    let query = match kind {
        Kind::Artist => ARTIST_GET,
        Kind::Album => ALBUM_GET,
    };
    Ok(sqlx::query_as(query)
        .bind(id)
        .fetch_optional(conn)
        .await
        .map_err(DbError::from)?)
}
pub async fn get(
    state: &AppState,
    kind: Kind,
    id: i64,
    viewer: Option<i64>,
) -> Result<Entity, ApiError> {
    for_ids(state, kind, &[id], viewer)
        .await?
        .pop()
        .ok_or(ApiError::NotFound)
}
/// Resolve a bounded set using exactly the same current source rule as `get()`.
pub async fn for_ids(
    state: &AppState,
    kind: Kind,
    ids: &[i64],
    viewer: Option<i64>,
) -> Result<Vec<Entity>, ApiError> {
    for_ids_pool(&state.pool, kind, ids, viewer).await
}
pub(crate) async fn for_ids_pool(
    pool: &sqlx::PgPool,
    kind: Kind,
    ids: &[i64],
    viewer: Option<i64>,
) -> Result<Vec<Entity>, ApiError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    if ids.len() > 100 {
        return Err(ApiError::Unprocessable(
            "Music entity batch is too large".into(),
        ));
    }
    let query = match kind {
        Kind::Artist => ARTIST_BATCH,
        Kind::Album => ALBUM_BATCH,
    };
    Ok(sqlx::query_as(query)
        .bind(ids)
        .bind(viewer)
        .fetch_all(pool)
        .await
        .map_err(DbError::from)?)
}
pub(crate) async fn public_conn(
    conn: &mut sqlx::PgConnection,
    kind: Kind,
    id: i64,
) -> Result<bool, ApiError> {
    let query = match kind {
        Kind::Artist => ARTIST_BATCH,
        Kind::Album => ALBUM_BATCH,
    };
    let row: Option<Entity> = sqlx::query_as(query)
        .bind([id].as_slice())
        .bind(None::<i64>)
        .fetch_optional(conn)
        .await
        .map_err(DbError::from)?;
    Ok(row.is_some())
}
pub async fn statistics(
    state: &AppState,
    kind: Kind,
    id: i64,
    viewer: Option<i64>,
) -> Result<Statistics, ApiError> {
    get(state, kind, id, viewer).await?;
    let mut conn = state.pool.acquire().await.map_err(DbError::from)?;
    statistics_conn(&mut conn, kind, id, viewer).await
}
pub(crate) async fn resource_owner(
    state: &AppState,
    target: &str,
) -> Result<Option<(Account, Kind, String)>, ApiError> {
    let Some((kind, id)) = local_id(&state.config.domain, target) else {
        return Ok(None);
    };
    let row = match get(state, kind, id, None).await {
        Ok(row) => row,
        Err(ApiError::NotFound) => return Ok(None),
        Err(error) => return Err(error),
    };
    let owner = account::find_by_id(&state.pool, row.owner_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !crate::instance_policy::public_account_visible(&state.pool, &state.config.domain, &owner)
        .await?
    {
        return Ok(None);
    }
    Ok(Some((owner, kind, row.name)))
}

fn timestamp(raw: &Value, key: &str) -> Value {
    json!(
        raw[key]
            .as_str()
            .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
            .and_then(|t| t.format(&Rfc3339).ok())
    )
}
fn hashtags(domain: &str, terms: &[String]) -> Value {
    json!(terms.iter().map(|term|json!({"type":"Hashtag","name":format!("#{term}"),"href":format!("https://{domain}/music/assets?{}",serde_urlencoded::to_string([("q",term)]).unwrap_or_default())})).collect::<Vec<_>>())
}
fn artist_document(domain: &str, raw: &Value, owner: &str) -> Value {
    json!({"type":"Artist","id":uri(domain,Kind::Artist,raw["id"].as_i64().unwrap_or_default()),"name":raw["name"],"summary":raw["description"],"attributedTo":owner,"published":timestamp(raw,"created_at"),"updated":timestamp(raw,"updated_at"),"musicbrainzId":raw["musicbrainz_id"],"tag":hashtags(domain,&serde_json::from_value::<Vec<String>>(raw["tags"].clone()).unwrap_or_default()),"image":crate::music_artworks::image(domain,&raw["artwork"])})
}
fn album_document(domain: &str, raw: &Value, artist: &Value, owner: &str) -> Value {
    json!({"type":"Album","id":uri(domain,Kind::Album,raw["id"].as_i64().unwrap_or_default()),"name":raw["name"],"summary":raw["description"],"attributedTo":owner,"artists":[artist],"published":timestamp(raw,"created_at"),"updated":timestamp(raw,"updated_at"),"musicbrainzId":raw["musicbrainz_id"],"released":raw["release_year"].as_i64().map(|y|format!("{y:04}-01-01")),"tag":hashtags(domain,&serde_json::from_value::<Vec<String>>(raw["tags"].clone()).unwrap_or_default()),"image":crate::music_artworks::image(domain,&raw["artwork"])})
}
#[derive(FromRow)]
struct SnapshotRow {
    status_id: i64,
    artist: Value,
    album: Option<Value>,
    album_artist: Option<Value>,
    artwork: Option<Value>,
    description: String,
    tags: Vec<String>,
}
#[derive(Debug, Clone)]
pub(crate) struct Snapshot {
    artist: Value,
    album: Option<Value>,
    album_artist: Option<Value>,
    artwork: Option<Value>,
    description: String,
    tags: Vec<String>,
}
pub(crate) async fn snapshots_conn(
    conn: &mut sqlx::PgConnection,
    ids: &[i64],
) -> Result<HashMap<i64, Snapshot>, ApiError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows:Vec<SnapshotRow>=sqlx::query_as("SELECT p.status_id,to_jsonb(r)||jsonb_build_object('artwork',(SELECT to_jsonb(i) FROM music_artworks i WHERE i.target_kind='artists' AND i.target_id=r.id AND NOT EXISTS(SELECT 1 FROM media_cleanup_jobs c WHERE c.file_name=i.file_name) ORDER BY CASE WHEN i.slot='image' THEN 0 ELSE 1 END LIMIT 1)) AS artist,to_jsonb(a)||jsonb_build_object('artwork',(SELECT to_jsonb(i) FROM music_artworks i WHERE i.target_kind='albums' AND i.target_id=a.id AND NOT EXISTS(SELECT 1 FROM media_cleanup_jobs c WHERE c.file_name=i.file_name) ORDER BY CASE WHEN i.slot='cover' THEN 0 ELSE 1 END LIMIT 1)) AS album,to_jsonb(ar)||jsonb_build_object('artwork',(SELECT to_jsonb(i) FROM music_artworks i WHERE i.target_kind='artists' AND i.target_id=ar.id AND NOT EXISTS(SELECT 1 FROM media_cleanup_jobs c WHERE c.file_name=i.file_name) ORDER BY CASE WHEN i.slot='image' THEN 0 ELSE 1 END LIMIT 1)) AS album_artist,(SELECT to_jsonb(i) FROM music_artworks i WHERE i.target_kind='assets' AND i.target_id=t.id AND NOT EXISTS(SELECT 1 FROM media_cleanup_jobs c WHERE c.file_name=i.file_name) ORDER BY CASE WHEN i.slot='image' THEN 0 ELSE 1 END LIMIT 1) AS artwork,t.description,t.tags FROM music_publications p JOIN music_assets t ON t.id=p.asset_id JOIN music_artists r ON r.id=t.artist_id LEFT JOIN music_albums a ON a.id=t.album_id LEFT JOIN music_artists ar ON ar.id=a.artist_id WHERE p.status_id=ANY($1) AND t.visibility='public'")
        .bind(ids).fetch_all(conn).await.map_err(DbError::from)?;
    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.status_id,
                Snapshot {
                    artist: r.artist,
                    album: r.album,
                    album_artist: r.album_artist,
                    artwork: r.artwork,
                    description: r.description,
                    tags: r.tags,
                },
            )
        })
        .collect())
}
impl Snapshot {
    pub(crate) fn enrich(&self, domain: &str, object: &mut Value) {
        let Some(owner) = plamenu_ap::activity::attributed_to_ids(object)
            .next()
            .map(str::to_owned)
        else {
            return;
        };
        let artist = artist_document(domain, &self.artist, &owner);
        object["track"]["artists"] = json!([artist]);
        if let Some(album) = &self.album {
            let album_artist = self.album_artist.as_ref().map_or_else(
                || object["track"]["artists"][0].clone(),
                |raw| artist_document(domain, raw, &owner),
            );
            object["track"]["album"] = if matches!(
                album["visibility"].as_str(),
                Some("private" | "local")
            ) {
                json!({"type":"Album","name":album["name"],"released":album["release_year"].as_i64().map(|year|format!("{year:04}-01-01")),"artists":[album_artist]})
            } else {
                album_document(domain, album, &album_artist, &owner)
            };
        } else {
            object["track"]["album"]["artists"] = object["track"]["artists"].clone();
        }
        let cover = self
            .artwork
            .as_ref()
            .or_else(|| {
                self.album
                    .as_ref()
                    .and_then(|album| album.get("artwork"))
                    .filter(|value| !value.is_null())
            })
            .or_else(|| self.artist.get("artwork").filter(|value| !value.is_null()));
        let image = cover.map_or(Value::Null, |raw| crate::music_artworks::image(domain, raw));
        if !self.description.is_empty() {
            object["track"]["summary"] = object["content"].clone();
        }
        object["track"]["tag"] = hashtags(domain, &self.tags);
        object["track"]["image"] = image.clone();
        object["image"] = image;
    }
}
pub async fn document(state: &AppState, kind: Kind, id: i64) -> Result<Value, ApiError> {
    let target = uri(&state.config.domain, kind, id);
    let (owner, _, _) = resource_owner(state, &target)
        .await?
        .ok_or(ApiError::NotFound)?;
    let mut conn = state.pool.acquire().await.map_err(DbError::from)?;
    document_conn(state, &mut conn, &owner, kind, id).await
}

async fn document_conn(
    state: &AppState,
    conn: &mut sqlx::PgConnection,
    owner: &Account,
    kind: Kind,
    id: i64,
) -> Result<Value, ApiError> {
    let target = uri(&state.config.domain, kind, id);
    let row = find_conn(conn, kind, id).await?.ok_or(ApiError::NotFound)?;
    let actor = entities::account_uri(&state.config.domain, owner);
    let mut raw = json!({"id":row.id,"name":row.name,"description":row.description,"created_at":row.created_at.format(&Rfc3339).ok(),"updated_at":row.updated_at.format(&Rfc3339).ok(),"musicbrainz_id":row.musicbrainz_id,"release_year":row.release_year,"tags":row.tags});
    raw["artwork"] = crate::music_artworks::raw_image_conn(
        conn,
        if kind == Kind::Artist {
            crate::music_artworks::Target::Artists
        } else {
            crate::music_artworks::Target::Albums
        },
        id,
    )
    .await?;
    let mut document = match kind {
        Kind::Artist => artist_document(&state.config.domain, &raw, &actor),
        Kind::Album => {
            let artist = find_conn(conn, Kind::Artist, row.artist_id.ok_or(ApiError::NotFound)?)
                .await?
                .ok_or(ApiError::NotFound)?;
            let artist_artwork = crate::music_artworks::raw_image_conn(
                conn,
                crate::music_artworks::Target::Artists,
                artist.id,
            )
            .await?;
            album_document(
                &state.config.domain,
                &raw,
                &artist_document(
                    &state.config.domain,
                    &json!({"id":artist.id,"name":artist.name,"description":artist.description,"created_at":artist.created_at.format(&Rfc3339).ok(),"updated_at":artist.updated_at.format(&Rfc3339).ok(),"musicbrainz_id":artist.musicbrainz_id,"tags":artist.tags,"artwork":artist_artwork}),
                    &actor,
                ),
                &actor,
            )
        }
    };
    document["@context"] = json!([
        "https://www.w3.org/ns/activitystreams",
        "https://funkwhale.audio/ns"
    ]);
    document["followers"] = json!(format!("{target}/followers"));
    document["to"] = json!([PUBLIC]);
    Ok(document)
}

/// Hold one current public source until the Follow commits. Source mutations
/// already lock assets first, so no artist-to-asset lock inversion is added.
pub(crate) async fn followable_conn(
    conn: &mut sqlx::PgConnection,
    domain: &str,
    target: &str,
) -> Result<bool, ApiError> {
    let Some((kind, id)) = local_id(domain, target) else {
        return Ok(true);
    };
    let query = match kind {
        Kind::Artist => music_assets::asset_query!(
            " WHERE (t.artist_id=$1 OR a.artist_id=$1) AND t.visibility='public' ",
            " LIMIT 1 FOR SHARE OF t"
        ),
        Kind::Album => music_assets::asset_query!(
            " WHERE t.album_id=$1 AND t.visibility='public' ",
            " LIMIT 1 FOR SHARE OF t"
        ),
    };
    let _source: Option<i64> = sqlx::query_scalar(query)
        .bind(id)
        .bind(None::<i64>)
        .fetch_optional(&mut *conn)
        .await
        .map_err(DbError::from)?;
    // Source rows are locked first, matching metadata and privacy mutations.
    let parent = match kind {
        Kind::Artist => "SELECT id FROM music_artists WHERE id=$1 FOR SHARE",
        Kind::Album => "SELECT id FROM music_albums WHERE id=$1 FOR SHARE",
    };
    sqlx::query(parent)
        .bind(id)
        .execute(&mut *conn)
        .await
        .map_err(DbError::from)?;
    public_conn(conn, kind, id).await
}

/// Notify the resource itself after source changes. A retained public source
/// keeps its artist/album alive even when its Audio publication is withdrawn.
pub(crate) async fn reconcile_conn(
    state: &AppState,
    conn: &mut sqlx::PgConnection,
    owner: i64,
    references: &[(Kind, i64)],
) -> Result<(), ApiError> {
    let actor = account::find_by_id(&mut *conn, owner)
        .await?
        .ok_or(ApiError::NotFound)?;
    let mut seen = std::collections::HashSet::new();
    for &(kind, id) in references {
        if !seen.insert((kind, id)) {
            continue;
        }
        let target = uri(&state.config.domain, kind, id);
        let current = public_conn(conn, kind, id).await?;
        let inboxes = crate::music_follows::target_inboxes(conn, &target, owner).await?;
        let object = if current {
            document_conn(state, conn, &actor, kind, id).await?
        } else {
            json!({"id":target,"type":"Tombstone"})
        };
        let activity = json!({"@context":["https://www.w3.org/ns/activitystreams","https://funkwhale.audio/ns"],"id":format!("{target}/activities/{}",plamenu_db::id::next()),"type":if current{"Update"}else{"Delete"},"actor":entities::account_uri(&state.config.domain,&actor),"to":[PUBLIC],"object":object});
        plamenu_db::job::enqueue_many(&mut *conn, owner, &inboxes, &activity, false).await?;
        if current {
            let row = find_conn(conn, kind, id).await?.ok_or(ApiError::NotFound)?;
            sqlx::query("UPDATE music_follows SET target_name=$2 WHERE target_uri=$1 AND owner_id=$3 AND local_target").bind(&target).bind(row.name).bind(owner).execute(&mut *conn).await.map_err(DbError::from)?;
        } else {
            sqlx::query(
                "DELETE FROM music_follows WHERE target_uri=$1 AND owner_id=$2 AND local_target",
            )
            .bind(&target)
            .bind(owner)
            .execute(&mut *conn)
            .await
            .map_err(DbError::from)?;
        }
    }
    Ok(())
}
#[must_use]
pub(crate) fn references(asset: &music_assets::Asset) -> Vec<(Kind, i64)> {
    let mut references = vec![(Kind::Artist, asset.artist_id)];
    if let Some(artist) = asset.album_artist_id {
        references.push((Kind::Artist, artist));
    }
    if let Some(album) = asset.album_id {
        references.push((Kind::Album, album));
    }
    references
}

#[derive(Debug)]
pub struct TrackPage {
    pub items: Vec<music_assets::Asset>,
    pub after: Option<i64>,
}
/// Source ACLs run before the limit, and the cursor belongs to a readable
/// source. Album/disc/track ordering remains stable without exposing hidden IDs.
pub async fn tracks(
    state: &AppState,
    kind: Kind,
    id: i64,
    viewer: Option<i64>,
    after: Option<i64>,
) -> Result<TrackPage, ApiError> {
    get(state, kind, id, viewer).await?;
    let cursor = if let Some(after) = after {
        let source = music_assets::get(state, after, viewer).await?;
        if (kind == Kind::Artist && source.artist_id != id && source.album_artist_id != Some(id))
            || (kind == Kind::Album && source.album_id != Some(id))
        {
            return Err(ApiError::NotFound);
        }
        Some(source)
    } else {
        None
    };
    let query = match kind {
        Kind::Artist => music_assets::asset_query!(
            " WHERE (t.artist_id=$1 OR a.artist_id=$1) ",
            " AND ($3::bigint IS NULL OR (COALESCE(t.album_id,0),COALESCE(t.disc_number,0),COALESCE(t.track_number,0),t.id)>($3,$4,$5,$6)) ORDER BY COALESCE(t.album_id,0),COALESCE(t.disc_number,0),COALESCE(t.track_number,0),t.id LIMIT 61"
        ),
        Kind::Album => music_assets::asset_query!(
            " WHERE t.album_id=$1 ",
            " AND ($3::bigint IS NULL OR (COALESCE(t.album_id,0),COALESCE(t.disc_number,0),COALESCE(t.track_number,0),t.id)>($3,$4,$5,$6)) ORDER BY COALESCE(t.album_id,0),COALESCE(t.disc_number,0),COALESCE(t.track_number,0),t.id LIMIT 61"
        ),
    };
    let mut items: Vec<music_assets::Asset> = sqlx::query_as(query)
        .bind(id)
        .bind(viewer)
        .bind(cursor.as_ref().map(|a| a.album_id.unwrap_or_default()))
        .bind(
            cursor
                .as_ref()
                .and_then(|a| a.disc_number)
                .unwrap_or_default(),
        )
        .bind(
            cursor
                .as_ref()
                .and_then(|a| a.track_number)
                .unwrap_or_default(),
        )
        .bind(cursor.as_ref().map_or(0, |a| a.id))
        .fetch_all(&state.pool)
        .await
        .map_err(DbError::from)?;
    let after = if items.len() > 60 {
        items.truncate(60);
        items.last().map(|a| a.id)
    } else {
        None
    };
    Ok(TrackPage { items, after })
}

const ARTIST_CATALOGUE: &str = concat!(
    "SELECT e.* FROM (",
    artist_select!(""),
    ") e JOIN accounts author ON author.id=e.owner_id WHERE author.domain IS NULL AND author.suspended_at IS NULL AND author.deleted_at IS NULL AND NOT EXISTS(SELECT 1 FROM users u WHERE u.account_id=author.id AND (NOT u.approved OR u.confirmed_at IS NULL)) AND (($3 AND e.owner_id=$2) OR (NOT $3 AND author.silenced_at IS NULL AND (e.id IN(SELECT artist_id FROM music_albums WHERE visibility='public') OR e.id IN(SELECT unnest(ARRAY[artist_id,album_artist_id]) FROM (",
    music_assets::asset_query!(" WHERE t.visibility='public' ", ""),
    ") readable)))) AND ($1::bigint IS NULL OR e.id<$1) AND (strpos(lower(e.name),lower($4))>0 OR strpos(lower(e.description),lower($4))>0 OR EXISTS(SELECT 1 FROM unnest(e.tags) term WHERE strpos(lower(term),lower($4))>0)) AND NOT EXISTS(SELECT 1 FROM blocks b WHERE (b.account_id=$2 AND b.target_account_id=e.owner_id) OR (b.account_id=e.owner_id AND b.target_account_id=$2)) AND NOT EXISTS(SELECT 1 FROM mutes m WHERE m.account_id=$2 AND m.target_account_id=e.owner_id AND (m.expires_at IS NULL OR m.expires_at>now())) ORDER BY e.id DESC LIMIT 61"
);
const ALBUM_CATALOGUE: &str = concat!(
    "SELECT e.* FROM (",
    album_select!(""),
    ") e JOIN accounts author ON author.id=e.owner_id WHERE author.domain IS NULL AND author.suspended_at IS NULL AND author.deleted_at IS NULL AND NOT EXISTS(SELECT 1 FROM users u WHERE u.account_id=author.id AND (NOT u.approved OR u.confirmed_at IS NULL)) AND (($3 AND e.owner_id=$2) OR (NOT $3 AND author.silenced_at IS NULL AND (e.visibility='public' OR (e.visibility='source' AND e.id IN(SELECT album_id FROM (",
    music_assets::asset_query!(" WHERE t.visibility='public' ", ""),
    ") readable))))) AND ($1::bigint IS NULL OR e.id<$1) AND (strpos(lower(e.name),lower($4))>0 OR strpos(lower(e.description),lower($4))>0 OR strpos(lower(e.artist),lower($4))>0 OR EXISTS(SELECT 1 FROM unnest(e.tags || e.genres) term WHERE strpos(lower(term),lower($4))>0)) AND NOT EXISTS(SELECT 1 FROM blocks b WHERE (b.account_id=$2 AND b.target_account_id=e.owner_id) OR (b.account_id=e.owner_id AND b.target_account_id=$2)) AND NOT EXISTS(SELECT 1 FROM mutes m WHERE m.account_id=$2 AND m.target_account_id=e.owner_id AND (m.expires_at IS NULL OR m.expires_at>now())) ORDER BY e.id DESC LIMIT 61"
);
pub async fn catalogue(
    state: &AppState,
    kind: Kind,
    viewer: Option<i64>,
    mine: bool,
    q: &str,
    before: Option<i64>,
) -> Result<Vec<Entity>, ApiError> {
    if q.chars().count() > 200 || q.contains('\0') {
        return Err(ApiError::Unprocessable("Invalid music search".into()));
    }
    if mine && viewer.is_none() {
        return Err(ApiError::NotFound);
    }
    let query = match kind {
        Kind::Artist => ARTIST_CATALOGUE,
        Kind::Album => ALBUM_CATALOGUE,
    };
    Ok(sqlx::query_as(query)
        .bind(before)
        .bind(viewer)
        .bind(mine)
        .bind(q)
        .fetch_all(&state.pool)
        .await
        .map_err(DbError::from)?)
}

#[derive(Debug, Deserialize)]
pub struct Edit {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub musicbrainz_id: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub genres: Vec<String>,
    pub release_year: Option<i32>,
    pub revision: String,
}
pub(crate) fn terms(values: &[String]) -> Result<Vec<String>, ApiError> {
    if values.len() > 50 {
        return Err(ApiError::Unprocessable("Use at most 50 music terms".into()));
    }
    let mut result = Vec::new();
    for term in values {
        let term = term.trim().to_lowercase();
        if term.is_empty() || term.chars().count() > 100 || term.chars().any(char::is_control) {
            return Err(ApiError::Unprocessable("Invalid music term".into()));
        }
        if !result.contains(&term) {
            result.push(term);
        }
    }
    Ok(result)
}
fn validate_edit(input: &Edit) -> Result<(i64, Vec<String>, Vec<String>), ApiError> {
    let revision = input
        .revision
        .parse::<i64>()
        .ok()
        .filter(|v| *v > 0)
        .ok_or_else(|| ApiError::Unprocessable("Entity revision is required".into()))?;
    if input.name.trim().is_empty()
        || input.name.chars().count() > 200
        || input.name.contains('\0')
        || input.description.chars().count() > 4000
        || input.description.contains('\0')
        || input.release_year.is_some_and(|y| !(1..=9999).contains(&y))
        || input
            .musicbrainz_id
            .as_ref()
            .is_some_and(|s| !crate::audio::valid_mbid(s))
    {
        return Err(ApiError::Unprocessable(
            "Invalid artist or album metadata".into(),
        ));
    }
    let tags = terms(&input.tags)?;
    let genres = terms(&input.genres)?;
    if genres.join("; ").chars().count() > 200 {
        return Err(ApiError::Unprocessable(
            "Combined genres must fit within 200 characters".into(),
        ));
    }
    Ok((revision, tags, genres))
}
pub(crate) async fn lock_sources(
    conn: &mut sqlx::PgConnection,
    owner: i64,
    kind: Kind,
    id: i64,
) -> Result<Vec<i64>, ApiError> {
    let query = match kind {
        Kind::Artist => {
            "SELECT t.id FROM music_assets t LEFT JOIN music_albums a ON a.id=t.album_id WHERE t.owner_id=$1 AND (t.artist_id=$2 OR a.artist_id=$2) ORDER BY t.id FOR UPDATE OF t"
        }
        Kind::Album => {
            "SELECT id FROM music_assets WHERE owner_id=$1 AND album_id=$2 ORDER BY id FOR UPDATE"
        }
    };
    Ok(sqlx::query_scalar(query)
        .bind(owner)
        .bind(id)
        .fetch_all(conn)
        .await
        .map_err(DbError::from)?)
}
pub(crate) async fn lock_entity(
    conn: &mut sqlx::PgConnection,
    owner: i64,
    kind: Kind,
    id: i64,
    revision: i64,
) -> Result<Entity, ApiError> {
    let query = match kind {
        Kind::Artist => "SELECT id FROM music_artists WHERE id=$1 AND owner_id=$2 FOR UPDATE",
        Kind::Album => "SELECT id FROM music_albums WHERE id=$1 AND owner_id=$2 FOR UPDATE",
    };
    sqlx::query_scalar::<_, i64>(query)
        .bind(id)
        .bind(owner)
        .fetch_optional(&mut *conn)
        .await
        .map_err(DbError::from)?
        .ok_or(ApiError::NotFound)?;
    let current = find_conn(conn, kind, id).await?.ok_or(ApiError::NotFound)?;
    if current.revision != revision {
        return Err(ApiError::Conflict(
            "Artist or album changed; reload before editing".into(),
        ));
    }
    Ok(current)
}
fn write_error(error: sqlx::Error) -> ApiError {
    if error
        .as_database_error()
        .is_some_and(sqlx::error::DatabaseError::is_unique_violation)
    {
        ApiError::Conflict("An artist or album with those details already exists".into())
    } else {
        DbError::from(error).into()
    }
}
pub async fn update(
    state: &AppState,
    owner: i64,
    kind: Kind,
    id: i64,
    input: &Edit,
) -> Result<Entity, ApiError> {
    let (revision, tags, genres) = validate_edit(input)?;
    get(state, kind, id, Some(owner)).await?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, owner).await?;
    let sources = lock_sources(&mut tx, owner, kind, id).await?;
    let previous = lock_entity(&mut tx, owner, kind, id, revision).await?;
    let mbid = input.musicbrainz_id.as_deref().map(str::to_ascii_lowercase);
    let mut references = vec![(kind, id)];
    match kind {
        Kind::Artist => {
            if input.release_year.is_some() || !genres.is_empty() {
                return Err(ApiError::Unprocessable(
                    "Years and genres belong to albums or tracks".into(),
                ));
            }
            sqlx::query("UPDATE music_artists SET name=$3,description=$4,musicbrainz_id=$5,tags=$6,revision=revision+1,updated_at=now() WHERE id=$1 AND owner_id=$2").bind(id).bind(owner).bind(input.name.trim()).bind(&input.description).bind(&mbid).bind(&tags).execute(&mut *tx).await.map_err(write_error)?;
            sqlx::query(
                "UPDATE music_assets SET revision=revision+1 WHERE owner_id=$1 AND id=ANY($2)",
            )
            .bind(owner)
            .bind(&sources)
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
            let albums: Vec<i64> = sqlx::query_scalar("UPDATE music_albums SET revision=revision+1,updated_at=now() WHERE owner_id=$1 AND artist_id=$2 RETURNING id")
                .bind(owner).bind(id).fetch_all(&mut *tx).await.map_err(DbError::from)?;
            references.extend(albums.into_iter().map(|album| (Kind::Album, album)));
        }
        Kind::Album => {
            sqlx::query("UPDATE music_albums SET name=$3,description=$4,musicbrainz_id=$5,tags=$6,genres=$7,release_year=$8,revision=revision+1,updated_at=now() WHERE id=$1 AND owner_id=$2").bind(id).bind(owner).bind(input.name.trim()).bind(&input.description).bind(&mbid).bind(&tags).bind(&genres).bind(input.release_year).execute(&mut *tx).await.map_err(write_error)?;
            let genre = (previous.genres != genres).then(|| genres.join("; "));
            sqlx::query("UPDATE music_assets SET release_year=CASE WHEN release_year IS NOT DISTINCT FROM $3 OR release_year IS NULL THEN $4 ELSE release_year END,genre=COALESCE($5,genre),revision=revision+1 WHERE owner_id=$1 AND album_id=$2").bind(owner).bind(id).bind(previous.release_year).bind(input.release_year).bind(genre).execute(&mut *tx).await.map_err(DbError::from)?;
        }
    }
    let mut edited = Vec::new();
    for source in sources {
        if let Some(status) =
            crate::music_publications::synchronize_conn(state, &mut tx, owner, source).await?
        {
            edited.push(status);
        }
    }
    reconcile_conn(state, &mut tx, owner, &references).await?;
    tx.commit().await.map_err(DbError::from)?;
    for status in &edited {
        crate::music_publications::publish_update(state, Some(status)).await;
    }
    get(state, kind, id, Some(owner)).await
}

#[derive(Debug, Deserialize)]
pub struct New {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub musicbrainz_id: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub genres: Vec<String>,
    pub release_year: Option<i32>,
    pub artist_id: Option<String>,
}
pub async fn create(
    state: &AppState,
    owner: i64,
    kind: Kind,
    input: &New,
) -> Result<Entity, ApiError> {
    let (_, tags, genres) = validate_edit(&Edit {
        name: input.name.clone(),
        description: input.description.clone(),
        musicbrainz_id: input.musicbrainz_id.clone(),
        tags: input.tags.clone(),
        genres: input.genres.clone(),
        release_year: input.release_year,
        revision: "1".into(),
    })?;
    if kind == Kind::Artist
        && (input.release_year.is_some() || !genres.is_empty() || input.artist_id.is_some())
    {
        return Err(ApiError::Unprocessable(
            "Album fields cannot describe an artist".into(),
        ));
    }
    let artist = if kind == Kind::Album {
        Some(
            input
                .artist_id
                .as_deref()
                .and_then(|s| s.parse::<i64>().ok())
                .filter(|id| *id > 0)
                .ok_or_else(|| {
                    ApiError::Unprocessable("Choose an owned artist for the album".into())
                })?,
        )
    } else {
        None
    };
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, owner).await?;
    let active:Option<i64>=sqlx::query_scalar("SELECT a.id FROM accounts a WHERE a.id=$1 AND a.domain IS NULL AND a.deleted_at IS NULL AND a.suspended_at IS NULL AND NOT EXISTS(SELECT 1 FROM users u WHERE u.account_id=a.id AND (NOT u.approved OR u.confirmed_at IS NULL))").bind(owner).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
    active.ok_or(ApiError::NotFound)?;
    if let Some(artist) = artist {
        let selected_artist: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM music_artists WHERE id=$1 AND owner_id=$2 FOR SHARE",
        )
        .bind(artist)
        .bind(owner)
        .fetch_optional(&mut *tx)
        .await
        .map_err(DbError::from)?;
        selected_artist.ok_or(ApiError::NotFound)?;
    }
    let id = plamenu_db::id::next();
    let mbid = input.musicbrainz_id.as_deref().map(str::to_ascii_lowercase);
    match kind {
        Kind::Artist => {
            sqlx::query("INSERT INTO music_artists(id,owner_id,name,description,musicbrainz_id,tags) VALUES($1,$2,$3,$4,$5,$6)").bind(id).bind(owner).bind(input.name.trim()).bind(&input.description).bind(mbid).bind(tags).execute(&mut *tx).await.map_err(write_error)?;
        }
        Kind::Album => {
            sqlx::query("INSERT INTO music_albums(id,owner_id,artist_id,name,release_year,description,musicbrainz_id,tags,genres,visibility) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,'private')").bind(id).bind(owner).bind(artist).bind(input.name.trim()).bind(input.release_year).bind(&input.description).bind(mbid).bind(tags).bind(genres).execute(&mut *tx).await.map_err(write_error)?;
        }
    }
    tx.commit().await.map_err(DbError::from)?;
    get(state, kind, id, Some(owner)).await
}

#[derive(Debug, Deserialize)]
pub struct Delete {
    pub revision: String,
    #[serde(default)]
    pub recursive: bool,
}
/// Albums may detach their tracks. Artists with references require explicit
/// recursive deletion. Every source, withdrawal and cleanup debt commits with
/// the entity tombstone; failures cannot leave a partly removed catalogue.
pub async fn delete(
    state: &AppState,
    owner: i64,
    kind: Kind,
    id: i64,
    input: &Delete,
) -> Result<(), ApiError> {
    let revision = input
        .revision
        .parse::<i64>()
        .ok()
        .filter(|r| *r > 0)
        .ok_or_else(|| ApiError::Unprocessable("Entity revision is required".into()))?;
    get(state, kind, id, Some(owner)).await?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, owner).await?;
    let sources = lock_sources(&mut tx, owner, kind, id).await?;
    let previous = lock_entity(&mut tx, owner, kind, id, revision).await?;
    let albums: Vec<i64> = if kind == Kind::Artist {
        sqlx::query_scalar(
            "SELECT id FROM music_albums WHERE artist_id=$1 AND owner_id=$2 ORDER BY id FOR UPDATE",
        )
        .bind(id)
        .bind(owner)
        .fetch_all(&mut *tx)
        .await
        .map_err(DbError::from)?
    } else {
        Vec::new()
    };
    if kind == Kind::Artist && !input.recursive && (!sources.is_empty() || !albums.is_empty()) {
        return Err(ApiError::Conflict(
            "Artist has associated albums or tracks; choose recursive deletion".into(),
        ));
    }
    let mut references = vec![(kind, id)];
    references.extend(albums.iter().map(|id| (Kind::Album, *id)));
    if let Some(artist) = previous.artist_id {
        references.push((Kind::Artist, artist));
    }
    let mut deleted = Vec::new();
    let mut edited = Vec::new();
    if input.recursive {
        for source in sources {
            let (events, refs) = music_assets::retire_conn(state, &mut tx, owner, source).await?;
            deleted.extend(events);
            references.extend(refs);
        }
    } else if kind == Kind::Album {
        sqlx::query("UPDATE music_assets SET album_id=NULL,revision=revision+1 WHERE owner_id=$1 AND album_id=$2").bind(owner).bind(id).execute(&mut *tx).await.map_err(DbError::from)?;
        for source in sources {
            if let Some(status) =
                crate::music_publications::synchronize_conn(state, &mut tx, owner, source).await?
            {
                edited.push(status);
            }
        }
    }
    if kind == Kind::Artist {
        sqlx::query("DELETE FROM music_albums WHERE owner_id=$1 AND artist_id=$2")
            .bind(owner)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
        sqlx::query("DELETE FROM music_artists WHERE owner_id=$1 AND id=$2")
            .bind(owner)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
    } else {
        sqlx::query("DELETE FROM music_albums WHERE owner_id=$1 AND id=$2")
            .bind(owner)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
    }
    reconcile_conn(state, &mut tx, owner, &references).await?;
    tx.commit().await.map_err(DbError::from)?;
    crate::music_publications::publish_deletes(state, &deleted).await;
    for status in &edited {
        crate::music_publications::publish_update(state, Some(status)).await;
    }
    Ok(())
}
