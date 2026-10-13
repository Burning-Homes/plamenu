//! Ordered references with separately opted-in, revocable access to owned tracks.
use crate::{AppState, error::ApiError, music_assets};
use plamenu_db::{DbError, account, id, status};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::FromRow;
use std::fmt::Write as _;
use time::OffsetDateTime;

#[derive(Debug, Clone, FromRow)]
pub struct Collection {
    pub id: i64,
    pub owner_id: i64,
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
    pub kind: String,
    pub visibility: String,
    pub share_tracks: bool,
    pub revision: i64,
    pub updated_at: OffsetDateTime,
}
impl Collection {
    #[must_use]
    pub fn entity(&self) -> Value {
        json!({"id":self.id.to_string(),"owner_id":self.owner_id.to_string(),"name":self.name,"description":self.description,"tags":self.tags,"kind":self.kind,"visibility":self.visibility,"share_tracks":self.share_tracks,"revision":self.revision.to_string(),"url":format!("/music/collections/{}",self.id)})
    }
}
#[derive(Debug, Clone, FromRow, Deserialize, Serialize, sqlx::Type)]
#[sqlx(type_name = "music_download_entry")]
pub struct Entry {
    pub remote_id: Option<i64>,
    pub id: i64,
    pub collection_id: i64,
    pub asset_id: Option<i64>,
    pub status_id: Option<i64>,
    pub episode_id: Option<i64>,
    pub position: i32,
}
#[derive(Debug, Clone, Serialize)]
pub struct Playable {
    pub entry_id: String,
    pub title: String,
    pub artist: String,
    pub url: String,
    pub stream_url: String,
    pub duration: f64,
    pub image_url: Option<String>,
}
#[derive(Debug, Deserialize)]
pub struct New {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub kind: String,
    pub visibility: String,
}
fn validate(input: &New) -> Result<(), ApiError> {
    if input.name.trim().is_empty()
        || input.name.chars().count() > 200
        || input.name.contains('\0')
        || input.description.chars().count() > 4000
        || input.description.contains('\0')
        || !["library", "playlist"].contains(&input.kind.as_str())
        || !["public", "local", "private"].contains(&input.visibility.as_str())
    {
        return Err(ApiError::Unprocessable("Invalid music collection".into()));
    }
    Ok(())
}
pub async fn create(state: &AppState, owner: i64, input: &New) -> Result<Collection, ApiError> {
    validate(input)?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, owner).await?;
    crate::music_queues::active_owner(&mut tx, owner).await?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM music_collections WHERE owner_id=$1")
        .bind(owner)
        .fetch_one(&mut *tx)
        .await
        .map_err(DbError::from)?;
    if count >= 100 {
        return Err(ApiError::Unprocessable(
            "Music collection limit reached".into(),
        ));
    }
    let item=sqlx::query_as("INSERT INTO music_collections(id,owner_id,name,description,kind,visibility) VALUES($1,$2,$3,$4,$5,$6) RETURNING *").bind(id::next()).bind(owner).bind(input.name.trim()).bind(&input.description).bind(&input.kind).bind(&input.visibility).fetch_one(&mut *tx).await.map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(item)
}
pub async fn get(
    state: &AppState,
    collection: i64,
    viewer: Option<i64>,
) -> Result<Collection, ApiError> {
    for_ids(state, &[collection], viewer)
        .await?
        .pop()
        .ok_or(ApiError::NotFound)
}
/// Direct and batched reads share one current container permission decision.
pub async fn for_ids(
    state: &AppState,
    ids: &[i64],
    viewer: Option<i64>,
) -> Result<Vec<Collection>, ApiError> {
    for_ids_pool(&state.pool, ids, viewer).await
}
pub(crate) async fn for_ids_pool(
    pool: &sqlx::PgPool,
    ids: &[i64],
    viewer: Option<i64>,
) -> Result<Vec<Collection>, ApiError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    if ids.len() > 100 {
        return Err(ApiError::Unprocessable(
            "Music collection batch is too large".into(),
        ));
    }
    Ok(sqlx::query_as("SELECT c.* FROM music_collections c JOIN accounts a ON a.id=c.owner_id WHERE c.id=ANY($1) AND a.suspended_at IS NULL AND a.deleted_at IS NULL AND (c.owner_id=$2 OR c.visibility='public' OR (c.visibility='local' AND EXISTS(SELECT 1 FROM accounts v WHERE v.id=$2 AND v.domain IS NULL)) OR EXISTS(SELECT 1 FROM music_collection_grants g WHERE g.collection_id=c.id AND g.account_id=$2)) AND NOT EXISTS(SELECT 1 FROM users u WHERE u.account_id=a.id AND (NOT u.approved OR u.confirmed_at IS NULL)) AND NOT EXISTS(SELECT 1 FROM blocks b WHERE (b.account_id=$2 AND b.target_account_id=c.owner_id) OR (b.account_id=c.owner_id AND b.target_account_id=$2))")
        .bind(ids).bind(viewer).fetch_all(pool).await.map_err(DbError::from)?)
}
pub async fn list(
    state: &AppState,
    viewer: Option<i64>,
    mine: bool,
    before: Option<i64>,
) -> Result<Vec<Collection>, ApiError> {
    search(state, viewer, mine, before, "", None).await
}
pub async fn search(
    state: &AppState,
    viewer: Option<i64>,
    mine: bool,
    before: Option<i64>,
    query: &str,
    kind: Option<&str>,
) -> Result<Vec<Collection>, ApiError> {
    let query = query.trim();
    if query.chars().count() > 200
        || query.contains('\0')
        || before.is_some_and(|v| v <= 0)
        || kind.is_some_and(|v| !matches!(v, "library" | "playlist"))
    {
        return Err(ApiError::Unprocessable(
            "Invalid collection search, kind or cursor".into(),
        ));
    }
    if mine && viewer.is_none() {
        return Err(ApiError::NotFound);
    }
    Ok(sqlx::query_as("SELECT c.* FROM music_collections c JOIN accounts a ON a.id=c.owner_id WHERE (($2 AND c.owner_id=$1) OR (NOT $2 AND c.visibility='public' AND a.silenced_at IS NULL)) AND a.suspended_at IS NULL AND a.deleted_at IS NULL AND NOT EXISTS(SELECT 1 FROM users u WHERE u.account_id=a.id AND (NOT u.approved OR u.confirmed_at IS NULL)) AND NOT EXISTS(SELECT 1 FROM blocks b WHERE (b.account_id=$1 AND b.target_account_id=c.owner_id) OR (b.account_id=c.owner_id AND b.target_account_id=$1)) AND NOT EXISTS(SELECT 1 FROM mutes m WHERE m.account_id=$1 AND m.target_account_id=c.owner_id AND (m.expires_at IS NULL OR m.expires_at>now())) AND ($3::bigint IS NULL OR c.id<$3) AND ($4='' OR strpos(lower(c.name || ' ' || c.description || ' ' || array_to_string(c.tags,' ')),lower($4))>0) AND ($5::text IS NULL OR c.kind=$5) ORDER BY c.id DESC LIMIT 61").bind(viewer).bind(mine).bind(before).bind(query).bind(kind).fetch_all(&state.pool).await.map_err(DbError::from)?)
}
async fn lock(
    conn: &mut sqlx::PgConnection,
    owner: i64,
    collection: i64,
    revision: i64,
) -> Result<Collection, ApiError> {
    music_assets::lock_owner(conn, owner).await?;
    crate::music_queues::active_owner(conn, owner).await?;
    let item: Collection =
        sqlx::query_as("SELECT * FROM music_collections WHERE id=$1 AND owner_id=$2 FOR UPDATE")
            .bind(collection)
            .bind(owner)
            .fetch_optional(conn)
            .await
            .map_err(DbError::from)?
            .ok_or(ApiError::NotFound)?;
    if item.revision != revision {
        return Err(ApiError::Conflict(
            "Music collection changed; reload before editing".into(),
        ));
    }
    Ok(item)
}
async fn bump(conn: &mut sqlx::PgConnection, collection: i64) -> Result<(), DbError> {
    sqlx::query("UPDATE music_collections SET revision=revision+1,updated_at=now() WHERE id=$1")
        .bind(collection)
        .execute(conn)
        .await?;
    Ok(())
}
/// A current parent permission and membership confer access only to its owner's
/// retained tracks. No independent grants or Audio publications are created.
pub async fn set_contents_sharing(
    state: &AppState,
    owner: i64,
    collection: i64,
    revision: i64,
    enabled: bool,
) -> Result<Collection, ApiError> {
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    lock(&mut tx, owner, collection, revision).await?;
    let updated: Collection = sqlx::query_as("UPDATE music_collections SET share_tracks=$2,revision=revision+1,updated_at=now() WHERE id=$1 RETURNING *")
        .bind(collection).bind(enabled).fetch_one(&mut *tx).await.map_err(DbError::from)?;
    crate::music_library_federation::changed_conn(state, &mut tx, &updated, false).await?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(updated)
}
pub async fn update(
    state: &AppState,
    owner: i64,
    collection: i64,
    revision: i64,
    input: &New,
) -> Result<Collection, ApiError> {
    validate(input)?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    let current = lock(&mut tx, owner, collection, revision).await?;
    if input.kind != current.kind {
        return Err(ApiError::Unprocessable(
            "A collection's kind cannot change".into(),
        ));
    }
    sqlx::query("UPDATE music_collections SET name=$2,description=$3,visibility=$4,revision=revision+1,updated_at=now() WHERE id=$1").bind(collection).bind(input.name.trim()).bind(&input.description).bind(&input.visibility).execute(&mut *tx).await.map_err(DbError::from)?;
    let updated: Collection = sqlx::query_as("SELECT * FROM music_collections WHERE id=$1")
        .bind(collection)
        .fetch_one(&mut *tx)
        .await
        .map_err(DbError::from)?;
    crate::music_library_federation::changed_conn(
        state,
        &mut tx,
        if input.visibility == "public" {
            &updated
        } else {
            &current
        },
        input.visibility != "public",
    )
    .await?;
    tx.commit().await.map_err(DbError::from)?;
    get(state, collection, Some(owner)).await
}
pub async fn delete(
    state: &AppState,
    owner: i64,
    collection: i64,
    revision: i64,
) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    let current = lock(&mut tx, owner, collection, revision).await?;
    crate::music_library_federation::changed_conn(state, &mut tx, &current, true).await?;
    sqlx::query("DELETE FROM music_collections WHERE id=$1")
        .bind(collection)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
