//! Integration tests for the Development settings area
//! (`/settings/applications`): registering an OAuth application from the web
//! UI, the one-time credential reveal, the owner access token's lifecycle
//! (regenerate, scope-change regeneration, deletion), and owner scoping.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::create_local_account;
use http_body_util::BodyExt;
use plamenu_db::{PgPool, oauth, user};
use tower::ServiceExt;

const EMAIL: &str = "dev@example.com";
const PASSWORD: &str = "correct horse battery";

struct Resp {
    status: StatusCode,
    location: Option<String>,
    set_cookie: Option<String>,
    body: String,
}

async fn send(app: &Router, request: Request<Body>) -> Resp {
    let response = app.clone().oneshot(request).await.unwrap();
    let header_string = |name| {
        response
            .headers()
            .get(name)
            .and_then(|v: &header::HeaderValue| v.to_str().ok())
            .map(ToOwned::to_owned)
    };
    let set_cookie = header_string(header::SET_COOKIE);
    let location = header_string(header::LOCATION);
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Resp {
        status,
        location,
        set_cookie,
        body: String::from_utf8(bytes.to_vec()).unwrap(),
    }
}

async fn get(app: &Router, uri: &str, cookie: &str) -> Resp {
    send(
        app,
        Request::builder()
            .uri(uri)
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

/// A bearer-authenticated API GET — proves a revealed token works (or not).
async fn api_get(app: &Router, uri: &str, token: &str) -> Resp {
    send(
        app,
        Request::builder()
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

async fn post(app: &Router, uri: &str, cookie: Option<&str>, fields: &[(&str, &str)]) -> Resp {
    let body = serde_urlencoded::to_string(fields).unwrap();
    let mut request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    send(app, request.body(Body::from(body)).unwrap()).await
}

fn cookie_pair(set_cookie: &str) -> &str {
    set_cookie.split(';').next().unwrap()
}

fn csrf(body: &str) -> String {
    let marker = "name=\"csrf\" value=\"";
    let start = body.find(marker).expect("csrf field") + marker.len();
    body[start..].split('"').next().unwrap().to_owned()
}

/// The `<code>…</code>` cell that follows a credentials-table row header.
fn code_cell_after(body: &str, row_header: &str) -> String {
    let at = body.find(row_header).expect(row_header);
    let rest = &body[at..];
    let start = rest.find("<code>").expect("code cell") + "<code>".len();
    rest[start..].split("</code>").next().unwrap().to_owned()
}

async fn seed_user(pool: &PgPool, username: &str, email: &str) -> i64 {
    let account = create_local_account(pool, username, username).await;
    let hash = plamenu::auth::hash_password(PASSWORD).unwrap();
    user::create(pool, account.id, Some(email), &hash)
        .await
        .unwrap()
        .id
}

async fn login_as(app: &Router, email: &str) -> String {
    let resp = post(
        app,
        "/login",
        None,
        &[("email", email), ("password", PASSWORD)],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    cookie_pair(&resp.set_cookie.expect("session cookie")).to_owned()
}

/// Creates an app through the web form and returns the reveal page.
async fn create_app(app: &Router, cookie: &str, name: &str, scopes: &[&str]) -> Resp {
    let form_page = get(app, "/settings/applications/new", cookie).await;
    let token = csrf(&form_page.body);
    let mut fields = vec![
        ("csrf", token.as_str()),
        ("name", name),
        ("website", ""),
        ("redirect_uris", "urn:ietf:wg:oauth:2.0:oob"),
    ];
    for scope in scopes {
        fields.push(("scopes", scope));
    }
    let resp = post(app, "/web/settings/applications", Some(cookie), &fields).await;
    assert_eq!(
        resp.status,
        StatusCode::OK,
        "create renders the reveal page"
    );
    resp
}

#[sqlx::test(migrations = "../db/migrations")]
async fn index_lists_without_a_create_form(pool: PgPool) {
    seed_user(&pool, "dev", EMAIL).await;
    let app = common::test_app(pool.clone());
    let cookie = login_as(&app, EMAIL).await;

    let index = get(&app, "/settings/applications", &cookie).await;
    assert_eq!(index.status, StatusCode::OK);
    assert!(index.body.contains("You have no applications yet."));
    // Creation is a separate page: the index links to it but carries no form
    // fields of its own.
    assert!(index.body.contains("href=\"/settings/applications/new\""));
    assert!(
        !index.body.contains("name=\"name\""),
        "no create form inline"
    );

    let new_page = get(&app, "/settings/applications/new", &cookie).await;
    assert_eq!(new_page.status, StatusCode::OK);
    assert!(new_page.body.contains("name=\"name\""));
    assert!(new_page.body.contains("name=\"redirect_uris\""));
    // Mastodon prefills: the OOB redirect URI and the `profile` scope.
    assert!(new_page.body.contains("urn:ietf:wg:oauth:2.0:oob"));
    assert!(
        new_page
            .body
            .contains("name=\"scopes\" value=\"profile\" checked")
    );

    // The settings navigation gained the Development section.
    assert!(index.body.contains("Development"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn create_reveals_working_credentials_once(pool: PgPool) {
    seed_user(&pool, "dev", EMAIL).await;
    let app = common::test_app(pool.clone());
    let cookie = login_as(&app, EMAIL).await;

    let reveal = create_app(&app, &cookie, "Fishbot", &["read", "write"]).await;
    let client_id = code_cell_after(&reveal.body, "Client key");
    let secret = code_cell_after(&reveal.body, "Client secret");
    let token = code_cell_after(&reveal.body, "Your access token");
    assert!(!client_id.is_empty() && !secret.is_empty() && !token.is_empty());

    // The stored app matches what was revealed, hashed.
    let stored = oauth::find_app_by_client_id(&pool, &client_id)
        .await
        .unwrap()
        .expect("app registered");
    assert_eq!(stored.name, "Fishbot");
    assert_eq!(stored.scopes, "read write");
    assert_eq!(
        stored.client_secret_hash,
        plamenu::auth::hash_secret(&secret)
    );

    // The revealed token authenticates the API as the owner.
    let me = api_get(&app, "/api/v1/accounts/verify_credentials", &token).await;
    assert_eq!(me.status, StatusCode::OK);
    assert!(me.body.contains("\"username\":\"dev\""));

    // Reloading the manage page shows metadata, never the secrets again.
    let manage = get(
        &app,
        &format!("/settings/applications/{}", stored.id),
        &cookie,
    )
    .await;
    assert_eq!(manage.status, StatusCode::OK);
    assert!(!manage.body.contains(&secret), "secret not re-shown");
    assert!(!manage.body.contains(&token), "token not re-shown");
    assert!(
        manage
            .body
            .contains("Shown once when the application was created")
    );
    assert!(manage.body.contains("Regenerate access token"));

    // And the index now lists the app.
    let index = get(&app, "/settings/applications", &cookie).await;
    assert!(index.body.contains("Fishbot"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn api_registered_and_foreign_apps_stay_invisible(pool: PgPool) {
    seed_user(&pool, "dev", EMAIL).await;
    let other_uid = seed_user(&pool, "rival", "rival@example.com").await;
    let app = common::test_app(pool.clone());
    let cookie = login_as(&app, EMAIL).await;

    // An app registered over the client API has no owner.
    let api_app = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/api/v1/apps")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"client_name":"headless","redirect_uris":"urn:ietf:wg:oauth:2.0:oob"}"#,
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(api_app.status, StatusCode::OK);

    // Another user's owned app.
    let foreign = oauth::create_owned_app(
        &pool,
        oauth::NewApp {
            name: "rivals-bot",
            website: None,
            client_id: "rivalclient",
            client_secret_hash: "h",
            redirect_uris: &["urn:ietf:wg:oauth:2.0:oob".to_owned()],
            scopes: "read",
        },
        other_uid,
    )
    .await
    .unwrap();

    let index = get(&app, "/settings/applications", &cookie).await;
    assert!(!index.body.contains("headless"), "API app not listed");
    assert!(!index.body.contains("rivals-bot"), "foreign app not listed");

    // A foreign id is a 404, for the page and every action on it.
    let manage = get(
        &app,
        &format!("/settings/applications/{}", foreign.id),
        &cookie,
    )
    .await;
    assert_eq!(manage.status, StatusCode::NOT_FOUND);
    let page = get(&app, "/settings/applications", &cookie).await;
    let regen = post(
        &app,
        &format!("/web/settings/applications/{}/regenerate", foreign.id),
        Some(&cookie),
        &[("csrf", &csrf(&page.body))],
    )
    .await;
    assert_eq!(regen.status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn create_validates_like_mastodon(pool: PgPool) {
    seed_user(&pool, "dev", EMAIL).await;
    let app = common::test_app(pool.clone());
    let cookie = login_as(&app, EMAIL).await;
    let form_page = get(&app, "/settings/applications/new", &cookie).await;
    let token = csrf(&form_page.body);

    let attempt = |fields: Vec<(&'static str, &'static str)>| {
        let app = app.clone();
        let token = token.clone();
        let cookie = cookie.clone();
        async move {
            let mut all = vec![("csrf", token.as_str())];
            all.extend(fields);
            post(&app, "/web/settings/applications", Some(&cookie), &all).await
        }
    };

    // Blank name.
    let resp = attempt(vec![
        ("name", "  "),
        ("redirect_uris", "urn:ietf:wg:oauth:2.0:oob"),
    ])
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    assert!(resp.location.unwrap().contains("error="));

    // Missing redirect URI.
    let resp = attempt(vec![("name", "bot"), ("redirect_uris", "  ")]).await;
    assert!(resp.location.unwrap().contains("error="));

    // Script-scheme and relative redirect URIs are rejected.
    let resp = attempt(vec![
        ("name", "bot"),
        ("redirect_uris", "javascript:alert(1)"),
    ])
    .await;
    assert!(resp.location.unwrap().contains("error="));
    let resp = attempt(vec![("name", "bot"), ("redirect_uris", "/relative/path")]).await;
    assert!(resp.location.unwrap().contains("error="));

    // Website must be http(s).
    let resp = attempt(vec![
        ("name", "bot"),
        ("redirect_uris", "urn:ietf:wg:oauth:2.0:oob"),
        ("website", "ftp://example.com"),
    ])
    .await;
    assert!(resp.location.unwrap().contains("error="));

    // Nothing was created along the way.
    assert!(
        oauth::list_owned_apps(
            &pool,
            user::find_by_email(&pool, EMAIL).await.unwrap().unwrap().id
        )
        .await
        .unwrap()
        .is_empty()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn scope_change_regenerates_the_owner_token(pool: PgPool) {
    seed_user(&pool, "dev", EMAIL).await;
    let app = common::test_app(pool.clone());
    let cookie = login_as(&app, EMAIL).await;

    let reveal = create_app(&app, &cookie, "Fishbot", &["read", "write"]).await;
    let client_id = code_cell_after(&reveal.body, "Client key");
    let old_token = code_cell_after(&reveal.body, "Your access token");
    let stored = oauth::find_app_by_client_id(&pool, &client_id)
        .await
        .unwrap()
        .unwrap();
    let manage_path = format!("/settings/applications/{}", stored.id);
    let update_path = format!("/web/settings/applications/{}", stored.id);

    // Same scopes, new name: the token is left alone (plain saved redirect).
    let page = get(&app, &manage_path, &cookie).await;
    let resp = post(
        &app,
        &update_path,
        Some(&cookie),
        &[
            ("csrf", &csrf(&page.body)),
            ("name", "Fishbot 2"),
            ("redirect_uris", "urn:ietf:wg:oauth:2.0:oob"),
            ("scopes", "write"),
            ("scopes", "read"),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    assert!(resp.location.unwrap().contains("saved=1"));
    assert_eq!(
        api_get(&app, "/api/v1/accounts/verify_credentials", &old_token)
            .await
            .status,
        StatusCode::OK,
        "token survives a rename"
    );

    // Narrowing scopes regenerates: the response reveals a new token once.
    let page = get(&app, &manage_path, &cookie).await;
    let resp = post(
        &app,
        &update_path,
        Some(&cookie),
        &[
            ("csrf", &csrf(&page.body)),
            ("name", "Fishbot 2"),
            ("redirect_uris", "urn:ietf:wg:oauth:2.0:oob"),
            ("scopes", "read"),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
    let new_token = code_cell_after(&resp.body, "Your access token");
    assert_ne!(new_token, old_token);

    // Old token dead, new token live but read-only.
    assert_eq!(
        api_get(&app, "/api/v1/accounts/verify_credentials", &old_token)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        api_get(&app, "/api/v1/accounts/verify_credentials", &new_token)
            .await
            .status,
        StatusCode::OK
    );
    let write = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/api/v1/statuses")
            .header(header::AUTHORIZATION, format!("Bearer {new_token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"status":"should 403"}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(
        write.status,
        StatusCode::FORBIDDEN,
        "write scope was dropped"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn regenerate_swaps_the_token(pool: PgPool) {
    seed_user(&pool, "dev", EMAIL).await;
    let app = common::test_app(pool.clone());
    let cookie = login_as(&app, EMAIL).await;

    let reveal = create_app(&app, &cookie, "Fishbot", &["read"]).await;
    let client_id = code_cell_after(&reveal.body, "Client key");
    let old_token = code_cell_after(&reveal.body, "Your access token");
    let stored = oauth::find_app_by_client_id(&pool, &client_id)
        .await
        .unwrap()
        .unwrap();

    let page = get(
        &app,
        &format!("/settings/applications/{}", stored.id),
        &cookie,
    )
    .await;
    let resp = post(
        &app,
        &format!("/web/settings/applications/{}/regenerate", stored.id),
        Some(&cookie),
        &[("csrf", &csrf(&page.body))],
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK, "reveal page");
    let new_token = code_cell_after(&resp.body, "Your access token");
    assert_ne!(new_token, old_token);

    assert_eq!(
        api_get(&app, "/api/v1/accounts/verify_credentials", &old_token)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        api_get(&app, "/api/v1/accounts/verify_credentials", &new_token)
            .await
            .status,
        StatusCode::OK
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn delete_kills_the_app_and_its_tokens(pool: PgPool) {
    seed_user(&pool, "dev", EMAIL).await;
    let app = common::test_app(pool.clone());
    let cookie = login_as(&app, EMAIL).await;

    let reveal = create_app(&app, &cookie, "Fishbot", &["read"]).await;
    let client_id = code_cell_after(&reveal.body, "Client key");
    let token = code_cell_after(&reveal.body, "Your access token");
    let stored = oauth::find_app_by_client_id(&pool, &client_id)
        .await
        .unwrap()
        .unwrap();

    let page = get(&app, "/settings/applications", &cookie).await;
    let resp = post(
        &app,
        &format!("/web/settings/applications/{}/delete", stored.id),
        Some(&cookie),
        &[("csrf", &csrf(&page.body))],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);

    assert!(
        oauth::find_app_by_client_id(&pool, &client_id)
            .await
            .unwrap()
            .is_none(),
        "app row gone"
    );
    assert_eq!(
        api_get(&app, "/api/v1/accounts/verify_credentials", &token)
            .await
            .status,
        StatusCode::UNAUTHORIZED,
        "its tokens die with it"
    );
    let index = get(&app, "/settings/applications", &cookie).await;
    assert!(index.body.contains("You have no applications yet."));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn create_requires_a_valid_csrf_token(pool: PgPool) {
    seed_user(&pool, "dev", EMAIL).await;
    let app = common::test_app(pool.clone());
    let cookie = login_as(&app, EMAIL).await;

    let resp = post(
        &app,
        "/web/settings/applications",
        Some(&cookie),
        &[
            ("csrf", "forged"),
            ("name", "bot"),
            ("redirect_uris", "urn:ietf:wg:oauth:2.0:oob"),
        ],
    )
    .await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN);
    let uid = user::find_by_email(&pool, EMAIL).await.unwrap().unwrap().id;
    assert!(oauth::list_owned_apps(&pool, uid).await.unwrap().is_empty());
}
