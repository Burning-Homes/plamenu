//! Exact, revocable music capabilities and private owner sharing management.
use crate::{
    AppState,
    auth::{generate_secret, hash_secret},
    error::ApiError,
    music_artworks, music_assets, music_collections, music_entities, music_radios,
    music_saved::{self, Kind, Reference},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use plamenu_db::{DbError, id};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

#[derive(Deserialize)]
pub struct New {
    pub reference: Reference,
    pub expires_at: Option<String>,
}
#[derive(Serialize)]
pub struct Created {
    pub id: String,
    pub url: String,
    pub token: String,
    pub reference: Reference,
    pub expires_at: Option<String>,
}
#[derive(Clone, sqlx::FromRow)]
pub struct Scope {
    pub id: i64,
    pub owner_id: i64,
    pub kind: String,
    pub target_id: i64,
    pub created_at: OffsetDateTime,
    pub expires_at: Option<OffsetDateTime>,
    pub revoked_at: Option<OffsetDateTime>,
}
impl Scope {
    pub fn reference(&self) -> Result<Reference, ApiError> {
        Ok(Reference {
            kind: Kind::parse(&self.kind)?,
            id: self.target_id.to_string(),
        })
    }
}
async fn normalize(
    state: &AppState,
    owner: i64,
    reference: &Reference,
) -> Result<Reference, ApiError> {
    if reference.kind != Kind::Audio {
        return Ok(reference.clone());
    }
    let (_, status) = reference.key()?;
    let asset:Option<i64>=sqlx::query_scalar("SELECT t.id FROM music_publications p JOIN music_assets t ON t.id=p.asset_id JOIN statuses s ON s.id=p.status_id JOIN accounts a ON a.id=s.account_id WHERE p.status_id=$1 AND t.owner_id=$2 AND s.account_id=t.owner_id AND a.domain IS NULL AND s.object_type='Audio' AND s.reblog_of_id IS NULL AND s.deleted_at IS NULL\n-- STUBFILTER: only current origin-owned retained Audio may create a share capability")
        .bind(status).bind(owner).fetch_optional(&state.pool).await.map_err(DbError::from)?;
    Ok(Reference {
        kind: Kind::Track,
        id: asset.ok_or(ApiError::NotFound)?.to_string(),
    })
}
pub async fn management_source(
    state: &AppState,
    owner: i64,
    reference: &Reference,
) -> Result<(Reference, Value), ApiError> {
    let reference = normalize(state, owner, reference).await?;
    let source = owned_source(state, owner, &reference).await?;
    Ok((reference, source))
}
async fn owned_source(
    state: &AppState,
    owner: i64,
    reference: &Reference,
) -> Result<Value, ApiError> {
    let key = reference.key()?;
    let source = music_saved::sources(state, std::slice::from_ref(reference), owner)
        .await?
        .remove(&key)
        .ok_or(ApiError::NotFound)?;
    if source["owner_id"].as_str() != Some(owner.to_string().as_str()) {
        return Err(ApiError::NotFound);
    }
    Ok(source)
}
pub async fn create(state: &AppState, owner: i64, input: &New) -> Result<Created, ApiError> {
    let expires = input
        .expires_at
        .as_deref()
        .map(|v| {
            OffsetDateTime::parse(v, &Rfc3339)
                .map_err(|_| ApiError::Unprocessable("Invalid share expiration".into()))
        })
        .transpose()?;
    if expires.is_some_and(|v| v <= OffsetDateTime::now_utc()) {
        return Err(ApiError::Unprocessable("Choose a future expiration".into()));
    }
    let reference = normalize(state, owner, &input.reference).await?;
    let (kind, target) = reference.key()?;
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, owner).await?;
    crate::music_queues::active_owner(&mut tx, owner).await?;
    owned_source(state, owner, &reference).await?;
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM music_share_links WHERE owner_id=$1 AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at>now())").bind(owner).fetch_one(&mut *tx).await.map_err(DbError::from)?;
    if count >= 1000 {
        return Err(ApiError::Unprocessable(
            "Active music share link limit reached".into(),
        ));
    }
    let raw = generate_secret();
    let link = id::next();
    sqlx::query("INSERT INTO music_share_links(id,owner_id,kind,asset_id,artist_id,album_id,collection_id,radio_id,token_hash,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)")
        .bind(link).bind(owner).bind(kind.text()).bind((kind==Kind::Track).then_some(target)).bind((kind==Kind::Artist).then_some(target)).bind((kind==Kind::Album).then_some(target)).bind(matches!(kind,Kind::Library|Kind::Playlist).then_some(target)).bind((kind==Kind::Radio).then_some(target)).bind(hash_secret(&raw)).bind(expires).execute(&mut *tx).await.map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(Created {
        id: link.to_string(),
        url: format!("https://{}/music/share?token={raw}", state.config.domain),
        token: raw,
        reference,
        expires_at: expires.map(crate::entities::rfc3339).transpose()?,
    })
}
pub async fn revoke(state: &AppState, owner: i64, link: i64) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    music_assets::lock_owner(&mut tx, owner).await?;
    let n=sqlx::query("UPDATE music_share_links SET revoked_at=COALESCE(revoked_at,now()) WHERE id=$1 AND owner_id=$2").bind(link).bind(owner).execute(&mut *tx).await.map_err(DbError::from)?.rows_affected();
    if n == 0 {
        return Err(ApiError::NotFound);
    }
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
/// Resolve every request afresh; a token never becomes an account/session.
pub async fn resolve(state: &AppState, raw: &str, viewer: Option<i64>) -> Result<Scope, ApiError> {
    if raw.len() != 43
        || !raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(ApiError::NotFound);
    }
    let scope:Scope=sqlx::query_as("SELECT l.* FROM music_share_links l JOIN accounts a ON a.id=l.owner_id WHERE l.token_hash=$1 AND l.revoked_at IS NULL AND (l.expires_at IS NULL OR l.expires_at>now()) AND a.domain IS NULL AND a.deleted_at IS NULL AND a.suspended_at IS NULL AND NOT EXISTS(SELECT 1 FROM users u WHERE u.account_id=a.id AND (NOT u.approved OR u.confirmed_at IS NULL)) AND NOT EXISTS(SELECT 1 FROM blocks b WHERE (b.account_id=$2 AND b.target_account_id=a.id) OR (b.account_id=a.id AND b.target_account_id=$2))")
        .bind(hash_secret(raw)).bind(viewer).fetch_optional(&state.pool).await.map_err(DbError::from)?.ok_or(ApiError::NotFound)?;
    owned_source(state, scope.owner_id, &scope.reference()?).await?;
    Ok(scope)
}
/// An admitted job stores only its link identity, never the raw capability.
/// This helper is internal; public requests must still prove the raw token.
pub(crate) async fn resolve_job(
    state: &AppState,
    link: i64,
    viewer: Option<i64>,
) -> Result<Scope, ApiError> {
    let scope:Scope=sqlx::query_as("SELECT l.* FROM music_share_links l JOIN accounts a ON a.id=l.owner_id WHERE l.id=$1 AND l.revoked_at IS NULL AND (l.expires_at IS NULL OR l.expires_at>now()) AND a.domain IS NULL AND a.deleted_at IS NULL AND a.suspended_at IS NULL AND NOT EXISTS(SELECT 1 FROM users u WHERE u.account_id=a.id AND (NOT u.approved OR u.confirmed_at IS NULL)) AND NOT EXISTS(SELECT 1 FROM blocks b WHERE (b.account_id=$2 AND b.target_account_id=a.id) OR (b.account_id=a.id AND b.target_account_id=$2))")
        .bind(link).bind(viewer).fetch_optional(&state.pool).await.map_err(DbError::from)?.ok_or(ApiError::NotFound)?;
    owned_source(state, scope.owner_id, &scope.reference()?).await?;
    Ok(scope)
}
/// Root metadata is explicitly shared; private related parent notes are omitted.
pub async fn source(state: &AppState, scope: &Scope) -> Result<Value, ApiError> {
    let mut source = owned_source(state, scope.owner_id, &scope.reference()?).await?;
    for key in [
        "owner_id",
        "config",
        "visibility",
        "revision",
        "stream_url",
        "download_url",
        "image",
        "image_url",
    ] {
        source
            .as_object_mut()
            .expect("music entities are objects")
            .remove(key);
    }
    source["images"] = json!(
        artworks(state, scope)
            .await?
            .iter()
            .map(|i| json!({"id":i.id.to_string(),"slot":i.slot}))
            .collect::<Vec<_>>()
    );
    Ok(source)
}
async fn own_child(state: &AppState, scope: &Scope, asset: i64) -> Result<bool, ApiError> {
    let allowed:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM music_assets t WHERE t.id=$1 AND t.owner_id=$2 AND (($3='track' AND t.id=$4) OR ($3='album' AND EXISTS(SELECT 1 FROM music_albums a WHERE a.id=$4 AND a.owner_id=$2 AND a.share_tracks AND t.album_id=a.id)) OR ($3 IN ('library','playlist') AND EXISTS(SELECT 1 FROM music_collections c JOIN music_collection_entries e ON e.collection_id=c.id WHERE c.id=$4 AND c.kind=$3 AND c.owner_id=$2 AND c.share_tracks AND e.asset_id=t.id))))")
        .bind(asset).bind(scope.owner_id).bind(&scope.kind).bind(scope.target_id).fetch_one(&state.pool).await.map_err(DbError::from)?;
    Ok(allowed)
}
/// Only this exact token's owned subset gains capability access. Ordinary access
/// remains available for public/independently granted entries of the target.
pub async fn asset(
    state: &AppState,
    scope: &Scope,
    asset: i64,
    viewer: Option<i64>,
) -> Result<music_assets::Asset, ApiError> {
    let member:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM music_assets t WHERE t.id=$1 AND (($2='track' AND t.id=$3) OR ($2='album' AND t.album_id=$3) OR ($2='artist' AND (t.artist_id=$3 OR EXISTS(SELECT 1 FROM music_albums a WHERE a.id=t.album_id AND a.artist_id=$3))) OR ($2 IN ('library','playlist') AND EXISTS(SELECT 1 FROM music_collection_entries e WHERE e.collection_id=$3 AND e.asset_id=t.id))))")
        .bind(asset).bind(&scope.kind).bind(scope.target_id).fetch_one(&state.pool).await.map_err(DbError::from)?;
    if !member {
        return Err(ApiError::NotFound);
    }
    if own_child(state, scope, asset).await? {
        music_assets::get(state, asset, Some(scope.owner_id)).await
    } else {
        music_assets::get(state, asset, viewer).await
    }
}
#[derive(Serialize)]
pub struct Tracks {
    pub items: Vec<Value>,
    pub before: Option<String>,
}
pub async fn tracks(
    state: &AppState,
    scope: &Scope,
    viewer: Option<i64>,
    before: Option<i64>,
) -> Result<Tracks, ApiError> {
    if before.is_some_and(|v| v <= 0) {
        return Err(ApiError::Unprocessable("Invalid track cursor".into()));
    }
    if matches!(scope.kind.as_str(), "library" | "playlist") {
        return collection_tracks(state, scope, viewer, before).await;
    }
    let mut cursor = before;
    let mut items = Vec::new();
    loop {
        let ids:Vec<i64>=sqlx::query_scalar("SELECT t.id FROM music_assets t WHERE ($1::bigint IS NULL OR t.id<$1) AND (($2='track' AND t.id=$3) OR ($2='album' AND t.album_id=$3) OR ($2='artist' AND (t.artist_id=$3 OR EXISTS(SELECT 1 FROM music_albums a WHERE a.id=t.album_id AND a.artist_id=$3))) OR ($2 IN ('library','playlist') AND EXISTS(SELECT 1 FROM music_collection_entries e WHERE e.collection_id=$3 AND e.asset_id=t.id))) ORDER BY t.id DESC LIMIT 100")
            .bind(cursor).bind(&scope.kind).bind(scope.target_id).fetch_all(&state.pool).await.map_err(DbError::from)?;
        // Bounded, shared source batches preserve every existing creator gate.
        let mut readable = readable_assets(state, scope, &ids, viewer).await?;
        for id in &ids {
            if let Some(a) = readable.remove(id) {
                items.push(json!({"id":a.id.to_string(),"title":a.title,"artist":a.artist,"album":a.album,"duration":a.duration,"image_id":a.artwork_id}));
                if items.len() > 50 {
                    items.truncate(50);
                    return Ok(Tracks {
                        before: items
                            .last()
                            .and_then(|v| v["id"].as_str())
                            .map(str::to_owned),
                        items,
                    });
                }
            }
        }
        if ids.len() < 100 {
            return Ok(Tracks {
                items,
                before: None,
            });
        }
        cursor = ids.last().copied();
    }
}
async fn collection_tracks(
    state: &AppState,
    scope: &Scope,
    viewer: Option<i64>,
    mut after: Option<i64>,
) -> Result<Tracks, ApiError> {
    let mut items = Vec::new();
    loop {
        let page = music_collections::entry_page(state, scope.target_id, after).await?;
        let ids = page
            .items
            .iter()
            .filter_map(|e| e.asset_id)
            .collect::<Vec<_>>();
        let readable = readable_assets(state, scope, &ids, viewer).await?;
        let remote = music_collections::sources(state, &page.items, viewer)
            .await?
            .into_iter()
            .map(|p| (p.entry_id.clone(), p))
            .collect::<std::collections::HashMap<_, _>>();
        for entry in &page.items {
            if let Some(a) = entry.asset_id.and_then(|id| readable.get(&id)) {
                items.push(json!({"id":a.id.to_string(),"entry_id":entry.id.to_string(),"title":a.title,"artist":a.artist,"album":a.album,"duration":a.duration,"image_id":a.artwork_id}));
            } else if (entry.status_id.is_some()
                || entry.episode_id.is_some()
                || entry.remote_id.is_some())
                && let Some(p) = remote.get(&entry.id.to_string())
            {
                items.push(json!({"entry_id":entry.id.to_string(),"title":p.title,"artist":p.artist,"duration":p.duration,"stream_url":p.stream_url,"remote_id":entry.remote_id.map(|id|id.to_string())}));
            }
            if items.len() > 50 {
                items.truncate(50);
                return Ok(Tracks {
                    before: items
                        .last()
                        .and_then(|v| v["entry_id"].as_str())
                        .map(str::to_owned),
                    items,
                });
            }
        }
        match page.after {
            Some(next) => after = Some(next),
            None => {
                return Ok(Tracks {
                    items,
                    before: None,
                });
            }
        }
    }
}
pub(crate) async fn readable_assets(
    state: &AppState,
    scope: &Scope,
    ids: &[i64],
    viewer: Option<i64>,
) -> Result<std::collections::HashMap<i64, music_assets::Asset>, ApiError> {
    let mut readable = music_assets::for_ids(state, ids, viewer)
        .await?
        .into_iter()
        .map(|a| (a.id, a))
        .collect::<std::collections::HashMap<_, _>>();
    let own:Vec<i64>=sqlx::query_scalar("SELECT t.id FROM music_assets t WHERE t.id=ANY($1) AND t.owner_id=$2 AND (($3='track' AND t.id=$4) OR ($3='album' AND EXISTS(SELECT 1 FROM music_albums a WHERE a.id=$4 AND a.owner_id=$2 AND a.share_tracks AND t.album_id=a.id)) OR ($3 IN ('library','playlist') AND EXISTS(SELECT 1 FROM music_collections c JOIN music_collection_entries e ON e.collection_id=c.id WHERE c.id=$4 AND c.kind=$3 AND c.owner_id=$2 AND c.share_tracks AND e.asset_id=t.id)))")
            .bind(ids).bind(scope.owner_id).bind(&scope.kind).bind(scope.target_id).fetch_all(&state.pool).await.map_err(DbError::from)?;
    readable.extend(
        music_assets::for_ids(state, &own, Some(scope.owner_id))
            .await?
            .into_iter()
            .map(|a| (a.id, a)),
    );
    Ok(readable)
}
pub async fn artworks(
    state: &AppState,
    scope: &Scope,
) -> Result<Vec<music_artworks::Artwork>, ApiError> {
    let target = match Kind::parse(&scope.kind)? {
        Kind::Track => music_artworks::Target::Assets,
        Kind::Artist => music_artworks::Target::Artists,
        Kind::Album => music_artworks::Target::Albums,
        Kind::Library | Kind::Playlist => music_artworks::Target::Collections,
        Kind::Radio => return Ok(Vec::new()),
        Kind::Audio | Kind::Remote => return Err(ApiError::NotFound),
    };
    music_artworks::for_target(state, target, scope.target_id, Some(scope.owner_id)).await
}
pub async fn artwork(
    state: &AppState,
    scope: &Scope,
    id: i64,
    viewer: Option<i64>,
) -> Result<music_artworks::Artwork, ApiError> {
    let row:music_artworks::Artwork=sqlx::query_as("SELECT a.* FROM music_artworks a WHERE a.id=$1 AND NOT EXISTS(SELECT 1 FROM media_cleanup_jobs c WHERE c.file_name=a.file_name)").bind(id).fetch_optional(&state.pool).await.map_err(DbError::from)?.ok_or(ApiError::NotFound)?;
    if artworks(state, scope).await?.iter().any(|a| a.id == id) {
        return Ok(row);
    }
    let mut cursor = None::<i64>;
    loop {
        let tracks:Vec<i64>=sqlx::query_scalar("SELECT t.id FROM music_assets t LEFT JOIN music_albums album ON album.id=t.album_id WHERE ($5::bigint IS NULL OR t.id<$5) AND (($1='assets' AND t.id=$2) OR ($1='albums' AND t.album_id=$2) OR ($1='artists' AND (t.artist_id=$2 OR album.artist_id=$2))) AND (($3='track' AND t.id=$4) OR ($3='album' AND t.album_id=$4) OR ($3='artist' AND (t.artist_id=$4 OR album.artist_id=$4)) OR ($3 IN ('library','playlist') AND EXISTS(SELECT 1 FROM music_collection_entries e WHERE e.collection_id=$4 AND e.asset_id=t.id))) ORDER BY t.id DESC LIMIT 100")
            .bind(&row.target_kind).bind(row.target_id).bind(&scope.kind).bind(scope.target_id).bind(cursor).fetch_all(&state.pool).await.map_err(DbError::from)?;
        if !readable_assets(state, scope, &tracks, viewer)
            .await?
            .is_empty()
        {
            return Ok(row);
        }
        if tracks.len() < 100 {
            break;
        }
        cursor = tracks.last().copied();
    }
    if scope.kind == "album" && row.target_kind == "artists" {
        let album = music_entities::get(
            state,
            music_entities::Kind::Album,
            scope.target_id,
            Some(scope.owner_id),
        )
        .await?;
        if album.artist_id == Some(row.target_id) && row.owner_id == scope.owner_id {
            return Ok(row);
        }
    }
    Err(ApiError::NotFound)
}
#[derive(Clone, Serialize, Deserialize)]
struct Cursor {
    at: OffsetDateTime,
    category: i32,
    id: i64,
}
#[derive(sqlx::FromRow)]
struct Managed {
    id: i64,
    category: i32,
    kind: String,
    target_id: i64,
    member_id: Option<i64>,
    username: Option<String>,
    created_at: OffsetDateTime,
    expires_at: Option<OffsetDateTime>,
    revoked_at: Option<OffsetDateTime>,
}
#[derive(Serialize)]
pub struct Share {
    pub id: String,
    pub category: String,
    pub reference: Reference,
    pub username: Option<String>,
    pub member_id: Option<String>,
    pub source: Option<Value>,
    pub created_at: String,
    pub expires_at: Option<String>,
    pub revoked: bool,
    pub expired: bool,
}
#[derive(Serialize)]
pub struct Management {
    pub items: Vec<Share>,
    pub before: Option<String>,
}
pub async fn page(
    state: &AppState,
    owner: i64,
    before: Option<&str>,
    include_revoked: bool,
) -> Result<Management, ApiError> {
    let cursor = before
        .map(|v| {
            if v.len() > 2000 {
                return Err(ApiError::Unprocessable("Invalid sharing cursor".into()));
            }
            URL_SAFE_NO_PAD
                .decode(v)
                .ok()
                .filter(|v| v.len() < 1000)
                .and_then(|v| serde_json::from_slice::<Cursor>(&v).ok())
                .filter(|v| (0..=4).contains(&v.category) && v.id > 0)
                .ok_or_else(|| ApiError::Unprocessable("Invalid sharing cursor".into()))
        })
        .transpose()?;
    let mut rows:Vec<Managed>=sqlx::query_as("SELECT q.* FROM (SELECT l.id,0::integer AS category,l.kind,l.target_id,NULL::bigint AS member_id,NULL::text AS username,l.created_at,l.expires_at,l.revoked_at FROM music_share_links l WHERE l.owner_id=$1 AND ($5 OR l.revoked_at IS NULL) UNION ALL SELECT g.id,1,'track',t.id,g.account_id,a.username,g.created_at,NULL,NULL FROM music_asset_grants g JOIN music_assets t ON t.id=g.asset_id JOIN accounts a ON a.id=g.account_id WHERE t.owner_id=$1 UNION ALL SELECT g.id,2,'album',t.id,g.account_id,a.username,g.created_at,NULL,NULL FROM music_album_grants g JOIN music_albums t ON t.id=g.album_id JOIN accounts a ON a.id=g.account_id WHERE t.owner_id=$1 UNION ALL SELECT g.id,3,t.kind,t.id,g.account_id,a.username,g.created_at,NULL,NULL FROM music_collection_grants g JOIN music_collections t ON t.id=g.collection_id JOIN accounts a ON a.id=g.account_id WHERE t.owner_id=$1 UNION ALL SELECT g.id,4,'radio',t.id,g.account_id,a.username,g.created_at,NULL,NULL FROM music_radio_grants g JOIN music_radios t ON t.id=g.radio_id JOIN accounts a ON a.id=g.account_id WHERE t.owner_id=$1) q WHERE ($2::timestamptz IS NULL OR (q.created_at,q.category,q.id)<($2,$3,$4)) ORDER BY q.created_at DESC,q.category DESC,q.id DESC LIMIT 51")
        .bind(owner).bind(cursor.as_ref().map(|c|c.at)).bind(cursor.as_ref().map(|c|c.category)).bind(cursor.as_ref().map(|c|c.id)).bind(include_revoked).fetch_all(&state.pool).await.map_err(DbError::from)?;
    let before = if rows.len() > 50 {
        rows.truncate(50);
        rows.last()
            .map(|r| {
                serde_json::to_vec(&Cursor {
                    at: r.created_at,
                    category: r.category,
                    id: r.id,
                })
                .map(|v| URL_SAFE_NO_PAD.encode(v))
            })
            .transpose()
            .map_err(|e| ApiError::Internal(Box::new(e)))?
    } else {
        None
    };
    let references = rows
        .iter()
        .map(|r| {
            Ok(Reference {
                kind: Kind::parse(&r.kind)?,
                id: r.target_id.to_string(),
            })
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    let sources = music_saved::sources(state, &references, owner).await?;
    let mut items = Vec::new();
    for (row, reference) in rows.into_iter().zip(references) {
        let source = sources.get(&reference.key()?).cloned();
        items.push(Share {
            id: row.id.to_string(),
            category: if row.category == 0 { "url" } else { "grant" }.into(),
            reference,
            username: row.username,
            member_id: row.member_id.map(|v| v.to_string()),
            source,
            created_at: crate::entities::rfc3339(row.created_at)?,
            expires_at: row.expires_at.map(crate::entities::rfc3339).transpose()?,
            revoked: row.revoked_at.is_some(),
            expired: row
                .expires_at
                .is_some_and(|v| v <= OffsetDateTime::now_utc()),
        });
    }
    Ok(Management { items, before })
}
pub async fn revoke_grant(
    state: &AppState,
    owner: i64,
    reference: &Reference,
    member: i64,
) -> Result<(), ApiError> {
    let (kind, target) = reference.key()?;
    owned_source(state, owner, reference).await?;
    match kind {
        Kind::Track => music_assets::grant(state, owner, target, member, false).await,
        Kind::Album => crate::music_album_access::grant(state, owner, target, member, false).await,
        Kind::Library | Kind::Playlist => {
            let row = music_collections::get(state, target, Some(owner)).await?;
            music_collections::grant(state, owner, target, row.revision, member, false).await
        }
        Kind::Radio => music_radios::grant(state, owner, target, member, false).await,
        _ => Err(ApiError::NotFound),
    }
}

/// Called only for a newly inserted grant, on its owner's transaction. Database
/// notification triggers publish streaming and push work only after commit.
pub(crate) async fn notify_grant(
    state: &AppState,
    conn: &mut sqlx::PgConnection,
    owner: i64,
    member: i64,
    kind: Kind,
    grant: i64,
) -> Result<(), ApiError> {
    use plamenu_db::notification_policy::{self, Disposition};
    if owner == member
        || plamenu_db::streaming::notification_filtered(&state.pool, member, owner).await?
    {
        return Ok(());
    }
    let disposition =
        notification_policy::evaluate(&state.pool, member, owner, "music.share", None).await?;
    if disposition == Disposition::Drop {
        return Ok(());
    }
    let notification = id::next();
    sqlx::query("INSERT INTO notifications(id,account_id,from_account_id,kind,filtered,reasons) VALUES($1,$2,$3,'music.share',$4,ARRAY['music.share'])").bind(notification).bind(member).bind(owner).bind(disposition==Disposition::Filter).execute(&mut *conn).await.map_err(DbError::from)?;
    let query = match kind {
        Kind::Track => {
            "UPDATE music_asset_grants SET notification_id=$1 WHERE id=$2 AND account_id=$3"
        }
        Kind::Album => {
            "UPDATE music_album_grants SET notification_id=$1 WHERE id=$2 AND account_id=$3"
        }
        Kind::Library | Kind::Playlist => {
            "UPDATE music_collection_grants SET notification_id=$1 WHERE id=$2 AND account_id=$3"
        }
        Kind::Radio => {
            "UPDATE music_radio_grants SET notification_id=$1 WHERE id=$2 AND account_id=$3"
        }
        _ => return Err(ApiError::NotFound),
    };
    sqlx::query(query)
        .bind(notification)
        .bind(grant)
        .bind(member)
        .execute(conn)
        .await
        .map_err(DbError::from)?;
    Ok(())
}
/// Notification rendering shares ordinary source gates and bounded batches.
/// No title, private configuration, token or grant capability is snapshotted.
pub(crate) async fn notification_sources(
    pool: &sqlx::PgPool,
    ids: &[i64],
    viewer: i64,
) -> Result<std::collections::HashMap<i64, Value>, ApiError> {
    let mut result = std::collections::HashMap::new();
    for chunk in ids.chunks(100) {
        let refs:Vec<(i64,String,i64)>=sqlx::query_as("SELECT q.notification_id,q.kind,q.target_id FROM (SELECT notification_id,'track'::text AS kind,asset_id AS target_id,account_id FROM music_asset_grants UNION ALL SELECT notification_id,'album',album_id,account_id FROM music_album_grants UNION ALL SELECT g.notification_id,c.kind,c.id,g.account_id FROM music_collection_grants g JOIN music_collections c ON c.id=g.collection_id UNION ALL SELECT notification_id,'radio',radio_id,account_id FROM music_radio_grants) q JOIN notifications n ON n.id=q.notification_id WHERE q.notification_id=ANY($1) AND q.account_id=$2 AND n.account_id=$2 AND n.kind='music.share'").bind(chunk).bind(viewer).fetch_all(pool).await.map_err(DbError::from)?;
        let targets = |kind: &str| {
            refs.iter()
                .filter_map(|(_, k, target)| (k == kind).then_some(*target))
                .collect::<Vec<_>>()
        };
        let mut sources = std::collections::HashMap::new();
        for row in music_assets::for_ids_pool(pool, &targets("track"), Some(viewer)).await? {
            let id = row.id;
            sources.insert(("track".to_owned(), id), row.entity());
        }
        for row in music_entities::for_ids_pool(
            pool,
            music_entities::Kind::Album,
            &targets("album"),
            Some(viewer),
        )
        .await?
        {
            let id = row.id;
            sources.insert(
                ("album".to_owned(), id),
                row.entity(music_entities::Kind::Album),
            );
        }
        let mut collections = targets("library");
        collections.extend(targets("playlist"));
        for row in music_collections::for_ids_pool(pool, &collections, Some(viewer)).await? {
            let key = (row.kind.clone(), row.id);
            sources.insert(key, row.entity());
        }
        for row in music_radios::for_ids_pool(pool, &targets("radio"), Some(viewer)).await? {
            let id = row.id;
            sources.insert(("radio".to_owned(), id), row.entity(Some(viewer)));
        }
        for (notification, kind, id) in refs {
            if let Some(mut source) = sources.get(&(kind.clone(), id)).cloned() {
                source["kind"] = json!(kind);
                if kind == "track" {
                    source["url"] = json!(format!("/music/assets/{id}"));
                }
                result.insert(notification, source);
            }
        }
    }
    Ok(result)
}
