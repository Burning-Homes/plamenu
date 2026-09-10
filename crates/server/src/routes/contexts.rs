//! `ActivityPub` serving for conversation collections (FEP-f228 / FEP-171b).
//!
//! `GET /contexts/{id}` is a conversation's **collection of posts** (FEP-f228):
//! an `OrderedCollection` of the thread's distributable (public/unlisted) post
//! IRIs in chronological order, `attributedTo` the conversation owner. Peers
//! backfill a whole thread by reading it, instead of crawling `replies` node
//! by node — and it keeps working when an interior reply's origin vanishes.
//!
//! Only conversations we own (`uri IS NULL`) and whose root is distributable
//! are served; a private conversation exposes no posts collection (its history
//! travels by the FEP-171b container, `/contexts/{id}/history`), so this
//! route 404s rather than reveal that a private thread exists.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Uri, header};
use axum::response::{IntoResponse, Response};
use plamenu_ap::urls::{LocalStatusUrls, context_history_uri, context_page_url, context_uri};
use plamenu_db::account::{self, Account};
use plamenu_db::{conversation, status};
use serde::Deserialize;
use serde_json::{Value, json};

use super::params::truthy;
use super::require_ap_accept;
use crate::AppState;
use crate::entities::{account_uri, can_view};
use crate::error::ApiError;

/// Posts per collection page — Mastodon's `ContextsController::DESCENDANTS_LIMIT`.
const CONTEXT_PER_PAGE: i64 = 60;

#[derive(Deserialize)]
pub struct ContextQuery {
    page: Option<String>,
    min_id: Option<String>,
}

