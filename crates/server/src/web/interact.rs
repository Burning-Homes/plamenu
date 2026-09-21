//! The remote-interaction interstitial — Mastodon's
//! `/authorize_interaction` flow, both directions:
//!
//! * **Inbound:** another server's remote-follow button resolves our JRD's `http://ostatus.org/schema/1.0/subscribe`
//!   template, which points here. A signed-in local lands on our copy of the object; everyone else
//!   gets the handle form.
//! * **Outbound:** the follow/reply/boost affordances on our public pages send a logged-out visitor
//!   here. They enter their own `user@example.org` handle, we resolve their home server's subscribe
//!   template over `WebFinger`, substitute the object's URI and send them home to finish.
//!
//! The POST is anonymous by design — no session, no CSRF (nothing changes
//! server-side; the outcome is a redirect). It can trigger one outbound
//! `WebFinger` fetch, so the rate limiter counts it against the per-IP
//! remote-ingress budget like the other dereferencing surfaces.

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use fluent_bundle::FluentArgs;
use maud::{Markup, html};
use plamenu_ap::acct::Acct;
use plamenu_db::account;
use serde::Deserialize;

use super::i18n::Locale;
use super::layout;
use super::pages::{account_local_path, anon_nav, not_found, status_local_path};
use super::session::MaybeWebUser;
use crate::AppState;
use crate::error::ApiError;
use crate::routes::search::{KnownUrl, UrlResource, resolve_url, resolve_url_known};

/// Sanity cap on the `uri` parameter — a federated object id, not a payload.
const MAX_URI_LEN: usize = 2048;
/// Sanity cap on the visitor-typed handle.
const MAX_HANDLE_LEN: usize = 255;

#[derive(Deserialize)]
pub struct InteractQuery {
    #[serde(default)]
    uri: String,
}

#[derive(Deserialize)]
pub struct InteractForm {
    #[serde(default)]
    uri: String,
    #[serde(default)]
    handle: String,
}

/// What the form is about, for the lead-in sentence: the local copy of the
/// object when we have one.
enum Subject {
    Account { acct: String, path: String },
    Status { acct: String, path: String },
    Unknown,
}

async fn subject_for(state: &AppState, uri: &str) -> Subject {
    match resolve_url_known(state, uri, None).await {
        Ok(KnownUrl::Found(UrlResource::Account(found))) => Subject::Account {
            path: account_local_path(&found, &state.config.domain),
            acct: crate::entities::account_acct(&state.config.domain, &found),
        },
        Ok(KnownUrl::Found(UrlResource::Status(found))) => {
            let acct = match account::find_by_id(&state.pool, found.account_id).await {
                Ok(Some(author)) => crate::entities::account_acct(&state.config.domain, &author),
                _ => String::new(),
            };
            match status_local_path(state, &found).await {
                Ok(path) => Subject::Status { acct, path },
                Err(_) => Subject::Unknown,
            }
        }
        _ => Subject::Unknown,
    }
}

async fn interact_page(
    state: &AppState,
    uri: &str,
    subject: &Subject,
    handle: &str,
    errors: &[String],
    locale: Locale,
) -> Markup {
    let bold_handle = |acct: &str| html! { strong { "@" (acct) } };
    let (lead, back): (Markup, Option<&str>) = match subject {
        Subject::Account { acct, path } => (
            locale.markup("interact-lead-account", &[("handle", bold_handle(acct))]),
            Some(path),
        ),
        Subject::Status { acct, path } if !acct.is_empty() => (
            locale.markup("interact-lead-status-by", &[("handle", bold_handle(acct))]),
            Some(path),
        ),
        Subject::Status { path, .. } => {
            (html! { (locale.text("interact-lead-status")) }, Some(path))
        }
        Subject::Unknown => (html! { (locale.text("interact-lead-unknown")) }, None),
    };
    let sign_in = html! { a href="/login" { (locale.text("nav-sign-in")) } };
    let content = html! {
        section.auth-card {
            h1 { (locale.text("interact-heading")) }
            p { (lead) }
            p { (locale.text("interact-explainer")) }
            @if !errors.is_empty() {
                ul.form-error id="interact-errors" role="alert" {
                    @for error in errors { li { (error) } }
                }
            }
            form.auth-form method="post" action="/interact" {
                input type="hidden" name="uri" value=(uri);
                label {
                    (locale.text("interact-handle-label"))
                    input type="text" name="handle" value=(handle)
                        placeholder="user@example.org" autocomplete="off" required autofocus
                        aria-invalid=[(!errors.is_empty()).then_some("true")]
                        aria-describedby=(if errors.is_empty() {
                            "interact-handle-hint"
                        } else {
                            "interact-handle-hint interact-errors"
                        });
                    span.settings-field__hint id="interact-handle-hint" {
                        (locale.text("interact-handle-hint"))
                    }
                }
                button type="submit" { (locale.text("interact-submit")) }
            }
            p.auth-card__alt {
                (locale.markup("interact-have-account", &[("signin", sign_in)]))
                @if let Some(path) = back {
                    " · " a href=(path) { (locale.text("common-back")) }
                }
            }
        }
    };
    layout::shell_visitor_localized(
        &locale.text("interact-title"),
        None,
        anon_nav(state).await,
        &content,
        locale,
    )
}

