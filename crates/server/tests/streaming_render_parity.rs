//! Viewer-set renderer parity: the viewer-set renderer must produce documents
//! **byte-identical** to the singular per-viewer render across every
//! viewer-dependent dimension (flags, filters, poll votes, RSVP state,
//! tagged-collection visibility, quote policy, the direct-media opt-in, the
//! viewer-relative author entity of a migrated account), and the streaming
//! routes must deliver exactly those documents — a list stream reusing its
//! owner's home render rather than paying a second one.

mod common;

use std::sync::Arc;

use common::{create_local_account, test_state_with};
use plamenu::streaming::Channel;
use plamenu::{AppState, actions, entities};
use plamenu_db::account::RemoteAccountData;
use plamenu_db::collection::{NewCollection, NewCollectionItem};
use plamenu_db::custom_filter::NewKeyword;
use plamenu_db::preview_card::NewPreviewCard;
use plamenu_db::status::{NewLocalStatus, Status};
use plamenu_db::streaming::Event;
use plamenu_db::user::UserSettings;
use plamenu_db::{
    PgPool, account, announcement, bookmark, collection, conversation, custom_filter, dislike,
    favourite, follow, id, list, pin, poll, preview_card, status, status_event,
    status_participation, tagged_object, user,
};
use serde_json::Value;

const DOMAIN: &str = "plamenu.test";

