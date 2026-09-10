//! Hosting local groups — the actor core. A local group is a
//! `Group`-typed account with a sidecar row: its actor document carries the
//! FEP-1b12 moderators collection (`attributedTo`), the FEP-5219
//! `affiliations` collection and Lemmy's community flags; membership is the
//! followers collection (open groups auto-accept, approval groups hold the
//! request, outcasts are rejected); groups stay out of the people surfaces
//! (nodeinfo, directory).

mod common;

use std::time::SystemTime;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{RemoteUser, StubFederation, TEST_DOMAIN, create_local_account, test_state_with};
use http_body_util::BodyExt;
use plamenu::actions::{self, EditParams, PostParams};
use plamenu::groups::{CreateGroupParams, create_group, may_create};
use plamenu::state::AppState;
use plamenu::{build_router, delivery};
use plamenu_db::group::{self, Affiliation, MembershipPolicy, PostingPolicy};
use plamenu_db::instance_settings::GroupCreationPolicy;
use plamenu_db::status::NewLocalStatus;
use plamenu_db::{PgPool, account, dislike, favourite, follow, job, mention, report, status};
use plamenu_federation::RequestSigner;
use serde_json::{Value, json};
use tower::ServiceExt;

const HIKING_URI: &str = "https://plamenu.test/users/hiking";

/// A ready state + router around an empty stub federation.
fn harness(pool: PgPool) -> (AppState, Router) {
    let state = test_state_with(pool, StubFederation::with_actors([]));
    let app = build_router(state.clone());
    (state, app)
}

/// Creates `@alice` and the group `!hiking` she owns.
async fn seed_group(state: &AppState, policy: MembershipPolicy) -> (i64, i64) {
    let alice = create_local_account(&state.pool, "alice", "Alice").await;
    // This long-running suite is the permanent compatibility matrix for
    // already-published `/users/:handle` group actors. Production creation is
    // independently covered through `create_group` and now uses immutable
    // numeric IDs; keeping this fixture legacy ensures upgrades do not abandon
    // existing communities while still exercising the normalized encrypted
    // key store used by both identity generations.
    let rsa = plamenu_ap::keys::generate_keypair().unwrap();
    let ed25519 = plamenu_ap::keys::generate_ed25519_keypair();
    let (group_account, _) = group::create(
        &state.pool,
        plamenu_db::group::NewLocalGroup {
            account: plamenu_db::account::NewLocalAccount {
                username: "hiking",
                display_name: "Hiking & trails",
                note: "",
                public_key_pem: &rsa.public_pem,
            },
            membership_policy: policy,
            posting_policy: PostingPolicy::Members,
            created_by: alice.id,
        },
    )
    .await
    .unwrap();
    let keyring = state.federation_keyring.as_deref().unwrap();
    plamenu::key_store::provision_account(
        &state.pool,
        keyring,
        TEST_DOMAIN,
        &group_account,
        &rsa,
        &ed25519,
    )
    .await
    .unwrap();
    follow::create(&state.pool, alice.id, group_account.id, None)
        .await
        .unwrap();
    (alice.id, group_account.id)
}

async fn get_ap(app: &Router, uri: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .uri(uri)
        .header(header::ACCEPT, "application/activity+json")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

async fn get_json(app: &Router, uri: &str) -> (StatusCode, Value) {
    let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

fn follow_group(sender: &RemoteUser, serial: u32) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}/follows/{serial}", sender.actor.id),
        "type": "Follow",
        "actor": sender.actor.id,
        "object": HIKING_URI,
    })
}

async fn post_signed(app: Router, path: &str, body: &Value, signer: &RequestSigner) -> StatusCode {
    let bytes = serde_json::to_vec(body).unwrap();
    let signed_headers = signer.sign_post(TEST_DOMAIN, path, &bytes, SystemTime::now());
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", signed_headers.host)
        .header("date", signed_headers.date)
        .header("digest", signed_headers.digest)
        .header("signature", signed_headers.signature)
        .header("content-type", "application/activity+json")
        .body(Body::from(bytes))
        .unwrap();
    app.oneshot(request).await.unwrap().status()
}

/// Web group creation is capped per account. The one reaching the
/// cap succeeds; the next is refused before any RSA key is generated.
#[sqlx::test(migrations = "../db/migrations")]
async fn create_group_enforces_the_per_account_quota(pool: PgPool) {
    let (state, _app) = harness(pool);
    let alice = create_local_account(&state.pool, "alice", "Alice").await;

    // Seed the account up to one below the cap with cheap dummy-key group rows
    // (no RSA) so the boundary is reached without minting real actor keys.
    let cap = plamenu::groups::MAX_GROUPS_PER_ACCOUNT;
    for i in 0..(cap - 1) {
        let username = format!("seed{i}");
        group::create(
            &state.pool,
            plamenu_db::group::NewLocalGroup {
                account: plamenu_db::account::NewLocalAccount {
                    username: &username,
                    display_name: "",
                    note: "",
                    public_key_pem: "pub",
                },
                membership_policy: MembershipPolicy::Open,
                posting_policy: PostingPolicy::Members,
                created_by: alice.id,
            },
        )
        .await
        .unwrap();
    }
    assert_eq!(
        group::count_created_by(&state.pool, alice.id)
            .await
            .unwrap(),
        cap - 1
    );

    let params = |name: &'static str| CreateGroupParams {
        name,
        display_name: "",
        membership_policy: MembershipPolicy::Open,
        posting_policy: PostingPolicy::Members,
        created_by: alice.id,
        enforce_username_blocklist: false,
        enforce_account_quota: true,
    };

    // The one that reaches the cap still succeeds…
    create_group(&state, params("atthecap")).await.unwrap();
    // …but the next is refused by the quota.
    let err = create_group(&state, params("overthecap"))
        .await
        .unwrap_err();
    // The typed refusal folds back into the exact ApiError the callers saw
    // before it was typed.
    assert!(
        matches!(
            plamenu::error::ApiError::from(err),
            plamenu::error::ApiError::Unprocessable(message) if message.contains("limit")
        ),
        "expected a quota rejection"
    );
}

// ---- The actor document --------------------------------------------------