pub async fn entries(state: &AppState, collection: i64) -> Result<Vec<Entry>, ApiError> {
    Ok(sqlx::query_as("SELECT * FROM music_collection_entries WHERE collection_id=$1 ORDER BY position,id LIMIT 10000").bind(collection).fetch_all(&state.pool).await.map_err(DbError::from)?)
}
#[derive(Debug, Serialize)]
pub struct Statistics {
    pub track_count: i64,
    pub total_duration: f64,
}
/// Count every currently readable entry, independently of the displayed page.
/// Playlist repeats retain their separate occurrences; hidden sources count zero.
pub async fn statistics(
    state: &AppState,
    collection: i64,
    viewer: Option<i64>,
) -> Result<Statistics, ApiError> {
    get(state, collection, viewer).await?;
    let mut result = Statistics {
        track_count: 0,
        total_duration: 0.0,
    };
    for page in entries(state, collection).await?.chunks(100) {
        for source in sources(state, page, viewer).await? {
            result.track_count += 1;
            if source.duration.is_finite() && source.duration > 0.0 {
                result.total_duration += source.duration;
                if !result.total_duration.is_finite() {
                    return Err(ApiError::Unprocessable(
                        "Music duration exceeds the supported range".into(),
                    ));
                }
            }
        }
    }
    get(state, collection, viewer).await?;
    Ok(result)
}
/// Authorized playables only. Hidden sources expose no title, URL or artwork.
/// Bound source batches rather than issuing one query per playlist entry.
pub async fn playables(
    state: &AppState,
    collection: i64,
    viewer: Option<i64>,
) -> Result<Vec<Playable>, ApiError> {
    get(state, collection, viewer).await?;
    sources(state, &entries(state, collection).await?, viewer).await
}
pub(crate) async fn sources(
    state: &AppState,
    entries: &[Entry],
    viewer: Option<i64>,
) -> Result<Vec<Playable>, ApiError> {
    sources_pool(&state.pool, &state.config.domain, entries, viewer).await
}
/// Shared current-source rendering for notifications as well as music pages.
pub(crate) async fn sources_pool(
    pool: &sqlx::PgPool,
    domain: &str,
    entries: &[Entry],
    viewer: Option<i64>,
) -> Result<Vec<Playable>, ApiError> {
    let mut items = Vec::new();
    let entries = crate::music_remote::readable_entries(pool, entries, viewer).await?;
    for page in entries.chunks(100) {
        let asset_ids = page.iter().filter_map(|e| e.asset_id).collect::<Vec<_>>();
        let assets = music_assets::for_ids_pool(pool, &asset_ids, viewer)
            .await?
            .into_iter()
            .map(|a| (a.id, a))
            .collect::<std::collections::HashMap<_, _>>();
        let status_ids = page.iter().filter_map(|e| e.status_id).collect::<Vec<_>>();
        let source = status::find_by_ids(pool, &status_ids).await?;
        let deleted = status::deleted_ids(pool, &status_ids).await?;
        let source = source
            .into_iter()
            .filter(|s| {
                s.object_type.as_deref() == Some("Audio")
                    && s.reblog_of_id.is_none()
                    && !deleted.contains(&s.id)
            })
            .collect::<Vec<_>>();
        let author_ids = source.iter().map(|s| s.account_id).collect::<Vec<_>>();
        let blocked:Vec<i64>=sqlx::query_scalar("SELECT target_account_id FROM blocks WHERE account_id=$1 AND target_account_id=ANY($2)").bind(viewer).bind(&author_ids).fetch_all(pool).await.map_err(DbError::from)?;
        let source = source
            .into_iter()
            .filter(|s| !blocked.contains(&s.account_id))
            .collect::<Vec<_>>();
        let visible = crate::entities::filter_viewable(pool, &source, viewer).await?;
        let rendered = crate::entities::render_statuses(pool, domain, &visible, viewer).await?;
        let statuses = rendered
            .into_iter()
            .filter_map(|v| {
                let id = v["id"].as_str()?.parse::<i64>().ok()?;
                let source = v["media_attachments"]
                    .as_array()?
                    .iter()
                    .find(|m| m["type"] == "audio")?;
                let playable = Playable {
                    entry_id: String::new(),
                    title: v["title"].as_str().unwrap_or("Audio").to_owned(),
                    artist: v["music"]["artist"]
                        .as_str()
                        .or_else(|| v["account"]["display_name"].as_str())
                        .unwrap_or_default()
                        .to_owned(),
                    url: v["url"].as_str()?.to_owned(),
                    stream_url: source["url"].as_str()?.to_owned(),
                    image_url: None,
                    duration: source["meta"]["original"]["duration"]
                        .as_f64()
                        .unwrap_or(0.0),
                };
                Some((id, playable))
            })
            .collect::<std::collections::HashMap<_, _>>();
        let episode_ids = page.iter().filter_map(|e| e.episode_id).collect::<Vec<_>>();
        let episodes = crate::music_podcasts::playables_pool(pool, &episode_ids).await?;
        let remote_ids = page
            .iter()
            .filter_map(|entry| entry.remote_id)
            .collect::<Vec<_>>();
        let remotes =
            crate::music_remote::playables_pool(pool, domain, &remote_ids, viewer).await?;
        let public_remotes: Vec<(i64, i64, i64)> = if remote_ids.is_empty() {
            Vec::new()
        } else {
            sqlx::query_as("SELECT e.id,e.collection_id,e.remote_id FROM music_collection_entries e JOIN music_collections c ON c.id=e.collection_id WHERE e.id=ANY($1) AND c.visibility='public' AND e.remote_id IS NOT NULL")
            .bind(page.iter().map(|entry|entry.id).collect::<Vec<_>>()).fetch_all(pool).await.map_err(DbError::from)?
        };
        for entry in page {
            if let Some(asset) = entry.asset_id.and_then(|id| assets.get(&id)) {
                items.push(Playable {
                    entry_id: entry.id.to_string(),
                    title: asset.title.clone(),
                    artist: asset.artist.clone(),
                    url: format!("/music/assets/{}", asset.id),
                    stream_url: format!("/music/assets/{}/stream", asset.id),
                    duration: asset.duration,
                    image_url: asset.artwork_id.map(|id| format!("/music/artwork/{id}")),
                });
            } else if let Some(item) = entry.status_id.and_then(|id| statuses.get(&id)) {
                let mut item = item.clone();
                item.entry_id = entry.id.to_string();
                items.push(item);
            } else if let Some(item) = entry.remote_id.and_then(|id| remotes.get(&id)) {
                let mut item = item.clone();
                item.entry_id = entry.id.to_string();
                public_remote_urls(&mut item, entry, &public_remotes);
                items.push(item);
            } else if let Some(item) = entry.episode_id.and_then(|id| episodes.get(&id)) {
                let mut item = item.clone();
                item.entry_id = entry.id.to_string();
                items.push(item);
            }
        }
    }
    Ok(items)
}

