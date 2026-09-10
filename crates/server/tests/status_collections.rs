//! The `ActivityPub` collections hanging off statuses and actors —
//! `/statuses/{id}/{replies,likes,shares}` and `/collections/featured` —
//! plus the Note and actor documents advertising them.

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use common::{TEST_DOMAIN, create_local_account, test_app, test_state_with};
use http_body_util::BodyExt;
use plamenu::actions::{self, PostParams};
use plamenu_db::account::{self, RemoteAccountData};
use plamenu_db::{PgPool, conversation, favourite, status};
use serde_json::{Value, json};
use tower::ServiceExt;

const AP_JSON: &str = "application/activity+json";
const AS_CONTEXT: &str = "https://www.w3.org/ns/activitystreams";

async fn get_json(app: Router, uri: &str, accept: Option<&str>) -> (StatusCode, HeaderMap, Value) {
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(accept) = accept {
        builder = builder.header(header::ACCEPT, accept);
    }
    let response = app
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, value)
}

/// Posts through the real compose pipeline (statuses get mentions, tags,
/// conversations etc. exactly like API-created ones).
async fn post(
    pool: &PgPool,
    username: &str,
    text: &str,
    visibility: &str,
    in_reply_to_id: Option<i64>,
) -> status::Status {
    actions::post_status(
        &test_state_with(pool.clone(), Arc::default()),
        PostParams {
            username,
            text,
            visibility,
            in_reply_to_id,
            media_ids: &[],
            quoted_status_id: None,
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .0
}

/// A stored remote account without the cost of real key generation.
async fn cheap_remote(pool: &PgPool, username: &str) -> account::Account {
    let uri = format!("https://remote.example/users/{username}");
    account::upsert_remote(
        pool,
        RemoteAccountData {
            username,
            domain: "remote.example",
            uri: &uri,
            display_name: "",
            note: "",
            inbox_url: &format!("{uri}/inbox"),
            shared_inbox_url: "",
            public_key_pem: "pub",
            public_key_id: &format!("{uri}#main-key"),
            avatar_remote_url: None,
            header_remote_url: None,
            avatar_description: "",
            header_description: "",
            created_at: None,
            fields: Vec::new(),
            featured_collection_url: None,
            locked: false,
            also_known_as: &[],
            moved_to_uri: None,
            url: None,
            discoverable: false,
            feature_approval_policy: 0,
            is_bot: false,
            indexable: false,
            show_media: None,
            show_media_replies: None,
            show_featured: None,
            memorial: false,
            actor_type: None,
        },
    )
    .await
    .unwrap()
}

async fn remote_reply(
    pool: &PgPool,
    account_id: i64,
    marker: &str,
    parent_id: i64,
) -> status::Status {
    let stored = status::upsert_remote(
        pool,
        status::NewRemoteStatus {
            title: None,
            object_type: None,
            external_url: None,
            uri: &format!("https://remote.example/statuses/{marker}"),
            account_id,
            content: "<p>remote reply</p>",
            created_at: time::OffsetDateTime::now_utc(),
            visibility: "public",
            in_reply_to_id: Some(parent_id),
            in_reply_to_uri: None,
            spoiler_text: "",
            sensitive: false,
            language: None,
            url: None,
            quote_approval_policy: 0,
        },
    )
    .await
    .unwrap();
    // Thread it into its parent's conversation, as ingestion would.
    conversation::ensure_for_status(
        pool,
        &conversation::EnsureConversation {
            status_id: stored.id,
            account_id,
            in_reply_to_id: Some(parent_id),
            is_reply: true,
            refs: conversation::ContextRefs::default(),
        },
    )
    .await
    .unwrap();
    stored
}

fn status_url(username: &str, id: i64) -> String {
    format!("https://{TEST_DOMAIN}/users/{username}/statuses/{id}")
}

#[sqlx::test(migrations = "../db/migrations")]
async fn featured_collection_is_empty_and_dereferenceable(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;

    let (status, headers, body) = get_json(
        test_app(pool.clone()),
        "/users/alice/collections/featured",
        Some(AP_JSON),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers[header::CONTENT_TYPE],
        "application/activity+json; charset=utf-8"
    );
    // The extended context, since inlined pinned Notes may carry quote terms.
    assert_eq!(body["@context"], plamenu_ap::activity::quote_context());
    assert_eq!(
        body["id"],
        format!("https://{TEST_DOMAIN}/users/alice/collections/featured")
    );
    assert_eq!(body["type"], "OrderedCollection");
    assert_eq!(body["totalItems"], 0);
    assert_eq!(body["orderedItems"], json!([]));

    // The actor document points at it (`toot:featured`).
    let (_, _, actor) = get_json(test_app(pool.clone()), "/users/alice", Some(AP_JSON)).await;
    assert_eq!(actor["featured"], body["id"]);

    // ActivityPub-only, like the actor document.
    let (status, _, _) = get_json(
        test_app(pool.clone()),
        "/users/alice/collections/featured",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_ACCEPTABLE);

    let (status, _, body) = get_json(
        test_app(pool.clone()),
        "/users/ghost/collections/featured",
        Some(AP_JSON),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "Record not found");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn likes_and_shares_collections_carry_counts_only(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let parent = post(&pool, "alice", "count me", "public", None).await;
    favourite::create(&pool, bob.id, parent.id, None)
        .await
        .unwrap();
    status::create_local_reblog(&pool, bob.id, parent.id)
        .await
        .unwrap();

    let base = format!("/users/alice/statuses/{}", parent.id);
    let (status, _, likes) = get_json(
        test_app(pool.clone()),
        &format!("{base}/likes"),
        Some(AP_JSON),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        likes,
        json!({
            "@context": AS_CONTEXT,
            "id": format!("{}/likes", status_url("alice", parent.id)),
            "type": "Collection",
            "totalItems": 1,
        }),
        "count only — favouriting actors are never listed"
    );

    let (_, _, shares) = get_json(
        test_app(pool.clone()),
        &format!("{base}/shares"),
        Some(AP_JSON),
    )
    .await;
    assert_eq!(shares["type"], "Collection");
    assert_eq!(shares["totalItems"], 1);
    assert_eq!(
        shares["id"],
        format!("{}/shares", status_url("alice", parent.id))
    );

    // The Note advertises the same counts.
    let (_, _, note) = get_json(test_app(pool.clone()), &base, Some(AP_JSON)).await;
    assert_eq!(note["likes"]["totalItems"], 1);
    assert_eq!(note["shares"]["totalItems"], 1);
    assert_eq!(note["likes"]["id"], likes["id"]);
    assert_eq!(note["shares"]["id"], shares["id"]);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn replies_collection_pages_like_mastodon(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    create_local_account(&pool, "bob", "Bob").await;
    let carol = cheap_remote(&pool, "carol").await;

    let parent = post(&pool, "alice", "thread root", "public", None).await;
    let self1 = post(&pool, "alice", "self reply 1", "public", Some(parent.id)).await;
    let self2 = post(&pool, "alice", "self reply 2", "unlisted", Some(parent.id)).await;
    // Followers-only replies are not distributable: never listed.
    post(&pool, "alice", "private reply", "private", Some(parent.id)).await;
    let bob_reply = post(&pool, "bob", "from bob", "public", Some(parent.id)).await;
    let carol_reply = remote_reply(&pool, carol.id, "c1", parent.id).await;

    let replies_url = format!("{}/replies", status_url("alice", parent.id));
    let base = format!("/users/alice/statuses/{}/replies", parent.id);

    // The bare URL: a Collection envelope inlining the self-replies page.
    let (status, headers, body) = get_json(test_app(pool.clone()), &base, Some(AP_JSON)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers[header::CONTENT_TYPE],
        "application/activity+json; charset=utf-8"
    );
    assert_eq!(body["type"], "Collection");
    assert_eq!(body["id"], replies_url.as_str());
    assert!(
        !body.as_object().unwrap().contains_key("totalItems"),
        "Mastodon's replies collection carries no size"
    );
    let first = &body["first"];
    assert_eq!(first["id"], format!("{replies_url}?page=true"));
    assert_eq!(first["type"], "CollectionPage");
    assert_eq!(first["partOf"], replies_url.as_str());
    assert_eq!(
        first["next"],
        format!("{replies_url}?only_other_accounts=true&page=true"),
        "few self-replies: next hands over to other accounts"
    );
    assert!(!first.as_object().unwrap().contains_key("@context"));
    let items = first["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "the private self-reply is excluded");
    assert_eq!(items[0]["id"], status_url("alice", self1.id));
    assert_eq!(items[1]["id"], status_url("alice", self2.id));
    assert!(
        items[0].get("@context").is_none(),
        "inlined Notes carry no own context"
    );
    assert_eq!(items[0]["attributedTo"], "https://plamenu.test/users/alice");

    // The other-accounts page: local replies inlined, remote ones as IRIs.
    let (_, _, page) = get_json(
        test_app(pool.clone()),
        &format!("{base}?page=true&only_other_accounts=true"),
        Some(AP_JSON),
    )
    .await;
    assert_eq!(page["type"], "CollectionPage");
    assert_eq!(
        page["id"],
        format!("{replies_url}?only_other_accounts=true&page=true"),
        "page id echoes the request params in Rails' param order"
    );
    assert_eq!(page["partOf"], replies_url.as_str());
    assert!(
        !page.as_object().unwrap().contains_key("next"),
        "short page: no further pages"
    );
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    // Bob's reply is local: a full inlined Note.
    assert!(items[0].is_object());
    assert_eq!(items[0]["id"], status_url("bob", bob_reply.id));
    assert_eq!(items[0]["content"].as_str().unwrap(), "<p>from bob</p>");
    // Carol's is remote: a bare IRI.
    assert_eq!(
        items[1],
        json!(carol_reply.uri.as_ref().unwrap()),
        "remote replies appear as IRIs only"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn replies_pagination_by_min_id(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let parent = post(&pool, "alice", "root", "public", None).await;
    let self1 = post(&pool, "alice", "one", "public", Some(parent.id)).await;
    let self2 = post(&pool, "alice", "two", "public", Some(parent.id)).await;

    let replies_url = format!("{}/replies", status_url("alice", parent.id));
    let (_, _, page) = get_json(
        test_app(pool.clone()),
        &format!(
            "/users/alice/statuses/{}/replies?page=true&min_id={}",
            parent.id, self1.id
        ),
        Some(AP_JSON),
    )
    .await;
    assert_eq!(
        page["id"],
        format!("{replies_url}?min_id={}&page=true", self1.id)
    );
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "min_id pages past the first reply");
    assert_eq!(items[0]["id"], status_url("alice", self2.id));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn status_collections_are_gated_like_the_note(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let public = post(&pool, "alice", "public", "public", None).await;
    let private = post(&pool, "alice", "secret", "private", None).await;
    let boost = status::create_local_reblog(&pool, bob.id, public.id)
        .await
        .unwrap();

    for (username, status_id) in [("alice", private.id), ("bob", boost.id)] {
        for collection in ["replies", "likes", "shares"] {
            let (status, _, body) = get_json(
                test_app(pool.clone()),
                &format!("/users/{username}/statuses/{status_id}/{collection}"),
                Some(AP_JSON),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::NOT_FOUND,
                "{collection} of a non-dereferenceable status ({username}/{status_id})"
            );
            assert_eq!(body["error"], "Record not found");
        }
    }

    // And they are ActivityPub-only.
    for collection in ["replies", "likes", "shares"] {
        let (status, _, _) = get_json(
            test_app(pool.clone()),
            &format!("/users/alice/statuses/{}/{collection}", public.id),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_ACCEPTABLE, "{collection}");
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn note_advertises_self_replies(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let parent = post(&pool, "alice", "root", "public", None).await;
    let reply = post(&pool, "alice", "more", "public", Some(parent.id)).await;

    let replies_url = format!("{}/replies", status_url("alice", parent.id));
    let (_, _, note) = get_json(
        test_app(pool.clone()),
        &format!("/users/alice/statuses/{}", parent.id),
        Some(AP_JSON),
    )
    .await;
    assert_eq!(note["replies"]["id"], replies_url.as_str());
    assert_eq!(note["replies"]["type"], "Collection");
    let first = &note["replies"]["first"];
    assert_eq!(first["type"], "CollectionPage");
    assert_eq!(first["partOf"], replies_url.as_str());
    assert_eq!(
        first["items"],
        json!([status_url("alice", reply.id)]),
        "the advertised page carries bare IRIs, not inlined Notes"
    );
    assert_eq!(
        first["next"],
        format!("{replies_url}?min_id={}&page=true", reply.id)
    );

    // The reply itself has no self-replies: its next jumps to other accounts.
    let (_, _, leaf) = get_json(
        test_app(pool.clone()),
        &format!("/users/alice/statuses/{}", reply.id),
        Some(AP_JSON),
    )
    .await;
    assert_eq!(leaf["replies"]["first"]["items"], json!([]));
    assert_eq!(
        leaf["replies"]["first"]["next"],
        format!(
            "{}/replies?only_other_accounts=true&page=true",
            status_url("alice", reply.id)
        )
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn context_collection_lists_distributable_thread_posts(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    create_local_account(&pool, "bob", "Bob").await;
    let carol = cheap_remote(&pool, "carol").await;

    let root = post(&pool, "alice", "thread root", "public", None).await;
    let self1 = post(&pool, "alice", "unlisted reply", "unlisted", Some(root.id)).await;
    // A followers-only reply is not distributable: never listed in the posts
    // collection (which is public backfill, matching Mastodon).
    post(&pool, "alice", "secret", "private", Some(root.id)).await;
    let bob_reply = post(&pool, "bob", "from bob", "public", Some(root.id)).await;
    let carol_reply = remote_reply(&pool, carol.id, "cc1", root.id).await;

    let conv = conversation::of_status(&pool, root.id)
        .await
        .unwrap()
        .unwrap();
    let context_url = format!("https://{TEST_DOMAIN}/contexts/{conv}");

    let (status, headers, body) = get_json(
        test_app(pool.clone()),
        &format!("/contexts/{conv}"),
        Some(AP_JSON),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers[header::CONTENT_TYPE],
        "application/activity+json; charset=utf-8"
    );
    assert_eq!(body["type"], "OrderedCollection");
    assert_eq!(body["id"], context_url.as_str());
    assert_eq!(
        body["attributedTo"],
        format!("https://{TEST_DOMAIN}/users/alice")
    );
    assert_eq!(body["first"]["partOf"], context_url.as_str());
    let items = body["first"]["orderedItems"].as_array().unwrap();
    assert_eq!(
        items,
        &[
            json!(status_url("alice", root.id)),
            json!(status_url("alice", self1.id)),
            json!(status_url("bob", bob_reply.id)),
            json!(carol_reply.uri.as_ref().unwrap()),
        ],
        "chronological, distributable only — the private reply is excluded"
    );

    // The root Note (and its replies) advertise the collection as `context`.
    for (username, id) in [("alice", root.id), ("bob", bob_reply.id)] {
        let (_, _, note) = get_json(
            test_app(pool.clone()),
            &format!("/users/{username}/statuses/{id}"),
            Some(AP_JSON),
        )
        .await;
        assert_eq!(note["context"], context_url.as_str(), "{username}/{id}");
        assert!(
            note.get("contextHistory").is_none(),
            "no container advertised while the flag is off"
        );
    }

    // ActivityPub-only, like the other collections.
    let (status, _, _) = get_json(test_app(pool.clone()), &format!("/contexts/{conv}"), None).await;
    assert_eq!(status, StatusCode::NOT_ACCEPTABLE);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn context_collection_hidden_for_private_conversation(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let root = post(&pool, "alice", "secret thread", "private", None).await;
    let conv = conversation::of_status(&pool, root.id)
        .await
        .unwrap()
        .unwrap();

    // The posts collection 404s — a private conversation exposes none, and the
    // 404 does not reveal that a private thread exists here.
    let (status, _, body) = get_json(
        test_app(pool.clone()),
        &format!("/contexts/{conv}"),
        Some(AP_JSON),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "Record not found");

    // A conversation we do not have (or do not own) is equally a 404.
    let (status, _, _) = get_json(test_app(pool.clone()), "/contexts/999999", Some(AP_JSON)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn a_status_linking_a_collection_tags_it_in_the_note_and_entity(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let state = test_state_with(pool.clone(), std::sync::Arc::default());

    // A local collection alice owns.
    let collection = plamenu::collections::create_collection(
        &state,
        &alice,
        plamenu::collections::CollectionParams {
            name: "Cool folks".to_owned(),
            description: "a curated set".to_owned(),
            discoverable: true,
            ..Default::default()
        },
        &[],
    )
    .await
    .unwrap();
    let collection_uri = format!(
        "https://{TEST_DOMAIN}/users/alice/collections/{}",
        collection.id
    );
    let collection_web_url = format!("https://{TEST_DOMAIN}/@alice/collections/{}", collection.id);

    // A post whose text links to the collection's web URL.
    let post = post(
        &pool,
        "alice",
        &format!("check out {collection_web_url}"),
        "public",
        None,
    )
    .await;

    // The reference was recorded.
    let recorded = plamenu_db::tagged_object::for_status(&pool, post.id)
        .await
        .unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].collection_id, collection.id);

    // The outbound Note carries a `FeaturedCollection` tag object keyed by the
    // collection's AP id.
    let (status, _, note) = get_json(
        test_app(pool.clone()),
        &format!("/users/alice/statuses/{}", post.id),
        Some(AP_JSON),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tags = note["tag"].as_array().unwrap();
    let featured = tags
        .iter()
        .find(|t| t["type"] == "FeaturedCollection")
        .expect("FeaturedCollection tag present");
    assert_eq!(featured["id"], collection_uri);

    // The REST Status entity carries the full Collection under
    // `tagged_collections`.
    let (status, _, entity) = get_json(
        test_app(pool.clone()),
        &format!("/api/v1/statuses/{}", post.id),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{entity}");
    let tagged = entity["tagged_collections"].as_array().unwrap();
    assert_eq!(tagged.len(), 1);
    assert_eq!(tagged[0]["id"], collection.id.to_string());
    assert_eq!(tagged[0]["name"], "Cool folks");
    assert_eq!(tagged[0]["uri"], collection_uri);

    // Editing the post to drop the link removes the reference.
    plamenu::actions::edit_status(
        &state,
        &alice,
        post.id,
        plamenu::actions::EditParams {
            text: Some("nothing here now"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let after = plamenu_db::tagged_object::for_status(&pool, post.id)
        .await
        .unwrap();
    assert!(after.is_empty(), "link removed on edit");
}
