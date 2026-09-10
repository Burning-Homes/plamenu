//! Consuming remote groups (FEP-1b12). A Group actor's `Announce`
//! delivers community activity in two shapes — a wrapped inner activity
//! (`Announce(Create(Page))`, Lemmy/Mitra) or a bare object (Lemmy's
//! Mastodon-compat duplicate `Announce(Page)`). The inner payload is never
//! trusted as delivered: it needs the author's own FEP-8b32 proof (Mitra) or
//! origin confirmation (Lemmy sends no proofs). Only top-level public posts
//! get a boost row; comments are ingested silently so threads stay complete.

mod common;

use std::sync::Arc;
use std::time::SystemTime;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    RemoteUser, StubFederation, TEST_DOMAIN, create_local_account, test_app_with, test_state_with,
};
use plamenu::actions::{self, PostParams};
use plamenu_db::{PgPool, account, group, media, remote_fetch_failure, remote_group, status};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn post_signed(app: Router, body: &Value, user: &RemoteUser) -> StatusCode {
    let bytes = serde_json::to_vec(body).unwrap();
    let signed_headers = user
        .signer()
        .sign_post(TEST_DOMAIN, "/inbox", &bytes, SystemTime::now());
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

/// A Lemmy-style community: an actor of type `Group` on its own host.
fn community(name: &str) -> RemoteUser {
    let mut group = RemoteUser::new("groups.example", name);
    "Group".clone_into(&mut group.actor.kind);
    group
}

/// The Page `author` serves at their own host, addressed to `group` the way
/// Lemmy addresses community posts (`to: [community, Public]`, `audience`).
fn origin_page(author: &RemoteUser, group: &RemoteUser, content: &str) -> (String, Value) {
    let uri = "https://origin.example/post/7".to_owned();
    let page = json!({
        "id": uri,
        "type": "Page",
        "attributedTo": author.actor.id,
        "name": "Announcing Plamenu 0.5",
        "content": content,
        "to": [group.actor.id, "https://www.w3.org/ns/activitystreams#Public"],
        "cc": [],
        "audience": group.actor.id,
        "published": "2026-07-11T09:00:00Z",
    });
    (uri, page)
}

/// The FEP-1b12 wrapper: `Announce(inner)` from the group, `cc` its
/// followers, exactly as Lemmy fans out community activity.
fn group_announce(group: &RemoteUser, suffix: &str, inner: &Value) -> Value {
    json!({
        "id": format!("{}/activities/announce/{suffix}", group.actor.id),
        "type": "Announce",
        "actor": group.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "cc": [format!("{}/followers", group.actor.id)],
        "object": inner,
        "published": "2026-07-11T09:00:01Z",
    })
}

/// A Lemmy `Create(Page)` inner activity as embedded in the Announce.
fn create_page(author: &RemoteUser, group: &RemoteUser, page: &Value) -> Value {
    json!({
        "id": format!("https://origin.example/activities/create/{}", page["id"].as_str().unwrap().rsplit('/').next().unwrap()),
        "type": "Create",
        "actor": author.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "cc": [group.actor.id],
        "object": page.clone(),
    })
}

/// A group's wrapped `Announce(Create(Page))` is authenticated at the origin:
/// the embedded (here: tampered) copy is never stored, the origin's is; the
/// group's announce becomes the boost row that puts the post in follower
/// timelines. Replay is idempotent.
#[sqlx::test(migrations = "../db/migrations")]
async fn wrapped_create_page_fetches_origin_and_boosts(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let author = RemoteUser::new("origin.example", "bob");
    let group = community("rustlang");
    let stub = StubFederation::with_actors([author.actor.clone(), group.actor.clone()]);
    let (page_uri, page) = origin_page(&author, &group, "<p>the real post</p>");
    stub.objects
        .lock()
        .unwrap()
        .insert(page_uri.clone(), page.clone());

    let mut tampered = page.clone();
    tampered["content"] = json!("<p>a forgery</p>");
    let announce = group_announce(&group, "1", &create_page(&author, &group, &tampered));
    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub.clone()), &announce, &group).await,
        StatusCode::ACCEPTED
    );

    let stored = status::find_by_uri(&pool, &page_uri)
        .await
        .unwrap()
        .expect("the announced post was ingested");
    assert_eq!(stored.content, "<p>the real post</p>", "origin copy wins");
    assert_eq!(stored.title.as_deref(), Some("Announcing Plamenu 0.5"));
    assert!(stub.fetches().contains(&page_uri), "origin was consulted");

    let group_account = account::find_by_uri(&pool, &group.actor.id)
        .await
        .unwrap()
        .expect("group actor stored");
    assert!(group_account.is_group());
    let boost = status::find_reblog_by(&pool, group_account.id, stored.id)
        .await
        .unwrap()
        .expect("the group's announce is a boost row");

    // Replayed delivery: no duplicate boost.
    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub.clone()), &announce, &group).await,
        StatusCode::ACCEPTED
    );
    let replayed = status::find_reblog_by(&pool, group_account.id, stored.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(replayed.id, boost.id);
}

/// Lemmy double-sends every post: the wrapper form plus a Mastodon-compat
/// `Announce(Page)` under a fresh activity id. Whichever arrives second must
/// not create a second boost.
#[sqlx::test(migrations = "../db/migrations")]
async fn compat_announce_page_dedups_against_wrapper(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let author = RemoteUser::new("origin.example", "bob");
    let group = community("rustlang");
    let stub = StubFederation::with_actors([author.actor.clone(), group.actor.clone()]);
    let (page_uri, page) = origin_page(&author, &group, "<p>hello community</p>");
    stub.objects
        .lock()
        .unwrap()
        .insert(page_uri.clone(), page.clone());

    // Compat form first: the embedded Page (with Lemmy's vestigial `actor`
    // field) is fetched from its origin and boosted.
    let mut compat_page = page.clone();
    compat_page["actor"] = json!(author.actor.id);
    let compat = group_announce(&group, "compat", &compat_page);
    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub.clone()), &compat, &group).await,
        StatusCode::ACCEPTED
    );
    let stored = status::find_by_uri(&pool, &page_uri)
        .await
        .unwrap()
        .expect("the compat announce ingested the post");
    let group_account = account::find_by_uri(&pool, &group.actor.id)
        .await
        .unwrap()
        .unwrap();
    let boost = status::find_reblog_by(&pool, group_account.id, stored.id)
        .await
        .unwrap()
        .expect("boosted");

    // Wrapper form second: same post, no second boost row.
    let wrapped = group_announce(&group, "wrapped", &create_page(&author, &group, &page));
    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub.clone()), &wrapped, &group).await,
        StatusCode::ACCEPTED
    );
    let after = status::find_reblog_by(&pool, group_account.id, stored.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.id, boost.id, "the duplicate announce was absorbed");
}