fn public_remote_urls(item: &mut Playable, entry: &Entry, public: &[(i64, i64, i64)]) {
    let Some(remote) = entry.remote_id else {
        return;
    };
    if !public.contains(&(entry.id, entry.collection_id, remote)) {
        return;
    }
    let base = format!(
        "/music/remote/{remote}/public/{}/{}",
        entry.collection_id, entry.id
    );
    item.stream_url = format!("{base}/stream");
    if item.image_url.is_some() {
        item.image_url = Some(format!("{base}/artwork"));
    }
}

#[derive(Debug)]
pub struct EntryPage {
    pub items: Vec<Entry>,
    pub after: Option<i64>,
}
/// A stable entry cursor bounds HTML/API work; ordering remains authoritative.
pub async fn entry_page(
    state: &AppState,
    collection: i64,
    after: Option<i64>,
) -> Result<EntryPage, ApiError> {
    let position = if let Some(after) = after {
        sqlx::query_scalar::<_, i32>(
            "SELECT position FROM music_collection_entries WHERE collection_id=$1 AND id=$2",
        )
        .bind(collection)
        .bind(after)
        .fetch_optional(&state.pool)
        .await
        .map_err(DbError::from)?
        .ok_or_else(|| {
            ApiError::Conflict("Music collection changed; reload the first page".into())
        })?
    } else {
        0
    };
    let mut items:Vec<Entry>=sqlx::query_as("SELECT * FROM music_collection_entries WHERE collection_id=$1 AND position>$2 ORDER BY position,id LIMIT 51").bind(collection).bind(position).fetch_all(&state.pool).await.map_err(DbError::from)?;
    let after = if items.len() > 50 {
        items.truncate(50);
        items.last().map(|v| v.id)
    } else {
        None
    };
    Ok(EntryPage { items, after })
}
pub async fn page_sources(
    state: &AppState,
    collection: i64,
    viewer: Option<i64>,
    page: &EntryPage,
) -> Result<Vec<Playable>, ApiError> {
    get(state, collection, viewer).await?;
    sources(state, &page.items, viewer).await
}
/// Return fifty authorized tracks; cursor references only a visible entry.
pub async fn playable_page(
    state: &AppState,
    collection: i64,
    viewer: Option<i64>,
    mut after: Option<i64>,
) -> Result<(Vec<Playable>, Option<i64>), ApiError> {
    get(state, collection, viewer).await?;
    let mut items = Vec::new();
    loop {
        let page = entry_page(state, collection, after).await?;
        items.extend(sources(state, &page.items, viewer).await?);
        if items.len() > 50 {
            items.truncate(50);
            let next = items.last().and_then(|v| v.entry_id.parse().ok());
            return Ok((items, next));
        }
        match page.after {
            Some(next) => after = Some(next),
            None => return Ok((items, None)),
        }
    }
}
async fn playable_status(state: &AppState, id: i64, viewer: Option<i64>) -> Result<bool, ApiError> {
    let Some(item) = status::find_by_id(&state.pool, id).await? else {
        return Ok(false);
    };
    if item.object_type.as_deref() != Some("Audio")
        || status::is_deleted(&state.pool, id).await?
        || item.reblog_of_id.is_some()
        || !crate::entities::can_view(&state.pool, &item, viewer).await?
    {
        return Ok(false);
    }
    let Some(author) = account::find_by_id(&state.pool, item.account_id).await? else {
        return Ok(false);
    };
    if !crate::instance_policy::public_account_visible(&state.pool, &state.config.domain, &author)
        .await?
    {
        return Ok(false);
    }
    let media = plamenu_db::media::for_statuses(&state.pool, &[id]).await?;
    Ok(media.get(&id).into_iter().flatten().any(|m| {
        m.content_type.starts_with("audio/")
            && (m.processing == "complete" || m.remote_url.is_some())
    }))
}
#[derive(Debug, Deserialize)]
pub struct Add {
    pub remote_id: Option<String>,
    pub asset_id: Option<String>,
    pub status_id: Option<String>,
    pub episode_id: Option<String>,
    pub revision: String,
}
pub async fn add(
    state: &AppState,
    owner: i64,
    collection: i64,
    input: &Add,
) -> Result<(), ApiError> {
    let revision = input
        .revision
        .parse()
        .map_err(|_| ApiError::Unprocessable("Invalid collection revision".into()))?;
    let mut source = crate::music_queues::Source {
        asset_id: input.asset_id.clone(),
        status_id: input.status_id.clone(),
        episode_id: input.episode_id.clone(),
        remote_id: input.remote_id.clone(),
    }
    .entry(0, collection, 0)?;
    crate::music_remote::normalize_sources(state, std::slice::from_mut(&mut source), owner).await?;
    let (asset, status, episode, remote) = (
        source.asset_id,
        source.status_id,
        source.episode_id,
        source.remote_id,
    );
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    let current = lock(&mut tx, owner, collection, revision).await?;
    let viewer = if current.visibility == "public" {
        None
    } else {
        Some(owner)
    };
    if let Some(asset) = asset {
        if current.share_tracks {
            let source = music_assets::get(state, asset, Some(owner)).await?;
            if source.owner_id != owner {
                music_assets::get(state, asset, viewer).await?;
            }
        } else {
            music_assets::get(state, asset, viewer).await?;
        }
    }
    if let Some(status) = status
        && !playable_status(state, status, viewer).await?
    {
        return Err(ApiError::NotFound);
    }
    if let Some(episode) = episode
        && !crate::music_podcasts::playables_pool(&state.pool, &[episode])
            .await?
            .contains_key(&episode)
    {
        return Err(ApiError::NotFound);
    }
    if let Some(remote) = remote
        && !crate::music_remote::playables_pool(
            &state.pool,
            &state.config.domain,
            &[remote],
            viewer,
        )
        .await?
        .contains_key(&remote)
    {
        return Err(ApiError::NotFound);
    }
    if current.kind == "library" {
        let exists:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM music_collection_entries WHERE collection_id=$1 AND asset_id IS NOT DISTINCT FROM $2 AND status_id IS NOT DISTINCT FROM $3 AND episode_id IS NOT DISTINCT FROM $4 AND remote_id IS NOT DISTINCT FROM $5)").bind(collection).bind(asset).bind(status).bind(episode).bind(remote).fetch_one(&mut *tx).await.map_err(DbError::from)?;
        if exists {
            tx.commit().await.map_err(DbError::from)?;
            return Ok(());
        }
    }
    let position: i32 = sqlx::query_scalar(
        "SELECT COALESCE(max(position),0)+1 FROM music_collection_entries WHERE collection_id=$1",
    )
    .bind(collection)
    .fetch_one(&mut *tx)
    .await
    .map_err(DbError::from)?;
    if position > 10000 {
        return Err(ApiError::Unprocessable("Music collection is full".into()));
    }
    sqlx::query("INSERT INTO music_collection_entries(id,collection_id,asset_id,status_id,episode_id,position,remote_id) VALUES($1,$2,$3,$4,$5,$6,$7)").bind(id::next()).bind(collection).bind(asset).bind(status).bind(episode).bind(position).bind(remote).execute(&mut *tx).await.map_err(DbError::from)?;
    crate::music_library_federation::added_conn(state, &mut tx, &current, asset, status).await?;
    bump(&mut tx, collection).await?;
    notify_change(state, &mut tx, collection).await?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
