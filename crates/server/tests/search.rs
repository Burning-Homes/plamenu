//! `/api/v2/search` and `/api/v1/accounts/search` integration tests —
//! text search over accounts/statuses/hashtags, exact acct matches,
//! webfinger resolution and search-by-URL, through the real router.

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{
    RemoteUser, StubFederation, TEST_DOMAIN, create_local_account, test_app, test_app_with,
    test_state_with,
};
use http_body_util::BodyExt;
use plamenu::actions::{self, PostParams};
use plamenu::auth::hash_password;
use plamenu_db::account::Account;
use plamenu_db::{PgPool, user};
use plamenu_federation::FetchedPage;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn user_with_token(pool: &PgPool, username: &str, scope: &str) -> (Account, String) {
    let account = create_local_account(pool, username, username).await;
    let email = format!("{username}@plamenu.test");
    let hash = hash_password("pw").unwrap();
    user::create(pool, account.id, Some(&email), &hash)
        .await
        .unwrap();
    let app_response = api(
        test_app(pool.clone()),
        "POST",
        "/api/v1/apps",
        None,
        Some(json!({
            "client_name": "search-tests",
            "redirect_uris": ["urn:ietf:wg:oauth:2.0:oob"],
            "scopes": scope,
        })),
    )
    .await;
    let client_id = app_response.1["client_id"].as_str().unwrap().to_owned();
    let client_secret = app_response.1["client_secret"].as_str().unwrap().to_owned();
    let auth = post_form(
        test_app(pool.clone()),
        "/oauth/authorize",
        &[
            ("client_id", client_id.as_str()),
            ("redirect_uri", "urn:ietf:wg:oauth:2.0:oob"),
            ("scope", scope),
            ("email", &email),
            ("password", "pw"),
        ],
    )
    .await;
    let code = auth
        .split("<pre class=\"oob-code\">")
        .nth(1)
        .unwrap()
        .split("</pre>")
        .next()
        .unwrap()
        .to_owned();
    let token = api(
        test_app(pool.clone()),
        "POST",
        "/oauth/token",
        None,
        Some(json!({
            "grant_type": "authorization_code",
            "code": code,
            "client_id": client_id,
            "client_secret": client_secret,
            "redirect_uri": "urn:ietf:wg:oauth:2.0:oob",
        })),
    )
    .await;
    (
        account,
        token.1["access_token"].as_str().unwrap().to_owned(),
    )
}

