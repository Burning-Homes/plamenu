//! Content-filter management — the web half of the v2 filter API (see
//! [`plamenu_db::custom_filter`] and [`crate::routes::filters`]). A signed-in
//! account can create keyword filters, tune their contexts, action and expiry,
//! edit keywords in place, and delete a filter. The web timelines consume the
//! same [`plamenu_db::custom_filter::active_for`] set the REST API serves, so
//! changes apply immediately.
//!
//! Everything works with JavaScript disabled: forms POST and each write
//! follows POST/Redirect/GET. Keywords edit in place — clear a keyword's text
//! to remove it, use the blank row to add one.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use fluent_bundle::FluentArgs;
use maud::{Markup, html};
use plamenu_db::custom_filter::{
    self, ACTIONS, CustomFilter, KEYWORD_LENGTH_LIMIT, KeywordChange, MAX_FILTERS_PER_ACCOUNT,
    MAX_KEYWORDS_PER_FILTER, NewKeyword, TITLE_LENGTH_LIMIT, VALID_CONTEXTS,
};
use serde::Deserialize;
use time::{Duration, OffsetDateTime};

use super::clock::ViewerClock;
use super::i18n::Locale;
use super::session::{WebUser, csrf_rejection};
use super::settings::{
    bad_form, checked, error_flash, field, form_pairs, redirect_to, saved_flash, settings_shell,
};
use crate::error::ApiError;
use crate::state::AppState;

/// The context checkboxes, in [`VALID_CONTEXTS`] order, with the catalog keys
/// for the labels Mastodon shows on its filter settings screen.
const CONTEXT_CHOICES: [(&str, &str); 5] = [
    ("home", "filters-context-home"),
    ("notifications", "filters-context-notifications"),
    ("public", "filters-context-public"),
    ("thread", "filters-context-thread"),
    ("account", "filters-context-account"),
];

/// The action select, paired with its label messages.
const ACTION_CHOICES: [(&str, &str); 3] = [
    ("warn", "filters-action-warn"),
    ("blur", "filters-action-blur"),
    ("hide", "filters-action-hide"),
];

/// The expiry choices (seconds), matching Mastodon's filter form.
const EXPIRY_CHOICES: [(i64, &str); 6] = [
    (1800, "filters-expiry-30-minutes"),
    (3600, "filters-expiry-1-hour"),
    (21_600, "filters-expiry-6-hours"),
    (43_200, "filters-expiry-12-hours"),
    (86_400, "filters-expiry-1-day"),
    (604_800, "filters-expiry-1-week"),
];

#[derive(Deserialize)]
pub struct FiltersQuery {
    saved: Option<String>,
    error: Option<String>,
}

/// Looks a filter up, scoped to the signed-in owner; renders the styled 404
/// page when it isn't theirs.
async fn owned_filter(
    state: &AppState,
    user: &WebUser,
    filter_id: i64,
) -> Result<CustomFilter, Response> {
    match custom_filter::find_owned(&state.pool, user.current.account.id, filter_id).await {
        Ok(Some(filter)) => Ok(filter),
        Ok(None) => Err(super::pages::not_found(state, Some(user), user.locale).await),
        Err(err) => Err(ApiError::from(err).into_response()),
    }
}

/// The filter's expiry as a settings-row label.
fn expiry_label(filter: &CustomFilter, clock: &ViewerClock, locale: Locale) -> String {
    match filter.expires_at {
        None => locale.text("filters-expires-never"),
        Some(at) if at <= OffsetDateTime::now_utc() => locale.text("filters-expired"),
        Some(at) => {
            let mut args = FluentArgs::new();
            args.set("time", clock.stamp(at));
            locale.text_with("filters-expires-at", &args)
        }
    }
}

/// The human label for a stored action value.
fn action_label(action: &str, locale: Locale) -> String {
    let message = ACTION_CHOICES
        .iter()
        .find(|(value, _)| *value == action)
        .map_or("filters-action-warn", |(_, message)| message);
    locale.text(message)
}