#[sqlx::test(migrations = "../db/migrations")]
async fn group_actor_document_carries_the_community_contract(pool: PgPool) {
    let (state, app) = harness(pool);
    seed_group(&state, MembershipPolicy::Open).await;

    let (status, actor) = get_ap(&app, "/users/hiking").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(actor["type"], "Group");
    assert_eq!(actor["preferredUsername"], "hiking");
    assert_eq!(actor["name"], "Hiking & trails");
    // Open membership: follows are accepted automatically.
    assert_eq!(actor["manuallyApprovesFollowers"], false);
    // FEP-1b12 mod list + FEP-5219 affiliations.
    assert_eq!(actor["attributedTo"], format!("{HIKING_URI}/moderators"));
    assert_eq!(actor["affiliations"], format!("{HIKING_URI}/affiliations"));
    // Lemmy's community flags, with defaults.
    assert_eq!(actor["postingRestrictedToMods"], false);
    assert_eq!(actor["sensitive"], false);
    // Still a signing-capable actor: keys and inbox as usual.
    assert_eq!(actor["publicKey"]["owner"], HIKING_URI);
    assert_eq!(actor["inbox"], format!("{HIKING_URI}/inbox"));
    assert!(actor["assertionMethod"][0]["publicKeyMultibase"].is_string());

    // Webfinger resolves the group like any local account.
    let (status, jrd) = get_json(
        &app,
        "/.well-known/webfinger?resource=acct:hiking@plamenu.test",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(jrd["subject"], "acct:hiking@plamenu.test");
}

// ---- The mod-list collections ---------------------------------------------

#[sqlx::test(migrations = "../db/migrations")]
async fn moderators_and_affiliations_collections(pool: PgPool) {
    let (state, app) = harness(pool);
    let (_, group_id) = seed_group(&state, MembershipPolicy::Open).await;
    let bob = create_local_account(&state.pool, "bob", "Bob").await;
    let mallory = create_local_account(&state.pool, "mallory", "Mallory").await;
    group::set_affiliation(&state.pool, group_id, bob.id, Affiliation::Moderator, None)
        .await
        .unwrap();
    group::set_affiliation(
        &state.pool,
        group_id,
        mallory.id,
        Affiliation::Outcast,
        None,
    )
    .await
    .unwrap();

    // Moderators: bare actor IRIs, owner first — the shape Lemmy serves.
    let (status, moderators) = get_ap(&app, "/users/hiking/moderators").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(moderators["type"], "OrderedCollection");
    assert_eq!(moderators["totalItems"], 2);
    assert_eq!(
        moderators["orderedItems"],
        json!([
            "https://plamenu.test/users/alice",
            "https://plamenu.test/users/bob",
        ])
    );

    // Affiliations: FEP-5219 Relationship items; owner maps to `admin`,
    // outcasts (bans) are never published.
    let (status, affiliations) = get_ap(&app, "/users/hiking/affiliations").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        affiliations["orderedItems"],
        json!([
            {
                "type": "Relationship",
                "subject": "https://plamenu.test/users/alice",
                "relationship": "admin",
            },
            {
                "type": "Relationship",
                "subject": "https://plamenu.test/users/bob",
                "relationship": "moderator",
            },
        ])
    );

    // People don't have mod lists: 404, like Lemmy's user actors.
    let (status, _) = get_ap(&app, "/users/alice/moderators").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get_ap(&app, "/users/alice/affiliations").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---- Membership on the wire -----------------------------------------------

#[sqlx::test(migrations = "../db/migrations")]
async fn open_group_accepts_follows_and_bans_reject(pool: PgPool) {
    let remote = RemoteUser::new("remote.example", "carl");
    let stub = StubFederation::with_actors([remote.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let (_, group_id) = seed_group(&state, MembershipPolicy::Open).await;

    // Open membership: the follow lands accepted and an Accept goes out,
    // signed by the group actor itself.
    let status = post_signed(
        app.clone(),
        "/users/hiking/inbox",
        &follow_group(&remote, 1),
        &remote.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let carl = account::find_by_uri(&pool, &remote.actor.id)
        .await
        .unwrap()
        .unwrap();
    let edge = follow::find(&pool, carl.id, group_id)
        .await
        .unwrap()
        .unwrap();
    assert!(!edge.pending);
    job::make_all_due(&pool).await.unwrap();
    assert_eq!(delivery::run_due(&state).await, 1);
    let accept = &stub.deliveries()[0];
    assert_eq!(accept.inbox_url, remote.actor.inbox);
    assert_eq!(accept.activity["type"], "Accept");
    assert_eq!(accept.activity["actor"], HIKING_URI);
    assert_eq!(accept.key_id, format!("{HIKING_URI}#main-key"));

    // Ban carl (the ban action will pair these two writes), then a
    // re-follow is rejected and no edge comes back.
    group::set_affiliation(&pool, group_id, carl.id, Affiliation::Outcast, None)
        .await
        .unwrap();
    follow::delete(&pool, carl.id, group_id).await.unwrap();
    let status = post_signed(
        app,
        "/users/hiking/inbox",
        &follow_group(&remote, 2),
        &remote.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(
        follow::find(&pool, carl.id, group_id)
            .await
            .unwrap()
            .is_none()
    );
    job::make_all_due(&pool).await.unwrap();
    assert_eq!(delivery::run_due(&state).await, 1);
    let reject = &stub.deliveries()[1];
    assert_eq!(reject.activity["type"], "Reject");
    assert_eq!(reject.activity["actor"], HIKING_URI);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn approval_group_holds_the_join_request(pool: PgPool) {
    let remote = RemoteUser::new("remote.example", "carl");
    let stub = StubFederation::with_actors([remote.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let (_, group_id) = seed_group(&state, MembershipPolicy::Approval).await;

    // The approval group's actor advertises manual follower approval.
    let (_, actor) = get_ap(&app, "/users/hiking").await;
    assert_eq!(actor["type"], "Group");
    assert_eq!(actor["manuallyApprovesFollowers"], true);

    let status = post_signed(
        app,
        "/users/hiking/inbox",
        &follow_group(&remote, 1),
        &remote.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let carl = account::find_by_uri(&pool, &remote.actor.id)
        .await
        .unwrap()
        .unwrap();
    let edge = follow::find(&pool, carl.id, group_id)
        .await
        .unwrap()
        .unwrap();
    assert!(edge.pending);
    // No Accept until a moderator authorizes.
    assert_eq!(delivery::run_due(&state).await, 0);
    assert!(stub.deliveries().is_empty());
}

// ---- Creation -------------------------------------------------------------

#[sqlx::test(migrations = "../db/migrations")]
async fn creation_seeds_owner_and_membership(pool: PgPool) {
    let (state, _) = harness(pool.clone());
    let (alice_id, group_id) = seed_group(&state, MembershipPolicy::Open).await;
    // The creator owns the group and is already a member of it.
    assert_eq!(
        group::affiliation_of(&pool, group_id, alice_id)
            .await
            .unwrap(),
        Some(Affiliation::Owner)
    );
    assert_eq!(
        group::joined_group_ids(&pool, alice_id).await.unwrap(),
        vec![group_id]
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn creation_validates_names(pool: PgPool) {
    let (state, _) = harness(pool.clone());
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let attempt = |name: &'static str| {
        let state = state.clone();
        let alice_id = alice.id;
        async move {
            create_group(
                &state,
                CreateGroupParams {
                    name,
                    display_name: "",
                    membership_policy: MembershipPolicy::Open,
                    posting_policy: PostingPolicy::Members,
                    created_by: alice_id,
                    enforce_username_blocklist: true,
                    enforce_account_quota: false,
                },
            )
            .await
        }
    };
    assert!(attempt("").await.is_err());
    assert!(attempt("no spaces").await.is_err());
    assert!(
        attempt("name-with-far-too-many-characters-x")
            .await
            .is_err()
    );
    // Group names share the account namespace.
    assert!(attempt("Alice").await.is_err());
    // Reserved names stay reserved for web callers, in both blocklist modes.
    plamenu_db::username_block::create(&pool, "staff", false, false)
        .await
        .unwrap();
    assert!(attempt("staff_hiking").await.is_err());
    assert!(attempt("hiking").await.is_ok());
}

// A pure gate over `GroupCreationPolicy`; no database needed, so it stays a
// plain `#[test]` rather than paying the per-test database + migration cost.
#[test]
fn creation_policy_gates() {
    assert!(may_create(GroupCreationPolicy::Everyone, false));
    assert!(may_create(GroupCreationPolicy::Everyone, true));
    assert!(!may_create(GroupCreationPolicy::Admins, false));
    assert!(may_create(GroupCreationPolicy::Admins, true));
    // `Approved` degrades to staff-only until the grant queue ships.
    assert!(!may_create(GroupCreationPolicy::Approved, false));
    assert!(may_create(GroupCreationPolicy::Approved, true));
}

// ---- Posting & announcing --------------------------------------------------

const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

/// A member's submission: `Create(Page|Note)` addressed to the group the way
/// Lemmy addresses community posts. Returns `(object_uri, create)`.
fn submission(
    author: &RemoteUser,
    serial: u32,
    title: Option<&str>,
    in_reply_to: Option<&str>,
) -> (String, Value) {
    let uri = format!("https://remote.example/objects/{serial}");
    let mut object = json!({
        "id": uri,
        "type": if title.is_some() { "Page" } else { "Note" },
        "attributedTo": author.actor.id,
        "content": "<p>hello group</p>",
        "to": [PUBLIC],
        "cc": [HIKING_URI],
        "audience": HIKING_URI,
        "published": "2026-07-12T09:00:00Z",
    });
    if let Some(title) = title {
        object["name"] = json!(title);
    }
    if let Some(parent) = in_reply_to {
        object["inReplyTo"] = json!(parent);
    }
    let create = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("https://remote.example/activities/create/{serial}"),
        "type": "Create",
        "actor": author.actor.id,
        "to": [PUBLIC],
        "cc": [HIKING_URI],
        "object": object,
    });
    (uri, create)
}

/// An open group with `carl` as accepted remote member; the follow's
/// `Accept` delivery is already flushed so tests count from zero.
async fn seeded_group_with_member(
    pool: &PgPool,
    carl: &RemoteUser,
    stub: &std::sync::Arc<StubFederation>,
    state: &AppState,
    app: &Router,
) -> (i64, i64) {
    let (alice_id, group_id) = seed_group(state, MembershipPolicy::Open).await;
    let status = post_signed(
        app.clone(),
        "/users/hiking/inbox",
        &follow_group(carl, 1),
        &carl.signer(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    job::make_all_due(pool).await.unwrap();
    assert_eq!(delivery::run_due(state).await, 1, "the follow Accept");
    assert_eq!(stub.deliveries().len(), 1);
    let _ = pool;
    (alice_id, group_id)
}

#[sqlx::test(migrations = "../db/migrations")]
async fn member_submission_is_wrapped_verbatim_and_boosted(pool: PgPool) {
    let carl = RemoteUser::new("remote.example", "carl");
    let stub = StubFederation::with_actors([carl.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let (_, group_id) = seeded_group_with_member(&pool, &carl, &stub, &state, &app).await;

    let (uri, create) = submission(&carl, 2, Some("First trail"), None);
    let status = post_signed(app.clone(), "/users/hiking/inbox", &create, &carl.signer()).await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // Ingested with its content fields, attributed to the group, boosted.
    let stored = status::find_by_uri(&pool, &uri).await.unwrap().unwrap();
    assert_eq!(stored.title.as_deref(), Some("First trail"));
    assert!(mention::exists(&pool, stored.id, group_id).await.unwrap());
    let boost = status::find_reblog_by(&pool, group_id, stored.id)
        .await
        .unwrap()
        .expect("top-level public submissions get a boost row");

    // Lemmy's exact double-send: the verbatim wrapper, then the
    // Mastodon-compat bare Announce under the boost row's activity id.
    assert_eq!(delivery::run_due(&state).await, 2);
    let deliveries = stub.deliveries();
    let wrapper = &deliveries[1];
    // Follower fan-out prefers the sharedInbox, like any other delivery.
    assert_eq!(wrapper.inbox_url, "https://remote.example/inbox");
    assert_eq!(wrapper.activity["type"], "Announce");
    assert_eq!(wrapper.activity["actor"], HIKING_URI);
    assert_eq!(wrapper.activity["audience"], HIKING_URI);
    assert!(
        wrapper.activity["id"]
            .as_str()
            .unwrap()
            .starts_with(&format!("{HIKING_URI}#announce/"))
    );
    assert_eq!(
        wrapper.activity["object"], create,
        "FEP-1b12: the inner activity is embedded byte-for-byte as received"
    );
    let compat = &deliveries[2];
    assert_eq!(compat.activity["type"], "Announce");
    assert_eq!(compat.activity["object"], json!(uri));
    assert_eq!(
        compat.activity["id"],
        json!(format!("{HIKING_URI}/statuses/{}/activity", boost.id))
    );

    // A redelivered Create announces nothing again.
    let status = post_signed(app, "/users/hiking/inbox", &create, &carl.signer()).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(delivery::run_due(&state).await, 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn non_member_outcast_and_nonpublic_submissions_never_announce(pool: PgPool) {
    let carl = RemoteUser::new("remote.example", "carl");
    let dave = RemoteUser::new("remote.example", "dave");
    let stub = StubFederation::with_actors([carl.actor.clone(), dave.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let (_, group_id) = seeded_group_with_member(&pool, &carl, &stub, &state, &app).await;

    // dave never joined: his post is ingested as an ordinary status but the
    // group detaches its attribution and announces nothing.
    let (uri, create) = submission(&dave, 2, Some("Drive-by"), None);
    post_signed(app.clone(), "/users/hiking/inbox", &create, &dave.signer()).await;
    let stored = status::find_by_uri(&pool, &uri).await.unwrap().unwrap();
    assert!(!mention::exists(&pool, stored.id, group_id).await.unwrap());
    assert!(
        status::find_reblog_by(&pool, group_id, stored.id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(delivery::run_due(&state).await, 0);

    // carl is banned: outcasts are dropped even as members.
    group::set_affiliation(
        &pool,
        group_id,
        account_id_of(&pool, &carl).await,
        Affiliation::Outcast,
        None,
    )
    .await
    .unwrap();
    let (_, create) = submission(&carl, 3, None, None);
    post_signed(app.clone(), "/users/hiking/inbox", &create, &carl.signer()).await;
    assert_eq!(delivery::run_due(&state).await, 0);

    // A followers-only submission by a member is ingested but never
    // announced — the group must not widen its audience.
    group::remove_affiliation(&pool, group_id, account_id_of(&pool, &carl).await)
        .await
        .unwrap();
    let (uri, mut create) = submission(&carl, 4, None, None);
    create["to"] = json!([format!("{}/followers", carl.actor.id)]);
    create["object"]["to"] = json!([format!("{}/followers", carl.actor.id)]);
    post_signed(app, "/users/hiking/inbox", &create, &carl.signer()).await;
    let stored = status::find_by_uri(&pool, &uri).await.unwrap().unwrap();
    assert_ne!(stored.visibility, "public");
    assert_eq!(delivery::run_due(&state).await, 0);
    assert!(
        status::find_reblog_by(&pool, group_id, stored.id)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn anyone_policy_announces_a_non_member_submission(pool: PgPool) {
    // A group whose posting policy is `anyone` accepts and announces a
    // top-level post from someone who never joined — unlike the `members`
    // default, which detaches it (see `non_member_...`).
    let dave = RemoteUser::new("remote.example", "dave");
    let stub = StubFederation::with_actors([dave.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let (_, group_id) = seed_group(&state, MembershipPolicy::Open).await;
    group::set_posting_policy(&pool, group_id, PostingPolicy::Anyone)
        .await
        .unwrap();

    let (uri, create) = submission(&dave, 7, Some("Drive-by welcome"), None);
    let code = post_signed(app.clone(), "/users/hiking/inbox", &create, &dave.signer()).await;
    assert_eq!(code, StatusCode::ACCEPTED);
    let stored = status::find_by_uri(&pool, &uri).await.unwrap().unwrap();
    // The group keeps the attribution (not detached) and boosts the post —
    // `members` would have dropped both (see `non_member_...`).
    assert!(
        mention::exists(&pool, stored.id, group_id).await.unwrap(),
        "an anyone-policy group accepts a non-member's post"
    );
    assert!(
        status::find_reblog_by(&pool, group_id, stored.id)
            .await
            .unwrap()
            .is_some(),
        "and announces it"
    );
}

async fn account_id_of(pool: &PgPool, user: &RemoteUser) -> i64 {
    account::find_by_uri(pool, &user.actor.id)
        .await
        .unwrap()
        .unwrap()
        .id
}

#[sqlx::test(migrations = "../db/migrations")]
async fn mods_only_gates_threads_but_not_comments(pool: PgPool) {
    let carl = RemoteUser::new("remote.example", "carl");
    let stub = StubFederation::with_actors([carl.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let (alice_id, group_id) = seeded_group_with_member(&pool, &carl, &stub, &state, &app).await;
    group::set_posting_policy(&pool, group_id, PostingPolicy::Mods)
        .await
        .unwrap();

    // A member's thread is dropped in an announcement-style community…
    let (_, create) = submission(&carl, 2, Some("Not allowed"), None);
    post_signed(app.clone(), "/users/hiking/inbox", &create, &carl.signer()).await;
    assert_eq!(delivery::run_due(&state).await, 0);

    // …but a moderator's goes through (the owner posts locally),
    let (thread, _) = actions::post_status(
        &state,
        PostParams {
            username: "alice",
            text: "announcement",
            visibility: "public",
            group_id: Some(group_id),
            title: Some("Welcome"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(delivery::run_due(&state).await, 2, "wrapper + compat");

    // …and comments stay open to everyone, wrapper-only (Lemmy's rule).
    let thread_uri = format!("https://plamenu.test/users/alice/statuses/{}", thread.id);
    let (comment_uri, comment) = submission(&carl, 3, None, Some(&thread_uri));
    post_signed(app, "/users/hiking/inbox", &comment, &carl.signer()).await;
    let stored = status::find_by_uri(&pool, &comment_uri)
        .await
        .unwrap()
        .unwrap();
    assert!(
        status::find_reblog_by(&pool, group_id, stored.id)
            .await
            .unwrap()
            .is_none(),
        "comments never get boost rows"
    );
    assert_eq!(delivery::run_due(&state).await, 1);
    let announce = stub.deliveries().pop().unwrap();
    assert_eq!(announce.activity["type"], "Announce");
    assert_eq!(announce.activity["object"], comment);
    let _ = alice_id;
}

#[sqlx::test(migrations = "../db/migrations")]
async fn author_update_and_delete_travel_wrapped(pool: PgPool) {
    let carl = RemoteUser::new("remote.example", "carl");
    let stub = StubFederation::with_actors([carl.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let (_, group_id) = seeded_group_with_member(&pool, &carl, &stub, &state, &app).await;

    let (uri, create) = submission(&carl, 2, Some("First trail"), None);
    post_signed(app.clone(), "/users/hiking/inbox", &create, &carl.signer()).await;
    let stored = status::find_by_uri(&pool, &uri).await.unwrap().unwrap();
    let boost = status::find_reblog_by(&pool, group_id, stored.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(delivery::run_due(&state).await, 2);

    // The author edits: the Update is re-announced wrapped, verbatim.
    let mut edited_object = create["object"].clone();
    edited_object["content"] = json!("<p>edited</p>");
    edited_object["updated"] = json!("2026-07-12T10:00:00Z");
    let update = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://remote.example/activities/update/2",
        "type": "Update",
        "actor": carl.actor.id,
        "to": [PUBLIC],
        "cc": [HIKING_URI],
        "object": edited_object,
    });
    post_signed(app.clone(), "/users/hiking/inbox", &update, &carl.signer()).await;
    assert_eq!(delivery::run_due(&state).await, 1);
    let announced_update = stub.deliveries().pop().unwrap();
    assert_eq!(announced_update.activity["type"], "Announce");
    assert_eq!(announced_update.activity["object"], update);

    // The author deletes: wrapped Delete + Undo of the compat Announce.
    let delete = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://remote.example/activities/delete/2",
        "type": "Delete",
        "actor": carl.actor.id,
        "to": [PUBLIC],
        "object": uri,
    });
    post_signed(app, "/users/hiking/inbox", &delete, &carl.signer()).await;
    assert!(status::find_by_uri(&pool, &uri).await.unwrap().is_none());
    assert_eq!(delivery::run_due(&state).await, 2);
    let deliveries = stub.deliveries();
    let wrapped_delete = &deliveries[deliveries.len() - 2];
    assert_eq!(wrapped_delete.activity["type"], "Announce");
    assert_eq!(wrapped_delete.activity["object"], delete);
    let undo = &deliveries[deliveries.len() - 1];
    assert_eq!(undo.activity["type"], "Undo");
    assert_eq!(undo.activity["object"]["type"], "Announce");
    assert_eq!(
        undo.activity["object"]["id"],
        json!(format!("{HIKING_URI}/statuses/{}/activity", boost.id))
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn local_titled_post_federates_as_page_and_announces(pool: PgPool) {
    let carl = RemoteUser::new("remote.example", "carl");
    let stub = StubFederation::with_actors([carl.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let (_, group_id) = seeded_group_with_member(&pool, &carl, &stub, &state, &app).await;

    let (stored, _) = actions::post_status(
        &state,
        PostParams {
            username: "alice",
            text: "check this out",
            visibility: "public",
            group_id: Some(group_id),
            title: Some("Trail map"),
            external_url: Some("https://example.com/map"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(stored.title.as_deref(), Some("Trail map"));
    assert_eq!(stored.object_type.as_deref(), Some("Page"));
    assert!(mention::exists(&pool, stored.id, group_id).await.unwrap());

    // The object endpoint serves the Lemmy shape: Page + name + audience +
    // leading Link attachment.
    let (code, object) = get_ap(&app, &format!("/users/alice/statuses/{}", stored.id)).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(object["type"], "Page");
    assert_eq!(object["name"], "Trail map");
    assert_eq!(object["audience"], HIKING_URI);
    assert_eq!(
        object["attachment"][0],
        json!({ "type": "Link", "href": "https://example.com/map" })
    );
    assert!(
        object["cc"]
            .as_array()
            .unwrap()
            .contains(&json!(HIKING_URI))
    );

    // The group's double-send reaches its member; the wrapper embeds the
    // exact Create(Page), the compat form carries the bare uri.
    let boost = status::find_reblog_by(&pool, group_id, stored.id)
        .await
        .unwrap()
        .expect("the group boosts the local submission");
    assert_eq!(delivery::run_due(&state).await, 2);
    let deliveries = stub.deliveries();
    let wrapper = &deliveries[1];
    assert_eq!(wrapper.activity["type"], "Announce");
    assert_eq!(wrapper.activity["actor"], HIKING_URI);
    assert_eq!(wrapper.activity["object"]["type"], "Create");
    assert_eq!(wrapper.activity["object"]["object"]["type"], "Page");
    assert_eq!(wrapper.activity["object"]["object"]["name"], "Trail map");
    let compat = &deliveries[2];
    assert_eq!(
        compat.activity["object"],
        json!(format!(
            "https://plamenu.test/users/alice/statuses/{}",
            stored.id
        ))
    );
    assert_eq!(
        compat.activity["id"],
        json!(format!("{HIKING_URI}/statuses/{}/activity", boost.id))
    );

    // A local reply inherits the group: announced wrapper-only, with the
    // audience stamped on its Note.
    create_local_account(&pool, "bob", "Bob").await;
    let (reply, _) = actions::post_status(
        &state,
        PostParams {
            username: "bob",
            text: "great map",
            visibility: "public",
            in_reply_to_id: Some(stored.id),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(mention::exists(&pool, reply.id, group_id).await.unwrap());
    assert!(
        status::find_reblog_by(&pool, group_id, reply.id)
            .await
            .unwrap()
            .is_none()
    );
    let (_, reply_object) = get_ap(&app, &format!("/users/bob/statuses/{}", reply.id)).await;
    assert_eq!(reply_object["type"], "Note");
    assert_eq!(reply_object["audience"], HIKING_URI);
    assert_eq!(delivery::run_due(&state).await, 1);
    let announce = stub.deliveries().pop().unwrap();
    assert_eq!(announce.activity["type"], "Announce");
    assert_eq!(announce.activity["object"]["type"], "Create");
    assert_eq!(
        announce.activity["object"]["object"]["content"],
        "<p>great map</p>"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn local_group_post_validation(pool: PgPool) {
    let (state, _) = harness(pool.clone());
    let (_, group_id) = seed_group(&state, MembershipPolicy::Open).await;
    create_local_account(&pool, "bob", "Bob").await;

    let attempt = |params: PostParams<'static>| {
        let state = state.clone();
        async move { actions::post_status(&state, params).await }
    };
    let base = || PostParams {
        username: "alice",
        text: "hello",
        visibility: "public",
        ..Default::default()
    };

    // Titles are a group-post feature.
    assert!(
        attempt(PostParams {
            title: Some("No group"),
            ..base()
        })
        .await
        .is_err()
    );
    // A link post needs a title.
    assert!(
        attempt(PostParams {
            group_id: Some(group_id),
            external_url: Some("https://example.com"),
            ..base()
        })
        .await
        .is_err()
    );
    // Group submissions are public.
    assert!(
        attempt(PostParams {
            group_id: Some(group_id),
            visibility: "unlisted",
            ..base()
        })
        .await
        .is_err()
    );
    // Non-members can't open threads.
    assert!(
        attempt(PostParams {
            username: "bob",
            group_id: Some(group_id),
            ..base()
        })
        .await
        .is_err()
    );
    // Overlong titles are rejected.
    assert!(
        attempt(PostParams {
            group_id: Some(group_id),
            title: Some(String::leak("x".repeat(201))),
            ..base()
        })
        .await
        .is_err()
    );
    // The owner's plain group post goes through.
    let (thread, _) = attempt(PostParams {
        group_id: Some(group_id),
        title: Some("Fine"),
        ..base()
    })
    .await
    .unwrap();
    // Replies inherit the group; naming one explicitly is an error.
    assert!(
        attempt(PostParams {
            group_id: Some(group_id),
            in_reply_to_id: Some(thread.id),
            ..base()
        })
        .await
        .is_err()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn local_edit_and_delete_are_reannounced(pool: PgPool) {
    let carl = RemoteUser::new("remote.example", "carl");
    let stub = StubFederation::with_actors([carl.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let (alice_id, group_id) = seeded_group_with_member(&pool, &carl, &stub, &state, &app).await;
    let alice = account::find_by_id(&pool, alice_id).await.unwrap().unwrap();

    let (stored, _) = actions::post_status(
        &state,
        PostParams {
            username: "alice",
            text: "v1",
            visibility: "public",
            group_id: Some(group_id),
            title: Some("Trail map"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let boost = status::find_reblog_by(&pool, group_id, stored.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(delivery::run_due(&state).await, 2);

    // An edit keeps the group attribution and re-announces the Update.
    actions::edit_status(
        &state,
        &alice,
        stored.id,
        EditParams {
            text: Some("v2"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(mention::exists(&pool, stored.id, group_id).await.unwrap());
    assert_eq!(delivery::run_due(&state).await, 1);
    let announced = stub.deliveries().pop().unwrap();
    assert_eq!(announced.activity["type"], "Announce");
    assert_eq!(announced.activity["actor"], HIKING_URI);
    assert_eq!(announced.activity["object"]["type"], "Update");
    assert_eq!(
        announced.activity["object"]["object"]["name"], "Trail map",
        "edits keep the title"
    );

    // Deleting retracts both forms.
    actions::delete_status(&state, &alice, stored.id, actions::DeleteMode::Wipe)
        .await
        .unwrap();
    assert_eq!(delivery::run_due(&state).await, 2);
    let deliveries = stub.deliveries();
    let wrapped_delete = &deliveries[deliveries.len() - 2];
    assert_eq!(wrapped_delete.activity["type"], "Announce");
    assert_eq!(wrapped_delete.activity["object"]["type"], "Delete");
    let undo = &deliveries[deliveries.len() - 1];
    assert_eq!(undo.activity["type"], "Undo");
    assert_eq!(
        undo.activity["object"]["id"],
        json!(format!("{HIKING_URI}/statuses/{}/activity", boost.id))
    );
}

// ---- Votes ------------------------------------------------------------------

fn vote(sender: &RemoteUser, kind: &str, serial: u32, object_uri: &str) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}/{}/{serial}", sender.actor.id, kind.to_lowercase()),
        "type": kind,
        "actor": sender.actor.id,
        "object": object_uri,
        "audience": HIKING_URI,
    })
}

fn undo_of(sender: &RemoteUser, inner: &Value) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}/undo", inner["id"].as_str().unwrap()),
        "type": "Undo",
        "actor": sender.actor.id,
        "object": inner,
    })
}

/// A member's post plus a second remote user, deliveries flushed — the vote
/// tests' starting grid. Returns the stored submission.
async fn seeded_group_post(
    pool: &PgPool,
    carl: &RemoteUser,
    stub: &std::sync::Arc<StubFederation>,
    state: &AppState,
    app: &Router,
) -> status::Status {
    seeded_group_with_member(pool, carl, stub, state, app).await;
    let (uri, create) = submission(carl, 2, Some("Vote on this"), None);
    post_signed(app.clone(), "/users/hiking/inbox", &create, &carl.signer()).await;
    assert_eq!(delivery::run_due(state).await, 2, "wrapper + compat");
    status::find_by_uri(pool, &uri).await.unwrap().unwrap()
}

#[sqlx::test(migrations = "../db/migrations")]
async fn inbound_votes_score_exclude_and_relay(pool: PgPool) {
    let carl = RemoteUser::new("remote.example", "carl");
    let dave = RemoteUser::new("elsewhere.example", "dave");
    let stub = StubFederation::with_actors([carl.actor.clone(), dave.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let target = seeded_group_post(&pool, &carl, &stub, &state, &app).await;

    // An upvote: stored as a favourite, relayed verbatim in the group's
    // Announce wrapper. dave is stored as a side effect of the signed post.
    let like = vote(&dave, "Like", 1, target.uri.as_deref().unwrap());
    post_signed(app.clone(), "/users/hiking/inbox", &like, &dave.signer()).await;
    let dave_id = account_id_of(&pool, &dave).await;
    assert!(
        !favourite::favourited_of(&pool, dave_id, &[target.id])
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(delivery::run_due(&state).await, 1);
    let relayed = &stub.deliveries()[3];
    assert_eq!(relayed.activity["type"], "Announce");
    assert_eq!(relayed.activity["object"], like, "vote embedded verbatim");

    // The downvote displaces it (Lemmy's mutual exclusion) and relays.
    let dislike = vote(&dave, "Dislike", 2, target.uri.as_deref().unwrap());
    post_signed(app.clone(), "/users/hiking/inbox", &dislike, &dave.signer()).await;
    assert!(
        favourite::favourited_of(&pool, dave_id, &[target.id])
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        !dislike::disliked_of(&pool, dave_id, &[target.id])
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(delivery::run_due(&state).await, 1);

    // The entity wears the vote extensions.
    let (code, entity) = get_json(&app, &format!("/api/v1/statuses/{}", target.id)).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(entity["group_post"], json!(true));
    assert_eq!(
        entity["groups"][0]["acct"],
        json!("hiking"),
        "the Status names its community even without the Announce wrapper"
    );
    assert_eq!(entity["downvotes_count"], json!(1));
    assert_eq!(entity["favourites_count"], json!(0));
    assert_eq!(entity["downvoted"], json!(false), "anonymous viewer");

    // A redelivered Dislike must not re-announce.
    post_signed(app.clone(), "/users/hiking/inbox", &dislike, &dave.signer()).await;
    assert_eq!(delivery::run_due(&state).await, 0);

    // Undo(Dislike) retracts the vote and relays the retraction.
    post_signed(
        app.clone(),
        "/users/hiking/inbox",
        &undo_of(&dave, &dislike),
        &dave.signer(),
    )
    .await;
    assert!(
        dislike::disliked_of(&pool, dave_id, &[target.id])
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(delivery::run_due(&state).await, 1);
    let undone = stub.deliveries().last().unwrap().activity.clone();
    assert_eq!(undone["type"], "Announce");
    assert_eq!(undone["object"]["type"], "Undo");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn dislike_off_group_posts_stays_a_reaction_withdrawal(pool: PgPool) {
    let dave = RemoteUser::new("elsewhere.example", "dave");
    let stub = StubFederation::with_actors([dave.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let plain = status::create_local(&pool, NewLocalStatus::new(bob.id, "hi", "public", None))
        .await
        .unwrap();
    let plain_uri = format!("https://{TEST_DOMAIN}/users/bob/statuses/{}", plain.id);

    let like = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}/likes/1", dave.actor.id),
        "type": "Like",
        "actor": dave.actor.id,
        "object": plain_uri,
    });
    post_signed(app.clone(), "/users/bob/inbox", &like, &dave.signer()).await;
    let dave_id = account_id_of(&pool, &dave).await;
    assert!(
        !favourite::favourited_of(&pool, dave_id, &[plain.id])
            .await
            .unwrap()
            .is_empty()
    );

    // Misskey-family legacy: a Dislike here withdraws the favourite and
    // stores nothing.
    let withdrawal = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}/dislikes/1", dave.actor.id),
        "type": "Dislike",
        "actor": dave.actor.id,
        "object": plain_uri,
    });
    post_signed(app.clone(), "/users/bob/inbox", &withdrawal, &dave.signer()).await;
    assert!(
        favourite::favourited_of(&pool, dave_id, &[plain.id])
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        dislike::disliked_of(&pool, dave_id, &[plain.id])
            .await
            .unwrap()
            .is_empty()
    );
    let (_, entity) = get_json(&app, &format!("/api/v1/statuses/{}", plain.id)).await;
    assert_eq!(entity["group_post"], json!(false));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn outcast_votes_are_dropped_at_the_gate(pool: PgPool) {
    let carl = RemoteUser::new("remote.example", "carl");
    let dave = RemoteUser::new("elsewhere.example", "dave");
    let stub = StubFederation::with_actors([carl.actor.clone(), dave.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let target = seeded_group_post(&pool, &carl, &stub, &state, &app).await;
    let group_id = account::find_local_by_username(&pool, "hiking")
        .await
        .unwrap()
        .unwrap()
        .id;

    // Dave joins (which stores his account), then gets banned.
    post_signed(
        app.clone(),
        "/users/hiking/inbox",
        &follow_group(&dave, 9),
        &dave.signer(),
    )
    .await;
    job::make_all_due(&pool).await.unwrap();
    assert_eq!(delivery::run_due(&state).await, 1, "the follow Accept");
    let dave_stored = account_id_of(&pool, &dave).await;
    group::set_affiliation(&pool, group_id, dave_stored, Affiliation::Outcast, None)
        .await
        .unwrap();

    let like = vote(&dave, "Like", 1, target.uri.as_deref().unwrap());
    post_signed(app.clone(), "/users/hiking/inbox", &like, &dave.signer()).await;
    assert!(
        favourite::favourited_of(&pool, dave_stored, &[target.id])
            .await
            .unwrap()
            .is_empty(),
        "an outcast's vote is dropped, not stored"
    );
    let dislike = vote(&dave, "Dislike", 2, target.uri.as_deref().unwrap());
    post_signed(app, "/users/hiking/inbox", &dislike, &dave.signer()).await;
    assert!(
        dislike::disliked_of(&pool, dave_stored, &[target.id])
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(delivery::run_due(&state).await, 0, "nothing relayed");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn local_votes_federate_to_author_and_group(pool: PgPool) {
    let carl = RemoteUser::new("remote.example", "carl");
    let stub = StubFederation::with_actors([carl.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let target = seeded_group_post(&pool, &carl, &stub, &state, &app).await;
    let bob = create_local_account(&pool, "bob", "Bob").await;
    // `Like`/`Dislike` to the remote author are held back briefly so a quick
    // retraction can cancel them in-queue; force everything due before each
    // flush so both the author copy and the (immediate) group relay land.
    let flush = |cursor: usize| {
        let state = state.clone();
        let stub = stub.clone();
        let pool = pool.clone();
        async move {
            job::make_all_due(&pool).await.unwrap();
            delivery::run_due(&state).await;
            let all = stub.deliveries();
            all[cursor..]
                .iter()
                .map(|d| {
                    let outer = d.activity["type"].as_str().unwrap();
                    let inner = d.activity["object"]["type"].as_str().unwrap_or("");
                    (
                        format!("{outer}({inner})"),
                        d.inbox_url.clone(),
                        d.activity.clone(),
                    )
                })
                .collect::<Vec<_>>()
        }
    };
    let mut cursor = stub.deliveries().len();

    // Upvote: the Like to the author plus the group's relay of it.
    actions::favourite_status(&state, &bob, target.id)
        .await
        .unwrap();
    let sent = flush(cursor).await;
    cursor += sent.len();
    assert!(
        sent.iter()
            .any(|(kind, inbox, _)| kind == "Like()" && inbox.starts_with("https://remote.example")),
        "the author's copy: {sent:?}"
    );
    assert!(
        sent.iter().any(|(kind, _, _)| kind == "Announce(Like)"),
        "the group's relay: {sent:?}"
    );

    // Downvote: displaces the upvote (Undo(Like) + its relay), then the
    // Dislike to the author and its relay.
    actions::downvote_status(&state, &bob, target.id)
        .await
        .unwrap();
    assert!(
        favourite::favourited_of(&pool, bob.id, &[target.id])
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        !dislike::disliked_of(&pool, bob.id, &[target.id])
            .await
            .unwrap()
            .is_empty()
    );
    let sent = flush(cursor).await;
    cursor += sent.len();
    let kinds: Vec<&str> = sent.iter().map(|(k, _, _)| k.as_str()).collect();
    assert!(kinds.contains(&"Undo(Like)"), "{kinds:?}");
    assert!(
        kinds.contains(&"Announce(Undo)"),
        "the Undo(Like) relay: {kinds:?}"
    );
    assert!(kinds.contains(&"Dislike()"), "{kinds:?}");
    assert!(kinds.contains(&"Announce(Dislike)"), "{kinds:?}");
    // The author's Dislike copy wears bob's marker-fragment id.
    let dislike_copy = sent
        .iter()
        .find(|(kind, _, _)| kind == "Dislike()")
        .map(|(_, _, activity)| activity.clone())
        .unwrap();
    assert!(
        dislike_copy["id"]
            .as_str()
            .unwrap()
            .starts_with(&format!("https://{TEST_DOMAIN}/users/bob#dislikes/"))
    );

    // Retract: Undo(Dislike) to the author plus the relay.
    actions::undownvote_status(&state, &bob, target.id)
        .await
        .unwrap();
    let sent = flush(cursor).await;
    assert!(
        dislike::disliked_of(&pool, bob.id, &[target.id])
            .await
            .unwrap()
            .is_empty()
    );
    let kinds: Vec<&str> = sent.iter().map(|(k, _, _)| k.as_str()).collect();
    assert!(kinds.contains(&"Undo(Dislike)"), "{kinds:?}");
    assert!(
        kinds.contains(&"Announce(Undo)"),
        "the Undo(Dislike) relay: {kinds:?}"
    );

    // Downvoting an ordinary status is refused.
    let plain = status::create_local(&pool, NewLocalStatus::new(bob.id, "hi", "public", None))
        .await
        .unwrap();
    let err = actions::downvote_status(&state, &bob, plain.id)
        .await
        .unwrap_err();
    assert!(matches!(err, plamenu::error::ApiError::Unprocessable(_)));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn ranked_sorts_order_by_votes(pool: PgPool) {
    let (state, app) = harness(pool.clone());
    let (alice_id, group_id) = seed_group(&state, MembershipPolicy::Open).await;

    // Three submissions, oldest first; voters are throwaway locals.
    let mut posts = Vec::new();
    for i in 0..3 {
        let item = status::create_local(
            &pool,
            NewLocalStatus::new(alice_id, &format!("post {i}"), "public", None),
        )
        .await
        .unwrap();
        mention::attach(&pool, item.id, group_id, true)
            .await
            .unwrap();
        status::create_local_reblog(&pool, group_id, item.id)
            .await
            .unwrap();
        posts.push(item.id);
    }
    let mut voters = Vec::new();
    for i in 0..6 {
        voters.push(
            create_local_account(&pool, &format!("voter{i}"), "V")
                .await
                .id,
        );
    }
    // posts[0] (oldest): score 5. posts[1]: score 2 (3 up, 1 down).
    // posts[2] (newest): score -1.
    for voter in &voters[..5] {
        favourite::create(&pool, *voter, posts[0], None)
            .await
            .unwrap();
    }
    for voter in &voters[..3] {
        favourite::create(&pool, *voter, posts[1], None)
            .await
            .unwrap();
    }
    dislike::create(&pool, voters[5], posts[1], None)
        .await
        .unwrap();
    dislike::create(&pool, voters[0], posts[2], None)
        .await
        .unwrap();

    let reblogged_ids = |feed: &Value| -> Vec<String> {
        feed.as_array()
            .unwrap()
            .iter()
            .map(|e| e["reblog"]["id"].as_str().unwrap().to_owned())
            .collect()
    };

    // New: reverse insertion order, boost rows of the group.
    let (code, feed) = get_json(&app, &format!("/api/v1/accounts/{group_id}/statuses")).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        reblogged_ids(&feed),
        vec![
            posts[2].to_string(),
            posts[1].to_string(),
            posts[0].to_string()
        ]
    );

    // Top (all time): by score.
    let (_, feed) = get_json(
        &app,
        &format!("/api/v1/accounts/{group_id}/statuses?sort=top&t=all"),
    )
    .await;
    assert_eq!(
        reblogged_ids(&feed),
        vec![
            posts[0].to_string(),
            posts[1].to_string(),
            posts[2].to_string()
        ]
    );

    // Top over a day window excludes a backdated target.
    sqlx::query("UPDATE statuses SET created_at = now() - interval '3 days' WHERE id = $1")
        .bind(posts[0])
        .execute(&pool)
        .await
        .unwrap();
    let (_, feed) = get_json(
        &app,
        &format!("/api/v1/accounts/{group_id}/statuses?sort=top&t=day"),
    )
    .await;
    assert_eq!(
        reblogged_ids(&feed),
        vec![posts[1].to_string(), posts[2].to_string()]
    );

    // Hot: same-age posts rank by score; the 3-day-old score-5 post decays
    // below the fresh score-2 one (half-life math, Lemmy's rank).
    let (_, feed) = get_json(
        &app,
        &format!("/api/v1/accounts/{group_id}/statuses?sort=hot"),
    )
    .await;
    let hot = reblogged_ids(&feed);
    assert_eq!(hot[0], posts[1].to_string(), "{hot:?}");

    // The ranked sorts page by offset.
    let (_, feed) = get_json(
        &app,
        &format!("/api/v1/accounts/{group_id}/statuses?sort=top&t=all&limit=2&page=1"),
    )
    .await;
    assert_eq!(reblogged_ids(&feed), vec![posts[2].to_string()]);
}

// ---- People surfaces exclude groups ----------------------------------------

#[sqlx::test(migrations = "../db/migrations")]
async fn groups_stay_out_of_user_counts_and_directory(pool: PgPool) {
    let (state, app) = harness(pool.clone());
    seed_group(&state, MembershipPolicy::Open).await;

    // nodeinfo counts people, not groups.
    let (status, nodeinfo) = get_json(&app, "/nodeinfo/2.0").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(nodeinfo["usage"]["users"]["total"], 1);

    // The people directory skips groups even though they're discoverable.
    let hiking = account::find_local_by_username(&pool, "hiking")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(hiking.discoverable, Some(true));
    let (status, directory) = get_json(&app, "/api/v1/directory?local=true").await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = directory
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["username"].as_str())
        .collect();
    assert!(names.contains(&"alice"));
    assert!(!names.contains(&"hiking"));
}

// ===========================================================================
// Membership & moderation
// ===========================================================================

/// Adds a second remote member (both remote.example, so they share one inbox —
/// deliveries dedup to one). Flushes the Accept.
async fn add_member(app: &Router, state: &AppState, user: &RemoteUser, serial: u32) {
    post_signed(
        app.clone(),
        "/users/hiking/inbox",
        &follow_group(user, serial),
        &user.signer(),
    )
    .await;
    job::make_all_due(&state.pool).await.unwrap();
    assert_eq!(delivery::run_due(state).await, 1, "the member's Accept");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn approval_join_queue_authorizes_and_rejects(pool: PgPool) {
    let carl = RemoteUser::new("remote.example", "carl");
    let dave = RemoteUser::new("remote.example", "dave");
    let stub = StubFederation::with_actors([carl.actor.clone(), dave.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let (_alice, group_id) = seed_group(&state, MembershipPolicy::Approval).await;

    // Both requests are held pending; approval groups never auto-Accept and a
    // group is never notified against itself.
    for (u, s) in [(&carl, 1u32), (&dave, 2)] {
        post_signed(
            app.clone(),
            "/users/hiking/inbox",
            &follow_group(u, s),
            &u.signer(),
        )
        .await;
    }
    assert_eq!(
        delivery::run_due(&state).await,
        0,
        "approval holds the request"
    );
    let carl_id = account_id_of(&pool, &carl).await;
    let dave_id = account_id_of(&pool, &dave).await;
    assert_eq!(
        follow::pending_state(&pool, carl_id, group_id)
            .await
            .unwrap(),
        Some(true)
    );

    let group_account = account::find_by_id(&pool, group_id).await.unwrap().unwrap();
    let carl_account = account::find_by_id(&pool, carl_id).await.unwrap().unwrap();
    let dave_account = account::find_by_id(&pool, dave_id).await.unwrap().unwrap();

    // Approve carl → membership accepted, the group signs the Accept.
    actions::authorize_follow_request(&state, &group_account, &carl_account)
        .await
        .unwrap();
    assert_eq!(
        follow::pending_state(&pool, carl_id, group_id)
            .await
            .unwrap(),
        Some(false)
    );
    assert_eq!(delivery::run_due(&state).await, 1);
    let accept = stub.deliveries().pop().unwrap();
    assert_eq!(accept.activity["type"], "Accept");
    assert_eq!(accept.activity["actor"], HIKING_URI);

    // Reject dave → the request (and any notification) disappears, the group
    // signs the Reject.
    actions::reject_follow_request(&state, &group_account, &dave_account)
        .await
        .unwrap();
    assert_eq!(
        follow::pending_state(&pool, dave_id, group_id)
            .await
            .unwrap(),
        None
    );
    assert_eq!(delivery::run_due(&state).await, 1);
    assert_eq!(stub.deliveries().pop().unwrap().activity["type"], "Reject");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn ban_severs_membership_and_unban_reverses(pool: PgPool) {
    let carl = RemoteUser::new("remote.example", "carl");
    let dave = RemoteUser::new("remote.example", "dave");
    let stub = StubFederation::with_actors([carl.actor.clone(), dave.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let (alice_id, group_id) = seeded_group_with_member(&pool, &carl, &stub, &state, &app).await;
    add_member(&app, &state, &dave, 5).await; // a bystander member keeps a follower around

    let alice = account::find_by_id(&pool, alice_id).await.unwrap().unwrap();
    let carl_id = account_id_of(&pool, &carl).await;
    let carl_account = account::find_by_id(&pool, carl_id).await.unwrap().unwrap();
    let group_account = account::find_by_id(&pool, group_id).await.unwrap().unwrap();

    plamenu::groups::ban_member(
        &state,
        &group_account,
        &alice,
        &carl_account,
        None,
        Some("spam"),
    )
    .await
    .unwrap();
    assert_eq!(
        group::affiliation_of(&pool, group_id, carl_id)
            .await
            .unwrap(),
        Some(Affiliation::Outcast)
    );
    assert!(
        follow::find(&pool, carl_id, group_id)
            .await
            .unwrap()
            .is_none(),
        "kicked"
    );
    assert_eq!(
        delivery::run_due(&state).await,
        1,
        "the wrapped Block to followers"
    );
    let ann = stub.deliveries().pop().unwrap();
    assert_eq!(ann.activity["type"], "Announce");
    assert_eq!(ann.activity["object"]["type"], "Block");
    assert_eq!(ann.activity["object"]["target"], HIKING_URI);
    assert_eq!(ann.activity["object"]["summary"], "spam");

    // An outcast's re-follow is rejected like a block.
    post_signed(
        app.clone(),
        "/users/hiking/inbox",
        &follow_group(&carl, 9),
        &carl.signer(),
    )
    .await;
    job::make_all_due(&pool).await.unwrap();
    assert_eq!(delivery::run_due(&state).await, 1);
    assert_eq!(stub.deliveries().pop().unwrap().activity["type"], "Reject");

    // Unban lifts the outcast and announces the Undo(Block) to remaining members.
    plamenu::groups::unban_member(&state, &group_account, &alice, &carl_account)
        .await
        .unwrap();
    assert_eq!(
        group::affiliation_of(&pool, group_id, carl_id)
            .await
            .unwrap(),
        None
    );
    assert_eq!(delivery::run_due(&state).await, 1);
    let undo = stub.deliveries().pop().unwrap();
    assert_eq!(undo.activity["object"]["type"], "Undo");
    assert_eq!(undo.activity["object"]["object"]["type"], "Block");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn moderator_remove_retracts_boost_and_announces_delete(pool: PgPool) {
    let carl = RemoteUser::new("remote.example", "carl");
    let stub = StubFederation::with_actors([carl.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let (alice_id, group_id) = seeded_group_with_member(&pool, &carl, &stub, &state, &app).await;
    let alice = account::find_by_id(&pool, alice_id).await.unwrap().unwrap();
    let group_account = account::find_by_id(&pool, group_id).await.unwrap().unwrap();

    let (uri, create) = submission(&carl, 2, Some("Spammy trail"), None);
    post_signed(app.clone(), "/users/hiking/inbox", &create, &carl.signer()).await;
    assert_eq!(delivery::run_due(&state).await, 2, "wrapper + compat");
    let stored = status::find_by_uri(&pool, &uri).await.unwrap().unwrap();
    assert!(
        status::find_reblog_by(&pool, group_id, stored.id)
            .await
            .unwrap()
            .is_some()
    );

    plamenu::groups::remove_from_group(&state, &group_account, &alice, &stored, "spam")
        .await
        .unwrap();
    // The boost row and group attribution are gone; the status itself survives.
    assert!(
        status::find_reblog_by(&pool, group_id, stored.id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(!mention::exists(&pool, stored.id, group_id).await.unwrap());
    assert!(status::find_by_uri(&pool, &uri).await.unwrap().is_some());

    assert_eq!(
        delivery::run_due(&state).await,
        2,
        "wrapped Delete + compat Undo(Announce)"
    );
    let deliveries = stub.deliveries();
    let wrapped = deliveries
        .iter()
        .find(|d| d.activity["object"]["type"] == "Delete")
        .expect("the mod-removal Delete, announced");
    assert_eq!(wrapped.activity["type"], "Announce");
    assert_eq!(wrapped.activity["object"]["summary"], "spam");
    assert!(
        deliveries
            .iter()
            .any(|d| d.activity["type"] == "Undo" && d.activity["object"]["type"] == "Announce"),
        "the compat boost is retracted for plain-Announce consumers"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn locked_thread_drops_comments_until_reopened(pool: PgPool) {
    let carl = RemoteUser::new("remote.example", "carl");
    let stub = StubFederation::with_actors([carl.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let (alice_id, group_id) = seeded_group_with_member(&pool, &carl, &stub, &state, &app).await;
    let alice = account::find_by_id(&pool, alice_id).await.unwrap().unwrap();
    let group_account = account::find_by_id(&pool, group_id).await.unwrap().unwrap();

    let (root_uri, create) = submission(&carl, 2, Some("Locked topic"), None);
    post_signed(app.clone(), "/users/hiking/inbox", &create, &carl.signer()).await;
    assert_eq!(delivery::run_due(&state).await, 2);
    let root = status::find_by_uri(&pool, &root_uri)
        .await
        .unwrap()
        .unwrap();

    // Lock the thread: the Lock is announced.
    plamenu::groups::set_thread_lock(&state, &group_account, &alice, &root, true)
        .await
        .unwrap();
    assert!(
        group::thread_locked(&pool, group_id, root.id)
            .await
            .unwrap()
    );
    assert_eq!(delivery::run_due(&state).await, 1);
    assert_eq!(
        stub.deliveries().pop().unwrap().activity["object"]["type"],
        "Lock"
    );

    // A comment into the locked thread is dropped: no group attribution, no announce.
    let (comment_uri, comment) = submission(&carl, 3, None, Some(&root_uri));
    post_signed(app.clone(), "/users/hiking/inbox", &comment, &carl.signer()).await;
    let stored_comment = status::find_by_uri(&pool, &comment_uri)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !mention::exists(&pool, stored_comment.id, group_id)
            .await
            .unwrap()
    );
    assert_eq!(
        delivery::run_due(&state).await,
        0,
        "locked comment announces nothing"
    );

    // Reopen, then a new comment threads through and is announced.
    plamenu::groups::set_thread_lock(&state, &group_account, &alice, &root, false)
        .await
        .unwrap();
    assert!(
        !group::thread_locked(&pool, group_id, root.id)
            .await
            .unwrap()
    );
    assert_eq!(delivery::run_due(&state).await, 1, "the Undo(Lock)");
    let (_, comment2) = submission(&carl, 4, None, Some(&root_uri));
    post_signed(
        app.clone(),
        "/users/hiking/inbox",
        &comment2,
        &carl.signer(),
    )
    .await;
    assert_eq!(
        delivery::run_due(&state).await,
        1,
        "the reopened comment is announced"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn community_flag_files_a_group_scoped_report(pool: PgPool) {
    let carl = RemoteUser::new("remote.example", "carl");
    let dave = RemoteUser::new("remote.example", "dave");
    let stub = StubFederation::with_actors([carl.actor.clone(), dave.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let (_alice, group_id) = seeded_group_with_member(&pool, &carl, &stub, &state, &app).await;

    let (post_uri, create) = submission(&carl, 2, Some("Report me"), None);
    post_signed(app.clone(), "/users/hiking/inbox", &create, &carl.signer()).await;
    assert_eq!(delivery::run_due(&state).await, 2);

    // dave reports the post to the community (Lemmy carries the reason in `summary`).
    let flag = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://remote.example/activities/flag/1",
        "type": "Flag",
        "actor": dave.actor.id,
        "object": post_uri,
        "audience": HIKING_URI,
        "to": [HIKING_URI],
        "summary": "off-topic spam",
    });
    let status = post_signed(app.clone(), "/users/hiking/inbox", &flag, &dave.signer()).await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let filter = report::AdminReportFilter {
        unresolved: true,
        group_account_id: Some(group_id),
        limit: 10,
        ..report::AdminReportFilter::default()
    };
    let reports = report::list_for_admin(&pool, &filter).await.unwrap();
    assert_eq!(
        reports.len(),
        1,
        "the community report is filed and scoped to the group"
    );
    assert_eq!(reports[0].group_account_id, Some(group_id));
    assert_eq!(reports[0].comment, "off-topic spam");
    // The reported post's author (carl) is the report target.
    assert_eq!(
        reports[0].target_account_id,
        account_id_of(&pool, &carl).await
    );
    // A group-scoped report doesn't leak into the instance-staff-only default view.
    assert_eq!(report::count_unresolved(&pool).await.unwrap(), 1);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn remote_moderator_ban_applies_and_reannounces(pool: PgPool) {
    let carl = RemoteUser::new("remote.example", "carl");
    let erin = RemoteUser::new("remote.example", "erin"); // a remote co-moderator
    let stub = StubFederation::with_actors([carl.actor.clone(), erin.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let (_alice, group_id) = seeded_group_with_member(&pool, &carl, &stub, &state, &app).await;
    add_member(&app, &state, &erin, 5).await;

    // Promote erin to moderator (owner action would federate this; here we set
    // the affiliation directly and exercise the inbound acceptance).
    let erin_id = account_id_of(&pool, &erin).await;
    group::set_affiliation(&pool, group_id, erin_id, Affiliation::Moderator, None)
        .await
        .unwrap();
    let carl_id = account_id_of(&pool, &carl).await;

    // erin bans carl by delivering a Block addressed to the community.
    let block = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://remote.example/activities/block/1",
        "type": "Block",
        "actor": erin.actor.id,
        "object": carl.actor.id,
        "target": HIKING_URI,
        "audience": HIKING_URI,
        "cc": [HIKING_URI],
        "to": [PUBLIC],
        "removeData": false,
    });
    let status = post_signed(app.clone(), "/users/hiking/inbox", &block, &erin.signer()).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(
        group::affiliation_of(&pool, group_id, carl_id)
            .await
            .unwrap(),
        Some(Affiliation::Outcast)
    );
    assert!(
        follow::find(&pool, carl_id, group_id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        delivery::run_due(&state).await,
        1,
        "re-announced to members"
    );
    let ann = stub.deliveries().pop().unwrap();
    assert_eq!(ann.activity["type"], "Announce");
    assert_eq!(ann.activity["object"]["type"], "Block");
}

// ---- Profile updates, deletion, owner transfer ----------------------------

#[sqlx::test(migrations = "../db/migrations")]
async fn group_profile_update_double_sends(pool: PgPool) {
    let carl = RemoteUser::new("remote.example", "carl");
    let stub = StubFederation::with_actors([carl.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let (_, group_id) = seeded_group_with_member(&pool, &carl, &stub, &state, &app).await;
    let group_account = account::find_by_id(&pool, group_id).await.unwrap().unwrap();

    plamenu::groups::update_settings(
        &state,
        &group_account,
        plamenu::groups::GroupSettings {
            display_name: "Hiking Club",
            note_html: "<p>trails</p>",
            note_source: "trails",
            policy: MembershipPolicy::Open,
            sensitive: false,
            posting_policy: PostingPolicy::Members,
            discoverable: true,
            profile: plamenu::groups::GroupProfileEdit::default(),
        },
    )
    .await
    .unwrap();

    // Lemmy double-send: a plain Update(Actor) so Mastodon followers refresh
    // the group actor, and the Lemmy-shaped Announce(Update(Group)) authored by
    // the owner so subscribing communities accept it.
    assert_eq!(delivery::run_due(&state).await, 2);
    let deliveries = stub.deliveries();
    let plain = deliveries
        .iter()
        .find(|d| d.activity["type"] == "Update")
        .expect("plain Update(Actor)");
    assert_eq!(plain.activity["actor"], HIKING_URI);
    assert_eq!(plain.activity["object"]["type"], "Group");
    assert_eq!(plain.activity["object"]["name"], "Hiking Club");

    let wrapped = deliveries
        .iter()
        .find(|d| d.activity["type"] == "Announce" && d.activity["object"]["type"] == "Update")
        .expect("Announce(Update(Group))");
    assert_eq!(wrapped.activity["actor"], HIKING_URI);
    assert_eq!(wrapped.activity["audience"], HIKING_URI);
    let inner = &wrapped.activity["object"];
    assert_eq!(
        inner["actor"], "https://plamenu.test/users/alice",
        "the owner is the acting moderator (Lemmy's verify_mod_action)"
    );
    assert_eq!(inner["object"]["type"], "Group");
    assert_eq!(inner["object"]["preferredUsername"], "hiking");
    assert_eq!(inner["object"]["name"], "Hiking Club");
    assert!(
        inner["object"].get("@context").is_none(),
        "the inner Group's own @context is stripped (the wrapper carries one)"
    );
    assert_eq!(inner["cc"], json!([HIKING_URI]));
    assert_eq!(inner["to"], json!([PUBLIC]));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn group_delete_double_sends_and_tombstones(pool: PgPool) {
    let carl = RemoteUser::new("remote.example", "carl");
    let stub = StubFederation::with_actors([carl.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let (_, group_id) = seeded_group_with_member(&pool, &carl, &stub, &state, &app).await;
    let group_account = account::find_by_id(&pool, group_id).await.unwrap().unwrap();

    plamenu::groups::delete_group(&state, &group_account)
        .await
        .unwrap();

    // The wrapped Announce(Delete(Group)) for Lemmy and the plain Delete(Actor)
    // for Mastodon, both fanned out before the purge.
    assert_eq!(delivery::run_due(&state).await, 2);
    let deliveries = stub.deliveries();
    let wrapped = deliveries
        .iter()
        .find(|d| d.activity["type"] == "Announce" && d.activity["object"]["type"] == "Delete")
        .expect("Announce(Delete(Group))");
    assert_eq!(
        wrapped.activity["object"]["actor"],
        "https://plamenu.test/users/alice"
    );
    assert_eq!(wrapped.activity["object"]["object"]["type"], "Tombstone");
    assert_eq!(wrapped.activity["object"]["object"]["id"], HIKING_URI);
    let plain = deliveries
        .iter()
        .find(|d| d.activity["type"] == "Delete")
        .expect("plain Delete(Actor)");
    assert_eq!(plain.activity["actor"], HIKING_URI);
    assert_eq!(plain.activity["object"]["type"], "Tombstone");

    // Tombstoned + suspended → dropped from the public directory.
    let after = account::find_by_id(&pool, group_id).await.unwrap().unwrap();
    assert!(after.suspended());
    assert!(account::is_deleted(&pool, group_id).await.unwrap());
    assert!(
        !group::local_group_ids(&pool, false, 10, 0)
            .await
            .unwrap()
            .contains(&group_id),
        "a deleted group leaves the directory"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn owner_transfer_flips_affiliations_and_announces(pool: PgPool) {
    let carl = RemoteUser::new("remote.example", "carl");
    let stub = StubFederation::with_actors([carl.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let app = build_router(state.clone());
    let (alice_id, group_id) = seeded_group_with_member(&pool, &carl, &stub, &state, &app).await;
    let group_account = account::find_by_id(&pool, group_id).await.unwrap().unwrap();

    // bob is a local, accepted member — not yet a moderator.
    let bob = create_local_account(&pool, "bob", "Bob").await;
    follow::create(&pool, bob.id, group_id, None).await.unwrap();

    plamenu::groups::transfer_owner(&state, &group_account, &bob)
        .await
        .unwrap();

    // Ownership moves to bob; the previous owner stays on as a moderator.
    assert_eq!(
        group::affiliation_of(&pool, group_id, bob.id)
            .await
            .unwrap(),
        Some(Affiliation::Owner)
    );
    assert_eq!(
        group::affiliation_of(&pool, group_id, alice_id)
            .await
            .unwrap(),
        Some(Affiliation::Moderator)
    );

    // bob wasn't a moderator, so an Add(moderators) is announced; a wrapped
    // Update(Group) then tells FEP-5219/Lemmy peers to re-read the affiliations
    // collection (owner change is meaningless to Mastodon, so no plain Update).
    assert_eq!(delivery::run_due(&state).await, 2);
    let deliveries = stub.deliveries();
    let add = deliveries
        .iter()
        .find(|d| d.activity["type"] == "Announce" && d.activity["object"]["type"] == "Add")
        .expect("Announce(Add(moderators))");
    assert_eq!(
        add.activity["object"]["object"],
        "https://plamenu.test/users/bob"
    );
    assert_eq!(
        add.activity["object"]["target"],
        "https://plamenu.test/users/hiking/moderators"
    );
    assert!(
        deliveries
            .iter()
            .any(|d| d.activity["type"] == "Announce" && d.activity["object"]["type"] == "Update"),
        "the affiliations refresh is announced too"
    );
}
