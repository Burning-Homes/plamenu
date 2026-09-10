//! `/.well-known/*` discovery endpoints and nodeinfo documents.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};
use plamenu_ap::acct::Acct;
use plamenu_ap::nodeinfo::{NodeInfo, NodeInfoIndex, NodeInfoStats};
use plamenu_ap::webfinger::{HostMeta, Jrd};
use plamenu_db::account;
use serde::Deserialize;

use crate::error::ApiError;
use crate::{AppState, VERSION};

#[derive(Deserialize)]
pub struct WebfingerQuery {
    resource: String,
}

/// Whether a webfinger resource names the instance actor: the bare domain
/// (optionally as a URL), `domain@domain`, or the `/actor` URL — the same
/// forms Mastodon's `WebfingerResource` resolves to its representative.
fn is_instance_actor_resource(resource: &str, account_domain: &str, actor_domain: &str) -> bool {
    let resource = resource.strip_prefix("acct:").unwrap_or(resource);
    if let Some((username, domain)) = resource.split_once('@') {
        let local_username = username.eq_ignore_ascii_case(account_domain)
            || username.eq_ignore_ascii_case(actor_domain);
        let local_domain = domain.eq_ignore_ascii_case(account_domain)
            || domain.eq_ignore_ascii_case(actor_domain);
        if local_username && local_domain {
            return true;
        }
    }
    let resource = resource
        .strip_prefix("https://")
        .or_else(|| resource.strip_prefix("http://"))
        .unwrap_or(resource);
    let resource = resource.strip_suffix('/').unwrap_or(resource);
    resource.eq_ignore_ascii_case(account_domain)
        || resource.eq_ignore_ascii_case(actor_domain)
        || resource.eq_ignore_ascii_case(&format!("{actor_domain}/actor"))
}

pub async fn webfinger(
    State(state): State<AppState>,
    Query(query): Query<WebfingerQuery>,
) -> Result<impl IntoResponse, ApiError> {
    if is_instance_actor_resource(
        &query.resource,
        &state.config.account_domain,
        &state.config.domain,
    ) {
        return Ok((
            [(header::CONTENT_TYPE, plamenu_ap::JRD_JSON_UTF8)],
            Json(Jrd::for_instance_actor_on_domain(
                &state.config.account_domain,
                &state.config.domain,
            )),
        ));
    }
    let acct: Acct = query
        .resource
        .parse()
        .map_err(|e: plamenu_ap::acct::AcctError| ApiError::BadRequest(e.to_string()))?;
    if !state.config.is_local_domain(acct.domain()) {
        return Err(ApiError::NotFound);
    }
    let Some(account) =
        account::find_public_local_account_by_username(&state.pool, acct.username()).await?
    else {
        return Err(ApiError::NotFound);
    };
    // A deleted account's JRD is gone; a reversibly suspended one still
    // answers (Mastodon 410s only `permanently_unavailable?` here).
    if crate::moderation::permanently_unavailable(&state.pool, &account).await? {
        return Err(ApiError::Gone);
    }
    // Answer with the stored username casing, not the query's.
    let canonical = Acct::new(&account.username, &state.config.account_domain)
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let mut jrd =
        Jrd::for_local_actor_on_domain(&canonical, &state.config.domain, account.uri.as_deref());
    // Mastodon appends a rel=avatar link when the account has one.
    if let Some(file) = &account.avatar_file_name {
        jrd = jrd.with_avatar(
            crate::media_processing::content_type_for(file),
            format!("https://{}/media/{file}", state.config.domain),
        );
    }
    Ok((
        [(header::CONTENT_TYPE, plamenu_ap::JRD_JSON_UTF8)],
        Json(jrd),
    ))
}

pub async fn host_meta(State(state): State<AppState>, headers: HeaderMap) -> Response {
    // Mastodon answers JSON only when the Accept header leads with
    // application/json; everything else (including */*) gets the XRD form.
    let leads_with_json = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .and_then(|accept| accept.split(',').next())
        .and_then(|item| item.split(';').next())
        .is_some_and(|mime| mime.trim() == "application/json");
    if leads_with_json {
        return host_meta_json(State(state)).await.into_response();
    }
    (
        [(header::CONTENT_TYPE, plamenu_ap::XRD_XML_UTF8)],
        HostMeta::for_domain(&state.config.domain).to_xml(),
    )
        .into_response()
}

