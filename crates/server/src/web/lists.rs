//! User lists in the first-party web UI — the client half of Mastodon's list
//! model (see [`plamenu_db::list`] and [`crate::routes::lists`]). A signed-in
//! account can create lists, browse each list's own timeline, edit or delete a
//! list, and manage its membership. Membership rides on the follow edge: a
//! member must be someone the owner follows (or the owner themself), exactly
//! as the REST API enforces.
//!
//! Everything works with JavaScript disabled: links navigate, forms POST, and
//! each write follows the POST/Redirect/GET pattern back to the page it came
//! from. State-changing endpoints live under `/web/lists/…` so they never
//! collide with a list-timeline URL.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use fluent_bundle::FluentArgs;
use maud::{Markup, html};
use plamenu_db::account;
use plamenu_db::list::{self, AddMemberError, List};
use serde::Deserialize;

use super::actions::safe_return;
use super::collapse;
use super::i18n::Locale;
use super::layout;
use super::pages::{older_link, prefs_of, resolve_handle, settings_for};
use super::session::{WebUser, csrf_rejection};
use super::settings::{
    bad_form, checkbox, error_flash, field, form_pairs, redirect_to, saved_flash,
};
use super::view::{self, Ctx};
use crate::entities::{render_accounts, render_statuses};
use crate::error::ApiError;
use crate::state::AppState;

/// Timeline page size, matching the other web timelines.
const LIMIT: i64 = 20;
/// Member-list page size, matching the follow/relationship listings.
const MEMBERS_LIMIT: i64 = 40;

/// The `replies_policy` choices, paired with the catalog identifiers for the
/// labels Mastodon shows on its list settings screen.
const POLICY_CHOICES: [(&str, &str); 3] = [
    ("list", "lists-policy-list"),
    ("followed", "lists-policy-followed"),
    ("none", "lists-policy-none"),
];

/// Why a list write was refused. The pages these writes redirect to are not
/// the ones that produced the refusal, so it travels as a stable `?error=`
/// code and is re-stated from the catalog where it lands — never as an English
/// sentence in the query string.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Refusal {
    TitleBlank,
    TitleTooLong,
    BadPolicy,
    OverLimit,
    NoSuchAccount,
    NotFollowed,
    AlreadyMember,
}

impl Refusal {
    fn code(self) -> &'static str {
        match self {
            Refusal::TitleBlank => "title_blank",
            Refusal::TitleTooLong => "title_too_long",
            Refusal::BadPolicy => "bad_policy",
            Refusal::OverLimit => "over_limit",
            Refusal::NoSuchAccount => "no_account",
            Refusal::NotFollowed => "not_followed",
            Refusal::AlreadyMember => "already_member",
        }
    }

    /// An unrecognized code renders nothing, so no text a caller puts in the
    /// query string reaches the page.
    fn from_code(code: &str) -> Option<Self> {
        Some(match code {
            "title_blank" => Refusal::TitleBlank,
            "title_too_long" => Refusal::TitleTooLong,
            "bad_policy" => Refusal::BadPolicy,
            "over_limit" => Refusal::OverLimit,
            "no_account" => Refusal::NoSuchAccount,
            "not_followed" => Refusal::NotFollowed,
            "already_member" => Refusal::AlreadyMember,
            _ => return None,
        })
    }

    fn message(self, locale: Locale) -> String {
        match self {
            Refusal::TitleBlank => locale.text("lists-error-title-blank"),
            Refusal::TitleTooLong => {
                let mut args = FluentArgs::new();
                args.set("limit", list::TITLE_LENGTH_LIMIT);
                locale.text_with("lists-error-title-too-long", &args)
            }
            Refusal::BadPolicy => locale.text("lists-error-bad-policy"),
            Refusal::OverLimit => locale.text("lists-error-over-limit"),
            Refusal::NoSuchAccount => locale.text("lists-error-no-account"),
            Refusal::NotFollowed => locale.text("lists-error-not-followed"),
            Refusal::AlreadyMember => locale.text("lists-error-already-member"),
        }
    }
}

/// The flash for a `?error=` code, or nothing when it is not one of ours.
fn error_message(code: Option<&str>, locale: Locale) -> Option<String> {
    code.and_then(Refusal::from_code)
        .map(|refusal| refusal.message(locale))
}

