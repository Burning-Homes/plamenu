//! Account collections (Mastodon 4.6 / FEP-7aa9 "Featured Collections") in the
//! first-party web UI — the client half of [`plamenu_db::collection`] and
//! [`crate::routes::account_collections`]. A signed-in account can create up to
//! [`collection::PER_ACCOUNT_LIMIT`] collections, each a curated set of up to
//! [`collection::MAX_ITEMS`] accounts, optionally tied to a hashtag; edit or
//! delete them; and manage their membership. Membership rides the same
//! featureability policy the REST API enforces (a member must be discoverable
//! and either unlocked, followed, or oneself — remote members go through the
//! FEP-7aa9 consent handshake).
//!
//! Management lives under `/settings/collections` (the "profile options" tab
//! strip) so it sits beside featured hashtags and the other profile surfaces.
//! A collection's public face is `/@name/collections/{id}`, linked from the
//! profile's "Collections" section. The profile overflow menu reaches
//! `/web/accounts/{id}/collections`, a checkbox panel for adding or removing an
//! account across every owned collection at once.
//!
//! Everything works with JavaScript disabled: links navigate, forms POST, and
//! each write follows POST/Redirect/GET back to where it came from.
//! State-changing endpoints live under `/web/…` so they never collide with a
//! read URL.

use std::collections::HashSet;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use fluent_bundle::FluentArgs;
use maud::{Markup, html};
use plamenu_db::account::{self, Account};
use plamenu_db::collection::{self, Collection};
use serde::Deserialize;
use serde_json::Value;

use super::actions::safe_return;
use super::i18n::Locale;
use super::pages::{ProfileHandleResolution, not_found, resolve_handle, resolve_profile_handle};
use super::session::{MaybeWebUser, WebUser, csrf_rejection};
use super::settings::{
    bad_form, checkbox, checked, error_flash, field, form_pairs, redirect_to, saved_flash,
    settings_shell,
};
use super::{layout, view};
use crate::collections as service;
use crate::entities::render_accounts_by_ids;
use crate::error::ApiError;
use crate::state::AppState;

/// The settings tab the collection pages highlight (the sub-pages aren't their
/// own sections, so they all light up "Collections").
const SETTINGS_TAB: &str = "/settings/collections";

