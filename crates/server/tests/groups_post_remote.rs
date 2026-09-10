//! Originating posts into a *remote* community (Lemmy/Mbin/PieFed). A
//! subscribed local user posts to a community the way a Lemmy client does — a
//! `Create` (a `Page` when titled, else a `Note`) addressed to the community
//! with the FEP-1b12 `audience` claim, delivered to the community's inbox. The
//! community announces it back; that returning `Announce` records the local
//! group attribution (a boost row) without re-ingesting or clobbering the
//! author's own copy. Edits and deletes travel to the community inbox too.
//!
//! Posting is gated on following (subscribing to) the community — the same
//! follow that brings the community's `Announce` back to attribute the post.
//!
//! The stub here drives the no-proof (Lemmy) round-trip; the proof-carrying
//! round-trip (proofs on) is exercised end-to-end by `test_lemmy_federation`.

mod common;

use std::time::SystemTime;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{RemoteUser, StubFederation, TEST_DOMAIN, create_local_account, test_state_with};
use plamenu::actions::{self, EditParams, PostParams};
use plamenu::{build_router, delivery};
use plamenu_db::account::Account;
use plamenu_db::{PgPool, account, follow, group, notification, status};
use serde_json::{Value, json};
use tower::ServiceExt;

const ALICE_URI: &str = "https://plamenu.test/users/alice";

/// A Lemmy-style community: a `Group`-typed actor on its own host.
fn community(name: &str) -> RemoteUser {
    let mut group = RemoteUser::new("lemmy.example", name);
    "Group".clone_into(&mut group.actor.kind);
    group
}

/// Stores `community` and subscribes alice to it (an accepted follow) — the
/// Posting gate. Returns the community's stored account.
async fn subscribed_community(pool: &PgPool, community: &RemoteUser) -> Account {
    let stored = plamenu::remote::store_remote_actor(pool, &community.actor)
        .await
        .unwrap();
    let alice = account::find_local_by_username(pool, "alice")
        .await
        .unwrap()
        .unwrap();
    follow::create(pool, alice.id, stored.id, None)
        .await
        .unwrap();
    stored
}

async fn post_signed(app: Router, body: &Value, user: &RemoteUser) -> StatusCode {
    let bytes = serde_json::to_vec(body).unwrap();
    let signed = user
        .signer()
        .sign_post(TEST_DOMAIN, "/inbox", &bytes, SystemTime::now());
    let request = Request::builder()
        .method("POST")
        .uri("/inbox")
        .header("host", signed.host)
        .header("date", signed.date)
        .header("digest", signed.digest)
        .header("signature", signed.signature)
        .header("content-type", "application/activity+json")
        .body(Body::from(bytes))
        .unwrap();
    app.oneshot(request).await.unwrap().status()
}

/// The community relays a member's `Create` back to its followers — the return
/// leg that records the local attribution. Mirrors Lemmy's no-proof wrapper
/// (`Announce(Create(Page))`), the inner activity attributed to the local
/// author, addressed to the community.
async fn announce_own_post_back(
    app: Router,
    community: &RemoteUser,
    status_uri: &str,
    name: &str,
    content: &str,
) -> StatusCode {
    let page = json!({
        "id": status_uri,
        "type": "Page",
        "attributedTo": ALICE_URI,
        "name": name,
        "content": content,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "cc": [community.actor.id],
        "audience": community.actor.id,
        "published": "2026-07-14T09:00:00Z",
    });
    let create = json!({
        "id": format!("{status_uri}/activity"),
        "type": "Create",
        "actor": ALICE_URI,
        "object": page,
        "audience": community.actor.id,
    });
    let announce = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}/announces/1", community.actor.id),
        "type": "Announce",
        "actor": community.actor.id,
        "object": create,
        "audience": community.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "cc": [format!("{}/followers", community.actor.id)],
    });
    post_signed(app, &announce, community).await
}