/// Add selected RSS episodes in one revisioned transaction, with library deduplication.
pub async fn add_podcast_episodes(
    state: &AppState,
    owner: i64,
    collection: i64,
    revision: i64,
    show: i64,
    episodes: &[i64],
) -> Result<(), ApiError> {
    if episodes.is_empty() || episodes.len() > 50 {
        return Err(ApiError::Unprocessable(
            "Choose up to 50 podcast episodes".into(),
        ));
    }
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    let current = lock(&mut tx, owner, collection, revision).await?;
    let unique: std::collections::HashSet<_> = episodes.iter().copied().collect();
    let found: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM music_podcast_episodes WHERE podcast_id=$1 AND id=ANY($2) FOR KEY SHARE",
    )
    .bind(show)
    .bind(episodes)
    .fetch_all(&mut *tx)
    .await
    .map_err(DbError::from)?;
    if found.len() != unique.len() {
        return Err(ApiError::NotFound);
    }
    let existing:Vec<i64>=sqlx::query_scalar("SELECT episode_id FROM music_collection_entries WHERE collection_id=$1 AND episode_id IS NOT NULL").bind(collection).fetch_all(&mut *tx).await.map_err(DbError::from)?;
    let mut seen: std::collections::HashSet<_> = existing.into_iter().collect();
    let additions: Vec<_> = episodes
        .iter()
        .copied()
        .filter(|episode| current.kind != "library" || seen.insert(*episode))
        .collect();
    if additions.is_empty() {
        tx.commit().await.map_err(DbError::from)?;
        return Ok(());
    }
    let position: i32 = sqlx::query_scalar(
        "SELECT COALESCE(max(position),0)+1 FROM music_collection_entries WHERE collection_id=$1",
    )
    .bind(collection)
    .fetch_one(&mut *tx)
    .await
    .map_err(DbError::from)?;
    if i64::from(position) + i64::try_from(additions.len()).unwrap_or(i64::MAX) > 10001 {
        return Err(ApiError::Unprocessable("Music collection is full".into()));
    }
    for (index, episode) in additions.into_iter().enumerate() {
        sqlx::query("INSERT INTO music_collection_entries(id,collection_id,episode_id,position) VALUES($1,$2,$3,$4)").bind(id::next()).bind(collection).bind(episode).bind(position+i32::try_from(index).unwrap_or(50)).execute(&mut *tx).await.map_err(DbError::from)?;
    }
    bump(&mut tx, collection).await?;
    notify_change(state, &mut tx, collection).await?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
