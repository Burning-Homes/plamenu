use axum::Json;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::StatusCode;
use axum::response::Redirect;
use plamenu_db::role::permission;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;

use super::auth::{LemmyAdmin, LemmyUser};
use super::entities::person;
use super::error::LemmyError;

pub async fn upload(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    mut multipart: Multipart,
) -> Result<Json<Value>, LemmyError> {
    current.require_scope("write").map_err(LemmyError::from)?;
    let mut files = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| LemmyError::bad_request("invalid_image"))?
    {
        if !matches!(field.name(), Some("images[]" | "file")) {
            continue;
        }
        let bytes = field
            .bytes()
            .await
            .map_err(|_| LemmyError::bad_request("invalid_image"))?
            .to_vec();
        let (_, entity) = crate::routes::media::store_upload(
            &state,
            current.account.id,
            bytes,
            None,
            None,
            false,
        )
        .await
        .map_err(LemmyError::from)?;
        let media_id = entity["id"]
            .as_str()
            .and_then(|id| id.parse::<i64>().ok())
            .or_else(|| entity["id"].as_i64())
            .ok_or_else(|| LemmyError::new(StatusCode::INTERNAL_SERVER_ERROR, "unknown"))?;
        let media = plamenu_db::media::find_owned(&state.pool, media_id, current.account.id)
            .await?
            .ok_or_else(|| LemmyError::new(StatusCode::INTERNAL_SERVER_ERROR, "unknown"))?;
        let alias = media
            .file_name
            .as_deref()
            .ok_or_else(|| LemmyError::bad_request("invalid_image"))?;
        let token = crate::auth::generate_secret();
        if let Err(error) = plamenu_db::lemmy_media::create(
            &state.pool,
            media.id,
            current.account.id,
            alias,
            &token,
        )
        .await
        {
            if let Ok(Some(orphan)) =
                plamenu_db::media::delete_unattached(&state.pool, media.id, current.account.id)
                    .await
            {
                for name in [orphan.file_name, orphan.small_file_name]
                    .into_iter()
                    .flatten()
                {
                    let _ = state.media.delete(&name).await;
                }
            }
            return Err(LemmyError::from(error));
        }
        files.push(json!({ "file": alias, "delete_token": token }));
    }
    if files.is_empty() {
        return Err(LemmyError::bad_request("no_image_given"));
    }
    Ok(Json(json!({ "msg": "ok", "files": files })))
}

pub async fn serve(Path(filename): Path<String>) -> Redirect {
    Redirect::temporary(&format!("/media/{filename}"))
}

pub async fn delete(
    State(state): State<AppState>,
    Path((token, filename)): Path<(String, String)>,
) -> Result<StatusCode, LemmyError> {
    let upload = plamenu_db::lemmy_media::find_capability(&state.pool, &filename, &token)
        .await?
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_image"))?;
    let item =
        plamenu_db::media::delete_unattached(&state.pool, upload.media_id, upload.account_id)
            .await?
            .ok_or_else(|| LemmyError::bad_request("image_in_use"))?;
    for name in [item.file_name, item.small_file_name].into_iter().flatten() {
        let _ = state.media.delete(&name).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Default, Deserialize)]
pub struct ListMedia {
    page: Option<i64>,
    limit: Option<i64>,
}

async fn list_views(
    state: &AppState,
    account_id: Option<i64>,
    query: &ListMedia,
) -> Result<Vec<Value>, LemmyError> {
    let limit = query.limit.unwrap_or(20).clamp(1, 50);
    let offset = (query.page.unwrap_or(1).max(1) - 1).saturating_mul(limit);
    let uploads = plamenu_db::lemmy_media::list(&state.pool, account_id, limit, offset).await?;
    let ids = uploads
        .iter()
        .map(|upload| upload.account_id)
        .collect::<Vec<_>>();
    let accounts = plamenu_db::account::find_by_ids(&state.pool, &ids).await?;
    let mut images = Vec::new();
    for upload in uploads {
        let Some(account) = accounts
            .iter()
            .find(|account| account.id == upload.account_id)
        else {
            continue;
        };
        let local_user_id = plamenu_db::user::find_by_account_id(&state.pool, account.id)
            .await?
            .map(|user| user.id);
        images.push(json!({
            "local_image": {
                "local_user_id": local_user_id,
                "pictrs_alias": upload.alias,
                "pictrs_delete_token": upload.delete_token,
                "published": crate::entities::rfc3339(upload.created_at).map_err(LemmyError::from)?,
            },
            "person": person(state, account, false).await?["person"],
        }));
    }
    Ok(images)
}

pub async fn list_owned(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Query(query): Query<ListMedia>,
) -> Result<Json<Value>, LemmyError> {
    current.require_scope("read").map_err(LemmyError::from)?;
    Ok(Json(
        json!({ "images": list_views(&state, Some(current.account.id), &query).await? }),
    ))
}

pub async fn list_all(
    State(state): State<AppState>,
    LemmyAdmin(admin): LemmyAdmin,
    Query(query): Query<ListMedia>,
) -> Result<Json<Value>, LemmyError> {
    admin
        .require(permission::ADMINISTRATOR, false)
        .map_err(LemmyError::from)?;
    Ok(Json(
        json!({ "images": list_views(&state, None, &query).await? }),
    ))
}

/// Resolve a same-instance Pictrs URL to the owned native upload bytes. Used
/// by profile/community image settings after Photon uploads the image first.
pub async fn owned_image_bytes(
    state: &AppState,
    account_id: i64,
    url: &str,
) -> Result<Option<Vec<u8>>, LemmyError> {
    let prefix = format!("https://{}/pictrs/image/", state.config.domain);
    let alias = url
        .strip_prefix(&prefix)
        .or_else(|| url.strip_prefix("/pictrs/image/"));
    let Some(alias) = alias else { return Ok(None) };
    let uploads = plamenu_db::lemmy_media::list(&state.pool, Some(account_id), 500, 0).await?;
    if !uploads.iter().any(|upload| upload.alias == alias) {
        return Err(LemmyError::new(StatusCode::FORBIDDEN, "image_not_owned"));
    }
    state
        .media
        .get(alias)
        .await
        .map(Some)
        .map_err(|error| LemmyError::from(crate::error::ApiError::Internal(Box::new(error))))
}