#[derive(Deserialize)]
pub struct CollectionsQuery {
    saved: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
pub struct ManageQuery {
    return_to: Option<String>,
    error: Option<String>,
}

// ---- Owned-collection lookups ------------------------------------------

/// A collection the signed-in user owns, or the styled 404 page (never the
/// API's JSON error body). Mirrors [`super::lists::owned_list`].
async fn owned_collection(
    state: &AppState,
    user: &WebUser,
    collection_id: i64,
) -> Result<Collection, Response> {
    match collection::find(&state.pool, collection_id).await {
        Ok(Some(coll)) if coll.account_id == user.current.account.id => Ok(coll),
        Ok(_) => Err(not_found(state, Some(user), user.locale).await),
        Err(err) => Err(ApiError::from(err).into_response()),
    }
}

// ---- Index (list + create) ---------------------------------------------

/// `GET /settings/collections` — every collection the viewer owns, each with
/// its item count and a manage link, plus a link to the dedicated creation
/// page.
pub async fn index(
    State(state): State<AppState>,
    user: WebUser,
    Query(query): Query<CollectionsQuery>,
) -> Response {
    let owner_id = user.current.account.id;
    // Owner sees all their collections (discoverable or not).
    let collections = match collection::owned_by(
        &state.pool,
        owner_id,
        false,
        0,
        collection::PER_ACCOUNT_LIMIT,
    )
    .await
    {
        Ok(collections) => collections,
        Err(err) => return ApiError::from(err).into_response(),
    };
    // One grouped count for every owned collection instead of a COUNT per row.
    let collection_ids: Vec<i64> = collections.iter().map(|c| c.id).collect();
    let counts_by_id = match collection::count_active_items_many(&state.pool, &collection_ids).await
    {
        Ok(counts) => counts,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let counts: Vec<i64> = collections
        .iter()
        .map(|coll| counts_by_id.get(&coll.id).copied().unwrap_or(0))
        .collect();
    let at_limit =
        i64::try_from(collections.len()).unwrap_or(i64::MAX) >= collection::PER_ACCOUNT_LIMIT;
    // The other half: collections (local or remote) that feature *me*, which I
    // can leave — Mastodon's "Featuring you" tab.
    let featuring = match collections_featuring(&state, owner_id).await {
        Ok(featuring) => featuring,
        Err(err) => return err.into_response(),
    };
    let discoverable = user.current.account.discoverable.unwrap_or(false);
    let locale = user.locale;
    let body = html! {
        (saved_flash(query.saved.is_some(), &locale.text("collections-saved")))
        (error_flash(query.error.as_deref()))
        p.settings-field__hint { (locale.text("collections-intro")) }

        h3 { (locale.text("collections-yours")) }
        @if at_limit {
            p.settings__error role="alert" { (limit_reached(locale)) }
        } @else {
            p {
                a.pill-button href="/settings/collections/new" {
                    (view::icon("collection")) " " (locale.text("collections-create"))
                }
            }
        }

        @if collections.is_empty() {
            p.empty { (locale.text("collections-empty")) }
        } @else {
            ul.list-index {
                @for (coll, count) in collections.iter().zip(&counts) {
                    li.list-index__item {
                        a.list-index__link href=(format!("/settings/collections/{}", coll.id)) {
                            (view::icon("collection"))
                            span { (coll.name) }
                        }
                        span.list-index__manage { (item_count(*count, locale)) }
                        a.list-index__manage href=(format!("/settings/collections/{}", coll.id)) {
                            (locale.text("collections-manage"))
                        }
                    }
                }
            }
        }

        h3 { (locale.text("collections-featuring-title")) }
        @if featuring.is_empty() {
            p.empty {
                (locale.text("collections-featuring-empty"))
                @if !discoverable {
                    " "
                    (locale.markup("collections-featuring-discovery", &[(
                        "privacy",
                        html! {
                            a href="/settings/privacy" {
                                (locale.text("settings-section-privacy"))
                            }
                        },
                    )]))
                }
            }
        } @else {
            ul.list-index {
                @for entry in &featuring {
                    (featuring_row(entry, &user.csrf, locale))
                }
            }
        }
    };
    settings_shell(
        &user,
        SETTINGS_TAB,
        &locale.text("collections-title"),
        &body,
    )
    .into_response()
}

/// "You've reached the maximum of N collections", pluralized on the cap.
fn limit_reached(locale: Locale) -> String {
    let mut args = FluentArgs::new();
    args.set("limit", collection::PER_ACCOUNT_LIMIT);
    locale.text_with("collections-limit-reached", &args)
}

/// "3/25 accounts" for a collection's row in the index.
fn item_count(count: i64, locale: Locale) -> String {
    let mut args = FluentArgs::new();
    args.set("count", count);
    args.set("limit", collection::MAX_ITEMS);
    locale.text_with("collections-item-count", &args)
}

/// A collection (owned by someone else) that features the current user, with
/// the owner's handle and public path for the list row.
struct Featuring {
    collection: Collection,
    owner_handle: String,
    owner_path: String,
}

/// The collections featuring `account_id` (Mastodon's `in_collections`),
/// resolved with each owner's handle. Rows whose owner has vanished are
/// dropped.
async fn collections_featuring(
    state: &AppState,
    account_id: i64,
) -> Result<Vec<Featuring>, ApiError> {
    // `containing` is offset-paginated; one generous page covers every
    // collection an account can be a member of.
    let limit = collection::PER_ACCOUNT_LIMIT * collection::MAX_ITEMS;
    let collections = collection::containing(&state.pool, account_id, 0, limit).await?;
    // Every distinct owner in one query — two collections by the same owner
    // used to cost two identical lookups.
    let mut owner_ids: Vec<i64> = collections.iter().map(|c| c.account_id).collect();
    owner_ids.sort_unstable();
    owner_ids.dedup();
    let owners: std::collections::HashMap<i64, plamenu_db::account::Account> =
        account::find_by_ids(&state.pool, &owner_ids)
            .await?
            .into_iter()
            .map(|a| (a.id, a))
            .collect();
    let mut entries = Vec::with_capacity(collections.len());
    for collection in collections {
        let Some(owner) = owners.get(&collection.account_id) else {
            continue;
        };
        entries.push(Featuring {
            collection,
            owner_handle: account_handle(&state.config.domain, owner),
            owner_path: account_profile_path(&state.config.domain, owner),
        });
    }
    Ok(entries)
}

/// One "featuring you" row: a link to the collection's public page, the owner's
/// handle, and a "Remove me" button that revokes the membership.
fn featuring_row(entry: &Featuring, csrf: &str, locale: Locale) -> Markup {
    let public = format!("{}/collections/{}", entry.owner_path, entry.collection.id);
    let leave_action = format!("/web/collections/{}/leave", entry.collection.id);
    let leave_message = locale.text("collections-leave-confirm");
    let mut args = FluentArgs::new();
    args.set("handle", entry.owner_handle.as_str());
    html! {
        li.list-index__item {
            a.list-index__link href=(public) {
                (view::icon("collection"))
                span { (entry.collection.name) }
            }
            span.list-index__manage { (locale.text_with("collections-owner", &args)) }
            form.settings-form method="post" action=(view::CONFIRM_PATH)
                data-confirm=(&leave_message) data-confirm-action=(&leave_action) {
                input type="hidden" name="csrf" value=(csrf);
                input type="hidden" name="return_to" value="/settings/collections";
                (view::confirmation_fields(&leave_action, Some(&leave_message)))
                button.settings-button--danger type="submit" {
                    (locale.text("collections-leave"))
                }
            }
        }
    }
}

/// `POST /web/collections/{id}/leave` — the current user removes themselves from
/// a collection that features them (the featured account's revoke, not the
/// owner's removal). Works for a remote collection too, sending the
/// `Delete(FeatureAuthorization)` upstream.
pub async fn leave_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(collection_id): Path<i64>,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let me = user.current.account.id;
    let item = match collection::find_item_by_account(&state.pool, collection_id, me).await {
        Ok(Some(item)) => item,
        Ok(None) => return not_found(&state, Some(&user), user.locale).await,
        Err(err) => return ApiError::from(err).into_response(),
    };
    // Only a live membership can be revoked; a stale POST is a harmless no-op.
    if !matches!(item.state.as_str(), "pending" | "accepted") {
        return redirect_to("/settings/collections");
    }
    match service::revoke_item(&state, &user.current.account, &item).await {
        Ok(()) => redirect_to("/settings/collections?saved=1"),
        Err(err) => err.into_response(),
    }
}

/// The create/edit attribute fields — name, description, optional hashtag, and
/// the sensitive/discoverable flags — shared by the create form and the manage
/// page. `existing` pre-fills for an edit.
fn collection_fieldset(legend: &str, existing: Option<&Collection>, locale: Locale) -> Markup {
    let name = existing.map(|c| c.name.as_str()).unwrap_or_default();
    let description = existing.map(|c| c.description.as_str()).unwrap_or_default();
    let sensitive = existing.is_some_and(|c| c.sensitive);
    let discoverable = existing.is_some_and(|c| c.discoverable);
    html! {
        fieldset.settings-form__group {
            legend { (legend) }
            label.settings-field {
                span.settings-field__label { (locale.text("collections-name")) }
                input type="text" name="name" value=(name)
                    maxlength=(collection::NAME_LENGTH_LIMIT.to_string())
                    autocomplete="off" required;
            }
            label.settings-field {
                span.settings-field__label { (locale.text("collections-description")) }
                textarea name="description" rows="2"
                    maxlength=(collection::DESCRIPTION_LENGTH_LIMIT.to_string()) { (description) }
            }
            (checkbox("discoverable", &locale.text("collections-discoverable"),
                &locale.text("collections-discoverable-hint"), discoverable))
            (checkbox("sensitive", &locale.text("collections-sensitive"),
                &locale.text("collections-sensitive-hint"), sensitive))
        }
    }
}

/// `GET /settings/collections/new` — the dedicated collection-creation page.
pub async fn new_page(
    State(state): State<AppState>,
    user: WebUser,
    Query(query): Query<CollectionsQuery>,
) -> Response {
    let owned = match collection::owned_by(
        &state.pool,
        user.current.account.id,
        false,
        0,
        collection::PER_ACCOUNT_LIMIT,
    )
    .await
    {
        Ok(collections) => collections,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let at_limit = i64::try_from(owned.len()).unwrap_or(i64::MAX) >= collection::PER_ACCOUNT_LIMIT;
    let locale = user.locale;
    let body = html! {
        (error_flash(query.error.as_deref()))
        @if at_limit {
            p.settings__error role="alert" { (limit_reached(locale)) }
            p { a href="/settings/collections" { (locale.text("collections-back")) } }
        } @else {
            form.settings-form method="post" action="/web/settings/collections" {
                input type="hidden" name="csrf" value=(user.csrf);
                (collection_fieldset(&locale.text("collections-new"), None, locale))
                div.settings-form__actions {
                    button type="submit" { (locale.text("collections-create")) }
                    a.settings-button--plain href="/settings/collections" {
                        (locale.text("filters-cancel"))
                    }
                }
            }
        }
    };
    settings_shell(&user, SETTINGS_TAB, &locale.text("collections-new"), &body).into_response()
}

/// `POST /web/settings/collections` — create a collection, then land on it.
pub async fn create_action(State(state): State<AppState>, user: WebUser, body: Bytes) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let params = collection_params(&pairs);
    match service::create_collection(&state, &user.current.account, params, &[]).await {
        Ok(created) => redirect_to(&format!("/settings/collections/{}", created.id)),
        Err(err) => redirect_error(
            "/settings/collections/new",
            &failure_message(err, user.locale),
        ),
    }
}

/// Reads the shared attribute fields off a submitted form into service params.
fn collection_params(pairs: &[(String, String)]) -> service::CollectionParams {
    service::CollectionParams {
        name: field(pairs, "name").unwrap_or_default().trim().to_owned(),
        description: field(pairs, "description").unwrap_or_default().to_owned(),
        language: None,
        sensitive: checked(pairs, "sensitive"),
        discoverable: checked(pairs, "discoverable"),
        tag_name: None,
    }
}

// ---- Manage one collection (edit / delete / members) -------------------

/// `GET /settings/collections/{id}` — edit the collection's attributes, manage
/// its members (add by handle, remove), or delete it.
pub async fn manage_page(
    State(state): State<AppState>,
    user: WebUser,
    Path(collection_id): Path<i64>,
    Query(query): Query<CollectionsQuery>,
) -> Response {
    let collection = match owned_collection(&state, &user, collection_id).await {
        Ok(collection) => collection,
        Err(response) => return response,
    };
    let owner_id = user.current.account.id;
    // The owner sees pending members too (Mastodon's `items_for`).
    let items =
        match collection::items_for(&state.pool, collection.id, owner_id, Some(owner_id)).await {
            Ok(items) => items,
            Err(err) => return ApiError::from(err).into_response(),
        };
    let members = match render_items(&state, &items, owner_id).await {
        Ok(members) => members,
        Err(response) => return response,
    };
    let at_item_limit = i64::try_from(items.len()).unwrap_or(i64::MAX) >= collection::MAX_ITEMS;
    let public_url = format!(
        "/@{}/collections/{}",
        user.current.account.username, collection.id
    );
    let locale = user.locale;
    let mut full_args = FluentArgs::new();
    full_args.set("limit", collection::MAX_ITEMS);
    let body = html! {
        (saved_flash(query.saved.is_some(), &locale.text("collections-updated")))
        (error_flash(query.error.as_deref()))
        p.settings-field__hint {
            a href=(public_url) { (locale.text("collections-view-public")) } "."
        }

        form.settings-form method="post"
            action=(format!("/web/settings/collections/{}", collection.id)) {
            input type="hidden" name="csrf" value=(user.csrf);
            (collection_fieldset(&locale.text("collections-settings"), Some(&collection), locale))
            div.settings-form__actions {
                button type="submit" { (locale.text("settings-profile-save")) }
            }
        }

        section.settings-form__group {
            h3 { (locale.text("collections-members")) }
            @if at_item_limit {
                p.settings__error role="alert" {
                    (locale.text_with("collections-full", &full_args))
                }
            } @else {
                form.settings-form method="post"
                    action=(format!("/web/settings/collections/{}/members/add", collection.id)) {
                    input type="hidden" name="csrf" value=(user.csrf);
                    fieldset.settings-form__group {
                        legend { (locale.text("collections-add-member")) }
                        p.settings-field__hint {
                            (locale.markup("collections-handle-hint", &[
                                ("local", html! { code { "@alice" } }),
                                ("remote", html! { code { "bob@example.social" } }),
                            ]))
                        }
                        label.settings-field {
                            span.settings-field__label { (locale.text("collections-handle")) }
                            input type="text" name="handle" placeholder="@user@domain"
                                autocomplete="off" required;
                        }
                    }
                    div.settings-form__actions {
                        button type="submit" { (locale.text("collections-add-submit")) }
                    }
                }
            }

            @if members.is_empty() {
                p.empty { (locale.text("collections-members-empty")) }
            } @else {
                ul.relationships-list {
                    @for member in &members {
                        (member_row(collection.id, member, &user.csrf, locale))
                    }
                }
            }
        }

        @let delete_action = format!("/web/settings/collections/{}/delete", collection.id);
        @let delete_message = locale.text("collections-delete-confirm");
        form.settings-form method="post" action=(view::CONFIRM_PATH)
            data-confirm=(&delete_message) data-confirm-action=(&delete_action) {
            input type="hidden" name="csrf" value=(user.csrf);
            input type="hidden" name="return_to" value=(format!("/settings/collections/{}", collection.id));
            (view::confirmation_fields(&delete_action, Some(&delete_message)))
            div.settings-form__actions {
                button.settings-button--danger type="submit" {
                    (locale.text("collections-delete"))
                }
            }
        }
    };
    settings_shell(&user, SETTINGS_TAB, &collection.name, &body).into_response()
}

/// One member as rendered for the owner's management list: its account card (or
/// a placeholder for an unresolved remote actor), a pending badge, and a Remove
/// button keyed on the membership id.
struct Member {
    item_id: i64,
    state: String,
    /// The rendered account entity, or `None` for an unresolved remote member.
    account: Option<Value>,
    /// The featured actor's URI, shown when the account isn't resolved yet.
    object_uri: Option<String>,
}

/// Renders each membership's featured account (owner view), preserving order.
async fn render_items(
    state: &AppState,
    items: &[collection::CollectionItem],
    viewer_id: i64,
) -> Result<Vec<Member>, Response> {
    let ids: Vec<i64> = items.iter().filter_map(|item| item.account_id).collect();
    let accounts = match render_accounts_by_ids(
        &state.pool,
        &state.config.domain,
        &ids,
        Some(viewer_id),
    )
    .await
    {
        Ok(accounts) => accounts,
        Err(err) => return Err(err.into_response()),
    };
    // `render_accounts_by_ids` preserves `ids` order, so walk the two in step.
    let mut rendered = accounts.into_iter();
    let mut members = Vec::with_capacity(items.len());
    for item in items {
        // Only advance the rendered-account cursor for items that had an id
        // (the same filter that built `ids`), keeping the two in step.
        let account = if item.account_id.is_some() {
            rendered.next()
        } else {
            None
        };
        members.push(Member {
            item_id: item.id,
            state: item.state.clone(),
            account,
            object_uri: item.object_uri.clone(),
        });
    }
    Ok(members)
}

/// One member row in the owner's management list.
fn member_row(collection_id: i64, member: &Member, csrf: &str, locale: Locale) -> Markup {
    let unresolved = locale.text("collections-unresolved");
    html! {
        li.relationships-list__item {
            @match &member.account {
                Some(value) => (view::account_card(&view::Account(value))),
                None => div.relationships-list__unresolved {
                    (member.object_uri.as_deref().unwrap_or(&unresolved))
                }
            }
            div.relationships-list__aside {
                @if member.state == "pending" {
                    span.profile__badge title=(locale.text("collections-pending-hint")) {
                        (locale.text("collections-pending"))
                    }
                }
                form method="post"
                    action=(format!("/web/settings/collections/{collection_id}/members/remove")) {
                    input type="hidden" name="csrf" value=(csrf);
                    input type="hidden" name="item_id" value=(member.item_id);
                    button.settings-button--danger type="submit" {
                        (locale.text("collections-remove-member"))
                    }
                }
            }
        }
    }
}

/// `POST /web/settings/collections/{id}` — save edited attributes.
pub async fn update_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(collection_id): Path<i64>,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let collection = match owned_collection(&state, &user, collection_id).await {
        Ok(collection) => collection,
        Err(response) => return response,
    };
    let params = collection_params(&pairs);
    let back = format!("/settings/collections/{collection_id}");
    match service::update_collection(&state, &collection, &user.current.account, params).await {
        Ok(_) => redirect_to(&format!("{back}?saved=1")),
        Err(err) => redirect_error(&back, &failure_message(err, user.locale)),
    }
}