pub async fn host_meta_json(State(state): State<AppState>) -> Json<HostMeta> {
    Json(HostMeta::for_domain(&state.config.domain))
}

/// `GET /.well-known/oauth-authorization-server` — RFC 8414 authorization
/// server metadata, Mastodon's `WellKnown::OAuthMetadataController`.
///
/// Clients read it to decide *how* to authorize before they hold any token.
/// Phanpy gates PKCE on `code_challenge_methods_supported` containing `S256`
/// and silently falls back to the plain code flow when the document is absent
/// — so a server that implements PKCE but never advertises it never gets
/// asked for it. Every field below describes what this server actually does,
/// not what Mastodon does: the token endpoint takes client credentials in the
/// request body only, and there is no refresh-token or implicit grant.
pub async fn oauth_metadata(State(state): State<AppState>) -> impl IntoResponse {
    let domain = &state.config.domain;
    let body = serde_json::json!({
        "issuer": format!("https://{domain}/"),
        "authorization_endpoint": format!("https://{domain}/oauth/authorize"),
        "token_endpoint": format!("https://{domain}/oauth/token"),
        "revocation_endpoint": format!("https://{domain}/oauth/revoke"),
        "scopes_supported": crate::oauth_app::SUPPORTED_SCOPES,
        "response_types_supported": ["code"],
        "response_modes_supported": ["query"],
        "grant_types_supported": ["authorization_code", "client_credentials"],
        "token_endpoint_auth_methods_supported": ["client_secret_post"],
        "code_challenge_methods_supported": ["S256"],
        "service_documentation": crate::SOURCE_URL,
        // Non-standard, like Mastodon's: `POST /api/v1/apps` predates and does
        // not conform to RFC 7591 dynamic client registration.
        "app_registration_endpoint": format!("https://{domain}/api/v1/apps"),
    });
    // Mastodon caches this for 15 minutes; the contents only change with the
    // build, so the same window is safe here.
    ([(header::CACHE_CONTROL, "public, max-age=900")], Json(body))
}

pub async fn nodeinfo_index(State(state): State<AppState>) -> impl IntoResponse {
    // Mastodon caches the discovery document for 3 days.
    (
        [(header::CACHE_CONTROL, "public, max-age=259200")],
        Json(NodeInfoIndex::for_domain(&state.config.domain)),
    )
}

pub async fn nodeinfo_20(State(state): State<AppState>) -> Result<Response, ApiError> {
    nodeinfo(&state, "2.0").await
}

pub async fn nodeinfo_21(State(state): State<AppState>) -> Result<Response, ApiError> {
    nodeinfo(&state, "2.1").await
}

async fn nodeinfo(state: &AppState, schema: &str) -> Result<Response, ApiError> {
    let now = time::OffsetDateTime::now_utc();
    let settings = plamenu_db::instance_settings::get(&state.pool).await?;
    let active = |days: i64| {
        plamenu_db::metrics::active_users_total(&state.pool, now - time::Duration::days(days), now)
    };
    let counters = NodeInfoStats {
        total_users: account::count_public_local(&state.pool).await?,
        active_month: u64::try_from(active(30).await?).unwrap_or(0),
        active_halfyear: u64::try_from(active(180).await?).unwrap_or(0),
        local_posts: plamenu_db::status::count_local(&state.pool).await?,
        open_registrations: settings.registrations_mode()
            == plamenu_db::instance_settings::RegistrationsMode::Open,
    };
    let mut info = NodeInfo::for_plamenu(schema, VERSION, counters);
    // The Pleroma/Mitra-convention instance identity keys crawlers read
    // (fediverse.observer, FediDB) — FEP-0151 instance metadata.
    info.metadata.insert(
        "nodeName".to_owned(),
        serde_json::json!(settings.site_title),
    );
    info.metadata.insert(
        "nodeDescription".to_owned(),
        serde_json::json!(settings.site_short_description),
    );
    // Pleroma advertises the accepted rich-text formats here too (P4);
    // pleroma-fe and friends read `metadata.postFormats`.
    info.metadata.insert(
        "postFormats".to_owned(),
        serde_json::json!(crate::compose::PostFormat::ADVERTISED),
    );
    // Mastodon serves nodeinfo with a 30-minute public cache; the counts
    // above are full-table aggregates, so keep this header in place.
    Ok((
        [(header::CACHE_CONTROL, "public, max-age=1800")],
        Json(info),
    )
        .into_response())
}
