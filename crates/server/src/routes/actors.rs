//! `ActivityPub` actor documents and follower/following collections for
//! local users.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Uri, header};
use axum::response::{IntoResponse, Response};
use plamenu_ap::activity;
use plamenu_ap::collection::{
    ITEMS_PER_PAGE, InlineCollection, InlineOrderedCollection, OrderedCollection,
    OrderedCollectionPage, OutboxCollection, OutboxPage,
};
use plamenu_ap::urls::{LocalUserUrls, outbox_page_url};
use plamenu_db::account::{self, Account};
use plamenu_db::{follow, status};
use serde::Deserialize;
use serde_json::Value;
use time::format_description::well_known::Rfc3339;

use super::params::truthy;
use super::{ap_requested, require_ap_accept};
use crate::AppState;
use crate::entities::status_uri_for_account;
use crate::error::ApiError;
use crate::note::NoteBatch;
use crate::profile::local_actor;
use crate::web::i18n::Locale;
use crate::web::session::MaybeWebUser;

pub async fn get_actor(
    State(state): State<AppState>,
    MaybeWebUser(session): MaybeWebUser,
    request_locale: Locale,
    uri: Uri,
    Path(username): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let account = super::local_actor_account(&state, &username, &uri).await?;
    if account.suspended() {
        // Deleted: gone for every consumer. Reversibly suspended: browsers
        // get a 403, but AP clients still get the blanked actor document
        // (with `suspended: true`) — Mastodon's `AccountsController`
        // serves JSON through a temporary suspension so peers can mirror
        // the state instead of dropping the account.
        if crate::moderation::permanently_unavailable(&state.pool, &account).await? {
            return Err(ApiError::Gone);
        }
        if !ap_requested(&headers) {
            return Err(ApiError::Forbidden("This account is suspended".into()));
        }
    }
    // Like Mastodon: serve the human profile page to browsers and the actor
    // document to ActivityPub clients, off the same URL.
    if !ap_requested(&headers) {
        return crate::web::pages::profile_view(&state, session, request_locale, account).await;
    }
    // Under authorized fetch (secure mode) the actor document is not gated by
    // the shared `signed_fetch` middleware — this handler owns the policy so it
    // can downgrade rather than reject. A *present* signature must verify to
    // unlock the full document (else 401/403, exactly as the gate would have
    // answered); an *unsigned* AP caller gets either the profile document or a
    // key-only "blanked" actor depending on `authorized_fetch_unsigned_profile`,
    // so peers that dereference us unsigned — e.g. default-config Lemmy — can
    // still verify our deliveries either way. A suspended account already
    // serializes blanked (with `suspended: true`), so it is served as-is.
    let actor = if !state.authorized_fetch().await || account.suspended() {
        local_actor(&state, &account).await?
    } else if crate::signed_fetch::request_is_signed(&headers) {
        crate::signed_fetch::verified_get_sender(&state, &uri, &headers).await?;
        local_actor(&state, &account).await?
    } else if state.authorized_fetch_unsigned_profile().await {
        local_actor(&state, &account).await?
    } else {
        crate::profile::key_only_actor(&state, &account).await?
    };
    Ok((
        [(header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8)],
        Json(actor),
    )
        .into_response())
}

/// `GET /users/{username}/collections/featured` — the pinned-statuses
/// collection the actor document advertises as `featured`, most recently
/// pinned first. Like Mastodon: distributable (public/unlisted) statuses
/// inlined as full Notes, others (followers-only pins) as bare IRIs.
pub async fn get_featured(
    State(state): State<AppState>,
    Path(username): Path<String>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    require_ap_accept(&headers)?;
    let account = super::local_actor_account(&state, &username, &uri).await?;
    ensure_available(&state, &account).await?;
    let urls = LocalUserUrls::for_account(
        &state.config.domain,
        &account.username,
        account.uri.as_deref(),
    );
    let pinned = plamenu_db::pin::pinned_statuses(&state.pool, account.id).await?;
    let inlined: Vec<&plamenu_db::status::Status> = pinned
        .iter()
        .filter(|item| matches!(item.visibility.as_str(), "public" | "unlisted"))
        .collect();
    let batch = NoteBatch::load(&state, &inlined).await?;
    let mut items = Vec::with_capacity(pinned.len());
    for item in &pinned {
        if item.visibility == "local" {
            continue;
        }
        if matches!(item.visibility.as_str(), "public" | "unlisted") {
            items.push(crate::note::note_in_batch(&state, item, &account, &batch)?);
        } else {
            items.push(Value::String(crate::entities::status_uri_for_account(
                &state.config.domain,
                item,
                &account,
            )));
        }
    }
    let mut document = serde_json::to_value(InlineOrderedCollection::new(&urls.featured, items))
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    // Inlined Notes may carry quote/sensitive terms; ship the full context.
    document["@context"] = plamenu_ap::activity::quote_context();
    Ok((
        [(header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8)],
        Json(document),
    ))
}