/// Mitra signs the activities its groups relay: a valid FEP-8b32 proof from
/// the inner author authenticates the embedded copy wholesale — no origin
/// fetch, stored as delivered.
#[sqlx::test(migrations = "../db/migrations")]
async fn wrapped_create_with_author_proof_trusts_embedded_copy(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let author = RemoteUser::new("origin.example", "erin").with_ed25519();
    let group = community("mitragroup");
    let stub = StubFederation::with_actors([author.actor.clone(), group.actor.clone()]);

    let note_uri = "https://origin.example/objects/0198";
    let inner = json!({
        "id": "https://origin.example/activities/create/0198",
        "type": "Create",
        "actor": author.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "cc": [],
        "object": {
            "id": note_uri,
            "type": "Note",
            "attributedTo": author.actor.id,
            "content": "<p>signed at the source</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "cc": [],
            "audience": group.actor.id,
            "published": "2026-07-11T10:00:00Z",
        },
    });
    // Mitra puts `audience` on the wrapper; the origin serves nothing (the
    // proof alone must carry the payload).
    let mut announce = group_announce(&group, "m1", &author.proof_signed(&inner));
    announce["audience"] = json!(group.actor.id);
    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub.clone()), &announce, &group).await,
        StatusCode::ACCEPTED
    );

    let stored = status::find_by_uri(&pool, note_uri)
        .await
        .unwrap()
        .expect("the proven embedded copy was ingested");
    assert_eq!(stored.content, "<p>signed at the source</p>");
    assert!(
        !stub.fetches().contains(&note_uri.to_owned()),
        "no origin fetch was needed: {:?}",
        stub.fetches()
    );
    let group_account = account::find_by_uri(&pool, &group.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        status::find_reblog_by(&pool, group_account.id, stored.id)
            .await
            .unwrap()
            .is_some(),
        "the group's announce boosts the post"
    );
}

/// Lemmy announces every comment in the community. Comments are ingested (so
/// threads arrive complete) but never boosted — a home timeline full of
/// strangers' comments is noise.
#[sqlx::test(migrations = "../db/migrations")]
async fn wrapped_comment_ingests_without_boost(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let author = RemoteUser::new("origin.example", "bob");
    let group = community("rustlang");
    let stub = StubFederation::with_actors([author.actor.clone(), group.actor.clone()]);
    let (page_uri, page) = origin_page(&author, &group, "<p>the post</p>");
    let comment_uri = "https://origin.example/comment/9".to_owned();
    let comment = json!({
        "id": comment_uri,
        "type": "Note",
        "attributedTo": author.actor.id,
        "content": "<p>a comment</p>",
        "inReplyTo": page_uri,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "cc": [group.actor.id],
        "audience": group.actor.id,
        "published": "2026-07-11T09:30:00Z",
    });
    {
        let mut objects = stub.objects.lock().unwrap();
        objects.insert(page_uri.clone(), page);
        objects.insert(comment_uri.clone(), comment.clone());
    }

    let inner = json!({
        "id": "https://origin.example/activities/create/9",
        "type": "Create",
        "actor": author.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "cc": [group.actor.id],
        "object": comment,
    });
    let announce = group_announce(&group, "c1", &inner);
    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub.clone()), &announce, &group).await,
        StatusCode::ACCEPTED
    );

    let stored = status::find_by_uri(&pool, &comment_uri)
        .await
        .unwrap()
        .expect("the comment was ingested");
    assert!(stored.in_reply_to_id.is_some(), "threaded under its post");
    let group_account = account::find_by_uri(&pool, &group.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        status::find_reblog_by(&pool, group_account.id, stored.id)
            .await
            .unwrap()
            .is_none(),
        "comments are not boosted into timelines"
    );
}

/// A wrapped `Update` is an edit: refreshed from the origin, never applied
/// from the embedded copy.
#[sqlx::test(migrations = "../db/migrations")]
async fn wrapped_update_refreshes_from_origin(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let author = RemoteUser::new("origin.example", "bob");
    let group = community("rustlang");
    let stub = StubFederation::with_actors([author.actor.clone(), group.actor.clone()]);
    let (page_uri, page) = origin_page(&author, &group, "<p>v1</p>");
    stub.objects
        .lock()
        .unwrap()
        .insert(page_uri.clone(), page.clone());

    let announce = group_announce(&group, "u0", &create_page(&author, &group, &page));
    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub.clone()), &announce, &group).await,
        StatusCode::ACCEPTED
    );

    // The origin now serves the edit; the announce embeds a forged one.
    let mut edited = page.clone();
    edited["content"] = json!("<p>v2</p>");
    edited["updated"] = json!("2026-07-11T11:00:00Z");
    stub.objects
        .lock()
        .unwrap()
        .insert(page_uri.clone(), edited.clone());
    let mut forged = edited.clone();
    forged["content"] = json!("<p>a forged edit</p>");
    let mut update = create_page(&author, &group, &forged);
    update["type"] = json!("Update");
    let announce = group_announce(&group, "u1", &update);
    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub.clone()), &announce, &group).await,
        StatusCode::ACCEPTED
    );

    let stored = status::find_by_uri(&pool, &page_uri)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.content, "<p>v2</p>", "the origin's edit was applied");
}

/// A wrapped `Delete` only applies once the origin confirms the object is
/// gone — a hostile group cannot delete a post its author still serves.
#[sqlx::test(migrations = "../db/migrations")]
async fn wrapped_delete_needs_origin_confirmation(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let author = RemoteUser::new("origin.example", "bob");
    let group = community("rustlang");
    let stub = StubFederation::with_actors([author.actor.clone(), group.actor.clone()]);
    let (page_uri, page) = origin_page(&author, &group, "<p>soon gone</p>");
    stub.objects
        .lock()
        .unwrap()
        .insert(page_uri.clone(), page.clone());

    let announce = group_announce(&group, "d0", &create_page(&author, &group, &page));
    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub.clone()), &announce, &group).await,
        StatusCode::ACCEPTED
    );
    assert!(
        status::find_by_uri(&pool, &page_uri)
            .await
            .unwrap()
            .is_some()
    );

    let delete = json!({
        "id": "https://origin.example/activities/delete/7",
        "type": "Delete",
        "actor": author.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "cc": [group.actor.id],
        "object": page_uri,
        "audience": group.actor.id,
    });

    // Origin still serves the post: the delete must not apply.
    let announce = group_announce(&group, "d1", &delete.clone());
    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub.clone()), &announce, &group).await,
        StatusCode::ACCEPTED
    );
    assert!(
        status::find_by_uri(&pool, &page_uri)
            .await
            .unwrap()
            .is_some(),
        "a delete the origin does not confirm is ignored"
    );

    // Origin gone (404): now it applies.
    stub.objects.lock().unwrap().remove(&page_uri);
    let announce = group_announce(&group, "d2", &delete);
    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub.clone()), &announce, &group).await,
        StatusCode::ACCEPTED
    );
    assert!(
        status::find_by_uri(&pool, &page_uri)
            .await
            .unwrap()
            .is_none(),
        "the origin-confirmed delete was applied"
    );
}