/// The shared title/action/expiry/context fields of the create and edit forms.
fn filter_fields(filter: Option<&CustomFilter>, locale: Locale) -> Markup {
    let title = filter.map_or("", |f| f.title.as_str());
    let action = filter.map_or("warn", |f| f.action.as_str());
    let empty: &[String] = &[];
    let contexts = filter.map_or(empty, |f| f.context.as_slice());
    let has = |key: &str| contexts.iter().any(|c| c == key);
    html! {
        fieldset.settings-form__group {
            legend { (locale.text("filters-group")) }
            label.settings-field {
                span.settings-field__label { (locale.text("filters-field-title")) }
                input type="text" name="title" value=(title)
                    maxlength=(TITLE_LENGTH_LIMIT) autocomplete="off" required;
            }
            label.settings-field {
                span.settings-field__label { (locale.text("filters-field-action")) }
                select name="action" {
                    @for (value, message) in ACTION_CHOICES {
                        option value=(value) selected[value == action] { (locale.text(message)) }
                    }
                }
                span.settings-field__hint { (locale.text("filters-action-hint")) }
            }
            label.settings-field {
                span.settings-field__label { (locale.text("filters-field-expiry")) }
                select name="expires_in" {
                    @if filter.is_some_and(|f| f.expires_at.is_some()) {
                        option value="keep" selected { (locale.text("filters-expiry-keep")) }
                    }
                    option value="" selected[filter.is_none_or(|f| f.expires_at.is_none())] {
                        (locale.text("filters-expiry-never"))
                    }
                    @for (seconds, message) in EXPIRY_CHOICES {
                        option value=(seconds) { (locale.text(message)) }
                    }
                }
            }
        }
        fieldset.settings-form__group {
            legend { (locale.text("filters-contexts")) }
            p.settings-field__hint { (locale.text("filters-contexts-hint")) }
            @for (value, message) in CONTEXT_CHOICES {
                label.settings-toggle {
                    input type="checkbox" name="context" value=(value) checked[has(value)];
                    span.settings-toggle__text {
                        span.settings-toggle__label { (locale.text(message)) }
                    }
                }
            }
        }
    }
}

// ---- Index -------------------------------------------------------------

/// `GET /settings/filters` — every filter the viewer owns.
pub async fn index(
    State(state): State<AppState>,
    user: WebUser,
    Query(query): Query<FiltersQuery>,
) -> Response {
    let filters = match custom_filter::owned_by(&state.pool, user.current.account.id).await {
        Ok(filters) => filters,
        Err(err) => return ApiError::from(err).into_response(),
    };
    // One grouped count for every owned filter instead of a COUNT per row.
    let filter_ids: Vec<i64> = filters.iter().map(|f| f.id).collect();
    let keyword_counts = match custom_filter::count_keywords_many(&state.pool, &filter_ids).await {
        Ok(counts) => counts,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let rows: Vec<(CustomFilter, i64)> = filters
        .into_iter()
        .map(|filter| {
            let keywords = keyword_counts.get(&filter.id).copied().unwrap_or(0);
            (filter, keywords)
        })
        .collect();
    let clock = &user.clock;
    let locale = user.locale;
    let summary = |filter: &CustomFilter, keywords: i64| {
        let mut args = FluentArgs::new();
        args.set("action", action_label(&filter.action, locale));
        args.set("keywords", keywords);
        args.set("expiry", expiry_label(filter, clock, locale));
        locale.text_with("filters-summary", &args)
    };
    let body = html! {
        (saved_flash(query.saved.is_some(), &locale.text("filters-updated")))
        (error_flash(query.error.as_deref()))
        p.settings-field__hint { (locale.text("filters-intro")) }
        p { a.pill-button href="/settings/filters/new" { (locale.text("filters-new")) } }
        @if rows.is_empty() {
            p.empty { (locale.text("filters-empty")) }
        } @else {
            ul.list-index {
                @for (filter, keywords) in &rows {
                    li.list-index__item {
                        a.list-index__link href=(format!("/settings/filters/{}", filter.id)) {
                            span { (filter.title) }
                        }
                        span.settings-field__hint { (summary(filter, *keywords)) }
                    }
                }
            }
        }
    };
    settings_shell(
        &user,
        "/settings/filters",
        &locale.text("filters-title"),
        &body,
    )
    .into_response()
}

// ---- Create ------------------------------------------------------------

/// `GET /settings/filters/new`.
pub async fn new_page(user: WebUser, Query(query): Query<FiltersQuery>) -> Markup {
    let locale = user.locale;
    let body = html! {
        (error_flash(query.error.as_deref()))
        form.settings-form method="post" action="/web/settings/filters" {
            input type="hidden" name="csrf" value=(user.csrf);
            (filter_fields(None, locale))
            fieldset.settings-form__group {
                legend { (locale.text("filters-first-keyword")) }
                p.settings-field__hint { (locale.text("filters-first-keyword-hint")) }
                label.settings-field {
                    span.settings-field__label { (locale.text("filters-keyword")) }
                    input type="text" name="new_keyword"
                        maxlength=(KEYWORD_LENGTH_LIMIT) autocomplete="off";
                }
                label.settings-toggle {
                    input type="hidden" name="new_whole_word" value="false";
                    input type="checkbox" name="new_whole_word" value="true";
                    span.settings-toggle__text {
                        span.settings-toggle__label { (locale.text("filters-whole-word")) }
                    }
                }
            }
            div.settings-form__actions {
                button type="submit" { (locale.text("filters-create")) }
                a.settings-button--plain href="/settings/filters" {
                    (locale.text("filters-cancel"))
                }
            }
        }
    };
    settings_shell(
        &user,
        "/settings/filters",
        &locale.text("filters-new"),
        &body,
    )
}

/// `POST /web/settings/filters` — create a filter, then land on its edit page.
pub async fn create_action(State(state): State<AppState>, user: WebUser, body: Bytes) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let title = field(&pairs, "title").unwrap_or_default().trim().to_owned();
    let action = field(&pairs, "action").unwrap_or("warn").to_owned();
    let context = context_values(&pairs);
    let locale = user.locale;
    if let Err(message) = validate(&title, &action, &context, locale) {
        return redirect_error("/settings/filters/new", &message);
    }
    match custom_filter::count_owned(&state.pool, user.current.account.id).await {
        Ok(count) => {
            if let Some(message) = filter_limit_error(count, locale) {
                return redirect_error("/settings/filters/new", &message);
            }
        }
        Err(err) => return ApiError::from(err).into_response(),
    }
    let expires_at = parse_expiry(field(&pairs, "expires_in"), None);
    let mut keywords = Vec::new();
    let new_text = field(&pairs, "new_keyword").unwrap_or_default().trim();
    if !new_text.is_empty() {
        if let Err(message) = validate_keyword(new_text, locale) {
            return redirect_error("/settings/filters/new", &message);
        }
        keywords.push(NewKeyword {
            keyword: new_text.to_owned(),
            whole_word: checked(&pairs, "new_whole_word"),
        });
    }
    match custom_filter::create(
        &state.pool,
        user.current.account.id,
        &title,
        &action,
        &context,
        expires_at,
        &keywords,
    )
    .await
    {
        Ok(created) => redirect_to(&format!("/settings/filters/{}?saved=1", created.id)),
        Err(err) => ApiError::from(err).into_response(),
    }
}

