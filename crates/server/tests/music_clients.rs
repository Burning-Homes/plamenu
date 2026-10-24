mod common;
use axum::{
    body::Body,
    http::{Request as HttpRequest, StatusCode},
};
use common::{StubFederation, create_local_account, test_state_with};
use http_body_util::BodyExt;
use md5::{Digest, Md5};
use std::fmt::Write;
fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, byte| {
        write!(out, "{byte:02x}").unwrap();
        out
    })
}
use plamenu::{
    AppState,
    auth::{generate_secret, hash_password, hash_secret},
    music_clients::{
        self, Request,
        auth::{Credentials, authenticate},
    },
};
use plamenu_db::{PgPool, oauth, user};
use tower::ServiceExt;
async fn fixture(pool: PgPool) -> (AppState, i64, i64) {
    let owner = create_local_account(&pool, "alice", "Alice").await.id;
    let user = user::create(
        &pool,
        owner,
        Some("alice@plamenu.test"),
        &hash_password("fixture-password").unwrap(),
    )
    .await
    .unwrap();
    (
        test_state_with(pool, StubFederation::with_actors([])),
        owner,
        user.id,
    )
}
fn input(manage: bool) -> Request {
    Request {
        label: "Music app".into(),
        days: 90,
        can_manage: manage,
        broadcast_playing: false,
    }
}
fn credentials(password: &str) -> Credentials<'_> {
    Credentials {
        username: "alice",
        password: Some(password),
        ..Default::default()
    }
}
async fn oauth_token(state: &AppState, user: i64, scopes: &str) -> (i64, String) {
    let app = oauth::create_app(
        &state.pool,
        oauth::NewApp {
            name: "Music client tests",
            website: None,
            client_id: &generate_secret(),
            client_secret_hash: &hash_secret(&generate_secret()),
            redirect_uris: &[],
            scopes: "read write",
        },
    )
    .await
    .unwrap();
    let token = generate_secret();
    let row = oauth::create_token(
        &state.pool,
        &hash_secret(&token),
        app.id,
        Some(user),
        scopes,
    )
    .await
    .unwrap();
    (row.id, token)
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn client_passwords_support_encoded_keys_and_salted_auth_without_plaintext_storage(
    pool: PgPool,
) {
    let (state, owner, _) = fixture(pool).await;
    let issued = music_clients::create(&state, owner, None, &input(false))
        .await
        .unwrap();
    let row: (String, String) =
        sqlx::query_as("SELECT encrypted_token,token_hash FROM music_client_keys WHERE id=$1")
            .bind(issued.key.id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert!(!row.0.contains(&issued.password));
    assert_eq!(row.1, hash_secret(&issued.password));
    let json = serde_json::to_string(&music_clients::list(&state, owner).await.unwrap()).unwrap();
    assert!(!json.contains(&issued.password) && !json.contains(&row.0) && !json.contains(&row.1));
    let principal = authenticate(&state, &credentials(&issued.password))
        .await
        .unwrap();
    assert_eq!(principal.owner, owner);
    assert!(principal.require_manage().is_err());
    let encoded = format!("enc:{}", hex(issued.password.as_bytes()));
    assert!(authenticate(&state, &credentials(&encoded)).await.is_ok());
    let api = Credentials {
        username: "alice",
        api_key: Some(&issued.password),
        ..Default::default()
    };
    assert!(authenticate(&state, &api).await.is_ok());
    let digest = hex(&Md5::new()
        .chain_update(issued.password.as_bytes())
        .chain_update(b"songhive-salt")
        .finalize());
    let salted = Credentials {
        username: "alice",
        token: Some(&digest),
        salt: Some("songhive-salt"),
        ..Default::default()
    };
    assert!(authenticate(&state, &salted).await.is_ok());
    assert!(
        authenticate(
            &state,
            &Credentials {
                salt: Some("wrong"),
                ..salted
            }
        )
        .await
        .is_err()
    );
    assert!(
        authenticate(
            &state,
            &Credentials {
                username: "bob",
                api_key: Some(&issued.password),
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
    assert!(authenticate(&state, &credentials("enc:zz")).await.is_err());
    music_clients::revoke(&state, owner, issued.key.id)
        .await
        .unwrap();
    assert!(principal.current(&state).await.is_err());
    assert!(authenticate(&state, &api).await.is_err());
    assert!(authenticate(&state, &salted).await.is_err());
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn client_parent_grants_expiry_scope_changes_and_operator_key_rotation_are_current(
    pool: PgPool,
) {
    let (mut state, owner, user) = fixture(pool).await;
    let (parent, token) = oauth_token(&state, user, "read:statuses").await;
    assert!(
        music_clients::create(&state, owner, Some(parent), &input(true))
            .await
            .is_err()
    );
    let issued = music_clients::create(&state, owner, Some(parent), &input(false))
        .await
        .unwrap();
    let principal = authenticate(&state, &credentials(&issued.password))
        .await
        .unwrap();
    let digest = hex(&Md5::new()
        .chain_update(issued.password.as_bytes())
        .chain_update(b"salt")
        .finalize());
    let salted = Credentials {
        username: "alice",
        token: Some(&digest),
        salt: Some("salt"),
        ..Default::default()
    };
    let old = state.config.encryption_secret.clone().unwrap();
    let config = std::sync::Arc::make_mut(&mut state.config);
    config
        .encryption_previous_secrets
        .push((config.encryption_secret_version, old));
    config.encryption_secret_version += 1;
    config.encryption_secret = Some(
        "new-music-client-encryption-secret-at-least-32-bytes"
            .to_owned()
            .into(),
    );
    assert!(authenticate(&state, &salted).await.is_ok());
    std::sync::Arc::make_mut(&mut state.config)
        .encryption_previous_secrets
        .clear();
    assert!(authenticate(&state, &salted).await.is_err());
    assert!(
        authenticate(&state, &credentials(&issued.password))
            .await
            .is_ok()
    );
    let read = authenticate(&state, &credentials(&token)).await.unwrap();
    assert!(read.require_manage().is_err());
    sqlx::query("UPDATE oauth_tokens SET revoked_at=now() WHERE id=$1")
        .bind(parent)
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(principal.current(&state).await.is_err());
    assert!(read.current(&state).await.is_err());
    assert!(
        authenticate(&state, &credentials(&issued.password))
            .await
            .is_err()
    );
    let independent = music_clients::create(&state, owner, None, &input(true))
        .await
        .unwrap();
    let principal = authenticate(&state, &credentials(&independent.password))
        .await
        .unwrap();
    assert!(principal.require_manage().is_ok());
    sqlx::query("UPDATE music_client_keys SET expires_at=now()-interval '1 second',created_at=now()-interval '2 days' WHERE id=$1").bind(independent.key.id).execute(&state.pool).await.unwrap();
    assert!(principal.current(&state).await.is_err());
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn password_auth_uses_native_login_policy_and_client_keys_cannot_bypass_account_retirement(
    pool: PgPool,
) {
    let (state, owner, user) = fixture(pool).await;
    assert!(
        authenticate(&state, &credentials("fixture-password"))
            .await
            .is_ok()
    );
    let issued = music_clients::create(&state, owner, None, &input(true))
        .await
        .unwrap();
    let key = authenticate(&state, &credentials(&issued.password))
        .await
        .unwrap();
    sqlx::query("UPDATE users SET otp_required_for_login=true WHERE id=$1")
        .bind(user)
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(
        authenticate(&state, &credentials("fixture-password"))
            .await
            .is_err()
    );
    assert!(
        authenticate(&state, &credentials(&issued.password))
            .await
            .is_ok()
    );
    sqlx::query("UPDATE users SET disabled=true WHERE id=$1")
        .bind(user)
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(key.current(&state).await.is_err());
    assert!(
        music_clients::create(&state, owner, None, &input(false))
            .await
            .is_err()
    );
    sqlx::query("UPDATE users SET disabled=false WHERE id=$1")
        .bind(user)
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE accounts SET suspended_at=now() WHERE id=$1")
        .bind(owner)
        .execute(&state.pool)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM music_client_keys WHERE account_id=$1")
            .bind(owner)
            .fetch_one(&state.pool)
            .await
            .unwrap(),
        0
    );
    assert!(key.current(&state).await.is_err());
}
async fn request(
    app: &axum::Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    cookie: Option<&str>,
    body: &str,
) -> axum::response::Response {
    let mut req = HttpRequest::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/x-www-form-urlencoded");
    if let Some(token) = token {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    if let Some(cookie) = cookie {
        req = req.header("Cookie", cookie);
    }
    app.clone()
        .oneshot(req.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap()
}
async fn text(response: axum::response::Response) -> String {
    String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap()
}
#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn native_client_forms_show_password_once_require_csrf_and_keep_one_music_workspace(
    pool: PgPool,
) {
    let (state, owner, user) = fixture(pool).await;
    let (_, token) = oauth_token(&state, user, "read:statuses").await;
    let app = plamenu::build_router(state.clone());
    assert_eq!(
        request(&app, "GET", "/api/v1/music/clients", Some(&token), None, "")
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    let login = request(
        &app,
        "POST",
        "/login",
        None,
        None,
        "email=alice%40plamenu.test&password=fixture-password",
    )
    .await;
    assert_eq!(login.status(), StatusCode::SEE_OTHER);
    let cookie = login
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let csrf = hash_secret(&format!(
        "plamenu-csrf:{}",
        cookie.split_once('=').unwrap().1
    ));
    let path = "/web/music/clients/create";
    assert_eq!(
        request(
            &app,
            "POST",
            path,
            None,
            Some(&cookie),
            "csrf=wrong&label=App&days=90"
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let form = serde_urlencoded::to_string([
        ("csrf", csrf.as_str()),
        ("label", "Browser app"),
        ("days", "90"),
    ])
    .unwrap();
    let created = request(&app, "POST", path, None, Some(&cookie), &form).await;
    assert_eq!(created.status(), StatusCode::OK);
    assert!(
        created.headers()["cache-control"]
            .to_str()
            .unwrap()
            .contains("no-store")
    );
    let html = text(created).await;
    let password = html
        .split("id=\"music-client-password\"")
        .nth(1)
        .unwrap()
        .split("value=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    assert!(password.starts_with("pmusic_"));
    assert!(authenticate(&state, &credentials(password)).await.is_ok());
    let listing = text(request(&app, "GET", "/music/clients", None, Some(&cookie), "").await).await;
    assert!(listing.contains("Browser app"));
    assert!(!listing.contains(password));
    assert!(!listing.contains("id=\"music-client-password\""));
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/v1/music/clients",
            Some(password),
            None,
            ""
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    let key = music_clients::list(&state, owner).await.unwrap().remove(0);
    let revoke = format!("/web/music/clients/{}/revoke", key.id);
    assert_eq!(
        request(
            &app,
            "POST",
            &revoke,
            None,
            Some(&cookie),
            &format!("csrf={csrf}")
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    assert!(authenticate(&state, &credentials(password)).await.is_err());
}

#[sqlx::test(migrator = "plamenu_db::MIGRATOR")]
async fn legacy_password_clients_reuse_verification_and_close_on_password_or_mfa_changes(
    pool: PgPool,
) {
    let (state, _, user) = fixture(pool).await;
    let principal = authenticate(&state, &credentials("fixture-password"))
        .await
        .unwrap();
    // A browsing client may authenticate every request without exhausting the
    // native budget for cold password checks; no usable password is cached.
    for _ in 0..30 {
        assert!(
            authenticate(&state, &credentials("fixture-password"))
                .await
                .is_ok()
        );
    }
    sqlx::query("UPDATE users SET password_hash=$2 WHERE id=$1")
        .bind(user)
        .bind(hash_password("changed-password").unwrap())
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(principal.current(&state).await.is_err());
    assert!(
        authenticate(&state, &credentials("fixture-password"))
            .await
            .is_err()
    );
    let changed = authenticate(&state, &credentials("changed-password"))
        .await
        .unwrap();
    sqlx::query("UPDATE users SET otp_required_for_login=true WHERE id=$1")
        .bind(user)
        .execute(&state.pool)
        .await
        .unwrap();
    assert!(changed.current(&state).await.is_err());
    assert!(
        authenticate(&state, &credentials("changed-password"))
            .await
            .is_err()
    );
}
