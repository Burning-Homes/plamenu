//! Resolve website URLs through current native public resource authorization.
use crate::{AppState, entities, error::ApiError};
use plamenu_db::{DbError, account, status};
use url::Url;

pub async fn owner(state: &AppState, target: &str) -> Result<i64, ApiError> {
    let url = Url::parse(target).map_err(|_| ApiError::NotFound)?;
    if url.origin()
        != Url::parse(&format!("https://{}", state.config.domain))
            .map_err(|_| ApiError::NotFound)?
            .origin()
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(ApiError::NotFound);
    }
    let path = url.path();
    if let Some(id) = path
        .strip_prefix("/webmentions/posts/")
        .and_then(|s| s.parse().ok())
    {
        return Ok(super::outgoing::public_status(state, id).await?.1.id);
    }
    let result = if let Some(id) = asset_id(path) {
        crate::music_assets::get(state, id, None).await?.owner_id
    } else if let Some(id) = path
        .strip_prefix("/music/collections/")
        .and_then(|s| s.parse().ok())
    {
        crate::music_collections::get(state, id, None)
            .await?
            .owner_id
    } else if let Some(id) = path
        .strip_prefix("/music/radios/")
        .and_then(|s| s.parse().ok())
    {
        crate::music_radios::get(state, id, None).await?.owner_id
    } else if let Some((kind, id)) = crate::music_entities::local_id(&state.config.domain, target) {
        crate::music_entities::get(state, kind, id, None)
            .await?
            .owner_id
    } else if let Some(id) = path
        .strip_prefix("/music/artists/")
        .and_then(|s| s.parse().ok())
    {
        crate::music_entities::get(state, crate::music_entities::Kind::Artist, id, None)
            .await?
            .owner_id
    } else if let Some(id) = path
        .strip_prefix("/music/albums/")
        .and_then(|s| s.parse().ok())
    {
        crate::music_entities::get(state, crate::music_entities::Kind::Album, id, None)
            .await?
            .owner_id
    } else if let Some((owner, _)) = crate::music_library_federation::owner(state, target).await? {
        owner.id
    } else if let Some(id) = path
        .strip_prefix("/music/artwork/")
        .and_then(|s| s.parse().ok())
    {
        crate::music_artworks::get(state, id, None).await?.owner_id
    } else if let Some(owner) = crate::music_follows::public_resource_owner(state, target).await? {
        owner
    } else if let Some(file) = path.strip_prefix("/media/").filter(|s| !s.contains('/')) {
        media_owner(state, file).await?
    } else {
        profile_or_status(state, target, &url).await?
    };
    let account = account::find_by_id(&state.pool, result)
        .await?
        .ok_or(ApiError::NotFound)?;
    let active:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts a WHERE a.id=$1 AND a.deleted_at IS NULL AND a.suspended_at IS NULL AND NOT EXISTS(SELECT 1 FROM users u WHERE u.account_id=a.id AND (NOT u.approved OR u.confirmed_at IS NULL)))").bind(result).fetch_one(&state.pool).await.map_err(DbError::from)?;
    if !active
        || !account.is_local()
        || !crate::instance_policy::public_account_visible(
            &state.pool,
            &state.config.domain,
            &account,
        )
        .await?
    {
        return Err(ApiError::NotFound);
    }
    Ok(result)
}
async fn profile_or_status(state: &AppState, target: &str, url: &Url) -> Result<i64, ApiError> {
    if let Some(owner) =
        crate::local_identity::find_actor(&state.pool, &state.config.domain, target).await?
    {
        return Ok(owner.id);
    }
    if let Some(username) = url.path().strip_prefix("/@").filter(|s| !s.contains('/')) {
        let owner = account::find_public_local_by_username(&state.pool, username)
            .await?
            .ok_or(ApiError::NotFound)?;
        if target == entities::account_web_url(&state.config.domain, &owner) {
            return Ok(owner.id);
        }
    }
    let id = url
        .path()
        .rsplit('/')
        .next()
        .and_then(|s| s.parse::<i64>().ok())
        .ok_or(ApiError::NotFound)?;
    let item = status::find_by_id(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let author = account::find_by_id(&state.pool, item.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if item.uri.is_some()
        || item.reblog_of_id.is_some()
        || !matches!(item.visibility.as_str(), "public" | "unlisted")
        || status::is_deleted(&state.pool, id).await?
        || !author.is_local()
    {
        return Err(ApiError::NotFound);
    }
    if target != entities::status_uri_for_account(&state.config.domain, &item, &author)
        && target != entities::status_web_url(&state.config.domain, &item, &author.username)
    {
        return Err(ApiError::NotFound);
    }
    Ok(author.id)
}

pub(super) fn asset_id(path: &str) -> Option<i64> {
    let tail = path.strip_prefix("/music/assets/")?;
    let (number, suffix) = tail.split_once('/').unwrap_or((tail, ""));
    let id = number.parse::<i64>().ok()?;
    (id > 0 && number == id.to_string() && matches!(suffix, "" | "stream" | "download"))
        .then_some(id)
}
async fn media_owner(state: &AppState, file: &str) -> Result<i64, ApiError> {
    if !plamenu_db::media::public_file_allowed(&state.pool, file).await? {
        return Err(ApiError::NotFound);
    }
    let owners:Vec<i64>=sqlx::query_scalar("SELECT DISTINCT s.account_id FROM media_attachments m JOIN statuses s ON s.id=m.status_id WHERE (m.file_name=$1 OR m.small_file_name=$1) AND s.uri IS NULL AND s.deleted_at IS NULL AND s.reblog_of_id IS NULL AND s.visibility IN ('public','unlisted') LIMIT 2
-- STUBFILTER: mentions never bind a deleted native post attachment").bind(file).fetch_all(&state.pool).await.map_err(DbError::from)?;
    if owners.len() != 1 {
        return Err(ApiError::NotFound);
    }
    Ok(owners[0])
}