#[derive(Deserialize)]
pub struct ListQuery {
    max_id: Option<i64>,
    saved: Option<String>,
    error: Option<String>,
}

/// Looks a list up, scoped to the signed-in owner; renders the styled 404 page
/// when it isn't theirs (never the API's JSON error body).
async fn owned_list(state: &AppState, user: &WebUser, list_id: i64) -> Result<List, Response> {
    match list::find_owned(&state.pool, user.current.account.id, list_id).await {
        Ok(Some(list)) => Ok(list),
        Ok(None) => Err(super::pages::not_found(state, Some(user), user.locale).await),
        Err(err) => Err(ApiError::from(err).into_response()),
    }
}

/// The `replies_policy` `<select>`, defaulting to `current`.
fn policy_select(current: &str, locale: Locale) -> Markup {
    html! {
        label.settings-field {
            span.settings-field__label { (locale.text("lists-policy-label")) }
            select name="replies_policy" {
                @for (value, message) in POLICY_CHOICES {
                    option value=(value) selected[value == current] { (locale.text(message)) }
                }
            }
        }
    }
}

/// The lead-in nav for a single list's pages: its title and the tab selector
/// switching between the timeline, members and settings.
fn list_tabs(list: &List, current: &str, locale: Locale) -> Markup {
    let timeline = format!("/lists/{}", list.id);
    let members = format!("/lists/{}/members", list.id);
    let edit = format!("/lists/{}/edit", list.id);
    let timeline_label = locale.text("lists-tab-timeline");
    let members_label = locale.text("lists-tab-members");
    let edit_label = locale.text("lists-tab-settings");
    html! {
        header.list-head {
            h1 { (view::icon("list")) " " (list.title) }
        }
        (view::tab_strip(&locale.text("lists-sections"), &[
            view::Tab::new(&timeline, &timeline_label, current == "timeline"),
            view::Tab::new(&members, &members_label, current == "members"),
            view::Tab::new(&edit, &edit_label, current == "settings"),
        ]))
    }
}

// ---- Index -------------------------------------------------------------