async fn post_form(app: Router, uri: &str, fields: &[(&str, &str)]) -> String {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(serde_urlencoded::to_string(fields).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Generic JSON API call; returns (status, body).
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

async fn post_status(pool: &PgPool, username: &str, text: &str, visibility: &str) -> i64 {
    let state = test_state_with(pool.clone(), Arc::default());
    let (stored, _) = actions::post_status(
        &state,
        PostParams {
            username,
            text,
            visibility,
            in_reply_to_id: None,
            media_ids: &[],
            quoted_status_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    stored.id
}

fn section<'a>(results: &'a Value, name: &str) -> &'a Vec<Value> {
    results[name].as_array().unwrap()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn search_requires_q_and_read_scope(pool: PgPool) {
    common::open_previews(&pool).await;
    let (status, _) = api(test_app(pool.clone()), "GET", "/api/v2/search", None, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A token without any read grant is rejected, anonymous is fine.
    let (_, write_token) = user_with_token(&pool, "writer", "write").await;
    let (status, body) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v2/search?q=x",
        Some(&write_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, _) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v2/search?q=x",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The v1 account-autocomplete endpoint always needs a user.
    let (status, _) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/accounts/search?q=x",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn anonymous_search_is_restricted(pool: PgPool) {
    common::open_previews(&pool).await;
    create_local_account(&pool, "abba", "Abba").await;
    post_status(&pool, "abba", "winter waterloo", "public").await;

    // Pagination and resolution need authentication (Mastodon's exact 401s).
    let (status, body) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v2/search?q=abba&offset=1",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        body["error"],
        "Search queries pagination is not supported without authentication"
    );
    let (status, body) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v2/search?q=abba&resolve=true",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        body["error"],
        "Search queries that resolve remote resources are not supported without authentication"
    );

    // Short queries return no non-exact account matches for anonymous
    // viewers; three characters are enough.
    let (_, results) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v2/search?q=ab",
        None,
        None,
    )
    .await;
    assert!(section(&results, "accounts").is_empty());
    let (_, results) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v2/search?q=abb",
        None,
        None,
    )
    .await;
    assert_eq!(section(&results, "accounts").len(), 1);

    // Statuses are never searched anonymously; hashtag entities carry no
    // viewer-specific `following` flag.
    let (_, results) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v2/search?q=winter",
        None,
        None,
    )
    .await;
    assert!(section(&results, "statuses").is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn combined_search_finds_accounts_statuses_and_hashtags(pool: PgPool) {
    let (_, token) = user_with_token(&pool, "searcher", "read write").await;
    create_local_account(&pool, "rustfan", "Rust Fan").await;
    create_local_account(&pool, "unrelated", "Someone Else").await;
    let status_id = post_status(&pool, "rustfan", "learning #rust today", "public").await;

    let (status, results) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v2/search?q=rust",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let accounts = section(&results, "accounts");
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0]["acct"], "rustfan");

    let statuses = section(&results, "statuses");
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0]["id"], status_id.to_string());

    let hashtags = section(&results, "hashtags");
    assert_eq!(hashtags.len(), 1);
    assert_eq!(hashtags[0]["name"], "rust");
    assert_eq!(
        hashtags[0]["url"],
        format!("https://{TEST_DOMAIN}/tags/rust")
    );
    assert_eq!(hashtags[0]["following"], false);
    assert_eq!(hashtags[0]["featuring"], false);
    // The single public #rust post today shows up in the seven-day history.
    let history = hashtags[0]["history"].as_array().unwrap();
    assert_eq!(history.len(), 7, "history is always seven days");
    assert_eq!(history[0]["uses"], "1", "one public use today");
    assert_eq!(history[0]["accounts"], "1", "by one account");

    // The Mastodon 4.5 serializer always carries `collections`.
    assert!(section(&results, "collections").is_empty());

    // `type` narrows to a single section.
    let (_, results) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v2/search?q=rust&type=hashtags",
        Some(&token),
        None,
    )
    .await;
    assert!(section(&results, "accounts").is_empty());
    assert!(section(&results, "statuses").is_empty());
    assert_eq!(section(&results, "hashtags").len(), 1);

    // Offsets work on single-type searches.
    let (_, results) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v2/search?q=rust&type=hashtags&offset=1",
        Some(&token),
        None,
    )
    .await;
    assert!(section(&results, "hashtags").is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn status_search_is_visibility_scoped(pool: PgPool) {
    let (_, token) = user_with_token(&pool, "searcher", "read write").await;
    create_local_account(&pool, "author", "Author").await;
    post_status(&pool, "author", "sailing in private", "private").await;
    let own = post_status(&pool, "searcher", "sailing my own boat", "private").await;

    let (_, results) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v2/search?q=sailing",
        Some(&token),
        None,
    )
    .await;
    let statuses = section(&results, "statuses");
    assert_eq!(statuses.len(), 1, "only the searcher's own private post");
    assert_eq!(statuses[0]["id"], own.to_string());

    // An unknown account_id filter is a 404, like Mastodon.
    let (status, _) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v2/search?q=sailing&account_id=1",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn exact_acct_match_resolves_with_webfinger(pool: PgPool) {
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_users(&[&bob]);
    let (_, token) = user_with_token(&pool, "searcher", "read write").await;
    let app = || test_app_with(pool.clone(), stub.clone());

    // Unknown account, no resolve: nothing.
    let (_, results) = api(
        app(),
        "GET",
        "/api/v2/search?q=bob%40remote.example",
        Some(&token),
        None,
    )
    .await;
    assert!(section(&results, "accounts").is_empty());

    // With resolve, webfinger + actor fetch store and return the account.
    let (_, results) = api(
        app(),
        "GET",
        "/api/v2/search?q=%40bob%40remote.example&resolve=true",
        Some(&token),
        None,
    )
    .await;
    let accounts = section(&results, "accounts");
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0]["acct"], "bob@remote.example");
    assert!(stub.fetches().contains(&bob.actor.id));

    // Some domains publish handles on one host while serving actors from
    // another. The stored/rest acct is the WebFinger acct, not the actor host.
    let split = RemoteUser::new("social.wake.st", "liaizon");
    let split_stub = StubFederation::with_actors([split.actor.clone()]);
    split_stub.webfinger.lock().unwrap().insert(
        "liaizon@wake.st".to_owned(),
        vec![plamenu_federation::WebfingerCandidate {
            actor_uri: split.actor.id.clone(),
            advertised_type: split.actor.actor_type().map(str::to_owned),
        }],
    );
    let split_app = || test_app_with(pool.clone(), split_stub.clone());
    let (_, results) = api(
        split_app(),
        "GET",
        "/api/v2/search?q=liaizon%40wake.st&resolve=true",
        Some(&token),
        None,
    )
    .await;
    let accounts = section(&results, "accounts");
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0]["acct"], "liaizon@wake.st");

    // Bluesky bridge handles can put a domain-shaped value in the username
    // part. Mastodon accepts those for remote accounts and WebFinger confirms
    // the exact acct before the actor is stored.
    let mut bridge = RemoteUser::new("bskytest2.sdu.li", "tysobro.bsky.social");
    bridge.actor.kind = "Service".to_owned();
    bridge.actor.url = Some(json!("https://bsky.app/profile/tysobro.bsky.social"));
    let bridge_stub = StubFederation::with_users(&[&bridge]);
    let bridge_app = || test_app_with(pool.clone(), bridge_stub.clone());
    let (_, results) = api(
        bridge_app(),
        "GET",
        "/api/v2/search?q=%40tysobro.bsky.social%40bskytest2.sdu.li&resolve=true&limit=40&type=accounts&offset=0",
        Some(&token),
        None,
    )
    .await;
    let accounts = section(&results, "accounts");
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0]["acct"], "tysobro.bsky.social@bskytest2.sdu.li");
    assert_eq!(accounts[0]["username"], "tysobro.bsky.social");
    assert_eq!(
        accounts[0]["url"],
        "https://bsky.app/profile/tysobro.bsky.social"
    );
    assert_eq!(accounts[0]["bot"], true);

    // FEP-2c59 actors may advertise their canonical handle with a `webfinger`
    // property. Mastodon accepts that even when `preferredUsername` is absent.
    let mut fep = RemoteUser::new("social.example", "sleyka");
    fep.actor.preferred_username.clear();
    fep.actor.webfinger = Some("acct:sleyka@social.example".to_owned());
    let fep_stub = StubFederation::with_users(&[&fep]);
    let fep_app = || test_app_with(pool.clone(), fep_stub.clone());
    let (_, results) = api(
        fep_app(),
        "GET",
        "/api/v2/search?q=%40sleyka%40social.example&resolve=true",
        Some(&token),
        None,
    )
    .await;
    let accounts = section(&results, "accounts");
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0]["acct"], "sleyka@social.example");

    // Once known, the exact match works without resolve too — and a local
    // `user@ourdomain` exact match never needs the network.
    let (_, results) = api(
        app(),
        "GET",
        "/api/v2/search?q=bob%40remote.example",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(section(&results, "accounts").len(), 1);
    let (_, results) = api(
        app(),
        "GET",
        &format!("/api/v2/search?q=searcher%40{TEST_DOMAIN}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(section(&results, "accounts")[0]["acct"], "searcher");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn url_search_resolves_local_and_remote_resources(pool: PgPool) {
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_users(&[&bob]);
    let note_uri = "https://remote.example/notes/100";
    stub.objects.lock().unwrap().insert(
        note_uri.to_owned(),
        json!({
            "id": note_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>hello from afar</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "published": "2026-06-11T08:00:00Z",
        }),
    );
    let (_, token) = user_with_token(&pool, "searcher", "read write").await;
    create_local_account(&pool, "author", "Author").await;
    let local_status = post_status(&pool, "author", "hello local", "public").await;
    let private_status = post_status(&pool, "author", "hello hidden", "private").await;
    let app = || test_app_with(pool.clone(), stub.clone());
    let search = |q: String, token: String| {
        let app = app();
        async move {
            api(
                app,
                "GET",
                &format!("/api/v2/search?q={}&resolve=true", urlencode(&q)),
                Some(&token),
                None,
            )
            .await
        }
    };

    // Our own status URL resolves from storage; visibility still applies.
    let local_url = format!("https://{TEST_DOMAIN}/users/author/statuses/{local_status}");
    let (_, results) = search(local_url, token.clone()).await;
    assert_eq!(
        section(&results, "statuses")[0]["id"],
        local_status.to_string()
    );
    let private_url = format!("https://{TEST_DOMAIN}/users/author/statuses/{private_status}");
    let (_, results) = search(private_url, token.clone()).await;
    assert!(section(&results, "statuses").is_empty());

    // A remote note URL is fetched, attributed, ingested and returned.
    let (_, results) = search(note_uri.to_owned(), token.clone()).await;
    let statuses = section(&results, "statuses");
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0]["content"], "<p>hello from afar</p>");
    assert_eq!(statuses[0]["account"]["acct"], "bob@remote.example");
    assert!(stub.fetches().contains(&note_uri.to_owned()));

    // A remote actor URL resolves to an account (served as a plain object).
    let actor_uri = "https://remote.example/users/carol";
    stub.objects.lock().unwrap().insert(
        actor_uri.to_owned(),
        json!({
            "id": actor_uri,
            "type": "Person",
            "preferredUsername": "carol",
            "inbox": format!("{actor_uri}/inbox"),
            "publicKey": {
                "id": format!("{actor_uri}#main-key"),
                "owner": actor_uri,
                "publicKeyPem": "PEM",
            },
        }),
    );
    let (_, results) = search(actor_uri.to_owned(), token.clone()).await;
    assert_eq!(
        section(&results, "accounts")[0]["acct"],
        "carol@remote.example"
    );

    // `type` gates which sections a URL result may land in.
    let (_, results) = api(
        app(),
        "GET",
        &format!(
            "/api/v2/search?q={}&resolve=true&type=accounts",
            urlencode(note_uri)
        ),
        Some(&token),
        None,
    )
    .await;
    assert!(section(&results, "statuses").is_empty());
    assert!(section(&results, "accounts").is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn url_search_follows_permalinks_to_canonical_id(pool: PgPool) {
    // A status's browser permalink (its AP `url`) differs from its canonical
    // `id`; resolving the permalink must follow to the id and ingest it.
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_users(&[&bob]);
    let permalink = "https://remote.example/@bob/200";
    let canonical = "https://remote.example/ap/notes/200";
    let note = json!({
        "id": canonical,
        "url": permalink,
        "type": "Note",
        "attributedTo": bob.actor.id,
        "content": "<p>permalinked note</p>",
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "published": "2026-06-25T08:00:00Z",
    });
    {
        let mut objects = stub.objects.lock().unwrap();
        // Served both at the permalink (where the id differs) and the
        // canonical id (the re-dereference target).
        objects.insert(permalink.to_owned(), note.clone());
        objects.insert(canonical.to_owned(), note);
    }
    let (_, token) = user_with_token(&pool, "searcher", "read write").await;
    let app = || test_app_with(pool.clone(), stub.clone());
    let search = |q: String| {
        let app = app();
        let token = token.clone();
        async move {
            api(
                app,
                "GET",
                &format!("/api/v2/search?q={}&resolve=true", urlencode(&q)),
                Some(&token),
                None,
            )
            .await
        }
    };

    let (_, results) = search(permalink.to_owned()).await;
    let statuses = section(&results, "statuses");
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0]["content"], "<p>permalinked note</p>");
    assert_eq!(statuses[0]["account"]["acct"], "bob@remote.example");
    // The permalink was fetched, then the canonical id re-dereferenced.
    let fetches = stub.fetches();
    assert!(fetches.contains(&permalink.to_owned()));
    assert!(fetches.contains(&canonical.to_owned()));

    // Searching the permalink again serves the now-known status from storage.
    let (_, results) = search(permalink.to_owned()).await;
    assert_eq!(section(&results, "statuses").len(), 1);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn exact_owncast_homepage_resolves_and_is_cached(pool: PgPool) {
    let mut caster = RemoteUser::new("owncast.example", "inex");
    caster.actor.kind = "Service".into();
    caster.actor.url = Some(json!("https://owncast.example/"));
    let stub = StubFederation::with_users(&[&caster]);
    stub.webfinger_hls.lock().unwrap().insert(
        caster.acct.clone(),
        "https://owncast.example/hls/stream.m3u8".into(),
    );
    stub.serve_page_as(
        "https://owncast.example/.well-known/nodeinfo",
        "https://owncast.example/.well-known/nodeinfo",
        "application/json",
        r#"{"links":[{"rel":"http://nodeinfo.diaspora.software/ns/schema/2.0","href":"https://owncast.example/nodeinfo/2.0"}]}"#,
    );
    stub.serve_page_as(
        "https://owncast.example/nodeinfo/2.0",
        "https://owncast.example/nodeinfo/2.0",
        "application/json",
        r#"{"software":{"name":"owncast"},"protocols":["activitypub"],"metadata":{"federation":{"username":"inex"}}}"#,
    );
    let (_, token) = user_with_token(&pool, "searcher", "read").await;
    let app = || test_app_with(pool.clone(), stub.clone());
    let query = format!(
        "/api/v2/search?q={}&resolve=true",
        urlencode("https://owncast.example/")
    );
    let (_, results) = api(app(), "GET", &query, Some(&token), None).await;
    assert_eq!(section(&results, "accounts").len(), 1);
    assert_eq!(
        section(&results, "accounts")[0]["acct"],
        "inex@owncast.example"
    );
    let fetched = stub.page_fetches().len();

    // Exact known-homepage lookup is storage-only after the first discovery.
    let (_, results) = api(app(), "GET", &query, Some(&token), None).await;
    assert_eq!(section(&results, "accounts").len(), 1);
    assert_eq!(stub.page_fetches().len(), fetched);
    assert!(
        !stub
            .fetches()
            .contains(&"https://owncast.example/".to_owned())
    );
}

/// Discourse serves category/topic browser URLs as HTML (406 for an AP
/// Accept), while its plugin exposes the category through `WebFinger` and a
/// topic's canonical AP object id in public topic JSON. URL search bridges
/// exactly those advertised identities and caches the resulting web URLs.
#[sqlx::test(migrations = "../db/migrations")]
async fn url_search_resolves_discourse_category_and_topic_pages(pool: PgPool) {
    let mut forum = RemoteUser::new("discourse.example", "federation");
    forum.actor.kind = "Group".to_owned();
    let category_url = "https://discourse.example/c/federation/5";
    forum.actor.url = Some(json!(category_url));
    let diana = RemoteUser::new("discourse.example", "diana");
    let stub = StubFederation::with_users(&[&forum, &diana]);

    let topic_url = "https://discourse.example/t/interop-thread/42/2";
    let topic_json_url = format!("{topic_url}.json");
    let first_object = "https://discourse.example/ap/object/first";
    let reply_object = "https://discourse.example/ap/object/reply";
    stub.pages.lock().unwrap().insert(
        topic_json_url.clone(),
        FetchedPage {
            final_url: topic_json_url.clone(),
            content_type: "application/json".to_owned(),
            body: json!({
                "activity_pub_object_id": first_object,
                "post_stream": {"posts": [
                    {"post_number": 1, "activity_pub_object_id": first_object},
                    {"post_number": 2, "activity_pub_object_id": reply_object},
                ]},
            })
            .to_string(),
        },
    );
    stub.objects.lock().unwrap().insert(
        reply_object.to_owned(),
        json!({
            "id": reply_object,
            "url": topic_url,
            "type": "Article",
            "name": "A Discourse reply",
            "attributedTo": diana.actor.id,
            "content": "<p>resolved through public topic JSON</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "published": "2026-08-13T16:00:00Z",
        }),
    );

    let (_, token) = user_with_token(&pool, "searcher", "read write").await;
    let search = |q: &'static str| {
        let app = test_app_with(pool.clone(), stub.clone());
        let token = token.clone();
        async move {
            api(
                app,
                "GET",
                &format!("/api/v2/search?q={}&resolve=true", urlencode(q)),
                Some(&token),
                None,
            )
            .await
        }
    };

    let (_, results) = search(category_url).await;
    let accounts = section(&results, "accounts");
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0]["acct"], "federation@discourse.example");
    assert_eq!(accounts[0]["group"], true);
    assert_eq!(accounts[0]["url"], category_url);
    assert!(
        !stub.fetches().contains(&category_url.to_owned()),
        "known Discourse category routes go straight to validated WebFinger"
    );

    let fetch_count = stub.fetches().len();
    let (_, cached) = search(category_url).await;
    assert_eq!(section(&cached, "accounts").len(), 1);
    assert_eq!(stub.fetches().len(), fetch_count, "cached URL avoids fetch");

    let (_, results) = search(topic_url).await;
    let statuses = section(&results, "statuses");
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0]["uri"], reply_object);
    assert_eq!(statuses[0]["url"], topic_url);
    assert_eq!(statuses[0]["title"], "A Discourse reply");
    assert!(stub.page_fetches.lock().unwrap().contains(&topic_json_url));
    assert!(stub.fetches().contains(&reply_object.to_owned()));
    assert!(
        !stub.fetches().contains(&topic_url.to_owned()),
        "known Discourse topic routes use public JSON before AP fetching"
    );
    assert!(
        !stub.fetches().contains(&first_object.to_owned()),
        "a post permalink selects its own post_number, not the topic root"
    );

    let page_fetch_count = stub.page_fetches.lock().unwrap().len();
    let (_, cached) = search(topic_url).await;
    assert_eq!(section(&cached, "statuses").len(), 1);
    assert_eq!(
        stub.page_fetches.lock().unwrap().len(),
        page_fetch_count,
        "cached permalink avoids Discourse JSON discovery"
    );
}

