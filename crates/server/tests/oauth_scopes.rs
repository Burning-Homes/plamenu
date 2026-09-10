//! Scope checks through the routed API: narrow and broad grants, legacy
//! alternatives, unrelated permissions, and anonymous reads of public objects.

mod common;

use std::collections::HashMap;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{create_local_account, test_app};
use http_body_util::BodyExt;
use plamenu::auth::{generate_secret, hash_password, hash_secret};
use plamenu_db::{PgPool, oauth, user};
use serde_json::{Value, json};
use tower::ServiceExt;

const SCOPE_ERROR: &str = "This action is outside the authorized scopes";

struct Fixture {
    app: Router,
    registration: oauth::App,
    user_id: i64,
    account_id: i64,
    tokens: HashMap<String, String>,
}

impl Fixture {
    async fn new(pool: &PgPool) -> Self {
        // Keep the permission matrix independent of request-budget tests.
        sqlx::query("UPDATE instance_settings SET rate_limiting_enabled = false, registrations_mode = 'open'")
            .execute(pool).await.unwrap();
        let account = create_local_account(pool, "alice", "Alice").await;
        let member = user::create(
            pool,
            account.id,
            Some("alice@example.com"),
            &hash_password("password").unwrap(),
        )
        .await
        .unwrap();
        let scopes = plamenu::oauth_app::SUPPORTED_SCOPES.join(" ");
        let registration = oauth::create_app(
            pool,
            oauth::NewApp {
                name: "Scope tests",
                website: None,
                client_id: "scope-tests",
                client_secret_hash: &hash_secret("secret"),
                redirect_uris: &["urn:ietf:wg:oauth:2.0:oob".to_owned()],
                scopes: &scopes,
            },
        )
        .await
        .unwrap();
        let mut fixture = Self {
            app: test_app(pool.clone()),
            registration,
            user_id: member.id,
            account_id: account.id,
            tokens: HashMap::new(),
        };
        for scope in plamenu::oauth_app::SUPPORTED_SCOPES {
            let token = fixture.mint(pool, scope).await;
            fixture.tokens.insert((*scope).to_owned(), token);
        }
        let token = fixture.mint(pool, "").await;
        fixture.tokens.insert(String::new(), token);
        fixture
    }

    async fn mint(&self, pool: &PgPool, scope: &str) -> String {
        let token = generate_secret();
        oauth::create_token(
            pool,
            &hash_secret(&token),
            self.registration.id,
            Some(self.user_id),
            scope,
        )
        .await
        .unwrap();
        token
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        scope: Option<&str>,
        body: Value,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(path.replace("{account}", &self.account_id.to_string()))
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(scope) = scope {
            request = request.header(
                header::AUTHORIZATION,
                format!("Bearer {}", self.tokens[scope]),
            );
        }
        send(
            &self.app,
            request.body(Body::from(body.to_string())).unwrap(),
        )
        .await
    }
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body =
        serde_json::from_slice(&bytes).unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes)));
    (status, body)
}

struct Case {
    method: &'static str,
    path: &'static str,
    scope: &'static str,
    status: u16,
    alternative: Option<&'static str>,
}

impl Case {
    const fn new(
        method: &'static str,
        path: &'static str,
        scope: &'static str,
        status: u16,
    ) -> Self {
        Self {
            method,
            path,
            scope,
            status,
            alternative: None,
        }
    }

    const fn alternative(mut self, scope: &'static str) -> Self {
        self.alternative = Some(scope);
        self
    }
}

