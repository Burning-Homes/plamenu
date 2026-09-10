use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use plamenu_db::custom_emoji::CustomEmoji;
use plamenu_db::role::permission;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{AppState, admin_log};

use super::auth::LemmyAdmin;
use super::error::LemmyError;
use super::ids::resolve_required;
use plamenu_db::lemmy_id::Kind;

fn image_url(state: &AppState, emoji: &CustomEmoji) -> String {
    emoji.image_file_name.as_deref().map_or_else(
        || emoji.image_remote_url.clone().unwrap_or_default(),
        |file| format!("https://{}/media/{file}", state.config.domain),
    )
}

pub async fn views(state: &AppState, emojis: &[CustomEmoji]) -> Result<Vec<Value>, LemmyError> {
    let ids = emojis.iter().map(|emoji| emoji.id).collect::<Vec<_>>();
    let metadata = plamenu_db::lemmy_emoji::metadata_for(&state.pool, &ids).await?;
    let mut views = Vec::with_capacity(emojis.len());
    for emoji in emojis {
        let meta = metadata.get(&emoji.id).cloned().unwrap_or_default();
        views.push(json!({
            "custom_emoji": {
                "id": emoji.id,
                "local_site_id": 1,
                "shortcode": emoji.shortcode,
                "image_url": image_url(state, emoji),
                "alt_text": meta.alt_text,
                "category": emoji.category.clone().unwrap_or_default(),
                "published": crate::entities::rfc3339(emoji.created_at).map_err(LemmyError::from)?,
                "updated": crate::entities::rfc3339(emoji.updated_at).map_err(LemmyError::from)?,
            },
            "keywords": meta.keywords.into_iter().map(|keyword| json!({
                "custom_emoji_id": emoji.id,
                "keyword": keyword,
            })).collect::<Vec<_>>(),
        }));
    }
    Ok(views)
}

fn normalize_keywords(values: Vec<String>) -> Vec<String> {
    let mut values = values
        .into_iter()
        .map(|value| value.trim().to_lowercase())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
}

async fn store_image(
    state: &AppState,
    bytes: Vec<u8>,
) -> Result<(String, &'static str, i64), LemmyError> {
    let max_bytes = plamenu_db::custom_emoji::settings(&state.pool)
        .await?
        .max_file_size_bytes();
    let (content_type, extension) =
        crate::media_processing::validate_emoji_image(&bytes, max_bytes)
            .map_err(LemmyError::from)?;
    let file_name = format!("{}.{}", plamenu_db::id::next(), extension);
    let size = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
    state
        .media
        .put(&file_name, bytes)
        .await
        .map_err(|error| LemmyError::from(crate::error::ApiError::Internal(Box::new(error))))?;
    Ok((file_name, content_type, size))
}

#[derive(Deserialize)]
pub struct CreateCustomEmoji {
    category: String,
    shortcode: String,
    image_url: String,
    alt_text: String,
    keywords: Vec<String>,
}

pub async fn create(
    State(state): State<AppState>,
    LemmyAdmin(admin): LemmyAdmin,
    Json(form): Json<CreateCustomEmoji>,
) -> Result<Json<Value>, LemmyError> {
    admin
        .require(permission::MANAGE_CUSTOM_EMOJIS, true)
        .map_err(LemmyError::from)?;
    if !plamenu_ap::emoji::is_valid_shortcode(&form.shortcode, plamenu_ap::emoji::MAX_SHORTCODE_LEN)
    {
        return Err(LemmyError::bad_request("invalid_custom_emoji"));
    }
    let bytes = super::media::owned_image_bytes(&state, admin.current.account.id, &form.image_url)
        .await?
        .ok_or_else(|| LemmyError::bad_request("invalid_image_url"))?;
    let (file, content_type, size) = store_image(&state, bytes).await?;
    let category = form.category.trim();
    let created = plamenu_db::custom_emoji::create_local(
        &state.pool,
        form.shortcode.trim(),
        &file,
        content_type,
        size,
        (!category.is_empty()).then_some(category),
    )
    .await?;
    let Some(created) = created else {
        let _ = state.media.delete(&file).await;
        return Err(LemmyError::bad_request("custom_emoji_already_exists"));
    };
    let keywords = normalize_keywords(form.keywords);
    if let Err(error) =
        plamenu_db::lemmy_emoji::set_metadata(&state.pool, created.id, &form.alt_text, &keywords)
            .await
    {
        let _ = plamenu_db::custom_emoji::delete_local_by_id(&state.pool, created.id).await;
        let _ = state.media.delete(&file).await;
        return Err(LemmyError::from(error));
    }
    admin_log::record(
        &state.pool,
        admin.current.account.id,
        "create",
        &admin_log::Target::custom_emoji(created.id, &created.shortcode),
    )
    .await?;
    let view = views(&state, &[created]).await?.remove(0);
    Ok(Json(json!({ "custom_emoji": view })))
}

