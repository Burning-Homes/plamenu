//! User lists: CRUD, membership management, the list timeline and the
//! exclusive-list home filtering — `/api/v1/lists*`,
//! `/api/v1/timelines/list/{id}` and `/api/v1/accounts/{id}/lists`.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use common::{create_local_account, test_app};
use http_body_util::BodyExt;
use plamenu::auth::{generate_secret, hash_password, hash_secret};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use plamenu_db::account::Account;
use plamenu_db::{PgPool, follow, oauth, user};
use serde_json::{Value, json};
use tower::ServiceExt;
use tracing_subscriber::prelude::*;

/// A local account with login credentials and an access token, minted
/// directly (the OAuth issuance flow is covered in `client_api.rs`).
async fn user_with_token(pool: &PgPool, username: &str) -> (Account, String) {
    let account = create_local_account(pool, username, username).await;
    let hash = hash_password("pw").unwrap();
    let row = user::create(
        pool,
        account.id,
        Some(&format!("{username}@plamenu.test")),
        &hash,
    )
    .await
    .unwrap();
    let app = oauth::create_app(
        pool,
        oauth::NewApp {
            name: "lists-tests",
            website: None,
            client_id: &generate_secret(),
            client_secret_hash: &hash_secret(&generate_secret()),
            redirect_uris: &["urn:ietf:wg:oauth:2.0:oob".to_owned()],
            scopes: "read write follow",
        },
    )
    .await
    .unwrap();
    let token = generate_secret();
    oauth::create_token(
        pool,
        &hash_secret(&token),
        app.id,
        Some(row.id),
        "read write",
    )
    .await
    .unwrap();
    (account, token)
}

async fn api(
    app: Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let (status, _, value) = api_with_headers(app, method, uri, bearer, body).await;
    (status, value)
}

async fn api_with_headers(
    app: Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let request = match body {
        Some(value) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&value).unwrap()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, value)
}

