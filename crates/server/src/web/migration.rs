//! Account migration settings — Mastodon's `/settings/aliases` and
//! `/settings/migration` pages. Aliases make this account a valid *target*
//! of a move from elsewhere; the migration page initiates the outbound
//! `Move`, guarded by the current password and the 30-day cooldown.

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use fluent_bundle::FluentArgs;
use maud::html;
use plamenu_db::{account_alias, account_migration};

use super::i18n::Locale;
use super::session::{WebUser, csrf_rejection};
use super::settings::{
    SettingsQuery, bad_form, error_flash, field, form_pairs, redirect_to, saved_flash,
    settings_shell,
};
use super::view;
use crate::error::ApiError;
use crate::migration::{self, Failure, Invalid, MIGRATION_COOLDOWN};
use crate::state::AppState;

/// Redirects back to `path` with a ready-to-show error in the query string —
/// the refusals here are already phrased for the reader (and, for a failed
/// lookup, carry the handle that failed), unlike the fixed error codes other
/// settings pages map through `?error=`.
fn redirect_error(path: &str, message: &str) -> Response {
    let query = serde_urlencoded::to_string([("error", message)]).unwrap_or_default();
    redirect_to(&format!("{path}?{query}"))
}

/// The reader's phrasing of a typed migration refusal. The service keeps its
/// own English `Display` for the CLI; this is the same rejection in the
/// interface language (the `oauth_app::Invalid` pattern).
fn invalid_message(invalid: &Invalid, locale: Locale) -> String {
    let mut args = FluentArgs::new();
    let id = match invalid {
        Invalid::Cooldown => "migration-error-cooldown",
        Invalid::SameAccount => "migration-error-same-account",
        Invalid::NotAnAlias => "migration-error-not-an-alias",
        Invalid::LocalTarget => "migration-error-local-target",
        Invalid::BlockedDomain => "migration-error-blocked-domain",
        Invalid::BadHandle(detail) => {
            args.set("detail", detail.clone());
            "migration-error-bad-handle"
        }
        // The technical reason is not translatable, but it is what tells the
        // user whether the handle was wrong or the server merely unreachable.
        Invalid::Unresolvable { acct, detail } => {
            args.set("target", acct.clone());
            args.set("detail", detail.clone());
            "migration-error-unreachable"
        }
        Invalid::Unfetchable { uri, detail } => {
            args.set("target", uri.clone());
            args.set("detail", detail.clone());
            "migration-error-unreachable"
        }
    };
    locale.plain_with(id, &args)
}

/// Turns a failed alias/move attempt into the response the form owes: a typed
/// refusal comes back on the page in the reader's language, anything else is
/// an ordinary error.
fn failure_response(failure: Failure, path: &str, locale: Locale) -> Response {
    match failure {
        Failure::Invalid(invalid) => redirect_error(path, &invalid_message(&invalid, locale)),
        Failure::Api(err) => err.into_response(),
    }
}