/// `GET /contexts/{id}` — a conversation's FEP-f228 collection of posts.
pub async fn get_context(
    State(state): State<AppState>,
    Path(conversation_id): Path<i64>,
    Query(query): Query<ContextQuery>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    require_ap_accept(&headers)?;
    // Locally owned only — a remote conversation's collection is the owner's.
    let conv = conversation::find(&state.pool, conversation_id)
        .await?
        .filter(|c| c.uri.is_none())
        .ok_or(ApiError::NotFound)?;
    let (Some(owner_id), Some(root_id)) = (conv.owner_account_id, conv.root_status_id) else {
        return Err(ApiError::NotFound);
    };
    let owner = account::find_by_id(&state.pool, owner_id)
        .await?
        .filter(Account::is_local)
        .ok_or(ApiError::NotFound)?;
    let root = status::find_by_id(&state.pool, root_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    // A private/direct conversation is not exposed as a posts collection.
    if !matches!(root.visibility.as_str(), "public" | "unlisted") {
        return Err(ApiError::NotFound);
    }

    let domain = &state.config.domain;
    let collection_url = context_uri(domain, conversation_id);
    let min_id = query
        .min_id
        .as_deref()
        .and_then(|raw| raw.parse::<i64>().ok())
        .unwrap_or(0);

    let posts =
        conversation::context_page(&state.pool, conversation_id, min_id, CONTEXT_PER_PAGE).await?;
    let items: Vec<Value> = posts
        .iter()
        .map(|post| {
            let iri = post.uri.clone().unwrap_or_else(|| {
                post.actor_uri.as_ref().map_or_else(
                    || LocalStatusUrls::new(domain, &post.username, post.status_id).id,
                    |actor_id| {
                        LocalStatusUrls::from_actor_id(
                            domain,
                            &post.username,
                            actor_id,
                            post.status_id,
                        )
                        .id
                    },
                )
            });
            Value::String(iri)
        })
        .collect();

    let full_page = i64::try_from(posts.len()).unwrap_or(i64::MAX) == CONTEXT_PER_PAGE;
    let next = full_page
        .then(|| context_page_url(&collection_url, posts.last().map(|post| post.status_id)));

    if truthy(query.page.as_deref()) {
        let page_id = context_page_url(&collection_url, query.min_id.as_ref().map(|_| min_id));
        let mut page = json!({
            "@context": plamenu_ap::AS_CONTEXT,
            "id": page_id,
            "type": "OrderedCollectionPage",
            "partOf": collection_url,
            "orderedItems": items,
        });
        if let Some(next) = next {
            page["next"] = json!(next);
        }
        return Ok((
            [(header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8)],
            Json(page),
        ));
    }

    // The envelope carries `attributedTo` (the owner) and inlines the first
    // page, so a single fetch gives a consumer the head of the thread.
    let mut first = json!({
        "id": context_page_url(&collection_url, None),
        "type": "OrderedCollectionPage",
        "partOf": collection_url,
        "orderedItems": items,
    });
    if let Some(next) = next {
        first["next"] = json!(next);
    }
    let document = json!({
        "@context": plamenu_ap::AS_CONTEXT,
        "id": collection_url,
        "type": "OrderedCollection",
        "attributedTo": account_uri(domain, &owner),
        "first": first,
    });
    Ok((
        [(header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8)],
        Json(document),
    ))
}

/// The verified signer of a container GET, if any (mirrors the outbox's
/// `signed_page_requester`): the container's content depends on who asks.
async fn signed_requester(state: &AppState, uri: &Uri, headers: &HeaderMap) -> Option<Account> {
    if !headers.contains_key("signature") {
        return None;
    }
    crate::signed_fetch::verified_get_sender(state, uri, headers)
        .await
        .ok()
}

/// `GET /contexts/{id}/history` — a private conversation's FEP-171b container:
/// an `OrderedCollection` of `Add(Create(...))`, `attributedTo` the owner.
/// Served only when the container is enabled and only to a verified requester
/// in the conversation's audience — private backfill is authorized, never open.
pub async fn get_context_history(
    State(state): State<AppState>,
    Path(conversation_id): Path<i64>,
    Query(query): Query<ContextQuery>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    require_ap_accept(&headers)?;
    // The owner emission is gated; a disabled instance has no container.
    if !state.conversation_containers().await {
        return Err(ApiError::NotFound);
    }
    let conv = conversation::find(&state.pool, conversation_id)
        .await?
        .filter(|c| c.uri.is_none())
        .ok_or(ApiError::NotFound)?;
    let (Some(owner_id), Some(root_id)) = (conv.owner_account_id, conv.root_status_id) else {
        return Err(ApiError::NotFound);
    };
    let owner = account::find_by_id(&state.pool, owner_id)
        .await?
        .filter(Account::is_local)
        .ok_or(ApiError::NotFound)?;
    let root = status::find_by_id(&state.pool, root_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    // Only private/direct conversations have a container.
    if !matches!(root.visibility.as_str(), "private" | "direct") {
        return Err(ApiError::NotFound);
    }
    // Authorize: the (verified) requester must be in the conversation audience.
    // An unsigned or non-audience request gets a 404, never the private thread.
    let requester = signed_requester(&state, &uri, &headers).await;
    if !can_view(&state.pool, &root, requester.as_ref().map(|r| r.id)).await? {
        return Err(ApiError::NotFound);
    }

    let domain = &state.config.domain;
    let container_url = context_history_uri(domain, conversation_id);
    let min_id = query
        .min_id
        .as_deref()
        .and_then(|raw| raw.parse::<i64>().ok())
        .unwrap_or(0);
    let items =
        crate::containers::history_items(&state, &conv, &owner, &root, min_id, CONTEXT_PER_PAGE)
            .await?;
    let full_page = i64::try_from(items.len()).unwrap_or(i64::MAX) == CONTEXT_PER_PAGE;
    // The last item's id ends `/{status_id}`; page by that.
    let last_id = items
        .last()
        .and_then(|item| item["id"].as_str())
        .and_then(|id| id.rsplit('/').next())
        .and_then(|id| id.parse::<i64>().ok());
    let next = full_page
        .then(|| last_id.map(|id| context_page_url(&container_url, Some(id))))
        .flatten();

    let ok = |body: Value| {
        (
            [
                (header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8),
                (header::VARY, "Signature"),
            ],
            Json(body),
        )
            .into_response()
    };

    if truthy(query.page.as_deref()) {
        let page_id = context_page_url(&container_url, query.min_id.as_ref().map(|_| min_id));
        let mut page = json!({
            "@context": plamenu_ap::activity::quote_context(),
            "id": page_id,
            "type": "OrderedCollectionPage",
            "partOf": container_url,
            "orderedItems": items,
        });
        if let Some(next) = next {
            page["next"] = json!(next);
        }
        return Ok(ok(page));
    }

    let mut first = json!({
        "id": context_page_url(&container_url, None),
        "type": "OrderedCollectionPage",
        "partOf": container_url,
        "orderedItems": items,
    });
    if let Some(next) = next {
        first["next"] = json!(next);
    }
    Ok(ok(json!({
        "@context": plamenu_ap::activity::quote_context(),
        "id": container_url,
        "type": "OrderedCollection",
        "attributedTo": account_uri(domain, &owner),
        "first": first,
    })))
}
