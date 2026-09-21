//! Static assets, embedded into the binary.
//!
//! The stylesheet, enhancement script and install imagery are compiled in
//! rather than served from disk, so the release artifact stays a single
//! self-contained static binary (matching the musl deploy). CSS and JavaScript
//! are served behind a versioned URL, so a long-lived `immutable` cache is safe.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::LazyLock;

use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use maud::{Markup, html};

const APP_CSS: &str = include_str!("assets/app.css");
const APP_JS: &str = include_str!("assets/app.js");
/// ALTCHA widget v3.2.3, pinned and vendored from the official npm package.
/// Kept separate because only the sign-up page needs its Web Component and
/// workers; ordinary browsing should not download CAPTCHA code.
const ALTCHA_JS: &[u8] = include_bytes!("assets/altcha.min.js");
pub(crate) const ALTCHA_VERSION: &str = "3.2.3";
/// Unicode Emoji 17.0's complete RGI set, kept out of `app.js` so an ordinary
/// timeline never downloads the 3,953-entry reaction catalog. The picker
/// fetches this versioned asset only when it opens; the no-JS picker reads the
/// same bytes server-side.
pub(crate) const EMOJI_CATALOG: &str = include_str!("assets/emoji-17.0.json");
pub(crate) const EMOJI_CATALOG_PATH: &str = "/assets/emoji-17.0.json";
const WEBXDC_HOST_JS: &str = include_str!("assets/webxdc-host.js");
/// The install/offline and Web Push service worker. Served at `/sw.js` —
/// root scope, and a stable URL because a service-worker URL cannot carry a
/// cache-buster (the browser refetches it by URL on its own schedule).
const SW_JS: &str = include_str!("assets/sw.js");
/// Installation metadata consumed by Chromium, Safari, Firefox and desktop
/// shells. It stays at a stable URL so installed copies can check for updates.
const WEB_MANIFEST: &str = r##"{
  "name": "Plamenu",
  "short_name": "Plamenu",
  "description": "A fast, focused client for the connected social web.",
  "id": "/",
  "start_url": "/",
  "scope": "/",
  "display": "standalone",
  "background_color": "#0f1419",
  "theme_color": "#0f1419",
  "categories": ["social"],
  "icons": [
    {
      "src": "/pwa/icon-192.png",
      "sizes": "192x192",
      "type": "image/png",
      "purpose": "any"
    },
    {
      "src": "/pwa/icon-512.png",
      "sizes": "512x512",
      "type": "image/png",
      "purpose": "any"
    },
    {
      "src": "/pwa/icon-maskable-192.png",
      "sizes": "192x192",
      "type": "image/png",
      "purpose": "maskable"
    },
    {
      "src": "/pwa/icon-maskable-512.png",
      "sizes": "512x512",
      "type": "image/png",
      "purpose": "maskable"
    }
  ],
  "shortcuts": [
    {
      "name": "New post",
      "short_name": "Post",
      "description": "Write a new post",
      "url": "/compose"
    },
    {
      "name": "Notifications",
      "short_name": "Notifications",
      "description": "Open your notifications",
      "url": "/notifications"
    },
    {
      "name": "Search",
      "short_name": "Search",
      "description": "Search Plamenu",
      "url": "/search"
    }
  ]
}"##;

