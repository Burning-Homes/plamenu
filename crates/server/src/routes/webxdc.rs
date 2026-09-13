//! Public FEP-752d actor/collection/package routes and the cookie-less app
//! runtime served from `webxdc.<domain>`.

use axum::Json;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use plamenu_db::webxdc;
use serde::Deserialize;
use serde_json::json;
use time::format_description::well_known::Rfc3339;

use super::ap_requested;
use crate::error::ApiError;
use crate::web::session::MaybeWebUser;
use crate::{AppState, webxdc as protocol};

fn activity_json(value: serde_json::Value) -> Response {
    (
        [
            (header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8),
            (header::VARY, "Accept"),
        ],
        Json(value),
    )
        .into_response()
}

fn vary_accept(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Accept"));
    response
}

async fn local_session(state: &AppState, id: i64) -> Result<webxdc::Session, ApiError> {
    if let Some(tombstone) = webxdc::tombstone_by_id(&state.pool, id).await? {
        let expected = format!("https://{}/webxdc/{id}", state.config.domain);
        return if tombstone.coordinator_uri == expected {
            Err(ApiError::Gone)
        } else {
            Err(ApiError::NotFound)
        };
    }
    let session = webxdc::find(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let expected = format!("https://{}/webxdc/{id}", state.config.domain);
    if session.coordinator_uri != expected {
        return Err(ApiError::NotFound);
    }
    Ok(session)
}

// Runtime assets also belong to locally cached remote sessions. Public
// ActivityPub routes still require this server to be the coordinator.
async fn runtime_session(state: &AppState, id: i64) -> Result<webxdc::Session, ApiError> {
    if webxdc::tombstone_by_id(&state.pool, id).await?.is_some() {
        return Err(ApiError::Gone);
    }
    webxdc::find(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)
}

pub async fn session(
    State(state): State<AppState>,
    MaybeWebUser(user): MaybeWebUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if let Some(tombstone) = webxdc::tombstone_by_id(&state.pool, id).await? {
        let expected = format!("https://{}/webxdc/{id}", state.config.domain);
        if tombstone.coordinator_uri != expected {
            return Err(ApiError::NotFound);
        }
        if ap_requested(&headers) {
            let deleted = tombstone.deleted_at.format(&Rfc3339).unwrap_or_default();
            return Ok((
                StatusCode::GONE,
                [
                    (header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8),
                    (header::VARY, "Accept"),
                ],
                Json(json!({
                    "@context": "https://www.w3.org/ns/activitystreams",
                    "id": tombstone.coordinator_uri,
                    "type": "Tombstone",
                    "formerType": ["Group", "WebxdcSession"],
                    "deleted": deleted,
                })),
            )
                .into_response());
        }
        return crate::web::webxdc::deleted_landing(&state, user, &tombstone, &headers)
            .await
            .map(vary_accept);
    }
    let session = local_session(&state, id).await?;
    if ap_requested(&headers) {
        return Ok(activity_json(
            protocol::actor_document(&state, &session).await?,
        ));
    }
    crate::web::webxdc::landing(&state, user, &session, &headers)
        .await
        .map(vary_accept)
}

pub async fn bundle(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    let session = local_session(&state, id).await?;
    Ok((
        [
            (header::CONTENT_TYPE, protocol::MEDIA_TYPE),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        webxdc::bundle(&state.pool, session.id)
            .await?
            .ok_or(ApiError::NotFound)?,
    )
        .into_response())
}

/// Public, operator-curated xdcget-compatible catalog. Extra integrity and
/// provenance fields are additive so existing catalog browsers can consume it
/// while Plamenu peers retain the locally verified digest.
pub async fn catalog(State(state): State<AppState>) -> Result<Response, ApiError> {
    let apps = webxdc::public_library(&state.pool).await?;
    let values: Vec<_> = apps
        .into_iter()
        .map(|app| {
            let bundle_url = format!(
                "https://{}/webxdc/catalog/version/{}/bundle.xdc",
                state.config.domain, app.version_id
            );
            let icon_url = app.icon_path.as_ref().map(|_| {
                format!(
                    "https://{}/webxdc/catalog/version/{}/icon",
                    state.config.domain, app.version_id
                )
            });
            json!({
                "app_id": format!("plamenu-{}", app.id),
                "tag_name": app.version,
                "url": bundle_url,
                "date": app.version_created_at.format(&Rfc3339).unwrap_or_default(),
                "description": app.summary,
                "source_code_url": app.source_code_url,
                "name": app.name,
                "category": app.category,
                "size": app.bundle_bytes,
                "icon": icon_url,
                "cache_relname": format!("catalog/version/{}/bundle.xdc", app.version_id),
                "icon_relname": app.icon_path.as_ref().map(|_| format!("catalog/version/{}/icon", app.version_id)),
                "digest_multibase": app.digest_multibase,
                "provenance": {
                    "kind": app.source_kind,
                    "source": app.version_source_url.or(app.source_url),
                },
            })
        })
        .collect();
    Ok((
        [
            (header::CONTENT_TYPE, "application/json; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=300"),
        ],
        Json(values),
    )
        .into_response())
}

pub async fn catalog_bundle(
    State(state): State<AppState>,
    Path(version_id): Path<i64>,
) -> Result<Response, ApiError> {
    let bytes = webxdc::public_library_bundle(&state.pool, version_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok((
        [
            (header::CONTENT_TYPE, protocol::MEDIA_TYPE),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        bytes,
    )
        .into_response())
}

pub async fn catalog_icon(
    State(state): State<AppState>,
    Path(version_id): Path<i64>,
) -> Result<Response, ApiError> {
    let asset = webxdc::library_icon(&state.pool, version_id, true)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok((
        [
            (header::CONTENT_TYPE, asset.media_type),
            (
                header::CACHE_CONTROL,
                "public, max-age=31536000, immutable".to_owned(),
            ),
        ],
        asset.bytes,
    )
        .into_response())
}

pub async fn outbox(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    let session = local_session(&state, id).await?;
    let updates = webxdc::all_updates(&state.pool, id).await?;
    let items: Vec<_> = updates
        .iter()
        .map(|update| protocol::announce_activity(&session, update))
        .collect();
    Ok(activity_json(json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}/outbox", session.coordinator_uri),
        "type": "OrderedCollection",
        "totalItems": items.len(),
        "orderedItems": items,
    })))
}

pub async fn followers(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    let session = local_session(&state, id).await?;
    let members = webxdc::accepted_participants(&state.pool, id).await?;
    let items: Vec<_> = members
        .into_iter()
        .map(|(membership, _)| membership.participant_uri)
        .collect();
    Ok(activity_json(json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}/followers", session.coordinator_uri),
        "type": "OrderedCollection",
        "totalItems": items.len(),
        "orderedItems": items,
    })))
}

