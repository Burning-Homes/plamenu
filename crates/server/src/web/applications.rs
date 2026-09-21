//! The Development settings area — Mastodon's `/settings/applications`: a
//! signed-in user registers OAuth applications (for bots and API scripts),
//! gets a ready-made access token for their own account, and can edit,
//! regenerate or delete them. The index lists applications only; creation
//! lives on its own `/settings/applications/new` page.
//!
//! One deliberate divergence from Mastodon: Plamenu stores only secret hashes
//! (see [`plamenu_db::oauth`]), so the client secret and access token are
//! revealed exactly once — rendered straight off the creating POST, the same
//! pattern as the two-factor recovery codes — with regeneration to recover
//! from a lost token. Everything else mirrors Mastodon's behavior: the owner
//! token carries the app's scopes, editing scopes regenerates it, deletion
//! drops the app's tokens, and someone else's application id reads as a 404.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use fluent_bundle::FluentArgs;
use maud::{Markup, html};
use plamenu_db::oauth::{self, App};
use time::OffsetDateTime;

use super::clock::zone_chip;
use super::i18n::Locale;
use super::pages::not_found;
use super::session::{WebUser, csrf_rejection};
use super::sessions::scopes_badges;
use super::settings::{
    SettingsQuery, bad_form, error_flash, field, form_pairs, redirect_to, saved_flash,
    settings_shell,
};
use super::view;
use crate::auth::{generate_secret, hash_secret};
use crate::error::ApiError;
use crate::oauth_app;
use crate::routes::oauth::OOB_REDIRECT;
use crate::state::AppState;

const SETTINGS_TAB: &str = "/settings/applications";

/// Mastodon's `ApplicationExtension` limits, shared with the public endpoint's
/// validator so the form hints and the enforced limits can't drift.
const NAME_LIMIT: usize = oauth_app::NAME_LIMIT;
const REDIRECT_URIS_LIMIT: usize = oauth_app::REDIRECT_URIS_LIMIT;
const WEBSITE_LIMIT: usize = oauth_app::WEBSITE_LIMIT;

/// The scopes offered on the form. Plamenu enforces a scope *lattice*
/// (a broad `read` covers `read:statuses`, but a granular `read:statuses` does
/// not widen back into `read` — [`crate::auth::scopes_satisfy`]), so the form
/// offers the broad family scopes, which are the sufficient common choice; a
/// narrower granular string can still be requested over `/api/v1/apps` and
/// stays correctly confined to that resource.
const SCOPE_CHOICES: &[(&str, &str)] = &[
    ("profile", "applications-scope-profile"),
    ("read", "applications-scope-read"),
    ("write", "applications-scope-write"),
    ("follow", "applications-scope-follow"),
    ("push", "applications-scope-push"),
    ("admin:read", "applications-scope-admin-read"),
    ("admin:write", "applications-scope-admin-write"),
];

/// The catalog message for a shared-validator rejection, with the offending
/// value interpolated where the wording needs it. Keeping the mapping next to
/// the form means a new [`oauth_app::Invalid`] variant fails to compile until
/// it has translated copy.
fn invalid_message(invalid: &oauth_app::Invalid, locale: Locale) -> String {
    use oauth_app::Invalid;

    let with_limit = |id: &str, limit: usize| {
        let mut args = FluentArgs::new();
        args.set("limit", i64::try_from(limit).unwrap_or(i64::MAX));
        locale.text_with(id, &args)
    };
    let with_value = |id: &str, value: &str| {
        let mut args = FluentArgs::new();
        args.set("value", value);
        locale.text_with(id, &args)
    };
    match invalid {
        Invalid::NameBlank => locale.text("applications-error-name-blank"),
        Invalid::NameTooLong => with_limit("applications-error-name-too-long", NAME_LIMIT),
        Invalid::RedirectUriMalformed(uri) => {
            with_value("applications-error-redirect-malformed", uri)
        }
        Invalid::RedirectUriFragment(uri) => {
            with_value("applications-error-redirect-fragment", uri)
        }
        Invalid::RedirectUriScheme(uri) => with_value("applications-error-redirect-scheme", uri),
        Invalid::RedirectUrisMissing => locale.text("applications-error-redirect-missing"),
        Invalid::RedirectUrisTooLong => {
            with_limit("applications-error-redirect-too-long", REDIRECT_URIS_LIMIT)
        }
        Invalid::WebsiteTooLong => with_limit("applications-error-website-too-long", WEBSITE_LIMIT),
        Invalid::WebsiteNotHttp => locale.text("applications-error-website-not-http"),
        Invalid::UnknownScope(scope) => with_value("applications-error-unknown-scope", scope),
    }
}

