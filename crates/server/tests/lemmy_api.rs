//! Lemmy `/api/v3` compatibility contract tests. These deliberately exercise
//! the public router rather than calling adapter handlers directly.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{RemoteUser, StubFederation, create_local_account, test_app, test_state_with};
use http_body_util::BodyExt;
use plamenu::auth::{hash_password, hash_secret};
use plamenu_db::lemmy_id::Kind;
use plamenu_db::{PgPool, oauth, role, status, user};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tower::ServiceExt;

struct ApiResponse {
    status: StatusCode,
    json: Value,
}

async fn send(app: Router, request: Request<Body>) -> ApiResponse {
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes)
        .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    ApiResponse { status, json }
}

async fn request_json(
    app: Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> ApiResponse {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let body = match body {
        Some(body) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(&body).unwrap())
        }
        None => Body::empty(),
    };
    send(app, builder.body(body).unwrap()).await
}

fn sample_png() -> Vec<u8> {
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        32,
        32,
        image::Rgb([80, 150, 220]),
    ))
    .write_to(
        &mut std::io::Cursor::new(&mut bytes),
        image::ImageFormat::Png,
    )
    .unwrap();
    bytes
}

async fn upload_pictrs(app: Router, bearer: &str, bytes: &[u8]) -> ApiResponse {
    const BOUNDARY: &str = "lemmy-pictrs-test-boundary";
    let mut body = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"images[]\"; filename=\"image.png\"\r\nContent-Type: image/png\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    send(
        app,
        Request::builder()
            .method("POST")
            .uri("/pictrs/image")
            .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(Body::from(body))
            .unwrap(),
    )
    .await
}

async fn create_user(
    pool: &PgPool,
    username: &str,
    email: &str,
    password: &str,
) -> (plamenu_db::account::Account, user::User) {
    let account = create_local_account(pool, username, username).await;
    let password_hash = hash_password(password).unwrap();
    let user = user::create(pool, account.id, Some(email), &password_hash)
        .await
        .unwrap();
    (account, user)
}

async fn login(pool: &PgPool, identifier: &str, password: &str) -> String {
    let response = request_json(
        test_app(pool.clone()),
        "POST",
        "/api/v3/user/login",
        None,
        Some(json!({
            "username_or_email": identifier,
            "password": password,
        })),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json);
    response.json["jwt"].as_str().unwrap().to_owned()
}

async fn lemmy_id(pool: &PgPool, kind: Kind, native_id: i64) -> i32 {
    plamenu_db::lemmy_id::alias_for(pool, kind, native_id)
        .await
        .unwrap()
}

async fn native_id(pool: &PgPool, kind: Kind, alias: i64) -> i64 {
    let alias = i32::try_from(alias).expect("Lemmy IDs are signed 32-bit values");
    plamenu_db::lemmy_id::resolve(pool, kind, alias)
        .await
        .unwrap()
        .expect("response alias resolves to a native ID")
}

