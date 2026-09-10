//! O6: the federation debug page — the web face of `plamenu federation fetch`
//! (signed, permalink-following GET of a remote AP object, dumped as pretty
//! JSON) and `plamenu federation webfinger` (a database-free handle resolve
//! listing every advertised actor). Both probes are read-only GET forms (the
//! canonical-e-mail-test precedent), so a result is refreshable and its URL
//! shareable between moderators. The fetches ride the regular federation
//! client: signed as the instance actor, SSRF-guarded, bounded timeouts.

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use maud::{Markup, html};
use plamenu_ap::acct::Acct;
use plamenu_db::role::permission;
use plamenu_federation::FederationError;
use serde::Deserialize;

use super::{WebAdmin, admin_shell};
use crate::AppState;

#[derive(Deserialize)]
pub struct DebugQuery {
    url: Option<String>,
    acct: Option<String>,
}

/// `GET /admin/federation-debug` — run whichever probe the query names.
pub async fn index(
    State(state): State<AppState>,
    admin: WebAdmin,
    Query(query): Query<DebugQuery>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_FEDERATION)?;

    let url = query
        .url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty());
    let acct = query
        .acct
        .as_deref()
        .map(str::trim)
        .map(|a| a.trim_start_matches('@'))
        .filter(|a| !a.is_empty());

    let fetch_result = match url {
        Some(url) => Some(fetch_section(&state, url).await),
        None => None,
    };
    let webfinger_result = match acct {
        Some(acct) => Some(webfinger_outcome(&state, acct).await),
        None => None,
    };

    let body = html! {
        p.admin__lead {
            "Read-only probes against other servers, as this server sees them "
            "— the web face of "
            code { "plamenu federation fetch" }
            " and "
            code { "plamenu federation webfinger" }
            ". Requests are signed as the instance actor."
        }
        section.admin-list {
            h3 { "Fetch an object" }
            form.admin-filter method="get" action="/admin/federation-debug" {
                label {
                    "Object or permalink URL"
                    input type="url" name="url" value=(url.unwrap_or_default())
                        placeholder="https://example.social/@someone/123";
                }
                button type="submit" { "Fetch" }
            }
            p.settings-field__hint {
                "A signed ActivityPub GET that follows a human permalink to "
                "its canonical object — paste a status or profile URL to see "
                "the JSON the origin serves us."
            }
            @if let Some(result) = fetch_result { (result) }
        }
        section.admin-list {
            h3 { "Resolve a handle" }
            form.admin-filter method="get" action="/admin/federation-debug" {
                label {
                    "Handle"
                    input type="text" name="acct" value=(acct.unwrap_or_default())
                        placeholder="someone@example.social";
                }
                button type="submit" { "Resolve" }
            }
            p.settings-field__hint {
                "WebFinger only — lists every actor the home server advertises "
                "for the handle without creating or touching any local record."
            }
            @if let Some(result) = webfinger_result { (result) }
        }
    };
    Ok(admin_shell(&admin, "/admin/federation-debug", "Federation debug", &body).into_response())
}

/// The signed-fetch probe outcome: pretty JSON or the client's error. A
/// budget-suppressed target is probed a second time past the budget — this
/// page exists to show the *underlying* failure, and the pre-flight refusal
/// alone says nothing about it. The probe records its outcome like any fetch,
/// so a success also clears the suppression.
async fn fetch_section(state: &AppState, url: &str) -> Markup {
    let suppression = match state.federation.fetch_object_following(url).await {
        Ok(object) => return fetched_json(url, &object, None),
        Err(FederationError::FetchSuppressed(key)) => key,
        Err(error) => {
            return html! {
                p.admin-flash.is-error role="alert" {
                    "Fetch failed: " (error.to_string())
                }
            };
        }
    };
    match state
        .federation
        .fetch_object_following_ignoring_budget(url)
        .await
    {
        Ok(object) => fetched_json(url, &object, Some(&suppression)),
        Err(error) => html! {
            p.admin-flash.is-error role="alert" {
                "Fetch suppressed by the finite failure budget for "
                code { (suppression) }
                " — probed past it for this page, and the underlying failure is:"
            }
            p.admin-flash.is-error role="alert" {
                (error.to_string())
            }
        },
    }
}

fn fetched_json(
    url: &str,
    object: &serde_json::Value,
    cleared_suppression: Option<&str>,
) -> Markup {
    let pretty = serde_json::to_string_pretty(object).unwrap_or_else(|_| object.to_string());
    html! {
        @if let Some(key) = cleared_suppression {
            p.admin-flash role="status" {
                "The finite failure budget for " code { (key) }
                " had suppressed this fetch, but probing past it succeeded — "
                "the suppression is now cleared."
            }
        }
        p.admin-flash role="status" { "Fetched " code { (url) } "." }
        pre.admin-debug__json { code { (pretty) } }
    }
}

/// The `WebFinger` probe outcome: the resolved pick plus every candidate.
async fn webfinger_outcome(state: &AppState, raw: &str) -> Markup {
    let parsed: Acct = match raw.parse() {
        Ok(parsed) => parsed,
        Err(error) => {
            return html! {
                p.admin-flash.is-error role="alert" {
                    code { (raw) } " is not a valid handle: " (error.to_string())
                }
            };
        }
    };
    match state.federation.resolve_acct(&parsed).await {
        Ok(resolved) => html! {
            p.admin-flash role="status" {
                code { (resolved.acct.to_string()) } " resolves to "
                code { (resolved.actor_uri) }
            }
            (crate::web::view::data_table(&html! {
                thead {
                    tr {
                        th scope="col" { "Advertised actor" }
                        th scope="col" { "Type" }
                    }
                }
                tbody {
                    @for candidate in &resolved.candidates {
                        tr {
                            td { code { (candidate.actor_uri) } }
                            td.is-tight {
                                (candidate.advertised_type.as_deref().unwrap_or("(unspecified)"))
                            }
                        }
                    }
                }
            }))
            @if let Some(template) = &resolved.subscribe_template {
                p.settings-field__hint {
                    "Remote-interaction template: " code { (template) }
                }
            }
        },
        Err(error) => html! {
            p.admin-flash.is-error role="alert" {
                "Resolve failed: " (error.to_string())
            }
        },
    }
}