// ---- Index ---------------------------------------------------------------

/// `GET /settings/applications` — the applications the viewer created, newest
/// first. List only: creation is a separate page.
pub async fn index(
    State(state): State<AppState>,
    user: WebUser,
    Query(query): Query<SettingsQuery>,
) -> Response {
    let apps = match oauth::list_owned_apps(&state.pool, user.current.user.id).await {
        Ok(apps) => apps,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let clock = &user.clock;
    let locale = user.locale;
    let body = html! {
        (saved_flash(query.saved.is_some(), &locale.text("applications-deleted")))
        (error_flash(query.error.as_deref()))
        p.settings-field__hint { (locale.text("applications-intro")) }
        (zone_chip(clock))
        p {
            a.pill-button href="/settings/applications/new" {
                (view::icon("bot")) " " (locale.text("applications-new"))
            }
        }
        @if apps.is_empty() {
            p.empty { (locale.text("applications-empty")) }
        } @else {
            (view::data_table(&html! {
                thead {
                    tr {
                        th scope="col" { (locale.text("applications-column-name")) }
                        th scope="col" { (locale.text("applications-column-scopes")) }
                        th scope="col" { (locale.text("applications-column-created")) }
                        th {}
                    }
                }
                tbody {
                        @for app in &apps {
                            @let delete_action = format!("/web/settings/applications/{}/delete", app.id);
                            @let delete_message = locale.text("applications-delete-confirm");
                            tr {
                            td {
                                a href=(format!("/settings/applications/{}", app.id)) {
                                    (app.name)
                                }
                            }
                            td { (scopes_badges(&app.scopes)) }
                            td {
                                time datetime=(rfc3339(app.created_at)) {
                                    (clock.element_date(app.created_at))
                                }
                            }
                            td {
                                form.settings-inline-form method="post"
                                    action=(view::CONFIRM_PATH)
                                    data-confirm=(&delete_message) data-confirm-action=(&delete_action) {
                                    input type="hidden" name="csrf" value=(user.csrf);
                                    input type="hidden" name="return_to" value="/settings/applications";
                                    (view::confirmation_fields(&delete_action, Some(&delete_message)))
                                    button.link-button type="submit" {
                                        (locale.text("applications-delete"))
                                    }
                                }
                            }
                        }
                    }
                }
            }))
        }
    };
    settings_shell(
        &user,
        SETTINGS_TAB,
        &locale.text("applications-title"),
        &body,
    )
    .into_response()
}

// ---- New (the dedicated creation page) ------------------------------------

/// `GET /settings/applications/new` — the application form, prefilled like
/// Mastodon's: the out-of-band redirect URI and the `profile` scope.
pub async fn new_page(user: WebUser, Query(query): Query<SettingsQuery>) -> Response {
    let locale = user.locale;
    let body = html! {
        (error_flash(query.error.as_deref()))
        form.settings-form method="post" action="/web/settings/applications" {
            input type="hidden" name="csrf" value=(user.csrf);
            (app_fieldset(&locale.text("applications-new"), None, locale))
            div.settings-form__actions {
                button type="submit" { (locale.text("applications-create")) }
                a.settings-button--plain href="/settings/applications" {
                    (locale.text("filters-cancel"))
                }
            }
        }
    };
    settings_shell(&user, SETTINGS_TAB, &locale.text("applications-new"), &body).into_response()
}

/// The shared create/edit fields. `existing` pre-fills for the manage page.
fn app_fieldset(legend: &str, existing: Option<&App>, locale: Locale) -> Markup {
    let name = existing.map(|a| a.name.as_str()).unwrap_or_default();
    let website = existing
        .and_then(|a| a.website.as_deref())
        .unwrap_or_default();
    let redirect_uris =
        existing.map_or_else(|| OOB_REDIRECT.to_owned(), |a| a.redirect_uris.join("\n"));
    let scopes: Vec<&str> = existing.map_or_else(
        || vec!["profile"],
        |a| a.scopes.split_whitespace().collect(),
    );
    let oob = html! { code { (OOB_REDIRECT) } };
    html! {
        fieldset.settings-form__group {
            legend { (legend) }
            label.settings-field {
                span.settings-field__label { (locale.text("applications-name")) }
                input type="text" name="name" value=(name)
                    maxlength=(NAME_LIMIT.to_string()) autocomplete="off" required;
            }
            label.settings-field {
                span.settings-field__label { (locale.text("applications-website")) }
                input type="url" name="website" value=(website)
                    maxlength=(WEBSITE_LIMIT.to_string()) placeholder="https://"
                    autocomplete="off";
            }
            label.settings-field {
                span.settings-field__label { (locale.text("applications-redirect-uris")) }
                textarea name="redirect_uris" rows="3" required
                    maxlength=(REDIRECT_URIS_LIMIT.to_string()) { (redirect_uris) }
            }
            p.settings-field__hint {
                (locale.markup("applications-redirect-hint", &[("oob", oob)]))
            }
        }
        fieldset.settings-form__group {
            legend { (locale.text("applications-scopes")) }
            p.settings-field__hint { (locale.text("applications-scopes-hint")) }
            @for (scope, hint) in SCOPE_CHOICES {
                label.settings-toggle {
                    input type="checkbox" name="scopes" value=(scope)
                        checked[scopes.contains(scope)];
                    span.settings-toggle__text {
                        span.settings-toggle__label { code { (scope) } }
                        span.settings-field__hint { (locale.text(hint)) }
                    }
                }
            }
        }
    }
}

// ---- Create ----------------------------------------------------------------

/// `POST /web/settings/applications` — register the application, mint the
/// owner's access token, and show both secrets once. This intentionally
/// renders on the POST (no redirect): the secrets exist only in this response,
/// exactly like the recovery-codes page.
pub async fn create_action(State(state): State<AppState>, user: WebUser, body: Bytes) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let locale = user.locale;
    let params = match validated_params(&pairs, locale) {
        Ok(params) => params,
        Err(message) => return redirect_error("/settings/applications/new", &message),
    };

    let client_id = generate_secret();
    let client_secret = generate_secret();
    let app = match oauth::create_owned_app(
        &state.pool,
        oauth::NewApp {
            name: &params.name,
            website: params.website.as_deref(),
            client_id: &client_id,
            client_secret_hash: &hash_secret(&client_secret),
            redirect_uris: &params.redirect_uris,
            scopes: &params.scopes,
        },
        user.current.user.id,
    )
    .await
    {
        Ok(app) => app,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let token = match mint_owner_token(&state, &user, &app).await {
        Ok(token) => token,
        Err(err) => return err.into_response(),
    };

    let body = html! {
        p.settings__saved role="status" { (locale.text("applications-created-notice")) }
        (credentials_table(&app, Some(&client_secret), &token, locale))
        p.settings-field__hint { (locale.text("applications-secrecy-warning")) }
        p { a.pill-button href=(format!("/settings/applications/{}", app.id)) {
            (locale.text("applications-continue"))
        } }
    };
    settings_shell(
        &user,
        SETTINGS_TAB,
        &locale.text("applications-created-title"),
        &body,
    )
    .into_response()
}

/// The one-time credentials block: client key, secret (creation only), and the
/// owner's fresh access token.
fn credentials_table(
    app: &App,
    client_secret: Option<&str>,
    access_token: &str,
    locale: Locale,
) -> Markup {
    html! {
        (view::data_table(&html! {
            tbody {
                tr {
                    th scope="row" { (locale.text("applications-column-name")) }
                    td { (app.name) }
                }
                tr {
                    th scope="row" { (locale.text("applications-client-key")) }
                    td { code { (app.client_id) } }
                }
                @if let Some(secret) = client_secret {
                    tr {
                        th scope="row" { (locale.text("applications-client-secret")) }
                        td { code { (secret) } }
                    }
                }
                tr {
                    th scope="row" { (locale.text("applications-access-token")) }
                    td { code { (access_token) } }
                }
            }
        }))
    }
}

// ---- Manage one application -------------------------------------------------

/// `GET /settings/applications/{id}` — credentials overview, the edit form,
/// token regeneration, and deletion. Only the owner's apps resolve here.
pub async fn manage_page(
    State(state): State<AppState>,
    user: WebUser,
    Path(app_id): Path<i64>,
    Query(query): Query<SettingsQuery>,
) -> Response {
    let app = match owned_app(&state, &user, app_id).await {
        Ok(app) => app,
        Err(response) => return response,
    };
    let token = match oauth::find_user_app_token(&state.pool, user.current.user.id, app.id).await {
        Ok(token) => token,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let clock = &user.clock;
    let locale = user.locale;
    let token_created = token.as_ref().map(|token| {
        let mut args = FluentArgs::new();
        args.set("date", clock.date(token.created_at));
        locale.text_with("applications-token-created", &args)
    });
    let body = html! {
        (saved_flash(query.saved.is_some(), &locale.text("applications-saved")))
        (error_flash(query.error.as_deref()))
        p.settings-field__hint { (locale.text("applications-secrecy-warning")) }

        section.settings-form__group {
            h3 { (locale.text("applications-credentials")) }
            (view::data_table(&html! {
                tbody {
                    tr {
                        th scope="row" { (locale.text("applications-client-key")) }
                        td { code { (app.client_id) } }
                    }
                    tr {
                        th scope="row" { (locale.text("applications-client-secret")) }
                        td.admin-table__sub {
                            (locale.text("applications-client-secret-lost"))
                        }
                    }
                    tr {
                        th scope="row" { (locale.text("applications-access-token")) }
                        td {
                            @match (&token, &token_created) {
                                (Some(token), Some(created)) => {
                                    (scopes_badges(&token.scopes))
                                    br;
                                    span.admin-table__sub { (created) }
                                }
                                _ => span.admin-table__sub {
                                    (locale.text("applications-no-token"))
                                }
                            }
                        }
                    }
                }
            }))
            @let regenerate_action = format!("/web/settings/applications/{}/regenerate", app.id);
            @let regenerate_message = locale.text("applications-regenerate-confirm");
            form.settings-inline-form method="post" action=(view::CONFIRM_PATH)
                data-confirm=(&regenerate_message) data-confirm-action=(&regenerate_action) {
                input type="hidden" name="csrf" value=(user.csrf);
                input type="hidden" name="return_to" value=(format!("/settings/applications/{}", app.id));
                (view::confirmation_fields(&regenerate_action, Some(&regenerate_message)))
                button type="submit" {
                    @if token.is_some() { (locale.text("applications-regenerate-token")) }
                    @else { (locale.text("applications-generate-token")) }
                }
            }
        }

        form.settings-form method="post"
            action=(format!("/web/settings/applications/{}", app.id)) {
            input type="hidden" name="csrf" value=(user.csrf);
            (app_fieldset(&locale.text("applications-settings"), Some(&app), locale))
            p.settings-field__hint { (locale.text("applications-scope-change-hint")) }
            div.settings-form__actions {
                button type="submit" { (locale.text("settings-profile-save")) }
            }
        }

        @let delete_action = format!("/web/settings/applications/{}/delete", app.id);
        @let delete_message = locale.text("applications-delete-confirm");
        form.settings-form method="post" action=(view::CONFIRM_PATH)
            data-confirm=(&delete_message) data-confirm-action=(&delete_action) {
            input type="hidden" name="csrf" value=(user.csrf);
            input type="hidden" name="return_to" value=(format!("/settings/applications/{}", app.id));
            (view::confirmation_fields(&delete_action, Some(&delete_message)))
            div.settings-form__actions {
                button.settings-button--danger type="submit" {
                    (locale.text("applications-delete-application"))
                }
            }
        }
    };
    settings_shell(&user, SETTINGS_TAB, &app.name, &body).into_response()
}

/// `POST /web/settings/applications/{id}` — save edits. A scope change revokes
/// the owner token and mints a fresh one with the new scopes (Mastodon
/// regenerates on `scopes_previously_changed?`), revealed once.
pub async fn update_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(app_id): Path<i64>,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let app = match owned_app(&state, &user, app_id).await {
        Ok(app) => app,
        Err(response) => return response,
    };
    let locale = user.locale;
    let back = format!("/settings/applications/{app_id}");
    let params = match validated_params(&pairs, locale) {
        Ok(params) => params,
        Err(message) => return redirect_error(&back, &message),
    };
    let scopes_changed = scope_set(&app.scopes) != scope_set(&params.scopes);
    if let Err(err) = oauth::update_owned_app(
        &state.pool,
        app.id,
        &params.name,
        params.website.as_deref(),
        &params.redirect_uris,
        &params.scopes,
    )
    .await
    {
        return ApiError::from(err).into_response();
    }
    if !scopes_changed {
        return redirect_to(&format!("{back}?saved=1"));
    }
    let app = App {
        scopes: params.scopes,
        ..app
    };
    regenerate_and_reveal(
        &state,
        &user,
        &app,
        &locale.text("applications-saved-and-regenerated"),
    )
    .await
}

/// `POST /web/settings/applications/{id}/regenerate` — revoke the owner's
/// token and mint a fresh one, shown once.
pub async fn regenerate_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(app_id): Path<i64>,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let app = match owned_app(&state, &user, app_id).await {
        Ok(app) => app,
        Err(response) => return response,
    };
    let notice = user.locale.text("applications-token-regenerated");
    regenerate_and_reveal(&state, &user, &app, &notice).await
}