/// A stored remote account on `domain` — `moved_to_uri` resolution matches
/// the `uri` column, which local accounts leave NULL.
async fn remote_account(
    pool: &PgPool,
    domain: &str,
    username: &str,
) -> plamenu_db::account::Account {
    let uri = format!("https://{domain}/users/{username}");
    account::upsert_remote(
        pool,
        RemoteAccountData {
            username,
            domain,
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
            discoverable: true,
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

async fn post(state: &AppState, params: actions::PostParams<'_>) -> Status {
    let (stored, _) = actions::post_status(state, params).await.unwrap();
    stored
}

struct Fixtures {
    statuses: Vec<Status>,
    /// alice (author), fan (flags + follows alice), filtered (keyword filter),
    /// `direct_media` (opt-in + poll voter + RSVP).
    viewers: Vec<i64>,
}

/// One instance-wide fixture set touching every viewer-dependent render
/// dimension at once.
#[allow(clippy::too_many_lines)] // one linear fixture script, each block one dimension
async fn rich_fixtures(state: &AppState, pool: &PgPool) -> Fixtures {
    let alice = create_local_account(pool, "alice", "Alice :party:").await;
    let fan = create_local_account(pool, "fan", "Fan").await;
    let filtered = create_local_account(pool, "filtered", "Filtered").await;
    let direct_media = create_local_account(pool, "directm", "DirectMedia").await;
    plamenu_db::custom_emoji::create_local(pool, "party", "party.png", "image/png", 0, None)
        .await
        .unwrap()
        .unwrap();

    // fan follows alice — the quote policy's `followers` branch and the
    // feature-approval edges must differ from the other viewers'.
    follow::create(pool, fan.id, alice.id, None).await.unwrap();

    // The direct-media viewer opted into the origin-fallback proxy URLs.
    let direct_user = user::create(pool, direct_media.id, None, "hash")
        .await
        .unwrap();
    user::update_settings(
        pool,
        direct_user.id,
        UserSettings {
            reading_allow_direct_remote_media: true,
            ..UserSettings::default()
        },
    )
    .await
    .unwrap();

    // The filtered viewer hides "banana" posts by keyword.
    custom_filter::create(
        pool,
        filtered.id,
        "no fruit",
        "warn",
        &["home".to_owned()],
        None,
        &[NewKeyword {
            keyword: "banana".to_owned(),
            whole_word: true,
        }],
    )
    .await
    .unwrap();

    let mut statuses = Vec::new();

    // 1. A rich plain post: tag + mention + custom emoji + the filter word.
    let plain = post(
        state,
        actions::PostParams {
            username: "alice",
            text: "banana :party: #parity hello @fan",
            visibility: "public",
            ..Default::default()
        },
    )
    .await;
    favourite::create(pool, fan.id, plain.id, None)
        .await
        .unwrap();
    bookmark::create(pool, fan.id, plain.id).await.unwrap();
    dislike::create(pool, filtered.id, plain.id, None)
        .await
        .unwrap();
    pin::create(pool, alice.id, plain.id).await.unwrap();
    // Reactions: a unicode one from fan and a custom-emoji one from alice —
    // `me` must flip per viewer off the shared groups.
    plamenu_db::reaction::create(
        pool,
        plamenu_db::reaction::NewReaction {
            account_id: fan.id,
            status_id: plain.id,
            name: "🎉",
            custom_emoji_url: None,
            uri: None,
        },
    )
    .await
    .unwrap();
    plamenu_db::reaction::create(
        pool,
        plamenu_db::reaction::NewReaction {
            account_id: alice.id,
            status_id: plain.id,
            name: "party",
            custom_emoji_url: Some(&format!("https://{DOMAIN}/media/party.png")),
            uri: None,
        },
    )
    .await
    .unwrap();
    statuses.push(plain.clone());

    // 2. A poll the direct-media viewer voted on.
    let polled = post(
        state,
        actions::PostParams {
            username: "alice",
            text: "which one?",
            visibility: "public",
            poll: Some(actions::PollParams {
                options: vec!["yes :party:".to_owned(), "no".to_owned()],
                expires_in: Some(3600),
                multiple: false,
                hide_totals: false,
            }),
            ..Default::default()
        },
    )
    .await;
    let poll_row = poll::find_by_status(pool, polled.id)
        .await
        .unwrap()
        .unwrap();
    poll::insert_vote(pool, poll_row.id, direct_media.id, 0, None)
        .await
        .unwrap();
    poll::refresh_local_tallies(pool, poll_row.id)
        .await
        .unwrap();
    statuses.push(polled);

    // 3. A quote of the plain post (per-viewer nested render).
    let quoting = post(
        state,
        actions::PostParams {
            username: "alice",
            text: "look at this",
            visibility: "public",
            quoted_status_id: Some(plain.id),
            ..Default::default()
        },
    )
    .await;
    statuses.push(quoting);

    // 4. A boost wrapper (flags mirror the target; fan boosted the target).
    let boost = actions::reblog_status(state, &fan, plain.id).await.unwrap();
    statuses.push(boost);

    // 5. A preview-card post with a verified creator.
    let carded = post(
        state,
        actions::PostParams {
            username: "alice",
            text: "read https://articles.example/1",
            visibility: "public",
            ..Default::default()
        },
    )
    .await;
    let card = preview_card::upsert(
        pool,
        NewPreviewCard {
            url: "https://articles.example/1",
            title: "An article",
            kind: "link",
            author_account_id: Some(fan.id),
            image_url: Some("https://articles.example/1.png"),
            ..NewPreviewCard::default()
        },
    )
    .await
    .unwrap();
    preview_card::attach(pool, carded.id, card.id, "https://articles.example/1")
        .await
        .unwrap();
    statuses.push(carded);

    // 6. An event with a sidecar; the direct-media viewer RSVP'd.
    let event = status::create_local(
        pool,
        NewLocalStatus {
            object_type: Some("Event"),
            ..NewLocalStatus::new(alice.id, "<p>meetup</p>", "public", None)
        },
    )
    .await
    .unwrap();
    let mut sidecar = status_event::StatusEvent::empty(event.id);
    sidecar.join_mode = Some("free".to_owned());
    sidecar.location_name = Some("the park".to_owned());
    status_event::upsert(pool, &sidecar).await.unwrap();
    status_participation::upsert(
        pool,
        event.id,
        direct_media.id,
        plamenu_db::status_participation::State::Accepted,
        None,
        None,
    )
    .await
    .unwrap();
    statuses.push(event);

    // 7. A post carrying a tagged collection with a pending item — pending is
    // visible only to the collection's owner (alice), so member lists differ
    // per viewer.
    let curated = post(
        state,
        actions::PostParams {
            username: "alice",
            text: "my people",
            visibility: "public",
            ..Default::default()
        },
    )
    .await;
    let coll = collection::create(
        pool,
        NewCollection {
            account_id: alice.id,
            name: "People",
            description: "",
            language: None,
            sensitive: false,
            discoverable: true,
            local: true,
            tag_id: None,
            uri: None,
            url: None,
            original_number_of_items: None,
        },
    )
    .await
    .unwrap();
    for (member, item_state) in [(fan.id, "accepted"), (filtered.id, "pending")] {
        collection::add_item(
            pool,
            NewCollectionItem {
                item_id: id::next(),
                collection_id: coll.id,
                account_id: Some(member),
                state: item_state,
                uri: None,
                object_uri: None,
                activity_uri: None,
                approval_uri: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
    }
    tagged_object::add(
        pool,
        curated.id,
        coll.id,
        &format!("https://{DOMAIN}/collections/{}", coll.id),
    )
    .await
    .unwrap();
    statuses.push(curated);

    // 8. A post by a migrated remote author — the `moved` attachment renders
    // per viewer inside the author entity.
    let mover = remote_account(pool, "old-home.example", "mover").await;
    let target = remote_account(pool, "new-home.example", "landed").await;
    account::set_moved_to(pool, mover.id, target.uri.as_deref())
        .await
        .unwrap()
        .unwrap();
    let moved_post = status::upsert_remote(
        pool,
        plamenu_db::status::NewRemoteStatus {
            uri: "https://old-home.example/objects/1",
            account_id: mover.id,
            content: "<p>from my old home</p>",
            created_at: time::OffsetDateTime::now_utc(),
            visibility: "public",
            in_reply_to_id: None,
            in_reply_to_uri: None,
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
    statuses.push(moved_post);

    // 9. A direct message whose conversation the fan mutes (the `muted` flag).
    let dm = post(
        state,
        actions::PostParams {
            username: "alice",
            text: "psst @fan",
            visibility: "direct",
            ..Default::default()
        },
    )
    .await;
    let conv = conversation::of_status(pool, dm.id).await.unwrap().unwrap();
    conversation::mute(pool, fan.id, conv).await.unwrap();
    statuses.push(dm);

    Fixtures {
        statuses,
        viewers: vec![alice.id, fan.id, filtered.id, direct_media.id],
    }
}

/// The core byte-identity claim: for every fixture status and viewer, the
/// viewer-set render equals the singular render, character for character.
#[sqlx::test(migrations = "../db/migrations")]
async fn viewer_set_render_matches_the_singular_render(pool: PgPool) {
    let state = test_state_with(pool.clone(), Arc::default());
    let fixtures = rich_fixtures(&state, &pool).await;

    for item in &fixtures.statuses {
        let batched = entities::render_status_for_viewers(&pool, DOMAIN, item, &fixtures.viewers)
            .await
            .unwrap();
        assert_eq!(batched.len(), fixtures.viewers.len());
        for &viewer in &fixtures.viewers {
            let singular = entities::render_status(&pool, DOMAIN, item, Some(viewer))
                .await
                .unwrap();
            assert_eq!(
                batched[&viewer].to_string(),
                singular.to_string(),
                "status {} diverges for viewer {viewer}",
                item.id
            );
        }
    }
}

/// Subscribes `viewer`'s `user` stream straight on the hub (no websocket) and
/// returns the payload receiver.
fn subscribe_user(state: &AppState, viewer: i64) -> tokio::sync::mpsc::Receiver<String> {
    subscribe(state, Channel::User(viewer), viewer)
}

fn subscribe(
    state: &AppState,
    channel: Channel,
    viewer: i64,
) -> tokio::sync::mpsc::Receiver<String> {
    let (sender, receiver) = tokio::sync::mpsc::channel(64);
    let connection = state.streaming.connection_id();
    state.streaming.subscribe(
        connection,
        channel,
        viewer,
        true,
        sender,
        Arc::new(tokio::sync::Notify::new()),
    );
    receiver
}

/// The `payload` field of the next wire message on `receiver`.
async fn next_payload(receiver: &mut tokio::sync::mpsc::Receiver<String>) -> String {
    let message = tokio::time::timeout(std::time::Duration::from_secs(10), receiver.recv())
        .await
        .expect("no streaming message arrived")
        .expect("stream closed");
    let parsed: Value = serde_json::from_str(&message).unwrap();
    parsed["payload"].as_str().unwrap().to_owned()
}

/// The routed `update` payloads must be the singular renders, and a list
/// stream must carry its owner's home document (one render, not two).
#[sqlx::test(migrations = "../db/migrations")]
async fn streamed_status_payloads_match_singular_renders_and_lists_share_them(pool: PgPool) {
    let state = test_state_with(pool.clone(), Arc::default());
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let fan = create_local_account(&pool, "fan", "Fan").await;
    follow::create(&pool, fan.id, alice.id, None).await.unwrap();
    let fans_list = list::create(&pool, fan.id, "faves", "list", false)
        .await
        .unwrap();
    list::add_members(&pool, fans_list.id, fan.id, &[alice.id])
        .await
        .unwrap()
        .unwrap();

    let mut fan_user = subscribe_user(&state, fan.id);
    let mut fan_list = subscribe(&state, Channel::List(fans_list.id), fan.id);

    let (stored, _) = actions::post_status(
        &state,
        actions::PostParams {
            username: "alice",
            text: "streamed :tada: post #live",
            visibility: "public",
            ..Default::default()
        },
    )
    .await
    .unwrap();
    plamenu::streaming::handle_event(
        &state,
        &Event::StatusNew {
            status_id: stored.id,
        },
    )
    .await
    .unwrap();

    let home_payload = next_payload(&mut fan_user).await;
    let list_payload = next_payload(&mut fan_list).await;
    let singular = entities::render_status(&pool, DOMAIN, &stored, Some(fan.id))
        .await
        .unwrap();
    assert_eq!(home_payload, singular.to_string());
    assert_eq!(
        list_payload, home_payload,
        "the list stream must reuse the home render"
    );
}

/// The routed `announcement` payloads must be the singular per-viewer
/// entities: `read` and reaction `me` stay viewer-relative under the batched
/// loads.
#[sqlx::test(migrations = "../db/migrations")]
async fn streamed_announcement_payloads_match_singular_renders(pool: PgPool) {
    let state = test_state_with(pool.clone(), Arc::default());
    create_local_account(&pool, "alice", "Alice").await;
    let reader = create_local_account(&pool, "reader", "Reader").await;
    let reactor = create_local_account(&pool, "reactor", "Reactor").await;

    // A cited public status rides along, exercising the nested status render.
    let (cited, _) = actions::post_status(
        &state,
        actions::PostParams {
            username: "alice",
            text: "the cited post",
            visibility: "public",
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let ann = announcement::create(
        &pool,
        announcement::NewAnnouncement {
            text: "hello everyone",
            scheduled_at: None,
            starts_at: None,
            ends_at: None,
            all_day: false,
            status_ids: Some(&[cited.id]),
        },
    )
    .await
    .unwrap();
    announcement::create_reaction(&pool, reactor.id, ann.id, "🎉", None)
        .await
        .unwrap();
    announcement::mute(&pool, reader.id, ann.id).await.unwrap();

    let mut reader_stream = subscribe_user(&state, reader.id);
    let mut reactor_stream = subscribe_user(&state, reactor.id);

    plamenu::streaming::handle_event(
        &state,
        &Event::Announcement {
            announcement_id: ann.id,
        },
    )
    .await
    .unwrap();

    let mut payloads = Vec::new();
    for (viewer, receiver) in [
        (reader.id, &mut reader_stream),
        (reactor.id, &mut reactor_stream),
    ] {
        let payload = next_payload(receiver).await;
        let groups = announcement::reactions_for(&pool, &[ann.id], Some(viewer))
            .await
            .unwrap()
            .remove(&ann.id)
            .unwrap_or_default();
        let read = announcement::muted_announcement_ids(&pool, viewer, &[ann.id])
            .await
            .unwrap()
            .contains(&ann.id);
        let singular = entities::announcement_json(&state, &ann, Some(viewer), &groups, read)
            .await
            .unwrap();
        assert_eq!(payload, singular.to_string(), "viewer {viewer} diverges");
        payloads.push(serde_json::from_str::<Value>(&payload).unwrap());
    }
    // The two viewers genuinely differ, so the parity above is not vacuous:
    // the reader dismissed it, the reactor's own emoji carries `me`.
    assert_eq!(payloads[0]["read"], Value::Bool(true));
    assert_eq!(payloads[1]["read"], Value::Bool(false));
    assert_eq!(payloads[0]["reactions"][0]["me"], Value::Bool(false));
    assert_eq!(payloads[1]["reactions"][0]["me"], Value::Bool(true));
}