/// PNGs used by install surfaces. Separate `any` and full-bleed `maskable`
/// icons keep the mark crisp whether the OS displays a square, circle or
/// squircle. All remain embedded so the release is one binary.
const PWA_IMAGES: &[(&str, &[u8])] = &[
    (
        "apple-touch-icon.png",
        include_bytes!("assets/pwa/apple-touch-icon.png"),
    ),
    ("badge-96.png", include_bytes!("assets/pwa/badge-96.png")),
    ("icon-192.png", include_bytes!("assets/pwa/icon-192.png")),
    ("icon-512.png", include_bytes!("assets/pwa/icon-512.png")),
    (
        "icon-maskable-192.png",
        include_bytes!("assets/pwa/icon-maskable-192.png"),
    ),
    (
        "icon-maskable-512.png",
        include_bytes!("assets/pwa/icon-maskable-512.png"),
    ),
];
/// hls.js (full build, v1.6.16) — the browser HLS player for `PeerTube`-style
/// video. Vendored and served same-origin (no CDN) so the release stays a
/// self-contained static binary; app.js loads it lazily on the first play.
/// The full build (not `light`) is required: `PeerTube`'s separated audio is an
/// `EXT-X-MEDIA:TYPE=AUDIO` alternate track, which the light build omits.
const HLS_JS: &[u8] = include_bytes!("assets/hls.min.js");
/// The default avatar/header placeholder, served at `/static/missing.png` —
/// the URL `entities.rs` hands out for accounts with no image.
const MISSING_IMAGE: &[u8] = include_bytes!("assets/missing.png");
/// The default favicon — the sidebar's "P" brand mark as a 48×48 PNG — served
/// until the operator uploads one. Every page links `/favicon.ico`, so the
/// route must never 404.
const DEFAULT_FAVICON: &[u8] = include_bytes!("assets/favicon.png");

/// Cache-buster for the asset URLs and service-worker shell, derived from the
/// **content** of the bundled client rather than the crate version.
///
/// The assets are served `immutable`, so the `?v=` query the shell appends is
/// what forces a browser to refetch. Keying it on `CARGO_PKG_VERSION` meant two
/// staging deploys at the same crate version shared a buster and the browser
/// kept the stale stylesheet. Hashing the actual bytes changes the buster
/// exactly when the install surface changes. `DefaultHasher` uses fixed keys,
/// so the value is stable for a given input across runs.
pub static ASSET_VERSION: LazyLock<String> = LazyLock::new(|| {
    let mut hasher = DefaultHasher::new();
    APP_CSS.hash(&mut hasher);
    APP_JS.hash(&mut hasher);
    ALTCHA_JS.hash(&mut hasher);
    EMOJI_CATALOG.hash(&mut hasher);
    SW_JS.hash(&mut hasher);
    WEB_MANIFEST.hash(&mut hasher);
    for (_, bytes) in PWA_IMAGES {
        bytes.hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
});

/// One year, `immutable` — correctness relies on the `?v=` cache-buster the
/// shell appends, which changes with every release.
const CACHE: &str = "public, max-age=31536000, immutable";

pub async fn css() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, CACHE),
        ],
        APP_CSS,
    )
}

pub async fn js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, CACHE),
        ],
        APP_JS,
    )
}

pub async fn altcha_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, CACHE),
        ],
        ALTCHA_JS,
    )
}

/// The full Unicode reaction palette. Its version is part of the URL, so it
/// can live in the browser's immutable cache across pages and deployments.
/// It is intentionally absent from the service-worker shell cache: people who
/// never open a reaction picker should never pay for it.
pub async fn emoji_catalog() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/json; charset=utf-8"),
            (header::CACHE_CONTROL, CACHE),
        ],
        EMOJI_CATALOG,
    )
}

pub async fn webxdc_host_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        WEBXDC_HOST_JS,
    )
}

/// `GET /sw.js` — the install/offline and Web Push worker. `no-cache` so an
/// update propagates on the browser's next registration check instead of
/// waiting a year behind the immutable policy above.
pub async fn sw_js() -> impl IntoResponse {
    // Including the content hash in the returned script makes the browser see
    // a worker update whenever a precached asset changes, even though `/sw.js`
    // itself deliberately stays at a stable registration URL.
    let script = format!(
        "const PLAMENU_ASSET_VERSION = \"{}\";\n{}",
        *ASSET_VERSION, SW_JS
    );
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        script,
    )
}

/// `GET /manifest.webmanifest` — stable installation metadata. Revalidation is
/// intentional: installed browsers use this URL to discover icon/name/theme
/// updates without waiting behind the immutable static-asset cache.
pub async fn manifest() -> impl IntoResponse {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/manifest+json; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        WEB_MANIFEST,
    )
}