fn runtime_session_id(state: &AppState, headers: &HeaderMap) -> Option<i64> {
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .and_then(|host| host.split(':').next())?;
    let suffix = format!(".{}", state.config.webxdc_domain());
    let label = host
        .strip_suffix(&suffix)
        .filter(|label| !label.is_empty() && !label.contains('.'))?;
    let id = label.parse::<i64>().ok()?;
    (host == state.config.webxdc_session_domain(id)).then_some(id)
}

fn runtime_host(state: &AppState, headers: &HeaderMap, session_id: i64) -> bool {
    runtime_session_id(state, headers) == Some(session_id)
}

#[derive(Deserialize, Default)]
pub struct RuntimeQuery {
    #[serde(default)]
    self_addr: String,
    #[serde(default)]
    self_name: String,
}

#[derive(Deserialize)]
pub struct CertificateQuery {
    domain: String,
}

/// Permission endpoint for Caddy On-Demand TLS. Only the exact dedicated
/// origin of an existing stored session is eligible, preventing arbitrary
/// certificate issuance through the HTTPS catch-all.
pub async fn certificate_allowed(
    State(state): State<AppState>,
    Query(query): Query<CertificateQuery>,
) -> Result<StatusCode, ApiError> {
    let suffix = format!(".{}", state.config.webxdc_domain());
    let label = query
        .domain
        .strip_suffix(&suffix)
        .filter(|label| !label.is_empty() && !label.contains('.'))
        .ok_or(ApiError::NotFound)?;
    let id = label.parse::<i64>().map_err(|_| ApiError::NotFound)?;
    runtime_session(&state, id).await?;
    if query.domain != state.config.webxdc_session_domain(id) {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn runtime_index(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if !runtime_host(&state, &headers, id) {
        return Err(ApiError::NotFound);
    }
    runtime_session(&state, id).await?;
    Ok(Redirect::temporary(&format!("/webxdc-runtime/{id}/index.html")).into_response())
}

pub async fn runtime_file(
    State(state): State<AppState>,
    Path((id, path)): Path<(i64, String)>,
    Query(query): Query<RuntimeQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if !runtime_host(&state, &headers, id) {
        return Err(ApiError::NotFound);
    }
    runtime_file_response(&state, id, &path, &query).await
}

async fn runtime_file_response(
    state: &AppState,
    id: i64,
    path: &str,
    query: &RuntimeQuery,
) -> Result<Response, ApiError> {
    let session = runtime_session(state, id).await?;
    let path = path.trim_start_matches('/');
    let file = webxdc::file(&state.pool, id, path)
        .await?
        .ok_or(ApiError::NotFound)?;
    let mut response = if file.media_type.starts_with("text/html") {
        let html = String::from_utf8(file.bytes)
            .map_err(|_| ApiError::BadRequest("Webxdc index.html is not UTF-8".into()))?;
        let bootstrap = format!(
            r"<script>window.__plamenuWebxdc={{session:{session_id},selfAddr:{addr},selfName:{name},sendUpdateInterval:{interval},sendUpdateMaxSize:{max_size},hostOrigin:{origin}}}</script>",
            session_id = serde_json::to_string(&id.to_string()).unwrap_or_else(|_| "\"\"".into()),
            addr = serde_json::to_string(&query.self_addr).unwrap_or_else(|_| "\"\"".into()),
            name = serde_json::to_string(&query.self_name).unwrap_or_else(|_| "\"\"".into()),
            interval = session.send_update_interval,
            max_size = session.send_update_max_size,
            origin = serde_json::to_string(&format!("https://{}", state.config.domain))
                .unwrap_or_else(|_| "\"\"".into()),
        );
        let html = if let Some(index) = html.to_ascii_lowercase().find("<head>") {
            let insert = index + "<head>".len();
            format!("{}{}{}", &html[..insert], bootstrap, &html[insert..])
        } else {
            format!("{bootstrap}{html}")
        };
        ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
    } else {
        let content_type = file.media_type.parse().map_err(|error| {
            ApiError::Internal(Box::new(std::io::Error::other(format!(
                "invalid stored Webxdc media type: {error}"
            ))))
        })?;
        let mut response = file.bytes.into_response();
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, content_type);
        response
    };
    response.headers_mut().insert(
        "x-plamenu-webxdc-runtime",
        "1".parse().expect("static header value"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        "no-store".parse().expect("static header value"),
    );
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        "no-referrer".parse().expect("static header value"),
    );
    Ok(response)
}

pub async fn bridge(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if !runtime_host(&state, &headers, id) {
        return Err(ApiError::NotFound);
    }
    bridge_response(&state, id).await
}

async fn bridge_response(state: &AppState, id: i64) -> Result<Response, ApiError> {
    runtime_session(state, id).await?;
    let body = include_str!("../web/assets/webxdc-bridge.js");
    let mut response = (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        body,
    )
        .into_response();
    response.headers_mut().insert(
        "x-plamenu-webxdc-runtime",
        "1".parse().expect("static header value"),
    );
    Ok(response)
}

fn percent_decode_path(path: &str) -> Option<String> {
    fn hex(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }

    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex(*bytes.get(index + 1)?)?;
            let low = hex(*bytes.get(index + 2)?)?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// A dedicated Webxdc origin is a package-only virtual host. Intercept it
/// before ordinary Plamenu routing so root-absolute package URLs such as
/// `/assets/app.js` work without exposing same-named server/API routes.
pub async fn runtime_origin_gate(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let Some(id) = runtime_session_id(&state, request.headers()) else {
        return next.run(request).await;
    };
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }

    let runtime_base = format!("/webxdc-runtime/{id}");
    let raw_path = request.uri().path();
    if raw_path == runtime_base {
        return Redirect::temporary(&format!("{runtime_base}/index.html")).into_response();
    }
    let package_path = if let Some(path) = raw_path.strip_prefix(&format!("{runtime_base}/")) {
        path
    } else if raw_path.starts_with("/webxdc-runtime/") {
        return ApiError::NotFound.into_response();
    } else {
        raw_path.trim_start_matches('/')
    };
    let package_path = if package_path.is_empty() {
        "index.html".to_owned()
    } else {
        match percent_decode_path(package_path) {
            Some(path) => path,
            None => return ApiError::NotFound.into_response(),
        }
    };

    if matches!(package_path.as_str(), "webxdc.js" | "_bridge.js") {
        return bridge_response(&state, id)
            .await
            .unwrap_or_else(IntoResponse::into_response);
    }
    let query = request
        .uri()
        .query()
        .and_then(|raw| serde_urlencoded::from_str(raw).ok())
        .unwrap_or_default();
    runtime_file_response(&state, id, &package_path, &query)
        .await
        .unwrap_or_else(IntoResponse::into_response)
}