/// An announced activity that never names the group (no `audience`, no
/// `to`/`cc` membership) is not community activity — ignored outright.
#[sqlx::test(migrations = "../db/migrations")]
async fn announce_without_group_claim_is_ignored(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let author = RemoteUser::new("origin.example", "bob");
    let group = community("rustlang");
    let stub = StubFederation::with_actors([author.actor.clone(), group.actor.clone()]);
    let note_uri = "https://origin.example/statuses/55".to_owned();
    let note = json!({
        "id": note_uri,
        "type": "Note",
        "attributedTo": author.actor.id,
        "content": "<p>unrelated</p>",
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "cc": [],
    });
    stub.objects
        .lock()
        .unwrap()
        .insert(note_uri.clone(), note.clone());

    let inner = json!({
        "id": "https://origin.example/activities/create/55",
        "type": "Create",
        "actor": author.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "cc": [],
        "object": note,
    });
    let announce = group_announce(&group, "x1", &inner);
    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub.clone()), &announce, &group).await,
        StatusCode::ACCEPTED
    );
    assert!(
        status::find_by_uri(&pool, &note_uri)
            .await
            .unwrap()
            .is_none(),
        "an unclaimed announce ingests nothing"
    );
}

/// A page addressed to the group and its followers only (a private Lemmy
/// community) is ingested with its restricted visibility but never boosted —
/// a public boost row would leak it into shared timelines.
#[sqlx::test(migrations = "../db/migrations")]
async fn non_public_page_is_not_boosted(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let author = RemoteUser::new("origin.example", "bob");
    let group = community("privclub");
    let stub = StubFederation::with_actors([author.actor.clone(), group.actor.clone()]);
    let page_uri = "https://origin.example/post/8".to_owned();
    let page = json!({
        "id": page_uri,
        "type": "Page",
        "attributedTo": author.actor.id,
        "name": "Members only",
        "content": "<p>private community post</p>",
        "to": [group.actor.id, format!("{}/followers", group.actor.id)],
        "cc": [],
        "audience": group.actor.id,
        "published": "2026-07-11T09:00:00Z",
    });
    stub.objects
        .lock()
        .unwrap()
        .insert(page_uri.clone(), page.clone());

    let announce = group_announce(&group, "p1", &create_page(&author, &group, &page));
    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub.clone()), &announce, &group).await,
        StatusCode::ACCEPTED
    );
    let Some(stored) = status::find_by_uri(&pool, &page_uri).await.unwrap() else {
        return; // not ingesting a non-public page at all is also safe
    };
    assert_ne!(stored.visibility, "public");
    let group_account = account::find_by_uri(&pool, &group.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        status::find_reblog_by(&pool, group_account.id, stored.id)
            .await
            .unwrap()
            .is_none(),
        "a non-public post must not get a public boost row"
    );
}

/// A `Person`'s Announce of a status we don't hold is fetched from its origin
/// and boosted, the same as a group's. Accounts boost across the whole network,
/// so the boosted post is almost never one already on our instance; dropping
/// unknown targets (the pre-group behavior) meant a followed Mastodon / Pleroma /
/// Misskey account's boosts never surfaced at all. Only the FEP-1b12 dialect
/// handling — comment/vote filtering, wrapper unwrapping — stays group-only.
#[sqlx::test(migrations = "../db/migrations")]
async fn person_announce_of_unknown_status_is_fetched_and_boosted(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("origin.example", "bob");
    let fred = RemoteUser::new("elsewhere.example", "fred");
    let stub = StubFederation::with_actors([bob.actor.clone(), fred.actor.clone()]);
    let note_uri = "https://origin.example/statuses/60".to_owned();
    stub.objects.lock().unwrap().insert(
        note_uri.clone(),
        json!({
            "id": note_uri,
            "type": "Note",
            "attributedTo": bob.actor.id,
            "content": "<p>would be fetchable</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
        }),
    );
    let announce = json!({
        "id": "https://elsewhere.example/announces/1",
        "type": "Announce",
        "actor": fred.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": note_uri,
    });
    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub.clone()), &announce, &fred).await,
        StatusCode::ACCEPTED
    );
    assert!(stub.fetches().contains(&note_uri));
    let ingested = status::find_by_uri(&pool, &note_uri)
        .await
        .unwrap()
        .expect("a person's boost of an unknown status is fetched from its origin");
    let fred_account = account::find_by_uri(&pool, &fred.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        status::find_reblog_by(&pool, fred_account.id, ingested.id)
            .await
            .unwrap()
            .is_some(),
        "the fetched post gets a boost row for the announcer"
    );
}

/// `Announce(Update(Group))` — Lemmy's `UpdateCommunity` — refreshes the
/// stored group actor.
#[sqlx::test(migrations = "../db/migrations")]
async fn group_self_update_refreshes_the_actor(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let author = RemoteUser::new("origin.example", "bob");
    let group = community("rustlang");
    let stub = StubFederation::with_actors([author.actor.clone(), group.actor.clone()]);

    // Seed the group account with a first delivery.
    let (page_uri, page) = origin_page(&author, &group, "<p>seed</p>");
    stub.objects
        .lock()
        .unwrap()
        .insert(page_uri.clone(), page.clone());
    let announce = group_announce(&group, "s0", &create_page(&author, &group, &page));
    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub.clone()), &announce, &group).await,
        StatusCode::ACCEPTED
    );

    // The group renames itself; a mod's UpdateCommunity is announced.
    let mut renamed = group.actor.clone();
    renamed.name = Some("Rust Language".to_owned());
    stub.actors
        .lock()
        .unwrap()
        .insert(group.actor.id.clone(), renamed);
    let update = json!({
        "id": "https://origin.example/activities/update/1",
        "type": "Update",
        "actor": author.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "cc": [group.actor.id],
        "object": { "id": group.actor.id, "type": "Group" },
        "audience": group.actor.id,
    });
    let announce = group_announce(&group, "s1", &update);
    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub.clone()), &announce, &group).await,
        StatusCode::ACCEPTED
    );
    let refreshed = account::find_by_uri(&pool, &group.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(refreshed.display_name, "Rust Language");
}

/// `Undo(Announce)` from the group retracts the boost (a mod removing the
/// post from the community without the author deleting it).
#[sqlx::test(migrations = "../db/migrations")]
async fn group_undo_announce_removes_the_boost(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let author = RemoteUser::new("origin.example", "bob");
    let group = community("rustlang");
    let stub = StubFederation::with_actors([author.actor.clone(), group.actor.clone()]);
    let (page_uri, page) = origin_page(&author, &group, "<p>boosted then pulled</p>");
    stub.objects
        .lock()
        .unwrap()
        .insert(page_uri.clone(), page.clone());

    let announce = group_announce(&group, "un0", &create_page(&author, &group, &page));
    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub.clone()), &announce, &group).await,
        StatusCode::ACCEPTED
    );
    let stored = status::find_by_uri(&pool, &page_uri)
        .await
        .unwrap()
        .unwrap();
    let group_account = account::find_by_uri(&pool, &group.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        status::find_reblog_by(&pool, group_account.id, stored.id)
            .await
            .unwrap()
            .is_some()
    );

    let undo = json!({
        "id": format!("{}/activities/undo/un0", group.actor.id),
        "type": "Undo",
        "actor": group.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": announce["id"],
            "type": "Announce",
            "actor": group.actor.id,
        },
    });
    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub.clone()), &undo, &group).await,
        StatusCode::ACCEPTED
    );
    assert!(
        status::find_reblog_by(&pool, group_account.id, stored.id)
            .await
            .unwrap()
            .is_none(),
        "the boost was retracted; the post itself survives"
    );
    assert!(
        status::find_by_uri(&pool, &page_uri)
            .await
            .unwrap()
            .is_some()
    );
}