#[sqlx::test(migrations = "../db/migrations")]
async fn subscribed_titled_post_reaches_the_community_inbox_as_a_page(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let hiking = community("hiking");
    let stub = StubFederation::with_actors([hiking.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let group = subscribed_community(&pool, &hiking).await;

    let (stored, _) = actions::post_status(
        &state,
        PostParams {
            username: "alice",
            text: "check this out",
            visibility: "public",
            group_id: Some(group.id),
            title: Some("Trail map"),
            external_url: Some("https://example.com/map"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(stored.title.as_deref(), Some("Trail map"));
    assert_eq!(stored.object_type.as_deref(), Some("Page"));
    // No local announce: a remote community relays on its own, so we hold no
    // boost row for it yet, and no silent mention row either.
    assert!(
        status::find_reblog_by(&pool, group.id, stored.id)
            .await
            .unwrap()
            .is_none()
    );

    // The Create is delivered to the community's own inbox, carrying the
    // FEP-1b12 community claim on both the activity and the object.
    assert_eq!(delivery::run_due(&state).await, 1);
    let delivered = stub.deliveries();
    let create = delivered
        .iter()
        .find(|d| d.inbox_url == hiking.actor.inbox)
        .expect("the Create reaches the community inbox");
    assert_eq!(create.activity["type"], "Create");
    assert_eq!(create.activity["actor"], ALICE_URI);
    assert_eq!(create.activity["audience"], hiking.actor.id);
    let object = &create.activity["object"];
    assert_eq!(object["type"], "Page");
    assert_eq!(object["name"], "Trail map");
    assert_eq!(object["audience"], hiking.actor.id);
    assert!(
        object["cc"]
            .as_array()
            .unwrap()
            .contains(&json!(hiking.actor.id)),
        "the community is a cc recipient"
    );
    assert_eq!(
        object["attachment"][0],
        json!({ "type": "Link", "href": "https://example.com/map" })
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn subscribed_untitled_post_federates_as_a_note(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let hiking = community("hiking");
    let stub = StubFederation::with_actors([hiking.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let group = subscribed_community(&pool, &hiking).await;

    let (stored, _) = actions::post_status(
        &state,
        PostParams {
            username: "alice",
            text: "no title here",
            visibility: "public",
            group_id: Some(group.id),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(stored.object_type, None);

    assert_eq!(delivery::run_due(&state).await, 1);
    let delivered = stub.deliveries();
    let create = delivered
        .iter()
        .find(|d| d.inbox_url == hiking.actor.inbox)
        .expect("the Create reaches the community inbox");
    // Lemmy auto-titles Notes it receives into a community — both worlds work.
    assert_eq!(create.activity["object"]["type"], "Note");
    assert_eq!(create.activity["object"]["audience"], hiking.actor.id);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn posting_to_an_unfollowed_community_is_allowed(pool: PgPool) {
    // We don't own a remote community's policy, so we defer to it: a user
    // may post without following first; the remote enforces its own rules and
    // the post simply won't come back attributed if it's rejected there.
    create_local_account(&pool, "alice", "Alice").await;
    let hiking = community("hiking");
    let stub = StubFederation::with_actors([hiking.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    // Store the community but do NOT subscribe.
    let group = plamenu::remote::store_remote_actor(&pool, &hiking.actor)
        .await
        .unwrap();

    actions::post_status(
        &state,
        PostParams {
            username: "alice",
            text: "let me in",
            visibility: "public",
            group_id: Some(group.id),
            ..Default::default()
        },
    )
    .await
    .expect("posting to a community we don't follow is allowed");

    // The Create is still delivered to the community inbox.
    assert_eq!(delivery::run_due(&state).await, 1);
    let delivered = stub.deliveries();
    assert!(
        delivered.iter().any(|d| d.inbox_url == hiking.actor.inbox
            && d.activity["type"] == "Create"
            && d.activity["audience"] == hiking.actor.id),
        "the post reaches the community inbox even without a prior follow"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn community_announce_of_own_post_attributes_without_duplicating(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let hiking = community("hiking");
    let stub = StubFederation::with_actors([hiking.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let group = subscribed_community(&pool, &hiking).await;

    let (stored, _) = actions::post_status(
        &state,
        PostParams {
            username: "alice",
            text: "hello trail",
            visibility: "public",
            group_id: Some(group.id),
            title: Some("Hi"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let status_uri = format!("{ALICE_URI}/statuses/{}", stored.id);
    let original = status::find_by_id(&pool, stored.id)
        .await
        .unwrap()
        .unwrap()
        .content;

    let code = announce_own_post_back(app, &hiking, &status_uri, "Hi", &original).await;
    assert_eq!(code, StatusCode::ACCEPTED);

    // Attribution recorded: the community boosts alice's own post.
    assert!(
        status::find_reblog_by(&pool, group.id, stored.id)
            .await
            .unwrap()
            .is_some(),
        "the returning announce records the group boost row"
    );
    // Our own copy is untouched: not re-ingested as a remote duplicate (a
    // duplicate would carry the object uri; ours is a local NULL-uri row), and
    // not clobbered.
    assert!(
        status::find_by_uri(&pool, &status_uri)
            .await
            .unwrap()
            .is_none(),
        "no duplicate remote copy of our own post is created"
    );
    assert_eq!(
        status::find_by_id(&pool, stored.id)
            .await
            .unwrap()
            .unwrap()
            .content,
        original,
        "the author's own status content is left untouched"
    );
    // The community announcing our own post back is not a "reblog" to notify
    // about, mirroring local-group posting's silent boost.
    assert!(
        !notification::exists(&pool, alice.id, group.id, "reblog", Some(stored.id))
            .await
            .unwrap(),
        "the return leg does not self-notify the author"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn edit_of_a_community_post_reaches_the_community_inbox(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let hiking = community("hiking");
    let stub = StubFederation::with_actors([hiking.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let group = subscribed_community(&pool, &hiking).await;

    let (stored, _) = actions::post_status(
        &state,
        PostParams {
            username: "alice",
            text: "before",
            visibility: "public",
            group_id: Some(group.id),
            title: Some("Note"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let status_uri = format!("{ALICE_URI}/statuses/{}", stored.id);
    // The community announces it back so the group boost row (the remote
    // attribution the edit re-addresses through) exists.
    announce_own_post_back(app, &hiking, &status_uri, "Note", &stored.content).await;
    assert_eq!(delivery::run_due(&state).await, 1); // drain the original Create

    actions::edit_status(
        &state,
        &alice,
        stored.id,
        EditParams {
            text: Some("after"),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert!(delivery::run_due(&state).await >= 1);
    let update = stub
        .deliveries()
        .into_iter()
        .find(|d| d.inbox_url == hiking.actor.inbox && d.activity["type"] == "Update")
        .expect("the edit reaches the community inbox as an Update");
    assert_eq!(update.activity["object"]["audience"], hiking.actor.id);
    assert_eq!(update.activity["object"]["content"], "<p>after</p>");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn delete_of_a_community_post_reaches_the_community_inbox(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let hiking = community("hiking");
    let stub = StubFederation::with_actors([hiking.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let group = subscribed_community(&pool, &hiking).await;

    let (stored, _) = actions::post_status(
        &state,
        PostParams {
            username: "alice",
            text: "delete me",
            visibility: "public",
            group_id: Some(group.id),
            title: Some("Note"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let status_uri = format!("{ALICE_URI}/statuses/{}", stored.id);
    announce_own_post_back(app, &hiking, &status_uri, "Note", &stored.content).await;
    assert_eq!(delivery::run_due(&state).await, 1); // drain the original Create

    actions::delete_status(&state, &alice, stored.id, actions::DeleteMode::Wipe)
        .await
        .unwrap();

    assert!(delivery::run_due(&state).await >= 1);
    let delete = stub
        .deliveries()
        .into_iter()
        .find(|d| d.inbox_url == hiking.actor.inbox && d.activity["type"] == "Delete")
        .expect("the delete reaches the community inbox");
    assert_eq!(delete.activity["audience"], hiking.actor.id);
}

/// A reply to a *consumed* remote community post must reach the community's OWN
/// inbox, stamped with `audience`/`cc` = community — otherwise Lemmy stores it
/// against the parent author's personal inbox but never runs its announce
/// fan-out, so the reply stays invisible on the origin (the reported bug). The
/// parent is attributed to the community by a BOOST row (not a mention), which
/// the mention-based reply discovery used to miss entirely.
#[sqlx::test(migrations = "../db/migrations")]
async fn reply_to_a_consumed_community_post_reaches_the_community_inbox(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let hiking = community("hiking");
    let bob = RemoteUser::new("origin.example", "bob");
    let stub = StubFederation::with_actors([hiking.actor.clone(), bob.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());

    // A remote member's post consumed via the community: the inbound Announce
    // records the boost row that is the ONLY local attribution to the community.
    let post_uri = "https://origin.example/post/42";
    let page = json!({
        "id": post_uri,
        "type": "Page",
        "attributedTo": bob.actor.id,
        "name": "Trailhead",
        "content": "<p>where do we meet?</p>",
        "to": [hiking.actor.id, "https://www.w3.org/ns/activitystreams#Public"],
        "cc": [],
        "audience": hiking.actor.id,
        "published": "2026-07-14T09:00:00Z",
    });
    stub.objects
        .lock()
        .unwrap()
        .insert(post_uri.to_owned(), page.clone());
    let create = json!({
        "id": format!("{post_uri}/activity"),
        "type": "Create",
        "actor": bob.actor.id,
        "object": page,
        "audience": hiking.actor.id,
        "cc": [hiking.actor.id],
    });
    let announce = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}/announces/9", hiking.actor.id),
        "type": "Announce",
        "actor": hiking.actor.id,
        "object": create,
        "audience": hiking.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "cc": [format!("{}/followers", hiking.actor.id)],
    });
    // Keep the two large federation/action futures out of this test's debug
    // state-machine layout; together they exceed the runner thread's small
    // default stack on some toolchains.
    assert_eq!(
        Box::pin(post_signed(build_router(state.clone()), &announce, &hiking,)).await,
        StatusCode::ACCEPTED
    );
    let parent = status::find_by_uri(&pool, post_uri)
        .await
        .unwrap()
        .expect("the community post was consumed");
    let group = account::find_by_uri(&pool, &hiking.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        status::find_reblog_by(&pool, group.id, parent.id)
            .await
            .unwrap()
            .is_some(),
        "the parent is attributed to the community by a boost row, not a mention"
    );

    // Alice replies — no follow required (defers to the remote's policy).
    Box::pin(actions::post_status(
        &state,
        PostParams {
            username: "alice",
            text: "at the north gate",
            visibility: "public",
            in_reply_to_id: Some(parent.id),
            ..Default::default()
        },
    ))
    .await
    .unwrap();

    // The decisive assertion: the reply Create is delivered to the community's
    // OWN inbox, carrying the community claim, so the origin announces it.
    assert!(Box::pin(delivery::run_due(&state)).await >= 1);
    let delivered = stub.deliveries();
    let reply = delivered
        .iter()
        .find(|d| d.inbox_url == hiking.actor.inbox && d.activity["type"] == "Create")
        .expect("the reply Create reaches the community inbox");
    assert_eq!(reply.activity["audience"], hiking.actor.id);
    assert_eq!(reply.activity["object"]["inReplyTo"], post_uri);
    assert!(
        reply.activity["object"]["cc"]
            .as_array()
            .unwrap()
            .contains(&json!(hiking.actor.id)),
        "the reply names the community in cc so Lemmy announces it"
    );
}

/// A community moderator locking one of *our* members' threads. The target is
/// a local status, which has no stored `uri` to look it up by, and its
/// community claim is the attribution we recorded when the community announced
/// it back — the same claim we serve as the post's `audience`, so there is
/// nothing to fetch from ourselves. Without both, the lock silently vanished
/// and the member's own copy stayed open.
#[sqlx::test(migrations = "../db/migrations")]
async fn community_lock_of_our_members_own_post_reaches_the_member(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let hiking = community("hiking");
    let stranger = community("evil");
    let stub = StubFederation::with_actors([hiking.actor.clone(), stranger.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let group = subscribed_community(&pool, &hiking).await;

    let (stored, _) = actions::post_status(
        &state,
        PostParams {
            username: "alice",
            text: "lock me",
            visibility: "public",
            group_id: Some(group.id),
            title: Some("Note"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let status_uri = format!("{ALICE_URI}/statuses/{}", stored.id);
    announce_own_post_back(app, &hiking, &status_uri, "Note", &stored.content).await;

    // A community that never announced it has no say (the audience gate).
    let stranger_account = plamenu::remote::store_remote_actor(&pool, &stranger.actor)
        .await
        .unwrap();
    assert_eq!(
        post_signed(
            build_router(state.clone()),
            &announce_lock(&stranger, &status_uri, "x1", false),
            &stranger,
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert!(
        !group::thread_locked(&pool, stranger_account.id, stored.id)
            .await
            .unwrap(),
        "a community the post never claimed cannot lock it"
    );

    // The community the post actually belongs to can.
    assert_eq!(
        post_signed(
            build_router(state.clone()),
            &announce_lock(&hiking, &status_uri, "l1", false),
            &hiking,
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert!(
        group::thread_locked(&pool, group.id, stored.id)
            .await
            .unwrap(),
        "the community's Lock reaches the member's own copy"
    );
    assert_eq!(
        group::locked_of(&pool, &[stored.id]).await.unwrap(),
        vec![stored.id],
        "the group-agnostic observable reflects it"
    );

    // …and reopen it again.
    assert_eq!(
        post_signed(
            build_router(state.clone()),
            &announce_lock(&hiking, &status_uri, "l2", true),
            &hiking,
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert!(
        !group::thread_locked(&pool, group.id, stored.id)
            .await
            .unwrap(),
        "the Undo(Lock) reopens it"
    );
}

/// A community retracting its announce. It announces every post twice — the
/// FEP-1b12 wrapper and the Mastodon-compat `Announce(Page)` beside it — and
/// the boost row keeps whichever id arrived first, while the retraction can
/// only ever name the compat one. Matching the boost by what the Announce
/// points *at* is what makes a removal actually remove.
#[sqlx::test(migrations = "../db/migrations")]
async fn community_retraction_removes_the_attribution_it_recorded(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let hiking = community("hiking");
    let stub = StubFederation::with_actors([hiking.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let group = subscribed_community(&pool, &hiking).await;

    let (stored, _) = actions::post_status(
        &state,
        PostParams {
            username: "alice",
            text: "retract me",
            visibility: "public",
            group_id: Some(group.id),
            title: Some("Note"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let status_uri = format!("{ALICE_URI}/statuses/{}", stored.id);
    announce_own_post_back(app, &hiking, &status_uri, "Note", &stored.content).await;
    assert!(
        status::find_reblog_by(&pool, group.id, stored.id)
            .await
            .unwrap()
            .is_some(),
        "the attribution was recorded under the wrapper's announce id"
    );

    // The retraction reconstructs the *compat* announce, whose id is not the
    // one the boost row was stored under.
    let undo = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}/undo/1", hiking.actor.id),
        "type": "Undo",
        "actor": hiking.actor.id,
        "object": {
            "id": format!("{}/announces/9999", hiking.actor.id),
            "type": "Announce",
            "actor": hiking.actor.id,
            "object": status_uri,
        },
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
    });
    assert_eq!(
        post_signed(build_router(state.clone()), &undo, &hiking).await,
        StatusCode::ACCEPTED
    );
    assert!(
        status::find_reblog_by(&pool, group.id, stored.id)
            .await
            .unwrap()
            .is_none(),
        "the retraction removed the community's attribution"
    );
    assert!(
        status::find_by_id(&pool, stored.id)
            .await
            .unwrap()
            .is_some(),
        "the author's own post survives a community's retraction"
    );
}

/// The group's `Announce(Lock)` / `Announce(Undo(Lock))` wrapper.
fn announce_lock(group: &RemoteUser, post_uri: &str, suffix: &str, undo: bool) -> Value {
    let lock = json!({
        "id": format!("{}/lock/{suffix}", group.actor.id),
        "type": "Lock",
        "actor": format!("{}/mod", group.actor.id),
        "object": post_uri,
        "audience": group.actor.id,
    });
    let inner = if undo {
        json!({
            "id": format!("{}/undo-lock/{suffix}", group.actor.id),
            "type": "Undo",
            "actor": format!("{}/mod", group.actor.id),
            "object": lock,
            "audience": group.actor.id,
        })
    } else {
        lock
    };
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}/announces/{suffix}", group.actor.id),
        "type": "Announce",
        "actor": group.actor.id,
        "object": inner,
        "audience": group.actor.id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "cc": [format!("{}/followers", group.actor.id)],
    })
}