/// Append canonical remote references atomically under the collection revision.
pub async fn add_remote_sources(
    state: &AppState,
    owner: i64,
    collection: i64,
    revision: i64,
    sources: &[i64],
) -> Result<(), ApiError> {
    if sources.is_empty() || sources.len() > 60 || sources.iter().any(|id| *id <= 0) {
        return Err(ApiError::Unprocessable(
            "Choose up to 60 remote tracks".into(),
        ));
    }
    let mut rows = sources
        .iter()
        .map(|remote| Entry {
            id: 0,
            collection_id: collection,
            position: 0,
            remote_id: Some(*remote),
            asset_id: None,
            status_id: None,
            episode_id: None,
        })
        .collect::<Vec<_>>();
    crate::music_remote::normalize_entries(&state.pool, &mut rows).await?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    let current = lock(&mut tx, owner, collection, revision).await?;
    let viewer = (current.visibility != "public").then_some(owner);
    let ids = rows
        .iter()
        .filter_map(|row| row.remote_id)
        .collect::<Vec<_>>();
    let playable =
        crate::music_remote::playables_pool(&state.pool, &state.config.domain, &ids, viewer)
            .await?;
    if ids.iter().any(|id| !playable.contains_key(id)) {
        return Err(ApiError::NotFound);
    }
    let existing: Vec<i64> = sqlx::query_scalar("SELECT remote_id FROM music_collection_entries WHERE collection_id=$1 AND remote_id IS NOT NULL")
        .bind(collection).fetch_all(&mut *tx).await.map_err(DbError::from)?;
    let mut seen = existing
        .into_iter()
        .collect::<std::collections::HashSet<_>>();
    let additions = ids
        .into_iter()
        .filter(|id| current.kind != "library" || seen.insert(*id))
        .collect::<Vec<_>>();
    if !additions.is_empty() {
        let position: i32 = sqlx::query_scalar("SELECT COALESCE(max(position),0)+1 FROM music_collection_entries WHERE collection_id=$1")
            .bind(collection).fetch_one(&mut *tx).await.map_err(DbError::from)?;
        if i64::from(position) + i64::try_from(additions.len()).unwrap_or(i64::MAX) > 10001 {
            return Err(ApiError::Unprocessable("Music collection is full".into()));
        }
        for (offset, remote) in additions.into_iter().enumerate() {
            sqlx::query("INSERT INTO music_collection_entries(id,collection_id,remote_id,position) VALUES($1,$2,$3,$4)")
                .bind(id::next()).bind(collection).bind(remote).bind(position+i32::try_from(offset).unwrap_or(60))
                .execute(&mut *tx).await.map_err(DbError::from)?;
        }
        bump(&mut tx, collection).await?;
        notify_change(state, &mut tx, collection).await?;
    }
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}