/// `GET /settings/aliases` — declared aliases with add/remove.
pub async fn aliases_page(
    State(state): State<AppState>,
    user: WebUser,
    Query(query): Query<SettingsQuery>,
) -> Response {
    let aliases = match account_alias::list(&state.pool, user.current.account.id).await {
        Ok(aliases) => aliases,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let clock = &user.clock;
    let locale = user.locale;
    let handle = format!(
        "{}@{}",
        user.current.account.username, state.config.account_domain
    );
    let body = html! {
        (saved_flash(query.saved.is_some(), &locale.text("aliases-saved")))
        (error_flash(query.error.as_deref()))

        p.settings-field__hint {
            (locale.markup("aliases-hint", &[("handle", html! { b { (handle) } })]))
        }

        form.settings-form method="post" action="/web/settings/aliases" {
            input type="hidden" name="csrf" value=(user.csrf);
            fieldset.settings-form__group {
                legend { (locale.text("aliases-add-legend")) }
                label.settings-field {
                    span.settings-field__label { (locale.text("aliases-handle")) }
                    input type="text" name="acct" placeholder="username@domain"
                        autocomplete="off" required;
                }
            }
            div.settings-form__actions {
                button type="submit" { (locale.text("aliases-add")) }
            }
        }

        @if aliases.is_empty() {
            p.settings-field__hint { (locale.text("aliases-empty")) }
        } @else {
            (view::data_table(&html! {
                thead {
                    tr {
                        th scope="col" { (locale.text("aliases-column-alias")) }
                        th scope="col" { (locale.text("aliases-column-added")) }
                        th scope="col" { "" }
                    }
                }
                tbody {
                    @for alias in &aliases {
                        tr {
                            td {
                                (alias.acct)
                                @if alias.acct != alias.uri {
                                    " " span.settings-field__hint { (alias.uri) }
                                }
                            }
                            td {
                                (clock.element(alias.created_at))
                            }
                            td {
                                form method="post" action="/web/settings/aliases/delete" {
                                    input type="hidden" name="csrf" value=(user.csrf);
                                    input type="hidden" name="uri" value=(alias.uri);
                                    button.settings-button--danger type="submit" {
                                        (locale.text("aliases-remove"))
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
        "/settings/aliases",
        &locale.text("aliases-title"),
        &body,
    )
    .into_response()
}

/// `POST /web/settings/aliases` — resolve and declare an alias.
pub async fn add_alias_action(
    State(state): State<AppState>,
    user: WebUser,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let locale = user.locale;
    let acct = field(&pairs, "acct").unwrap_or_default().trim();
    if acct.is_empty() {
        return redirect_error("/settings/aliases", &locale.text("aliases-error-empty"));
    }
    match migration::add_local_alias(&state, &user.current.account.username, acct).await {
        Ok(_) => redirect_to("/settings/aliases?saved=1"),
        Err(failure) => failure_response(failure, "/settings/aliases", locale),
    }
}

/// `POST /web/settings/aliases/delete` — remove a declared alias by URI.
pub async fn remove_alias_action(
    State(state): State<AppState>,
    user: WebUser,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let uri = field(&pairs, "uri").unwrap_or_default();
    match migration::remove_local_alias(&state, &user.current.account.username, uri).await {
        Ok(_) => redirect_to("/settings/aliases?saved=1"),
        Err(failure) => failure_response(failure, "/settings/aliases", user.locale),
    }
}

/// `GET /settings/migration` — outbound move form, cooldown state, history.
pub async fn migration_page(
    State(state): State<AppState>,
    user: WebUser,
    Query(query): Query<SettingsQuery>,
) -> Response {
    let account = &user.current.account;
    let history = match account_migration::list(&state.pool, account.id).await {
        Ok(history) => history,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let cooldown_until = history
        .first()
        .map(|migration| migration.created_at + MIGRATION_COOLDOWN)
        .filter(|until| *until > time::OffsetDateTime::now_utc());
    let clock = &user.clock;
    let locale = user.locale;
    let body = html! {
        (saved_flash(query.saved.is_some(), &locale.text("migration-saved")))
        (error_flash(query.error.as_deref()))

        @if let Some(target) = account.moved_to_uri.as_deref() {
            p.settings__saved role="status" {
                (locale.markup(
                    "migration-redirects",
                    &[("target", html! { a href=(target) { (target) } })],
                ))
            }
        }

        p.settings-field__hint { (locale.text("migration-hint")) }

        @if let Some(until) = cooldown_until {
            p.settings__error role="alert" {
                (locale.markup(
                    "migration-cooldown",
                    &[("until", html! {
                        (clock.element(until))
                    })],
                ))
            }
        }

        form.settings-form method="post" action="/web/settings/migration" {
            input type="hidden" name="csrf" value=(user.csrf);
            fieldset.settings-form__group.settings-form__group--danger {
                legend { (locale.text("migration-legend")) }
                label.settings-field {
                    span.settings-field__label { (locale.text("migration-handle")) }
                    input type="text" name="acct" placeholder="username@domain"
                        autocomplete="off" required;
                }
                label.settings-field {
                    span.settings-field__label { (locale.text("security-current-password")) }
                    input type="password" name="current_password"
                        autocomplete="current-password" required;
                }
            }
            div.settings-form__actions {
                button.settings-button--danger type="submit" disabled[cooldown_until.is_some()] {
                    (locale.text("migration-move"))
                }
            }
        }

        @if !history.is_empty() {
            h3 { (locale.text("migration-history-title")) }
            (view::data_table(&html! {
                thead {
                    tr {
                        th scope="col" { (locale.text("migration-column-target")) }
                        th scope="col" { (locale.text("migration-column-followers")) }
                        th scope="col" { (locale.text("migration-column-date")) }
                    }
                }
                tbody {
                    @for migration in &history {
                        tr {
                            td { (migration.target_acct) }
                            td { (migration.followers_count) }
                            td {
                                (clock.element(migration.created_at))
                            }
                        }
                    }
                }
            }))
        }
    };
    settings_shell(
        &user,
        "/settings/migration",
        &locale.text("migration-title"),
        &body,
    )
    .into_response()
}

/// `POST /web/settings/migration` — password-confirmed outbound `Move`.
pub async fn move_action(State(state): State<AppState>, user: WebUser, body: Bytes) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let locale = user.locale;
    if !crate::auth::verify_password_gated(
        field(&pairs, "current_password")
            .unwrap_or_default()
            .to_owned(),
        user.current.user.password_hash.clone(),
    )
    .await
    {
        return redirect_error(
            "/settings/migration",
            &locale.text("migration-error-password"),
        );
    }
    let target = field(&pairs, "acct").unwrap_or_default().trim();
    if target.is_empty() {
        return redirect_error("/settings/migration", &locale.text("migration-error-empty"));
    }
    match migration::migrate_local_account(&state, &user.current.account.username, target).await {
        Ok(_) => redirect_to("/settings/migration?saved=1"),
        Err(failure) => failure_response(failure, "/settings/migration", locale),
    }
}
