//! `/api/v1/collections/*`, `/api/v1/accounts/{id}/collections` and
//! `/api/v1/accounts/{id}/in_collections` — account collections (Mastodon
//! 4.6 / FEP-7aa9). The deprecated `/api/v1_alpha` alias is intentionally
//! omitted (Mastodon deprecated it 2026-06-10). The service logic lives in
//! [`crate::collections`].

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, header};
use axum::response::IntoResponse;
use plamenu_db::collection::{self, Collection};
use plamenu_db::{account, block};
use serde::Deserialize;
use serde_json::{Value, json};

use super::params::{parse_body, truthy};
use crate::auth::{CurrentUser, MaybeUser};
use crate::collections::{self as service, CollectionParams};
use crate::error::ApiError;
use crate::state::AppState;

const DEFAULT_LIMIT: i64 = 40;
const MAX_LIMIT: i64 = 100;

#[derive(Deserialize, Default)]
pub struct PageQuery {
    offset: Option<i64>,
    limit: Option<i64>,
}

/// Builds Mastodon's offset-paginated `Link` header (`next` while a full page
/// came back, `prev` whenever the offset is non-zero).
fn offset_link_header(
    domain: &str,
    path: &str,
    limit: i64,
    offset: i64,
    returned: usize,
) -> HeaderMap {
    let mut links = Vec::new();
    if i64::try_from(returned).unwrap_or(i64::MAX) == limit {
        links.push(format!(
            "<https://{domain}{path}?limit={limit}&offset={}>; rel=\"next\"",
            offset + limit
        ));
    }
    if offset > 0 {
        links.push(format!(
            "<https://{domain}{path}?limit={limit}&offset={}>; rel=\"prev\"",
            (offset - limit).max(0)
        ));
    }
    let mut headers = HeaderMap::new();
    if !links.is_empty()
        && let Ok(value) = links.join(", ").parse()
    {
        headers.insert(header::LINK, value);
    }
    headers
}

/// A collection by id or `404` (the REST `show`/`update`/`destroy` look up,
/// then authorize).
async fn find_collection(state: &AppState, collection_id: i64) -> Result<Collection, ApiError> {
    collection::find(&state.pool, collection_id)
        .await?
        .ok_or(ApiError::NotFound)
}