#[derive(Deserialize)]
pub struct Tags {
    pub revision: String,
    pub tags: Vec<String>,
}
pub enum TagAction {
    Replace,
    Add,
    Remove(String),
}
/// Tag management is independent of track access and uses the collection fence.
pub async fn tags(
    state: &AppState,
    owner: i64,
    collection: i64,
    input: &Tags,
    action: TagAction,
) -> Result<Collection, ApiError> {
    let revision = input
        .revision
        .parse()
        .map_err(|_| ApiError::Unprocessable("Invalid collection revision".into()))?;
    let normalized = input
        .tags
        .iter()
        .map(|tag| tag.trim().trim_start_matches('#').trim().to_owned())
        .collect::<Vec<_>>();
    let normalized = crate::music_entities::terms(&normalized)?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    let current = lock(&mut tx, owner, collection, revision).await?;
    let tags = match action {
        TagAction::Replace => normalized,
        TagAction::Add => {
            let mut merged = current.tags.clone();
            for tag in normalized {
                if !merged.contains(&tag) {
                    merged.push(tag);
                }
            }
            crate::music_entities::terms(&merged)?
        }
        TagAction::Remove(tag) => {
            let normalized = crate::music_entities::terms(&[tag
                .trim()
                .trim_start_matches('#')
                .trim()
                .to_owned()])?;
            current
                .tags
                .iter()
                .filter(|tag| !normalized.contains(tag))
                .cloned()
                .collect()
        }
    };
    if tags != current.tags {
        sqlx::query("UPDATE music_collections SET tags=$2 WHERE id=$1")
            .bind(collection)
            .bind(&tags)
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
        bump(&mut tx, collection).await?;
        notify_change(state, &mut tx, collection).await?;
    }
    let updated = sqlx::query_as("SELECT * FROM music_collections WHERE id=$1")
        .bind(collection)
        .fetch_one(&mut *tx)
        .await
        .map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(updated)
}
#[derive(Debug, Deserialize)]
pub struct Reorder {
    pub revision: String,
    pub entry_ids: Vec<String>,
}
pub async fn reorder(
    state: &AppState,
    owner: i64,
    collection: i64,
    input: &Reorder,
) -> Result<(), ApiError> {
    let revision = input
        .revision
        .parse()
        .map_err(|_| ApiError::Unprocessable("Invalid collection revision".into()))?;
    if input.entry_ids.len() > 10000 {
        return Err(ApiError::Unprocessable("Music collection is full".into()));
    }
    let ids = input
        .entry_ids
        .iter()
        .map(|v| v.parse::<i64>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ApiError::Unprocessable("Invalid music entry".into()))?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    lock(&mut tx, owner, collection, revision).await?;
    let mut current: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM music_collection_entries WHERE collection_id=$1 ORDER BY id FOR UPDATE",
    )
    .bind(collection)
    .fetch_all(&mut *tx)
    .await
    .map_err(DbError::from)?;
    let mut provided = ids.clone();
    provided.sort_unstable();
    current.sort_unstable();
    if current != provided {
        return Err(ApiError::Conflict(
            "Entry list changed; reload before reordering".into(),
        ));
    }
    sqlx::query("UPDATE music_collection_entries e SET position=desired.position::integer FROM unnest($2::bigint[]) WITH ORDINALITY desired(id,position) WHERE e.collection_id=$1 AND e.id=desired.id").bind(collection).bind(ids).execute(&mut *tx).await.map_err(DbError::from)?;
    bump(&mut tx, collection).await?;
    notify_change(state, &mut tx, collection).await?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