/// `GET /users/{username}/collections/tags` — the featured-hashtags
/// collection the actor document advertises as `featuredTags`: an unordered
/// `Collection` of `Hashtag` objects, like Mastodon's
/// `ActivityPub::CollectionsController` with `id=tags`. Peers dereference it
/// on actor refresh to sync the profile's featured tags.
pub async fn get_featured_tags(
    State(state): State<AppState>,
    Path(username): Path<String>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    require_ap_accept(&headers)?;
    let account = super::local_actor_account(&state, &username, &uri).await?;
    ensure_available(&state, &account).await?;
    let urls = LocalUserUrls::for_account(
        &state.config.domain,
        &account.username,
        account.uri.as_deref(),
    );
    let items = plamenu_db::featured_tag::list(&state.pool, account.id)
        .await?
        .iter()
        .map(|featured| {
            plamenu_ap::actor::profile_hashtag_tag(
                &state.config.domain,
                &account.username,
                &featured.name,
            )
        })
        .collect();
    Ok((
        [(header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8)],
        Json(InlineCollection::new(&urls.featured_tags, items)),
    ))
}

/// `GET /users/{username}/moderators` — a local group's mod list, the
/// collection its actor document advertises as `attributedTo` (FEP-1b12).
/// Bare actor IRIs, owner first, exactly the shape Lemmy serves and
/// dereferences for communities. 404 for non-group accounts.
pub async fn get_moderators(
    State(state): State<AppState>,
    Path(username): Path<String>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let (account, urls) = local_group(&state, &username, &uri, &headers).await?;
    let items = elevated_accounts(&state, account.id)
        .await?
        .into_iter()
        .map(|(entry_account, _)| Value::String(actor_uri(&state.config.domain, &entry_account)))
        .collect();
    Ok((
        [(header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8)],
        Json(InlineOrderedCollection::new(&urls.moderators, items)),
    ))
}

/// `GET /users/{username}/affiliations` — the group's FEP-5219 affiliations
/// collection: `Relationship` items naming the owner (`admin`, the FEP's
/// ladder) and moderators. Plain members are the followers collection, and
/// bans are not published. 404 for non-group accounts.
pub async fn get_affiliations(
    State(state): State<AppState>,
    Path(username): Path<String>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let (account, urls) = local_group(&state, &username, &uri, &headers).await?;
    let items = elevated_accounts(&state, account.id)
        .await?
        .into_iter()
        .map(|(entry_account, affiliation)| {
            serde_json::json!({
                "type": "Relationship",
                "subject": actor_uri(&state.config.domain, &entry_account),
                "relationship": match affiliation.as_str() {
                    "owner" => "admin",
                    other => other,
                },
            })
        })
        .collect();
    Ok((
        [(header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8)],
        Json(InlineOrderedCollection::new(&urls.affiliations, items)),
    ))
}

/// Resolves a local **group** account for its mod-list collections; 404 for
/// people (the endpoints only exist on groups, like Lemmy's).
async fn local_group(
    state: &AppState,
    username: &str,
    uri: &Uri,
    headers: &HeaderMap,
) -> Result<(Account, LocalUserUrls), ApiError> {
    require_ap_accept(headers)?;
    let account = super::local_actor_account(state, username, uri).await?;
    ensure_available(state, &account).await?;
    if !account.is_group() {
        return Err(ApiError::NotFound);
    }
    let urls = LocalUserUrls::for_account(
        &state.config.domain,
        &account.username,
        account.uri.as_deref(),
    );
    Ok((account, urls))
}

/// The group's owner/moderator affiliation rows hydrated into accounts,
/// preserving the owner-first order.
async fn elevated_accounts(
    state: &AppState,
    group_account_id: i64,
) -> Result<Vec<(Account, String)>, ApiError> {
    let entries = plamenu_db::group::elevated(&state.pool, group_account_id).await?;
    let accounts = account::find_by_ids(
        &state.pool,
        &entries.iter().map(|e| e.account_id).collect::<Vec<_>>(),
    )
    .await?;
    Ok(entries
        .into_iter()
        .filter_map(|entry| {
            accounts
                .iter()
                .find(|a| a.id == entry.account_id)
                .map(|a| (a.clone(), entry.affiliation))
        })
        .collect())
}