/// Lemmy relays members' votes as unproven wrapped `Announce`s. The
/// group is authoritative for votes on posts it announced — those count
/// (voter resolved by fetch, mutual exclusion applied); a wrapped vote on
/// anything the group never boosted is ignored.
#[sqlx::test(migrations = "../db/migrations")]
async fn group_relayed_votes_count_on_the_groups_own_posts(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let author = RemoteUser::new("origin.example", "bob");
    let voter = RemoteUser::new("elsewhere.example", "fred");
    let group = community("rustlang");
    let stub = StubFederation::with_actors([
        author.actor.clone(),
        voter.actor.clone(),
        group.actor.clone(),
    ]);
    let (page_uri, page) = origin_page(&author, &group, "<p>vote fodder</p>");
    stub.objects
        .lock()
        .unwrap()
        .insert(page_uri.clone(), page.clone());
    let announce = group_announce(&group, "v0", &create_page(&author, &group, &page));
    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub.clone()), &announce, &group).await,
        StatusCode::ACCEPTED
    );
    let stored = status::find_by_uri(&pool, &page_uri)
        .await
        .unwrap()
        .unwrap();

    // A wrapped Like from a voter we've never seen: the voter is fetched and
    // the upvote lands.
    let like = json!({
        "id": format!("{}/activities/like/1", voter.actor.id),
        "type": "Like",
        "actor": voter.actor.id,
        "object": page_uri,
        "audience": group.actor.id,
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &group_announce(&group, "v1", &like),
            &group,
        )
        .await,
        StatusCode::ACCEPTED
    );
    let fred = account::find_by_uri(&pool, &voter.actor.id)
        .await
        .unwrap()
        .expect("the voter was fetched and stored");
    assert!(
        !plamenu_db::favourite::favourited_of(&pool, fred.id, &[stored.id])
            .await
            .unwrap()
            .is_empty()
    );

    // The wrapped Dislike displaces it (mutual exclusion).
    let dislike = json!({
        "id": format!("{}/activities/dislike/2", voter.actor.id),
        "type": "Dislike",
        "actor": voter.actor.id,
        "object": page_uri,
        "audience": group.actor.id,
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &group_announce(&group, "v2", &dislike),
            &group,
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert!(
        plamenu_db::favourite::favourited_of(&pool, fred.id, &[stored.id])
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        !plamenu_db::dislike::disliked_of(&pool, fred.id, &[stored.id])
            .await
            .unwrap()
            .is_empty()
    );

    // A wrapped Undo(Dislike) retracts it.
    let undo = json!({
        "id": format!("{}/activities/undo/3", voter.actor.id),
        "type": "Undo",
        "actor": voter.actor.id,
        "object": dislike,
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &group_announce(&group, "v3", &undo),
            &group,
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert!(
        plamenu_db::dislike::disliked_of(&pool, fred.id, &[stored.id])
            .await
            .unwrap()
            .is_empty()
    );

    // A wrapped vote on a post this group never announced is not trusted —
    // no boost row, no authority, no stored vote.
    let local = plamenu_db::status::create_local(
        &pool,
        plamenu_db::status::NewLocalStatus::new(alice.id, "unrelated", "public", None),
    )
    .await
    .unwrap();
    let local_uri = format!("https://{TEST_DOMAIN}/users/alice/statuses/{}", local.id);
    let stray = json!({
        "id": format!("{}/activities/like/4", voter.actor.id),
        "type": "Like",
        "actor": voter.actor.id,
        "object": local_uri,
        "audience": group.actor.id,
    });
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &group_announce(&group, "v4", &stray),
            &group,
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert!(
        plamenu_db::favourite::favourited_of(&pool, fred.id, &[local.id])
            .await
            .unwrap()
            .is_empty(),
        "the group has no authority over posts it never announced"
    );
}

/// A community announces every comment. A comment whose parent post cannot be
/// fetched (the origin 403s, or serves `text/html` to an AP request) is stored
/// with no resolved parent — but it must still be recognised as a reply and
/// kept out of home timelines, not mistaken for a root post and boosted.
#[sqlx::test(migrations = "../db/migrations")]
async fn wrapped_reply_with_unfetchable_parent_is_not_boosted(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let author = RemoteUser::new("origin.example", "bob");
    let group = community("rustlang");
    let stub = StubFederation::with_actors([author.actor.clone(), group.actor.clone()]);

    // The comment is fetchable; its parent post is not (never added to the
    // stub → a 404, standing in for the real 403 / non-AP-content-type cases).
    let reply_uri = "https://origin.example/comment/42".to_owned();
    let reply = json!({
        "id": reply_uri,
        "type": "Note",
        "attributedTo": author.actor.id,
        "content": "<p>a reply to a post we can't reach</p>",
        "inReplyTo": "https://origin.example/post/unreachable",
        "to": [group.actor.id, "https://www.w3.org/ns/activitystreams#Public"],
        "cc": [],
        "audience": group.actor.id,
        "published": "2026-07-11T09:05:00Z",
    });
    stub.objects
        .lock()
        .unwrap()
        .insert(reply_uri.clone(), reply.clone());

    let announce = group_announce(&group, "reply1", &create_page(&author, &group, &reply));
    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub.clone()), &announce, &group).await,
        StatusCode::ACCEPTED
    );

    // The comment is ingested (threads stay complete) ...
    let stored = status::find_by_uri(&pool, &reply_uri)
        .await
        .unwrap()
        .expect("the announced comment was ingested");
    assert!(
        stored.in_reply_to_id.is_none(),
        "parent was unreachable, so it stays unresolved"
    );
    // ... but it is still a reply (in_reply_to_uri kept), so it is not boosted.
    assert!(
        status::is_reply(&pool, stored.id).await.unwrap(),
        "an orphaned reply is still a reply"
    );
    let group_account = account::find_by_uri(&pool, &group.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        status::find_reblog_by(&pool, group_account.id, stored.id)
            .await
            .unwrap()
            .is_none(),
        "an orphaned reply must not enter timelines as a root post"
    );
}

/// The double-send race: Lemmy delivers the wrapper `Announce(Create(Page))`
/// and the Mastodon-compat `Announce(Page)` under two different activity ids.
/// Delivered *concurrently* they must still yield exactly one boost row — the
/// sequential dedup test above cannot exercise the check-then-insert race.
#[sqlx::test(migrations = "../db/migrations")]
async fn concurrent_wrapper_and_compat_announce_boost_once(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let author = RemoteUser::new("origin.example", "bob");
    let group = community("rustlang");
    let stub = StubFederation::with_actors([author.actor.clone(), group.actor.clone()]);
    let (page_uri, page) = origin_page(&author, &group, "<p>hello community</p>");
    stub.objects
        .lock()
        .unwrap()
        .insert(page_uri.clone(), page.clone());

    let mut compat_page = page.clone();
    compat_page["actor"] = json!(author.actor.id);
    let compat = group_announce(&group, "compat", &compat_page);
    let wrapped = group_announce(&group, "wrapped", &create_page(&author, &group, &page));

    let (compat_status, wrapped_status) = tokio::join!(
        post_signed(test_app_with(pool.clone(), stub.clone()), &compat, &group),
        post_signed(test_app_with(pool.clone(), stub.clone()), &wrapped, &group),
    );
    assert_eq!(compat_status, StatusCode::ACCEPTED);
    assert_eq!(wrapped_status, StatusCode::ACCEPTED);

    let stored = status::find_by_uri(&pool, &page_uri)
        .await
        .unwrap()
        .expect("the post was ingested");
    let group_account = account::find_by_uri(&pool, &group.actor.id)
        .await
        .unwrap()
        .unwrap();
    let boosts = sqlx::query_scalar!(
        "SELECT count(*) FROM statuses WHERE account_id = $1 AND reblog_of_id = $2",
        group_account.id,
        stored.id,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        boosts,
        Some(1),
        "concurrent wrapper + compat delivery still yields exactly one boost"
    );
}

