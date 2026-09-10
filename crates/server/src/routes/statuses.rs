//! `ActivityPub` `Note` documents for local statuses, their
//! replies/likes/shares collections, and the `QuoteAuthorization` stamps
//! other servers fetch to verify quotes.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use plamenu_ap::activity::{quote_authorization_for_actor, quote_context};
use plamenu_ap::collection::{CountCollection, RepliesCollection, RepliesPage};
use plamenu_ap::urls::{LocalStatusUrls, LocalUserUrls, replies_page_url};
use plamenu_db::account::Account;
use plamenu_db::status::Status;
use plamenu_db::{account, quote, status};
use serde::Deserialize;
use serde_json::Value;

use super::params::truthy;
use super::{ap_requested, require_ap_accept};
use crate::AppState;
use crate::entities::{can_view, status_uri_for_account};
use crate::error::ApiError;
use crate::note::{NoteBatch, note_for_status, note_in_batch};
use crate::web::session::MaybeWebUser;

/// Looks up a local status that may be dereferenced by anyone:
/// followers-only posts, local-only posts, direct messages and boost rows are not
/// dereferenceable (remotes received them via their inboxes), and neither
/// are the collections hanging off them.
async fn dereferenceable_status(
    state: &AppState,
    actor_key: &str,
    request_uri: &Uri,
    status_id: i64,
) -> Result<(Account, Status), ApiError> {
    let account = super::local_actor_account(state, actor_key, request_uri).await?;
    let stored = status::find_local(&state.pool, account.id, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if matches!(stored.visibility.as_str(), "private" | "direct" | "local")
        || stored.reblog_of_id.is_some()
    {
        return Err(ApiError::NotFound);
    }
    Ok((account, stored))
}

/// `GET /users/{username}/statuses/{id}` — the Note a `Create` referenced.
pub async fn get_status(
    State(state): State<AppState>,
    MaybeWebUser(session): MaybeWebUser,
    Path((username, status_id)): Path<(String, i64)>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    // Like Mastodon: serve the human thread page to browsers, the Note document
    // to ActivityPub clients. The web view honours the viewer's visibility, so
    // it is not bound by the stricter AP dereferencing rules below.
    if !ap_requested(&headers) {
        let viewer_id = session.as_ref().map(|u| u.current.account.id);
        let focus = status::find_by_id(&state.pool, status_id)
            .await?
            .ok_or(ApiError::NotFound)?;
        if !can_view(&state.pool, &focus, viewer_id).await? {
            return Err(ApiError::NotFound);
        }
        let request_locale = crate::web::i18n::Locale::from_headers(&headers);
        return crate::web::pages::thread_view(&state, session, request_locale, focus, false).await;
    }
    let (account, stored) = match dereferenceable_status(&state, &username, &uri, status_id).await {
        Ok(pair) => pair,
        // A recently-deleted status answers `410 Gone` with a `Tombstone`
        // while the federated `Delete` propagates, then 404 once the
        // tombstone is swept.
        Err(ApiError::NotFound) => {
            if let Some(tomb) = plamenu_db::status_tombstone::find(&state.pool, status_id).await? {
                return Ok(tombstone_response(&tomb));
            }
            return Err(ApiError::NotFound);
        }
        Err(other) => return Err(other),
    };
    // A soft-deleted stub (the row survives to keep the reply tree; B/GtS)
    // answers `410 Gone` like a hard delete, never an emptied Note. A local
    // original recorded a tombstone on delete; fall back to a minimal
    // `Tombstone` synthesized from the derived AP id if it has lapsed.
    if status::is_deleted(&state.pool, stored.id).await? {
        if let Some(tomb) = plamenu_db::status_tombstone::find(&state.pool, status_id).await? {
            return Ok(tombstone_response(&tomb));
        }
        let actor_id = LocalUserUrls::for_account(
            &state.config.domain,
            &account.username,
            account.uri.as_deref(),
        )
        .id;
        let id = LocalStatusUrls::from_actor_id(
            &state.config.domain,
            &account.username,
            &actor_id,
            stored.id,
        )
        .id;
        return Ok(gone_tombstone(&id));
    }
    let mut note = note_for_status(&state, &stored, &account).await?;
    note["@context"] = quote_context();
    Ok((
        [(header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8)],
        Json(note),
    )
        .into_response())
}

/// The `410 Gone` `Tombstone` document for a deleted status, matching
/// Mastodon's `ActivityPub::TombstoneSerializer`.
fn tombstone_response(tomb: &plamenu_db::status_tombstone::StatusTombstone) -> Response {
    let body = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": tomb.uri,
        "type": "Tombstone",
        "formerType": "Note",
        "deleted": crate::entities::rfc3339(tomb.deleted_at).unwrap_or_default(),
    });
    (
        StatusCode::GONE,
        [(header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8)],
        Json(body),
    )
        .into_response()
}