/// Mastodon's `OutboxesController::LIMIT`: statuses per outbox page.
const OUTBOX_PER_PAGE: i64 = 20;

#[derive(Deserialize)]
pub struct OutboxQuery {
    page: Option<String>,
    max_id: Option<String>,
    min_id: Option<String>,
    since_id: Option<String>,
}

/// `GET /users/{username}/outbox` — the bare URL is an `OrderedCollection`
/// envelope whose `totalItems` other servers read as the account's status
/// count; `?page=true` inlines the distributable statuses as
/// `Create`/`Announce` activities, 20 per page, keyset-paginated by
/// `max_id`/`min_id`/`since_id`, like Mastodon's `OutboxesController`.
///
/// Pages are viewer-aware, like Mastodon's (`AccountStatusesFilter` keyed on
/// `signed_request_account`): the verified signer of the GET also receives
/// followers-only statuses if they follow the account, and statuses that
/// mention them; a signer the account blocks (or whose domain it personally
/// blocks) gets an empty page. Unsigned requests — and signatures that fail
/// to verify, when authorized fetch isn't gating them to a 401 first — are
/// served the anonymous public/unlisted slice.
pub async fn get_outbox(
    State(state): State<AppState>,
    Path(username): Path<String>,
    Query(query): Query<OutboxQuery>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    require_ap_accept(&headers)?;
    let account = super::local_actor_account(&state, &username, &uri).await?;
    ensure_available(&state, &account).await?;
    let urls = LocalUserUrls::for_account(
        &state.config.domain,
        &account.username,
        account.uri.as_deref(),
    );

    if !truthy(query.page.as_deref()) {
        let total = status::count_outbox_by_account(&state.pool, account.id).await?;
        let document = serde_json::to_value(OutboxCollection::new(&urls.outbox, total))
            .map_err(|e| ApiError::Internal(Box::new(e)))?;
        return Ok((
            [(header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8)],
            Json(document),
        )
            .into_response());
    }

    let viewer = signed_page_requester(&state, &uri, &headers).await;
    let blocked = match &viewer {
        Some(viewer) => {
            plamenu_db::block::exists(&state.pool, account.id, viewer.id).await?
                || match viewer
                    .domain
                    .as_deref()
                    .filter(|_| !viewer.is_portable_on(&state.config.domain))
                {
                    Some(domain) => {
                        plamenu_db::account_domain_block::exists(&state.pool, account.id, domain)
                            .await?
                    }
                    None => false,
                }
        }
        None => false,
    };

    let max_id = query.max_id.as_deref().and_then(|raw| raw.parse().ok());
    let min_id = query.min_id.as_deref().and_then(|raw| raw.parse().ok());
    let since_id = query.since_id.as_deref().and_then(|raw| raw.parse().ok());
    let statuses = if blocked {
        Vec::new()
    } else {
        status::outbox_page(
            &state.pool,
            account.id,
            viewer.as_ref().map(|v| v.id),
            max_id,
            since_id,
            min_id,
            OUTBOX_PER_PAGE,
        )
        .await?
    };

    let items = outbox_items(&state, &account, &statuses).await?;

    let full_page = i64::try_from(statuses.len()).unwrap_or(i64::MAX) == OUTBOX_PER_PAGE;
    let next =
        full_page.then(|| outbox_page_url(&urls.outbox, statuses.last().map(|s| s.id), None));
    let prev = statuses
        .first()
        .map(|s| outbox_page_url(&urls.outbox, None, Some(s.id)));
    // The page id echoes the request's pagination params, like Rails.
    let page_id = outbox_page_url(&urls.outbox, max_id, min_id);
    let page = OutboxPage::new(
        // Inlined Notes may carry quote/sensitive terms; ship the full
        // context, like the featured collection does.
        activity::quote_context(),
        page_id,
        &urls.outbox,
        next,
        prev,
        items,
    );
    let document = serde_json::to_value(page).map_err(|e| ApiError::Internal(Box::new(e)))?;
    Ok((
        // Page content depends on who signed the request.
        [
            (header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8),
            (header::VARY, "Signature"),
        ],
        Json(document),
    )
        .into_response())
}