/// A Lemmy/PieFed image post carries its image only inline in the body, with an
/// empty `attachment`. The sanitizer strips the `<img>`, so the image must be
/// recovered as a remote-media attachment rather than lost.
#[sqlx::test(migrations = "../db/migrations")]
async fn inline_image_post_recovers_attachment(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let author = RemoteUser::new("origin.example", "bob");
    let group = community("pics");
    let stub = StubFederation::with_actors([author.actor.clone(), group.actor.clone()]);
    let uri = "https://origin.example/post/9".to_owned();
    let page = json!({
        "id": uri,
        "type": "Page",
        "attributedTo": author.actor.id,
        "name": "great news meme day",
        "content": "<p>a caption</p><p><img src=\"https://origin.example/pictrs/image/abc.avif\" alt=\"\"></p>",
        "attachment": [],
        "to": [group.actor.id, "https://www.w3.org/ns/activitystreams#Public"],
        "cc": [],
        "audience": group.actor.id,
        "published": "2026-07-11T09:00:00Z",
    });
    stub.objects
        .lock()
        .unwrap()
        .insert(uri.clone(), page.clone());

    let announce = group_announce(&group, "img", &create_page(&author, &group, &page));
    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub.clone()), &announce, &group).await,
        StatusCode::ACCEPTED
    );

    let stored = status::find_by_uri(&pool, &uri)
        .await
        .unwrap()
        .expect("the post was ingested");
    let attached = media::for_statuses(&pool, &[stored.id])
        .await
        .unwrap()
        .remove(&stored.id)
        .unwrap_or_default();
    assert_eq!(
        attached.len(),
        1,
        "the inline image is recovered as an attachment"
    );
    assert_eq!(
        attached[0].remote_url.as_deref(),
        Some("https://origin.example/pictrs/image/abc.avif"),
    );
}

// ---------------------------------------------------------------------------
// Inbound community-lifecycle reflection on a *consumed* remote community.
// The community relays these unproven (Lemmy signs nothing); each is trusted on
// the HTTP-signed Announce plus the audience claim, and a thread lock also on
// this group having actually announced the target (scoped trust, like votes).
// ---------------------------------------------------------------------------

/// A Lemmy `Lock` on a community post, embedded in the group's Announce.
fn lock_activity(actor: &RemoteUser, group: &RemoteUser, post_uri: &str, suffix: &str) -> Value {
    json!({
        "id": format!("{}/activities/lock/{suffix}", actor.actor.id),
        "type": "Lock",
        "actor": actor.actor.id,
        "object": post_uri,
        "to": [group.actor.id],
        "audience": group.actor.id,
    })
}

/// `Undo(inner)` from a community mod, embedded in the group's Announce.
fn undo_activity(actor: &RemoteUser, group: &RemoteUser, inner: &Value, suffix: &str) -> Value {
    json!({
        "id": format!("{}/activities/undo/{suffix}", actor.actor.id),
        "type": "Undo",
        "actor": actor.actor.id,
        "object": inner.clone(),
        "to": [group.actor.id],
        "audience": group.actor.id,
    })
}

/// A `Delete` addressed to the community, embedded in the group's Announce
/// (`object` is a post for a mod-removal, or the community itself for a
/// community deletion).
fn delete_activity(
    actor: &RemoteUser,
    group: &RemoteUser,
    object_uri: &str,
    suffix: &str,
) -> Value {
    json!({
        "id": format!("{}/activities/delete/{suffix}", actor.actor.id),
        "type": "Delete",
        "actor": actor.actor.id,
        "object": object_uri,
        "to": [group.actor.id],
        "audience": group.actor.id,
    })
}

/// Consumes `page` into the community (records the boost row) and returns the
/// stored post + the stored group account.
async fn consume_post(
    pool: &PgPool,
    stub: &Arc<StubFederation>,
    author: &RemoteUser,
    group: &RemoteUser,
    page_uri: &str,
    page: &Value,
) -> (status::Status, account::Account) {
    stub.objects
        .lock()
        .unwrap()
        .insert(page_uri.to_owned(), page.clone());
    let announce = group_announce(group, "seed", &create_page(author, group, page));
    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub.clone()), &announce, group).await,
        StatusCode::ACCEPTED
    );
    let stored = status::find_by_uri(pool, page_uri)
        .await
        .unwrap()
        .expect("the announced post was ingested");
    let group_account = account::find_by_uri(pool, &group.actor.id)
        .await
        .unwrap()
        .expect("group actor stored");
    (stored, group_account)
}

/// A wrapped `Lock` on a post the community announced locks the thread (the
/// group-agnostic `group_locked` observable then lights up); a later
/// `Undo(Lock)` clears it. The lock is keyed on the remote group account.
#[sqlx::test(migrations = "../db/migrations")]
async fn wrapped_lock_and_unlock_toggle_the_consumed_thread(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let author = RemoteUser::new("origin.example", "bob");
    let group = community("rustlang");
    let stub = StubFederation::with_actors([author.actor.clone(), group.actor.clone()]);
    let (page_uri, page) = origin_page(&author, &group, "<p>lockable</p>");
    let (stored, group_account) =
        consume_post(&pool, &stub, &author, &group, &page_uri, &page).await;

    // The community locks the thread.
    let lock = lock_activity(&author, &group, &page_uri, "1");
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &group_announce(&group, "lk1", &lock),
            &group,
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert!(
        group::thread_locked(&pool, group_account.id, stored.id)
            .await
            .unwrap(),
        "the consumed thread is locked under the remote group account"
    );
    assert_eq!(
        group::locked_of(&pool, &[stored.id]).await.unwrap(),
        vec![stored.id],
        "the group-agnostic observable reflects it"
    );

    // The community unlocks it (Undo(Lock)).
    let undo = undo_activity(&author, &group, &lock, "1");
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &group_announce(&group, "lk2", &undo),
            &group,
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert!(
        !group::thread_locked(&pool, group_account.id, stored.id)
            .await
            .unwrap(),
        "the Undo(Lock) cleared the lock"
    );
}