/// A minimal `410 Gone` `Tombstone` for a stub whose row has lapsed —
/// same shape as [`tombstone_response`] without a recorded `deleted` time.
fn gone_tombstone(id: &str) -> Response {
    let body = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": id,
        "type": "Tombstone",
        "formerType": "Note",
    });
    (
        StatusCode::GONE,
        [(header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8)],
        Json(body),
    )
        .into_response()
}

/// Mastodon's `DESCENDANTS_LIMIT`: replies per collection page.
const REPLIES_PER_PAGE: i64 = 60;

#[derive(Deserialize)]
pub struct RepliesQuery {
    page: Option<String>,
    only_other_accounts: Option<String>,
    min_id: Option<String>,
}

/// `GET /users/{username}/statuses/{id}/replies` — the replies collection,
/// shaped like Mastodon's: the bare URL inlines a first page of the
/// author's own replies whose `next` pages by `min_id` and then switches to
/// `only_other_accounts=true`; local replies are inlined as full Notes,
/// remote ones appear as bare IRIs.
pub async fn get_replies(
    State(state): State<AppState>,
    Path((username, status_id)): Path<(String, i64)>,
    Query(query): Query<RepliesQuery>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    require_ap_accept(&headers)?;
    let (account, stored) = dereferenceable_status(&state, &username, &uri, status_id).await?;
    let actor_id = LocalUserUrls::for_account(
        &state.config.domain,
        &account.username,
        account.uri.as_deref(),
    )
    .id;
    let urls = LocalStatusUrls::from_actor_id(
        &state.config.domain,
        &account.username,
        &actor_id,
        stored.id,
    );

    let page_requested = truthy(query.page.as_deref());
    let only_other = truthy(query.only_other_accounts.as_deref());
    let min_id = query
        .min_id
        .as_deref()
        .and_then(|raw| raw.parse::<i64>().ok())
        .unwrap_or(0);

    let replies = status::replies_page(
        &state.pool,
        stored.id,
        account.id,
        !only_other,
        min_id,
        REPLIES_PER_PAGE,
    )
    .await?;

    // Local replies inline as full Notes; remote ones are bare IRIs. Both the
    // authors and every Note sidecar load once for the page — this collection
    // is the one crawlers walk to backfill a thread, and a page of 60 used to
    // cost ~19 round trips each.
    let local: Vec<&Status> = replies.iter().filter(|reply| reply.uri.is_none()).collect();
    let authors = if local.is_empty() {
        Vec::new()
    } else {
        account::find_by_ids(
            &state.pool,
            &local
                .iter()
                .map(|reply| reply.account_id)
                .collect::<Vec<_>>(),
        )
        .await?
    };
    let batch = NoteBatch::load(&state, &local).await?;
    let mut items = Vec::with_capacity(replies.len());
    for reply in &replies {
        items.push(if let Some(remote_uri) = &reply.uri {
            Value::String(remote_uri.clone())
        } else {
            let author = authors
                .iter()
                .find(|a| a.id == reply.account_id)
                .ok_or(ApiError::NotFound)?;
            note_in_batch(&state, reply, author, &batch)?
        });
    }

    let full_page = i64::try_from(replies.len()).unwrap_or(i64::MAX) == REPLIES_PER_PAGE;
    let last_id = replies.last().map(|reply| reply.id);
    // Mastodon's next-link scheme: the self-replies page keeps paging by
    // min_id while full, then hands over to the other-accounts pages, which
    // end (no `next`) once short.
    let next = if only_other {
        full_page.then(|| replies_page_url(&urls.replies, last_id, Some(true)))
    } else if full_page {
        Some(replies_page_url(&urls.replies, last_id, Some(false)))
    } else {
        Some(replies_page_url(&urls.replies, None, Some(true)))
    };

    // The page id echoes the request's pagination params, like Rails.
    let page_id = replies_page_url(
        &urls.replies,
        query.min_id.as_ref().map(|_| min_id),
        query.only_other_accounts.as_ref().map(|_| only_other),
    );
    let page = RepliesPage::standalone(quote_context(), page_id, &urls.replies, next, items);
    let document = if page_requested {
        serde_json::to_value(page)
    } else {
        serde_json::to_value(RepliesCollection::new(quote_context(), &urls.replies, page))
    }
    .map_err(|e| ApiError::Internal(Box::new(e)))?;
    Ok((
        [(header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8)],
        Json(document),
    ))
}