/// Revokes every token the owner holds for the app, mints a replacement with
/// the app's current scopes, and renders the one-time reveal page.
async fn regenerate_and_reveal(
    state: &AppState,
    user: &WebUser,
    app: &App,
    notice: &str,
) -> Response {
    if let Err(err) = oauth::revoke_app_for_user(&state.pool, user.current.user.id, app.id).await {
        return ApiError::from(err).into_response();
    }
    let token = match mint_owner_token(state, user, app).await {
        Ok(token) => token,
        Err(err) => return err.into_response(),
    };
    let locale = user.locale;
    let shown_once = {
        let mut args = FluentArgs::new();
        args.set("notice", notice);
        locale.text_with("applications-shown-once", &args)
    };
    let body = html! {
        p.settings__saved role="status" { (shown_once) }
        (credentials_table(app, None, &token, locale))
        p { a.pill-button href=(format!("/settings/applications/{}", app.id)) {
            (locale.text("applications-back"))
        } }
    };
    settings_shell(
        user,
        SETTINGS_TAB,
        &locale.text("applications-access-token-title"),
        &body,
    )
    .into_response()
}

/// `POST /web/settings/applications/{id}/delete` — drop the app; its grants
/// and tokens cascade away.
pub async fn delete_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(app_id): Path<i64>,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let app = match owned_app(&state, &user, app_id).await {
        Ok(app) => app,
        Err(response) => return response,
    };
    match oauth::delete_app(&state.pool, app.id).await {
        Ok(()) => redirect_to("/settings/applications?saved=1"),
        Err(err) => ApiError::from(err).into_response(),
    }
}