async fn check_matrix(fixture: &Fixture, cases: &[Case]) {
    let mut failures = Vec::new();
    for case in cases {
        let broad = case.scope.split(':').next().unwrap();
        for scope in fixture.tokens.keys() {
            let allowed =
                scope == case.scope || scope == broad || case.alternative == Some(scope.as_str());
            let (status, body) = fixture
                .request(case.method, case.path, Some(scope), json!({}))
                .await;
            let expected = if allowed { case.status } else { 403 };
            if status.as_u16() != expected || (!allowed && body["error"] != SCOPE_ERROR) {
                failures.push(format!(
                    "{} {} with {scope:?}: expected {expected}, got {status}: {body}",
                    case.method, case.path
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[sqlx::test(migrations = "../db/migrations")]
#[allow(
    clippy::too_many_lines,
    reason = "explicit endpoint matrix or complete client flow"
)]
async fn read_endpoints_accept_only_their_scopes(pool: PgPool) {
    let fixture = Fixture::new(&pool).await;
    check_matrix(
        &fixture,
        &[
            Case::new(
                "GET",
                "/api/v1/accounts/verify_credentials",
                "read:accounts",
                200,
            )
            .alternative("profile"),
            Case::new("GET", "/api/v1/profile", "read:accounts", 200).alternative("profile"),
            Case::new("GET", "/api/v1/accounts", "read:accounts", 200),
            Case::new("GET", "/api/v1/accounts/0", "read:accounts", 404),
            Case::new(
                "GET",
                "/api/v1/accounts/lookup?acct=alice",
                "read:accounts",
                200,
            ),
            Case::new(
                "GET",
                "/api/v1/accounts/search?q=alice",
                "read:accounts",
                200,
            ),
            Case::new("GET", "/api/v1/accounts/0/followers", "read:accounts", 404),
            Case::new("GET", "/api/v1/accounts/0/following", "read:accounts", 404),
            Case::new("GET", "/api/v1/preferences", "read:accounts", 200),
            Case::new("GET", "/api/v1/suggestions", "read:accounts", 200),
            Case::new("GET", "/api/v2/suggestions", "read:accounts", 200),
            Case::new("GET", "/api/v1/featured_tags", "read:accounts", 200),
            Case::new(
                "GET",
                "/api/v1/featured_tags/suggestions",
                "read:accounts",
                200,
            ),
            Case::new("GET", "/api/v1/endorsements", "read:accounts", 200),
            Case::new("GET", "/api/v1/timelines/home", "read:statuses", 200),
            Case::new("GET", "/api/v1/timelines/public", "read:statuses", 200),
            Case::new("GET", "/api/v1/timelines/tag/test", "read:statuses", 200),
            Case::new("GET", "/api/v1/statuses", "read:statuses", 200),
            Case::new("GET", "/api/v1/statuses/0", "read:statuses", 404),
            Case::new("GET", "/api/v1/statuses/0/context", "read:statuses", 404),
            Case::new("GET", "/api/v1/statuses/0/history", "read:statuses", 404),
            Case::new("GET", "/api/v1/statuses/0/source", "read:statuses", 404),
            Case::new(
                "GET",
                "/api/v1/statuses/0/favourited_by",
                "read:accounts",
                404,
            ),
            Case::new(
                "GET",
                "/api/v1/statuses/0/reblogged_by",
                "read:accounts",
                404,
            ),
            Case::new("GET", "/api/v1/statuses/0/quotes", "read:statuses", 404),
            Case::new("GET", "/api/v1/accounts/0/statuses", "read:statuses", 404),
            Case::new("GET", "/api/v1/polls/0", "read:statuses", 404),
            Case::new("GET", "/api/v1/scheduled_statuses", "read:statuses", 200),
            Case::new("GET", "/api/v1/scheduled_statuses/0", "read:statuses", 404),
            Case::new("GET", "/api/v1/conversations", "read:statuses", 200),
            Case::new(
                "GET",
                "/api/v1/markers?timeline[]=home",
                "read:statuses",
                200,
            ),
            Case::new("GET", "/api/v1/lists", "read:lists", 200),
            Case::new("GET", "/api/v1/lists/0", "read:lists", 404),
            Case::new("GET", "/api/v1/lists/0/accounts", "read:lists", 404),
            Case::new("GET", "/api/v1/timelines/list/0", "read:lists", 404),
            Case::new("GET", "/api/v1/filters", "read:filters", 200),
            Case::new("GET", "/api/v2/filters", "read:filters", 200),
            Case::new("GET", "/api/v2/filters/0", "read:filters", 404),
            Case::new("GET", "/api/v2/filters/0/keywords", "read:filters", 404),
            Case::new("GET", "/api/v2/filters/0/statuses", "read:filters", 404),
            Case::new("GET", "/api/v1/notifications", "read:notifications", 200),
            Case::new("GET", "/api/v2/notifications", "read:notifications", 200),
            Case::new(
                "GET",
                "/api/v1/notifications/unread_count",
                "read:notifications",
                200,
            ),
            Case::new(
                "GET",
                "/api/v2/notifications/unread_count",
                "read:notifications",
                200,
            ),
            Case::new(
                "GET",
                "/api/v1/notifications/policy",
                "read:notifications",
                200,
            ),
            Case::new(
                "GET",
                "/api/v2/notifications/policy",
                "read:notifications",
                200,
            ),
            Case::new(
                "GET",
                "/api/v1/notifications/requests",
                "read:notifications",
                200,
            ),
            Case::new("GET", "/api/v1/favourites", "read:favourites", 200),
            Case::new("GET", "/api/v1/bookmarks", "read:bookmarks", 200),
            Case::new("GET", "/api/v1/blocks", "read:blocks", 200).alternative("follow"),
            Case::new("GET", "/api/v1/domain_blocks", "read:blocks", 200).alternative("follow"),
            Case::new("GET", "/api/v1/mutes", "read:mutes", 200).alternative("follow"),
            Case::new("GET", "/api/v1/follow_requests", "read:follows", 200).alternative("follow"),
            Case::new("GET", "/api/v1/accounts/relationships", "read:follows", 200)
                .alternative("follow"),
            Case::new(
                "GET",
                "/api/v1/accounts/familiar_followers",
                "read:follows",
                200,
            )
            .alternative("follow"),
            Case::new("GET", "/api/v1/followed_tags", "read:follows", 200).alternative("follow"),
            Case::new("GET", "/api/v2/search?q=alice", "read:search", 200),
            Case::new("GET", "/api/v1/collections/0", "read:collections", 404),
            Case::new("GET", "/api/v1/media/0", "write:media", 404),
        ],
    )
    .await;
}

#[sqlx::test(migrations = "../db/migrations")]
#[allow(
    clippy::too_many_lines,
    reason = "explicit endpoint matrix or complete client flow"
)]
async fn write_endpoints_accept_only_their_scopes(pool: PgPool) {
    let fixture = Fixture::new(&pool).await;
    check_matrix(
        &fixture,
        &[
            Case::new("POST", "/api/v1/statuses", "write:statuses", 422),
            Case::new("POST", "/api/v1/statuses/preview", "write:statuses", 422),
            Case::new("PUT", "/api/v1/statuses/0", "write:statuses", 404),
            Case::new("DELETE", "/api/v1/statuses/0", "write:statuses", 404),
            Case::new("POST", "/api/v1/statuses/0/reblog", "write:statuses", 404),
            Case::new("POST", "/api/v1/statuses/0/unreblog", "write:statuses", 404),
            Case::new(
                "POST",
                "/api/v1/statuses/0/favourite",
                "write:favourites",
                404,
            ),
            Case::new(
                "POST",
                "/api/v1/statuses/0/unfavourite",
                "write:favourites",
                404,
            ),
            Case::new(
                "POST",
                "/api/v1/statuses/0/bookmark",
                "write:bookmarks",
                404,
            ),
            Case::new(
                "POST",
                "/api/v1/statuses/0/unbookmark",
                "write:bookmarks",
                404,
            ),
            Case::new("POST", "/api/v1/statuses/0/pin", "write:accounts", 404),
            Case::new("POST", "/api/v1/statuses/0/unpin", "write:accounts", 404),
            Case::new("POST", "/api/v1/statuses/0/mute", "write:mutes", 404),
            Case::new("POST", "/api/v1/statuses/0/unmute", "write:mutes", 404),
            Case::new("POST", "/api/v1/polls/0/votes", "write:statuses", 404),
            Case::new("PUT", "/api/v1/scheduled_statuses/0", "write:statuses", 422),
            Case::new(
                "DELETE",
                "/api/v1/scheduled_statuses/0",
                "write:statuses",
                404,
            ),
            Case::new("PUT", "/api/v1/media/0", "write:media", 404),
            Case::new("DELETE", "/api/v1/media/0", "write:media", 404),
            Case::new("POST", "/api/v1/lists", "write:lists", 422),
            Case::new("PUT", "/api/v1/lists/0", "write:lists", 404),
            Case::new("DELETE", "/api/v1/lists/0", "write:lists", 404),
            Case::new("POST", "/api/v1/lists/0/accounts", "write:lists", 404),
            Case::new("DELETE", "/api/v1/lists/0/accounts", "write:lists", 404),
            Case::new("POST", "/api/v1/filters", "write:filters", 422),
            Case::new("POST", "/api/v2/filters", "write:filters", 422),
            Case::new("PUT", "/api/v2/filters/0", "write:filters", 404),
            Case::new("DELETE", "/api/v2/filters/0", "write:filters", 404),
            Case::new("POST", "/api/v2/filters/0/keywords", "write:filters", 404),
            Case::new("POST", "/api/v2/filters/0/statuses", "write:filters", 404),
            Case::new(
                "POST",
                "/api/v1/notifications/clear",
                "write:notifications",
                200,
            ),
            Case::new(
                "POST",
                "/api/v2/notifications/clear",
                "write:notifications",
                200,
            ),
            Case::new(
                "POST",
                "/api/v1/notifications/0/dismiss",
                "write:notifications",
                404,
            ),
            Case::new(
                "PATCH",
                "/api/v1/notifications/policy",
                "write:notifications",
                200,
            ),
            Case::new(
                "PATCH",
                "/api/v2/notifications/policy",
                "write:notifications",
                200,
            ),
            Case::new(
                "POST",
                "/api/v1/notifications/requests/0/accept",
                "write:notifications",
                404,
            ),
            Case::new(
                "DELETE",
                "/api/v1/conversations/0",
                "write:conversations",
                404,
            ),
            Case::new(
                "POST",
                "/api/v1/conversations/0/read",
                "write:conversations",
                404,
            ),
            Case::new("POST", "/api/v1/reports", "write:reports", 404),
            Case::new("POST", "/api/v1/accounts/0/follow", "write:follows", 404)
                .alternative("follow"),
            Case::new("POST", "/api/v1/accounts/0/unfollow", "write:follows", 404)
                .alternative("follow"),
            Case::new("POST", "/api/v1/accounts/0/block", "write:blocks", 404)
                .alternative("follow"),
            Case::new("POST", "/api/v1/accounts/0/unblock", "write:blocks", 404)
                .alternative("follow"),
            Case::new("POST", "/api/v1/accounts/0/mute", "write:mutes", 404).alternative("follow"),
            Case::new("POST", "/api/v1/accounts/0/unmute", "write:mutes", 404)
                .alternative("follow"),
            Case::new("POST", "/api/v1/accounts/0/note", "write:accounts", 404),
            Case::new("POST", "/api/v1/accounts/0/endorse", "write:accounts", 404),
            Case::new(
                "POST",
                "/api/v1/accounts/0/unendorse",
                "write:accounts",
                404,
            ),
            Case::new(
                "POST",
                "/api/v1/follow_requests/0/authorize",
                "write:follows",
                404,
            )
            .alternative("follow"),
            Case::new(
                "POST",
                "/api/v1/follow_requests/0/reject",
                "write:follows",
                404,
            )
            .alternative("follow"),
            Case::new("POST", "/api/v1/tags/test/follow", "write:follows", 200)
                .alternative("follow"),
            Case::new("POST", "/api/v1/tags/test/unfollow", "write:follows", 200)
                .alternative("follow"),
            Case::new(
                "DELETE",
                "/api/v1/suggestions/{account}",
                "write:accounts",
                200,
            ),
            Case::new("POST", "/api/v1/collections", "write:collections", 422),
            Case::new("PUT", "/api/v1/collections/0", "write:collections", 404),
            Case::new("DELETE", "/api/v1/collections/0", "write:collections", 404),
        ],
    )
    .await;
}

#[sqlx::test(migrations = "../db/migrations")]
#[allow(
    clippy::too_many_lines,
    reason = "explicit endpoint matrix or complete client flow"
)]
async fn granular_oauth_flow_can_upload_post_and_read(pool: PgPool) {
    use base64::Engine;
    use sha2::{Digest, Sha256};

    let mut fixture = Fixture::new(&pool).await;
    let scopes =
        "profile read:statuses write:statuses write:media read:collections write:collections";
    let (status, registration) = fixture.request("POST", "/api/v1/apps", None, json!({
        "client_name": "Granular client", "redirect_uris": "urn:ietf:wg:oauth:2.0:oob", "scopes": scopes,
    })).await;
    assert_eq!(status, StatusCode::OK, "{registration}");
    let verifier = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-._~";
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    let fields = [
        ("response_type", "code"),
        ("client_id", registration["client_id"].as_str().unwrap()),
        ("redirect_uri", "urn:ietf:wg:oauth:2.0:oob"),
        ("scope", scopes),
        ("state", "scope-test-state"),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("email", "alice@example.com"),
        ("password", "password"),
    ];
    let (status, html) = send(
        &fixture.app,
        Request::builder()
            .method("POST")
            .uri("/oauth/authorize")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(serde_urlencoded::to_string(fields).unwrap()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{html}");
    let code = html
        .as_str()
        .unwrap()
        .split("<pre class=\"oob-code\">")
        .nth(1)
        .expect("authorization code")
        .split("</pre>")
        .next()
        .unwrap();
    let (status, token) = fixture.request("POST", "/oauth/token", None, json!({
        "grant_type": "authorization_code", "client_id": registration["client_id"],
        "client_secret": registration["client_secret"], "redirect_uri": "urn:ietf:wg:oauth:2.0:oob",
        "code": code, "code_verifier": verifier,
    })).await;
    assert_eq!(status, StatusCode::OK, "{token}");
    assert_eq!(token["scope"], scopes);
    fixture.tokens.insert(
        "client".to_owned(),
        token["access_token"].as_str().unwrap().to_owned(),
    );
    let (status, account) = fixture
        .request(
            "GET",
            "/api/v1/accounts/verify_credentials",
            Some("client"),
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{account}");
    assert_eq!(account["id"], fixture.account_id.to_string());

    // Actual PNG uploads exercise both media routes and their shared guard.
    let image = image::DynamicImage::new_rgb8(2, 2);
    let mut png = std::io::Cursor::new(Vec::new());
    image.write_to(&mut png, image::ImageFormat::Png).unwrap();
    let mut media_ids = Vec::new();
    for path in ["/api/v1/media", "/api/v2/media"] {
        let mut multipart = b"--scope-test\r\nContent-Disposition: form-data; name=\"file\"; filename=\"test.png\"\r\nContent-Type: image/png\r\n\r\n".to_vec();
        multipart.extend_from_slice(png.get_ref());
        multipart.extend_from_slice(b"\r\n--scope-test--\r\n");
        let (status, uploaded) = send(
            &fixture.app,
            Request::builder()
                .method("POST")
                .uri(path)
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", fixture.tokens["client"]),
                )
                .header(
                    header::CONTENT_TYPE,
                    "multipart/form-data; boundary=scope-test",
                )
                .body(Body::from(multipart))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{path}: {uploaded}");
        media_ids.push(uploaded["id"].clone());
    }
    let (status, post) = fixture.request("POST", "/api/v1/statuses", Some("client"), json!({
        "status": "A post from a narrowly scoped client", "media_ids": media_ids, "visibility": "private",
    })).await;
    assert_eq!(status, StatusCode::OK, "{post}");
    assert_eq!(post["media_attachments"].as_array().unwrap().len(), 2);
    let id = post["id"].as_str().unwrap();
    let (status, timeline) = fixture
        .request("GET", "/api/v1/timelines/home", Some("client"), json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "{timeline}");
    assert!(
        timeline
            .as_array()
            .unwrap()
            .iter()
            .any(|post| post["id"] == id)
    );
    let (status, denied) = fixture
        .request("GET", "/api/v1/notifications", Some("client"), json!({}))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
    let (status, collection) = fixture
        .request(
            "POST",
            "/api/v1/collections",
            Some("client"),
            json!({"name":"Friends"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{collection}");
    let collection_path = format!(
        "/api/v1/collections/{}",
        collection["collection"]["id"].as_str().unwrap()
    );
    assert_eq!(
        fixture
            .request("GET", &collection_path, Some("client"), json!({}))
            .await
            .0,
        StatusCode::OK
    );

    let (status, revoked) = fixture.request("POST", "/oauth/revoke", None, json!({
        "client_id": registration["client_id"], "client_secret": registration["client_secret"], "token": token["access_token"],
    })).await;
    assert_eq!(status, StatusCode::OK, "{revoked}");
    assert_eq!(
        fixture
            .request("GET", "/api/v1/timelines/home", Some("client"), json!({}))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn scopes_preserve_visibility_and_ownership(pool: PgPool) {
    let fixture = Fixture::new(&pool).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let bob_user = user::create(
        &pool,
        bob.id,
        Some("bob@example.com"),
        &hash_password("password").unwrap(),
    )
    .await
    .unwrap();
    let bob_token = generate_secret();
    oauth::create_token(
        &pool,
        &hash_secret(&bob_token),
        fixture.registration.id,
        Some(bob_user.id),
        "read:statuses write:statuses",
    )
    .await
    .unwrap();
    for visibility in ["public", "private", "direct"] {
        let (status, post) = fixture
            .request(
                "POST",
                "/api/v1/statuses",
                Some("write:statuses"),
                json!({
                    "status": format!("A {visibility} post"), "visibility": visibility,
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{post}");
        let path = format!("/api/v1/statuses/{}", post["id"].as_str().unwrap());
        assert_eq!(
            fixture
                .request("GET", &path, Some("read:statuses"), json!({}))
                .await
                .0,
            StatusCode::OK
        );
        assert_eq!(
            fixture
                .request("GET", &path, Some("read:notifications"), json!({}))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        let expected = if visibility == "public" {
            StatusCode::OK
        } else {
            StatusCode::NOT_FOUND
        };
        assert_eq!(
            fixture.request("GET", &path, None, json!({})).await.0,
            expected
        );
        let (status, _) = send(
            &fixture.app,
            Request::builder()
                .uri(&path)
                .header(header::AUTHORIZATION, format!("Bearer {bob_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, expected);
        for method in ["PUT", "DELETE"] {
            let (status, body) = send(
                &fixture.app,
                Request::builder()
                    .method(method)
                    .uri(&path)
                    .header(header::AUTHORIZATION, format!("Bearer {bob_token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"status":"Not mine"}"#))
                    .unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{method}: {body}");
        }
        let (status, _) = fixture
            .request(
                "PUT",
                &path,
                Some("write:accounts"),
                json!({"status":"Wrong permission"}),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, edited) = fixture
            .request(
                "PUT",
                &path,
                Some("write:statuses"),
                json!({"status":"Edited by the owner"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{edited}");
        assert_eq!(
            fixture
                .request("DELETE", &path, Some("write:statuses"), json!({}))
                .await
                .0,
            StatusCode::OK
        );
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn uploads_and_registration_enforce_resource_scopes(pool: PgPool) {
    let fixture = Fixture::new(&pool).await;
    for scope in fixture.tokens.keys() {
        for path in ["/api/v1/media", "/api/v2/media"] {
            let (status, body) = send(
                &fixture.app,
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header(
                        header::AUTHORIZATION,
                        format!("Bearer {}", fixture.tokens[scope]),
                    )
                    .header(
                        header::CONTENT_TYPE,
                        "multipart/form-data; boundary=scope-test",
                    )
                    .body(Body::from("--scope-test--\r\n"))
                    .unwrap(),
            )
            .await;
            if matches!(scope.as_str(), "write" | "write:media") {
                assert_eq!(
                    status,
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "{path} {scope}: {body}"
                );
                assert_eq!(body["error"], "Validation failed: File can't be blank");
            } else {
                assert_eq!(status, StatusCode::FORBIDDEN, "{path} {scope}: {body}");
                assert_eq!(body["error"], SCOPE_ERROR);
            }
        }
        let token = generate_secret();
        oauth::create_token(
            &pool,
            &hash_secret(&token),
            fixture.registration.id,
            None,
            scope,
        )
        .await
        .unwrap();
        let (status, body) = send(
            &fixture.app,
            Request::builder()
                .method("POST")
                .uri("/api/v1/accounts")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await;
        if matches!(scope.as_str(), "write" | "write:accounts") {
            // Missing signup fields fail validation, after scope authorization.
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{scope}: {body}");
        } else {
            assert_eq!(status, StatusCode::FORBIDDEN, "{scope}: {body}");
            assert_eq!(body["error"], SCOPE_ERROR);
        }
    }
}
