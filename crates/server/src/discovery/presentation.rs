//! Server branding is acquired once per month, through the ordinary discovery
//! wire budget. Browsing serves a small local PNG and never calls the origin.
use super::transport;
use crate::AppState;
use plamenu_db::discovery_store::{self as db, Lease};
use plamenu_federation::{DiscoveryFormat, FederationError};

fn database(e: &plamenu_db::DbError) -> FederationError {
    FederationError::Admission(e.to_string())
}
fn transient(e: &FederationError) -> bool {
    matches!(
        e,
        FederationError::DiscoveryDeferred(_)
            | FederationError::DiscoveryStale
            | FederationError::RateLimited { .. }
    )
}

fn advertised_icon(document: &serde_json::Value, base: &url::Url) -> Option<String> {
    let raw = [
        "/site_view/site/icon",
        "/instance/avatars/0/path",
        "/instance/avatars/0/fileUrl",
        "/icon/0/src",
        "/thumbnail/url",
    ]
    .iter()
    .find_map(|p| document.pointer(p).and_then(serde_json::Value::as_str));
    raw.and_then(|u| base.join(u).ok())
        .filter(|u| {
            matches!(u.scheme(), "http" | "https")
                && u.as_str().len() <= 2048
                && u.fragment().is_none()
                && u.username().is_empty()
                && u.password().is_none()
        })
        .map(String::from)
}

pub(super) async fn server_icon(state: &AppState, lease: &Lease) -> Result<(), FederationError> {
    if lease.search_run_id.is_some() || lease.exploration {
        return Ok(());
    }
    let settings = db::settings(&state.pool).await.map_err(|e| database(&e))?;
    if settings
        .disabled_mechanisms
        .iter()
        .any(|m| m == "metadata_cache")
    {
        return Ok(());
    }
    let Some(source) = db::source(&state.pool, lease.source_id)
        .await
        .map_err(|e| database(&e))?
    else {
        return Ok(());
    };
    if source.verified_at.is_none() {
        return Ok(());
    }
    let cached = db::server_icon(&state.pool, source.id)
        .await
        .map_err(|e| database(&e))?;
    if cached.as_ref().is_some_and(|c| !c.due) {
        return Ok(());
    }
    let base =
        url::Url::parse(&source.origin).map_err(|e| FederationError::InvalidUrl(e.to_string()))?;
    let mut uri = cached.and_then(|c| c.icon_uri);
    if uri.is_none() {
        let path = if source
            .capabilities
            .iter()
            .any(|c| c == "community_directory")
        {
            "/api/v3/site"
        } else if source.capabilities.iter().any(|c| c == "video_catalogue") {
            "/api/v1/config"
        } else {
            "/api/v2/instance"
        };
        match transport::fetch(
            state,
            lease,
            base.join(path).unwrap().as_str(),
            DiscoveryFormat::Json,
        )
        .await
        {
            Ok(r) => {
                uri = advertised_icon(&r.document, &base);
            }
            Err(e) if transient(&e) => return Err(e),
            Err(_) => {}
        }
        if uri.is_none() {
            uri = Some(base.join("/favicon.ico").unwrap().into());
        }
        db::save_server_icon(&state.pool, lease, uri.as_deref(), None, "pending", 0)
            .await
            .map_err(|e| database(&e))?;
    }
    let mut uri = uri.unwrap();
    let fallback = base.join("/favicon.ico").unwrap().to_string();
    let mut png = None;
    for attempt in 0..2 {
        let image = match transport::fetch(state, lease, &uri, DiscoveryFormat::Image).await {
            Ok(r) => r.image,
            Err(e) if transient(&e) => return Err(e),
            Err(_) => None,
        };
        png = rasterize(image).await;
        if png.is_some() || uri == fallback || attempt == 1 {
            break;
        }
        uri = fallback.clone();
        db::save_server_icon(&state.pool, lease, Some(&uri), None, "pending", 0)
            .await
            .map_err(|e| database(&e))?;
    }
    db::save_server_icon(
        &state.pool,
        lease,
        Some(&uri),
        png.as_deref(),
        if png.is_some() {
            "resolved"
        } else {
            "unavailable"
        },
        if png.is_some() { 720 } else { 24 },
    )
    .await
    .map_err(|e| database(&e))?;
    Ok(())
}

async fn rasterize(image: Option<Vec<u8>>) -> Option<Vec<u8>> {
    if let Some(bytes) = image {
        tokio::task::spawn_blocking(move || {
            let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
                .with_guessed_format()
                .ok()?;
            let mut limits = image::Limits::default();
            limits.max_image_width = Some(4096);
            limits.max_image_height = Some(4096);
            limits.max_alloc = Some(32 * 1024 * 1024);
            reader.limits(limits);
            let small = reader.decode().ok()?.thumbnail(96, 96);
            let mut png = std::io::Cursor::new(Vec::new());
            small.write_to(&mut png, image::ImageFormat::Png).ok()?;
            Some(png.into_inner())
        })
        .await
        .ok()
        .flatten()
    } else {
        None
    }
}
