//! Featured-hashtag settings — Mastodon's `/settings/featured_tags`.
//! Lists the tags pinned to this profile with their usage stats, offers the
//! account's most-used unfeatured tags as one-click suggestions, and takes a
//! free-form hashtag to feature. Featuring/unfeaturing federates the
//! `Add`/`Remove(Hashtag)` through [`crate::actions`], exactly as the REST
//! endpoints do.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use maud::{Markup, html};
use plamenu_db::{featured_tag, tag};

use super::session::{WebUser, csrf_rejection};
use super::settings::{
    SettingsQuery, bad_form, error_flash, field, form_pairs, redirect_to, saved_flash,
    settings_shell,
};
use super::view;
use crate::actions;
use crate::error::ApiError;
use crate::routes::tags::normalize_hashtag;
use crate::state::AppState;

/// Redirects back to the page with a human-readable error in the query string,
/// like the aliases page — an invalid hashtag is worth showing verbatim.
fn redirect_error(message: &str) -> Response {
    let query = serde_urlencoded::to_string([("error", message)]).unwrap_or_default();
    redirect_to(&format!("/settings/featured-tags?{query}"))
}

/// One "feature this tag" button, used for both the suggestions and (implicitly)
/// the add form — a POST carrying the tag name.
fn feature_button(csrf: &str, name: &str, label: &Markup) -> Markup {
    html! {
        form method="post" action="/web/settings/featured-tags" {
            input type="hidden" name="csrf" value=(csrf);
            input type="hidden" name="name" value=(name);
            button type="submit" { (label) }
        }
    }
}

/// `GET /settings/featured-tags` — current featured tags, add form and
/// suggestions.
pub async fn page(
    State(state): State<AppState>,
    user: WebUser,
    Query(query): Query<SettingsQuery>,
) -> Response {
    let account_id = user.current.account.id;
    let featured = match featured_tag::list(&state.pool, account_id).await {
        Ok(featured) => featured,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let suggestions = match featured_tag::suggestions(&state.pool, account_id).await {
        Ok(suggestions) => suggestions,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let clock = &user.clock;
    let locale = user.locale;
    let body = html! {
        (saved_flash(query.saved.is_some(), &locale.text("featured-tags-saved")))
        (error_flash(query.error.as_deref()))

        p.settings-field__hint { (locale.text("featured-tags-intro")) }

        form.settings-form method="post" action="/web/settings/featured-tags" {
            input type="hidden" name="csrf" value=(user.csrf);
            fieldset.settings-form__group {
                legend { (locale.text("featured-tags-feature")) }
                label.settings-field {
                    span.settings-field__label { (locale.text("featured-tags-hashtag")) }
                    input type="text" name="name" placeholder="#hashtag"
                        autocomplete="off" required;
                }
            }
            div.settings-form__actions {
                button type="submit" { (locale.text("featured-tags-submit")) }
            }
        }

        @if featured.is_empty() {
            p.settings-field__hint { (locale.text("featured-tags-empty")) }
        } @else {
            (view::data_table(&html! {
                thead {
                    tr {
                        th scope="col" { (locale.text("featured-tags-hashtag")) }
                        th scope="col" { (locale.text("featured-tags-column-posts")) }
                        th scope="col" { (locale.text("featured-tags-column-last-used")) }
                        th scope="col" { "" }
                    }
                }
                tbody {
                    @for entry in &featured {
                        tr {
                            td { "#" (entry.name) }
                            td { (entry.statuses_count) }
                            td {
                                @if let Some(last) = entry.last_status_at {
                                    (clock.element(last))
                                } @else {
                                    "—"
                                }
                            }
                            td {
                                form method="post"
                                    action=(format!("/web/settings/featured-tags/{}/remove", entry.id)) {
                                    input type="hidden" name="csrf" value=(user.csrf);
                                    button.settings-button--danger type="submit" {
                                        (locale.text("featured-tags-unfeature"))
                                    }
                                }
                            }
                        }
                    }
                }
            }))
        }

        @if !suggestions.is_empty() {
            h3 { (locale.text("featured-tags-suggestions")) }
            p.settings-field__hint { (locale.text("featured-tags-suggestions-hint")) }
            div.featured-tags__suggestions {
                @for suggestion in &suggestions {
                    (feature_button(&user.csrf, &suggestion.name, &html! { "#" (suggestion.name) }))
                }
            }
        }
    };
    settings_shell(
        &user,
        "/settings/featured-tags",
        &locale.text("featured-tags-title"),
        &body,
    )
    .into_response()
}

/// `POST /web/settings/featured-tags` — feature the named hashtag, federating
/// the `Add(Hashtag)`. Mirrors `routes::featured_tags::create`.
pub async fn add_action(State(state): State<AppState>, user: WebUser, body: Bytes) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let raw = field(&pairs, "name").unwrap_or_default();
    let Some(name) = normalize_hashtag(raw) else {
        return redirect_error(&user.locale.text("featured-tags-error-invalid"));
    };
    let tag_id = match tag::ensure(&state.pool, &name).await {
        Ok(tag_id) => tag_id,
        Err(err) => return ApiError::from(err).into_response(),
    };
    match actions::feature_tag(&state, &user.current.account, tag_id, &name).await {
        Ok(_) => redirect_to("/settings/featured-tags?saved=1"),
        Err(err) => err.into_response(),
    }
}

/// `POST /web/settings/featured-tags/{id}/remove` — unfeature by row id,
/// federating the `Remove(Hashtag)`. Mirrors `routes::featured_tags::destroy`.
pub async fn remove_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    if let Err(err) = actions::unfeature_tag_by_id(&state, &user.current.account, id).await {
        return err.into_response();
    }
    redirect_to("/settings/featured-tags?saved=1")
}
