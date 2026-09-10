//! FEP-044f quote posts: the consent handshake in all four directions —
//! local↔local, inbound `QuoteRequest`, outbound `QuoteRequest` with
//! Accept/Reject, and verification of third-party quote authorizations.

mod common;

use std::time::SystemTime;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{
    RemoteUser, StubFederation, TEST_DOMAIN, create_local_account, test_app, test_app_with,
    test_state_with,
};
use http_body_util::BodyExt;
use plamenu::auth::{generate_secret, hash_password, hash_secret};
use plamenu::{actions, delivery, remote};
use plamenu_db::notification::NotificationFilter;
use plamenu_db::{PgPool, account, follow, notification, oauth, quote, status, user};
use plamenu_federation::RequestSigner;
use serde_json::{Value, json};
use tower::ServiceExt;

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
    let code = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn get_ap(app: Router, uri: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .uri(uri)
        .header(header::ACCEPT, "application/activity+json")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let code = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn post_signed(app: Router, body: &Value, signer: &RequestSigner) -> StatusCode {
    let bytes = serde_json::to_vec(body).unwrap();
    let signed_headers = signer.sign_post(TEST_DOMAIN, "/inbox", &bytes, SystemTime::now());
    let request = Request::builder()
        .method("POST")
        .uri("/inbox")
        .header("host", signed_headers.host)
        .header("date", signed_headers.date)
        .header("digest", signed_headers.digest)
        .header("signature", signed_headers.signature)
        .header("content-type", "application/activity+json")
        .body(Body::from(bytes))
        .unwrap();
    app.oneshot(request).await.unwrap().status()
}

/// Local user with an access token, via the real OAuth machinery.
async fn user_with_token(pool: &PgPool, username: &str) -> (plamenu_db::account::Account, String) {
    // Mint the app + access token directly; the full OAuth authorization-code
    // flow is covered on its own in `client_api.rs`. Re-driving it per user just
    // added three router builds + HTTP round-trips to every quote test.
    let account = create_local_account(pool, username, username).await;
    let row = user::create(
        pool,
        account.id,
        Some(&format!("{username}@plamenu.test")),
        &hash_password("pw").unwrap(),
    )
    .await
    .unwrap();
    let app = oauth::create_app(
        pool,
        oauth::NewApp {
            name: "quotes",
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

#[sqlx::test(migrations = "../db/migrations")]
async fn local_quote_accepts_notifies_and_serves_a_stamp(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let (carol, carol_token) = user_with_token(&pool, "carol").await;
    let app = || test_app(pool.clone());

    let (_, original) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&alice_token),
        Some(json!({"status": "quote me"})),
    )
    .await;
    assert_eq!(original["quote_approval"]["automatic"][0], "public");
    let original_id = original["id"].as_str().unwrap().to_owned();

    let (code, quoted) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&carol_token),
        Some(json!({"status": "look at this", "quoted_status_id": original_id})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{quoted}");
    assert_eq!(quoted["quote"]["state"], "accepted");
    assert_eq!(quoted["quote"]["quoted_status"]["id"], original_id.as_str());
    assert_eq!(
        quoted["quote"]["quoted_status"]["content"],
        "<p>quote me</p>"
    );
    let quote_post_id: i64 = quoted["id"].as_str().unwrap().parse().unwrap();

    // Alice was notified.
    let alice_account = plamenu_db::account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    let items = notification::list(
        &pool,
        alice_account.id,
        None,
        None,
        None,
        NotificationFilter::default(),
        10,
    )
    .await
    .unwrap();
    assert_eq!(items[0].kind, "quote");
    // The notification points at carol's quoting post, not alice's own.
    assert_eq!(items[0].status_id, Some(quote_post_id));

    // Carol's quote is listed under alice's status' quotes.
    let (code, listed) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{original_id}/quotes"),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{listed}");
    assert_eq!(listed[0]["id"], quote_post_id.to_string());

    // The AP Note carries quote + authorization; the stamp dereferences.
    let (_, note) = get_ap(app(), &format!("/users/carol/statuses/{quote_post_id}")).await;
    assert_eq!(
        note["quote"],
        format!("https://plamenu.test/users/alice/statuses/{original_id}")
    );
    assert_eq!(note["quoteUri"], note["quote"]);
    // The served content carries the `RE:` fallback so non-quote-aware servers
    // (e.g. GoToSocial) still show the quote context.
    let content = note["content"].as_str().unwrap();
    assert!(
        content.starts_with(r#"<p class="quote-inline">RE: "#),
        "{content}"
    );
    assert!(
        content.contains(&format!("https://plamenu.test/@alice/{original_id}")),
        "Mastodon's fallback prefers the quoted post's human URL: {content}"
    );
    let stamp_uri = note["quoteAuthorization"].as_str().unwrap().to_owned();
    assert!(
        stamp_uri.contains("/users/alice/quote_authorizations/"),
        "{stamp_uri}"
    );
    let stamp_path = stamp_uri.strip_prefix("https://plamenu.test").unwrap();
    let (code, stamp) = get_ap(app(), stamp_path).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(stamp["type"], "QuoteAuthorization");
    assert_eq!(stamp["attributedTo"], "https://plamenu.test/users/alice");
    assert_eq!(stamp["interactingObject"], note["id"]);
    assert_eq!(stamp["interactionTarget"], note["quote"]);

    // A private post is quotable by nobody but its author (who may always quote
    // themselves, Mastodon-style); a follower who can see it still cannot quote.
    let alice_row = plamenu_db::account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    plamenu_db::follow::create(&pool, carol.id, alice_row.id, None)
        .await
        .unwrap();
    let (_, private_post) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&alice_token),
        Some(json!({"status": "secret", "visibility": "private"})),
    )
    .await;
    assert_eq!(private_post["quote_approval"]["automatic"], json!([]));
    assert_eq!(private_post["quote_approval"]["current_user"], "automatic");
    let (code, _) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&carol_token),
        Some(json!({"status": "ha", "quoted_status_id": private_post["id"]})),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
}