pub async fn remove(
    state: &AppState,
    owner: i64,
    collection: i64,
    revision: i64,
    entry: i64,
) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    lock(&mut tx, owner, collection, revision).await?;
    let deleted =
        sqlx::query("DELETE FROM music_collection_entries WHERE collection_id=$1 AND id=$2")
            .bind(collection)
            .bind(entry)
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?
            .rows_affected();
    if deleted == 0 {
        return Err(ApiError::NotFound);
    }
    // Normalize gaps so a full playlist remains appendable after removals.
    sqlx::query("UPDATE music_collection_entries e SET position=ordered.position::integer FROM (SELECT id,row_number() OVER(ORDER BY position,id) position FROM music_collection_entries WHERE collection_id=$1) ordered WHERE e.id=ordered.id").bind(collection).execute(&mut *tx).await.map_err(DbError::from)?;

    notify_change(state, &mut tx, collection).await?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
pub async fn m3u(
    state: &AppState,
    collection: i64,
    viewer: Option<i64>,
) -> Result<String, ApiError> {
    let items = playables(state, collection, viewer).await?;
    let mut output = String::from("#EXTM3U\n");
    for item in items {
        let title = item.title.replace(['\r', '\n'], " ");
        let url = if item.stream_url.starts_with('/') {
            format!("https://{}{}", state.config.domain, item.stream_url)
        } else {
            item.stream_url
        };
        if url.contains(['\r', '\n']) {
            continue;
        }
        let _ = write!(
            output,
            "#EXTINF:{},{}\n{}\n",
            item.duration.max(0.0).floor(),
            title,
            url
        );
    }
    Ok(output)
}

/// Resolve a catalogue link to a stored, origin-owned source. This does not
/// dereference a player-supplied URL or create a new social publication.
pub async fn resolve_reference(
    state: &AppState,
    url: &str,
) -> Result<(Option<String>, Option<String>), ApiError> {
    let parsed = url::Url::parse(url).map_err(|_| {
        ApiError::Unprocessable("Choose a Music library track or Audio post URL".into())
    })?;
    if parsed.scheme() != "https"
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || url.len() > 2048
    {
        return Err(ApiError::Unprocessable(
            "Choose a canonical music URL".into(),
        ));
    }
    let origin = url::Url::parse(&format!("https://{}", state.config.domain))
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    if parsed.origin() == origin.origin()
        && let Some(id) = parsed
            .path()
            .strip_prefix("/music/assets/")
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|v| *v > 0)
        && url == format!("https://{}/music/assets/{id}", state.config.domain)
    {
        return Ok((Some(id.to_string()), None));
    }
    if parsed.origin() == origin.origin()
        && let Some(id) = parsed
            .path()
            .rsplit('/')
            .next()
            .and_then(|v| v.parse::<i64>().ok())
        && let Some(item) = status::find_by_id(&state.pool, id).await?
        && let Some(author) = account::find_by_id(&state.pool, item.account_id).await?
        && (url == crate::entities::status_web_url(&state.config.domain, &item, &author.username)
            || url == crate::entities::status_uri_for_account(&state.config.domain, &item, &author))
    {
        return Ok((None, Some(id.to_string())));
    }
    let ids:Vec<i64>=sqlx::query_scalar("SELECT id FROM statuses WHERE (uri=$1 OR url=$1) AND object_type='Audio' AND deleted_at IS NULL LIMIT 2\n-- STUBFILTER: references resolve only live Audio publications").bind(url).fetch_all(&state.pool).await.map_err(DbError::from)?;
    if ids.len() != 1 {
        return Err(ApiError::NotFound);
    }
    Ok((None, Some(ids[0].to_string())))
}

/// Only an exact native episode URL resolves; enclosure URLs are never imported.
pub async fn resolve_episode_reference(
    state: &AppState,
    raw: &str,
) -> Result<Option<String>, ApiError> {
    let prefix = format!("https://{}/music/podcasts/episodes/", state.config.domain);
    let Some(id) = raw
        .strip_prefix(&prefix)
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|v| *v > 0)
    else {
        return Ok(None);
    };
    if raw != format!("{prefix}{id}") {
        return Ok(None);
    }
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM music_podcast_episodes WHERE id=$1)")
            .bind(id)
            .fetch_one(&state.pool)
            .await
            .map_err(DbError::from)?;
    if !exists {
        return Err(ApiError::NotFound);
    }
    Ok(Some(id.to_string()))
}