/// Scoped trust (hardening): a boost row is NOT authority to lock. Even
/// after a stranger community boosts a post — a forgeable object-form Announce —
/// its `Lock` is ignored unless the post genuinely claims that community as its
/// `audience`. The community the post actually claims can lock it.
#[sqlx::test(migrations = "../db/migrations")]
async fn wrapped_lock_needs_the_post_to_claim_the_community(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let author = RemoteUser::new("origin.example", "bob");
    let owner = community("rustlang");
    let stranger = community("evil");
    let stub = StubFederation::with_actors([
        author.actor.clone(),
        owner.actor.clone(),
        stranger.actor.clone(),
    ]);
    let (page_uri, page) = origin_page(&author, &owner, "<p>owned by rustlang</p>");
    let (stored, _) = consume_post(&pool, &stub, &author, &owner, &page_uri, &page).await;

    // The stranger boosts the post (object-form Announce — a forgeable boost
    // row), then relays a Lock. The Lock is ignored: the post's audience names
    // rustlang, not evil.
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &group_announce(&stranger, "eb", &json!(page_uri)),
            &stranger,
        )
        .await,
        StatusCode::ACCEPTED
    );
    let evil = account::find_by_uri(&pool, &stranger.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        group::boosting_group_ids(&pool, stored.id)
            .await
            .unwrap()
            .contains(&evil.id),
        "the stranger holds a (forged) boost row on the post"
    );
    let lock = lock_activity(&author, &stranger, &page_uri, "x");
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &group_announce(&stranger, "s1", &lock),
            &stranger,
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert!(
        group::locked_of(&pool, &[stored.id])
            .await
            .unwrap()
            .is_empty(),
        "a boost row is not authority: the stranger cannot lock a post it does not host"
    );

    // The rightful community (the post's claimed audience) can lock it.
    let lock = lock_activity(&author, &owner, &page_uri, "o");
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &group_announce(&owner, "o1", &lock),
            &owner,
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        group::locked_of(&pool, &[stored.id]).await.unwrap(),
        vec![stored.id],
        "the community the post claims as its audience can lock it"
    );
}

/// After a mod-removal drops the copy (origin-confirmed gone), the community's
/// `Undo(Delete)` restores it: the now-live origin Page is re-fetched and the
/// community boost re-recorded.
#[sqlx::test(migrations = "../db/migrations")]
async fn wrapped_undo_delete_restores_a_removed_post(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let author = RemoteUser::new("origin.example", "bob");
    let group = community("rustlang");
    let stub = StubFederation::with_actors([author.actor.clone(), group.actor.clone()]);
    let (page_uri, page) = origin_page(&author, &group, "<p>removed then restored</p>");
    let (_stored, group_account) =
        consume_post(&pool, &stub, &author, &group, &page_uri, &page).await;

    // Mod-removal: frank == author in Lemmy's single-owner case, so the
    // forwarded Delete matches and, with the origin gone, drops the copy.
    stub.objects.lock().unwrap().remove(&page_uri);
    let delete = delete_activity(&author, &group, &page_uri, "rm");
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &group_announce(&group, "rm1", &delete),
            &group,
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert!(
        status::find_by_uri(&pool, &page_uri)
            .await
            .unwrap()
            .is_none(),
        "the mod-removal dropped the post"
    );

    // The removal's origin 410 records a fetch-failure whose backoff would
    // suppress a re-fetch (the real server goes through the fetch budget; the
    // stub does not, so simulate it here).
    remote_fetch_failure::record_failure(&pool, "resource", &page_uri, "gone")
        .await
        .unwrap();
    assert!(
        !remote_fetch_failure::should_attempt(&pool, "resource", &page_uri)
            .await
            .unwrap(),
        "the removed post is under its fetch-failure backoff"
    );

    // Restore: the origin serves the post again and the community relays
    // Undo(Delete). The post and its community boost come back.
    stub.objects
        .lock()
        .unwrap()
        .insert(page_uri.clone(), page.clone());
    let undo = undo_activity(&author, &group, &delete, "rm");
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &group_announce(&group, "rm2", &undo),
            &group,
        )
        .await,
        StatusCode::ACCEPTED
    );
    let restored = status::find_by_uri(&pool, &page_uri)
        .await
        .unwrap()
        .expect("the restored post was re-ingested");
    assert!(
        status::find_reblog_by(&pool, group_account.id, restored.id)
            .await
            .unwrap()
            .is_some(),
        "the community boost was re-recorded on restore"
    );
    assert!(
        remote_fetch_failure::should_attempt(&pool, "resource", &page_uri)
            .await
            .unwrap(),
        "the restore cleared the post's fetch-failure negative cache"
    );
}

/// The community deletes itself (Lemmy: `Delete(actor=person, object=community)`):
/// once the origin confirms the community is gone, the stored remote group
/// account is dropped. A transient failure (origin still serves it) keeps it —
/// fail-closed.
#[sqlx::test(migrations = "../db/migrations")]
async fn wrapped_community_self_delete_drops_the_stored_group(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let author = RemoteUser::new("origin.example", "bob");
    let group = community("rustlang");
    let stub = StubFederation::with_actors([author.actor.clone(), group.actor.clone()]);
    let (_stored, group_account) = consume_post(
        &pool,
        &stub,
        &author,
        &group,
        "https://origin.example/post/7",
        &origin_page(&author, &group, "<p>a post in a doomed community</p>").1,
    )
    .await;

    // Fail-closed: the origin still serves the community, so a delete does not
    // drop the account.
    stub.objects.lock().unwrap().insert(
        group.actor.id.clone(),
        json!({ "id": group.actor.id, "type": "Group" }),
    );
    let delete = delete_activity(&author, &group, &group.actor.id, "c0");
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &group_announce(&group, "cd0", &delete),
            &group,
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert!(
        account::find_by_uri(&pool, &group.actor.id)
            .await
            .unwrap()
            .is_some(),
        "a community the origin still serves is not dropped"
    );

    // The origin now confirms the community is gone: the account is dropped and
    // its boost rows cascade away.
    stub.objects.lock().unwrap().insert(
        group.actor.id.clone(),
        json!({ "id": group.actor.id, "type": "Tombstone" }),
    );
    let delete = delete_activity(&author, &group, &group.actor.id, "c1");
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &group_announce(&group, "cd1", &delete),
            &group,
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert!(
        account::find_by_uri(&pool, &group.actor.id)
            .await
            .unwrap()
            .is_none(),
        "the origin-confirmed community deletion dropped the stored group account"
    );
    assert_eq!(
        group::boosting_group_ids(&pool, group_account.id)
            .await
            .unwrap_or_default()
            .len(),
        0,
        "the community's boosts cascaded away with it"
    );
}