// ---- Shared helpers ---------------------------------------------------------

/// An application the signed-in user owns, or the styled 404 page.
async fn owned_app(state: &AppState, user: &WebUser, app_id: i64) -> Result<App, Response> {
    match oauth::find_owned_app(&state.pool, user.current.user.id, app_id).await {
        Ok(Some(app)) => Ok(app),
        Ok(None) => Err(not_found(state, Some(user), user.locale).await),
        Err(err) => Err(ApiError::from(err).into_response()),
    }
}

/// Mints a fresh access token for the app owner with the app's scopes and
/// returns the plaintext (stored hashed). Mastodon's `User#token_for_app`,
/// eager because the plaintext exists only at mint time.
async fn mint_owner_token(state: &AppState, user: &WebUser, app: &App) -> Result<String, ApiError> {
    let token = generate_secret();
    oauth::create_token(
        &state.pool,
        &hash_secret(&token),
        app.id,
        Some(user.current.user.id),
        &app.scopes,
    )
    .await?;
    Ok(token)
}

struct AppParams {
    name: String,
    website: Option<String>,
    redirect_uris: Vec<String>,
    scopes: String,
}

/// Validates the shared form fields, mirroring Mastodon's rules: name required
/// and ≤ 60 chars; at least one redirect URI, each absolute, fragment-free and
/// not a script scheme (`javascript:` & co.), the field ≤ 2000 chars; website
/// optional but http(s) and ≤ 2000 chars; scopes from the offered set. The
/// name/redirect/website rules live in [`crate::oauth_app`], shared verbatim
/// with the public `POST /api/v1/apps` endpoint.
fn validated_params(pairs: &[(String, String)], locale: Locale) -> Result<AppParams, String> {
    let rejected = |invalid: oauth_app::Invalid| invalid_message(&invalid, locale);
    let name = field(pairs, "name").unwrap_or_default().trim().to_owned();
    oauth_app::validate_name(&name).map_err(rejected)?;

    let redirect_uris: Vec<String> = field(pairs, "redirect_uris")
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    oauth_app::validate_redirect_uris(&redirect_uris).map_err(rejected)?;

    let website = oauth_app::validate_website(field(pairs, "website").unwrap_or_default())
        .map_err(rejected)?;

    let mut scopes: Vec<&str> = Vec::new();
    for (key, value) in pairs {
        if key != "scopes" {
            continue;
        }
        if !SCOPE_CHOICES.iter().any(|(scope, _)| scope == value) {
            return Err(rejected(oauth_app::Invalid::UnknownScope(value.clone())));
        }
        if !scopes.contains(&value.as_str()) {
            scopes.push(value);
        }
    }
    // Mastodon falls back to Doorkeeper's default scope when nothing is
    // ticked; `read` matches `POST /api/v1/apps`.
    let scopes = if scopes.is_empty() {
        "read".to_owned()
    } else {
        scopes.join(" ")
    };

    Ok(AppParams {
        name,
        website,
        redirect_uris,
        scopes,
    })
}

/// The scope string as an order-insensitive set, so a reordered submission
/// doesn't count as a change.
fn scope_set(scopes: &str) -> std::collections::BTreeSet<&str> {
    scopes.split_whitespace().collect()
}

/// Redirects back to `base` with a human-readable error in the query string.
fn redirect_error(base: &str, message: &str) -> Response {
    let query = serde_urlencoded::to_string([("error", message)]).unwrap_or_default();
    redirect_to(&format!("{base}?{query}"))
}

fn rfc3339(dt: OffsetDateTime) -> String {
    dt.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}