/// The verified signer of a collection-page GET, if any — Mastodon's
/// `signed_request_account`: `None` for an unsigned request, and for a
/// signature that fails verification (under authorized fetch an invalid
/// signature never gets this far — the gate middleware already 401'd it).
async fn signed_page_requester(
    state: &AppState,
    uri: &Uri,
    headers: &HeaderMap,
) -> Option<Account> {
    if !headers.contains_key("signature") {
        return None;
    }
    match crate::signed_fetch::verified_get_sender(state, uri, headers).await {
        Ok(sender) => Some(sender),
        Err(err) => {
            tracing::debug!("ignoring unverifiable signature on collection GET: {err}");
            None
        }
    }
}

/// Renders a page of outbox rows: a boost becomes its `Announce`, anything
/// else the `Create` around the full Note (no per-item `@context` — the page
/// carries it), like Mastodon's `OutboxSerializer`.
///
/// Everything the page needs loads once for the page: the Note sidecars
/// through [`NoteBatch`], and the boost targets and their authors through one
/// `find_by_ids` apiece.
pub(crate) async fn outbox_items(
    state: &AppState,
    account: &Account,
    items: &[plamenu_db::status::Status],
) -> Result<Vec<Value>, ApiError> {
    let domain = &state.config.domain;
    let (boosts, originals): (Vec<_>, Vec<_>) =
        items.iter().partition(|item| item.reblog_of_id.is_some());
    let batch = NoteBatch::load(state, &originals).await?;

    // A page with no boost on it must not pay for the empty lookups: neither
    // `find_by_ids` short-circuits on an empty slice.
    let target_ids: Vec<i64> = boosts.iter().filter_map(|item| item.reblog_of_id).collect();
    let (targets, target_authors) = if target_ids.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        let targets = status::find_by_ids(&state.pool, &target_ids).await?;
        let authors = account::find_by_ids(
            &state.pool,
            &targets
                .iter()
                .map(|target| target.account_id)
                .collect::<Vec<_>>(),
        )
        .await?;
        (targets, authors)
    };

    let mut values = Vec::with_capacity(items.len());
    for item in items {
        let Some(reblog_of_id) = item.reblog_of_id else {
            let note = crate::note::note_in_batch(state, item, account, &batch)?;
            values.push(activity::create_with_note(domain, &account.username, note));
            continue;
        };
        let target = targets
            .iter()
            .find(|target| target.id == reblog_of_id)
            .ok_or(ApiError::NotFound)?;
        let target_author = target_authors
            .iter()
            .find(|a| a.id == target.account_id)
            .ok_or(ApiError::NotFound)?;
        let published = item
            .created_at
            .format(&Rfc3339)
            .map_err(|e| ApiError::Internal(Box::new(e)))?;
        let mut announce = activity::announce(
            domain,
            &account.username,
            item.id,
            &status_uri_for_account(domain, target, target_author),
            &published,
        );
        if let Some(fields) = announce.as_object_mut() {
            fields.remove("@context");
        }
        values.push(announce);
    }
    Ok(values)
}

#[derive(Deserialize)]
pub struct CollectionQuery {
    page: Option<String>,
}

/// Which follow collection of an actor is being served.
#[derive(Clone, Copy)]
enum FollowCollection {
    Followers,
    Following,
}

/// `GET /users/{username}/followers`.
pub async fn get_followers(
    state: State<AppState>,
    path: Path<String>,
    query: Query<CollectionQuery>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    follow_collection(
        state,
        path,
        query,
        uri,
        headers,
        FollowCollection::Followers,
    )
    .await
}

/// `GET /users/{username}/following`.
pub async fn get_following(
    state: State<AppState>,
    path: Path<String>,
    query: Query<CollectionQuery>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    follow_collection(
        state,
        path,
        query,
        uri,
        headers,
        FollowCollection::Following,
    )
    .await
}

/// `GET /users/{username}/followers_synchronization` — Mastodon's signed,
/// origin-scoped partial followers collection for FEP-8fcf.
pub async fn get_followers_synchronization(
    State(state): State<AppState>,
    uri: Uri,
    Path(username): Path<String>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    require_ap_accept(&headers)?;
    let account = super::local_actor_account(&state, &username, &uri).await?;
    ensure_available(&state, &account).await?;
    let requester = crate::signed_fetch::verified_get_sender(&state, &uri, &headers).await?;
    crate::followers_sync::partial_collection_response(&state, &account, &requester).await
}