/// A form-encoded API call — Mastodon clients send both encodings.
async fn api_form(
    app: Router,
    method: &str,
    uri: &str,
    bearer: &str,
    fields: &[(&str, &str)],
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(serde_urlencoded::to_string(fields).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

async fn post_status(pool: &PgPool, token: &str, body: Value) -> Value {
    let (status, entity) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(token),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{entity}");
    entity
}

fn ids_of(page: &Value) -> Vec<&str> {
    page.as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn list_crud_lifecycle(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let (_bob, bob_token) = user_with_token(&pool, "bob").await;

    // Anonymous access is refused.
    let (status, _) = api(test_app(pool.clone()), "GET", "/api/v1/lists", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Create with defaults: replies_policy "list", exclusive false.
    let (status, created) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/lists",
        Some(&token),
        Some(json!({"title": "Friends"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    assert_eq!(created["title"], "Friends");
    assert_eq!(created["replies_policy"], "list");
    assert_eq!(created["exclusive"], false);
    assert!(created["id"].is_string(), "entity ids are strings");
    let list_id = created["id"].as_str().unwrap().to_owned();

    // Form bodies work too.
    let (status, second) = api_form(
        test_app(pool.clone()),
        "POST",
        "/api/v1/lists",
        &token,
        &[
            ("title", "Work"),
            ("replies_policy", "followed"),
            ("exclusive", "true"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(second["replies_policy"], "followed");
    assert_eq!(second["exclusive"], true);

    let (status, index) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/lists",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(index.as_array().unwrap().len(), 2);

    // Lists are scoped to their owner.
    let (status, _) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/lists/{list_id}"),
        Some(&bob_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, index) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/lists",
        Some(&bob_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(index, json!([]));

    // Update merges absent attributes.
    let (status, updated) = api(
        test_app(pool.clone()),
        "PUT",
        &format!("/api/v1/lists/{list_id}"),
        Some(&token),
        Some(json!({"replies_policy": "none"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["title"], "Friends", "absent title kept");
    assert_eq!(updated["replies_policy"], "none");

    let (status, body) = api(
        test_app(pool.clone()),
        "DELETE",
        &format!("/api/v1/lists/{list_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({}));
    let (status, _) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/lists/{list_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn list_validations_match_mastodons_wording(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;

    let cases = [
        (json!({}), "Validation failed: Title can't be blank"),
        (
            json!({"title": "   "}),
            "Validation failed: Title can't be blank",
        ),
        (
            json!({"title": "x".repeat(257)}),
            "Validation failed: Title is too long (maximum is 256 characters)",
        ),
        (
            json!({"title": "ok", "replies_policy": "everyone"}),
            "Validation failed: Replies policy is not included in the list",
        ),
    ];
    for (body, message) in cases {
        let (status, error) = api(
            test_app(pool.clone()),
            "POST",
            "/api/v1/lists",
            Some(&token),
            Some(body),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(error["error"], message);
    }

    // The same validations guard updates.
    let (_, created) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/lists",
        Some(&token),
        Some(json!({"title": "ok"})),
    )
    .await;
    let list_id = created["id"].as_str().unwrap();
    let (status, error) = api(
        test_app(pool.clone()),
        "PUT",
        &format!("/api/v1/lists/{list_id}"),
        Some(&token),
        Some(json!({"title": ""})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error["error"], "Validation failed: Title can't be blank");

    // 50 lists per account, like Mastodon's PER_ACCOUNT_LIMIT.
    for n in 1..50 {
        let (status, body) = api(
            test_app(pool.clone()),
            "POST",
            "/api/v1/lists",
            Some(&token),
            Some(json!({"title": format!("list {n}")})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (status, error) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/lists",
        Some(&token),
        Some(json!({"title": "one too many"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        error["error"],
        "Validation failed: You have reached the maximum number of lists"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn list_membership_requires_a_follow(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let (bob, _) = user_with_token(&pool, "bob").await;
    let (carol, _) = user_with_token(&pool, "carol").await;

    let (_, created) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/lists",
        Some(&token),
        Some(json!({"title": "Friends"})),
    )
    .await;
    let list_id = created["id"].as_str().unwrap().to_owned();
    let accounts_uri = format!("/api/v1/lists/{list_id}/accounts");

    // Unknown account ids 404 before anything is validated.
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        &accounts_uri,
        Some(&token),
        Some(json!({"account_ids": ["1"]})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Not followed → Mastodon's must_be_following message.
    let (status, error) = api(
        test_app(pool.clone()),
        "POST",
        &accounts_uri,
        Some(&token),
        Some(json!({"account_ids": [bob.id.to_string()]})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        error["error"],
        "Validation failed: Account must be a followed account"
    );

    // A failing batch is atomic: carol (followed) must not slip in.
    follow::create(&pool, alice.id, carol.id, None)
        .await
        .unwrap();
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        &accounts_uri,
        Some(&token),
        Some(json!({"account_ids": [carol.id.to_string(), bob.id.to_string()]})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (_, members) = api(
        test_app(pool.clone()),
        "GET",
        &accounts_uri,
        Some(&token),
        None,
    )
    .await;
    assert_eq!(members, json!([]), "failed batch left no members");

    // Following bob fixes it; the owner may list themself without a follow.
    follow::create(&pool, alice.id, bob.id, None).await.unwrap();
    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        &accounts_uri,
        Some(&token),
        Some(json!({"account_ids": [bob.id.to_string(), alice.id.to_string()]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({}));

    // Re-adding is Mastodon's uniqueness 422.
    let (status, error) = api(
        test_app(pool.clone()),
        "POST",
        &accounts_uri,
        Some(&token),
        Some(json!({"account_ids": [bob.id.to_string()]})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        error["error"],
        "Validation failed: Account has already been taken"
    );

    // Form-encoded account_ids[] work too (for the removal).
    let (status, body) = api_form(
        test_app(pool.clone()),
        "DELETE",
        &accounts_uri,
        &token,
        &[("account_ids[]", alice.id.to_string().as_str())],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (_, members) = api(
        test_app(pool.clone()),
        "GET",
        &accounts_uri,
        Some(&token),
        None,
    )
    .await;
    assert_eq!(ids_of(&members), [bob.id.to_string()]);

    // /accounts/{id}/lists shows the membership to the owner.
    let (status, lists) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/accounts/{}/lists", bob.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(lists[0]["id"], list_id);

    // Unfollowing bob drops the membership (the follow cascade).
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/accounts/{}/unfollow", bob.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, members) = api(
        test_app(pool.clone()),
        "GET",
        &accounts_uri,
        Some(&token),
        None,
    )
    .await;
    assert_eq!(members, json!([]));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn list_accounts_paginate_by_account_id(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let mut member_ids = Vec::new();
    for n in 0..3 {
        let member = create_local_account(&pool, &format!("member{n}"), "m").await;
        follow::create(&pool, alice.id, member.id, None)
            .await
            .unwrap();
        member_ids.push(member.id);
    }
    let (_, created) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/lists",
        Some(&token),
        Some(json!({"title": "Friends"})),
    )
    .await;
    let list_id = created["id"].as_str().unwrap().to_owned();
    let ids: Vec<String> = member_ids.iter().map(ToString::to_string).collect();
    api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/lists/{list_id}/accounts"),
        Some(&token),
        Some(json!({"account_ids": ids})),
    )
    .await;

    // Newest account id first, Link next/prev like Mastodon.
    let (status, headers, page) = api_with_headers(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/lists/{list_id}/accounts?limit=2"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        ids_of(&page),
        [member_ids[2].to_string(), member_ids[1].to_string()]
    );
    let link = headers[header::LINK].to_str().unwrap();
    assert!(
        link.contains(&format!("max_id={}", member_ids[1])),
        "{link}"
    );
    assert!(link.contains("rel=\"next\""), "{link}");

    let (_, _, next) = api_with_headers(
        test_app(pool.clone()),
        "GET",
        &format!(
            "/api/v1/lists/{list_id}/accounts?limit=2&max_id={}",
            member_ids[1]
        ),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(ids_of(&next), [member_ids[0].to_string()]);

    // limit=0 lists everyone (under the cap) with no pagination headers.
    let (status, headers, all) = api_with_headers(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/lists/{list_id}/accounts?limit=0"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Everyone, still newest-account-first — the unbounded path must preserve
    // the query order (the O(n) rank map replaced a quadratic position scan).
    assert_eq!(
        ids_of(&all),
        [
            member_ids[2].to_string(),
            member_ids[1].to_string(),
            member_ids[0].to_string()
        ]
    );
    assert!(!headers.contains_key(header::LINK));
}

/// `limit=0` ("whole list") is bounded. A list larger than the
/// cap returns exactly the cap (newest account first) plus a `next` Link,
/// instead of materializing and rendering an unbounded membership set.
#[sqlx::test(migrations = "../db/migrations")]
async fn list_accounts_limit_zero_is_capped_and_paginates(pool: PgPool) {
    // Mirrors `FULL_LIST_LIMIT` in `routes::lists`.
    const CAP: usize = 500;
    let (owner, token) = user_with_token(&pool, "owner").await;
    let list = plamenu_db::list::create(&pool, owner.id, "big", "list", false)
        .await
        .unwrap();

    // Seed CAP + 3 members with dummy keys (cheap, no RSA) and insert the
    // membership rows directly, so the response overflows the cap. Ids climb
    // with creation order, so the last-seeded members are the "newest".
    let mut member_ids = Vec::new();
    for i in 0..CAP + 3 {
        let member = plamenu_db::account::create_local(
            &pool,
            plamenu_db::account::NewLocalAccount {
                username: &format!("m{i}"),
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap();
        sqlx::query!(
            "INSERT INTO list_accounts (id, list_id, account_id) VALUES ($1, $2, $3)",
            plamenu_db::id::next(),
            list.id,
            member.id,
        )
        .execute(&pool)
        .await
        .unwrap();
        member_ids.push(member.id);
    }

    let (status, headers, page) = api_with_headers(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/lists/{}/accounts?limit=0", list.id),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Exactly the cap, not the whole (CAP + 3) membership.
    let returned = ids_of(&page);
    assert_eq!(returned.len(), CAP, "limit=0 must be capped");
    // Newest account first: the highest id (last seeded) leads.
    let newest = member_ids.iter().max().unwrap();
    assert_eq!(returned[0], newest.to_string());
    // A `next` Link lets the client page past the cap with ordinary keyset
    // pagination.
    let link = headers[header::LINK].to_str().unwrap();
    assert!(link.contains("rel=\"next\""), "{link}");
    assert!(link.contains("max_id="), "{link}");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn list_timeline_and_exclusive_home_filtering(pool: PgPool) {
    let (alice, alice_token) = user_with_token(&pool, "alice").await;
    let (bob, bob_token) = user_with_token(&pool, "bob").await;
    let (carol, carol_token) = user_with_token(&pool, "carol").await;
    follow::create(&pool, alice.id, bob.id, None).await.unwrap();
    follow::create(&pool, alice.id, carol.id, None)
        .await
        .unwrap();

    let (_, created) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/lists",
        Some(&alice_token),
        Some(json!({"title": "Friends"})),
    )
    .await;
    let list_id = created["id"].as_str().unwrap().to_owned();
    api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/lists/{list_id}/accounts"),
        Some(&alice_token),
        Some(json!({"account_ids": [bob.id.to_string()]})),
    )
    .await;

    let bob_post = post_status(&pool, &bob_token, json!({"status": "from bob"})).await;
    let carol_post = post_status(&pool, &carol_token, json!({"status": "from carol"})).await;

    // The timeline carries members only; the list must be the caller's own.
    let timeline_uri = format!("/api/v1/timelines/list/{list_id}");
    let (status, page) = api(
        test_app(pool.clone()),
        "GET",
        &timeline_uri,
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(ids_of(&page), [bob_post["id"].as_str().unwrap()]);
    let (status, _) = api(
        test_app(pool.clone()),
        "GET",
        &timeline_uri,
        Some(&bob_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Both followed authors are on home while the list is not exclusive.
    let (_, home) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/timelines/home",
        Some(&alice_token),
        None,
    )
    .await;
    let home_ids = ids_of(&home);
    assert!(home_ids.contains(&bob_post["id"].as_str().unwrap()));
    assert!(home_ids.contains(&carol_post["id"].as_str().unwrap()));

    // Flipping the list exclusive pulls its members out of home — their
    // posts live on the list timeline instead.
    api(
        test_app(pool.clone()),
        "PUT",
        &format!("/api/v1/lists/{list_id}"),
        Some(&alice_token),
        Some(json!({"exclusive": true})),
    )
    .await;
    let (_, home) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/timelines/home",
        Some(&alice_token),
        None,
    )
    .await;
    let home_ids = ids_of(&home);
    assert!(!home_ids.contains(&bob_post["id"].as_str().unwrap()));
    assert!(home_ids.contains(&carol_post["id"].as_str().unwrap()));
    let (_, page) = api(
        test_app(pool.clone()),
        "GET",
        &timeline_uri,
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(ids_of(&page), [bob_post["id"].as_str().unwrap()]);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn list_timeline_applies_replies_policy(pool: PgPool) {
    let (alice, alice_token) = user_with_token(&pool, "alice").await;
    let (bob, bob_token) = user_with_token(&pool, "bob").await;
    let (carol, carol_token) = user_with_token(&pool, "carol").await;
    follow::create(&pool, alice.id, bob.id, None).await.unwrap();
    follow::create(&pool, alice.id, carol.id, None)
        .await
        .unwrap();

    let (_, created) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/lists",
        Some(&alice_token),
        Some(json!({"title": "Friends", "replies_policy": "none"})),
    )
    .await;
    let list_id = created["id"].as_str().unwrap().to_owned();
    api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v1/lists/{list_id}/accounts"),
        Some(&alice_token),
        Some(json!({"account_ids": [bob.id.to_string()]})),
    )
    .await;

    let carol_post = post_status(&pool, &carol_token, json!({"status": "hello"})).await;
    let reply = post_status(
        &pool,
        &bob_token,
        json!({"status": "a reply", "in_reply_to_id": carol_post["id"]}),
    )
    .await;

    let timeline_uri = format!("/api/v1/timelines/list/{list_id}");
    let (_, page) = api(
        test_app(pool.clone()),
        "GET",
        &timeline_uri,
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(page, json!([]), "policy none hides replies to others");

    // policy "followed" admits it — alice follows carol.
    api(
        test_app(pool.clone()),
        "PUT",
        &format!("/api/v1/lists/{list_id}"),
        Some(&alice_token),
        Some(json!({"replies_policy": "followed"})),
    )
    .await;
    let (_, page) = api(
        test_app(pool.clone()),
        "GET",
        &timeline_uri,
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(ids_of(&page), [reply["id"].as_str().unwrap()]);
}

/// A duplicate `account_ids` array is deduplicated, so repeating
/// one valid followed account in a single request succeeds (adding it once)
/// rather than failing its own uniqueness check on the second copy.
#[sqlx::test(migrations = "../db/migrations")]
async fn list_add_deduplicates_account_ids(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let (bob, _) = user_with_token(&pool, "bob").await;
    follow::create(&pool, alice.id, bob.id, None).await.unwrap();

    let (_, created) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/lists",
        Some(&token),
        Some(json!({ "title": "Friends" })),
    )
    .await;
    let list_id = created["id"].as_str().unwrap().to_owned();
    let accounts_uri = format!("/api/v1/lists/{list_id}/accounts");

    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        &accounts_uri,
        Some(&token),
        Some(json!({ "account_ids": [bob.id.to_string(), bob.id.to_string()] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (_, members) = api(
        test_app(pool.clone()),
        "GET",
        &accounts_uri,
        Some(&token),
        None,
    )
    .await;
    assert_eq!(ids_of(&members), [bob.id.to_string().as_str()]);
}

/// An `account_ids` array past the per-request cap is rejected
/// with 422 before the per-id existence probe and membership transaction.
#[sqlx::test(migrations = "../db/migrations")]
async fn list_add_over_the_cap_is_rejected(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;

    let (_, created) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/lists",
        Some(&token),
        Some(json!({ "title": "Friends" })),
    )
    .await;
    let list_id = created["id"].as_str().unwrap().to_owned();
    let accounts_uri = format!("/api/v1/lists/{list_id}/accounts");

    // 101 distinct ids — one past MAX_LIST_ACCOUNTS_PER_REQUEST (100).
    let account_ids: Vec<String> = (1..=101).map(|i| i.to_string()).collect();
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        &accounts_uri,
        Some(&token),
        Some(json!({ "account_ids": account_ids })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

/// Adding members is set-based end to end. Adding many followed
/// accounts to a list issues the same number of queries as adding one — proof
/// both the route's per-id existence probe and the per-entry membership loop
/// are gone. Each call targets a fresh list so no id is a repeat member.
#[sqlx::test(migrations = "../db/migrations")]
async fn list_add_query_count_is_flat(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;

    // 40 distinct accounts alice follows — all eligible for membership.
    let mut member_ids = Vec::new();
    for i in 0..40 {
        let member = create_local_account(&pool, &format!("m{i}"), "Member").await;
        follow::create(&pool, alice.id, member.id, None)
            .await
            .unwrap();
        member_ids.push(member.id.to_string());
    }

    let make_list = |title: &'static str| {
        let (pool, token) = (pool.clone(), token.clone());
        async move {
            let (_, created) = api(
                test_app(pool),
                "POST",
                "/api/v1/lists",
                Some(&token),
                Some(json!({ "title": title })),
            )
            .await;
            created["id"].as_str().unwrap().to_owned()
        }
    };
    let list_one = make_list("One").await;
    let list_many = make_list("Many").await;

    let counter = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(common::QueryCounter(counter.clone())),
    );

    let add = |list_id: String, ids: Vec<String>| {
        let (pool, token) = (pool.clone(), token.clone());
        async move {
            let (status, body) = api(
                test_app(pool),
                "POST",
                &format!("/api/v1/lists/{list_id}/accounts"),
                Some(&token),
                Some(json!({ "account_ids": ids })),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
    };

    counter.store(0, Ordering::Relaxed);
    add(list_one, vec![member_ids[0].clone()]).await;
    let one = counter.swap(0, Ordering::Relaxed);

    add(list_many, member_ids.clone()).await;
    let many = counter.load(Ordering::Relaxed);

    println!("list add: 1 member -> {one} queries, 40 -> {many}");
    assert_eq!(
        one, many,
        "adding 40 followed accounts must not add queries over adding one",
    );
}