/// A public post whose quote policy is restricted still federates as quotable,
/// but a local quoter who doesn't satisfy the automatic policy is refused at
/// compose time — mirroring the inbound `QuoteRequest` decision.
#[sqlx::test(migrations = "../db/migrations")]
async fn local_quote_honours_followers_and_nobody_policy(pool: PgPool) {
    let (alice, alice_token) = user_with_token(&pool, "alice").await;
    let (carol, carol_token) = user_with_token(&pool, "carol").await;
    let app = || test_app(pool.clone());

    let (_, original) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&alice_token),
        Some(json!({"status": "quote me, maybe"})),
    )
    .await;
    let original_id = original["id"].as_str().unwrap().to_owned();

    // Restrict quoting to alice's followers.
    let (code, _) = api(
        app(),
        "PATCH",
        &format!("/api/v1/statuses/{original_id}/interaction_policy"),
        Some(&alice_token),
        Some(json!({ "quote_approval_policy": "followers" })),
    )
    .await;
    assert_eq!(code, StatusCode::OK);

    // Carol does not yet follow alice: her quote is refused.
    let quote_as_carol = |token: &str| {
        let token = token.to_owned();
        let id = original_id.clone();
        async move {
            api(
                app(),
                "POST",
                "/api/v1/statuses",
                Some(&token),
                Some(json!({"status": "look", "quoted_status_id": id})),
            )
            .await
        }
    };
    let (code, body) = quote_as_carol(&carol_token).await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // Once carol follows alice, the followers policy is satisfied.
    follow::create(&pool, carol.id, alice.id, None)
        .await
        .unwrap();
    let (code, body) = quote_as_carol(&carol_token).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["quote"]["state"], "accepted");

    // "nobody" locks it down even for a follower…
    let (code, _) = api(
        app(),
        "PATCH",
        &format!("/api/v1/statuses/{original_id}/interaction_policy"),
        Some(&alice_token),
        Some(json!({ "quote_approval_policy": "nobody" })),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let (code, body) = quote_as_carol(&carol_token).await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // …but the author may always quote their own post.
    let (code, body) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&alice_token),
        Some(json!({"status": "self-quote", "quoted_status_id": original_id})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["quote"]["state"], "accepted");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_quote_request_is_accepted_for_public_posts(pool: PgPool) {
    let (alice, _token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());

    let (stored, _) = actions::post_status(
        &state,
        actions::PostParams {
            username: "alice",
            text: "public and quotable",
            visibility: "public",
            in_reply_to_id: None,
            media_ids: &[],
            quoted_status_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let quoted_uri = format!("https://plamenu.test/users/alice/statuses/{}", stored.id);
    let instrument_uri = format!("{}/statuses/55", bob.actor.id);

    let request = json!({
        "id": format!("{}#quote_requests/55", bob.actor.id),
        "type": "QuoteRequest",
        "actor": bob.actor.id,
        "object": quoted_uri,
        "instrument": instrument_uri,
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &request,
            &bob.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );

    // Quote row accepted. The notification is held back until the quoting
    // post itself arrives — there is nothing meaningful to point it at yet.
    let row = quote::find_by_status_uri(&pool, &instrument_uri)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, "accepted");
    let quote_notifications = |pool: PgPool| async move {
        let kinds = vec!["quote".to_owned()];
        notification::list(
            &pool,
            alice.id,
            None,
            None,
            None,
            NotificationFilter {
                kinds: Some(&kinds),
                from_account_id: None,
                ..Default::default()
            },
            10,
        )
        .await
        .unwrap()
    };
    assert!(
        quote_notifications(pool.clone()).await.is_empty(),
        "no quote notification before the post lands"
    );

    // The Accept goes back to bob with the stamp as result.
    delivery::run_due(&state).await;
    let sent = stub.deliveries();
    let accept = &sent.last().unwrap().activity;
    assert_eq!(sent.last().unwrap().inbox_url, bob.actor.inbox);
    assert_eq!(accept["type"], "Accept");
    assert_eq!(accept["object"]["type"], "QuoteRequest");
    assert_eq!(accept["object"]["id"], request["id"]);
    assert_eq!(accept["object"]["instrument"], instrument_uri.as_str());
    let stamp_uri = accept["result"].as_str().unwrap().to_owned();
    assert_eq!(
        stamp_uri,
        format!(
            "https://plamenu.test/users/alice/quote_authorizations/{}",
            row.id
        )
    );

    // Bob now delivers the quoting Note itself, carrying the stamp. Ingesting
    // it links the pending quote row and fires alice's notification — pointed
    // at bob's quoting post, not at alice's own quoted post.
    let create = json!({
        "id": format!("{instrument_uri}/activity"),
        "type": "Create",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": instrument_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>look at this</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "quote": quoted_uri,
            "quoteAuthorization": stamp_uri,
        },
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &create,
            &bob.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    let quoting = status::find_by_uri(&pool, &instrument_uri)
        .await
        .unwrap()
        .expect("bob's quoting post was ingested");
    let items = quote_notifications(pool.clone()).await;
    assert_eq!(items.len(), 1, "exactly one quote notification");
    assert_eq!(items[0].kind, "quote");
    assert_eq!(items[0].from_account_id, quoting.account_id);
    assert_eq!(
        items[0].status_id,
        Some(quoting.id),
        "the notification points at the quoting post, not alice's own"
    );

    // Re-delivery of the same Note must not produce a second notification.
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &create,
            &bob.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert_eq!(quote_notifications(pool.clone()).await.len(), 1);

    // The quoting post now shows up under the quoted status' quotes listing.
    let (code, listed) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/statuses/{}/quotes", stored.id),
        None,
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{listed}");
    let ids: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [quoting.id.to_string()]);

    // A private post gets a Reject instead.
    let (private_post, _) = actions::post_status(
        &state,
        actions::PostParams {
            username: "alice",
            text: "not quotable",
            visibility: "private",
            in_reply_to_id: None,
            media_ids: &[],
            quoted_status_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let request = json!({
        "id": format!("{}#quote_requests/56", bob.actor.id),
        "type": "QuoteRequest",
        "actor": bob.actor.id,
        "object": format!("https://plamenu.test/users/alice/statuses/{}", private_post.id),
        "instrument": format!("{}/statuses/56", bob.actor.id),
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &request,
            &bob.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    delivery::run_due(&state).await;
    let reject = &stub.deliveries().last().unwrap().activity.clone();
    assert_eq!(reject["type"], "Reject");
    assert_eq!(reject["object"]["type"], "QuoteRequest");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn outbound_quote_of_remote_post_completes_the_handshake(pool: PgPool) {
    let (_carol, carol_token) = user_with_token(&pool, "carol").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());

    // A remote post from bob we want to quote.
    let remote_account = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    let target = status::upsert_remote(
        &pool,
        status::NewRemoteStatus {
            title: None,
            object_type: None,
            external_url: None,
            uri: "https://remote.example/users/bob/statuses/7",
            account_id: remote_account.id,
            content: "<p>bob's wisdom</p>",
            created_at: time::OffsetDateTime::now_utc(),
            visibility: "public",
            in_reply_to_id: None,
            in_reply_to_uri: None,
            spoiler_text: "",
            sensitive: false,
            language: None,
            url: Some("https://remote.example/@bob/7"),
            // Bob explicitly offers manual review to the public, so creating
            // this quote is allowed but still begins in `pending`.
            quote_approval_policy: plamenu_ap::quote_policy::flag::PUBLIC,
        },
    )
    .await
    .unwrap();

    let (code, posted) = api(
        test_app_with(pool.clone(), stub.clone()),
        "POST",
        "/api/v1/statuses",
        Some(&carol_token),
        Some(json!({"status": "hot take incoming", "quoted_status_id": target.id.to_string()})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{posted}");
    assert_eq!(posted["quote"]["state"], "pending");
    assert!(
        posted["quote"]["quoted_status"].is_null(),
        "pending quotes hide the target"
    );
    let quoting_id: i64 = posted["id"].as_str().unwrap().parse().unwrap();
    let stored_quote = status::find_by_id(&pool, quoting_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        stored_quote
            .content
            .starts_with(r#"<p class="quote-inline">RE: "#),
        "the fallback must be part of the stored body: {}",
        stored_quote.content
    );
    assert!(
        stored_quote
            .content
            .contains("https://remote.example/@bob/7")
    );
    assert!(posted["content"].as_str().unwrap().contains("RE:"));

    // The QuoteRequest reaches bob's inbox with our post inlined.
    delivery::run_due(&state).await;
    let request_delivery = stub
        .deliveries()
        .into_iter()
        .find(|d| d.activity["type"] == "QuoteRequest")
        .expect("QuoteRequest delivered");
    assert_eq!(request_delivery.inbox_url, bob.actor.inbox);
    assert_eq!(
        request_delivery.activity["object"],
        "https://remote.example/users/bob/statuses/7"
    );
    assert_eq!(request_delivery.activity["instrument"]["type"], "Note");
    assert_eq!(
        request_delivery.activity["instrument"]["quote"],
        "https://remote.example/users/bob/statuses/7"
    );
    assert!(
        request_delivery.activity["instrument"]["content"]
            .as_str()
            .unwrap()
            .contains(r#"class="quote-inline""#),
        "the federated body must carry the same fallback"
    );
    assert!(
        request_delivery.activity["instrument"]["content"]
            .as_str()
            .unwrap()
            .contains("https://remote.example/@bob/7")
    );
    let request_uri = request_delivery.activity["id"].as_str().unwrap().to_owned();

    // Bob's server accepts with a stamp on its own host.
    let stamp_uri = format!("{}/quote_authorizations/9", bob.actor.id);
    let accept = json!({
        "id": format!("{}#accepts/quote_requests/9", bob.actor.id),
        "type": "Accept",
        "actor": bob.actor.id,
        "object": {
            "id": request_uri,
            "type": "QuoteRequest",
            "actor": "https://plamenu.test/users/carol",
            "object": "https://remote.example/users/bob/statuses/7",
            "instrument": posted["uri"],
        },
        "result": stamp_uri,
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &accept,
            &bob.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );

    // The quote is accepted and now renders the target.
    let (_, shown) = api(
        test_app_with(pool.clone(), stub.clone()),
        "GET",
        &format!("/api/v1/statuses/{}", posted["id"].as_str().unwrap()),
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(shown["quote"]["state"], "accepted");
    assert_eq!(
        shown["quote"]["quoted_status"]["content"],
        "<p>bob's wisdom</p>"
    );
    assert_eq!(shown["content"], "<p>hot take incoming</p>");

    // An Update with the authorization stamp is re-distributed.
    delivery::run_due(&state).await;
    let update = stub
        .deliveries()
        .into_iter()
        .find(|d| d.activity["type"] == "Update")
        .expect("Update delivered");
    assert_eq!(
        update.activity["object"]["quoteAuthorization"],
        stamp_uri.as_str()
    );
}

/// A policy-less remote post and one that explicitly limits automatic quote
/// approval to its author are both non-quotable. Refusing before status/quote
/// creation prevents an eternal `pending` row for non-quote-aware servers.
#[sqlx::test(migrations = "../db/migrations")]
async fn outbound_quote_requires_remote_permission(pool: PgPool) {
    let (_carol, carol_token) = user_with_token(&pool, "carol").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let remote_account = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    let app = || test_app(pool.clone());

    for (suffix, policy) in [
        ("missing", 0),
        ("disabled", plamenu_ap::quote_policy::flag::DISABLED << 16),
    ] {
        let target = status::upsert_remote(
            &pool,
            status::NewRemoteStatus {
                title: None,
                object_type: None,
                external_url: None,
                uri: &format!("https://remote.example/users/bob/statuses/{suffix}"),
                account_id: remote_account.id,
                content: "<p>not quotable</p>",
                created_at: time::OffsetDateTime::now_utc(),
                visibility: "public",
                in_reply_to_id: None,
                in_reply_to_uri: None,
                spoiler_text: "",
                sensitive: false,
                language: None,
                url: None,
                quote_approval_policy: policy,
            },
        )
        .await
        .unwrap();

        let (code, body) = api(
            app(),
            "POST",
            "/api/v1/statuses",
            Some(&carol_token),
            Some(json!({
                "status": "this must not be parked",
                "quoted_status_id": target.id.to_string()
            })),
        )
        .await;
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY, "{suffix}: {body}");
    }

    let pending = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM quotes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(pending, 0, "a denied attempt must create no quote row");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn third_party_quotes_are_verified_via_the_stamp(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let dave = RemoteUser::new("elsewhere.example", "dave");
    let stub = StubFederation::with_actors([bob.actor.clone(), dave.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());

    // Bob's post exists locally.
    let bob_note_uri = format!("{}/statuses/40", bob.actor.id);
    let bob_web_url = "https://remote.example/@bob/40";
    let bob_create = json!({
        "type": "Create",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": bob_note_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>worth quoting</p>",
            "url": bob_web_url,
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
        },
    });
    assert_eq!(
        post_signed(app(), &bob_create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );

    // Dave quotes bob with a valid stamp hosted on bob's server.
    let dave_note_uri = format!("{}/statuses/41", dave.actor.id);
    let stamp_uri = format!("{}/quote_authorizations/41", bob.actor.id);
    stub.objects.lock().unwrap().insert(
        stamp_uri.clone(),
        json!({
            "id": stamp_uri,
            "type": "QuoteAuthorization",
            "attributedTo": bob.actor.id,
            "interactingObject": dave_note_uri,
            "interactionTarget": bob_note_uri,
        }),
    );
    // Mastodon bakes a `<p class="quote-inline">RE: …</p>` fallback into the
    // content of every quote post. Some compatible servers link the target's
    // human web URL there even though the structural quote names its distinct
    // ActivityPub id. Plamenu keeps it at ingest (only the marker class is
    // dropped by the sanitizer) so a quote still shows its context when native
    // rendering is pending or denied.
    let dave_create = json!({
        "type": "Create",
        "actor": dave.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": dave_note_uri,
            "type": "Note",
            "attributedTo": dave.actor.id,
            "content": format!(
                "<p class=\"quote-inline\">RE: <a href=\"{bob_web_url}\">{bob_web_url}</a></p><p>bob said this</p>"
            ),
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "quote": bob_note_uri,
            "quoteAuthorization": stamp_uri,
        },
    });
    assert_eq!(
        post_signed(app(), &dave_create, &dave.signer()).await,
        StatusCode::ACCEPTED
    );

    let dave_status = status::find_by_uri(&pool, &dave_note_uri)
        .await
        .unwrap()
        .unwrap();
    let (_, shown) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}", dave_status.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(shown["quote"]["state"], "accepted");
    assert_eq!(
        shown["quote"]["quoted_status"]["content"],
        "<p>worth quoting</p>"
    );
    // A fully accepted, natively-rendered quote drops the compatibility link
    // from client-facing text while leaving the author's content intact.
    let dave_content = shown["content"].as_str().unwrap();
    assert_eq!(dave_content, "<p>bob said this</p>");

    // If the accepted target later becomes unavailable, no native card can be
    // shown and the stored compatibility link becomes useful again.
    sqlx::query("UPDATE quotes SET quoted_status_id = NULL WHERE status_id = $1")
        .bind(dave_status.id)
        .execute(&pool)
        .await
        .unwrap();
    let (_, unavailable) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}", dave_status.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(unavailable["quote"]["state"], "deleted");
    let unavailable_content = unavailable["content"].as_str().unwrap();
    assert!(unavailable_content.contains("RE:"), "{unavailable_content}");
    assert!(
        unavailable_content.contains(bob_web_url),
        "{unavailable_content}"
    );

    // A forged stamp (wrong interactionTarget) leaves the quote pending.
    let forged_note_uri = format!("{}/statuses/42", dave.actor.id);
    let forged_stamp_uri = format!("{}/quote_authorizations/42", bob.actor.id);
    stub.objects.lock().unwrap().insert(
        forged_stamp_uri.clone(),
        json!({
            "id": forged_stamp_uri,
            "type": "QuoteAuthorization",
            "attributedTo": bob.actor.id,
            "interactingObject": forged_note_uri,
            "interactionTarget": "https://remote.example/users/bob/statuses/other",
        }),
    );
    let forged_create = json!({
        "type": "Create",
        "actor": dave.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": forged_note_uri,
            "type": "Note",
            "attributedTo": dave.actor.id,
            "content": format!(
                "<p class=\"quote-inline\">RE: <a href=\"{bob_note_uri}\">{bob_note_uri}</a></p><p>unauthorized quote</p>"
            ),
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "quote": bob_note_uri,
            "quoteAuthorization": forged_stamp_uri,
        },
    });
    assert_eq!(
        post_signed(app(), &forged_create, &dave.signer()).await,
        StatusCode::ACCEPTED
    );
    let forged_status = status::find_by_uri(&pool, &forged_note_uri)
        .await
        .unwrap()
        .unwrap();
    let (_, shown) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}", forged_status.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(shown["quote"]["state"], "pending");
    assert!(shown["quote"]["quoted_status"].is_null());
    let pending_content = shown["content"].as_str().unwrap();
    assert!(pending_content.contains("RE:"), "{pending_content}");
    assert!(pending_content.contains(&bob_note_uri), "{pending_content}");

    // Self-quotes need no stamp at all.
    let self_note_uri = format!("{}/statuses/43", bob.actor.id);
    let self_create = json!({
        "type": "Create",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": self_note_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>quoting myself</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "quote": bob_note_uri,
        },
    });
    assert_eq!(
        post_signed(app(), &self_create, &bob.signer()).await,
        StatusCode::ACCEPTED
    );
    let self_status = status::find_by_uri(&pool, &self_note_uri)
        .await
        .unwrap()
        .unwrap();
    let row = quote::for_statuses(&pool, &[self_status.id]).await.unwrap();
    assert_eq!(row[&self_status.id].state, "accepted");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn third_party_quote_fetches_missing_quoted_status_before_accepting(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let mastodon = RemoteUser::new("mastodon.social", "mastodon");
    let poleguy = RemoteUser::new("mastodon.social", "poleguy");
    let stub = StubFederation::with_actors([mastodon.actor.clone(), poleguy.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());

    let quoted_uri = format!("{}/statuses/116840233192142775", poleguy.actor.id);
    stub.objects.lock().unwrap().insert(
        quoted_uri.clone(),
        json!({
            "id": quoted_uri,
            "type": "Note",
            "attributedTo": poleguy.actor.id,
            "content": "<p>I have a full-featured Mastodon client on iOS.</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "interactionPolicy": {
                "canQuote": {
                    "automaticApproval": ["https://www.w3.org/ns/activitystreams#Public"]
                }
            },
        }),
    );

    let quote_uri = format!("{}/statuses/116840458345748131", mastodon.actor.id);
    let stamp_uri = format!(
        "{}/quote_authorizations/116840458345720044",
        poleguy.actor.id
    );
    stub.objects.lock().unwrap().insert(
        stamp_uri.clone(),
        json!({
            "id": stamp_uri,
            "type": "QuoteAuthorization",
            "attributedTo": poleguy.actor.id,
            "interactingObject": quote_uri,
            "interactionTarget": quoted_uri,
        }),
    );
    let create = json!({
        "type": "Create",
        "actor": mastodon.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": quote_uri,
            "type": "Note",
            "attributedTo": mastodon.actor.id,
            "content": format!(
                "<p class=\"quote-inline\">RE: <a href=\"{quoted_uri}\">{quoted_uri}</a></p><p>Test and provide feedback.</p>"
            ),
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "quote": quoted_uri,
            "_misskey_quote": quoted_uri,
            "quoteUri": quoted_uri,
            "quoteAuthorization": stamp_uri,
        },
    });
    assert_eq!(
        post_signed(app(), &create, &mastodon.signer()).await,
        StatusCode::ACCEPTED
    );

    let quote_status = status::find_by_uri(&pool, &quote_uri)
        .await
        .unwrap()
        .unwrap();
    let quoted_status = status::find_by_uri(&pool, &quoted_uri)
        .await
        .unwrap()
        .unwrap();
    let rows = quote::for_statuses(&pool, &[quote_status.id])
        .await
        .unwrap();
    let row = &rows[&quote_status.id];
    assert_eq!(row.state, "accepted");
    assert_eq!(row.quoted_status_id, Some(quoted_status.id));

    let (_, shown) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}", quote_status.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(shown["quote"]["state"], "accepted");
    assert_eq!(
        shown["quote"]["quoted_status"]["content"],
        "<p>I have a full-featured Mastodon client on iOS.</p>"
    );
    let quote_content = shown["content"].as_str().unwrap();
    assert_eq!(quote_content, "<p>Test and provide feedback.</p>");
    let fetches = stub.fetches();
    assert!(fetches.contains(&quoted_uri));
    assert!(fetches.contains(&stamp_uri));
    assert!(fetches.contains(&poleguy.actor.id));

    let missing_uri = format!("{}/statuses/missing", poleguy.actor.id);
    let missing_quote_uri = format!("{}/statuses/116840458345748132", mastodon.actor.id);
    let missing_stamp_uri = format!(
        "{}/quote_authorizations/116840458345720045",
        poleguy.actor.id
    );
    let missing_create = json!({
        "type": "Create",
        "actor": mastodon.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": missing_quote_uri,
            "type": "Note",
            "attributedTo": mastodon.actor.id,
            "content": "<p>unreachable quote target</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "quote": missing_uri,
            "quoteAuthorization": missing_stamp_uri,
        },
    });
    assert_eq!(
        post_signed(app(), &missing_create, &mastodon.signer()).await,
        StatusCode::ACCEPTED
    );
    let missing_quote_status = status::find_by_uri(&pool, &missing_quote_uri)
        .await
        .unwrap()
        .unwrap();
    let rows = quote::for_statuses(&pool, &[missing_quote_status.id])
        .await
        .unwrap();
    let row = &rows[&missing_quote_status.id];
    assert_eq!(row.state, "pending");
    assert!(row.quoted_status_id.is_none());
    assert!(stub.fetches().contains(&missing_uri));
}

/// Regression for a real staging failure: Plamenu first learns about a quote
/// post because somebody boosts it. Resolving the unknown boost target is not
/// a live Create delivery, but the fetched Note's quote edge is still canonical
/// object structure and must be persisted and exposed to clients.
#[sqlx::test(migrations = "../db/migrations")]
async fn person_announce_of_unknown_quote_preserves_the_quote_attachment(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let quoter = RemoteUser::new("mastodon.example", "ashed");
    let quoted_author = RemoteUser::new("mstdn.example", "free_press");
    let booster = RemoteUser::new("mastodon.example", "booster");
    let stub = StubFederation::with_actors([
        quoter.actor.clone(),
        quoted_author.actor.clone(),
        booster.actor.clone(),
    ]);
    let app = || test_app_with(pool.clone(), stub.clone());

    let quoted_uri = format!("{}/statuses/117088023784793722", quoted_author.actor.id);
    stub.objects.lock().unwrap().insert(
        quoted_uri.clone(),
        json!({
            "id": quoted_uri,
            "type": "Note",
            "attributedTo": quoted_author.actor.id,
            "content": "<p>TOTAL SOLAR ECLIPSE</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
        }),
    );

    let quote_uri = format!("{}/statuses/117088482229700865", quoter.actor.id);
    let stamp_uri = format!(
        "{}/quote_authorizations/117088482290950181",
        quoted_author.actor.id
    );
    stub.objects.lock().unwrap().insert(
        stamp_uri.clone(),
        json!({
            "id": stamp_uri,
            "type": "QuoteAuthorization",
            "attributedTo": quoted_author.actor.id,
            "interactingObject": quote_uri,
            "interactionTarget": quoted_uri,
        }),
    );
    stub.objects.lock().unwrap().insert(
        quote_uri.clone(),
        json!({
            "id": quote_uri,
            "type": "Note",
            "attributedTo": quoter.actor.id,
            "content": format!(
                "<p class=\"quote-inline\">RE: <a href=\"{quoted_uri}\">{quoted_uri}</a></p><p>Have you some photos of this?</p>"
            ),
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "quote": quoted_uri,
            "_misskey_quote": quoted_uri,
            "quoteUri": quoted_uri,
            "quoteAuthorization": stamp_uri,
        }),
    );

    let announce = json!({
        "id": format!("{}/announces/1", booster.actor.id),
        "type": "Announce",
        "actor": booster.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": quote_uri,
    });
    assert_eq!(
        post_signed(app(), &announce, &booster.signer()).await,
        StatusCode::ACCEPTED
    );

    let quoting = status::find_by_uri(&pool, &quote_uri)
        .await
        .unwrap()
        .expect("the unknown boost target is fetched");
    assert_eq!(
        status::ingest_provenance(&pool, quoting.id)
            .await
            .unwrap()
            .as_deref(),
        Some("explicit_resolution")
    );
    let quoted = status::find_by_uri(&pool, &quoted_uri)
        .await
        .unwrap()
        .expect("quote verification fetches the quoted post");
    let rows = quote::for_statuses(&pool, &[quoting.id]).await.unwrap();
    let row = &rows[&quoting.id];
    assert_eq!(row.state, "accepted");
    assert_eq!(row.quoted_status_id, Some(quoted.id));

    let booster_account = account::find_by_uri(&pool, &booster.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        status::find_reblog_by(&pool, booster_account.id, quoting.id)
            .await
            .unwrap()
            .is_some(),
        "the Announce still creates the boost row"
    );

    let (_, shown) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}", quoting.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(shown["quote"]["state"], "accepted");
    assert_eq!(shown["quote"]["quoted_status"]["id"], quoted.id.to_string());
    assert_eq!(
        shown["quote"]["quoted_status"]["content"],
        "<p>TOTAL SOLAR ECLIPSE</p>"
    );

    let fetches = stub.fetches();
    assert!(fetches.contains(&quote_uri));
    assert!(fetches.contains(&quoted_uri));
    assert!(fetches.contains(&stamp_uri));
}

/// Posts a public status as `username` through the real action path.
async fn post_public(state: &plamenu::AppState, username: &str, text: &str) -> status::Status {
    actions::post_status(
        state,
        actions::PostParams {
            username,
            text,
            visibility: "public",
            in_reply_to_id: None,
            media_ids: &[],
            quoted_status_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .0
}

#[sqlx::test(migrations = "../db/migrations")]
async fn interaction_policy_updates_quote_approval_and_refederates(pool: PgPool) {
    let (alice, alice_token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let stored = post_public(&state, "alice", "quotable").await;

    // A remote follower, so the policy change actually fans out.
    let bob_account = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    follow::create(&pool, bob_account.id, alice.id, None)
        .await
        .unwrap();

    // Restrict quoting to followers.
    let (code, body) = api(
        test_app_with(pool.clone(), stub.clone()),
        "PATCH",
        &format!("/api/v1/statuses/{}/interaction_policy", stored.id),
        Some(&alice_token),
        Some(json!({ "quote_approval_policy": "followers" })),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["quote_approval"]["automatic"], json!(["followers"]));
    // The author may always quote themselves.
    assert_eq!(body["quote_approval"]["current_user"], "automatic");

    // The Update carries the new interactionPolicy to followers.
    delivery::run_due(&state).await;
    let update = stub
        .deliveries()
        .into_iter()
        .rev()
        .find(|d| d.activity["type"] == "Update")
        .expect("an Update(Note) was federated");
    assert!(update.inbox_url.contains("remote.example"));
    assert_eq!(
        update.activity["object"]["interactionPolicy"]["canQuote"]["automaticApproval"],
        json!(["https://plamenu.test/users/alice/followers"])
    );

    // The AP Note served at its own URL reflects the policy too.
    let (_, note) = get_ap(
        test_app(pool.clone()),
        &format!("/users/alice/statuses/{}", stored.id),
    )
    .await;
    assert_eq!(
        note["interactionPolicy"]["canQuote"]["automaticApproval"],
        json!(["https://plamenu.test/users/alice/followers"])
    );

    // "nobody" clears every automatic audience.
    let (code, body) = api(
        test_app_with(pool.clone(), stub.clone()),
        "PATCH",
        &format!("/api/v1/statuses/{}/interaction_policy", stored.id),
        Some(&alice_token),
        Some(json!({ "quote_approval_policy": "nobody" })),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["quote_approval"]["automatic"], json!([]));

    // An unknown value is rejected.
    let (code, _) = api(
        test_app_with(pool.clone(), stub.clone()),
        "PATCH",
        &format!("/api/v1/statuses/{}/interaction_policy", stored.id),
        Some(&alice_token),
        Some(json!({ "quote_approval_policy": "everyone" })),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn followers_only_policy_gates_inbound_quote_requests(pool: PgPool) {
    let (alice, _token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let stored = post_public(&state, "alice", "followers only").await;
    status::update_quote_policy(
        &pool,
        stored.id,
        alice.id,
        plamenu_ap::quote_policy::AUTOMATIC_FOLLOWERS,
    )
    .await
    .unwrap();
    let quoted_uri = format!("https://plamenu.test/users/alice/statuses/{}", stored.id);

    // A stranger's QuoteRequest is rejected.
    let request = json!({
        "id": format!("{}#quote_requests/70", bob.actor.id),
        "type": "QuoteRequest",
        "actor": bob.actor.id,
        "object": quoted_uri,
        "instrument": format!("{}/statuses/70", bob.actor.id),
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &request,
            &bob.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    delivery::run_due(&state).await;
    assert_eq!(
        stub.deliveries().last().unwrap().activity["type"],
        "Reject",
        "a non-follower may not quote a followers-only post"
    );

    // Bob now follows alice; a fresh QuoteRequest is accepted with a stamp.
    let bob_account = account::find_by_uri(&pool, &bob.actor.id)
        .await
        .unwrap()
        .expect("bob was resolved while handling his QuoteRequest");
    follow::create(&pool, bob_account.id, alice.id, None)
        .await
        .unwrap();
    let request = json!({
        "id": format!("{}#quote_requests/71", bob.actor.id),
        "type": "QuoteRequest",
        "actor": bob.actor.id,
        "object": quoted_uri,
        "instrument": format!("{}/statuses/71", bob.actor.id),
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &request,
            &bob.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    delivery::run_due(&state).await;
    let accept = stub.deliveries().last().unwrap().activity.clone();
    assert_eq!(accept["type"], "Accept", "a follower may quote");
    assert!(
        accept["result"]
            .as_str()
            .unwrap()
            .contains("/quote_authorizations/")
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn revoking_a_quote_withdraws_the_authorization(pool: PgPool) {
    let (alice, alice_token) = user_with_token(&pool, "alice").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let stored = post_public(&state, "alice", "go ahead and quote").await;
    let quoted_uri = format!("https://plamenu.test/users/alice/statuses/{}", stored.id);
    let instrument_uri = format!("{}/statuses/80", bob.actor.id);

    // Bob's quote is requested, authorized, and the quoting Note ingested.
    let request = json!({
        "id": format!("{}#quote_requests/80", bob.actor.id),
        "type": "QuoteRequest",
        "actor": bob.actor.id,
        "object": quoted_uri,
        "instrument": instrument_uri,
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &request,
            &bob.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    delivery::run_due(&state).await;
    let stamp_uri = stub.deliveries().last().unwrap().activity["result"]
        .as_str()
        .unwrap()
        .to_owned();
    let create = json!({
        "id": format!("{instrument_uri}/activity"),
        "type": "Create",
        "actor": bob.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": instrument_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>look</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "quote": quoted_uri,
            "quoteAuthorization": stamp_uri,
        },
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &create,
            &bob.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    let quoting = status::find_by_uri(&pool, &instrument_uri)
        .await
        .unwrap()
        .unwrap();

    // Alice revokes the quote.
    let (code, body) = api(
        test_app_with(pool.clone(), stub.clone()),
        "POST",
        &format!(
            "/api/v1/statuses/{}/quotes/{}/revoke",
            stored.id, quoting.id
        ),
        Some(&alice_token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    // The response is the quoting status, its quote now revoked.
    assert_eq!(body["id"], quoting.id.to_string());
    assert_eq!(body["quote"]["state"], "revoked");
    let row = quote::for_statuses(&pool, &[quoting.id]).await.unwrap();
    assert_eq!(row[&quoting.id].state, "revoked");

    // A Delete(QuoteAuthorization) is sent to bob.
    delivery::run_due(&state).await;
    let delete = stub
        .deliveries()
        .into_iter()
        .rev()
        .find(|d| d.activity["type"] == "Delete")
        .expect("a stamp deletion was federated");
    assert_eq!(delete.inbox_url, bob.actor.inbox);
    assert_eq!(delete.activity["object"]["type"], "QuoteAuthorization");
    assert_eq!(delete.activity["actor"], "https://plamenu.test/users/alice");
    let _ = alice;

    // The quote no longer appears under the status' quotes listing.
    let (code, listed) = api(
        test_app(pool.clone()),
        "GET",
        &format!("/api/v1/statuses/{}/quotes", stored.id),
        None,
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        listed.as_array().unwrap().len(),
        0,
        "revoked quotes are not listed"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn create_accepts_quote_approval_policy(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let app = || test_app(pool.clone());

    // The explicit param wins over the (public) preference default.
    let (code, body) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "followers may quote", "quote_approval_policy": "followers"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["quote_approval"]["automatic"], json!(["followers"]));
    let id: i64 = body["id"].as_str().unwrap().parse().unwrap();
    let stored = status::find_by_id(&pool, id).await.unwrap().unwrap();
    assert_eq!(
        stored.quote_approval_policy,
        plamenu_ap::quote_policy::AUTOMATIC_FOLLOWERS
    );
    // The AP Note federates the create-time policy.
    let (_, note) = get_ap(app(), &format!("/users/alice/statuses/{id}")).await;
    assert_eq!(
        note["interactionPolicy"]["canQuote"]["automaticApproval"],
        json!(["https://plamenu.test/users/alice/followers"])
    );

    // "nobody" clears every automatic audience.
    let (code, body) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "nobody may quote", "quote_approval_policy": "nobody"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["quote_approval"]["automatic"], json!([]));

    // An unknown value is a 422, like Mastodon's InteractionPoliciesConcern.
    let (code, _) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "nope", "quote_approval_policy": "everyone"})),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);

    // A non-distributable post is downgraded to nobody whatever the param
    // says (Mastodon's `downgrade_quote_policy`).
    let (code, body) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "private", "visibility": "private",
                    "quote_approval_policy": "public"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let id: i64 = body["id"].as_str().unwrap().parse().unwrap();
    let stored = status::find_by_id(&pool, id).await.unwrap().unwrap();
    assert_eq!(stored.quote_approval_policy, 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn edit_applies_quote_approval_policy_only_when_present(pool: PgPool) {
    let (_alice, token) = user_with_token(&pool, "alice").await;
    let app = || test_app(pool.clone());

    let (_, body) = api(
        app(),
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "editable"})),
    )
    .await;
    let id = body["id"].as_str().unwrap().to_owned();
    assert_eq!(body["quote_approval"]["automatic"], json!(["public"]));

    // A policy-only edit applies (and is a real edit: edited_at is set).
    let (code, body) = api(
        app(),
        "PUT",
        &format!("/api/v1/statuses/{id}"),
        Some(&token),
        Some(json!({"quote_approval_policy": "followers"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["quote_approval"]["automatic"], json!(["followers"]));
    assert!(body["edited_at"].is_string(), "{body}");

    // Absent param keeps the current policy — no preference fallback on PUT.
    let (code, body) = api(
        app(),
        "PUT",
        &format!("/api/v1/statuses/{id}"),
        Some(&token),
        Some(json!({"status": "edited text"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["quote_approval"]["automatic"], json!(["followers"]));

    // An unknown value is a 422.
    let (code, _) = api(
        app(),
        "PUT",
        &format!("/api/v1/statuses/{id}"),
        Some(&token),
        Some(json!({"quote_approval_policy": "everyone"})),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
}

/// The inbound mirror of `revoking_a_quote_withdraws_the_authorization`: the
/// remote quoted author deletes the `QuoteAuthorization` stamp it issued for a
/// local quote (M31), which revokes the quote and re-federates the quoting
/// post without its stamp.
#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_stamp_deletion_revokes_a_local_quote(pool: PgPool) {
    let (carol, carol_token) = user_with_token(&pool, "carol").await;
    let bob = RemoteUser::new("remote.example", "bob");
    let mallory = RemoteUser::new("evil.example", "mallory");
    let stub = StubFederation::with_actors([bob.actor.clone(), mallory.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());

    let remote_account = remote::store_remote_actor(&pool, &bob.actor).await.unwrap();
    // Bob follows carol, so the post-revocation re-federation has an audience.
    follow::create(&pool, remote_account.id, carol.id, None)
        .await
        .unwrap();
    let target = status::upsert_remote(
        &pool,
        status::NewRemoteStatus {
            title: None,
            object_type: None,
            external_url: None,
            uri: "https://remote.example/users/bob/statuses/7",
            account_id: remote_account.id,
            content: "<p>bob's wisdom</p>",
            created_at: time::OffsetDateTime::now_utc(),
            visibility: "public",
            in_reply_to_id: None,
            in_reply_to_uri: None,
            spoiler_text: "",
            sensitive: false,
            language: None,
            url: None,
            quote_approval_policy: plamenu_ap::quote_policy::flag::PUBLIC,
        },
    )
    .await
    .unwrap();

    // Carol quotes bob; bob's server accepts with a stamp.
    let (code, posted) = api(
        test_app_with(pool.clone(), stub.clone()),
        "POST",
        "/api/v1/statuses",
        Some(&carol_token),
        Some(json!({"status": "hot take", "quoted_status_id": target.id.to_string()})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{posted}");
    delivery::run_due(&state).await;
    let request_uri = stub
        .deliveries()
        .into_iter()
        .find(|d| d.activity["type"] == "QuoteRequest")
        .expect("QuoteRequest delivered")
        .activity["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let stamp_uri = format!("{}/quote_authorizations/9", bob.actor.id);
    let accept = json!({
        "id": format!("{}#accepts/quote_requests/9", bob.actor.id),
        "type": "Accept",
        "actor": bob.actor.id,
        "object": {
            "id": request_uri,
            "type": "QuoteRequest",
            "actor": "https://plamenu.test/users/carol",
            "object": "https://remote.example/users/bob/statuses/7",
            "instrument": posted["uri"],
        },
        "result": stamp_uri,
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &accept,
            &bob.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    let quoting_id: i64 = posted["id"].as_str().unwrap().parse().unwrap();
    let row = quote::for_statuses(&pool, &[quoting_id]).await.unwrap();
    assert_eq!(row[&quoting_id].state, "accepted");

    // Someone else deleting the stamp revokes nothing.
    let forged = json!({
        "id": format!("{stamp_uri}#delete"),
        "type": "Delete",
        "actor": mallory.actor.id,
        "object": { "id": stamp_uri, "type": "QuoteAuthorization" },
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &forged,
            &mallory.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    let row = quote::for_statuses(&pool, &[quoting_id]).await.unwrap();
    assert_eq!(
        row[&quoting_id].state, "accepted",
        "a foreign Delete is ignored"
    );

    // Bob withdraws his consent.
    let revoke = json!({
        "id": format!("{stamp_uri}#delete"),
        "type": "Delete",
        "actor": bob.actor.id,
        "object": { "id": stamp_uri, "type": "QuoteAuthorization" },
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &revoke,
            &bob.signer()
        )
        .await,
        StatusCode::ACCEPTED
    );
    let row = quote::for_statuses(&pool, &[quoting_id]).await.unwrap();
    assert_eq!(row[&quoting_id].state, "revoked");
    assert!(row[&quoting_id].approval_uri.is_none());
    let (_, shown) = api(
        test_app_with(pool.clone(), stub.clone()),
        "GET",
        &format!("/api/v1/statuses/{quoting_id}"),
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(shown["quote"]["state"], "revoked");

    // The quoting post is re-federated without its stamp so remotes drop
    // the embed.
    delivery::run_due(&state).await;
    let update = stub
        .deliveries()
        .into_iter()
        .filter(|d| d.activity["type"] == "Update")
        .find(|d| d.activity["object"]["quoteAuthorization"].is_null())
        .expect("the quoting post was re-federated without its stamp");
    assert_eq!(update.activity["object"]["id"], posted["uri"]);
}

/// Mastodon distributes a remote-to-remote quote's Create *before* the
/// `QuoteRequest` handshake finishes — no `quoteAuthorization` yet — and
/// delivers the stamp afterwards in an Update whose visible content is
/// unchanged. That implicit Update must upgrade the pending quote (this froze
/// real universeodon quotes at `pending` on staging).
#[sqlx::test(migrations = "../db/migrations")]
async fn implicit_update_with_stamp_upgrades_pending_quote(pool: PgPool) {
    let bob = RemoteUser::new("remote.example", "bob");
    let dave = RemoteUser::new("elsewhere.example", "dave");
    let stub = StubFederation::with_actors([bob.actor.clone(), dave.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());

    let bob_note_uri = format!("{}/statuses/50", bob.actor.id);
    let dave_note_uri = format!("{}/statuses/51", dave.actor.id);
    let stamp_uri = format!("{}/quote_authorizations/51", bob.actor.id);
    stub.objects.lock().unwrap().insert(
        bob_note_uri.clone(),
        json!({
            "id": bob_note_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>quotable</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
        }),
    );

    // The pre-handshake Create: quote fields, no stamp.
    let create = json!({
        "type": "Create",
        "actor": dave.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": dave_note_uri,
            "type": "Note",
            "attributedTo": dave.actor.id,
            "content": "<p>look at this</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "quote": bob_note_uri,
        },
    });
    assert_eq!(
        post_signed(app(), &create, &dave.signer()).await,
        StatusCode::ACCEPTED
    );
    let dave_status = status::find_by_uri(&pool, &dave_note_uri)
        .await
        .unwrap()
        .unwrap();
    let row = &quote::for_statuses(&pool, &[dave_status.id]).await.unwrap()[&dave_status.id];
    assert_eq!(row.state, "pending");
    assert_eq!(row.quoted_uri.as_deref(), Some(bob_note_uri.as_str()));
    // A background re-verification is queued for it.
    let queued = sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM quote_verify_jobs"#)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(queued, 1);

    // The handshake finished at the origin: the stamp exists…
    stub.objects.lock().unwrap().insert(
        stamp_uri.clone(),
        json!({
            "id": stamp_uri,
            "type": "QuoteAuthorization",
            "attributedTo": bob.actor.id,
            "interactingObject": dave_note_uri,
            "interactionTarget": bob_note_uri,
        }),
    );
    // …and Mastodon sends an otherwise-identical Update carrying it.
    let update = json!({
        "type": "Update",
        "actor": dave.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": dave_note_uri,
            "type": "Note",
            "attributedTo": dave.actor.id,
            "content": "<p>look at this</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "quote": bob_note_uri,
            "quoteAuthorization": stamp_uri,
        },
    });
    assert_eq!(
        post_signed(app(), &update, &dave.signer()).await,
        StatusCode::ACCEPTED
    );

    let row = &quote::for_statuses(&pool, &[dave_status.id]).await.unwrap()[&dave_status.id];
    assert_eq!(row.state, "accepted", "the implicit Update must upgrade it");
    assert_eq!(row.approval_uri.as_deref(), Some(stamp_uri.as_str()));
    assert!(
        row.quoted_status_id.is_some(),
        "quoted post gets linked too"
    );
}

/// A relay may never forward that stamp-carrying Update; the verify worker
/// re-fetches the quoting post from its origin, whose JSON carries the stamp
/// once the handshake completed, and settles the quote.
#[sqlx::test(migrations = "../db/migrations")]
async fn verify_worker_recovers_stamp_from_origin(pool: PgPool) {
    let bob = RemoteUser::new("remote.example", "bob");
    let dave = RemoteUser::new("elsewhere.example", "dave");
    let stub = StubFederation::with_actors([bob.actor.clone(), dave.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = || test_app_with(pool.clone(), stub.clone());

    let bob_note_uri = format!("{}/statuses/60", bob.actor.id);
    let dave_note_uri = format!("{}/statuses/61", dave.actor.id);
    let stamp_uri = format!("{}/quote_authorizations/61", bob.actor.id);
    stub.objects.lock().unwrap().insert(
        bob_note_uri.clone(),
        json!({
            "id": bob_note_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>quotable</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
        }),
    );
    let create = json!({
        "type": "Create",
        "actor": dave.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": dave_note_uri,
            "type": "Note",
            "attributedTo": dave.actor.id,
            "content": "<p>look</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "quote": bob_note_uri,
        },
    });
    assert_eq!(
        post_signed(app(), &create, &dave.signer()).await,
        StatusCode::ACCEPTED
    );
    let dave_status = status::find_by_uri(&pool, &dave_note_uri)
        .await
        .unwrap()
        .unwrap();
    let row = &quote::for_statuses(&pool, &[dave_status.id]).await.unwrap()[&dave_status.id];
    assert_eq!(row.state, "pending");

    // Origin state after the handshake: the post JSON now carries the stamp.
    stub.objects.lock().unwrap().insert(
        dave_note_uri.clone(),
        json!({
            "id": dave_note_uri,
            "type": "Note",
            "attributedTo": dave.actor.id,
            "content": "<p>look</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "quote": bob_note_uri,
            "quoteAuthorization": stamp_uri,
        }),
    );
    stub.objects.lock().unwrap().insert(
        stamp_uri.clone(),
        json!({
            "id": stamp_uri,
            "type": "QuoteAuthorization",
            "attributedTo": bob.actor.id,
            "interactingObject": dave_note_uri,
            "interactionTarget": bob_note_uri,
        }),
    );

    // Pull the scheduled retry forward and run the worker.
    sqlx::query!("UPDATE quote_verify_jobs SET run_at = now()")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(plamenu::quote_verify::run_due(&state).await, 1);

    let row = &quote::for_statuses(&pool, &[dave_status.id]).await.unwrap()[&dave_status.id];
    assert_eq!(row.state, "accepted");
    assert_eq!(row.approval_uri.as_deref(), Some(stamp_uri.as_str()));
    assert!(row.quoted_status_id.is_some());
    let left = sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM quote_verify_jobs"#)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0, "a settled quote leaves no retry job behind");
}

/// Misskey-style legacy quotes name the quoted post via `quoteUrl` (one of
/// Mastodon's four accepted aliases) and never carry a stamp — the quote row
/// must still be created, it stays `pending` like on Mastodon, and (also like
/// Mastodon's `Quote#acceptable?`) the entity omits the quote entirely rather
/// than rendering an eternal "pending" placeholder.
#[sqlx::test(migrations = "../db/migrations")]
async fn legacy_quoteurl_alias_creates_pending_quote(pool: PgPool) {
    let (_alice, alice_token) = user_with_token(&pool, "alice").await;
    let misskey = RemoteUser::new("notes.example", "misskeyuser");
    let other = RemoteUser::new("notes.example", "otheruser");
    let stub = StubFederation::with_actors([misskey.actor.clone(), other.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());

    let quoted_uri = format!("{}/notes/aaa", other.actor.id);
    stub.objects.lock().unwrap().insert(
        quoted_uri.clone(),
        json!({
            "id": quoted_uri,
            "type": "Note",
            "attributedTo": other.actor.id,
            "content": "<p>original</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
        }),
    );
    let note_uri = format!("{}/notes/bbb", misskey.actor.id);
    let create = json!({
        "type": "Create",
        "actor": misskey.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": note_uri,
            "type": "Note",
            "attributedTo": misskey.actor.id,
            "content": "<p>check this</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "quoteUrl": quoted_uri,
        },
    });
    assert_eq!(
        post_signed(app(), &create, &misskey.signer()).await,
        StatusCode::ACCEPTED
    );

    let stored = status::find_by_uri(&pool, &note_uri)
        .await
        .unwrap()
        .unwrap();
    let row = &quote::for_statuses(&pool, &[stored.id]).await.unwrap()[&stored.id];
    assert_eq!(row.state, "pending");
    assert_eq!(row.quoted_uri.as_deref(), Some(quoted_uri.as_str()));
    // Same-origin targets are fetched even without a stamp, so the quoted
    // post links up for the day consent arrives.
    assert!(row.quoted_status_id.is_some());
    assert!(row.legacy);
    // No pointless verification retries: nothing can ever settle a resolved,
    // stamp-less legacy quote.
    let queued = sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM quote_verify_jobs"#)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(queued, 0);

    // The rendered status carries no quote attachment at all — the post
    // shows plain, like on Mastodon, instead of a "pending" placeholder.
    let (_, shown) = api(
        app(),
        "GET",
        &format!("/api/v1/statuses/{}", stored.id),
        Some(&alice_token),
        None,
    )
    .await;
    assert!(
        shown["quote"].is_null(),
        "legacy pending quote must be omitted: {}",
        shown["quote"]
    );
}

/// A `quoteAuthorization` whose stamp is permanently gone at the origin
/// (404/410) rejects the quote — Mastodon's `quote.reject!` on a deleted
/// approval object.
#[sqlx::test(migrations = "../db/migrations")]
async fn permanently_gone_stamp_rejects_the_quote(pool: PgPool) {
    let bob = RemoteUser::new("remote.example", "bob");
    let dave = RemoteUser::new("elsewhere.example", "dave");
    let stub = StubFederation::with_actors([bob.actor.clone(), dave.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());

    let bob_note_uri = format!("{}/statuses/70", bob.actor.id);
    stub.objects.lock().unwrap().insert(
        bob_note_uri.clone(),
        json!({
            "id": bob_note_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>quotable</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
        }),
    );
    let dave_note_uri = format!("{}/statuses/71", dave.actor.id);
    let create = json!({
        "type": "Create",
        "actor": dave.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": dave_note_uri,
            "type": "Note",
            "attributedTo": dave.actor.id,
            "content": "<p>sneaky</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "quote": bob_note_uri,
            // The stub 404s this: the origin never issued (or deleted) it.
            "quoteAuthorization": format!("{}/quote_authorizations/nope", bob.actor.id),
        },
    });
    assert_eq!(
        post_signed(app(), &create, &dave.signer()).await,
        StatusCode::ACCEPTED
    );

    let stored = status::find_by_uri(&pool, &dave_note_uri)
        .await
        .unwrap()
        .unwrap();
    let row = &quote::for_statuses(&pool, &[stored.id]).await.unwrap()[&stored.id];
    assert_eq!(row.state, "rejected");
}