/// A reply into a locked consumed thread is refused up front (not created as an
/// orphan the remote would silently drop). Local groups keep their silent-drop;
/// this hard refusal is scoped to remote communities.
#[sqlx::test(migrations = "../db/migrations")]
async fn reply_to_a_locked_consumed_thread_is_refused(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let author = RemoteUser::new("origin.example", "bob");
    let group = community("rustlang");
    let stub = StubFederation::with_actors([author.actor.clone(), group.actor.clone()]);
    let (page_uri, page) = origin_page(&author, &group, "<p>reply target</p>");
    // Keeping these large debug futures behind pointers prevents their state
    // machines from being laid out together in the test runner's small stack.
    let (stored, _) = Box::pin(consume_post(
        &pool, &stub, &author, &group, &page_uri, &page,
    ))
    .await;

    // Before the lock, alice may reply.
    let state = test_state_with(pool.clone(), stub.clone());
    let reply = Box::pin(actions::post_status(
        &state,
        PostParams {
            username: "alice",
            text: "first reply",
            visibility: "public",
            in_reply_to_id: Some(stored.id),
            ..Default::default()
        },
    ))
    .await;
    assert!(reply.is_ok(), "an unlocked thread accepts replies");

    // The community locks the thread.
    let lock = lock_activity(&author, &group, &page_uri, "r");
    assert_eq!(
        Box::pin(post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &group_announce(&group, "rl1", &lock),
            &group,
        ))
        .await,
        StatusCode::ACCEPTED
    );

    // Now the reply is refused, not silently un-delivered.
    let err = Box::pin(actions::post_status(
        &state,
        PostParams {
            username: "alice",
            text: "too late",
            visibility: "public",
            in_reply_to_id: Some(stored.id),
            ..Default::default()
        },
    ))
    .await
    .unwrap_err();
    assert!(matches!(err, plamenu::error::ApiError::Unprocessable(_)));
}

/// A Lemmy community ban (`Block`/`BlockUser`), embedded in the group's
/// Announce. `removeData` decides whether it also purges the user's content.
fn block_activity(
    actor: &RemoteUser,
    group: &RemoteUser,
    person_uri: &str,
    remove_data: bool,
    suffix: &str,
) -> Value {
    json!({
        "id": format!("{}/activities/block/{suffix}", actor.actor.id),
        "type": "Block",
        "actor": actor.actor.id,
        "object": person_uri,
        "target": group.actor.id,
        "removeData": remove_data,
        "to": [group.actor.id, "https://www.w3.org/ns/activitystreams#Public"],
        "audience": group.actor.id,
    })
}

/// A community *moderator* (not the post's author, and on a different host)
/// removes a member's post — Lemmy sends one `Delete` type for both author
/// delete and mod removal, with the mod as `actor`. The author-gated forwarded
/// path drops this (the deferred residual), so reflection now rests on the
/// post's OWN origin confirming it gone: while the origin still serves the Page
/// the relayed removal is refused (fail-closed), and once the origin 410s the
/// copy is dropped regardless of who signed the removal.
#[sqlx::test(migrations = "../db/migrations")]
async fn wrapped_mod_removal_of_another_users_post_is_reflected(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let author = RemoteUser::new("origin.example", "bob");
    let moderator = RemoteUser::new("groups.example", "carol");
    let group = community("rustlang");
    let stub = StubFederation::with_actors([
        author.actor.clone(),
        moderator.actor.clone(),
        group.actor.clone(),
    ]);
    let (page_uri, page) = origin_page(&author, &group, "<p>a member post</p>");
    let (_stored, _) = consume_post(&pool, &stub, &author, &group, &page_uri, &page).await;

    // The mod removes it while the origin still serves the Page: fail-closed, the
    // copy stays — a relayed Announce alone is never authority to delete.
    let delete = delete_activity(&moderator, &group, &page_uri, "mr");
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &group_announce(&group, "mr1", &delete),
            &group,
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert!(
        status::find_by_uri(&pool, &page_uri)
            .await
            .unwrap()
            .is_some(),
        "while the origin still serves the post, a relayed removal is refused"
    );

    // The mod removal propagated to the origin too, which now 410s the Page. The
    // same relayed Delete — actor is the mod, not the author, different host —
    // now drops the copy. The old author-gated path could never do this.
    stub.objects.lock().unwrap().remove(&page_uri);
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &group_announce(&group, "mr2", &delete),
            &group,
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert!(
        status::find_by_uri(&pool, &page_uri)
            .await
            .unwrap()
            .is_none(),
        "an origin-confirmed mod removal is reflected even though actor != author"
    );
}

/// A community ban that purges content (Lemmy: `Block`/`BlockUser` with
/// `removeData: true`) drops every post the banned user has in that community —
/// Lemmy runs the purge locally and sends no per-post Delete, so this wrapper is
/// the only signal. A plain ban (no `removeData`) touches nothing, and the purge
/// is scoped to the banned user's own content (a different member is untouched).
#[sqlx::test(migrations = "../db/migrations")]
async fn wrapped_ban_with_remove_data_purges_the_users_content(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("origin.example", "bob");
    let dan = RemoteUser::new("elsewhere.example", "dan");
    let moderator = RemoteUser::new("groups.example", "carol");
    let group = community("rustlang");
    let stub = StubFederation::with_actors([
        bob.actor.clone(),
        dan.actor.clone(),
        moderator.actor.clone(),
        group.actor.clone(),
    ]);

    let page = |author: &RemoteUser, uri: &str| {
        json!({
            "id": uri,
            "type": "Page",
            "attributedTo": author.actor.id,
            "name": "Post",
            "content": "<p>hi</p>",
            "to": [group.actor.id, "https://www.w3.org/ns/activitystreams#Public"],
            "cc": [],
            "audience": group.actor.id,
            "published": "2026-07-11T09:00:00Z",
        })
    };
    let bob1 = "https://origin.example/post/1".to_owned();
    let bob2 = "https://origin.example/post/2".to_owned();
    let dan1 = "https://elsewhere.example/post/9".to_owned();
    for (author, uri, sfx) in [
        (&bob, &bob1, "p1"),
        (&bob, &bob2, "p2"),
        (&dan, &dan1, "p3"),
    ] {
        let pg = page(author, uri);
        stub.objects.lock().unwrap().insert(uri.clone(), pg.clone());
        let announce = group_announce(&group, sfx, &create_page(author, &group, &pg));
        assert_eq!(
            post_signed(test_app_with(pool.clone(), stub.clone()), &announce, &group).await,
            StatusCode::ACCEPTED
        );
    }
    for uri in [&bob1, &bob2, &dan1] {
        assert!(
            status::find_by_uri(&pool, uri).await.unwrap().is_some(),
            "each announced post was ingested"
        );
    }

    // A plain ban (no removeData) purges nothing.
    let ban = block_activity(&moderator, &group, &bob.actor.id, false, "b1");
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &group_announce(&group, "bn1", &ban),
            &group,
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert!(
        status::find_by_uri(&pool, &bob1).await.unwrap().is_some(),
        "a ban without removeData leaves the banned user's content intact"
    );

    // The ban is re-issued with removeData: bob's content in the community is
    // purged; dan's (a different user) is untouched.
    let purge = block_activity(&moderator, &group, &bob.actor.id, true, "b2");
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &group_announce(&group, "bn2", &purge),
            &group,
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert!(
        status::find_by_uri(&pool, &bob1).await.unwrap().is_none(),
        "removeData purged the banned user's first post"
    );
    assert!(
        status::find_by_uri(&pool, &bob2).await.unwrap().is_none(),
        "removeData purged the banned user's second post"
    );
    assert!(
        status::find_by_uri(&pool, &dan1).await.unwrap().is_some(),
        "the purge is scoped to the banned user's own content"
    );
}

// ===========================================================================
// Community facts: the flags and moderator roster a Group actor publishes
// about itself, mirrored locally (`remote_groups`).
// ===========================================================================