/// `GET /api/v1/accounts/{id}/collections` — an account's collections; a
/// non-owner sees only discoverable ones, a blocked viewer sees none.
pub async fn index(
    State(state): State<AppState>,
    MaybeUser(viewer): MaybeUser,
    Path(account_id): Path<i64>,
    Query(query): Query<PageQuery>,
) -> Result<impl IntoResponse, ApiError> {
    if let Some(user) = &viewer {
        user.require_scope("read:collections")?;
    }
    let account = account::find_publicly_available_by_id(&state.pool, account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let viewer_id = viewer.as_ref().map(|u| u.account.id);
    // A blocked viewer (or domain-blocked) gets an empty list, like Mastodon's
    // `index_collections?` rescue.
    let blocked = match viewer_id {
        Some(id) => {
            block::exists(&state.pool, account.id, id).await?
                || plamenu_db::account_domain_block::blocks_account_domain(
                    &state.pool,
                    account.id,
                    id,
                )
                .await?
        }
        None => false,
    };
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let offset = query.offset.unwrap_or(0).max(0);
    let only_discoverable = viewer_id != Some(account.id);
    let collections = if blocked {
        Vec::new()
    } else {
        collection::owned_by(&state.pool, account.id, only_discoverable, offset, limit).await?
    };
    let mut entities = Vec::with_capacity(collections.len());
    for collection in &collections {
        entities.push(
            service::collection_json(&state.pool, &state.config.domain, collection, viewer_id)
                .await?,
        );
    }
    let path = format!("/api/v1/accounts/{account_id}/collections");
    let headers = offset_link_header(&state.config.domain, &path, limit, offset, entities.len());
    // Mastodon renders the index with `adapter: :json`, wrapping the array in a
    // `collections` root key — 3rd-party clients read `data.collections`.
    Ok((headers, Json(json!({ "collections": entities }))))
}

/// `GET /api/v1/accounts/{id}/in_collections` — the collections an account is
/// featured in; self only.
pub async fn in_collections(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(account_id): Path<i64>,
    Query(query): Query<PageQuery>,
) -> Result<impl IntoResponse, ApiError> {
    current.require_scope("read:collections")?;
    let account = account::find_by_id(&state.pool, account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if account.id != current.account.id {
        // Mastodon's `index_featured_in_collections?` policy: self only,
        // otherwise Pundit denies with 403 (not 404).
        return Err(ApiError::Forbidden("This action is not allowed".into()));
    }
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let offset = query.offset.unwrap_or(0).max(0);
    let collections = collection::containing(&state.pool, account.id, offset, limit).await?;
    let mut entities = Vec::with_capacity(collections.len());
    for collection in &collections {
        entities.push(
            service::collection_json(
                &state.pool,
                &state.config.domain,
                collection,
                Some(current.account.id),
            )
            .await?,
        );
    }
    let path = format!("/api/v1/accounts/{account_id}/in_collections");
    let headers = offset_link_header(&state.config.domain, &path, limit, offset, entities.len());
    // Same `adapter: :json` `collections` root wrapper as `index`.
    Ok((headers, Json(json!({ "collections": entities }))))
}

#[derive(Deserialize, Default)]
pub struct CreateParams {
    name: Option<String>,
    description: Option<String>,
    language: Option<String>,
    sensitive: Option<Value>,
    discoverable: Option<Value>,
    tag_name: Option<String>,
    #[serde(default)]
    account_ids: Vec<String>,
}

fn bool_param(value: Option<&Value>, default: bool) -> bool {
    match value {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => truthy(Some(s)),
        _ => default,
    }
}

/// `POST /api/v1/collections`.
pub async fn create(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:collections")?;
    let mut params: CreateParams = parse_body(&headers, &body)?;
    // Mastodon clients send `account_ids[]` form-encoded; recover it.
    if params.account_ids.is_empty() {
        params.account_ids = super::params::repeated_form_field(&headers, &body, "account_ids");
    }
    let account_ids = parse_account_ids(&params.account_ids)?;
    let collection_params = CollectionParams {
        name: params.name.unwrap_or_default(),
        description: params.description.unwrap_or_default(),
        language: params.language,
        sensitive: bool_param(params.sensitive.as_ref(), false),
        discoverable: bool_param(params.discoverable.as_ref(), false),
        tag_name: params.tag_name,
    };
    let collection =
        service::create_collection(&state, &current.account, collection_params, &account_ids)
            .await?;
    let entity = service::collection_json(
        &state.pool,
        &state.config.domain,
        &collection,
        Some(current.account.id),
    )
    .await?;
    // Mastodon renders create with `adapter: :json` → `{ "collection": … }`.
    Ok(Json(json!({ "collection": entity })))
}

fn parse_account_ids(raw: &[String]) -> Result<Vec<i64>, ApiError> {
    raw.iter()
        .map(|s| s.parse::<i64>().map_err(|_| ApiError::NotFound))
        .collect()
}

/// `GET /api/v1/collections/{id}` — 404 when the owner blocks the viewer.
pub async fn show(
    State(state): State<AppState>,
    MaybeUser(viewer): MaybeUser,
    Path(collection_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    if let Some(user) = &viewer {
        user.require_scope("read:collections")?;
    }
    let collection = find_collection(&state, collection_id).await?;
    let viewer_id = viewer.as_ref().map(|u| u.account.id);
    if let Some(id) = viewer_id
        && block::exists(&state.pool, collection.account_id, id).await?
    {
        return Err(ApiError::NotFound);
    }
    Ok(Json(
        service::collection_with_accounts_json(
            &state.pool,
            &state.config.domain,
            &collection,
            viewer_id,
        )
        .await?,
    ))
}

/// A collection the current user owns, or the right error (404 missing, 403
/// not owner) — Mastodon's `owner?` policy.
async fn owned_collection(
    state: &AppState,
    current: &CurrentUser,
    collection_id: i64,
) -> Result<Collection, ApiError> {
    let collection = find_collection(state, collection_id).await?;
    if collection.account_id != current.account.id {
        return Err(ApiError::Forbidden("This action is not allowed".into()));
    }
    Ok(collection)
}

#[derive(Deserialize, Default)]
pub struct UpdateParams {
    name: Option<String>,
    description: Option<String>,
    language: Option<String>,
    sensitive: Option<Value>,
    discoverable: Option<Value>,
    tag_name: Option<String>,
}

/// `PATCH /api/v1/collections/{id}` — absent attributes keep their value.
pub async fn update(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(collection_id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:collections")?;
    let existing = owned_collection(&state, &current, collection_id).await?;
    let params: UpdateParams = parse_body(&headers, &body)?;
    // `update_collection` keeps the existing tag when `tag_name` is absent.
    let collection_params = CollectionParams {
        name: params.name.unwrap_or_else(|| existing.name.clone()),
        description: params
            .description
            .unwrap_or_else(|| existing.description.clone()),
        language: params.language.or_else(|| existing.language.clone()),
        sensitive: bool_param(params.sensitive.as_ref(), existing.sensitive),
        discoverable: bool_param(params.discoverable.as_ref(), existing.discoverable),
        tag_name: params.tag_name,
    };
    let updated =
        service::update_collection(&state, &existing, &current.account, collection_params).await?;
    let entity = service::collection_json(
        &state.pool,
        &state.config.domain,
        &updated,
        Some(current.account.id),
    )
    .await?;
    // Mastodon renders update with `adapter: :json` → `{ "collection": … }`.
    Ok(Json(json!({ "collection": entity })))
}

/// `DELETE /api/v1/collections/{id}`.
pub async fn destroy(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(collection_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:collections")?;
    let collection = owned_collection(&state, &current, collection_id).await?;
    service::delete_collection(&state, &collection, &current.account).await?;
    Ok(Json(json!({})))
}

#[derive(Deserialize, Default)]
pub struct ItemParams {
    account_id: Option<String>,
}

/// `POST /api/v1/collections/{id}/items`.
pub async fn add_item(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(collection_id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:collections")?;
    let collection = owned_collection(&state, &current, collection_id).await?;
    let params: ItemParams = parse_body(&headers, &body)?;
    let account_id: i64 = params
        .account_id
        .ok_or_else(|| ApiError::Unprocessable("`account_id` parameter is missing".into()))?
        .parse()
        .map_err(|_| ApiError::NotFound)?;
    let member = account::find_by_id(&state.pool, account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let item = service::add_account(&state, &collection, &current.account, &member).await?;
    let entity = crate::collections::collection_item_entity(&item)?;
    // Mastodon renders the item with `adapter: :json` → `{ "collection_item": … }`.
    Ok(Json(json!({ "collection_item": entity })))
}

/// `DELETE /api/v1/collections/{id}/items/{item_id}` — owner removal.
pub async fn remove_item(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((collection_id, item_id)): Path<(i64, i64)>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:collections")?;
    let collection = owned_collection(&state, &current, collection_id).await?;
    let item = collection::find_item(&state.pool, collection.id, item_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    service::delete_item(&state, &collection, &current.account, &item).await?;
    Ok(Json(json!({})))
}

/// `POST /api/v1/collections/{id}/items/{item_id}/revoke` — the *featured*
/// account removes itself (not the owner); Mastodon's `revoke?` policy.
pub async fn revoke_item(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((collection_id, item_id)): Path<(i64, i64)>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:collections")?;
    let item = collection::find_item(&state.pool, collection_id, item_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if item.account_id != Some(current.account.id) {
        return Err(ApiError::Forbidden("This action is not allowed".into()));
    }
    service::revoke_item(&state, &current.account, &item).await?;
    Ok(Json(json!({})))
}
