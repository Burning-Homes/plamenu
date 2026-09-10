//! Integration tests for multi-account web sessions: the roster cookie that
//! tracks every account a browser is signed into, switching the active session
//! between them, and signing out falling back to the account chooser.
//!
//! The browser holds two cookies — `__Host-plamenu_session` (the active token)
//! and `__Host-plamenu_accounts` (the roster) — so the harness keeps a small
//! cookie jar rather than a single cookie string, and asserts on both.

mod common;

use std::collections::HashMap;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::create_local_account;
use http_body_util::BodyExt;
use plamenu::auth::hash_secret;
use plamenu_db::{PgPool, oauth, user};
use tower::ServiceExt;

const SESSION_COOKIE: &str = "__Host-plamenu_session";
const ROSTER_COOKIE: &str = "__Host-plamenu_accounts";
const WEB_CLIENT_ID: &str = "plamenu-web-ui";
const PASSWORD: &str = "correct horse battery";
/// Mirrors `web::session::MAX_ACCOUNTS` (private); the roster caps at this many.
const MAX_ACCOUNTS: usize = 5;

// ---- HTTP harness ------------------------------------------------------

struct Resp {
    status: StatusCode,
    location: Option<String>,
    set_cookies: Vec<String>,
    body: String,
}

async fn send(app: &Router, request: Request<Body>) -> Resp {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let location = response
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(ToOwned::to_owned);
    let set_cookies = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(ToOwned::to_owned)
        .collect();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Resp {
        status,
        location,
        set_cookies,
        body: String::from_utf8(bytes.to_vec()).unwrap(),
    }
}

/// A minimal cookie jar: applies `Set-Cookie` headers (honouring `Max-Age=0` as
/// a delete) and rebuilds the `Cookie` request header.
#[derive(Default)]
struct Jar {
    cookies: HashMap<String, String>,
}

impl Jar {
    fn update(&mut self, resp: &Resp) {
        for sc in &resp.set_cookies {
            let pair = sc.split(';').next().unwrap();
            let (name, value) = pair.split_once('=').unwrap();
            if value.is_empty() || sc.contains("Max-Age=0") {
                self.cookies.remove(name);
            } else {
                self.cookies.insert(name.to_owned(), value.to_owned());
            }
        }
    }

