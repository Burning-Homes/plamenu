//! Human pages and canonical native objects share website cards, never capabilities.
use crate::{AppState, entities, error::ApiError};
use plamenu_db::{DbError, account, status};
pub async fn for_target(state: &AppState, target: &str) -> Result<Vec<String>, ApiError> {
    super::target::owner(state, target).await?;
    let mut result = vec![target.to_owned()];
    let url = url::Url::parse(target).map_err(|_| ApiError::NotFound)?;
    let path = url.path();
    for kind in [
        crate::music_entities::Kind::Artist,
        crate::music_entities::Kind::Album,
    ] {
        let native = format!("/music/{}/", kind.path());
        let id = path
            .strip_prefix(&native)
            .and_then(|s| s.parse().ok())
            .or_else(|| {
                crate::music_entities::local_id(&state.config.domain, target)
                    .filter(|(k, _)| *k == kind)
                    .map(|(_, id)| id)
            });
        if let Some(id) = id {
            result.push(format!(
                "https://{}/{}/{id}",
                state.config.domain,
                native.trim_matches('/')
            ));
            result.push(crate::music_entities::uri(&state.config.domain, kind, id));
        }
    }
    let collection = path
        .strip_prefix("/music/collections/")
        .and_then(|s| s.parse().ok())
        .or_else(|| crate::music_library_federation::local_id(&state.config.domain, target));
    if let Some(id) = collection {
        let item = crate::music_collections::get(state, id, None).await?;
        result.push(format!(
            "https://{}/music/collections/{id}",
            state.config.domain
        ));
        if item.kind == "library" {
            result.push(crate::music_library_federation::uri(
                &state.config.domain,
                id,
            ));
        }
    }
    let asset = super::target::asset_id(path);
    if let Some(asset) = asset {
        result.push(format!(
            "https://{}/music/assets/{asset}",
            state.config.domain
        ));
        result.push(format!(
            "https://{}/music/assets/{asset}/stream",
            state.config.domain
        ));
        result.push(format!(
            "https://{}/music/assets/{asset}/download",
            state.config.domain
        ));
    }
    let status_id = if let Some(asset) = asset {
        sqlx::query_scalar("SELECT status_id FROM music_publications WHERE asset_id=$1")
            .bind(asset)
            .fetch_optional(&state.pool)
            .await
            .map_err(DbError::from)?
    } else {
        let candidate = path.rsplit('/').next().and_then(|s| s.parse().ok());
        if let Some(id) = candidate {
            if let Some(item) = status::find_by_id(&state.pool, id).await? {
                let owner = account::find_by_id(&state.pool, item.account_id)
                    .await?
                    .ok_or(ApiError::NotFound)?;
                (target == entities::status_uri_for_account(&state.config.domain, &item, &owner)
                    || target
                        == entities::status_web_url(&state.config.domain, &item, &owner.username)
                    || path == format!("/webmentions/posts/{id}"))
                .then_some(id)
            } else {
                None
            }
        } else {
            None
        }
    };
    if let Some(id) = status_id {
        append_status(state, id, &mut result).await?;
    }
    result.sort();
    result.dedup();
    Ok(result)
}

async fn append_status(
    state: &AppState,
    id: i64,
    result: &mut Vec<String>,
) -> Result<(), ApiError> {
    let (item, owner) = super::outgoing::public_status(state, id).await?;
    let uri = entities::status_uri_for_account(&state.config.domain, &item, &owner);
    result.push(uri.clone());
    result.push(entities::status_web_url(
        &state.config.domain,
        &item,
        &owner.username,
    ));
    result.push(format!(
        "https://{}/webmentions/posts/{id}",
        state.config.domain
    ));
    if item.object_type.as_deref() == Some("Audio") {
        result.push(format!("{uri}/track"));
    }
    if let Some(asset) =
        sqlx::query_scalar::<_, i64>("SELECT asset_id FROM music_publications WHERE status_id=$1")
            .bind(id)
            .fetch_optional(&state.pool)
            .await
            .map_err(DbError::from)?
    {
        result.push(format!(
            "https://{}/music/assets/{asset}",
            state.config.domain
        ));
    }
    Ok(())
}