/// `POST /web/settings/collections/{id}/delete`.
pub async fn delete_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(collection_id): Path<i64>,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let collection = match owned_collection(&state, &user, collection_id).await {
        Ok(collection) => collection,
        Err(response) => return response,
    };
    match service::delete_collection(&state, &collection, &user.current.account).await {
        Ok(()) => redirect_to("/settings/collections?saved=1"),
        Err(err) => err.into_response(),
    }
}

/// `POST /web/settings/collections/{id}/members/add` — resolve the typed handle
/// and feature it, or bounce back with a readable error.
pub async fn add_member_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(collection_id): Path<i64>,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let collection = match owned_collection(&state, &user, collection_id).await {
        Ok(collection) => collection,
        Err(response) => return response,
    };
    let back = format!("/settings/collections/{collection_id}");
    let raw = field(&pairs, "handle").unwrap_or_default().trim();
    let account = match resolve_handle(&state, &normalize_handle(raw)).await {
        Ok(Some(account)) => account,
        Ok(None) => {
            return redirect_error(&back, &user.locale.text("collections-error-no-account"));
        }
        Err(err) => return err.into_response(),
    };
    match service::add_account(&state, &collection, &user.current.account, &account).await {
        Ok(_) => redirect_to(&format!("{back}?saved=1")),
        Err(err) => redirect_error(&back, &failure_message(err, user.locale)),
    }
}