/// `GET /lists` — every list the viewer owns, with a link to the dedicated
/// creation page.
pub async fn index(
    State(state): State<AppState>,
    user: WebUser,
    Query(query): Query<ListQuery>,
) -> Response {
    let lists = match list::owned_by(&state.pool, user.current.account.id).await {
        Ok(lists) => lists,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let at_limit = i64::try_from(lists.len()).unwrap_or(i64::MAX) >= list::PER_ACCOUNT_LIMIT;
    let locale = user.locale;
    let title = locale.text("lists-title");
    let body = html! {
        section.column {
            h1 { (view::icon("list")) " " (title) }
            (saved_flash(query.saved.is_some(), &locale.text("lists-saved")))
            (error_flash(error_message(query.error.as_deref(), locale).as_deref()))
            p.settings-field__hint { (locale.text("lists-intro")) }

            @if at_limit {
                p.settings__error role="alert" { (at_limit_message(locale)) }
            } @else {
                p {
                    a.pill-button href="/lists/new" {
                        (view::icon("list")) " " (locale.text("lists-create"))
                    }
                }
            }

            @if lists.is_empty() {
                p.empty { (locale.text("lists-empty")) }
            } @else {
                ul.list-index {
                    @for list in &lists {
                        li.list-index__item {
                            a.list-index__link href=(format!("/lists/{}", list.id)) {
                                (view::icon("list")) span { (list.title) }
                            }
                            a.list-index__manage href=(format!("/lists/{}/members", list.id)) {
                                (locale.text("lists-tab-members"))
                            }
                            a.list-index__manage href=(format!("/lists/{}/edit", list.id)) {
                                (locale.text("lists-tab-settings"))
                            }
                        }
                    }
                }
            }
        }
    };
    layout::shell(&title, Some(&user), &body).into_response()
}

/// "You've reached the maximum of N lists."
fn at_limit_message(locale: Locale) -> String {
    let mut args = FluentArgs::new();
    args.set("limit", list::PER_ACCOUNT_LIMIT);
    locale.text_with("lists-at-limit", &args)
}

/// `GET /lists/new` — the dedicated list-creation page.
pub async fn new_page(
    State(state): State<AppState>,
    user: WebUser,
    Query(query): Query<ListQuery>,
) -> Response {
    let owned = match list::count_owned(&state.pool, user.current.account.id).await {
        Ok(count) => count,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let locale = user.locale;
    let title = locale.text("lists-new-title");
    let body = html! {
        section.column {
            h1 { (view::icon("list")) " " (title) }
            (error_flash(error_message(query.error.as_deref(), locale).as_deref()))
            @if owned >= list::PER_ACCOUNT_LIMIT {
                p.settings__error role="alert" { (at_limit_message(locale)) }
                p { a href="/lists" { (locale.text("lists-back")) } }
            } @else {
                form.settings-form method="post" action="/web/lists" {
                    input type="hidden" name="csrf" value=(user.csrf);
                    fieldset.settings-form__group {
                        legend { (title) }
                        label.settings-field {
                            span.settings-field__label { (locale.text("lists-field-title")) }
                            input type="text" name="title"
                                placeholder=(locale.text("lists-title-placeholder"))
                                maxlength="256" autocomplete="off" required;
                        }
                        (policy_select("list", locale))
                        (checkbox("exclusive", &locale.text("lists-exclusive"),
                            &locale.text("lists-exclusive-hint"), false))
                    }
                    div.settings-form__actions {
                        button type="submit" { (locale.text("lists-create")) }
                        a.settings-button--plain href="/lists" { (locale.text("filters-cancel")) }
                    }
                }
            }
        }
    };
    layout::shell(&title, Some(&user), &body).into_response()
}

/// `POST /web/lists` — create a list, then land on it.
pub async fn create_action(State(state): State<AppState>, user: WebUser, body: Bytes) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let title = field(&pairs, "title").unwrap_or_default().trim().to_owned();
    let policy = field(&pairs, "replies_policy").unwrap_or("list");
    let exclusive = super::settings::checked(&pairs, "exclusive");

    let owned = match list::count_owned(&state.pool, user.current.account.id).await {
        Ok(count) => count,
        Err(err) => return ApiError::from(err).into_response(),
    };
    if let Err(refusal) = validate(&title, policy, owned >= list::PER_ACCOUNT_LIMIT) {
        return redirect_error("/lists/new", refusal);
    }
    match list::create(
        &state.pool,
        user.current.account.id,
        &title,
        policy,
        exclusive,
    )
    .await
    {
        Ok(created) => redirect_to(&format!("/lists/{}", created.id)),
        Err(err) => ApiError::from(err).into_response(),
    }
}

// ---- Timeline ----------------------------------------------------------

/// `GET /lists/{id}` — the list's timeline.
pub async fn timeline(
    State(state): State<AppState>,
    user: WebUser,
    Path(list_id): Path<i64>,
    Query(query): Query<ListQuery>,
) -> Response {
    let list = match owned_list(&state, &user, list_id).await {
        Ok(list) => list,
        Err(response) => return response,
    };
    let viewer_id = user.current.account.id;
    let settings = match settings_for(&user, &state).await {
        Ok(settings) => settings,
        Err(err) => return err.into_response(),
    };
    let statuses = match list::timeline(
        &state.pool,
        &list,
        settings.timeline_order,
        query.max_id,
        LIMIT,
    )
    .await
    {
        Ok(statuses) => statuses,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let entities = match render_statuses(
        &state.pool,
        &state.config.domain,
        &statuses,
        Some(viewer_id),
    )
    .await
    {
        Ok(entities) => entities,
        Err(err) => return err.into_response(),
    };
    // Merge repeated boosts of one post into a single card, exactly as
    // home does — the pager below still reads the uncollapsed rows.
    let policy = collapse::Policy::resolve(&state, &settings).await;
    let entities = if policy.enabled {
        let seen = match policy.window_for(query.max_id) {
            Some((lookback, cursor)) => {
                match list::timeline(&state.pool, &list, settings.timeline_order, None, lookback)
                    .await
                {
                    Ok(head) => collapse::seen_targets(&head, cursor),
                    Err(err) => return ApiError::from(err).into_response(),
                }
            }
            None => std::collections::HashSet::new(),
        };
        collapse::collapse(entities, &seen)
    } else {
        entities
    };
    // Reply hints and thread grouping, also exactly as home does.
    let mut entities = entities;
    if let Err(err) = super::thread::annotate_reply_peeks(&state, viewer_id, &mut entities).await {
        return err.into_response();
    }
    let cards = super::thread::group(entities);
    let base = format!("/lists/{list_id}");
    let viewer = viewer_id.to_string();
    let ctx = Ctx {
        csrf: Some(&user.csrf),
        viewer_id: Some(&viewer),
        return_to: &base,
        filter_context: Some(view::FilterContext::Home),
        prefs: prefs_of(
            &settings,
            crate::translation::web_language_map(&state).await,
        ),
        locale: user.locale,
        clock: user.clock.clone(),
        admin: user.admin_capabilities(),
    };
    let locale = user.locale;
    let body = html! {
        section.column {
            (list_tabs(&list, "timeline", locale))
            @if statuses.is_empty() {
                p.empty { (locale.text("lists-timeline-empty")) }
            } @else {
                (view::threaded_feed(&cards, &ctx))
                (older_link(&base, &statuses, LIMIT))
            }
        }
    };
    layout::shell(&list.title, Some(&user), &body).into_response()
}

// ---- Settings (edit / delete) ------------------------------------------

/// `GET /lists/{id}/edit` — rename, retune replies/exclusivity, or delete.
pub async fn edit_form(
    State(state): State<AppState>,
    user: WebUser,
    Path(list_id): Path<i64>,
    Query(query): Query<ListQuery>,
) -> Response {
    let list = match owned_list(&state, &user, list_id).await {
        Ok(list) => list,
        Err(response) => return response,
    };
    let locale = user.locale;
    let body = html! {
        section.column {
            (list_tabs(&list, "settings", locale))
            (saved_flash(query.saved.is_some(), &locale.text("lists-updated")))
            (error_flash(error_message(query.error.as_deref(), locale).as_deref()))
            form.settings-form method="post" action=(format!("/web/lists/{}/edit", list.id)) {
                input type="hidden" name="csrf" value=(user.csrf);
                fieldset.settings-form__group {
                    legend { (locale.text("lists-settings-legend")) }
                    label.settings-field {
                        span.settings-field__label { (locale.text("lists-field-title")) }
                        input type="text" name="title" value=(list.title)
                            maxlength="256" autocomplete="off" required;
                    }
                    (policy_select(&list.replies_policy, locale))
                    (checkbox("exclusive", &locale.text("lists-exclusive"),
                        &locale.text("lists-exclusive-hint"), list.exclusive))
                }
                div.settings-form__actions {
                    button type="submit" { (locale.text("lists-save")) }
                }
            }
            @let delete_action = format!("/web/lists/{}/delete", list.id);
            @let delete_message = locale.plain("lists-delete-confirm");
            form.settings-form method="post" action=(view::CONFIRM_PATH)
                data-confirm=(&delete_message) data-confirm-action=(&delete_action) {
                input type="hidden" name="csrf" value=(user.csrf);
                input type="hidden" name="return_to" value=(format!("/lists/{}/edit", list.id));
                (view::confirmation_fields(&delete_action, Some(&delete_message)))
                div.settings-form__actions {
                    button.settings-button--danger type="submit" {
                        (locale.text("lists-delete"))
                    }
                }
            }
        }
    };
    layout::shell(&list.title, Some(&user), &body).into_response()
}

/// `POST /web/lists/{id}/edit`.
pub async fn update_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(list_id): Path<i64>,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    if owned_list(&state, &user, list_id).await.is_err() {
        return super::pages::not_found(&state, Some(&user), user.locale).await;
    }
    let title = field(&pairs, "title").unwrap_or_default().trim().to_owned();
    let policy = field(&pairs, "replies_policy").unwrap_or("list");
    let exclusive = super::settings::checked(&pairs, "exclusive");
    let back = format!("/lists/{list_id}/edit");
    if let Err(refusal) = validate(&title, policy, false) {
        return redirect_error(&back, refusal);
    }
    match list::update(&state.pool, list_id, &title, policy, exclusive).await {
        Ok(_) => redirect_to(&format!("{back}?saved=1")),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// `POST /web/lists/{id}/delete`.
pub async fn delete_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(list_id): Path<i64>,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    match list::delete(&state.pool, user.current.account.id, list_id).await {
        Ok(true) => redirect_to("/lists?saved=1"),
        Ok(false) => super::pages::not_found(&state, Some(&user), user.locale).await,
        Err(err) => ApiError::from(err).into_response(),
    }
}

// ---- Members -----------------------------------------------------------

/// `GET /lists/{id}/members` — the list's members, with a remove control on
/// each and a form to add another followed account by handle.
pub async fn members_page(
    State(state): State<AppState>,
    user: WebUser,
    Path(list_id): Path<i64>,
    Query(query): Query<ListQuery>,
) -> Response {
    let list = match owned_list(&state, &user, list_id).await {
        Ok(list) => list,
        Err(response) => return response,
    };
    let ids = match list::members_page(
        &state.pool,
        list_id,
        query.max_id,
        None,
        Some(MEMBERS_LIMIT),
    )
    .await
    {
        Ok(ids) => ids,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let members = match ordered_accounts(&state, &ids, user.current.account.id).await {
        Ok(accounts) => accounts,
        Err(response) => return response,
    };
    let full = ids.len() >= usize::try_from(MEMBERS_LIMIT).unwrap_or(usize::MAX);
    let locale = user.locale;
    // The two example handles are markup (they render as `<code>`), so the
    // sentence around them stays one translatable message.
    let add_hint = locale.markup(
        "lists-add-hint",
        &[
            ("local", html! { code { "@alice" } }),
            ("remote", html! { code { "bob@example.social" } }),
        ],
    );
    let body = html! {
        section.column {
            (list_tabs(&list, "members", locale))
            (saved_flash(query.saved.is_some(), &locale.text("lists-members-saved")))
            (error_flash(error_message(query.error.as_deref(), locale).as_deref()))

            form.settings-form method="post" action=(format!("/web/lists/{list_id}/members/add")) {
                input type="hidden" name="csrf" value=(user.csrf);
                fieldset.settings-form__group {
                    legend { (locale.text("lists-add-legend")) }
                    p.settings-field__hint { (add_hint) }
                    label.settings-field {
                        span.settings-field__label { (locale.text("lists-field-handle")) }
                        input type="text" name="handle"
                            placeholder=(locale.text("lists-handle-placeholder"))
                            autocomplete="off" required;
                    }
                }
                div.settings-form__actions {
                    button type="submit" { (locale.text("lists-add-submit")) }
                }
            }

            @if members.is_empty() {
                p.empty { (locale.text("lists-members-empty")) }
            } @else {
                ul.relationships-list data-paged {
                    @for value in &members {
                        (member_row(list_id, value, &user.csrf, locale))
                    }
                }
            }
            @if full {
                nav.pager {
                    a.pager__more href=(format!("/lists/{list_id}/members?max_id={}",
                        ids.last().copied().unwrap_or_default())) {
                        (locale.text("page-load-more"))
                    }
                }
            }
        }
    };
    layout::shell(&list.title, Some(&user), &body).into_response()
}

/// One member row: the account card plus a Remove button.
fn member_row(list_id: i64, value: &serde_json::Value, csrf: &str, locale: Locale) -> Markup {
    let account = view::Account(value);
    html! {
        li.relationships-list__item {
            (view::account_card(&account))
            form method="post" action=(format!("/web/lists/{list_id}/members/remove")) {
                input type="hidden" name="csrf" value=(csrf);
                input type="hidden" name="account_id" value=(account.id());
                button.settings-button--danger type="submit" {
                    (locale.text("lists-remove-member"))
                }
            }
        }
    }
}

/// `POST /web/lists/{id}/members/add` — resolve the typed handle to a followed
/// account and add it, or bounce back with a readable error.
pub async fn add_member_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(list_id): Path<i64>,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    if owned_list(&state, &user, list_id).await.is_err() {
        return super::pages::not_found(&state, Some(&user), user.locale).await;
    }
    let back = format!("/lists/{list_id}/members");
    let raw = field(&pairs, "handle").unwrap_or_default().trim();
    let handle = normalize_handle(raw);
    let account = match resolve_handle(&state, &handle).await {
        Ok(Some(account)) => account,
        Ok(None) => return redirect_error(&back, Refusal::NoSuchAccount),
        Err(err) => return err.into_response(),
    };
    match list::add_members(&state.pool, list_id, user.current.account.id, &[account.id]).await {
        Ok(Ok(())) => redirect_to(&format!("{back}?saved=1")),
        Ok(Err(reason)) => redirect_error(&back, add_refusal(Some(reason))),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// `POST /web/lists/{id}/members/remove`.
pub async fn remove_member_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(list_id): Path<i64>,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    if owned_list(&state, &user, list_id).await.is_err() {
        return super::pages::not_found(&state, Some(&user), user.locale).await;
    }
    let account_id: i64 = field(&pairs, "account_id")
        .and_then(|s| s.parse().ok())
        .unwrap_or_default();
    match list::remove_members(&state.pool, list_id, &[account_id]).await {
        Ok(()) => redirect_to(&format!("/lists/{list_id}/members?saved=1")),
        Err(err) => ApiError::from(err).into_response(),
    }
}

// ---- Manage an account's lists (from its profile) ----------------------

#[derive(Deserialize)]
pub struct ManageQuery {
    return_to: Option<String>,
    error: Option<String>,
}

/// `GET /web/accounts/{id}/lists` — the "add to lists" panel reached from a
/// profile's overflow menu: a checkbox per owned list, ticked where the account
/// is already a member. Submitting reconciles the whole set.
pub async fn account_lists_form(
    State(state): State<AppState>,
    user: WebUser,
    Path(account_id): Path<i64>,
    Query(query): Query<ManageQuery>,
) -> Response {
    let account = match account::find_by_id(&state.pool, account_id).await {
        Ok(Some(account)) => account,
        Ok(None) => return super::pages::not_found(&state, Some(&user), user.locale).await,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let owner_id = user.current.account.id;
    let lists = match list::owned_by(&state.pool, owner_id).await {
        Ok(lists) => lists,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let member_of: std::collections::HashSet<i64> =
        match list::containing(&state.pool, owner_id, account_id).await {
            Ok(lists) => lists.into_iter().map(|l| l.id).collect(),
            Err(err) => return ApiError::from(err).into_response(),
        };
    let handle = format!(
        "@{}",
        crate::entities::account_acct(&state.config.domain, &account)
    );
    let back = safe_return(query.return_to.as_deref(), "/lists");
    let locale = user.locale;
    let mut heading = FluentArgs::new();
    heading.set("handle", handle.as_str());
    let empty = locale.markup(
        "lists-manage-empty",
        &[(
            "create",
            html! { a href="/lists" { (locale.text("lists-manage-create")) } },
        )],
    );
    let body = html! {
        section.column {
            h1 { (locale.text_with("lists-manage-title", &heading)) }
            (error_flash(error_message(query.error.as_deref(), locale).as_deref()))
            p.settings-field__hint { (locale.text("lists-manage-hint")) }
            @if lists.is_empty() {
                p.empty { (empty) }
            } @else {
                form.settings-form method="post"
                    action=(format!("/web/accounts/{account_id}/lists")) {
                    input type="hidden" name="csrf" value=(user.csrf);
                    input type="hidden" name="return_to" value=(back);
                    div.list-membership {
                        @for list in &lists {
                            label.settings-toggle {
                                input type="checkbox" name="list_ids" value=(list.id)
                                    checked[member_of.contains(&list.id)];
                                span.settings-toggle__text {
                                    span.settings-toggle__label { (list.title) }
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
    layout::shell(&locale.text("lists-manage-page-title"), Some(&user), &body).into_response()
}

/// `POST /web/accounts/{id}/lists` — reconcile the account's membership against
/// the ticked lists: add it to newly-checked ones, remove it from unchecked
/// ones. A follow-requirement failure on any add stops and reports.
pub async fn account_lists_action(
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
    match account::find_by_id(&state.pool, account_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return super::pages::not_found(&state, Some(&user), user.locale).await,
        Err(err) => return ApiError::from(err).into_response(),
    }
    let owner_id = user.current.account.id;
    let self_back = safe_return(field(&pairs, "return_to"), "/lists");
    let error_back = format!("/web/accounts/{account_id}/lists");

    let owned: Vec<i64> = match list::owned_by(&state.pool, owner_id).await {
        Ok(lists) => lists.into_iter().map(|l| l.id).collect(),
        Err(err) => return ApiError::from(err).into_response(),
    };
    let checked: std::collections::HashSet<i64> = pairs
        .iter()
        .filter(|(key, _)| key == "list_ids")
        .filter_map(|(_, value)| value.parse().ok())
        .filter(|id| owned.contains(id))
        .collect();
    let member_of: std::collections::HashSet<i64> =
        match list::containing(&state.pool, owner_id, account_id).await {
            Ok(lists) => lists.into_iter().map(|l| l.id).collect(),
            Err(err) => return ApiError::from(err).into_response(),
        };

    for list_id in &owned {
        let want = checked.contains(list_id);
        let have = member_of.contains(list_id);
        let result = if want && !have {
            match list::add_members(&state.pool, *list_id, owner_id, &[account_id]).await {
                Ok(result) => result.map_err(Some),
                Err(err) => return ApiError::from(err).into_response(),
            }
        } else if !want && have {
            match list::remove_members(&state.pool, *list_id, &[account_id]).await {
                Ok(()) => Ok(()),
                Err(err) => return ApiError::from(err).into_response(),
            }
        } else {
            Ok(())
        };
        if let Err(reason) = result {
            let query = serde_urlencoded::to_string([
                ("return_to", self_back.as_str()),
                ("error", add_refusal(reason).code()),
            ])
            .unwrap_or_default();
            return redirect_to(&format!("{error_back}?{query}"));
        }
    }
    redirect_to(&self_back)
}

// ---- Shared helpers ----------------------------------------------------

/// Validates a list's merged attributes the way the REST endpoint does, but
/// collapsed to the first refusal for the flash banner.
fn validate(title: &str, policy: &str, over_limit: bool) -> Result<(), Refusal> {
    if title.trim().is_empty() {
        return Err(Refusal::TitleBlank);
    }
    if title.chars().count() > list::TITLE_LENGTH_LIMIT {
        return Err(Refusal::TitleTooLong);
    }
    if !list::REPLIES_POLICIES.contains(&policy) {
        return Err(Refusal::BadPolicy);
    }
    if over_limit {
        return Err(Refusal::OverLimit);
    }
    Ok(())
}

/// The refusal for an add-member failure. An absent reason means the store
/// declined without saying why, which only happens when the follow edge is
/// missing.
fn add_refusal(reason: Option<AddMemberError>) -> Refusal {
    match reason {
        Some(AddMemberError::NotFollowed) | None => Refusal::NotFollowed,
        Some(AddMemberError::AlreadyMember) => Refusal::AlreadyMember,
    }
}

/// Fetches `ids` as account entities in the given order (newest member first).
async fn ordered_accounts(
    state: &AppState,
    ids: &[i64],
    viewer_id: i64,
) -> Result<Vec<serde_json::Value>, Response> {
    let mut accounts = match account::find_by_ids(&state.pool, ids).await {
        Ok(accounts) => accounts,
        Err(err) => return Err(ApiError::from(err).into_response()),
    };
    accounts.sort_by_key(|a| ids.iter().position(|id| *id == a.id).unwrap_or(usize::MAX));
    render_accounts(
        &state.pool,
        &state.config.domain,
        &accounts,
        Some(viewer_id),
    )
    .await
    .map_err(IntoResponse::into_response)
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

/// Redirects back to `base` carrying the refusal's code, which the page it
/// lands on re-states in the reader's language.
fn redirect_error(base: &str, refusal: Refusal) -> Response {
    let sep = if base.contains('?') { '&' } else { '?' };
    let query = serde_urlencoded::to_string([("error", refusal.code())]).unwrap_or_default();
    redirect_to(&format!("{base}{sep}{query}"))
}
