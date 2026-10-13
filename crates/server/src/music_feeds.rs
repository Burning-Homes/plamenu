//! RSS/Atom syndication resolves the same current sources as native playback.
use crate::{AppState, error::ApiError, music_collections};
use plamenu_db::DbError;
use std::fmt::Write as _;
use time::{
    OffsetDateTime,
    format_description::well_known::{Rfc2822, Rfc3339},
};

#[derive(Clone, Copy)]
pub enum Format {
    Rss,
    Atom,
}
impl Format {
    pub fn parse(value: &str) -> Result<Self, ApiError> {
        match value {
            "rss" => Ok(Self::Rss),
            "atom" => Ok(Self::Atom),
            _ => Err(ApiError::NotFound),
        }
    }
    #[must_use]
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Rss => "application/rss+xml; charset=utf-8",
            Self::Atom => "application/atom+xml; charset=utf-8",
        }
    }
}
pub struct Item {
    id: String,
    browse_key: i64,
    browse_family: u8,
    pub title: String,
    pub link: String,
    description: String,
    author: String,
    published: OffsetDateTime,
    updated: OffsetDateTime,
    enclosure: Option<(String, String, i64)>,
}
pub struct Feed {
    id: String,
    title: String,
    link: String,
    description: String,
    updated: OffsetDateTime,
    pub items: Vec<Item>,
}
#[derive(sqlx::FromRow)]
struct EntryMetadata {
    id: i64,
    added: OffsetDateTime,
    updated: OffsetDateTime,
    content_type: String,
    length: i64,
}
fn absolute(domain: &str, path: &str) -> String {
    if path.starts_with('/') {
        format!("https://{domain}{path}")
    } else {
        path.to_owned()
    }
}
/// Feed limits apply after source authorization, so hidden sources never take
/// the place of readable items. The collection is capped at 10,000 references.
pub async fn collection(
    state: &AppState,
    collection: i64,
    viewer: Option<i64>,
    limit: usize,
) -> Result<Feed, ApiError> {
    collection_inner(state, collection, viewer, limit, None).await
}
pub async fn shared_collection(
    state: &AppState,
    collection: i64,
    viewer: Option<i64>,
    limit: usize,
    token: &str,
) -> Result<Feed, ApiError> {
    let scope = crate::music_shares::resolve(state, token, viewer).await?;
    if scope.target_id != collection || !matches!(scope.kind.as_str(), "library" | "playlist") {
        return Err(ApiError::NotFound);
    }
    let feed = collection_inner(state, collection, viewer, limit, Some((&scope, token))).await?;
    crate::music_shares::resolve(state, token, viewer).await?;
    Ok(feed)
}
async fn collection_inner(
    state: &AppState,
    collection: i64,
    viewer: Option<i64>,
    limit: usize,
    capability: Option<(&crate::music_shares::Scope, &str)>,
) -> Result<Feed, ApiError> {
    if !(1..=100).contains(&limit) {
        return Err(ApiError::Unprocessable(
            "Feed size must be between 1 and 100".into(),
        ));
    }
    let parent = music_collections::get(
        state,
        collection,
        capability.map_or(viewer, |(scope, _)| Some(scope.owner_id)),
    )
    .await?;
    let (selected, mut playable) =
        latest_entries(state, collection, viewer, limit, capability).await?;
    let ids: Vec<i64> = playable
        .iter()
        .filter_map(|p| p.entry_id.parse().ok())
        .collect();
    let mut metadata:Vec<EntryMetadata>=sqlx::query_as("-- STUBKEEP: metadata for already-authorized playable entry ids\nSELECT e.id,e.created_at AS added,COALESCE(t.updated_at,s.edited_at,s.created_at,p.updated_at,e.created_at) AS updated,COALESCE(p.audio_type,media.content_type,'audio/mpeg') AS content_type,COALESCE(t.stream_bytes,media.file_size,p.audio_length,0) AS length FROM music_collection_entries e LEFT JOIN music_assets t ON t.id=e.asset_id LEFT JOIN statuses s ON s.id=e.status_id LEFT JOIN music_podcast_episodes p ON p.id=e.episode_id LEFT JOIN LATERAL(SELECT m.content_type,m.file_size FROM media_attachments m WHERE m.status_id=s.id AND (m.kind='audio' OR m.content_type LIKE 'audio/%') ORDER BY m.id LIMIT 1) media ON true WHERE e.collection_id=$1 AND e.id=ANY($2)").bind(collection).bind(&ids).fetch_all(&state.pool).await.map_err(DbError::from)?;
    let mut notes = captions(state, &selected, viewer).await?;
    remote_entry_metadata(state, &selected, viewer, &mut metadata, &mut notes).await?;
    if let Some((scope, _)) = capability {
        let ids = selected
            .iter()
            .filter_map(|e| e.asset_id)
            .collect::<Vec<_>>();
        let assets = crate::music_shares::readable_assets(state, scope, &ids, viewer).await?;
        for entry in &selected {
            if let Some(asset) = entry.asset_id.and_then(|id| assets.get(&id)) {
                notes.insert(
                    entry.id,
                    asset_item(
                        &state.config.domain,
                        DatedAsset {
                            asset: asset.clone(),
                            created_at: OffsetDateTime::UNIX_EPOCH,
                            updated_at: OffsetDateTime::UNIX_EPOCH,
                        },
                    )
                    .description,
                );
            }
        }
    }
    playable = collection_sources(state, &selected, viewer, capability).await?;
    playable.sort_by_cached_key(|p| std::cmp::Reverse(p.entry_id.parse::<i64>().unwrap_or(0)));
    music_collections::get(
        state,
        collection,
        capability.map_or(viewer, |(scope, _)| Some(scope.owner_id)),
    )
    .await?;
    let metadata: std::collections::HashMap<_, _> = metadata
        .into_iter()
        .map(|entry| (entry.id, entry))
        .collect();
    let domain = &state.config.domain;
    let link = capability.map_or_else(
        || format!("https://{domain}/music/collections/{collection}"),
        |(_, token)| format!("https://{domain}/music/share?token={token}"),
    );
    let items = playable
        .into_iter()
        .filter_map(|source| {
            let row = metadata.get(&source.entry_id.parse::<i64>().ok()?)?;
            Some(Item {
                id: format!("{link}#entry-{}", row.id),
                browse_key: row.id,
                browse_family: 0,
                title: source.title,
                link: absolute(domain, &source.url),
                description: notes.get(&row.id).cloned().unwrap_or_default(),
                author: source.artist,
                published: row.added,
                updated: row.updated.max(row.added),
                enclosure: Some((
                    absolute(domain, &source.stream_url),
                    row.content_type.clone(),
                    row.length,
                )),
            })
        })
        .collect::<Vec<_>>();
    let updated = items
        .iter()
        .map(|item| item.updated)
        .max()
        .unwrap_or(parent.updated_at)
        .max(parent.updated_at);
    Ok(Feed {
        id: link.clone(),
        title: parent.name,
        link,
        description: parent.description,
        updated,
        items,
    })
}
/// Public protocol readers and signed-in listeners use the native profile ACLs.
pub async fn profile(
    state: &AppState,
    username: &str,
    viewer: Option<i64>,
    limit: usize,
) -> Result<Feed, ApiError> {
    if !(1..=100).contains(&limit) {
        return Err(ApiError::Unprocessable(
            "Feed size must be between 1 and 100".into(),
        ));
    }
    let account = plamenu_db::account::find_public_local_by_username(&state.pool, username)
        .await?
        .ok_or(ApiError::NotFound)?;
    if account.suspended()
        || plamenu_db::account::is_deleted(&state.pool, account.id).await?
        || (viewer.is_none()
            && plamenu_db::account::effectively_silenced(&state.pool, account.id).await?)
    {
        return Err(ApiError::NotFound);
    }
    let hidden: bool = sqlx::query_scalar("SELECT account_hidden($1,$2)")
        .bind(viewer)
        .bind(account.id)
        .fetch_one(&state.pool)
        .await
        .map_err(DbError::from)?;
    if hidden {
        return Err(ApiError::NotFound);
    }
    let rows = plamenu_db::status::by_account(
        &state.pool,
        account.id,
        viewer,
        &plamenu_db::status::AccountStatusesFilter {
            exclude_replies: true,
            exclude_reblogs: false,
            only_media: false,
            media_through_reblog: false,
            tagged: None,
            max_id: None,
            since_id: None,
        },
        plamenu_db::user::TimelineOrder::default(),
        i64::try_from(limit).unwrap_or(100),
    )
    .await?;
    let items = status_items(state, &rows, viewer).await?;
    let link = format!("https://{}/@{}", state.config.domain, account.username);
    let updated = items
        .iter()
        .map(|item| item.updated)
        .max()
        .unwrap_or(account.updated_at)
        .max(account.updated_at);
    Ok(Feed {
        id: link.clone(),
        title: if account.display_name.is_empty() {
            account.username
        } else {
            account.display_name
        },
        link,
        description: crate::filters::plain_text(&account.note),
        updated,
        items,
    })
}
async fn status_items(
    state: &AppState,
    rows: &[plamenu_db::status::Status],
    viewer: Option<i64>,
) -> Result<Vec<Item>, ApiError> {
    let rendered =
        crate::entities::render_statuses(&state.pool, &state.config.domain, rows, viewer).await?;
    let media_ids = rendered
        .iter()
        .flat_map(|value| {
            status_content(value)["media_attachments"]
                .as_array()
                .into_iter()
                .flatten()
        })
        .filter_map(|media| media["id"].as_str()?.parse::<i64>().ok())
        .collect::<Vec<_>>();
    let media:Vec<MediaMetadata>=sqlx::query_as("SELECT id,content_type,COALESCE(file_size,0) AS length FROM media_attachments WHERE id=ANY($1)").bind(&media_ids).fetch_all(&state.pool).await.map_err(DbError::from)?;
    let media = media
        .into_iter()
        .map(|item| (item.id, item))
        .collect::<std::collections::HashMap<_, _>>();
    let items = rendered
        .iter()
        .filter_map(|item| status_item(item, &media))
        .collect::<Vec<_>>();
    Ok(items)
}
#[derive(sqlx::FromRow)]
struct MediaMetadata {
    id: i64,
    content_type: String,
    length: i64,
}
fn status_content(raw: &serde_json::Value) -> &serde_json::Value {
    if raw["reblog"].is_object() {
        &raw["reblog"]
    } else {
        raw
    }
}
fn status_item(
    raw: &serde_json::Value,
    media_rows: &std::collections::HashMap<i64, MediaMetadata>,
) -> Option<Item> {
    let content = status_content(raw);
    let description =
        plamenu_ap::text::sanitize_remote_html(content["content"].as_str().unwrap_or_default());
    let title = content["spoiler_text"]
        .as_str()
        .filter(|value| !value.is_empty())
        .or_else(|| content["title"].as_str())
        .map_or_else(
            || {
                crate::filters::plain_text(&description)
                    .chars()
                    .take(120)
                    .collect()
            },
            str::to_owned,
        );
    let published = OffsetDateTime::parse(raw["created_at"].as_str()?, &Rfc3339).ok()?;
    let updated = raw["edited_at"]
        .as_str()
        .and_then(|date| OffsetDateTime::parse(date, &Rfc3339).ok())
        .unwrap_or(published);
    let enclosure = content["media_attachments"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["type"] == "audio"))
        .and_then(|media| {
            let row = media_rows.get(&media["id"].as_str()?.parse::<i64>().ok()?)?;
            Some((
                media["url"].as_str()?.to_owned(),
                row.content_type.clone(),
                row.length,
            ))
        });
    let author = content["account"]["display_name"]
        .as_str()
        .filter(|value| !value.is_empty())
        .or_else(|| content["account"]["username"].as_str())
        .unwrap_or_default()
        .to_owned();
    Some(Item {
        id: raw["uri"].as_str()?.to_owned(),
        browse_key: raw["id"].as_str()?.parse().ok()?,
        browse_family: 0,
        title: if title.is_empty() {
            author.clone()
        } else {
            title
        },
        link: raw["url"]
            .as_str()
            .or_else(|| content["url"].as_str())?
            .to_owned(),
        description,
        author,
        published,
        updated,
        enclosure,
    })
}
macro_rules! artist_albums {
    () => {
        crate::music_entities::entity_batch!(
            crate::music_entities::album_select!("WHERE a.artist_id=$1"),
            crate::music_assets::asset_query!(" WHERE a.artist_id=$1 ", ""),
            "SELECT album_id"
        )
    };
}
const ARTIST_RELEASE_ALBUMS: &str = concat!(
    artist_albums!(),
    " AND NOT EXISTS(SELECT 1 FROM mutes m WHERE m.account_id=$2 AND m.target_account_id=e.owner_id AND (m.expires_at IS NULL OR m.expires_at>now())) ORDER BY e.created_at DESC,e.id DESC LIMIT $3"
);
const ARTIST_RELEASE_TRACKS: &str = concat!(
    "WITH visible_albums AS (",
    artist_albums!(),
    ") SELECT tracks.* FROM (",
    crate::music_assets::asset_query!(" WHERE t.artist_id=$1 ", ""),
    ") tracks WHERE NOT EXISTS(SELECT 1 FROM visible_albums visible WHERE visible.id=tracks.album_id) AND NOT EXISTS(SELECT 1 FROM mutes m WHERE m.account_id=$2 AND m.target_account_id=tracks.owner_id AND (m.expires_at IS NULL OR m.expires_at>now())) ORDER BY tracks.created_at DESC,tracks.id DESC LIMIT $3"
);
#[derive(sqlx::FromRow)]
struct DatedAsset {
    #[sqlx(flatten)]
    asset: crate::music_assets::Asset,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}
