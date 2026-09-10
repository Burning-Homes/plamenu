//! `ActivityPub` serving for account collections (Mastodon 4.6 / FEP-7aa9):
//! the per-actor `featuredCollections` listing, a single `FeaturedCollection`
//! document, and the `FeatureAuthorization` stamp a local featured account
//! grants. The document bodies are built in [`crate::collections`].

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Uri, header};
use axum::response::IntoResponse;
use plamenu_ap::urls::LocalUserUrls;
use plamenu_db::collection;
use serde::Deserialize;
use serde_json::{Value, json};

use super::require_ap_accept;
use crate::error::ApiError;
use crate::{AppState, collections as service};

/// Items per page, matching Mastodon's featured-collections endpoint.
const PER_PAGE: i64 = 5;

#[derive(Deserialize)]
pub struct PageQuery {
    page: Option<i64>,
}

/// `GET /users/{username}/featured_collections` — the actor's
/// `featuredCollections` endpoint. The bare URL is an unordered `Collection`
/// envelope pointing at the first page; `?page=N` inlines up to `PER_PAGE`
/// `FeaturedCollection` objects, like Mastodon's `CollectionPresenter`.
pub async fn get_featured_collections(
    State(state): State<AppState>,
    Path(username): Path<String>,
    Query(query): Query<PageQuery>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    require_ap_accept(&headers)?;
    let account = super::local_actor_account(&state, &username, &uri).await?;
    let url = LocalUserUrls::for_account(
        &state.config.domain,
        &account.username,
        account.uri.as_deref(),
    )
    .featured_collections;
    let size = collection::count_owned(&state.pool, account.id).await?;

    let Some(page) = query.page else {
        let document = json!({
            "@context": plamenu_ap::AS_CONTEXT,
            "id": url,
            "type": "Collection",
            "totalItems": size,
            "first": format!("{url}?page=1"),
        });
        return Ok((
            [(header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8)],
            Json(document),
        ));
    };

    let page = page.max(1);
    let offset = (page - 1) * PER_PAGE;
    let collections =
        collection::owned_by(&state.pool, account.id, false, offset, PER_PAGE).await?;
    // One batched build for the page (owner, items, members and topics each
    // one query) instead of up to ~28 round trips per collection.
    let mut built = service::featured_collection_values_for(&state, &collections).await?;
    let items: Vec<Value> = collections
        .iter()
        .filter_map(|collection| built.remove(&collection.id))
        .collect();
    let mut document = json!({
        "@context": plamenu_ap::featured::featured_context(),
        "id": format!("{url}?page={page}"),
        "type": "CollectionPage",
        "totalItems": size,
        "partOf": url,
        "items": items,
    });
    if page * PER_PAGE < size {
        document["next"] = json!(format!("{url}?page={}", page + 1));
    }
    if page > 1 {
        document["prev"] = json!(format!("{url}?page={}", page - 1));
    }
    Ok((
        [(header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8)],
        Json(document),
    ))
}

/// `GET /users/{username}/collections/{id}` — a single `FeaturedCollection`
/// document.
pub async fn get_collection(
    State(state): State<AppState>,
    Path((username, collection_id)): Path<(String, i64)>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    require_ap_accept(&headers)?;
    let account = super::local_actor_account(&state, &username, &uri).await?;
    let collection = collection::find(&state.pool, collection_id)
        .await?
        .filter(|c| c.account_id == account.id && c.local)
        .ok_or(ApiError::NotFound)?;
    let document = service::featured_collection_document(&state, &collection).await?;
    Ok((
        [(header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8)],
        Json(document),
    ))
}

/// `GET /users/{username}/feature_authorizations/{id}` — the
/// `FeatureAuthorization` stamp proving the local featured account consented to
/// an accepted membership.
pub async fn get_feature_authorization(
    State(state): State<AppState>,
    Path((username, item_id)): Path<(String, i64)>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    require_ap_accept(&headers)?;
    let account = super::local_actor_account(&state, &username, &uri).await?;
    let item = collection::find_item_by_id(&state.pool, item_id)
        .await?
        .filter(|i| i.account_id == Some(account.id) && i.state == "accepted")
        .ok_or(ApiError::NotFound)?;
    let document: Value = service::feature_authorization_document(&state, &account, &item).await?;
    Ok((
        [(header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8)],
        Json(document),
    ))
}