/// Serves a followers/following collection the way Mastodon does: the bare
/// URL is an `OrderedCollection` envelope, `?page=N` inlines actor IRIs,
/// newest follow first.
async fn follow_collection(
    State(state): State<AppState>,
    Path(username): Path<String>,
    Query(query): Query<CollectionQuery>,
    uri: Uri,
    headers: HeaderMap,
    which: FollowCollection,
) -> Result<impl IntoResponse, ApiError> {
    require_ap_accept(&headers)?;
    let account = super::local_actor_account(&state, &username, &uri).await?;
    ensure_available(&state, &account).await?;
    // `hideCollections`: the bare collection (with `totalItems`) is still served,
    // but the item pages are forbidden, matching Mastodon's
    // `protect_hidden_collections`.
    if query.page.is_some() && account.hide_collections {
        return Err(ApiError::Forbidden("Collection is hidden".to_owned()));
    }
    let urls = LocalUserUrls::for_account(
        &state.config.domain,
        &account.username,
        account.uri.as_deref(),
    );
    let (collection_url, total) = match which {
        FollowCollection::Followers => (
            urls.followers,
            follow::count_followers(&state.pool, account.id).await?,
        ),
        FollowCollection::Following => (
            urls.following,
            follow::count_following(&state.pool, account.id).await?,
        ),
    };

    let document: Value = match query.page {
        None => serde_json::to_value(OrderedCollection::new(&collection_url, total))
            .map_err(|e| ApiError::Internal(Box::new(e)))?,
        Some(raw) => {
            // Out-of-range page values clamp to 1, like Kaminari's `page`.
            let page = raw.parse::<u64>().ok().filter(|p| *p >= 1).unwrap_or(1);
            let offset = (page - 1).saturating_mul(ITEMS_PER_PAGE);
            let limit = i64::try_from(ITEMS_PER_PAGE).unwrap_or(i64::MAX);
            // Federation collection: no per-request viewer, so pass the owner
            // as the viewer to keep member-level `hide_collections` filtering
            // off here. Whole-collection hiding is already handled above by the
            // `protect_hidden_collections` check.
            let viewer = Some(account.id);
            let account_ids = match which {
                FollowCollection::Followers => {
                    follow::followers_page(
                        &state.pool,
                        account.id,
                        i64::try_from(offset).unwrap_or(i64::MAX),
                        limit,
                        viewer,
                    )
                    .await?
                }
                FollowCollection::Following => {
                    follow::following_page(
                        &state.pool,
                        account.id,
                        i64::try_from(offset).unwrap_or(i64::MAX),
                        limit,
                        viewer,
                    )
                    .await?
                }
            };
            // One lookup for the page, like `elevated_accounts` above; the
            // page's own order is the one `followers_page` returned, not the
            // arbitrary one `find_by_ids` gives back.
            let entries = account::find_by_ids(&state.pool, &account_ids).await?;
            let mut items = Vec::with_capacity(account_ids.len());
            for account_id in account_ids {
                let entry = entries
                    .iter()
                    .find(|entry| entry.id == account_id)
                    .ok_or(ApiError::NotFound)?;
                items.push(actor_uri(&state.config.domain, entry));
            }
            serde_json::to_value(OrderedCollectionPage::new(
                &collection_url,
                page,
                total,
                items,
            ))
            .map_err(|e| ApiError::Internal(Box::new(e)))?
        }
    };
    Ok((
        [(header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8)],
        Json(document),
    ))
}

/// Mastodon's availability ladder for a local actor's collections: a deleted
/// account (permanently unavailable) is `410 Gone`, a reversibly suspended
/// one `403 Forbidden` (`ActivityPub::BaseController`'s
/// `temporary_suspension_response`). The actor document itself is served
/// blanked instead — see [`get_actor`].
async fn ensure_available(state: &AppState, account: &Account) -> Result<(), ApiError> {
    if !account.suspended() {
        return Ok(());
    }
    if crate::moderation::permanently_unavailable(&state.pool, account).await? {
        Err(ApiError::Gone)
    } else {
        Err(ApiError::Forbidden("This account is suspended".into()))
    }
}

/// The `ActivityPub` id of an account (stored for remote, derived for local).
fn actor_uri(domain: &str, account: &Account) -> String {
    account.uri.clone().unwrap_or_else(|| {
        LocalUserUrls::for_account(domain, &account.username, account.uri.as_deref()).id
    })
}