// ---- Edit / delete -----------------------------------------------------

/// `GET /settings/filters/{id}` — edit attributes and keywords in one form.
pub async fn edit_form(
    State(state): State<AppState>,
    user: WebUser,
    Path(filter_id): Path<i64>,
    Query(query): Query<FiltersQuery>,
) -> Response {
    let filter = match owned_filter(&state, &user, filter_id).await {
        Ok(filter) => filter,
        Err(response) => return response,
    };
    let keywords = match custom_filter::keywords_for(&state.pool, filter.id).await {
        Ok(keywords) => keywords,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let clock = &user.clock;
    let locale = user.locale;
    let body = html! {
        (saved_flash(query.saved.is_some(), &locale.text("filters-saved")))
        (error_flash(query.error.as_deref()))
        p.settings-field__hint { (expiry_label(&filter, clock, locale)) }
        form.settings-form method="post"
            action=(format!("/web/settings/filters/{}", filter.id)) {
            input type="hidden" name="csrf" value=(user.csrf);
            (filter_fields(Some(&filter), locale))
            fieldset.settings-form__group {
                legend { (locale.text("filters-keywords")) }
                p.settings-field__hint { (locale.text("filters-keywords-hint")) }
                @for keyword in &keywords {
                    div.settings-field__pair {
                        label.settings-field {
                            span.settings-field__label { (locale.text("filters-keyword")) }
                            input type="text"
                                name=(format!("keywords[{}][keyword]", keyword.id))
                                maxlength=(KEYWORD_LENGTH_LIMIT)
                                value=(keyword.keyword);
                        }
                        label.settings-toggle {
                            input type="hidden"
                                name=(format!("keywords[{}][whole_word]", keyword.id))
                                value="false";
                            input type="checkbox"
                                name=(format!("keywords[{}][whole_word]", keyword.id))
                                value="true" checked[keyword.whole_word];
                            span.settings-toggle__text {
                                span.settings-toggle__label {
                                    (locale.text("filters-whole-word"))
                                }
                            }
                        }
                    }
                }
                div.settings-field__pair {
                    label.settings-field {
                        span.settings-field__label { (locale.text("filters-add-keyword")) }
                        input type="text" name="new_keyword"
                            maxlength=(KEYWORD_LENGTH_LIMIT) autocomplete="off";
                    }
                    label.settings-toggle {
                        input type="hidden" name="new_whole_word" value="false";
                        input type="checkbox" name="new_whole_word" value="true";
                        span.settings-toggle__text {
                            span.settings-toggle__label { (locale.text("filters-whole-word")) }
                        }
                    }
                }
            }
            div.settings-form__actions {
                button type="submit" { (locale.text("filters-save")) }
                a href="/settings/filters" { (locale.text("filters-back")) }
            }
        }
        form.settings-form method="post"
            action=(format!("/web/settings/filters/{}/delete", filter.id))
            data-confirm=(locale.text("filters-delete-confirm")) {
            input type="hidden" name="csrf" value=(user.csrf);
            div.settings-form__actions {
                button.settings-button--danger type="submit" { (locale.text("filters-delete")) }
            }
        }
    };
    settings_shell(&user, "/settings/filters", &filter.title, &body).into_response()
}

/// `POST /web/settings/filters/{id}` — rewrite attributes and apply keyword
/// edits (update, clear-to-delete, add) in one transaction.
pub async fn update_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(filter_id): Path<i64>,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let filter = match owned_filter(&state, &user, filter_id).await {
        Ok(filter) => filter,
        Err(response) => return response,
    };
    let back = format!("/settings/filters/{filter_id}");
    let title = field(&pairs, "title").unwrap_or_default().trim().to_owned();
    let action = field(&pairs, "action").unwrap_or("warn").to_owned();
    let context = context_values(&pairs);
    let locale = user.locale;
    if let Err(message) = validate(&title, &action, &context, locale) {
        return redirect_error(&back, &message);
    }
    let expires_at = parse_expiry(field(&pairs, "expires_in"), filter.expires_at);

    let mut changes = Vec::new();
    for (id, row) in keyword_rows(&pairs) {
        let Some(text) = row.keyword else { continue };
        let text = text.trim().to_owned();
        if text.is_empty() {
            changes.push(KeywordChange::Destroy { id });
        } else {
            if let Err(message) = validate_keyword(&text, locale) {
                return redirect_error(&back, &message);
            }
            changes.push(KeywordChange::Update {
                id,
                keyword: Some(text),
                whole_word: Some(row.whole_word),
            });
        }
    }
    let new_text = field(&pairs, "new_keyword").unwrap_or_default().trim();
    if !new_text.is_empty() {
        if let Err(message) = validate_keyword(new_text, locale) {
            return redirect_error(&back, &message);
        }
        changes.push(KeywordChange::Create {
            keyword: new_text.to_owned(),
            whole_word: checked(&pairs, "new_whole_word"),
        });
    }

    let added = changes
        .iter()
        .filter(|c| matches!(c, KeywordChange::Create { .. }))
        .count();
    if added > 0 {
        match custom_filter::count_keywords(&state.pool, filter.id).await {
            Ok(current) => {
                let current = usize::try_from(current).unwrap_or(usize::MAX);
                if let Some(message) = keyword_limit_error(current.saturating_add(added), locale) {
                    return redirect_error(&back, &message);
                }
            }
            Err(err) => return ApiError::from(err).into_response(),
        }
    }

    match custom_filter::update(
        &state.pool,
        filter.id,
        &title,
        &action,
        &context,
        expires_at,
        &changes,
    )
    .await
    {
        Ok(_) => redirect_to(&format!("{back}?saved=1")),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// `POST /web/settings/filters/{id}/delete`.
pub async fn delete_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(filter_id): Path<i64>,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    match custom_filter::delete(&state.pool, user.current.account.id, filter_id).await {
        Ok(true) => redirect_to("/settings/filters?saved=1"),
        Ok(false) => super::pages::not_found(&state, Some(&user), user.locale).await,
        Err(err) => ApiError::from(err).into_response(),
    }
}

// ---- Shared helpers ----------------------------------------------------

/// The checked context values, deduplicated and restricted to the valid set.
fn context_values(pairs: &[(String, String)]) -> Vec<String> {
    let mut contexts: Vec<String> = Vec::new();
    for (key, value) in pairs {
        if key == "context" && VALID_CONTEXTS.contains(&value.as_str()) && !contexts.contains(value)
        {
            contexts.push(value.clone());
        }
    }
    contexts
}

/// The submitted expiry: `keep` preserves the stored instant, blank clears it,
/// a number of seconds re-arms it from now.
fn parse_expiry(raw: Option<&str>, existing: Option<OffsetDateTime>) -> Option<OffsetDateTime> {
    match raw {
        Some("keep") | None => existing,
        Some(value) => value
            .parse::<i64>()
            .ok()
            .filter(|&seconds| seconds > 0)
            .map(|seconds| OffsetDateTime::now_utc() + Duration::seconds(seconds)),
    }
}

/// One existing keyword's submitted state, keyed by its row id.
#[derive(Default)]
struct KeywordRow {
    keyword: Option<String>,
    whole_word: bool,
}

/// Collects the `keywords[{id}][keyword|whole_word]` pairs. The hidden-false /
/// checkbox-true pattern means the last `whole_word` value wins.
fn keyword_rows(pairs: &[(String, String)]) -> std::collections::BTreeMap<i64, KeywordRow> {
    let mut rows: std::collections::BTreeMap<i64, KeywordRow> = std::collections::BTreeMap::new();
    for (key, value) in pairs {
        let Some(rest) = key.strip_prefix("keywords[") else {
            continue;
        };
        let Some((id, attr)) = rest.split_once(']') else {
            continue;
        };
        let Ok(id) = id.parse::<i64>() else { continue };
        let row = rows.entry(id).or_default();
        match attr {
            "[keyword]" => row.keyword = Some(value.clone()),
            "[whole_word]" => row.whole_word = value == "true",
            _ => {}
        }
    }
    rows
}

/// Validates the merged filter attributes the way the REST endpoint does,
/// collapsed to one human-readable flash message.
fn validate(title: &str, action: &str, context: &[String], locale: Locale) -> Result<(), String> {
    if title.trim().is_empty() {
        return Err(locale.text("filters-error-title-blank"));
    }
    if title.chars().count() > TITLE_LENGTH_LIMIT {
        return Err(limit_message(
            "filters-error-title-long",
            TITLE_LENGTH_LIMIT,
            locale,
        ));
    }
    if !ACTIONS.contains(&action) {
        return Err(locale.text("filters-error-action"));
    }
    if context.is_empty() {
        return Err(locale.text("filters-error-context"));
    }
    Ok(())
}

fn validate_keyword(keyword: &str, locale: Locale) -> Result<(), String> {
    if keyword.chars().count() > KEYWORD_LENGTH_LIMIT {
        return Err(limit_message(
            "filters-error-keyword-long",
            KEYWORD_LENGTH_LIMIT,
            locale,
        ));
    }
    Ok(())
}

/// Cardinality guard (audit #58): the message to show when a new filter would
/// exceed the per-account cap, or `None` when it is admitted. Mirrors the REST
/// `within_filter_limit` guard so both surfaces bound the same durable set.
fn filter_limit_error(existing: i64, locale: Locale) -> Option<String> {
    (usize::try_from(existing).unwrap_or(usize::MAX) >= MAX_FILTERS_PER_ACCOUNT).then(|| {
        limit_message(
            "filters-error-filter-limit",
            MAX_FILTERS_PER_ACCOUNT,
            locale,
        )
    })
}

/// Cardinality guard (audit #58): the message to show when a keyword add would
/// push a filter past the per-filter cap (`total` is the resulting count).
fn keyword_limit_error(total: usize, locale: Locale) -> Option<String> {
    (total > MAX_KEYWORDS_PER_FILTER).then(|| {
        limit_message(
            "filters-error-keyword-limit",
            MAX_KEYWORDS_PER_FILTER,
            locale,
        )
    })
}

/// A validation message whose only variable is a numeric cap.
fn limit_message(id: &str, limit: usize, locale: Locale) -> String {
    let mut args = FluentArgs::new();
    args.set("limit", i64::try_from(limit).unwrap_or(i64::MAX));
    locale.text_with(id, &args)
}

/// Redirects back to `base` with a human-readable error in the query string.
fn redirect_error(base: &str, message: &str) -> Response {
    let sep = if base.contains('?') { '&' } else { '?' };
    let query = serde_urlencoded::to_string([("error", message)]).unwrap_or_default();
    redirect_to(&format!("{base}{sep}{query}"))
}