/// `GET /pwa/{asset}` — embedded install icons.
pub async fn pwa_image(Path(asset): Path<String>) -> Response {
    let Some((_, bytes)) = PWA_IMAGES.iter().find(|(name, _)| *name == asset) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        *bytes,
    )
        .into_response()
}

/// Root fallback requested by iOS versions that look for the conventional
/// filename even when the document supplies an explicit touch-icon link.
pub async fn apple_touch_icon() -> impl IntoResponse {
    let bytes = PWA_IMAGES
        .iter()
        .find(|(name, _)| *name == "apple-touch-icon.png")
        .map_or(&[][..], |(_, bytes)| *bytes);
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        bytes,
    )
}

/// `GET /thumbnail.png` — the bundled instance thumbnail advertised by the
/// Mastodon v2 instance API until the operator uploads a custom thumbnail.
/// Use the largest non-maskable PWA icon so clients receive a useful branding
/// image rather than a broken URL.
pub async fn instance_thumbnail() -> impl IntoResponse {
    let bytes = PWA_IMAGES
        .iter()
        .find(|(name, _)| *name == "icon-512.png")
        .map_or(&[][..], |(_, bytes)| *bytes);
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        bytes,
    )
}

/// Network-failure destination for service-worker navigations. It is a real,
/// same-origin HTML page (rather than a synthetic response), so the normal CSP
/// and the same responsive/themed CSS apply when it is shown from `CacheStorage`.
pub async fn offline() -> Markup {
    super::layout::focused_shell(
        "Offline",
        &html! {
            section.auth-card.offline-card {
                img.offline-card__icon src="/pwa/icon-192.png" alt="" width="96" height="96";
                h1 { "You're offline" }
                p.muted { "Plamenu couldn't reach this server. Check your connection and try again." }
                a.offline-card__retry href="/" { "Try again" }
            }
        },
    )
}

/// `GET /assets/hls.min.js` — the vendored hls.js player, loaded lazily by
/// `app.js` on the first play of a `PeerTube`-style HLS video. Immutable: `app.js`
/// requests it with a `?v=<hls version>` buster, so a version bump refetches.
pub async fn hls_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, CACHE),
        ],
        HLS_JS,
    )
}

/// `GET /static/missing.png` — the default avatar/header for image-less
/// accounts, mirroring Mastodon's `/avatars/original/missing.png`. The account
/// serializer hands this URL out when an account has no avatar or header. Phanpy
/// special-cases the `missing.png` suffix and shows initials instead, but other
/// clients (Elk, Ivory, the official apps) render this placeholder — without it
/// they show a broken image. A day's cache: it only changes across a release.
pub async fn missing_image() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        MISSING_IMAGE,
    )
}

/// `GET /favicon.ico` — the operator's favicon upload (largest rendition;
/// browsers request this path by default), falling back to the embedded
/// brand-mark default until one is uploaded. Always PNG either way; the
/// `rel=icon` link in the shell declares the type.
pub async fn favicon(
    axum::extract::State(state): axum::extract::State<crate::AppState>,
) -> Result<impl IntoResponse, crate::error::ApiError> {
    let variants = plamenu_db::site_upload::variants_for(&state.pool, "favicon").await?;
    let uploaded = match variants.last() {
        Some(best) => state.media.get(&best.file_name).await.ok(),
        None => None,
    };
    Ok((
        [
            (header::CONTENT_TYPE, "image/png"),
            // Revalidate rather than cache: the operator can swap the upload
            // at runtime and the URL never changes.
            (header::CACHE_CONTROL, "no-cache"),
        ],
        uploaded.unwrap_or_else(|| DEFAULT_FAVICON.to_vec()),
    ))
}

/// `GET /custom.css` — operator-supplied stylesheet from the instance
/// settings, loaded by every page after the bundled styles (Mastodon's
/// `custom_css#show`). Unlike the embedded assets it changes at runtime, so
/// it revalidates instead of caching immutably.
pub async fn custom_css(
    axum::extract::State(state): axum::extract::State<crate::AppState>,
) -> Result<impl IntoResponse, crate::error::ApiError> {
    let settings = plamenu_db::instance_settings::get(&state.pool).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        settings.custom_css,
    ))
}