/// Which engagement collection of a status is being served.
#[derive(Clone, Copy)]
enum EngagementCollection {
    Likes,
    Shares,
}

/// `GET /users/{username}/statuses/{id}/likes`.
pub async fn get_likes(
    state: State<AppState>,
    path: Path<(String, i64)>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    engagement_collection(state, path, uri, headers, EngagementCollection::Likes).await
}

/// `GET /users/{username}/statuses/{id}/shares`.
pub async fn get_shares(
    state: State<AppState>,
    path: Path<(String, i64)>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    engagement_collection(state, path, uri, headers, EngagementCollection::Shares).await
}

/// Serves a likes/shares collection the way Mastodon does: a count-only
/// unordered `Collection` — the favouriting/boosting actors are never
/// listed.
async fn engagement_collection(
    State(state): State<AppState>,
    Path((username, status_id)): Path<(String, i64)>,
    uri: Uri,
    headers: HeaderMap,
    which: EngagementCollection,
) -> Result<impl IntoResponse, ApiError> {
    require_ap_accept(&headers)?;
    let (account, stored) = dereferenceable_status(&state, &username, &uri, status_id).await?;
    let actor_id = LocalUserUrls::for_account(
        &state.config.domain,
        &account.username,
        account.uri.as_deref(),
    )
    .id;
    let urls = LocalStatusUrls::from_actor_id(
        &state.config.domain,
        &account.username,
        &actor_id,
        stored.id,
    );
    let engagement = status::engagement_for(&state.pool, &[stored.id])
        .await?
        .remove(&stored.id)
        .unwrap_or_default();
    let (collection_url, total) = match which {
        EngagementCollection::Likes => (urls.likes, engagement.favourites),
        EngagementCollection::Shares => (urls.shares, engagement.reblogs),
    };
    let document = CountCollection::new(&collection_url, u64::try_from(total).unwrap_or(0));
    Ok((
        [(header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8)],
        Json(document),
    ))
}

/// `GET /users/{username}/quote_authorizations/{id}` — the FEP-044f stamp
/// proving the quoted (local) author authorized a quote.
pub async fn get_quote_authorization(
    State(state): State<AppState>,
    Path((username, quote_id)): Path<(String, i64)>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    require_ap_accept(&headers)?;
    let account = super::local_actor_account(&state, &username, &uri).await?;
    let row = quote::find_by_id(&state.pool, quote_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if row.state != "accepted" || row.quoted_account_id != Some(account.id) {
        return Err(ApiError::NotFound);
    }
    let quoted_status_id = row.quoted_status_id.ok_or(ApiError::NotFound)?;
    let quoted = status::find_by_id(&state.pool, quoted_status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let stamp = quote_authorization_for_actor(
        &crate::entities::account_uri(&state.config.domain, &account),
        row.id,
        &row.status_uri,
        &status_uri_for_account(&state.config.domain, &quoted, &account),
    );
    Ok((
        [(header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8)],
        Json(stamp),
    ))
}