/// Container access is independent from every entry's own authorization.
pub async fn grant(
    state: &AppState,
    owner: i64,
    collection: i64,
    revision: i64,
    member: i64,
    enabled: bool,
) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    lock(&mut tx, owner, collection, revision).await?;
    if enabled {
        if !music_assets::lock_recipient(&mut tx, member).await? {
            return Err(ApiError::NotFound);
        }
        let inserted:Option<i64>=sqlx::query_scalar("INSERT INTO music_collection_grants(collection_id,account_id) VALUES($1,$2) ON CONFLICT DO NOTHING RETURNING id").bind(collection).bind(member).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
        if let Some(grant) = inserted {
            crate::music_shares::notify_grant(
                state,
                &mut tx,
                owner,
                member,
                crate::music_saved::Kind::Library,
                grant,
            )
            .await?;
        }
    } else {
        sqlx::query("DELETE FROM music_collection_grants WHERE collection_id=$1 AND account_id=$2")
            .bind(collection)
            .bind(member)
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
    }
    bump(&mut tx, collection).await?;
    notify_change(state, &mut tx, collection).await?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}

async fn notify_change(
    state: &AppState,
    conn: &mut sqlx::PgConnection,
    collection: i64,
) -> Result<(), ApiError> {
    let row: Collection = sqlx::query_as("SELECT * FROM music_collections WHERE id=$1")
        .bind(collection)
        .fetch_one(&mut *conn)
        .await
        .map_err(DbError::from)?;
    crate::music_library_federation::changed_conn(state, conn, &row, false).await
}

/// Atomic compatibility-client playlist edits reuse native validation and fences.
pub(crate) async fn edit_client_playlist(
    state: &AppState,
    principal: &crate::music_clients::auth::Principal,
    input: &New,
    previous: Option<&Collection>,
    remove: &std::collections::HashSet<i64>,
    additions: &[Entry],
    replace: bool,
) -> Result<Collection, ApiError> {
    validate(input)?;
    let owner = principal.owner;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, owner).await?;
    crate::music_queues::active_owner(&mut tx, owner).await?;
    principal.current(state).await?;
    let current = if let Some(previous) = previous {
        let current = lock(&mut tx, owner, previous.id, previous.revision).await?;
        if current.kind != "playlist" {
            return Err(ApiError::NotFound);
        }
        current
    } else {
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM music_collections WHERE owner_id=$1")
                .bind(owner)
                .fetch_one(&mut *tx)
                .await
                .map_err(DbError::from)?;
        if count >= 100 {
            return Err(ApiError::Unprocessable("Too many music collections".into()));
        }
        sqlx::query_as("INSERT INTO music_collections(id,owner_id,name,description,kind,visibility) VALUES($1,$2,$3,$4,'playlist',$5) RETURNING *").bind(id::next()).bind(owner).bind(input.name.trim()).bind(input.description.trim()).bind(&input.visibility).fetch_one(&mut *tx).await.map_err(DbError::from)?
    };
    let viewer = (input.visibility != "public").then_some(owner);
    let readable = crate::music_remote::readable_entries(&state.pool, additions, viewer).await?;
    let playable = sources(state, &readable, viewer).await?;
    // Temporary ordinal ids preserve repeated songs when building the allow set.
    let allowed = playable
        .into_iter()
        .map(|row| row.entry_id)
        .collect::<std::collections::HashSet<_>>();
    let rows = readable
        .into_iter()
        .filter(|row| allowed.contains(&row.id.to_string()))
        .collect::<Vec<_>>();
    if replace {
        // Client-invisible episodes survive a song-list replacement.
        sqlx::query(
            "DELETE FROM music_collection_entries WHERE collection_id=$1 AND episode_id IS NULL",
        )
        .bind(current.id)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    } else {
        sqlx::query("DELETE FROM music_collection_entries WHERE collection_id=$1 AND id=ANY($2)")
            .bind(current.id)
            .bind(remove.iter().copied().collect::<Vec<_>>())
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
    }
    sqlx::query("UPDATE music_collection_entries e SET position=ordered.position::integer FROM (SELECT id,row_number() OVER(ORDER BY position,id) position FROM music_collection_entries WHERE collection_id=$1) ordered WHERE e.id=ordered.id").bind(current.id).execute(&mut *tx).await.map_err(DbError::from)?;
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM music_collection_entries WHERE collection_id=$1")
            .bind(current.id)
            .fetch_one(&mut *tx)
            .await
            .map_err(DbError::from)?;
    if count + i64::try_from(rows.len()).unwrap_or(i64::MAX) > 10000 {
        return Err(ApiError::Unprocessable("Music playlist is full".into()));
    }
    for (index, row) in rows.into_iter().enumerate() {
        let position =
            i32::try_from(count).unwrap_or(10000) + i32::try_from(index).unwrap_or(500) + 1;
        sqlx::query("INSERT INTO music_collection_entries(id,collection_id,asset_id,status_id,remote_id,position) VALUES($1,$2,$3,$4,$5,$6)").bind(id::next()).bind(current.id).bind(row.asset_id).bind(row.status_id).bind(row.remote_id).bind(position).execute(&mut *tx).await.map_err(DbError::from)?;
        crate::music_library_federation::added_conn(
            state,
            &mut tx,
            &current,
            row.asset_id,
            row.status_id,
        )
        .await?;
    }
    let updated:Collection=sqlx::query_as("UPDATE music_collections SET name=$2,description=$3,visibility=$4,revision=revision+1,updated_at=now() WHERE id=$1 RETURNING *").bind(current.id).bind(input.name.trim()).bind(input.description.trim()).bind(&input.visibility).fetch_one(&mut *tx).await.map_err(DbError::from)?;
    crate::music_library_federation::changed_conn(
        state,
        &mut tx,
        if input.visibility == "public" {
            &updated
        } else {
            &current
        },
        input.visibility != "public",
    )
    .await?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(updated)
}