#[derive(Deserialize)]
pub struct EditCustomEmoji {
    id: i32,
    category: String,
    image_url: String,
    alt_text: String,
    keywords: Vec<String>,
}

pub async fn edit(
    State(state): State<AppState>,
    LemmyAdmin(admin): LemmyAdmin,
    Json(form): Json<EditCustomEmoji>,
) -> Result<Json<Value>, LemmyError> {
    admin
        .require(permission::MANAGE_CUSTOM_EMOJIS, true)
        .map_err(LemmyError::from)?;
    let id = resolve_required(
        &state,
        Kind::CustomEmoji,
        form.id,
        "couldnt_find_custom_emoji",
    )
    .await?;
    let old = plamenu_db::custom_emoji::find_local_by_id(&state.pool, id)
        .await?
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_custom_emoji"))?;
    let current_url = image_url(&state, &old);
    let replacement = if form.image_url == current_url {
        None
    } else {
        let bytes =
            super::media::owned_image_bytes(&state, admin.current.account.id, &form.image_url)
                .await?
                .ok_or_else(|| LemmyError::bad_request("invalid_image_url"))?;
        Some(store_image(&state, bytes).await?)
    };
    let category = form.category.trim();
    let keywords = normalize_keywords(form.keywords);
    let image = replacement
        .as_ref()
        .map(|(file, content_type, size)| (file.as_str(), *content_type, *size));
    if !plamenu_db::lemmy_emoji::update(
        &state.pool,
        id,
        image,
        (!category.is_empty()).then_some(category),
        &form.alt_text,
        &keywords,
    )
    .await?
    {
        if let Some((file, _, _)) = replacement {
            let _ = state.media.delete(&file).await;
        }
        return Err(LemmyError::new(
            StatusCode::NOT_FOUND,
            "couldnt_find_custom_emoji",
        ));
    }
    if replacement.is_some()
        && let Some(old_file) = old.image_file_name.as_deref()
    {
        let _ = state.media.delete(old_file).await;
    }
    let updated = plamenu_db::custom_emoji::find_local_by_id(&state.pool, id)
        .await?
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_custom_emoji"))?;
    admin_log::record(
        &state.pool,
        admin.current.account.id,
        "update",
        &admin_log::Target::custom_emoji(updated.id, &updated.shortcode),
    )
    .await?;
    let view = views(&state, &[updated]).await?.remove(0);
    Ok(Json(json!({ "custom_emoji": view })))
}

#[derive(Deserialize)]
pub struct DeleteCustomEmoji {
    id: i32,
}

pub async fn delete(
    State(state): State<AppState>,
    LemmyAdmin(admin): LemmyAdmin,
    Json(form): Json<DeleteCustomEmoji>,
) -> Result<Json<Value>, LemmyError> {
    admin
        .require(permission::MANAGE_CUSTOM_EMOJIS, true)
        .map_err(LemmyError::from)?;
    let id = resolve_required(
        &state,
        Kind::CustomEmoji,
        form.id,
        "couldnt_find_custom_emoji",
    )
    .await?;
    let deleted = plamenu_db::custom_emoji::delete_local_by_id(&state.pool, id)
        .await?
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_custom_emoji"))?;
    if let Some(file) = deleted.image_file_name.as_deref() {
        let _ = state.media.delete(file).await;
    }
    admin_log::record(
        &state.pool,
        admin.current.account.id,
        "destroy",
        &admin_log::Target::custom_emoji(deleted.id, &deleted.shortcode),
    )
    .await?;
    Ok(Json(json!({ "success": true })))
}