/// Lemmy's community extensions land on the stored community: the NSFW flag,
/// the mods-only posting restriction, and where its moderators live.
#[sqlx::test(migrations = "../db/migrations")]
async fn lemmy_community_flags_are_mirrored(pool: PgPool) {
    let mut group = community("nsfwart");
    group.actor.sensitive = Some(true);
    group.actor.posting_restricted_to_mods = Some(true);
    group.actor.attributed_to = Some(json!(format!("{}/moderators", group.actor.id)));
    let stub = StubFederation::with_actors([group.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());

    let stored = plamenu::remote::store_remote_actor(&pool, &group.actor)
        .await
        .unwrap();
    let facts = remote_group::find(&pool, stored.id).await.unwrap().unwrap();
    assert!(facts.sensitive, "the community-wide NSFW flag is mirrored");
    assert_eq!(facts.posting_policy(), group::PostingPolicy::Mods);
    assert_eq!(
        facts.moderators_uri,
        format!("{}/moderators", group.actor.id)
    );
    // A hosted group is untouched by any of this: the sidecar is the policy we
    // enforce, this table only the origin's claims.
    assert!(group::find(&pool, stored.id).await.unwrap().is_none());

    // The roster is dereferenced like Lemmy does, resolving each entry — a
    // known local account included.
    create_local_account(&pool, "alice", "Alice").await;
    let bob = RemoteUser::new("origin.example", "bob");
    stub.actors
        .lock()
        .unwrap()
        .insert(bob.actor.id.clone(), bob.actor.clone());
    stub.objects.lock().unwrap().insert(
        format!("{}/moderators", group.actor.id),
        json!({
            "type": "OrderedCollection",
            "id": format!("{}/moderators", group.actor.id),
            "orderedItems": [
                bob.actor.id,
                format!("https://{TEST_DOMAIN}/users/alice"),
                // The community itself is not one of its own moderators.
                group.actor.id,
            ],
        }),
    );
    plamenu::remote::sync_group_moderators(
        &state,
        stored.id,
        &plamenu::remote::ModeratorSource::Collection(format!("{}/moderators", group.actor.id)),
    )
    .await
    .unwrap();

    let alice = account::find_local_by_username(&pool, "alice")
        .await
        .unwrap()
        .unwrap();
    let bob_stored = account::find_by_uri(&pool, &bob.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        remote_group::moderator_ids(&pool, stored.id).await.unwrap(),
        vec![bob_stored.id, alice.id],
        "roster keeps the origin's order and resolves local moderators"
    );
    assert!(
        remote_group::is_moderator(&pool, stored.id, alice.id)
            .await
            .unwrap()
    );
}

/// A community that stops advertising its flags stops being marked, and one
/// that stops naming moderators has its roster dropped: the actor document is
/// authoritative about its own community on every refresh.
#[sqlx::test(migrations = "../db/migrations")]
async fn community_facts_are_refreshed_not_accumulated(pool: PgPool) {
    let mut group = community("rustlang");
    group.actor.sensitive = Some(true);
    group.actor.attributed_to = Some(json!(format!("{}/moderators", group.actor.id)));
    let stored = plamenu::remote::store_remote_actor(&pool, &group.actor)
        .await
        .unwrap();
    let bob = RemoteUser::new("origin.example", "bob");
    plamenu_db::remote_group::set_moderators(
        &pool,
        stored.id,
        &[plamenu::remote::store_remote_actor(&pool, &bob.actor)
            .await
            .unwrap()
            .id],
    )
    .await
    .unwrap();

    group.actor.sensitive = None;
    group.actor.attributed_to = None;
    plamenu::remote::store_remote_actor(&pool, &group.actor)
        .await
        .unwrap();

    let facts = remote_group::find(&pool, stored.id).await.unwrap().unwrap();
    assert!(!facts.sensitive);
    assert!(facts.moderators_uri.is_empty());
    assert!(
        remote_group::moderator_ids(&pool, stored.id)
            .await
            .unwrap()
            .is_empty(),
        "an actor naming no moderators drops the roster it named before"
    );
}

/// Mitra publishes FEP-5219 `affiliations` and no `attributedTo` at all, so
/// that collection is the fallback — otherwise a Mitra community would look
/// unmoderated. Only the upper rungs of the ladder count.
#[sqlx::test(migrations = "../db/migrations")]
async fn mitra_affiliations_supply_the_roster(pool: PgPool) {
    let mut group = community("mitracommunity");
    let affiliations = format!("{}/affiliations", group.actor.id);
    group.actor.affiliations = Some(json!(affiliations));
    let admin = RemoteUser::new("groups.example", "erin");
    let member = RemoteUser::new("groups.example", "frank");
    let stub = StubFederation::with_actors([
        group.actor.clone(),
        admin.actor.clone(),
        member.actor.clone(),
    ]);
    stub.objects.lock().unwrap().insert(
        affiliations.clone(),
        json!({
            "type": "OrderedCollection",
            "id": affiliations,
            "orderedItems": [
                { "type": "Relationship", "subject": admin.actor.id, "relationship": "admin" },
                { "type": "Relationship", "subject": member.actor.id, "relationship": "member" },
            ],
        }),
    );
    let state = test_state_with(pool.clone(), stub.clone());
    let stored = plamenu::remote::store_remote_actor(&pool, &group.actor)
        .await
        .unwrap();
    assert_eq!(
        remote_group::find(&pool, stored.id)
            .await
            .unwrap()
            .unwrap()
            .moderators_uri,
        format!("{}/affiliations", group.actor.id),
    );

    plamenu::remote::sync_group_moderators(
        &state,
        stored.id,
        &plamenu::remote::ModeratorSource::Affiliations(format!("{}/affiliations", group.actor.id)),
    )
    .await
    .unwrap();
    let admin_id = account::find_by_uri(&pool, &admin.actor.id)
        .await
        .unwrap()
        .unwrap()
        .id;
    assert_eq!(
        remote_group::moderator_ids(&pool, stored.id).await.unwrap(),
        vec![admin_id],
        "an `admin` affiliation moderates; a plain `member` does not"
    );
}

/// A Plamenu peer's members-only community survives the round trip. Lemmy's
/// boolean cannot express it, so the tri-state term is what keeps two
/// instances of this software agreeing on who may post.
#[sqlx::test(migrations = "../db/migrations")]
async fn members_only_policy_survives_the_round_trip(pool: PgPool) {
    let mut group = community("hiking");
    group.actor.posting_restricted_to_mods = Some(false);
    group.actor.posting_policy = Some("members".to_owned());
    let stored = plamenu::remote::store_remote_actor(&pool, &group.actor)
        .await
        .unwrap();
    assert_eq!(
        remote_group::find(&pool, stored.id)
            .await
            .unwrap()
            .unwrap()
            .posting_policy(),
        group::PostingPolicy::Members,
    );

    // A peer that does not know the term still gets the two states Lemmy's
    // vocabulary has, never a wrong third one.
    let mut lemmy = community("lemmyonly");
    lemmy.actor.posting_restricted_to_mods = Some(false);
    let stored = plamenu::remote::store_remote_actor(&pool, &lemmy.actor)
        .await
        .unwrap();
    assert_eq!(
        remote_group::find(&pool, stored.id)
            .await
            .unwrap()
            .unwrap()
            .posting_policy(),
        group::PostingPolicy::Anyone,
    );
}
