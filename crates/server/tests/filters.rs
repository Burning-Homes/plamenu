//! Content filters: the v2 filter-with-rules API, the deprecated v1 keyword
//! API, and the `filtered` attribute statuses carry for an authenticated
//! viewer — `/api/v1/filters*` and `/api/v2/filters*`.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{create_local_account, test_app};
use http_body_util::BodyExt;
use plamenu::auth::{generate_secret, hash_password, hash_secret};
use plamenu_db::account::Account;
use plamenu_db::custom_filter::{
    self, MAX_FILTERS_PER_ACCOUNT, MAX_KEYWORDS_PER_FILTER, NewKeyword,
};
use plamenu_db::{PgPool, oauth, user};
use serde_json::{Value, json};
use tower::ServiceExt;

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
            name: "filters-tests",
            website: None,
            client_id: &generate_secret(),
            client_secret_hash: &hash_secret(&generate_secret()),
            redirect_uris: &["urn:ietf:wg:oauth:2.0:oob".to_owned()],
            scopes: "read write",
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
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
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

async fn post_status(pool: &PgPool, token: &str, text: &str) -> Value {
    let (status, entity) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(token),
        Some(json!({ "status": text })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{entity}");
    entity
}

#[sqlx::test(migrations = "../db/migrations")]
async fn v2_filter_crud_lifecycle(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let (_bob, bob_token) = user_with_token(&pool, "bob").await;

    // Anonymous access is refused.
    let (status, _) = api(test_app(pool.clone()), "GET", "/api/v2/filters", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Create with a nested keyword.
    let (status, created) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v2/filters",
        Some(&token),
        Some(json!({
            "title": "Monsters",
            "context": ["home", "public"],
            "filter_action": "warn",
            "keywords_attributes": [{ "keyword": "godzilla", "whole_word": true }],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    assert!(created["id"].is_string(), "entity ids are strings");
    assert_eq!(created["title"], "Monsters");
    assert_eq!(created["context"], json!(["home", "public"]));
    assert_eq!(created["filter_action"], "warn");
    assert_eq!(created["keywords"].as_array().unwrap().len(), 1);
    assert_eq!(created["keywords"][0]["keyword"], "godzilla");
    assert_eq!(created["keywords"][0]["whole_word"], true);
    assert_eq!(created["statuses"], json!([]));
    let filter_id = created["id"].as_str().unwrap().to_owned();
    let keyword_id = created["keywords"][0]["id"].as_str().unwrap().to_owned();

    // Index lists it; another user sees none of it.
    let (_, index) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v2/filters",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(index.as_array().unwrap().len(), 1);
    let (_, bob_index) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v2/filters",
        Some(&bob_token),
        None,
    )
    .await;
    assert_eq!(bob_index, json!([]));

    // Owner scoping: bob can't read alice's filter.
    let (status, _) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v2/filters/{filter_id}"),
        Some(&bob_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Update: rename, narrow the context, change the action, and apply keyword
    // changes (update one, add one). Absent fields keep their value.
    let (status, updated) = api(
        test_app(pool.clone()),
        "PUT",
        &format!("/api/v2/filters/{filter_id}"),
        Some(&token),
        Some(json!({
            "title": "Kaiju",
            "filter_action": "hide",
            "keywords_attributes": [
                { "id": keyword_id, "keyword": "gojira" },
                { "keyword": "mothra", "whole_word": false },
            ],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["title"], "Kaiju");
    assert_eq!(updated["filter_action"], "hide");
    assert_eq!(
        updated["context"],
        json!(["home", "public"]),
        "absent context kept"
    );
    let keywords = updated["keywords"].as_array().unwrap();
    assert_eq!(keywords.len(), 2);
    assert_eq!(keywords[0]["keyword"], "gojira");
    assert_eq!(keywords[0]["whole_word"], true, "absent whole_word kept");

    // Destroy a keyword through the nested `_destroy` flag.
    let (_, pruned) = api(
        test_app(pool.clone()),
        "PUT",
        &format!("/api/v2/filters/{filter_id}"),
        Some(&token),
        Some(json!({ "keywords_attributes": [{ "id": keywords[1]["id"], "_destroy": true }] })),
    )
    .await;
    assert_eq!(pruned["keywords"].as_array().unwrap().len(), 1);

    // Delete.
    let (status, body) = api(
        test_app(pool.clone()),
        "DELETE",
        &format!("/api/v2/filters/{filter_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({}));
    let (status, _) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v2/filters/{filter_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn v2_validation_wording(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;

    // Blank title and empty context collect Mastodon's messages in order.
    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v2/filters",
        Some(&token),
        Some(json!({ "title": "", "context": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body["error"],
        "Validation failed: Title can't be blank, Context can't be blank, Context None or invalid context supplied"
    );

    // An unknown context value.
    let (_, body) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v2/filters",
        Some(&token),
        Some(json!({ "title": "x", "context": ["bogus"] })),
    )
    .await;
    assert_eq!(
        body["error"],
        "Validation failed: Context None or invalid context supplied"
    );

    // An unknown action value.
    let (_, body) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v2/filters",
        Some(&token),
        Some(json!({ "title": "x", "context": ["home"], "filter_action": "explode" })),
    )
    .await;
    assert_eq!(
        body["error"],
        "Validation failed: Action is not included in the list"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn v2_form_body_with_arrays(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;

    let (status, created) = api_form(
        test_app(pool.clone()),
        "POST",
        "/api/v2/filters",
        &token,
        &[
            ("title", "Monsters"),
            ("context[]", "home"),
            ("context[]", "public"),
            ("keywords_attributes[][keyword]", "godzilla"),
            ("keywords_attributes[][whole_word]", "true"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    assert_eq!(created["context"], json!(["home", "public"]));
    assert_eq!(created["keywords"].as_array().unwrap().len(), 1);
    assert_eq!(created["keywords"][0]["keyword"], "godzilla");
    assert_eq!(created["keywords"][0]["whole_word"], true);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn v2_keyword_and_status_subresources(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let (_bob, bob_token) = user_with_token(&pool, "bob").await;

    let (_, filter) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v2/filters",
        Some(&token),
        Some(json!({ "title": "f", "context": ["home"] })),
    )
    .await;
    let filter_id = filter["id"].as_str().unwrap().to_owned();

    // Keyword create/show/update/destroy.
    let (status, keyword) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v2/filters/{filter_id}/keywords"),
        Some(&token),
        Some(json!({ "keyword": "spoiler", "whole_word": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{keyword}");
    assert_eq!(keyword["keyword"], "spoiler");
    assert_eq!(keyword["whole_word"], false);
    let keyword_id = keyword["id"].as_str().unwrap().to_owned();

    let (_, listed) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v2/filters/{filter_id}/keywords"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(listed.as_array().unwrap().len(), 1);

    let (status, blank) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v2/filters/{filter_id}/keywords"),
        Some(&token),
        Some(json!({ "keyword": "" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(blank["error"], "Validation failed: Keyword can't be blank");

    let (_, updated) = api(
        test_app(pool.clone()),
        "PUT",
        &format!("/api/v2/filters/keywords/{keyword_id}"),
        Some(&token),
        Some(json!({ "keyword": "spoilers" })),
    )
    .await;
    assert_eq!(updated["keyword"], "spoilers");
    assert_eq!(updated["whole_word"], false, "absent whole_word kept");

    // A status entry: must be a status the account can see; duplicates 422.
    let post = post_status(&pool, &token, "<p>secret plans</p>").await;
    let post_id = post["id"].as_str().unwrap().to_owned();
    let (status, entry) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v2/filters/{filter_id}/statuses"),
        Some(&token),
        Some(json!({ "status_id": post_id })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{entry}");
    assert_eq!(entry["status_id"], post_id);
    let entry_id = entry["id"].as_str().unwrap().to_owned();

    let (status, dup) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v2/filters/{filter_id}/statuses"),
        Some(&token),
        Some(json!({ "status_id": post_id })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        dup["error"],
        "Validation failed: Status has already been taken"
    );

    // A status alice can't see (bob's followers-only post) is rejected as
    // invalid, like Mastodon's StatusPolicy `show?` check.
    let (_, bob_private) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/statuses",
        Some(&bob_token),
        Some(json!({ "status": "<p>secret</p>", "visibility": "private" })),
    )
    .await;
    let (status, invalid) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v2/filters/{filter_id}/statuses"),
        Some(&token),
        Some(json!({ "status_id": bob_private["id"] })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{invalid}");
    assert_eq!(invalid["error"], "Validation failed: Status is invalid");

    // An unknown id does not exist.
    let (status, missing) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v2/filters/{filter_id}/statuses"),
        Some(&token),
        Some(json!({ "status_id": "999999999999" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(missing["error"], "Validation failed: Status must exist");

    let (status, _) = api(
        test_app(pool.clone()),
        "DELETE",
        &format!("/api/v2/filters/statuses/{entry_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Destroying the keyword empties the keyword list.
    let (status, _) = api(
        test_app(pool.clone()),
        "DELETE",
        &format!("/api/v2/filters/keywords/{keyword_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn filtered_attribute_on_statuses(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;

    // A keyword filter and a status-pin filter.
    api(
        test_app(pool.clone()),
        "POST",
        "/api/v2/filters",
        Some(&token),
        Some(json!({
            "title": "Monsters",
            "context": ["home"],
            "filter_action": "warn",
            "keywords_attributes": [{ "keyword": "godzilla", "whole_word": true }],
        })),
    )
    .await;
    let (_, pin_filter) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v2/filters",
        Some(&token),
        Some(json!({ "title": "Pinned", "context": ["home"] })),
    )
    .await;
    let pin_id = pin_filter["id"].as_str().unwrap().to_owned();

    // A post hitting the keyword carries a FilterResult; the embedded filter
    // is the rules-less summary.
    let matched = post_status(&pool, &token, "I love godzilla movies").await;
    let matched_id = matched["id"].as_str().unwrap().to_owned();
    let (status, entity) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/statuses/{matched_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let filtered = entity["filtered"].as_array().unwrap();
    assert_eq!(filtered.len(), 1, "{}", entity["filtered"]);
    assert_eq!(filtered[0]["filter"]["title"], "Monsters");
    assert_eq!(filtered[0]["filter"]["filter_action"], "warn");
    assert!(
        filtered[0]["filter"].get("keywords").is_none(),
        "FilterResult's filter omits the rules"
    );
    assert_eq!(filtered[0]["keyword_matches"], json!(["godzilla"]));
    assert_eq!(filtered[0]["status_matches"], json!([]));

    // A status pinned to the second filter matches by id, not keyword.
    let plain = post_status(&pool, &token, "<p>nothing notable</p>").await;
    let plain_id = plain["id"].as_str().unwrap().to_owned();
    api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v2/filters/{pin_id}/statuses"),
        Some(&token),
        Some(json!({ "status_id": plain_id })),
    )
    .await;
    let (_, entity) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/statuses/{plain_id}"),
        Some(&token),
        None,
    )
    .await;
    let filtered = entity["filtered"].as_array().unwrap();
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0]["filter"]["title"], "Pinned");
    assert_eq!(filtered[0]["status_matches"], json!([plain_id]));
    assert_eq!(filtered[0]["keyword_matches"], json!([]));

    // An authenticated viewer with no match still gets an empty array.
    let (_, entity) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/statuses/{matched_id}"),
        Some(&token),
        None,
    )
    .await;
    assert!(entity["filtered"].is_array());

    // Anonymous viewers never carry `filtered` at all.
    let (_, anon) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/statuses/{matched_id}"),
        None,
        None,
    )
    .await;
    assert!(anon.get("filtered").is_none(), "{anon}");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn v1_deprecated_api(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;

    // Create maps a phrase + context onto a filter and its single keyword.
    let (status, created) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/filters",
        Some(&token),
        Some(json!({
            "phrase": "spoiler",
            "context": ["home", "public"],
            "irreversible": true,
            "whole_word": true,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    assert_eq!(created["phrase"], "spoiler");
    assert_eq!(created["context"], json!(["home", "public"]));
    assert_eq!(
        created["irreversible"], true,
        "irreversible maps to the hide action"
    );
    assert_eq!(created["whole_word"], true);
    let keyword_id = created["id"].as_str().unwrap().to_owned();

    let (_, index) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/filters",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(index.as_array().unwrap().len(), 1);

    // A keyword-only change is allowed.
    let (status, updated) = api(
        test_app(pool.clone()),
        "PUT",
        &format!("/api/v1/filters/{keyword_id}"),
        Some(&token),
        Some(json!({ "phrase": "spoilers", "whole_word": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["phrase"], "spoilers");
    assert_eq!(updated["whole_word"], false);
    assert_eq!(
        updated["context"],
        json!(["home", "public"]),
        "filter attrs untouched"
    );

    // Delete removes the keyword.
    let (status, _) = api(
        test_app(pool.clone()),
        "DELETE",
        &format!("/api/v1/filters/{keyword_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/filters/{keyword_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn v1_multiple_keyword_guard(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;

    // A filter with two keywords (built through v2).
    let (_, filter) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v2/filters",
        Some(&token),
        Some(json!({
            "title": "f",
            "context": ["home"],
            "keywords_attributes": [{ "keyword": "alpha" }, { "keyword": "beta" }],
        })),
    )
    .await;
    let first_keyword = filter["keywords"][0]["id"].as_str().unwrap().to_owned();

    // Changing a filter-level attribute through the single-keyword v1 API is
    // refused while the filter has more than one keyword.
    let (status, body) = api(
        test_app(pool.clone()),
        "PUT",
        &format!("/api/v1/filters/{first_keyword}"),
        Some(&token),
        Some(json!({ "context": ["public"] })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body["error"],
        "These parameters cannot be changed from this application because they apply to more than one filter keyword. Use a more recent application or the web interface."
    );

    // A keyword-only change is still fine.
    let (status, _) = api(
        test_app(pool.clone()),
        "PUT",
        &format!("/api/v1/filters/{first_keyword}"),
        Some(&token),
        Some(json!({ "phrase": "alphas" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

// --- Cardinality caps (audit #58) ------------------------------------------

fn home() -> Vec<String> {
    vec!["home".to_owned()]
}

/// One create body can't persist an unbounded keyword set.
#[sqlx::test(migrations = "../db/migrations")]
async fn create_rejects_too_many_keywords(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let keywords: Vec<Value> = (0..=MAX_KEYWORDS_PER_FILTER)
        .map(|i| json!({ "keyword": format!("k{i}") }))
        .collect();
    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v2/filters",
        Some(&token),
        Some(json!({ "title": "f", "context": ["home"], "keywords_attributes": keywords })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("Keywords are too many"),
        "{body}"
    );
    // Nothing was persisted.
    assert_eq!(
        custom_filter::count_owned(&pool, alice.id).await.unwrap(),
        0
    );
}

/// An account can't accumulate unbounded filters — both API versions refuse
/// once it is at the per-account cap.
#[sqlx::test(migrations = "../db/migrations")]
async fn create_rejects_at_filter_limit(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    for i in 0..MAX_FILTERS_PER_ACCOUNT {
        custom_filter::create(
            &pool,
            alice.id,
            &format!("f{i}"),
            "warn",
            &home(),
            None,
            &[],
        )
        .await
        .unwrap();
    }
    assert_eq!(
        custom_filter::count_owned(&pool, alice.id).await.unwrap(),
        i64::try_from(MAX_FILTERS_PER_ACCOUNT).unwrap()
    );

    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v2/filters",
        Some(&token),
        Some(json!({ "title": "one more", "context": ["home"] })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("limit of"),
        "{body}"
    );

    // The deprecated v1 create is guarded by the same cap.
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/filters",
        Some(&token),
        Some(json!({ "phrase": "one more", "context": ["home"] })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // No over-limit filter slipped through either path.
    assert_eq!(
        custom_filter::count_owned(&pool, alice.id).await.unwrap(),
        i64::try_from(MAX_FILTERS_PER_ACCOUNT).unwrap()
    );
}

/// Both keyword-adding paths on an existing filter refuse to push it past the
/// per-filter keyword cap.
#[sqlx::test(migrations = "../db/migrations")]
async fn keyword_adds_reject_over_filter_keyword_limit(pool: PgPool) {
    let (alice, token) = user_with_token(&pool, "alice").await;
    let keywords: Vec<NewKeyword> = (0..MAX_KEYWORDS_PER_FILTER)
        .map(|i| NewKeyword {
            keyword: format!("k{i}"),
            whole_word: false,
        })
        .collect();
    let filter = custom_filter::create(&pool, alice.id, "full", "warn", &home(), None, &keywords)
        .await
        .unwrap();
    let filter_id = filter.id;

    // Incremental keyword add (POST .../keywords) is refused at the cap.
    let (status, body) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v2/filters/{filter_id}/keywords"),
        Some(&token),
        Some(json!({ "keyword": "overflow" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // A v2 update that creates another keyword is refused too.
    let (status, _) = api(
        test_app(pool.clone()),
        "PUT",
        &format!("/api/v2/filters/{filter_id}"),
        Some(&token),
        Some(json!({ "keywords_attributes": [{ "keyword": "overflow" }] })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // The keyword set stayed at the cap.
    assert_eq!(
        custom_filter::count_keywords(&pool, filter_id)
            .await
            .unwrap(),
        i64::try_from(MAX_KEYWORDS_PER_FILTER).unwrap()
    );

    // The guard only blocks *growth* past the cap, not ordinary editing: a
    // destroy (which never grows the set) is admitted at the cap, and once the
    // set is below the cap again a create is admitted.
    let existing = custom_filter::keywords_for(&pool, filter_id).await.unwrap();
    let (status, _) = api(
        test_app(pool.clone()),
        "PUT",
        &format!("/api/v2/filters/{filter_id}"),
        Some(&token),
        Some(json!({
            "keywords_attributes": [{ "id": existing[0].id.to_string(), "_destroy": true }],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        custom_filter::count_keywords(&pool, filter_id)
            .await
            .unwrap(),
        i64::try_from(MAX_KEYWORDS_PER_FILTER).unwrap() - 1
    );
    let (status, _) = api(
        test_app(pool.clone()),
        "POST",
        &format!("/api/v2/filters/{filter_id}/keywords"),
        Some(&token),
        Some(json!({ "keyword": "refill" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        custom_filter::count_keywords(&pool, filter_id)
            .await
            .unwrap(),
        i64::try_from(MAX_KEYWORDS_PER_FILTER).unwrap()
    );
}
