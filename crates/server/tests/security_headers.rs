//! Response security headers. Every HTML page is frame-denied so
//! login, OAuth consent, and settings actions cannot be clickjacked, and
//! credential-bearing responses are `no-store` so browsers and shared proxies
//! never retain them. These assert the middleware is actually wired into the
//! real router — the path classifier itself is unit-tested in the crate.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{create_local_account, test_app, test_app_csp_reporting};
use plamenu_db::{PgPool, user};
use tower::ServiceExt;

/// Fetches a path and returns `(status, content-type, x-frame-options, csp,
/// cache-control)` for header assertions.
async fn head(
    app: &Router,
    path: &str,
) -> (
    StatusCode,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    let request = Request::builder()
        .method("GET")
        .uri(path)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let get = |name: header::HeaderName| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(ToOwned::to_owned)
    };
    let status = response.status();
    let content_type = get(header::CONTENT_TYPE).unwrap_or_default();
    (
        status,
        content_type,
        get(header::X_FRAME_OPTIONS),
        get(header::CONTENT_SECURITY_POLICY),
        get(header::CACHE_CONTROL),
    )
}

#[sqlx::test(migrations = "../db/migrations")]
async fn credential_page_is_frame_denied_and_no_store(pool: PgPool) {
    let app = test_app(pool);
    let (status, content_type, xfo, csp, cache) = head(&app, "/login").await;

    assert_eq!(status, StatusCode::OK);
    assert!(content_type.starts_with("text/html"), "{content_type}");
    assert_eq!(xfo.as_deref(), Some("DENY"));
    let csp = csp.expect("credential HTML must carry the policy");
    assert!(csp.starts_with("default-src 'none'"), "{csp}");
    assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
    assert_eq!(cache.as_deref(), Some("no-store"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn public_html_is_frame_denied_but_still_cacheable(pool: PgPool) {
    // The anonymous landing page is HTML: it must be frame-denied (clickjacking
    // defence) but NOT forced `no-store` — it is a public, cacheable surface.
    let app = test_app(pool);
    let (status, content_type, xfo, csp, cache) = head(&app, "/").await;

    assert_eq!(status, StatusCode::OK);
    assert!(content_type.starts_with("text/html"), "{content_type}");
    assert_eq!(xfo.as_deref(), Some("DENY"));
    let csp = csp.expect("public HTML must carry the policy");
    assert!(csp.starts_with("default-src 'none'"), "{csp}");
    assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
    assert_ne!(
        cache.as_deref(),
        Some("no-store"),
        "a public page must not be forced no-store"
    );
}

async fn login_cookie(app: &Router) -> String {
    let body =
        serde_urlencoded::to_string([("identifier", "alice@example.com"), ("password", "pw")])
            .unwrap();
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    response
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .expect("login session cookie")
        .to_owned()
}

async fn signed_in_csp(app: &Router, cookie: &str) -> String {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response
        .headers()
        .get(header::CONTENT_SECURITY_POLICY)
        .and_then(|value| value.to_str().ok())
        .expect("signed-in HTML CSP")
        .to_owned()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn direct_media_csp_follows_the_stored_viewer_preference(pool: PgPool) {
    let account = create_local_account(&pool, "alice", "Alice").await;
    let password = plamenu::auth::hash_password("pw").unwrap();
    let created = user::create(&pool, account.id, Some("alice@example.com"), &password)
        .await
        .unwrap();
    let app = test_app(pool.clone());
    let cookie = login_cookie(&app).await;

    let default_csp = signed_in_csp(&app, &cookie).await;
    assert!(
        default_csp.contains("img-src 'self' blob: https:;"),
        "{default_csp}"
    );
    assert!(
        default_csp.contains("media-src 'self' blob: https:;"),
        "{default_csp}"
    );

    sqlx::query("UPDATE users SET reading_allow_direct_remote_media = false WHERE id = $1")
        .bind(created.id)
        .execute(&pool)
        .await
        .unwrap();
    let strict = signed_in_csp(&app, &cookie).await;
    assert!(strict.contains("img-src 'self' blob:;"), "{strict}");
    assert!(strict.contains("media-src 'self' blob:;"), "{strict}");
    assert!(!strict.contains("img-src 'self' blob: https:"), "{strict}");

    sqlx::query!(
        "UPDATE users SET reading_allow_direct_remote_media = true WHERE id = $1",
        created.id
    )
    .execute(&pool)
    .await
    .unwrap();
    let opted_in = signed_in_csp(&app, &cookie).await;
    assert!(
        opted_in.contains("img-src 'self' blob: https:;"),
        "{opted_in}"
    );
    assert!(
        opted_in.contains("media-src 'self' blob: https:;"),
        "{opted_in}"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn static_asset_is_untouched(pool: PgPool) {
    // A non-HTML asset gets no frame headers, and its own long-lived caching is
    // preserved (its path is not a credential path).
    let app = test_app(pool);
    let (status, content_type, xfo, _csp, cache) = head(&app, "/assets/app.css").await;

    assert_eq!(status, StatusCode::OK);
    assert!(content_type.starts_with("text/css"), "{content_type}");
    assert_eq!(xfo, None, "non-HTML must not be frame-denied");
    assert_ne!(
        cache.as_deref(),
        Some("no-store"),
        "static asset caching must be preserved"
    );
}

/// The full content-security policy is **enforced** on every HTML response.
/// It must admit exactly one inline script — the layout's bootstrap — by a
/// hash recomputed here from the *rendered* page, so the emitted script and
/// the policy cannot drift apart (drift is the classic way a CSP silently
/// breaks a page).
#[sqlx::test(migrations = "../db/migrations")]
async fn enforced_csp_covers_html_and_admits_only_the_bootstrap(pool: PgPool) {
    use base64::Engine;
    use sha2::Digest;

    let app = test_app(pool);
    for path in ["/", "/login"] {
        let request = Request::builder().uri(path).body(Body::empty()).unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let policy = response
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_else(|| panic!("{path} must carry the enforced policy"))
            .to_owned();
        assert!(
            response
                .headers()
                .get(header::CONTENT_SECURITY_POLICY_REPORT_ONLY)
                .is_none(),
            "{path}: the report-only header retired when enforcement began"
        );
        let bytes = http_body_util::BodyExt::collect(response.into_body())
            .await
            .unwrap()
            .to_bytes();
        let body = String::from_utf8(bytes.to_vec()).unwrap();

        assert!(
            policy.starts_with("default-src 'none'"),
            "{path}: the default must deny what a future feature forgets to declare: {policy}"
        );
        assert!(
            !policy.contains("report-uri"),
            "{path}: default HTML must not advertise CSP telemetry: {policy}"
        );

        // Exactly one source-less <script> — the bootstrap — and its sha256 is
        // the one the policy admits.
        assert_eq!(
            body.matches("<script>").count(),
            1,
            "{path}: a second inline script would violate the policy at runtime"
        );
        let inline = body
            .split("<script>")
            .nth(1)
            .and_then(|rest| rest.split("</script>").next())
            .expect("the bootstrap script");
        let hash = base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(inline));
        assert!(
            policy.contains(&format!("'sha256-{hash}'")),
            "{path}: the policy must admit the rendered bootstrap (script {inline:?}): {policy}"
        );
    }
}

/// ALTCHA injects its bundled stylesheet into the document. Admit that exact
/// pinned stylesheet by hash instead of weakening `style-src` with
/// `unsafe-inline`; a widget upgrade that changes the CSS must update this
/// assertion and receive a real-browser CSP pass.
#[sqlx::test(migrations = "../db/migrations")]
async fn enforced_csp_admits_only_the_pinned_altcha_stylesheet(pool: PgPool) {
    let app = test_app(pool);
    let (_, _, _, csp, _) = head(&app, "/signup").await;
    let policy = csp.expect("signup HTML must carry the policy");

    assert!(
        policy.contains("style-src 'self' 'sha256-ZgqGuQlekW98cv0XQjYUGCLTvc3q5MkU+2SkqlFGoTM=';"),
        "the exact ALTCHA 3.2.3 stylesheet hash must be allowed: {policy}"
    );
    assert!(
        !policy.contains("style-src 'self' 'unsafe-inline'"),
        "element styles must not gain a blanket inline exception: {policy}"
    );
}

/// The policy is HTML-only — on an asset or API response it would be dead
/// weight on every attachment byte served.
#[sqlx::test(migrations = "../db/migrations")]
async fn enforced_csp_stays_off_non_html(pool: PgPool) {
    let app = test_app(pool);
    for path in ["/assets/app.css", "/api/v1/instance"] {
        let request = Request::builder().uri(path).body(Body::empty()).unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert!(
            response
                .headers()
                .get(header::CONTENT_SECURITY_POLICY)
                .is_none(),
            "{path} must not carry the policy"
        );
    }
}

/// Reporting is wholly absent by default: browsers receive no reporting
/// directive and a caller cannot discover a live sink at the conventional URL.
#[sqlx::test(migrations = "../db/migrations")]
async fn csp_reporting_is_absent_by_default(pool: PgPool) {
    let app = test_app(pool);
    let page = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let policy = page.headers()[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap();
    assert!(!policy.contains("report-uri"), "{policy}");

    let report = Request::builder()
        .method("POST")
        .uri("/csp-report")
        .header(header::CONTENT_TYPE, "application/csp-report")
        .body(Body::from("{}"))
        .unwrap();
    let status = app.oneshot(report).await.unwrap().status();
    assert!(
        matches!(
            status,
            StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
        ),
        "the disabled sink must not accept reports: {status}"
    );
}

/// An explicit opt-in adds the matching directive and sink. The sink accepts a
/// browser-shaped report (logging it) and answers a malformed body the same way
/// — `204`, never an error an attacker can farm.
#[sqlx::test(migrations = "../db/migrations")]
async fn csp_reporting_opt_in_adds_directive_and_sink(pool: PgPool) {
    let app = test_app_csp_reporting(pool);
    let page = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let policy = page.headers()[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap();
    assert!(policy.contains("report-uri /csp-report"), "{policy}");

    let bodies = [
        r#"{"csp-report":{"document-uri":"https://x/","violated-directive":"img-src","blocked-uri":"https://evil/"}}"#,
        "not json at all",
    ];
    for body in bodies {
        let request = Request::builder()
            .method("POST")
            .uri("/csp-report")
            .header(header::CONTENT_TYPE, "application/csp-report")
            .body(Body::from(body))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT, "{body}");
    }
}

/// Content-type sniffing is refused on *every* response, not just HTML, and the
/// referrer is held to the origin when leaving the site.
///
/// `nosniff` matters most on `/media/{file}`, which serves attacker-supplied
/// bytes under a server-chosen content type (and `application/octet-stream` for
/// anything unrecognised): a browser that sniffs such a response into HTML
/// would run it on our own origin. `Referrer-Policy` keeps the path and query
/// — where password-reset and confirmation links carry their token — out of
/// the `Referer` sent to other sites.
#[sqlx::test(migrations = "../db/migrations")]
async fn every_response_refuses_sniffing_and_bounds_the_referrer(pool: PgPool) {
    let app = test_app(pool);
    for path in ["/login", "/", "/assets/app.css", "/api/v1/instance"] {
        let request = Request::builder().uri(path).body(Body::empty()).unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let header_value = |name: header::HeaderName| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(ToOwned::to_owned)
        };
        assert_eq!(
            header_value(header::X_CONTENT_TYPE_OPTIONS).as_deref(),
            Some("nosniff"),
            "{path} must refuse content-type sniffing",
        );
        assert_eq!(
            header_value(header::REFERRER_POLICY).as_deref(),
            Some("strict-origin-when-cross-origin"),
            "{path} must bound the referrer",
        );
    }
}