    fn header(&self) -> String {
        self.cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    fn get(&self, name: &str) -> Option<&String> {
        self.cookies.get(name)
    }

    /// The CSRF token the account chooser binds to the roster cookie — the same
    /// derivation the server uses (`csrf_token` over the roster cookie value).
    fn roster_csrf(&self) -> String {
        let raw = self.get(ROSTER_COOKIE).expect("roster cookie");
        hash_secret(&format!("plamenu-csrf:{raw}"))
    }

    /// The CSRF token the shell's logout form binds to the active session
    /// cookie — same derivation, keyed to the session token instead.
    fn session_csrf(&self) -> String {
        let raw = self.get(SESSION_COOKIE).expect("session cookie");
        hash_secret(&format!("plamenu-csrf:{raw}"))
    }

    fn roster_ids(&self) -> Vec<i64> {
        self.get(ROSTER_COOKIE)
            .map(|raw| {
                raw.split('~')
                    .filter_map(|e| e.split_once('.'))
                    .filter_map(|(id, _)| id.parse().ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn roster_token(&self, account_id: i64) -> Option<String> {
        let raw = self.get(ROSTER_COOKIE)?;
        raw.split('~')
            .filter_map(|e| e.split_once('.'))
            .find(|(id, _)| id.parse::<i64>().ok() == Some(account_id))
            .map(|(_, t)| t.to_owned())
    }
}

async fn get(app: &Router, uri: &str, jar: &Jar) -> Resp {
    let request = Request::builder()
        .uri(uri)
        .header(header::COOKIE, jar.header())
        .body(Body::empty())
        .unwrap();
    send(app, request).await
}

async fn post_form(app: &Router, uri: &str, jar: &Jar, fields: &[(&str, &str)]) -> Resp {
    let body = serde_urlencoded::to_string(fields).unwrap();
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(header::COOKIE, jar.header())
        .body(Body::from(body))
        .unwrap();
    send(app, request).await
}

fn hidden_value(body: &str, name: &str) -> String {
    let marker = format!("name=\"{name}\" value=\"");
    let start = body
        .find(&marker)
        .unwrap_or_else(|| panic!("missing hidden {name}"))
        + marker.len();
    body[start..].split('"').next().unwrap().to_owned()
}

// ---- Fixtures ----------------------------------------------------------

/// A confirmed, login-capable local account (username == email local part).
async fn seed(pool: &PgPool, username: &str) -> i64 {
    let account = create_local_account(pool, username, username).await;
    let hash = plamenu::auth::hash_password(PASSWORD).unwrap();
    let email = format!("{username}@example.com");
    user::create(pool, account.id, Some(&email), &hash)
        .await
        .unwrap();
    account.id
}

/// Signs `username` in through `POST /login`, folding the resulting cookies into
/// `jar` (which may already carry another account, exercising "add account").
async fn sign_in(app: &Router, jar: &mut Jar, username: &str) -> Resp {
    let email = format!("{username}@example.com");
    let resp = post_form(
        app,
        "/login",
        jar,
        &[("email", &email), ("password", PASSWORD)],
    )
    .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER, "sign-in {username}");
    jar.update(&resp);
    resp
}

async fn seed_oauth_app(pool: &PgPool) {
    oauth::create_app(
        pool,
        oauth::NewApp {
            name: "TestClient",
            website: None,
            client_id: "test-client",
            client_secret_hash: &hash_secret("test-secret"),
            redirect_uris: &["https://client.example/cb".to_owned()],
            scopes: "read write",
        },
    )
    .await
    .unwrap();
}

// ---- Tests -------------------------------------------------------------

#[sqlx::test(migrations = "../db/migrations")]
async fn signing_in_a_second_account_keeps_both_in_the_roster(pool: PgPool) {
    let alice = seed(&pool, "alice").await;
    let bob = seed(&pool, "bob").await;
    let app = common::test_app(pool);

    let mut jar = Jar::default();
    sign_in(&app, &mut jar, "alice").await;
    assert_eq!(
        jar.roster_ids(),
        vec![alice],
        "first login seeds the roster"
    );
    let alice_token = jar.get(SESSION_COOKIE).cloned().expect("session token");

    // Adding Bob (the browser still carries Alice's cookies) keeps Alice and
    // makes Bob active.
    sign_in(&app, &mut jar, "bob").await;
    let mut ids = jar.roster_ids();
    ids.sort_unstable();
    let mut expected = vec![alice, bob];
    expected.sort_unstable();
    assert_eq!(ids, expected, "roster holds both accounts");
    let bob_token = jar.get(SESSION_COOKIE).cloned().expect("session token");
    assert_ne!(alice_token, bob_token, "active session switched to Bob");
    assert_eq!(jar.roster_token(alice), Some(alice_token));

    // The chooser lists both accounts.
    let chooser = get(&app, "/login?switch=1", &jar).await;
    assert_eq!(chooser.status, StatusCode::OK);
    assert!(chooser.body.contains("Choose an account"));
    assert!(chooser.body.contains("@alice"));
    assert!(chooser.body.contains("@bob"));
    assert!(chooser.body.contains("Sign in to another account"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn switching_flips_the_active_session(pool: PgPool) {
    let alice = seed(&pool, "alice").await;
    let _bob = seed(&pool, "bob").await;
    let app = common::test_app(pool);

    let mut jar = Jar::default();
    sign_in(&app, &mut jar, "alice").await;
    sign_in(&app, &mut jar, "bob").await;
    let alice_token = jar.roster_token(alice).expect("alice in roster");

    // A bad CSRF token is refused.
    let bad = post_form(
        &app,
        "/web/accounts/switch",
        &jar,
        &[("account_id", &alice.to_string()), ("csrf", "not-it")],
    )
    .await;
    assert_eq!(bad.status, StatusCode::FORBIDDEN);

    // Switching to Alice makes her token the active session.
    let csrf = jar.roster_csrf();
    let switch = post_form(
        &app,
        "/web/accounts/switch",
        &jar,
        &[("account_id", &alice.to_string()), ("csrf", &csrf)],
    )
    .await;
    assert_eq!(switch.status, StatusCode::SEE_OTHER);
    assert_eq!(switch.location.as_deref(), Some("/"));
    jar.update(&switch);
    assert_eq!(jar.get(SESSION_COOKIE), Some(&alice_token));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn oauth_consent_can_choose_an_already_signed_in_account(pool: PgPool) {
    let alice = seed(&pool, "alice").await;
    let _bob = seed(&pool, "bob").await;
    seed_oauth_app(&pool).await;
    let app = common::test_app(pool.clone());

    let mut jar = Jar::default();
    sign_in(&app, &mut jar, "alice").await;
    let alice_token = jar.roster_token(alice).expect("alice in roster");
    sign_in(&app, &mut jar, "bob").await; // Bob is initially active.

    let authorize_uri = "/oauth/authorize?response_type=code&client_id=test-client\
        &redirect_uri=https%3A%2F%2Fclient.example%2Fcb&scope=read&state=choose-me";
    let consent = get(&app, authorize_uri, &jar).await;
    assert_eq!(consent.status, StatusCode::OK);
    assert!(consent.body.contains("Authorize"));
    assert!(consent.body.contains("Deny"));
    assert!(consent.body.contains("@alice"));
    assert!(consent.body.contains("@bob"));
    assert!(consent.body.contains("Use another signed-in account"));

    // OAuth account selection is roster-CSRF protected and preserves the
    // complete authorization request while changing only the active session.
    let bad = post_form(
        &app,
        "/oauth/authorize",
        &jar,
        &[
            ("client_id", "test-client"),
            ("redirect_uri", "https://client.example/cb"),
            ("scope", "read"),
            ("state", "choose-me"),
            ("decision", "switch"),
            ("account_id", &alice.to_string()),
            ("roster_csrf", "not-it"),
        ],
    )
    .await;
    assert_eq!(bad.status, StatusCode::FORBIDDEN);

    let roster_csrf = jar.roster_csrf();
    let switched = post_form(
        &app,
        "/oauth/authorize",
        &jar,
        &[
            ("client_id", "test-client"),
            ("redirect_uri", "https://client.example/cb"),
            ("scope", "read"),
            ("state", "choose-me"),
            ("decision", "switch"),
            ("account_id", &alice.to_string()),
            ("roster_csrf", &roster_csrf),
        ],
    )
    .await;
    assert_eq!(switched.status, StatusCode::SEE_OTHER);
    let next = switched.location.clone().expect("return to consent");
    assert!(next.starts_with("/oauth/authorize?"));
    assert!(next.contains("state=choose-me"));
    jar.update(&switched);
    assert_eq!(jar.get(SESSION_COOKIE), Some(&alice_token));

    let selected = get(&app, &next, &jar).await;
    assert_eq!(selected.status, StatusCode::OK);
    let current_start = selected
        .body
        .find("<div class=\"account-card account-card--current\"")
        .expect("current account card");
    let current = &selected.body[current_start..];
    let current = &current[..current.find("</div>").expect("current card end")];
    assert!(current.contains("@alice"), "Alice is the consent identity");

    // Approving now binds the grant to Alice, not to the account that happened
    // to be active when the client first opened the authorization page.
    let consent_csrf = hidden_value(&selected.body, "csrf");
    let allow = post_form(
        &app,
        "/oauth/authorize",
        &jar,
        &[
            ("client_id", "test-client"),
            ("redirect_uri", "https://client.example/cb"),
            ("scope", "read"),
            ("state", "choose-me"),
            ("decision", "allow"),
            ("csrf", &consent_csrf),
        ],
    )
    .await;
    assert_eq!(allow.status, StatusCode::FOUND);
    let callback = allow.location.as_deref().expect("client callback");
    assert!(callback.ends_with("&state=choose-me"));
    let code = callback
        .split("code=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap();
    let grant = oauth::take_grant(&pool, &hash_secret(code))
        .await
        .unwrap()
        .expect("authorization grant");
    let alice_user = user::find_by_account_id(&pool, alice)
        .await
        .unwrap()
        .expect("Alice user");
    assert_eq!(grant.user_id, alice_user.id);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn signing_out_drops_the_account_and_offers_the_rest(pool: PgPool) {
    let alice = seed(&pool, "alice").await;
    let bob = seed(&pool, "bob").await;
    let app = common::test_app(pool);

    let mut jar = Jar::default();
    sign_in(&app, &mut jar, "alice").await;
    sign_in(&app, &mut jar, "bob").await; // Bob active.

    let logout = post_form(&app, "/logout", &jar, &[("csrf", &jar.session_csrf())]).await;
    assert_eq!(logout.status, StatusCode::SEE_OTHER);
    assert_eq!(logout.location.as_deref(), Some("/login"));
    jar.update(&logout);

    // Active session cleared, but Alice stays in the roster (Bob is gone).
    assert!(jar.get(SESSION_COOKIE).is_none(), "active session cleared");
    assert_eq!(jar.roster_ids(), vec![alice]);
    assert!(!jar.roster_ids().contains(&bob));

    // Landing on /login with no active session but a roster shows the chooser.
    let chooser = get(&app, "/login", &jar).await;
    assert_eq!(chooser.status, StatusCode::OK);
    assert!(chooser.body.contains("Choose an account"));
    assert!(chooser.body.contains("@alice"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn signing_in_past_the_cap_revokes_the_evicted_session(pool: PgPool) {
    // MAX_ACCOUNTS is 5, so a sixth sign-in evicts the least-recently-added
    // account from the switcher.
    let names = ["a1", "a2", "a3", "a4", "a5", "a6"];
    let mut ids = Vec::new();
    for name in names {
        ids.push(seed(&pool, name).await);
    }
    let app = common::test_app(pool.clone());

    let mut jar = Jar::default();
    sign_in(&app, &mut jar, "a1").await;
    // Capture a1's token before any later sign-in can drop it from the roster.
    let a1_token = jar.roster_token(ids[0]).expect("a1 in roster");
    assert!(
        oauth::find_active_token(&pool, &hash_secret(&a1_token))
            .await
            .unwrap()
            .is_some(),
        "a1's session is live right after login",
    );

    for name in &names[1..] {
        sign_in(&app, &mut jar, name).await;
    }

    // The roster is capped at five and a1 (the oldest) has fallen off it...
    let roster = jar.roster_ids();
    assert_eq!(roster.len(), MAX_ACCOUNTS, "roster capped at MAX_ACCOUNTS");
    assert!(!roster.contains(&ids[0]), "a1 evicted from the switcher");

    // ...and crucially its token is revoked, not left live behind the browser's
    // back with no way to sign it out.
    assert!(
        oauth::find_active_token(&pool, &hash_secret(&a1_token))
            .await
            .unwrap()
            .is_none(),
        "the evicted session's token is revoked",
    );

    // A still-rostered account keeps its live session (control: eviction revokes
    // only the account it drops).
    let a6_token = jar.roster_token(ids[5]).expect("a6 still in roster");
    assert!(
        oauth::find_active_token(&pool, &hash_secret(&a6_token))
            .await
            .unwrap()
            .is_some(),
        "a still-rostered account keeps its live session",
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn switching_to_a_revoked_session_prunes_it(pool: PgPool) {
    let alice = seed(&pool, "alice").await;
    let _bob = seed(&pool, "bob").await;
    let app = common::test_app(pool.clone());

    let mut jar = Jar::default();
    sign_in(&app, &mut jar, "alice").await;
    sign_in(&app, &mut jar, "bob").await;

    // Revoke Alice's session out from under the roster (as another device would).
    let alice_token = jar.roster_token(alice).expect("alice in roster");
    let web_app = oauth::find_app_by_client_id(&pool, WEB_CLIENT_ID)
        .await
        .unwrap()
        .expect("web app bootstrapped on first login");
    oauth::revoke_token(&pool, &hash_secret(&alice_token), web_app.id)
        .await
        .unwrap();

    let csrf = jar.roster_csrf();
    let switch = post_form(
        &app,
        "/web/accounts/switch",
        &jar,
        &[("account_id", &alice.to_string()), ("csrf", &csrf)],
    )
    .await;
    assert_eq!(switch.status, StatusCode::SEE_OTHER);
    assert_eq!(switch.location.as_deref(), Some("/login?switch=1"));
    jar.update(&switch);
    assert!(
        !jar.roster_ids().contains(&alice),
        "the dead session is pruned from the roster"
    );
}