/// `GET /interact?uri=…`.
pub async fn page(
    State(state): State<AppState>,
    MaybeWebUser(session): MaybeWebUser,
    request_locale: Locale,
    Query(query): Query<InteractQuery>,
) -> Result<Response, ApiError> {
    let uri = query.uri.trim();
    if uri.is_empty() || uri.len() > MAX_URI_LEN {
        return Ok(not_found(&state, session.as_ref(), request_locale).await);
    }
    if let Some(user) = &session {
        // A signed-in local needs no interstitial: resolve (fetching if
        // unknown) and land on our copy; fall back to the search page, which
        // renders its own empty state.
        let viewer = Some(user.current.account.id);
        if let Some(found) = Box::pin(resolve_url(&state, uri, viewer)).await? {
            let path = match found {
                UrlResource::Account(found) => account_local_path(&found, &state.config.domain),
                UrlResource::Status(found) => status_local_path(&state, &found).await?,
            };
            return Ok(Redirect::to(&path).into_response());
        }
        let query = serde_urlencoded::to_string([("q", uri)]).unwrap_or_default();
        return Ok(Redirect::to(&format!("/search?{query}")).into_response());
    }
    let subject = subject_for(&state, uri).await;
    Ok(
        interact_page(&state, uri, &subject, "", &[], request_locale)
            .await
            .into_response(),
    )
}

/// `POST /interact` — resolve the visitor's home server and send them there.
pub async fn submit(
    State(state): State<AppState>,
    request_locale: Locale,
    axum::extract::Form(form): axum::extract::Form<InteractForm>,
) -> Result<Response, ApiError> {
    let uri = form.uri.trim();
    if uri.is_empty() || uri.len() > MAX_URI_LEN {
        return Ok(not_found(&state, None, request_locale).await);
    }
    let raw_handle = form.handle.trim().trim_start_matches('@');
    let state_ref = &state;
    // The refusal renders straight back into this response — it never crosses
    // a redirect — so it can travel as finished localized text.
    let rerender = |errors: Vec<String>| async move {
        let subject = subject_for(state_ref, uri).await;
        Ok(interact_page(
            state_ref,
            uri,
            &subject,
            raw_handle,
            &errors,
            request_locale,
        )
        .await
        .into_response())
    };
    let domain_error = |id: &str, domain: &str| {
        let mut args = FluentArgs::new();
        args.set("domain", domain.to_owned());
        request_locale.text_with(id, &args)
    };
    if raw_handle.is_empty() || raw_handle.len() > MAX_HANDLE_LEN {
        return rerender(vec![request_locale.text("interact-error-enter-handle")]).await;
    }
    let Ok(acct) = raw_handle.parse::<Acct>() else {
        let mut args = FluentArgs::new();
        args.set("handle", raw_handle);
        return rerender(vec![
            request_locale.text_with("interact-error-not-a-handle", &args),
        ])
        .await;
    };
    if state.config.is_local_domain(acct.domain()) {
        // Their home is right here: interacting just needs a session.
        return Ok(Redirect::to("/login").into_response());
    }
    let resolved = match state.federation.resolve_acct(&acct).await {
        Ok(resolved) => resolved,
        Err(error) => {
            tracing::debug!(%acct, %error, "remote-interaction webfinger failed");
            return rerender(vec![domain_error(
                "interact-error-unreachable",
                acct.domain(),
            )])
            .await;
        }
    };
    let Some(template) = resolved
        .subscribe_template
        .filter(|template| template.contains("{uri}"))
    else {
        return rerender(vec![domain_error(
            "interact-error-no-endpoint",
            acct.domain(),
        )])
        .await;
    };
    let encoded: String = url::form_urlencoded::byte_serialize(uri.as_bytes()).collect();
    let target = template.replace("{uri}", &encoded);
    if !(target.starts_with("https://") || target.starts_with("http://")) {
        return rerender(vec![domain_error(
            "interact-error-bad-endpoint",
            acct.domain(),
        )])
        .await;
    }
    Ok(Redirect::to(&target).into_response())
}

/// The `/interact?uri=…` link the public pages' anonymous affordances point
/// at.
pub(super) fn interact_href(uri: &str) -> String {
    let query = serde_urlencoded::to_string([("uri", uri)]).unwrap_or_default();
    format!("/interact?{query}")
}

/// The anonymous stand-in for the profile follow button: an accent-styled
/// anchor into the interstitial.
pub(super) fn follow_via_interact(uri: &str, locale: Locale) -> Markup {
    html! {
        a.follow-remote href=(interact_href(uri)) { (locale.text("profile-follow")) }
    }
}