/// Readable albums represent their tracks; unreadable albums never hide an
/// independently readable standalone track from the artist's release feed.
pub async fn artist(
    state: &AppState,
    artist: i64,
    viewer: Option<i64>,
    limit: usize,
) -> Result<Feed, ApiError> {
    if !(1..=100).contains(&limit) {
        return Err(ApiError::Unprocessable(
            "Feed size must be between 1 and 100".into(),
        ));
    }
    let parent =
        crate::music_entities::get(state, crate::music_entities::Kind::Artist, artist, viewer)
            .await?;
    let bound = i64::try_from(limit).unwrap_or(100);
    let albums: Vec<crate::music_entities::Entity> = sqlx::query_as(ARTIST_RELEASE_ALBUMS)
        .bind(artist)
        .bind(viewer)
        .bind(bound)
        .fetch_all(&state.pool)
        .await
        .map_err(DbError::from)?;
    let tracks: Vec<DatedAsset> = sqlx::query_as(ARTIST_RELEASE_TRACKS)
        .bind(artist)
        .bind(viewer)
        .bind(bound)
        .fetch_all(&state.pool)
        .await
        .map_err(DbError::from)?;
    let domain = &state.config.domain;
    let mut items = albums
        .into_iter()
        .map(|album| album_item(domain, album))
        .chain(tracks.into_iter().map(|track| asset_item(domain, track)))
        .collect::<Vec<_>>();
    items.sort_by(|a, b| b.published.cmp(&a.published).then_with(|| b.id.cmp(&a.id)));
    items.truncate(limit);
    crate::music_entities::get(state, crate::music_entities::Kind::Artist, artist, viewer).await?;
    let link = format!("https://{domain}/music/artists/{artist}");
    let updated = items
        .iter()
        .map(|item| item.updated)
        .max()
        .unwrap_or(parent.updated_at)
        .max(parent.updated_at);
    Ok(Feed {
        id: link.clone(),
        title: parent.name,
        link,
        description: parent.description,
        updated,
        items,
    })
}
fn album_item(domain: &str, album: crate::music_entities::Entity) -> Item {
    let link = format!("https://{domain}/music/albums/{}", album.id);
    Item {
        id: link.clone(),
        browse_key: album.id,
        browse_family: 2,
        title: format!("{} — {}", album.artist, album.name),
        link,
        description: plamenu_ap::text::plain_to_html(&album.description),
        author: album.artist,
        published: album.created_at,
        updated: album.updated_at,
        enclosure: None,
    }
}
#[derive(Clone, Copy)]
pub enum Term {
    Tag,
    Genre,
}
impl Term {
    #[must_use]
    pub fn path(self) -> &'static str {
        match self {
            Self::Tag => "tags",
            Self::Genre => "genres",
        }
    }
}
#[must_use]
pub fn term_segment(name: &str) -> String {
    url::form_urlencoded::byte_serialize(name.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}
pub fn term_name(raw: &str) -> Result<String, ApiError> {
    let normalized =
        crate::music_entities::terms(&[raw.trim().trim_start_matches('#').trim().to_owned()])?;
    normalized.into_iter().next().ok_or(ApiError::NotFound)
}
macro_rules! term_albums {
    () => {
        crate::music_entities::entity_batch!(
            crate::music_entities::album_select!(
                "WHERE (($4 AND $1=ANY(a.tags)) OR (NOT $4 AND $1=ANY(a.genres)))"
            ),
            crate::music_assets::asset_query!(
                " WHERE (($4 AND $1=ANY(a.tags)) OR (NOT $4 AND $1=ANY(a.genres))) ",
                ""
            ),
            "SELECT album_id"
        )
    };
}
const FEED_TERM_ALBUMS: &str = concat!(
    term_albums!(),
    " AND NOT EXISTS(SELECT 1 FROM mutes m WHERE m.account_id=$2 AND m.target_account_id=e.owner_id AND (m.expires_at IS NULL OR m.expires_at>now())) AND $5::bigint IS NULL ORDER BY e.created_at DESC,e.id DESC LIMIT $3"
);
const FEED_TERM_TRACKS: &str = crate::music_assets::asset_query!(
    " WHERE (($4 AND $1=ANY(t.tags)) OR (NOT $4 AND EXISTS(SELECT 1 FROM unnest(regexp_split_to_array(lower(t.genre),'[;,|]')) term WHERE trim(term)=$1))) ",
    " AND NOT EXISTS(SELECT 1 FROM mutes m WHERE m.account_id=$2 AND m.target_account_id=t.owner_id AND (m.expires_at IS NULL OR m.expires_at>now())) AND $5::bigint IS NULL ORDER BY t.created_at DESC,t.id DESC LIMIT $3"
);
const FEED_TERM_COLLECTIONS: &str = "SELECT c.* FROM music_collections c JOIN accounts author ON author.id=c.owner_id WHERE $1=ANY(c.tags) AND (c.owner_id=$2 OR c.visibility='public' OR (c.visibility='local' AND EXISTS(SELECT 1 FROM accounts viewer WHERE viewer.id=$2 AND viewer.domain IS NULL)) OR EXISTS(SELECT 1 FROM music_collection_grants grant_row WHERE grant_row.collection_id=c.id AND grant_row.account_id=$2)) AND author.suspended_at IS NULL AND author.deleted_at IS NULL AND NOT EXISTS(SELECT 1 FROM users u WHERE u.account_id=author.id AND (NOT u.approved OR u.confirmed_at IS NULL)) AND NOT EXISTS(SELECT 1 FROM blocks b WHERE (b.account_id=$2 AND b.target_account_id=c.owner_id) OR (b.account_id=c.owner_id AND b.target_account_id=$2)) AND NOT EXISTS(SELECT 1 FROM mutes m WHERE m.account_id=$2 AND m.target_account_id=c.owner_id AND (m.expires_at IS NULL OR m.expires_at>now())) AND $4::bigint IS NULL ORDER BY c.created_at DESC,c.id DESC LIMIT $3";
const TERM_ALBUMS: &str = concat!(
    term_albums!(),
    " AND NOT EXISTS(SELECT 1 FROM mutes m WHERE m.account_id=$2 AND m.target_account_id=e.owner_id AND (m.expires_at IS NULL OR m.expires_at>now())) AND ($5::bigint IS NULL OR e.id <= $5) ORDER BY e.id DESC LIMIT $3"
);
const TERM_TRACKS: &str = crate::music_assets::asset_query!(
    " WHERE (($4 AND $1=ANY(t.tags)) OR (NOT $4 AND EXISTS(SELECT 1 FROM unnest(regexp_split_to_array(lower(t.genre),'[;,|]')) term WHERE trim(term)=$1))) ",
    " AND NOT EXISTS(SELECT 1 FROM mutes m WHERE m.account_id=$2 AND m.target_account_id=t.owner_id AND (m.expires_at IS NULL OR m.expires_at>now())) AND ($5::bigint IS NULL OR t.id <= $5) ORDER BY t.id DESC LIMIT $3"
);
const TERM_COLLECTIONS: &str = "SELECT c.* FROM music_collections c JOIN accounts author ON author.id=c.owner_id WHERE $1=ANY(c.tags) AND (c.owner_id=$2 OR c.visibility='public' OR (c.visibility='local' AND EXISTS(SELECT 1 FROM accounts viewer WHERE viewer.id=$2 AND viewer.domain IS NULL)) OR EXISTS(SELECT 1 FROM music_collection_grants grant_row WHERE grant_row.collection_id=c.id AND grant_row.account_id=$2)) AND author.suspended_at IS NULL AND author.deleted_at IS NULL AND NOT EXISTS(SELECT 1 FROM users u WHERE u.account_id=author.id AND (NOT u.approved OR u.confirmed_at IS NULL)) AND NOT EXISTS(SELECT 1 FROM blocks b WHERE (b.account_id=$2 AND b.target_account_id=c.owner_id) OR (b.account_id=c.owner_id AND b.target_account_id=$2)) AND NOT EXISTS(SELECT 1 FROM mutes m WHERE m.account_id=$2 AND m.target_account_id=c.owner_id AND (m.expires_at IS NULL OR m.expires_at>now())) AND ($4::bigint IS NULL OR c.id <= $4) ORDER BY c.id DESC LIMIT $3";
#[derive(sqlx::FromRow)]
struct DatedCollection {
    #[sqlx(flatten)]
    collection: crate::music_collections::Collection,
    created_at: OffsetDateTime,
}
/// Exact normalized terms merge only currently readable source families.
pub async fn term(
    state: &AppState,
    kind: Term,
    raw: &str,
    viewer: Option<i64>,
    limit: usize,
) -> Result<Feed, ApiError> {
    if !(1..=100).contains(&limit) {
        return Err(ApiError::Unprocessable(
            "Feed size must be between 1 and 100".into(),
        ));
    }
    term_inner(state, kind, raw, viewer, limit, None, false).await
}

/// A global numeric key and family tie-breaker preserve every typed resource.
#[derive(Clone, Copy)]
struct TermCursor {
    key: i64,
    family: u8,
}
impl TermCursor {
    fn parse(raw: &str) -> Result<Self, ApiError> {
        let (key, family) = raw
            .split_once(':')
            .ok_or_else(|| ApiError::Unprocessable("Invalid music cursor".into()))?;
        let key = key.parse::<i64>().ok().filter(|key| *key > 0);
        let family = family.parse::<u8>().ok().filter(|family| *family <= 4);
        match (key, family) {
            (Some(key), Some(family)) => Ok(Self { key, family }),
            _ => Err(ApiError::Unprocessable("Invalid music cursor".into())),
        }
    }
}
pub struct TermPage {
    pub items: Vec<Item>,
    pub before: Option<String>,
}
pub async fn term_page(
    state: &AppState,
    kind: Term,
    raw: &str,
    viewer: Option<i64>,
    before: Option<&str>,
) -> Result<TermPage, ApiError> {
    let cursor = before.map(TermCursor::parse).transpose()?;
    let mut feed = term_inner(state, kind, raw, viewer, 61, cursor, true).await?;
    let more = feed.items.len() > 60;
    feed.items.truncate(60);
    let before = more
        .then(|| feed.items.last())
        .flatten()
        .map(|item| format!("{}:{}", item.browse_key, item.browse_family));
    Ok(TermPage {
        items: feed.items,
        before,
    })
}
async fn term_inner(
    state: &AppState,
    kind: Term,
    raw: &str,
    viewer: Option<i64>,
    limit: usize,
    cursor: Option<TermCursor>,
    browse: bool,
) -> Result<Feed, ApiError> {
    let name = term_name(raw)?;
    let bound = i64::try_from(limit + usize::from(browse)).unwrap_or(101);
    let tag = matches!(kind, Term::Tag);
    let albums: Vec<crate::music_entities::Entity> = sqlx::query_as(if browse {
        TERM_ALBUMS
    } else {
        FEED_TERM_ALBUMS
    })
    .bind(&name)
    .bind(viewer)
    .bind(bound)
    .bind(tag)
    .bind(cursor.map(|cursor| cursor.key))
    .fetch_all(&state.pool)
    .await
    .map_err(DbError::from)?;
    let tracks: Vec<DatedAsset> = sqlx::query_as(if browse {
        TERM_TRACKS
    } else {
        FEED_TERM_TRACKS
    })
    .bind(&name)
    .bind(viewer)
    .bind(bound)
    .bind(tag)
    .bind(cursor.map(|cursor| cursor.key))
    .fetch_all(&state.pool)
    .await
    .map_err(DbError::from)?;
    let domain = &state.config.domain;
    let mut items = albums
        .into_iter()
        .map(|album| album_item(domain, album))
        .chain(tracks.into_iter().map(|track| asset_item(domain, track)))
        .collect::<Vec<_>>();
    if tag {
        items.extend(term_tag_items(state, &name, viewer, bound, cursor, browse).await?);
    }
    if browse {
        if let Some(cursor) = cursor {
            items
                .retain(|item| (item.browse_key, item.browse_family) < (cursor.key, cursor.family));
        }
        items.sort_by_key(|item| std::cmp::Reverse((item.browse_key, item.browse_family)));
    } else {
        items.sort_by(|a, b| b.published.cmp(&a.published).then_with(|| b.id.cmp(&a.id)));
    }
    items.truncate(limit);
    let encoded = term_segment(&name);
    let link = format!("https://{domain}/music/{}/{encoded}", kind.path());
    let updated = items
        .iter()
        .map(|item| item.updated)
        .max()
        .unwrap_or(OffsetDateTime::UNIX_EPOCH);
    Ok(Feed {
        id: link.clone(),
        title: name,
        link,
        description: String::new(),
        updated,
        items,
    })
}
async fn term_tag_items(
    state: &AppState,
    name: &str,
    viewer: Option<i64>,
    bound: i64,
    cursor: Option<TermCursor>,
    browse: bool,
) -> Result<Vec<Item>, ApiError> {
    let domain = &state.config.domain;
    let statuses = plamenu_db::tag::timeline(
        &state.pool,
        name,
        viewer,
        if browse {
            plamenu_db::user::TimelineOrder::Received
        } else {
            plamenu_db::user::TimelineOrder::Published
        },
        cursor.map(|cursor| cursor.key.saturating_add(1)),
        bound,
    )
    .await?;
    let mut items = status_items(state, &statuses, viewer).await?;
    let collections: Vec<DatedCollection> = sqlx::query_as(if browse {
        TERM_COLLECTIONS
    } else {
        FEED_TERM_COLLECTIONS
    })
    .bind(name)
    .bind(viewer)
    .bind(bound)
    .bind(cursor.map(|cursor| cursor.key))
    .fetch_all(&state.pool)
    .await
    .map_err(DbError::from)?;
    let artists: Vec<crate::music_entities::Entity> = sqlx::query_as(if browse { concat!(crate::music_entities::artist_batch_query!("WHERE $1=ANY(r.tags)"," WHERE $1::text IS NOT NULL ","WHERE $1::text IS NOT NULL"," WHERE $1::text IS NOT NULL ")," AND NOT EXISTS(SELECT 1 FROM mutes m WHERE m.account_id=$2 AND m.target_account_id=e.owner_id AND (m.expires_at IS NULL OR m.expires_at>now())) AND ($4::bigint IS NULL OR e.id <= $4) ORDER BY e.id DESC LIMIT $3") } else { concat!(crate::music_entities::artist_batch_query!("WHERE $1=ANY(r.tags)"," WHERE $1::text IS NOT NULL ","WHERE $1::text IS NOT NULL"," WHERE $1::text IS NOT NULL ")," AND NOT EXISTS(SELECT 1 FROM mutes m WHERE m.account_id=$2 AND m.target_account_id=e.owner_id AND (m.expires_at IS NULL OR m.expires_at>now())) AND $4::bigint IS NULL ORDER BY e.created_at DESC,e.id DESC LIMIT $3") }).bind(name).bind(viewer).bind(bound).bind(cursor.map(|cursor| cursor.key)).fetch_all(&state.pool).await.map_err(DbError::from)?;
    for artist in artists {
        let link = format!("https://{domain}/music/artists/{}", artist.id);
        items.push(Item {
            id: link.clone(),
            browse_key: artist.id,
            browse_family: 3,
            title: artist.name,
            link,
            description: plamenu_ap::text::plain_to_html(&artist.description),
            author: artist.artist,
            published: artist.created_at,
            updated: artist.updated_at,
            enclosure: None,
        });
    }
    for row in collections {
        let collection = row.collection;
        let link = format!("https://{domain}/music/collections/{}", collection.id);
        items.push(Item {
            id: link.clone(),
            browse_key: collection.id,
            browse_family: 4,
            title: collection.name,
            link,
            description: plamenu_ap::text::plain_to_html(&collection.description),
            author: String::new(),
            published: row.created_at,
            updated: collection.updated_at,
            enclosure: None,
        });
    }
    Ok(items)
}
fn asset_item(domain: &str, track: DatedAsset) -> Item {
    let asset = track.asset;
    let link = format!("https://{domain}/music/assets/{}", asset.id);
    let description = match asset.description_type.as_str() {
        "text/markdown" => crate::compose::markdown_to_sanitized_html(&asset.description, false),
        "text/plain" => plamenu_ap::text::plain_to_html(&asset.description),
        _ => plamenu_ap::text::sanitize_remote_html(&asset.description),
    };
    Item {
        id: link.clone(),
        browse_key: asset.id,
        browse_family: 1,
        title: format!("{} — {}", asset.artist, asset.title),
        link,
        description,
        author: asset.artist,
        published: track.created_at,
        updated: track.updated_at,
        enclosure: Some((
            format!("https://{domain}/music/assets/{}/stream", asset.id),
            "audio/mpeg".into(),
            asset.stream_bytes,
        )),
    }
}
async fn collection_sources(
    state: &AppState,
    entries: &[music_collections::Entry],
    viewer: Option<i64>,
    capability: Option<(&crate::music_shares::Scope, &str)>,
) -> Result<Vec<music_collections::Playable>, ApiError> {
    let mut sources = music_collections::sources(state, entries, viewer).await?;
    if let Some((scope, token)) = capability {
        for source in &mut sources {
            if let Some(entry) = entries
                .iter()
                .find(|entry| entry.id.to_string() == source.entry_id)
                && let Some(remote) = entry.remote_id
            {
                source.url = format!("/music/share?token={token}#entry-{}", entry.id);
                source.stream_url = format!(
                    "/music/share/remote/{remote}/{}/stream?token={token}",
                    entry.id
                );
                source.image_url = None;
            }
        }

        let ids = entries
            .iter()
            .filter_map(|entry| entry.asset_id)
            .collect::<Vec<_>>();
        let assets = crate::music_shares::readable_assets(state, scope, &ids, viewer).await?;
        sources.retain(|source| {
            !entries
                .iter()
                .any(|entry| entry.asset_id.is_some() && entry.id.to_string() == source.entry_id)
        });
        for entry in entries {
            if let Some(asset) = entry.asset_id.and_then(|id| assets.get(&id)) {
                sources.push(music_collections::Playable {
                    entry_id: entry.id.to_string(),
                    title: asset.title.clone(),
                    artist: asset.artist.clone(),
                    url: format!("/music/share?token={token}#entry-{}", entry.id),
                    stream_url: format!("/music/share/assets/{}/stream?token={token}", asset.id),
                    duration: asset.duration,
                    image_url: None,
                });
            }
        }
    }
    Ok(sources)
}
// Inspect newest references in bounded batches, stopping after enough readable
// sources. Hidden entries never consume the feed limit.
async fn latest_entries(
    state: &AppState,
    collection: i64,
    viewer: Option<i64>,
    limit: usize,
    capability: Option<(&crate::music_shares::Scope, &str)>,
) -> Result<
    (
        Vec<music_collections::Entry>,
        Vec<music_collections::Playable>,
    ),
    ApiError,
> {
    let mut before: Option<i64> = None;
    let mut selected = Vec::new();
    let mut playable = Vec::new();
    loop {
        let batch: Vec<music_collections::Entry> = sqlx::query_as(
            "SELECT * FROM music_collection_entries WHERE collection_id=$1 AND ($2::bigint IS NULL OR id<$2) ORDER BY id DESC LIMIT 100",
        ).bind(collection).bind(before).fetch_all(&state.pool).await.map_err(DbError::from)?;
        if batch.is_empty() {
            break;
        }
        before = batch.last().map(|entry| entry.id);
        let sources = collection_sources(state, &batch, viewer, capability).await?;
        let by_id = sources
            .into_iter()
            .map(|source| (source.entry_id.clone(), source))
            .collect::<std::collections::HashMap<_, _>>();
        for entry in batch {
            if let Some(source) = by_id.get(&entry.id.to_string()) {
                selected.push(entry);
                playable.push(source.clone());
                if selected.len() == limit {
                    return Ok((selected, playable));
                }
            }
        }
    }
    Ok((selected, playable))
}
async fn remote_entry_metadata(
    state: &AppState,
    entries: &[music_collections::Entry],
    viewer: Option<i64>,
    metadata: &mut [EntryMetadata],
    notes: &mut std::collections::HashMap<i64, String>,
) -> Result<(), ApiError> {
    let ids = entries
        .iter()
        .filter_map(|entry| entry.remote_id)
        .collect::<Vec<_>>();
    let current =
        crate::music_remote::feed_metadata_pool(&state.pool, &state.config.domain, &ids, viewer)
            .await?;
    for entry in entries {
        if let Some(remote) = entry.remote_id
            && let Some((caption, updated, mime)) = current.get(&remote)
        {
            notes.insert(entry.id, caption.clone());
            if let Some(row) = metadata.iter_mut().find(|row| row.id == entry.id) {
                row.updated = row.updated.max(*updated);
                row.content_type.clone_from(mime);
            }
        }
    }
    Ok(())
}

async fn captions(
    state: &AppState,
    selected: &[music_collections::Entry],
    viewer: Option<i64>,
) -> Result<std::collections::HashMap<i64, String>, ApiError> {
    // Reuse canonical authorization when loading captions; a raw metadata join
    // must never read new private notes after an earlier playback check.
    let asset_ids = selected
        .iter()
        .filter_map(|entry| entry.asset_id)
        .collect::<Vec<_>>();
    let assets = crate::music_assets::for_ids(state, &asset_ids, viewer).await?;
    let asset_notes = assets
        .into_iter()
        .map(|asset| {
            (
                asset.id,
                match asset.description_type.as_str() {
                    "text/markdown" => {
                        crate::compose::markdown_to_sanitized_html(&asset.description, false)
                    }
                    "text/plain" => plamenu_ap::text::plain_to_html(&asset.description),
                    _ => plamenu_ap::text::sanitize_remote_html(&asset.description),
                },
            )
        })
        .collect::<std::collections::HashMap<_, _>>();
    let status_ids = selected
        .iter()
        .filter_map(|entry| entry.status_id)
        .collect::<Vec<_>>();
    let statuses = plamenu_db::status::find_by_ids(&state.pool, &status_ids).await?;
    let statuses = crate::entities::filter_viewable(&state.pool, &statuses, viewer).await?;
    let deleted = plamenu_db::status::deleted_ids(&state.pool, &status_ids).await?;
    let status_notes = statuses
        .into_iter()
        .filter(|status| !deleted.contains(&status.id))
        .map(|status| {
            (
                status.id,
                plamenu_ap::text::sanitize_remote_html(&status.content),
            )
        })
        .collect::<std::collections::HashMap<_, _>>();
    let episode_ids = selected
        .iter()
        .filter_map(|entry| entry.episode_id)
        .collect::<Vec<_>>();
    let episode_rows: Vec<(i64, String)> =
        sqlx::query_as("SELECT id,description FROM music_podcast_episodes WHERE id=ANY($1)")
            .bind(&episode_ids)
            .fetch_all(&state.pool)
            .await
            .map_err(DbError::from)?;
    let episode_notes = episode_rows
        .into_iter()
        .map(|(id, description)| {
            (
                id,
                summary(&plamenu_ap::text::sanitize_remote_html(&description)),
            )
        })
        .collect::<std::collections::HashMap<_, _>>();
    let notes = selected
        .iter()
        .filter_map(|entry| {
            let note = if let Some(asset) = entry.asset_id {
                asset_notes.get(&asset)?.clone()
            } else if let Some(status) = entry.status_id {
                status_notes.get(&status)?.clone()
            } else {
                episode_notes.get(&entry.episode_id?)?.clone()
            };
            Some((entry.id, note))
        })
        .collect::<std::collections::HashMap<_, _>>();
    Ok(notes)
}
fn escape(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => result.push_str("&amp;"),
            '<' => result.push_str("&lt;"),
            '>' => result.push_str("&gt;"),
            '"' => result.push_str("&quot;"),
            '\'' => result.push_str("&apos;"),
            c if matches!(c, '\t' | '\n' | '\r')
                || (c >= ' ' && c != '\u{fffe}' && c != '\u{ffff}') =>
            {
                result.push(c);
            }
            _ => {}
        }
    }
    result
}
// Keep a 100-item feed readable by the same bounded native feed importer.
// Oversized HTML becomes a plain-text excerpt, so truncation cannot split tags.
fn summary(value: &str) -> String {
    if value.len() <= 2048 {
        return value.to_owned();
    }
    let plain = crate::filters::plain_text(value);
    let mut end = plain.len().min(512);
    while !plain.is_char_boundary(end) {
        end -= 1;
    }
    plamenu_ap::text::plain_to_html(&format!("{}…", &plain[..end]))
}
fn element(xml: &mut String, tag: &str, value: &str) {
    let _ = write!(xml, "<{tag}>{}</{tag}>", escape(value));
}
fn date(date: OffsetDateTime, format: Format) -> String {
    match format {
        Format::Rss => date.format(&Rfc2822),
        Format::Atom => date.format(&Rfc3339),
    }
    .unwrap_or_default()
}
impl Feed {
    #[must_use]
    pub fn render(&self, format: Format, feed_url: &str) -> String {
        let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>");
        match format {
            Format::Rss => {
                xml.push_str("<rss version=\"2.0\" xmlns:atom=\"http://www.w3.org/2005/Atom\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><channel>");
                element(&mut xml, "title", &self.title);
                element(&mut xml, "link", &self.link);
                element(&mut xml, "description", &summary(&self.description));
                element(&mut xml, "lastBuildDate", &date(self.updated, format));
                let _ = write!(
                    xml,
                    "<atom:link rel=\"self\" type=\"application/rss+xml\" href=\"{}\"/>",
                    escape(feed_url)
                );
                for item in &self.items {
                    xml.push_str("<item>");
                    element(&mut xml, "title", &item.title);
                    element(&mut xml, "link", &item.link);
                    let _ = write!(
                        xml,
                        "<guid isPermaLink=\"false\">{}</guid>",
                        escape(&item.id)
                    );
                    element(&mut xml, "pubDate", &date(item.published, format));
                    element(&mut xml, "description", &summary(&item.description));
                    element(&mut xml, "dc:creator", &item.author);
                    if let Some((url, kind, length)) = &item.enclosure {
                        let _ = write!(
                            xml,
                            "<enclosure url=\"{}\" type=\"{}\" length=\"{}\"/>",
                            escape(url),
                            escape(kind),
                            length.max(&0)
                        );
                    }
                    xml.push_str("</item>");
                }
                xml.push_str("</channel></rss>");
            }
            Format::Atom => {
                xml.push_str("<feed xmlns=\"http://www.w3.org/2005/Atom\">");
                element(&mut xml, "id", &self.id);
                element(&mut xml, "title", &self.title);
                element(&mut xml, "subtitle", &summary(&self.description));
                element(&mut xml, "updated", &date(self.updated, format));
                let _ = write!(
                    xml,
                    "<link rel=\"self\" type=\"application/atom+xml\" href=\"{}\"/><link rel=\"alternate\" href=\"{}\"/>",
                    escape(feed_url),
                    escape(&self.link)
                );
                for item in &self.items {
                    xml.push_str("<entry>");
                    element(&mut xml, "id", &item.id);
                    element(&mut xml, "title", &item.title);
                    let _ = write!(
                        xml,
                        "<link rel=\"alternate\" href=\"{}\"/>",
                        escape(&item.link)
                    );
                    element(&mut xml, "published", &date(item.published, format));
                    element(&mut xml, "updated", &date(item.updated, format));
                    xml.push_str("<author>");
                    element(&mut xml, "name", &item.author);
                    xml.push_str("</author>");
                    let _ = write!(
                        xml,
                        "<summary type=\"html\">{}</summary>",
                        escape(&summary(&item.description))
                    );
                    if let Some((url, kind, length)) = &item.enclosure {
                        let _ = write!(
                            xml,
                            "<link rel=\"enclosure\" href=\"{}\" type=\"{}\" length=\"{}\"/>",
                            escape(url),
                            escape(kind),
                            length.max(&0)
                        );
                    }
                    xml.push_str("</entry>");
                }
                xml.push_str("</feed>");
            }
        }
        xml
    }
}