#[sqlx::test(migrations = "../db/migrations")]
async fn login_site_validate_and_logout_follow_lemmy_contract(pool: PgPool) {
    create_user(&pool, "lemmy_user", "lemmy@example.test", "correct horse").await;

    let anonymous = request_json(test_app(pool.clone()), "GET", "/api/v3/site", None, None).await;
    assert_eq!(anonymous.status, StatusCode::OK);
    assert_eq!(anonymous.json["version"], "0.19.11");
    assert!(anonymous.json.get("my_user").is_none() || anonymous.json["my_user"].is_null());
    assert_eq!(anonymous.json["site_view"]["counts"]["users"], 1);

    let wrong = request_json(
        test_app(pool.clone()),
        "POST",
        "/api/v3/user/login",
        None,
        Some(json!({
            "username_or_email": "lemmy_user",
            "password": "wrong password",
        })),
    )
    .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert_eq!(wrong.json, json!({ "error": "incorrect_login" }));

    let token = login(&pool, "lemmy_user", "correct horse").await;
    let site = request_json(
        test_app(pool.clone()),
        "GET",
        "/api/v3/site",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(site.status, StatusCode::OK, "{:?}", site.json);
    assert_eq!(
        site.json["my_user"]["local_user_view"]["person"]["name"],
        "lemmy_user"
    );
    assert!(site.json["my_user"]["follows"].is_array());

    let valid = request_json(
        test_app(pool.clone()),
        "GET",
        "/api/v3/user/validate_auth",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(valid.json, json!({ "success": true }));

    let logged_out = request_json(
        test_app(pool.clone()),
        "POST",
        "/api/v3/user/logout",
        Some(&token),
        Some(json!({})),
    )
    .await;
    assert_eq!(logged_out.json, json!({ "success": true }));
    let invalid = request_json(
        test_app(pool),
        "GET",
        "/api/v3/user/validate_auth",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(invalid.status, StatusCode::UNAUTHORIZED);
    assert_eq!(invalid.json, json!({ "error": "not_logged_in" }));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn remote_html_is_exposed_as_markdown_in_posts_and_comments(pool: PgPool) {
    let owner = create_local_account(&pool, "html_owner", "HTML owner").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let (group, _) = plamenu::groups::create_group(
        &state,
        plamenu::groups::CreateGroupParams {
            name: "html_compat",
            display_name: "HTML compatibility",
            membership_policy: plamenu_db::group::MembershipPolicy::Open,
            posting_policy: plamenu_db::group::PostingPolicy::Anyone,
            created_by: owner.id,
            enforce_username_blocklist: false,
            enforce_account_quota: false,
        },
    )
    .await
    .unwrap();
    let remote = RemoteUser::new("remote.example", "alice");
    let author = plamenu::remote::store_remote_actor(&pool, &remote.actor)
        .await
        .unwrap();
    let root = status::upsert_remote(
        &pool,
        status::NewRemoteStatus {
            uri: "https://remote.example/users/alice/statuses/1",
            account_id: author.id,
            content: "<p>A <strong>remote</strong> post</p>",
            created_at: OffsetDateTime::now_utc(),
            visibility: "public",
            in_reply_to_id: None,
            in_reply_to_uri: None,
            spoiler_text: "",
            sensitive: false,
            language: None,
            url: None,
            quote_approval_policy: 0,
            title: Some("Remote post"),
            object_type: Some("Page"),
            external_url: None,
        },
    )
    .await
    .unwrap();
    plamenu_db::mention::attach(&pool, root.id, group.id, true)
        .await
        .unwrap();
    status::upsert_remote(
        &pool,
        status::NewRemoteStatus {
            uri: "https://remote.example/users/alice/statuses/2",
            account_id: author.id,
            content: concat!(
                "<p><a href=\"https://remote.example/@alice\" rel=\"nofollow\">",
                "@alice</a> ping!</p>"
            ),
            created_at: OffsetDateTime::now_utc(),
            visibility: "public",
            in_reply_to_id: Some(root.id),
            in_reply_to_uri: root.uri.as_deref(),
            spoiler_text: "",
            sensitive: false,
            language: None,
            url: None,
            quote_approval_policy: 0,
            title: None,
            object_type: None,
            external_url: None,
        },
    )
    .await
    .unwrap();
    let post_id = lemmy_id(&pool, Kind::Status, root.id).await;
    let app = plamenu::build_router(state);

    let post = request_json(
        app.clone(),
        "GET",
        &format!("/api/v3/post?id={post_id}"),
        None,
        None,
    )
    .await;
    assert_eq!(post.status, StatusCode::OK, "{:?}", post.json);
    assert_eq!(post.json["post_view"]["post"]["body"], "A **remote** post");

    let comments = request_json(
        app,
        "GET",
        &format!("/api/v3/comment/list?post_id={post_id}"),
        None,
        None,
    )
    .await;
    assert_eq!(comments.status, StatusCode::OK, "{:?}", comments.json);
    assert_eq!(
        comments.json["comments"][0]["comment"]["content"],
        "[@alice](https://remote.example/@alice) ping!"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn remote_communities_have_markdown_descriptions_exact_feeds_and_all_directory(pool: PgPool) {
    let mut remote_group = RemoteUser::new("lemmy.test", "technology");
    remote_group.actor.kind = "Group".to_owned();
    remote_group.actor.summary = Some(
        "<p>A <strong>remote</strong> community with <a href=\"https://example.test\">rules</a>.</p>"
            .to_owned(),
    );
    let group = plamenu::remote::store_remote_actor(&pool, &remote_group.actor)
        .await
        .unwrap();
    let remote_author = RemoteUser::new("lemmy.test", "poster");
    let author = plamenu::remote::store_remote_actor(&pool, &remote_author.actor)
        .await
        .unwrap();
    let target = status::upsert_remote(
        &pool,
        status::NewRemoteStatus {
            uri: "https://lemmy.test/post/1",
            account_id: author.id,
            content: "<p>Technology post</p>",
            created_at: OffsetDateTime::now_utc(),
            visibility: "public",
            in_reply_to_id: None,
            in_reply_to_uri: None,
            spoiler_text: "",
            sensitive: false,
            language: None,
            url: None,
            quote_approval_policy: 0,
            title: Some("Technology post"),
            object_type: Some("Page"),
            external_url: None,
        },
    )
    .await
    .unwrap();
    status::upsert_remote_reblog(
        &pool,
        "https://lemmy.test/c/technology/announces/1",
        group.id,
        target.id,
        None,
    )
    .await
    .unwrap();

    // A separate local community post makes a failed-open name resolver
    // observable: it must never leak into the remote community feed.
    let owner = create_local_account(&pool, "other_owner", "Other owner").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let (other_group, _) = plamenu::groups::create_group(
        &state,
        plamenu::groups::CreateGroupParams {
            name: "other_group",
            display_name: "Other group",
            membership_policy: plamenu_db::group::MembershipPolicy::Open,
            posting_policy: plamenu_db::group::PostingPolicy::Anyone,
            created_by: owner.id,
            enforce_username_blocklist: false,
            enforce_account_quota: false,
        },
    )
    .await
    .unwrap();
    let other = status::create_local(
        &pool,
        status::NewLocalStatus::new(owner.id, "other post", "public", None),
    )
    .await
    .unwrap();
    plamenu_db::mention::attach(&pool, other.id, other_group.id, true)
        .await
        .unwrap();
    let app = plamenu::build_router(state);

    let detail = request_json(
        app.clone(),
        "GET",
        "/api/v3/community?name=technology%40lemmy.test",
        None,
        None,
    )
    .await;
    assert_eq!(detail.status, StatusCode::OK, "{:?}", detail.json);
    assert_eq!(
        detail.json["community_view"]["community"]["description"],
        "A **remote** community with [rules](https://example.test)."
    );

    let feed = request_json(
        app.clone(),
        "GET",
        "/api/v3/post/list?community_name=technology%40lemmy.test&type_=All&sort=Hot&limit=20",
        None,
        None,
    )
    .await;
    assert_eq!(feed.status, StatusCode::OK, "{:?}", feed.json);
    assert_eq!(feed.json["posts"].as_array().unwrap().len(), 1);
    assert_eq!(feed.json["posts"][0]["post"]["name"], "Technology post");
    assert_eq!(feed.json["posts"][0]["community"]["name"], "technology");

    let all = request_json(
        app.clone(),
        "GET",
        "/api/v3/community/list?type_=All&limit=50",
        None,
        None,
    )
    .await;
    assert_eq!(all.status, StatusCode::OK, "{:?}", all.json);
    assert!(
        all.json["communities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|view| view["community"]["actor_id"] == "https://lemmy.test/users/technology")
    );
    let local = request_json(
        app.clone(),
        "GET",
        "/api/v3/community/list?type_=Local&limit=50",
        None,
        None,
    )
    .await;
    assert!(
        local.json["communities"]
            .as_array()
            .unwrap()
            .iter()
            .all(|view| view["community"]["local"] == true)
    );

    let missing = request_json(
        app,
        "GET",
        "/api/v3/post/list?community_name=missing%40lemmy.test&type_=All",
        None,
        None,
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.json, json!({ "error": "couldnt_find_community" }));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn community_feed_does_not_lose_old_posts_behind_global_timeline(pool: PgPool) {
    let (owner, _) = create_user(
        &pool,
        "history_owner",
        "history@example.test",
        "history password",
    )
    .await;
    let token = login(&pool, "history_owner", "history password").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let (group, _) = plamenu::groups::create_group(
        &state,
        plamenu::groups::CreateGroupParams {
            name: "history_group",
            display_name: "History group",
            membership_policy: plamenu_db::group::MembershipPolicy::Open,
            posting_policy: plamenu_db::group::PostingPolicy::Anyone,
            created_by: owner.id,
            enforce_username_blocklist: false,
            enforce_account_quota: false,
        },
    )
    .await
    .unwrap();
    let mut old_form = status::NewLocalStatus::new(owner.id, "old body", "public", None);
    old_form.title = Some("Old community post");
    let old = status::create_local(&pool, old_form).await.unwrap();
    plamenu_db::mention::attach(&pool, old.id, group.id, true)
        .await
        .unwrap();

    // The old implementation fetched at most the newest 50 global rows and
    // filtered them afterward, so this unrelated traffic hid `old`.
    for index in 0..60 {
        status::create_local(
            &pool,
            status::NewLocalStatus::new(owner.id, &format!("noise {index}"), "public", None),
        )
        .await
        .unwrap();
    }
    let mut new_form = status::NewLocalStatus::new(owner.id, "new body", "public", None);
    new_form.title = Some("New community post");
    let new = status::create_local(&pool, new_form).await.unwrap();
    plamenu_db::mention::attach(&pool, new.id, group.id, true)
        .await
        .unwrap();
    let app = plamenu::build_router(state);

    for bearer in [None, Some(token.as_str())] {
        let feed = request_json(
            app.clone(),
            "GET",
            "/api/v3/post/list?community_name=history_group%40plamenu.test&type_=All&sort=Hot&limit=20",
            bearer,
            None,
        )
        .await;
        assert_eq!(feed.status, StatusCode::OK, "{:?}", feed.json);
        let names = feed.json["posts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|view| view["post"]["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 2, "{:?}", feed.json);
        assert!(names.contains(&"Old community post"));
        assert!(names.contains(&"New community post"));
    }

    let first = request_json(
        app.clone(),
        "GET",
        "/api/v3/post/list?community_name=history_group%40plamenu.test&type_=All&sort=New&limit=1",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(first.json["posts"][0]["post"]["name"], "New community post");
    let cursor = first.json["next_page"].as_str().unwrap();
    let second = request_json(
        app,
        "GET",
        &format!(
            "/api/v3/post/list?community_name=history_group%40plamenu.test&type_=All&sort=New&limit=1&page_cursor={cursor}"
        ),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(second.status, StatusCode::OK, "{:?}", second.json);
    assert_eq!(
        second.json["posts"][0]["post"]["name"],
        "Old community post"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn admin_promotion_bans_and_purges_are_real_and_audited(pool: PgPool) {
    let (owner_account, _) =
        create_user(&pool, "owner", "owner@example.test", "owner password").await;
    let (target_account, _) =
        create_user(&pool, "target", "target@example.test", "target password").await;
    role::assign_to_account(&pool, owner_account.id, Some(3))
        .await
        .unwrap();
    let token = login(&pool, "owner", "owner password").await;
    let target_id = lemmy_id(&pool, Kind::Account, target_account.id).await;

    let promoted = request_json(
        test_app(pool.clone()),
        "POST",
        "/api/v3/admin/add",
        Some(&token),
        Some(json!({ "person_id": target_id, "added": true })),
    )
    .await;
    assert_eq!(promoted.status, StatusCode::OK, "{:?}", promoted.json);
    assert!(
        promoted.json["admins"]
            .as_array()
            .unwrap()
            .iter()
            .any(|view| view["person"]["id"] == target_id)
    );

    let demoted = request_json(
        test_app(pool.clone()),
        "POST",
        "/api/v3/admin/add",
        Some(&token),
        Some(json!({ "person_id": target_id, "added": false })),
    )
    .await;
    assert_eq!(demoted.status, StatusCode::OK, "{:?}", demoted.json);

    let banned = request_json(
        test_app(pool.clone()),
        "POST",
        "/api/v3/user/ban",
        Some(&token),
        Some(json!({
            "person_id": target_id,
            "ban": true,
            "reason": "compatibility test",
        })),
    )
    .await;
    assert_eq!(banned.status, StatusCode::OK, "{:?}", banned.json);
    assert_eq!(banned.json["banned"], true);
    assert!(
        plamenu_db::account::find_by_id(&pool, target_account.id)
            .await
            .unwrap()
            .unwrap()
            .suspended()
    );

    let unbanned = request_json(
        test_app(pool.clone()),
        "POST",
        "/api/v3/user/ban",
        Some(&token),
        Some(json!({ "person_id": target_id, "ban": false })),
    )
    .await;
    assert_eq!(unbanned.status, StatusCode::OK, "{:?}", unbanned.json);

    let post = status::create_local(
        &pool,
        status::NewLocalStatus {
            account_id: target_account.id,
            content: "to purge",
            text: "to purge",
            content_type: "text/plain",
            visibility: "public",
            in_reply_to_id: None,
            spoiler_text: "",
            sensitive: false,
            language: Some("en"),
            quote_approval_policy: None,
            title: Some("Purge me"),
            object_type: Some("Page"),
            external_url: None,
        },
    )
    .await
    .unwrap();
    let post_id = lemmy_id(&pool, Kind::Status, post.id).await;
    let purged = request_json(
        test_app(pool.clone()),
        "POST",
        "/api/v3/admin/purge/post",
        Some(&token),
        Some(json!({ "post_id": post_id, "reason": "test" })),
    )
    .await;
    assert_eq!(purged.status, StatusCode::OK, "{:?}", purged.json);
    assert_eq!(purged.json, json!({ "success": true }));
    assert!(status::find_by_id(&pool, post.id).await.unwrap().is_none());

    let app = oauth::find_app_by_client_id(&pool, "plamenu-lemmy-api-v3")
        .await
        .unwrap()
        .unwrap();
    assert!(
        oauth::find_active_token(&pool, &hash_secret(&token))
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(app.name, "Plamenu Lemmy API v3 compatibility");
    let actions = plamenu_db::admin_action_log::list(
        &pool,
        &plamenu_db::admin_action_log::LogFilter {
            account_id: Some(owner_account.id),
            limit: 20,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(actions.iter().any(|action| action.action == "purge"));
    assert!(actions.iter().any(|action| action.action == "promote"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn community_feed_reports_and_admin_lifecycle_use_real_state(pool: PgPool) {
    let (owner, _) = create_user(
        &pool,
        "community_owner",
        "community-owner@example.test",
        "owner password",
    )
    .await;
    let (reporter, _) = create_user(
        &pool,
        "reporter",
        "reporter@example.test",
        "reporter password",
    )
    .await;
    role::assign_to_account(&pool, owner.id, Some(3))
        .await
        .unwrap();
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let (group, _) = plamenu::groups::create_group(
        &state,
        plamenu::groups::CreateGroupParams {
            name: "compat",
            display_name: "Compatibility",
            membership_policy: plamenu_db::group::MembershipPolicy::Open,
            posting_policy: plamenu_db::group::PostingPolicy::Anyone,
            created_by: owner.id,
            enforce_username_blocklist: false,
            enforce_account_quota: false,
        },
    )
    .await
    .unwrap();
    let app = plamenu::build_router(state);
    let owner_token = login(&pool, "community_owner", "owner password").await;
    let reporter_token = login(&pool, "reporter", "reporter password").await;

    let bootstrap =
        request_json(app.clone(), "GET", "/api/v3/site", Some(&owner_token), None).await;
    assert_eq!(bootstrap.status, StatusCode::OK, "{:?}", bootstrap.json);
    let group_id = lemmy_id(&pool, Kind::Account, group.id).await;
    let owner_id = lemmy_id(&pool, Kind::Account, owner.id).await;
    assert_ne!(i64::from(group_id), group.id);
    assert!(
        bootstrap.json["my_user"]["moderates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|view| view["community"]["id"] == group_id)
    );
    let followed = request_json(
        app.clone(),
        "POST",
        "/api/v3/community/follow",
        Some(&reporter_token),
        Some(json!({ "community_id": group_id, "follow": true })),
    )
    .await;
    assert_eq!(followed.status, StatusCode::OK, "{:?}", followed.json);
    assert_eq!(followed.json["community_view"]["subscribed"], "Subscribed");

    let created = request_json(
        app.clone(),
        "POST",
        "/api/v3/post",
        Some(&owner_token),
        Some(json!({
            "community_id": group_id,
            "name": "A compatible post",
            "body": "Hello from the Lemmy adapter",
        })),
    )
    .await;
    assert_eq!(created.status, StatusCode::OK, "{:?}", created.json);
    let post_id = created.json["post_view"]["post"]["id"].as_i64().unwrap();
    assert!(post_id <= i64::from(i32::MAX));
    let native_post_id = native_id(&pool, Kind::Status, post_id).await;
    assert!(
        native_post_id > 9_007_199_254_740_991,
        "the regression must exercise an ID JavaScript cannot represent exactly"
    );
    assert_eq!(created.json["post_view"]["counts"]["post_id"], post_id);
    assert_eq!(created.json["post_view"]["community"]["id"], group_id);
    assert_eq!(created.json["post_view"]["creator"]["id"], owner_id);

    let detail = request_json(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v3/post?id={post_id}"),
        Some(&owner_token),
        None,
    )
    .await;
    assert_eq!(detail.status, StatusCode::OK, "{:?}", detail.json);
    assert_eq!(detail.json["post_view"]["post"]["id"], post_id);
    assert_eq!(detail.json["post_view"]["read"], false);

    let marked_read = request_json(
        app.clone(),
        "POST",
        "/api/v3/post/mark_as_read",
        Some(&reporter_token),
        Some(json!({ "post_ids": [post_id, post_id], "read": true })),
    )
    .await;
    assert_eq!(marked_read.status, StatusCode::OK, "{:?}", marked_read.json);
    assert_eq!(marked_read.json, json!({ "success": true }));
    assert!(
        plamenu_db::post_read::contains(&pool, reporter.id, native_post_id)
            .await
            .unwrap()
    );
    let reporter_detail = request_json(
        app.clone(),
        "GET",
        &format!("/api/v3/post?id={post_id}"),
        Some(&reporter_token),
        None,
    )
    .await;
    assert_eq!(reporter_detail.json["post_view"]["read"], true);

    let voted = request_json(
        app.clone(),
        "POST",
        "/api/v3/post/like",
        Some(&reporter_token),
        Some(json!({ "post_id": post_id, "score": 1 })),
    )
    .await;
    assert_eq!(voted.status, StatusCode::OK, "{:?}", voted.json);
    assert_eq!(voted.json["post_view"]["my_vote"], 1);
    let saved = request_json(
        app.clone(),
        "PUT",
        "/api/v3/post/save",
        Some(&reporter_token),
        Some(json!({ "post_id": post_id, "save": true })),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.json);
    assert_eq!(saved.json["post_view"]["saved"], true);

    let comment = request_json(
        app.clone(),
        "POST",
        "/api/v3/comment",
        Some(&reporter_token),
        Some(json!({ "post_id": post_id, "content": "An exact-ID reply" })),
    )
    .await;
    assert_eq!(comment.status, StatusCode::OK, "{:?}", comment.json);
    let comment_id = comment.json["comment_view"]["comment"]["id"]
        .as_i64()
        .unwrap();
    assert!(comment_id <= i64::from(i32::MAX));
    let native_comment_id = native_id(&pool, Kind::Status, comment_id).await;
    assert!(native_comment_id > 9_007_199_254_740_991);
    assert_ne!(comment_id, post_id);
    let path = comment.json["comment_view"]["comment"]["path"]
        .as_str()
        .unwrap();
    assert!(path.split('.').all(|part| part.parse::<i32>().is_ok()));
    assert!(path.ends_with(&comment_id.to_string()));
    let comment_is_not_a_post = request_json(
        app.clone(),
        "POST",
        "/api/v3/post/mark_as_read",
        Some(&reporter_token),
        Some(json!({ "post_ids": [comment_id], "read": true })),
    )
    .await;
    assert_eq!(comment_is_not_a_post.status, StatusCode::NOT_FOUND);
    assert_eq!(
        comment_is_not_a_post.json,
        json!({ "error": "couldnt_find_post" })
    );

    let unvoted = request_json(
        app.clone(),
        "POST",
        "/api/v3/post/like",
        Some(&reporter_token),
        Some(json!({ "post_id": post_id, "score": 0 })),
    )
    .await;
    assert_eq!(unvoted.status, StatusCode::OK, "{:?}", unvoted.json);
    let unsaved = request_json(
        app.clone(),
        "PUT",
        "/api/v3/post/save",
        Some(&reporter_token),
        Some(json!({ "post_id": post_id, "save": false })),
    )
    .await;
    assert_eq!(unsaved.status, StatusCode::OK, "{:?}", unsaved.json);

    let profile = request_json(
        app.clone(),
        "GET",
        &format!("/api/v3/user?person_id={owner_id}"),
        Some(&reporter_token),
        None,
    )
    .await;
    assert_eq!(profile.status, StatusCode::OK, "{:?}", profile.json);
    assert_eq!(profile.json["posts"][0]["post"]["id"], post_id);
    let qualified_profile = request_json(
        app.clone(),
        "GET",
        "/api/v3/user?username=community_owner%40plamenu.test",
        Some(&reporter_token),
        None,
    )
    .await;
    assert_eq!(
        qualified_profile.status,
        StatusCode::OK,
        "{:?}",
        qualified_profile.json
    );
    assert_eq!(
        qualified_profile.json["person_view"]["person"]["id"],
        owner_id
    );
    let wrong_domain = request_json(
        app.clone(),
        "GET",
        "/api/v3/user?username=community_owner%40elsewhere.test",
        Some(&reporter_token),
        None,
    )
    .await;
    assert_eq!(wrong_domain.status, StatusCode::NOT_FOUND);
    let resolved_object = request_json(
        app.clone(),
        "GET",
        "/api/v3/resolve_object?q=!compat",
        Some(&reporter_token),
        None,
    )
    .await;
    assert_eq!(
        resolved_object.status,
        StatusCode::OK,
        "{:?}",
        resolved_object.json
    );
    assert_eq!(
        resolved_object.json["community"]["community"]["id"],
        group_id
    );
    let search = request_json(
        app.clone(),
        "GET",
        "/api/v3/search?q=compat&type_=Communities",
        Some(&reporter_token),
        None,
    )
    .await;
    assert_eq!(search.status, StatusCode::OK, "{:?}", search.json);
    assert_eq!(search.json["communities"][0]["community"]["id"], group_id);

    let feed = request_json(
        app.clone(),
        "GET",
        &format!("/api/v3/post/list?community_id={group_id}"),
        Some(&reporter_token),
        None,
    )
    .await;
    assert_eq!(feed.status, StatusCode::OK, "{:?}", feed.json);
    assert_eq!(feed.json["posts"][0]["post"]["id"], post_id);
    assert_eq!(feed.json["posts"][0]["read"], true);

    let marked_unread = request_json(
        app.clone(),
        "POST",
        "/api/v3/post/mark_as_read",
        Some(&reporter_token),
        Some(json!({ "post_ids": [post_id], "read": false })),
    )
    .await;
    assert_eq!(marked_unread.json, json!({ "success": true }));
    let unread_detail = request_json(
        app.clone(),
        "GET",
        &format!("/api/v3/post?id={post_id}"),
        Some(&reporter_token),
        None,
    )
    .await;
    assert_eq!(unread_detail.json["post_view"]["read"], false);

    let filed = request_json(
        app.clone(),
        "POST",
        "/api/v3/post/report",
        Some(&reporter_token),
        Some(json!({ "post_id": post_id, "reason": "review this" })),
    )
    .await;
    assert_eq!(filed.status, StatusCode::OK, "{:?}", filed.json);
    let report_id = filed.json["post_report_view"]["post_report"]["id"]
        .as_i64()
        .unwrap();
    let native_report_id = native_id(&pool, Kind::Report, report_id).await;
    assert_eq!(
        plamenu_db::report::find_by_id(&pool, native_report_id)
            .await
            .unwrap()
            .unwrap()
            .group_account_id,
        Some(group.id)
    );

    let queue = request_json(
        app.clone(),
        "GET",
        &format!("/api/v3/post/report/list?community_id={group_id}"),
        Some(&owner_token),
        None,
    )
    .await;
    assert_eq!(queue.status, StatusCode::OK, "{:?}", queue.json);
    assert_eq!(
        queue.json["post_reports"][0]["post_report"]["id"],
        report_id
    );
    let resolved = request_json(
        app.clone(),
        "PUT",
        "/api/v3/post/report/resolve",
        Some(&owner_token),
        Some(json!({ "report_id": report_id, "resolved": true })),
    )
    .await;
    assert_eq!(resolved.status, StatusCode::OK, "{:?}", resolved.json);
    assert_eq!(
        resolved.json["post_report_view"]["post_report"]["resolved"],
        true
    );

    let hidden = request_json(
        app,
        "PUT",
        "/api/v3/community/hide",
        Some(&owner_token),
        Some(json!({ "community_id": group_id, "hidden": true })),
    )
    .await;
    assert_eq!(hidden.status, StatusCode::OK, "{:?}", hidden.json);
    assert_eq!(hidden.json, json!({ "success": true }));
    assert_eq!(
        plamenu_db::account::find_by_id(&pool, group.id)
            .await
            .unwrap()
            .unwrap()
            .discoverable,
        Some(false)
    );

    let blocked = request_json(
        test_app(pool.clone()),
        "POST",
        "/api/v3/community/block",
        Some(&reporter_token),
        Some(json!({ "community_id": group_id, "block": true })),
    )
    .await;
    assert_eq!(blocked.status, StatusCode::OK, "{:?}", blocked.json);
    assert_eq!(blocked.json["blocked"], true);
    assert!(
        plamenu_db::block::exists(&pool, reporter.id, group.id)
            .await
            .unwrap()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn unread_counts_project_notifications_and_direct_conversations(pool: PgPool) {
    let (alice, alice_user) = create_user(
        &pool,
        "unread_alice",
        "alice@example.test",
        "alice password",
    )
    .await;
    let (bob, _) = create_user(&pool, "unread_bob", "bob@example.test", "bob password").await;
    let token = login(&pool, "unread_alice", "alice password").await;

    let parent = status::create_local(
        &pool,
        status::NewLocalStatus::new(alice.id, "parent", "public", None),
    )
    .await
    .unwrap();
    let reply = status::create_local(
        &pool,
        status::NewLocalStatus::new(bob.id, "reply", "public", Some(parent.id)),
    )
    .await
    .unwrap();
    let mention = status::create_local(
        &pool,
        status::NewLocalStatus::new(bob.id, "mention", "public", None),
    )
    .await
    .unwrap();
    plamenu_db::notification::create(&pool, alice.id, bob.id, "mention", Some(reply.id))
        .await
        .unwrap();
    plamenu_db::notification::create(&pool, alice.id, bob.id, "mention", Some(mention.id))
        .await
        .unwrap();

    let direct = status::create_local(
        &pool,
        status::NewLocalStatus::new(bob.id, "private", "direct", None),
    )
    .await
    .unwrap();
    let conversation_id = plamenu_db::conversation::ensure_for_status(
        &pool,
        &plamenu_db::conversation::EnsureConversation {
            status_id: direct.id,
            account_id: bob.id,
            in_reply_to_id: None,
            is_reply: false,
            refs: plamenu_db::conversation::ContextRefs::default(),
        },
    )
    .await
    .unwrap();
    plamenu_db::conversation::add_status(
        &pool,
        plamenu_db::conversation::AddStatus {
            account_id: alice.id,
            conversation_id,
            participant_account_ids: &[bob.id],
            status_id: direct.id,
            sender_id: bob.id,
        },
    )
    .await
    .unwrap();

    let unread = request_json(
        test_app(pool.clone()),
        "GET",
        "/api/v3/user/unread_count",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(unread.status, StatusCode::OK, "{:?}", unread.json);
    assert_eq!(
        unread.json,
        json!({ "replies": 1, "mentions": 1, "private_messages": 1 })
    );

    let newest_notification =
        sqlx::query_scalar::<_, i64>("SELECT max(id) FROM notifications WHERE account_id = $1")
            .bind(alice.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    plamenu_db::marker::advance(&pool, alice_user.id, "notifications", newest_notification)
        .await
        .unwrap();
    plamenu_db::conversation::mark_read_conversation(&pool, alice.id, conversation_id)
        .await
        .unwrap();
    let cleared = request_json(
        test_app(pool),
        "GET",
        "/api/v3/user/unread_count",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(
        cleared.json,
        json!({ "replies": 0, "mentions": 0, "private_messages": 0 })
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn community_post_comment_and_account_mutations_round_trip(pool: PgPool) {
    let settings = plamenu_db::instance_settings::get(&pool).await.unwrap();
    plamenu_db::instance_settings::save(
        &pool,
        plamenu_db::instance_settings::SettingsUpdate {
            registrations_mode: plamenu_db::instance_settings::RegistrationsMode::Open,
            ..settings.as_update()
        },
    )
    .await
    .unwrap();
    let registered = request_json(
        test_app(pool.clone()),
        "POST",
        "/api/v3/user/register",
        None,
        Some(json!({
            "username": "compat_writer",
            "email": "writer@example.test",
            "password": "initial password",
            "password_verify": "initial password",
            "honeypot": "",
        })),
    )
    .await;
    assert_eq!(registered.status, StatusCode::OK, "{:?}", registered.json);
    let token = registered.json["jwt"]
        .as_str()
        .expect("open registration logs in");
    let app = test_app(pool.clone());

    let created_group = request_json(
        app.clone(),
        "POST",
        "/api/v3/community",
        Some(token),
        Some(json!({
            "name": "adapter_tests",
            "title": "Adapter tests",
            "description": "Initial description",
            "posting_restricted_to_mods": false,
        })),
    )
    .await;
    assert_eq!(
        created_group.status,
        StatusCode::OK,
        "{:?}",
        created_group.json
    );
    let community_id = created_group.json["community_view"]["community"]["id"]
        .as_i64()
        .unwrap();
    let qualified_group = request_json(
        app.clone(),
        "GET",
        "/api/v3/community?name=adapter_tests%40plamenu.test",
        Some(token),
        None,
    )
    .await;
    assert_eq!(
        qualified_group.status,
        StatusCode::OK,
        "{:?}",
        qualified_group.json
    );
    assert_eq!(
        qualified_group.json["community_view"]["community"]["id"],
        community_id
    );
    let edited_group = request_json(
        app.clone(),
        "PUT",
        "/api/v3/community",
        Some(token),
        Some(json!({
            "community_id": community_id,
            "title": "Edited adapter tests",
            "description": "Edited description",
            "nsfw": true,
            "posting_restricted_to_mods": true,
        })),
    )
    .await;
    assert_eq!(
        edited_group.status,
        StatusCode::OK,
        "{:?}",
        edited_group.json
    );
    assert_eq!(
        edited_group.json["community_view"]["community"]["title"],
        "Edited adapter tests"
    );
    assert_eq!(
        edited_group.json["community_view"]["community"]["description"],
        "Edited description"
    );

    let post = request_json(
        app.clone(),
        "POST",
        "/api/v3/post",
        Some(token),
        Some(json!({ "community_id": community_id, "name": "Before", "body": "old body" })),
    )
    .await;
    assert_eq!(post.status, StatusCode::OK, "{:?}", post.json);
    let post_id = post.json["post_view"]["post"]["id"].as_i64().unwrap();
    let edited_post = request_json(
        app.clone(),
        "PUT",
        "/api/v3/post",
        Some(token),
        Some(json!({ "post_id": post_id, "name": "After", "body": "new body", "nsfw": true })),
    )
    .await;
    assert_eq!(edited_post.status, StatusCode::OK, "{:?}", edited_post.json);
    assert_eq!(edited_post.json["post_view"]["post"]["name"], "After");
    assert_eq!(edited_post.json["post_view"]["post"]["body"], "new body");

    let comment = request_json(
        app.clone(),
        "POST",
        "/api/v3/comment",
        Some(token),
        Some(json!({ "post_id": post_id, "content": "before comment" })),
    )
    .await;
    let comment_id = comment.json["comment_view"]["comment"]["id"]
        .as_i64()
        .unwrap();
    let edited_comment = request_json(
        app.clone(),
        "PUT",
        "/api/v3/comment",
        Some(token),
        Some(json!({ "comment_id": comment_id, "content": "after comment" })),
    )
    .await;
    assert_eq!(
        edited_comment.status,
        StatusCode::OK,
        "{:?}",
        edited_comment.json
    );
    assert_eq!(
        edited_comment.json["comment_view"]["comment"]["content"],
        "after comment"
    );

    let settings = request_json(
        app.clone(),
        "PUT",
        "/api/v3/user/save_user_settings",
        Some(token),
        Some(json!({
            "display_name": "Compatibility Writer",
            "bio": "editable **bio**",
            "bot_account": true,
            "interface_language": "en",
            "discussion_languages": [1],
            "show_nsfw": false,
            "theme": "browser",
            "default_sort_type": "New",
        })),
    )
    .await;
    assert_eq!(settings.json, json!({ "success": true }));
    let settings_round_trip =
        request_json(app.clone(), "GET", "/api/v3/site", Some(token), None).await;
    let local = &settings_round_trip.json["my_user"]["local_user_view"]["local_user"];
    assert_eq!(local["show_nsfw"], false);
    assert_eq!(local["theme"], "browser");
    assert_eq!(local["default_sort_type"], "New");

    let changed = request_json(
        app.clone(),
        "PUT",
        "/api/v3/user/change_password",
        Some(token),
        Some(json!({
            "old_password": "initial password",
            "new_password": "replacement password",
            "new_password_verify": "replacement password",
        })),
    )
    .await;
    assert_eq!(changed.status, StatusCode::OK, "{:?}", changed.json);
    assert!(changed.json["jwt"].as_str().is_some());
    let stale = request_json(app, "GET", "/api/v3/user/validate_auth", Some(token), None).await;
    assert_eq!(stale.status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbox_read_overrides_and_private_messages_are_durable(pool: PgPool) {
    let (alice, alice_user) = create_user(
        &pool,
        "inbox_alice",
        "inbox-a@example.test",
        "alice password",
    )
    .await;
    let (bob, _) = create_user(&pool, "inbox_bob", "inbox-b@example.test", "bob password").await;
    let alice_token = login(&pool, "inbox_alice", "alice password").await;
    let bob_token = login(&pool, "inbox_bob", "bob password").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let (group, _) = plamenu::groups::create_group(
        &state,
        plamenu::groups::CreateGroupParams {
            name: "inbox_group",
            display_name: "Inbox group",
            membership_policy: plamenu_db::group::MembershipPolicy::Open,
            posting_policy: plamenu_db::group::PostingPolicy::Anyone,
            created_by: alice.id,
            enforce_username_blocklist: false,
            enforce_account_quota: false,
        },
    )
    .await
    .unwrap();
    let root = status::create_local(
        &pool,
        status::NewLocalStatus::new(alice.id, "root", "public", None),
    )
    .await
    .unwrap();
    plamenu_db::mention::attach(&pool, root.id, group.id, true)
        .await
        .unwrap();
    let reply = status::create_local(
        &pool,
        status::NewLocalStatus::new(bob.id, "reply", "public", Some(root.id)),
    )
    .await
    .unwrap();
    plamenu_db::notification::create(&pool, alice.id, bob.id, "mention", Some(reply.id))
        .await
        .unwrap();

    let app = plamenu::build_router(state);
    let replies = request_json(
        app.clone(),
        "GET",
        "/api/v3/user/replies",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(replies.status, StatusCode::OK, "{:?}", replies.json);
    assert_eq!(replies.json["replies"].as_array().unwrap().len(), 1);
    let reply_id = replies.json["replies"][0]["comment_reply"]["id"]
        .as_i64()
        .unwrap();
    assert_eq!(replies.json["replies"][0]["comment_reply"]["read"], false);
    let marked = request_json(
        app.clone(),
        "POST",
        "/api/v3/comment_reply/mark_as_read",
        Some(&alice_token),
        Some(json!({ "comment_reply_id": reply_id, "read": true })),
    )
    .await;
    assert_eq!(marked.status, StatusCode::OK, "{:?}", marked.json);
    assert_eq!(
        marked.json["comment_reply_view"]["comment_reply"]["read"],
        true
    );
    let unread = request_json(
        app.clone(),
        "GET",
        "/api/v3/user/unread_count",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(unread.json["replies"], 0);
    assert!(
        plamenu_db::marker::find(&pool, alice_user.id, "notifications")
            .await
            .unwrap()
            .is_none()
    );

    // A later reply whose thread was never associated with a community models
    // a notification left behind after its Lemmy-projectable context vanished.
    // It must be skipped without hiding the valid reply above.
    let orphan_root = status::create_local(
        &pool,
        status::NewLocalStatus::new(alice.id, "orphan root", "public", None),
    )
    .await
    .unwrap();
    let orphan_reply = status::create_local(
        &pool,
        status::NewLocalStatus::new(bob.id, "orphan reply", "public", Some(orphan_root.id)),
    )
    .await
    .unwrap();
    plamenu_db::notification::create(&pool, alice.id, bob.id, "mention", Some(orphan_reply.id))
        .await
        .unwrap();
    let replies_with_orphan = request_json(
        app.clone(),
        "GET",
        "/api/v3/user/replies",
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(
        replies_with_orphan.status,
        StatusCode::OK,
        "{:?}",
        replies_with_orphan.json
    );
    assert_eq!(
        replies_with_orphan.json["replies"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let bob_id = lemmy_id(&pool, Kind::Account, bob.id).await;
    let sent = request_json(
        app.clone(),
        "POST",
        "/api/v3/private_message",
        Some(&alice_token),
        Some(json!({ "recipient_id": bob_id, "content": "secret hello" })),
    )
    .await;
    assert_eq!(sent.status, StatusCode::OK, "{:?}", sent.json);
    let message_id = sent.json["private_message_view"]["private_message"]["id"]
        .as_i64()
        .unwrap();
    let listed = request_json(
        app.clone(),
        "GET",
        "/api/v3/private_message/list?unread_only=true",
        Some(&bob_token),
        None,
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "{:?}", listed.json);
    assert_eq!(
        listed.json["private_messages"][0]["private_message"]["content"],
        "secret hello"
    );
    assert_eq!(
        listed.json["private_messages"][0]["private_message"]["id"],
        message_id
    );
    let read = request_json(
        app,
        "POST",
        "/api/v3/private_message/mark_as_read",
        Some(&bob_token),
        Some(json!({ "private_message_id": message_id, "read": true })),
    )
    .await;
    assert_eq!(read.status, StatusCode::OK, "{:?}", read.json);
    assert_eq!(
        read.json["private_message_view"]["private_message"]["read"],
        true
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn pictrs_media_and_custom_emoji_mutations_round_trip(pool: PgPool) {
    let (owner, _) = create_user(
        &pool,
        "media_owner",
        "media-owner@example.test",
        "owner password",
    )
    .await;
    role::assign_to_account(&pool, owner.id, Some(3))
        .await
        .unwrap();
    let token = login(&pool, "media_owner", "owner password").await;
    let state = test_state_with(pool.clone(), StubFederation::with_actors([]));
    let app = plamenu::build_router(state);
    let uploaded = upload_pictrs(app.clone(), &token, &sample_png()).await;
    assert_eq!(uploaded.status, StatusCode::OK, "{:?}", uploaded.json);
    let alias = uploaded.json["files"][0]["file"].as_str().unwrap();
    let delete_token = uploaded.json["files"][0]["delete_token"].as_str().unwrap();
    let listed = request_json(
        app.clone(),
        "GET",
        "/api/v3/account/list_media",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(
        listed.json["images"][0]["local_image"]["pictrs_alias"],
        alias
    );

    let created = request_json(
        app.clone(),
        "POST",
        "/api/v3/custom_emoji",
        Some(&token),
        Some(json!({
            "category": "Reactions",
            "shortcode": "compat_wave",
            "image_url": format!("https://plamenu.test/pictrs/image/{alias}"),
            "alt_text": "a friendly wave",
            "keywords": ["hello", "wave"],
        })),
    )
    .await;
    assert_eq!(created.status, StatusCode::OK, "{:?}", created.json);
    let emoji_id = created.json["custom_emoji"]["custom_emoji"]["id"]
        .as_i64()
        .unwrap();
    assert_eq!(
        created.json["custom_emoji"]["custom_emoji"]["alt_text"],
        "a friendly wave"
    );
    assert_eq!(
        created.json["custom_emoji"]["keywords"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let site = request_json(app.clone(), "GET", "/api/v3/site", Some(&token), None).await;
    assert!(
        site.json["custom_emojis"]
            .as_array()
            .unwrap()
            .iter()
            .any(|view| {
                view["custom_emoji"]["id"] == emoji_id
                    && view["custom_emoji"]["alt_text"] == "a friendly wave"
            })
    );
    let deleted_emoji = request_json(
        app.clone(),
        "POST",
        "/api/v3/custom_emoji/delete",
        Some(&token),
        Some(json!({ "id": emoji_id })),
    )
    .await;
    assert_eq!(deleted_emoji.json, json!({ "success": true }));
    let deleted_image = request_json(
        app,
        "GET",
        &format!("/pictrs/image/delete/{delete_token}/{alias}"),
        None,
        None,
    )
    .await;
    assert_eq!(deleted_image.status, StatusCode::NO_CONTENT);
}