/// `POST /web/settings/collections/{id}/members/remove`.
pub async fn remove_member_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(collection_id): Path<i64>,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let collection = match owned_collection(&state, &user, collection_id).await {
        Ok(collection) => collection,
        Err(response) => return response,
    };
    let back = format!("/settings/collections/{collection_id}");
    let item_id: i64 = field(&pairs, "item_id")
        .and_then(|s| s.parse().ok())
        .unwrap_or_default();
    let item = match collection::find_item(&state.pool, collection.id, item_id).await {
        Ok(Some(item)) => item,
        Ok(None) => return not_found(&state, Some(&user), user.locale).await,
        Err(err) => return ApiError::from(err).into_response(),
    };
    match service::delete_item(&state, &collection, &user.current.account, &item).await {
        Ok(()) => redirect_to(&format!("{back}?saved=1")),
        Err(err) => err.into_response(),
    }
}

// ---- Manage an account across all collections (from its profile) -------

/// `GET /web/accounts/{id}/collections` — the "add to collections" panel
/// reached from a profile's overflow menu: a checkbox per owned collection,
/// ticked where the account is already featured. Submitting reconciles the set.
pub async fn account_collections_form(
    State(state): State<AppState>,
    user: WebUser,
    Path(account_id): Path<i64>,
    Query(query): Query<ManageQuery>,
) -> Response {
    let account = match account::find_by_id(&state.pool, account_id).await {
        Ok(Some(account)) => account,
        Ok(None) => return not_found(&state, Some(&user), user.locale).await,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let owner_id = user.current.account.id;
    let collections = match collection::owned_by(
        &state.pool,
        owner_id,
        false,
        0,
        collection::PER_ACCOUNT_LIMIT,
    )
    .await
    {
        Ok(collections) => collections,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let member_of = match member_collection_ids(&state, account_id).await {
        Ok(ids) => ids,
        Err(err) => return err.into_response(),
    };
    let handle = account_handle(&state.config.domain, &account);
    let back = safe_return(
        query.return_to.as_deref(),
        &account_profile_path(&state.config.domain, &account),
    );
    let locale = user.locale;
    let mut heading_args = FluentArgs::new();
    heading_args.set("handle", handle.as_str());
    let body = html! {
        section.column {
            h1 { (locale.text_with("collections-feature-account", &heading_args)) }
            (error_flash(query.error.as_deref()))
            p.settings-field__hint { (locale.text("collections-feature-hint")) }
            @if collections.is_empty() {
                p.empty {
                    (locale.text("collections-empty")) " "
                    (locale.markup("collections-create-first", &[(
                        "create",
                        html! {
                            a href="/settings/collections" {
                                (locale.text("collections-create-one"))
                            }
                        },
                    )]))
                }
            } @else {
                form.settings-form method="post"
                    action=(format!("/web/accounts/{account_id}/collections")) {
                    input type="hidden" name="csrf" value=(user.csrf);
                    input type="hidden" name="return_to" value=(back);
                    div.list-membership {
                        @for coll in &collections {
                            label.settings-toggle {
                                input type="checkbox" name="collection_ids" value=(coll.id)
                                    checked[member_of.contains(&coll.id)];
                                span.settings-toggle__text {
                                    span.settings-toggle__label { (coll.name) }
                                }
                            }
                        }
                    }
                    div.settings-form__actions {
                        button type="submit" { (locale.text("common-save")) }
                        a.settings-button--plain href=(back) { (locale.text("common-back")) }
                    }
                }
            }
        }
    };
    layout::shell(
        &locale.text("profile-feature-collections"),
        Some(&user),
        &body,
    )
    .into_response()
}

/// `POST /web/accounts/{id}/collections` — reconcile the account's membership
/// against the ticked collections: feature it in newly-checked ones, remove it
/// from unchecked ones. A featureability failure on any add stops and reports.
pub async fn account_collections_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(account_id): Path<i64>,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let account = match account::find_by_id(&state.pool, account_id).await {
        Ok(Some(account)) => account,
        Ok(None) => return not_found(&state, Some(&user), user.locale).await,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let owner_id = user.current.account.id;
    let self_back = safe_return(
        field(&pairs, "return_to"),
        &account_profile_path(&state.config.domain, &account),
    );
    let error_back = format!("/web/accounts/{account_id}/collections");

    let owned: Vec<Collection> = match collection::owned_by(
        &state.pool,
        owner_id,
        false,
        0,
        collection::PER_ACCOUNT_LIMIT,
    )
    .await
    {
        Ok(collections) => collections,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let checked: HashSet<i64> = pairs
        .iter()
        .filter(|(key, _)| key == "collection_ids")
        .filter_map(|(_, value)| value.parse().ok())
        .filter(|id| owned.iter().any(|c| c.id == *id))
        .collect();
    let member_of = match member_collection_ids(&state, account_id).await {
        Ok(ids) => ids,
        Err(err) => return err.into_response(),
    };

    for collection in &owned {
        let want = checked.contains(&collection.id);
        let have = member_of.contains(&collection.id);
        let result = if want && !have {
            service::add_account(&state, collection, &user.current.account, &account)
                .await
                .map(|_| ())
        } else if !want && have {
            remove_membership(&state, collection, &user.current.account, account_id)
                .await
                .map_err(service::Failure::from)
        } else {
            continue;
        };
        if let Err(err) = result {
            let query = serde_urlencoded::to_string([
                ("return_to", self_back.as_str()),
                ("error", failure_message(err, user.locale).as_str()),
            ])
            .unwrap_or_default();
            return redirect_to(&format!("{error_back}?{query}"));
        }
    }
    redirect_to(&self_back)
}

/// Removes `account_id` from a collection by finding its membership, then
/// distributing the owner-initiated removal. A no-op when there is no row.
async fn remove_membership(
    state: &AppState,
    collection: &Collection,
    owner: &Account,
    account_id: i64,
) -> Result<(), ApiError> {
    if let Some(item) =
        collection::find_item_by_account(&state.pool, collection.id, account_id).await?
    {
        service::delete_item(state, collection, owner, &item).await?;
    }
    Ok(())
}

/// The set of collection ids `account_id` is a pending/accepted member of.
async fn member_collection_ids(
    state: &AppState,
    account_id: i64,
) -> Result<HashSet<i64>, ApiError> {
    // `containing` is offset-paginated; the per-account cap is small, so one
    // generous page covers every collection an account can belong to.
    let limit = collection::PER_ACCOUNT_LIMIT * collection::MAX_ITEMS;
    let collections = collection::containing(&state.pool, account_id, 0, limit).await?;
    Ok(collections.into_iter().map(|c| c.id).collect())
}

// ---- Public collection page --------------------------------------------

/// `GET /@{handle}/collections/{id}` — a collection's public face: its name,
/// description and member accounts. Only the owner's own collections resolve
/// here; a non-owner viewer sees only accepted members (Mastodon's `items_for`).
pub async fn public_page(
    State(state): State<AppState>,
    MaybeWebUser(session): MaybeWebUser,
    request_locale: Locale,
    uri: Uri,
    Path((handle, collection_id)): Path<(String, i64)>,
    headers: HeaderMap,
) -> Response {
    let owner = match resolve_profile_handle(&state, &handle, &uri).await {
        Ok(ProfileHandleResolution::Account(owner)) => *owner,
        Ok(ProfileHandleResolution::Redirect(response)) => return response,
        Ok(ProfileHandleResolution::Missing) => {
            return not_found(&state, session.as_ref(), request_locale).await;
        }
        Err(err) => return err.into_response(),
    };
    // Content-negotiate like Mastodon: an ActivityPub client dereferencing this
    // shareable collection URL gets the FEP-7aa9 `FeaturedCollection` document
    // off the same URL, served in place (not redirected) so the caller's HTTP
    // signature stays valid. Only for our own collections; `get_collection` is
    // normally behind the `signed_fetch` middleware, so apply the same
    // secure-mode gate here first.
    if owner.is_local() && crate::routes::ap_requested(&headers) {
        if let Err(err) = crate::signed_fetch::enforce(&state, &uri, &headers).await {
            return err.into_response();
        }
        return match crate::routes::collections_ap::get_collection(
            State(state),
            Path((owner.username.clone(), collection_id)),
            uri,
            headers,
        )
        .await
        {
            Ok(resp) => resp.into_response(),
            Err(err) => err.into_response(),
        };
    }
    let collection = match collection::find(&state.pool, collection_id).await {
        Ok(Some(coll)) if coll.account_id == owner.id => coll,
        Ok(_) => return not_found(&state, session.as_ref(), request_locale).await,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let viewer_id = session.as_ref().map(|u| u.current.account.id);
    // A non-owner may only see a discoverable collection.
    if viewer_id != Some(owner.id) && !collection.discoverable {
        return not_found(&state, session.as_ref(), request_locale).await;
    }
    let items = match collection::items_for(&state.pool, collection.id, owner.id, viewer_id).await {
        Ok(items) => items,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let ids: Vec<i64> = items.iter().filter_map(|item| item.account_id).collect();
    let accounts =
        match render_accounts_by_ids(&state.pool, &state.config.domain, &ids, viewer_id).await {
            Ok(accounts) => accounts,
            Err(err) => return err.into_response(),
        };
    let owner_path = account_profile_path(&state.config.domain, &owner);
    // Anonymous readers reach this page, so honour the negotiated header locale
    // rather than falling back to English.
    let locale = session.as_ref().map_or(request_locale, |user| user.locale);
    let body = html! {
        section.column {
            header.list-head {
                p.settings-field__hint {
                    a href=(owner_path) { "← " (account_handle(&state.config.domain, &owner)) }
                }
                h1 { (view::icon("collection")) " " (collection.name) }
                @if !collection.description.is_empty() {
                    p.profile__note { (collection.description) }
                }
            }
            @if accounts.is_empty() {
                p.empty { (locale.text("collections-public-empty")) }
            } @else {
                div.profile-featured__grid {
                    @for value in &accounts {
                        (view::account_card(&view::Account(value)))
                    }
                }
            }
        }
    };
    let site_name = match super::meta::site_name(&state).await {
        Ok(name) => name,
        Err(err) => return err.into_response(),
    };
    let meta = super::meta::collection_page(site_name, &state.config.domain, &owner, &collection);
    layout::shell_visitor_subject_localized(
        &collection.name,
        session.as_ref(),
        super::pages::anon_nav(&state).await,
        &body,
        &meta,
        locale,
    )
    .into_response()
}

/// A profile's public collections, rendered as a "Collections" section for the
/// profile page. Empty (renders nothing) when the account features none.
pub async fn profile_section(
    state: &AppState,
    account: &Account,
    viewer_id: Option<i64>,
    locale: Locale,
) -> Result<Markup, ApiError> {
    let only_discoverable = viewer_id != Some(account.id);
    let collections = collection::owned_by(
        &state.pool,
        account.id,
        only_discoverable,
        0,
        collection::PER_ACCOUNT_LIMIT,
    )
    .await?;
    if collections.is_empty() {
        return Ok(html! {});
    }
    let base = account_profile_path(&state.config.domain, account);
    Ok(html! {
        section.profile-featured {
            h2.profile-featured__title { (locale.text("profile-collections")) }
            ul.list-index {
                @for coll in &collections {
                    li.list-index__item {
                        a.list-index__link href=(format!("{base}/collections/{}", coll.id)) {
                            (view::icon("collection"))
                            span { (coll.name) }
                        }
                    }
                }
            }
        }
    })
}

// ---- Shared helpers ----------------------------------------------------

/// The `@user` / `@user@host` handle of an account.
fn account_handle(domain: &str, account: &Account) -> String {
    format!("@{}", crate::entities::account_acct(domain, account))
}

/// The local profile path for an account (`/@user`, `/@user@host`, or
/// `/!user@host` for a remote Group so it stays distinct from a same-named
/// person).
fn account_profile_path(domain: &str, account: &Account) -> String {
    let acct = crate::entities::account_acct(domain, account);
    if account.is_group() && !account.has_local_account_on(domain) {
        format!("/!{acct}")
    } else {
        format!("/@{acct}")
    }
}

/// Normalizes a typed handle for [`resolve_handle`], which expects a leading
/// `@` (person) or `!` (group): accepts `@user`, `user`, `@user@host`,
/// `user@host` and the `!`-prefixed community forms.
fn normalize_handle(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix('!') {
        format!("!{}", rest.trim_start_matches('@'))
    } else {
        format!("@{}", trimmed.trim_start_matches('@'))
    }
}

/// A translated flash message for a rejected mutation. A typed rejection maps
/// straight onto the catalog; anything else is either a vanished row or an
/// unexpected failure, both of which the reader can only be told about plainly.
fn failure_message(failure: service::Failure, locale: Locale) -> String {
    match failure {
        service::Failure::Invalid(errors) => errors
            .iter()
            .map(|invalid| invalid_message(*invalid, locale))
            .collect::<Vec<_>>()
            .join(" "),
        service::Failure::Api(ApiError::NotFound) => locale.text("collections-error-missing"),
        service::Failure::Api(_) => locale.text("collections-error-generic"),
    }
}

/// The catalog message for one typed rejection, carrying whichever limit it
/// speaks about. Adding a variant fails to compile until it has translated copy.
fn invalid_message(invalid: service::Invalid, locale: Locale) -> String {
    let (id, limit) = match invalid {
        service::Invalid::NameBlank => ("collections-error-name-blank", None),
        service::Invalid::NameTooLong => (
            "collections-error-name-too-long",
            Some(i64::try_from(collection::NAME_LENGTH_LIMIT).unwrap_or(i64::MAX)),
        ),
        service::Invalid::DescriptionTooLong => (
            "collections-error-description-too-long",
            Some(i64::try_from(collection::DESCRIPTION_LENGTH_LIMIT).unwrap_or(i64::MAX)),
        ),
        service::Invalid::CollectionLimit => (
            "collections-error-collection-limit",
            Some(collection::PER_ACCOUNT_LIMIT),
        ),
        service::Invalid::ItemLimit => {
            ("collections-error-item-limit", Some(collection::MAX_ITEMS))
        }
        service::Invalid::NotFeatureable => ("collections-error-not-featureable", None),
        service::Invalid::AlreadyAMember => ("collections-error-already-member", None),
    };
    match limit {
        Some(limit) => {
            let mut args = FluentArgs::new();
            args.set("limit", limit);
            locale.text_with(id, &args)
        }
        None => locale.text(id),
    }
}

/// Redirects back to `base` with a human-readable error in the query string.
fn redirect_error(base: &str, message: &str) -> Response {
    let sep = if base.contains('?') { '&' } else { '?' };
    let query = serde_urlencoded::to_string([("error", message)]).unwrap_or_default();
    redirect_to(&format!("{base}{sep}{query}"))
}