/// Search-by-URL of a Lemmy `Page` ingests it natively: the real title
/// in `title`, the full body as `content` (not Mastodon's `<h2>`-plus-link
/// compaction), the excerpt `summary` dropped rather than shown, and the
/// attachment still proxied through the instance.
#[sqlx::test(migrations = "../db/migrations")]
async fn url_search_resolves_non_note_status_types_natively(pool: PgPool) {
    let bob = RemoteUser::new("lemmy.test", "bob");
    let stub = StubFederation::with_users(&[&bob]);
    let page_uri = "https://lemmy.test/post/49442160";
    let image_url = "https://lemmy.test/pictrs/image/c310cbc4.webp";
    stub.objects.lock().unwrap().insert(
        page_uri.to_owned(),
        json!({
            "id": page_uri,
            "type": "Page",
            "attributedTo": bob.actor.id,
            "name": "Lemmy page title",
            "content": "<p>This long body is not used for converted statuses</p>",
            "summary": "<p>Short summary</p>",
            "attachment": [{
                "type": "Image",
                "url": image_url
            }],
            "to": [
                "https://lemmy.test/c/memes",
                "https://www.w3.org/ns/activitystreams#Public"
            ],
            "published": "2026-06-30T17:28:50Z",
        }),
    );
    let (_, token) = user_with_token(&pool, "searcher", "read write").await;
    let (status, results) = api(
        test_app_with(pool.clone(), stub.clone()),
        "GET",
        &format!(
            "/api/v2/search?q={}&type=statuses&resolve=true&limit=1",
            urlencode(page_uri)
        ),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let statuses = section(&results, "statuses");
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0]["account"]["acct"], "bob@lemmy.test");
    assert_eq!(statuses[0]["url"], page_uri);
    let content = statuses[0]["content"].as_str().unwrap();
    assert_eq!(statuses[0]["title"], "Lemmy page title");
    assert_eq!(statuses[0]["object_type"], "Page");
    assert!(
        content.contains("This long body"),
        "the body is stored natively, not discarded: {content}"
    );
    assert!(
        !content.contains("<h2>"),
        "no compacted title heading: {content}"
    );
    assert!(
        !content.contains("Short summary"),
        "the excerpt is neither body nor CW: {content}"
    );
    assert_eq!(statuses[0]["spoiler_text"], "");
    assert_eq!(statuses[0]["media_attachments"][0]["type"], "image");
    // The attachment is proxied through the instance, never the origin.
    let url = statuses[0]["media_attachments"][0]["url"].as_str().unwrap();
    assert!(
        url.starts_with("https://plamenu.test/media/proxy/attachment/"),
        "{url}"
    );
    assert!(!url.contains("lemmy.test"), "{url}");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn v1_account_search_returns_entities(pool: PgPool) {
    let (_, token) = user_with_token(&pool, "searcher", "read write").await;
    create_local_account(&pool, "mira", "Mira").await;
    create_local_account(&pool, "miranda", "Miranda").await;

    let (status, results) = api(
        test_app(pool.clone()),
        "GET",
        "/api/v1/accounts/search?q=mir&limit=1",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let accounts = results.as_array().unwrap();
    assert_eq!(accounts.len(), 1);
    assert!(accounts[0]["acct"].as_str().unwrap().starts_with("mir"));
}

fn urlencode(s: &str) -> String {
    serde_urlencoded::to_string([("q", s)])
        .unwrap()
        .trim_start_matches("q=")
        .to_owned()
}

/// One Lemmy handle served as both a Person (`/u/collision`) and a Group
/// (`/c/collision`): `@` finds the person, `!` the group, a bare handle both.
#[sqlx::test(migrations = "../db/migrations")]
async fn search_disambiguates_lemmy_person_and_group(pool: PgPool) {
    let mut person = RemoteUser::new("lemmy.test", "collision");
    person.actor.id = "https://lemmy.test/u/collision".to_owned();
    person.actor.inbox = "https://lemmy.test/u/collision/inbox".to_owned();
    person.actor.public_key.id = "https://lemmy.test/u/collision#main-key".to_owned();
    person.actor.public_key.owner = "https://lemmy.test/u/collision".to_owned();

    let mut group = RemoteUser::new("lemmy.test", "collision");
    group.actor.kind = "Group".to_owned();
    group.actor.id = "https://lemmy.test/c/collision".to_owned();
    group.actor.inbox = "https://lemmy.test/c/collision/inbox".to_owned();
    group.actor.public_key.id = "https://lemmy.test/c/collision#main-key".to_owned();
    group.actor.public_key.owner = "https://lemmy.test/c/collision".to_owned();

    let stub = StubFederation::with_users(&[&person, &group]);
    let (_, token) = user_with_token(&pool, "searcher", "read write").await;
    let app = || test_app_with(pool.clone(), stub.clone());

    // `@name@host` → the person only.
    let (_, res) = api(
        app(),
        "GET",
        "/api/v2/search?q=%40collision%40lemmy.test&resolve=true&type=accounts",
        Some(&token),
        None,
    )
    .await;
    let accounts = section(&res, "accounts");
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0]["acct"], "collision@lemmy.test");
    assert_eq!(accounts[0]["group"], false);

    // `!name@host` → the group only.
    let (_, res) = api(
        app(),
        "GET",
        "/api/v2/search?q=%21collision%40lemmy.test&resolve=true&type=accounts",
        Some(&token),
        None,
    )
    .await;
    let accounts = section(&res, "accounts");
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0]["group"], true);

    // Bare `name@host` → BOTH, person first.
    let (_, res) = api(
        app(),
        "GET",
        "/api/v2/search?q=collision%40lemmy.test&resolve=true&type=accounts",
        Some(&token),
        None,
    )
    .await;
    let accounts = section(&res, "accounts");
    assert_eq!(accounts.len(), 2, "a bare handle surfaces both actors");
    assert_eq!(accounts[0]["group"], false, "person leads");
    assert_eq!(accounts[1]["group"], true);
}
