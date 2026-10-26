mod common;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
    response::Response,
};
use common::{StubFederation, create_local_account, test_state_with};
use http_body_util::BodyExt;
use plamenu::{
    auth::{hash_password, hash_secret},
    music_radios, music_saved,
};
use plamenu_db::{PgPool, user};
use tower::ServiceExt;
async fn request(
    app: &Router,
    cookie: &str,
    method: &str,
    path: &str,
    fields: &[(&str, &str)],
) -> Response {
    app.clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header(header::COOKIE, cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(serde_urlencoded::to_string(fields).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
}
async fn text(response: Response) -> String {
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
async fn native_saved_forms_require_csrf_revoke_metadata_and_remove_unavailable_rows(pool: PgPool) {
    let owner = create_local_account(&pool, "alice", "Alice").await.id;
    let viewer = create_local_account(&pool, "bob", "Bob").await.id;
    user::create(
        &pool,
        viewer,
        Some("bob@plamenu.test"),
        &hash_password("fixture-password").unwrap(),
    )
    .await
    .unwrap();
    let state = test_state_with(pool, StubFederation::with_actors([]));
    let radio = music_radios::create(
        &state,
        owner,
        &music_radios::New {
            name: "Native saved station".into(),
            description: String::new(),
            config: "Owner-only key".into(),
            visibility: "public".into(),
        },
    )
    .await
    .unwrap();
    let app = plamenu::build_router(state.clone());
    let login = request(
        &app,
        "",
        "POST",
        "/login",
        &[
            ("email", "bob@plamenu.test"),
            ("password", "fixture-password"),
        ],
    )
    .await;
    assert_eq!(login.status(), StatusCode::SEE_OTHER);
    let cookie = login
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|header| header.to_str().ok())
        .find(|value| value.starts_with("__Host-plamenu_session="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let token = cookie.split_once('=').unwrap().1;
    let csrf = hash_secret(&format!("plamenu-csrf:{token}"));
    let id = radio.id.to_string();
    let fields = [
        ("csrf", "invalid"),
        ("kind", "radio"),
        ("id", id.as_str()),
        ("operation", "save"),
    ];
    assert_eq!(
        request(&app, &cookie, "POST", "/web/music/saved", &fields)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert!(
        music_saved::page(&state, viewer, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let page = request(&app, &cookie, "GET", &format!("/music/radios/{id}"), &[]).await;
    assert_eq!(page.status(), StatusCode::OK);
    let html = text(page).await;
    assert!(html.contains("Save to your library"));
    assert!(!html.contains("Owner-only key"));
    let fields = [
        ("csrf", csrf.as_str()),
        ("kind", "radio"),
        ("id", id.as_str()),
        ("operation", "save"),
        ("return_to", "https://evil.example/"),
    ];
    let saved = request(&app, &cookie, "POST", "/web/music/saved", &fields).await;
    assert_eq!(saved.status(), StatusCode::SEE_OTHER);
    assert_eq!(saved.headers()[header::LOCATION], "/music/mine");
    let mine = request(&app, &cookie, "GET", "/music/mine", &[]).await;
    assert_eq!(mine.headers()[header::CACHE_CONTROL], "private, no-store");
    let html = text(mine).await;
    assert!(html.contains("Native saved station"));
    assert!(html.contains("Remove from your library"));
    assert!(!html.contains("Owner-only key"));
    music_radios::update(
        &state,
        owner,
        radio.id,
        radio.revision,
        &music_radios::New {
            name: radio.name,
            description: String::new(),
            config: radio.config,
            visibility: "private".into(),
        },
    )
    .await
    .unwrap();
    let mine = text(request(&app, &cookie, "GET", "/music/mine", &[]).await).await;
    assert!(!mine.contains("Native saved station"));
    assert!(!mine.contains("Owner-only key"));
    let manager = text(request(&app, &cookie, "GET", "/music/saved", &[]).await).await;
    assert!(manager.contains("This saved resource is no longer available to you."));
    assert!(!manager.contains("Native saved station"));
    let rows = music_saved::page(&state, viewer, None).await.unwrap();
    let saved_id = &rows.items[0].id;
    assert_eq!(
        request(
            &app,
            &cookie,
            "POST",
            "/web/music/saved/remove",
            &[("csrf", "invalid"), ("saved_id", saved_id)]
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let removed = request(
        &app,
        &cookie,
        "POST",
        "/web/music/saved/remove",
        &[("csrf", csrf.as_str()), ("saved_id", saved_id)],
    )
    .await;
    assert_eq!(removed.status(), StatusCode::SEE_OTHER);
    assert!(
        music_saved::page(&state, viewer, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}
