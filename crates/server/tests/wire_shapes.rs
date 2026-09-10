//! Wire-fixture corpus tests: checked-in federation shapes from other
//! software, driven through the signed inbox path. Provenance for every
//! fixture is documented in `fixtures/README.md` — the sender is always
//! `bob@remote.example`, the recipient `alice@plamenu.test`, so fixtures
//! stay verbatim wire JSON with no templating.

mod common;

use std::time::SystemTime;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{RemoteUser, StubFederation, TEST_DOMAIN, create_local_account, test_app_with};
use plamenu_db::{PgPool, media, mention, notification, poll, quote, reaction, status};
use serde_json::Value;
use tower::ServiceExt;

fn fixture(raw: &str) -> Value {
    serde_json::from_str(raw).expect("fixture is valid JSON")
}

/// The fixture sender every corpus file is written against.
fn sender() -> RemoteUser {
    RemoteUser::new("remote.example", "bob")
}

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

/// The stored status a fixture's object should have produced.
async fn stored_object(pool: &PgPool, activity: &Value) -> status::Status {
    let uri = activity["object"]["id"].as_str().unwrap();
    status::find_by_uri(pool, uri)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("fixture object {uri} was not stored"))
}

/// `GoToSocial` ships a single mention as a bare `tag` object; dropping it
/// would lose the mention row and the notification.
#[sqlx::test(migrations = "../db/migrations")]
async fn gotosocial_singleton_tag_mention_is_stored_and_notifies(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let activity = fixture(include_str!(
        "fixtures/gotosocial/create_note_singleton_tag.json"
    ));

    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub), &activity, &bob).await,
        StatusCode::ACCEPTED
    );
    let stored = stored_object(&pool, &activity).await;
    assert!(
        mention::exists(&pool, stored.id, alice.id).await.unwrap(),
        "the singleton Mention tag must attach"
    );
    let notifications = notification::list(
        &pool,
        alice.id,
        None,
        None,
        None,
        notification::NotificationFilter::default(),
        10,
    )
    .await
    .unwrap();
    assert_eq!(notifications.len(), 1);
}

/// A bare `attachment` object (go-fed serialization) must store its media
/// like a one-element array would.
#[sqlx::test(migrations = "../db/migrations")]
async fn gotosocial_singleton_attachment_stores_media(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let activity = fixture(include_str!(
        "fixtures/gotosocial/create_note_singleton_attachment.json"
    ));

    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub), &activity, &bob).await,
        StatusCode::ACCEPTED
    );
    let stored = stored_object(&pool, &activity).await;
    let attached = media::for_statuses(&pool, &[stored.id])
        .await
        .unwrap()
        .remove(&stored.id)
        .unwrap_or_default();
    assert_eq!(attached.len(), 1);
    assert_eq!(
        attached[0].remote_url.as_deref(),
        Some("https://remote.example/fileserver/01JZQYA/attachment/original/photo.jpg")
    );
    assert_eq!(attached[0].description.as_deref(), Some("A single photo"));
    assert!(attached[0].blurhash.is_some());
}

/// A top-level `Document` post keeps its native body and treats a non-HTML
/// `url` Link as the published file. This is Hubzilla's real file-post shape.
#[sqlx::test(migrations = "../db/migrations")]
async fn document_post_ingests_natively(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let activity = fixture(include_str!("fixtures/as2/create_document.json"));

    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub), &activity, &bob).await,
        StatusCode::ACCEPTED
    );
    let stored = stored_object(&pool, &activity).await;
    assert_eq!(stored.object_type.as_deref(), Some("Document"));
    assert!(
        stored
            .content
            .contains("Bob shared the complete quarterly report"),
        "the native body must survive ingestion: {}",
        stored.content
    );
    assert!(
        !stored
            .content
            .contains("<strong>Quarterly interop report</strong>"),
        "the title is rendered from the title field, not duplicated into content: {}",
        stored.content
    );
    let attached = media::for_statuses(&pool, &[stored.id])
        .await
        .unwrap()
        .remove(&stored.id)
        .unwrap_or_default();
    assert_eq!(attached.len(), 2, "both cover and native document are kept");
    assert!(attached.iter().any(|item| {
        item.remote_url.as_deref() == Some("https://remote.example/files/quarterly-report.pdf")
            && item.content_type == "application/pdf"
            && !item.download_on_demand
    }));
}

/// `GoToSocial`'s Update carries the edited Note with the same bare `tag`
/// shape as its Create; the edit must apply (content + `edited_at`) without
/// losing the mention, and replaying the same Update must not double-apply.
#[sqlx::test(migrations = "../db/migrations")]
async fn gotosocial_update_edits_in_place_and_is_idempotent(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());
    let create = fixture(include_str!(
        "fixtures/gotosocial/create_note_singleton_tag.json"
    ));
    let update = fixture(include_str!(
        "fixtures/gotosocial/update_note_singleton_tag.json"
    ));

    assert_eq!(
        post_signed(app(), &create, &bob).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        post_signed(app(), &update, &bob).await,
        StatusCode::ACCEPTED
    );

    let stored = stored_object(&pool, &update).await;
    assert!(
        stored.content.contains("the singleton tag got an edit"),
        "the Update must rewrite the content: {}",
        stored.content
    );
    assert!(stored.edited_at.is_some(), "the edit must stamp edited_at");
    assert!(
        mention::exists(&pool, stored.id, alice.id).await.unwrap(),
        "the mention must survive the edit"
    );

    // Replay: same Update again is a no-op, not a second edit.
    assert_eq!(
        post_signed(app(), &update, &bob).await,
        StatusCode::ACCEPTED
    );
    let replayed = stored_object(&pool, &update).await;
    assert_eq!(replayed.edited_at, stored.edited_at);
    assert_eq!(replayed.content, stored.content);
}

/// `GoToSocial`'s Delete ships the object as a bare IRI string (go-fed
/// unnests single values); the status must go away, and a replayed Delete
/// must stay a 202 no-op.
#[sqlx::test(migrations = "../db/migrations")]
async fn gotosocial_delete_bare_iri_object_is_idempotent(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());
    let create = fixture(include_str!(
        "fixtures/gotosocial/create_note_singleton_tag.json"
    ));
    let delete = fixture(include_str!("fixtures/gotosocial/delete_note.json"));
    let uri = create["object"]["id"].as_str().unwrap();

    assert_eq!(
        post_signed(app(), &create, &bob).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        post_signed(app(), &delete, &bob).await,
        StatusCode::ACCEPTED
    );
    assert!(
        status::find_by_uri(&pool, uri).await.unwrap().is_none(),
        "the bare-IRI Delete must remove the status"
    );
    assert_eq!(
        post_signed(app(), &delete, &bob).await,
        StatusCode::ACCEPTED,
        "a replayed Delete stays accepted"
    );
}

/// Sharkey quotes carry no FEP-044f `quote` property — only `_misskey_quote`
/// / `quoteUrl` / `quoteUri` and the FEP-e232 Link tag — so the quote must
/// link as a legacy quote.
#[sqlx::test(migrations = "../db/migrations")]
async fn sharkey_quote_note_links_as_legacy_quote(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());
    let base = fixture(include_str!("fixtures/sharkey/create_note.json"));
    let quote_note = fixture(include_str!("fixtures/sharkey/create_note_quote.json"));

    assert_eq!(post_signed(app(), &base, &bob).await, StatusCode::ACCEPTED);
    assert_eq!(
        post_signed(app(), &quote_note, &bob).await,
        StatusCode::ACCEPTED
    );

    let quoted = stored_object(&pool, &base).await;
    let quoting_uri = quote_note["object"]["id"].as_str().unwrap();
    let linked = quote::find_by_status_uri(&pool, quoting_uri)
        .await
        .unwrap()
        .expect("the Misskey-dialect quote must link");
    assert_eq!(linked.quoted_status_id, Some(quoted.id));
    assert!(
        linked.legacy,
        "no FEP-044f `quote` property => legacy quote"
    );
    let quoting = stored_object(&pool, &quote_note).await;
    let rendered = plamenu::entities::render_status(&pool, TEST_DOMAIN, &quoting, None)
        .await
        .unwrap();
    assert_eq!(
        rendered["content"], "<p>quoting my earlier note</p>",
        "a successful Sharkey quote drops its trailing inline RE link"
    );
}

/// A quote carried *only* as a FEP-e232 `tag` Link (no flat properties —
/// what the streams/Hubzilla family sends): the Link with an `ActivityPub`
/// `mediaType` and the Misskey quote rel must link as a legacy quote.
/// Decoys must not: an ordinary `text/html` Link with a quote rel is a
/// hyperlink, not an object link, and a rel-less object link is a bare
/// reference without quote semantics.
#[sqlx::test(migrations = "../db/migrations")]
async fn e232_only_quote_links_as_legacy_quote(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());
    let base = fixture(include_str!("fixtures/sharkey/create_note.json"));
    let quote_note = fixture(include_str!("fixtures/as2/create_note_e232_quote.json"));

    assert_eq!(post_signed(app(), &base, &bob).await, StatusCode::ACCEPTED);
    assert_eq!(
        post_signed(app(), &quote_note, &bob).await,
        StatusCode::ACCEPTED
    );

    let quoted = stored_object(&pool, &base).await;
    let quoting_uri = quote_note["object"]["id"].as_str().unwrap();
    let linked = quote::find_by_status_uri(&pool, quoting_uri)
        .await
        .unwrap()
        .expect("the e232-only quote must link");
    assert_eq!(
        linked.quoted_uri.as_deref(),
        Some("https://remote.example/notes/9sh4rkbase0001"),
        "the AP-typed quote-rel Link wins over the decoys"
    );
    assert_eq!(linked.quoted_status_id, Some(quoted.id));
    assert!(
        linked.legacy,
        "no FEP-044f `quote` property => legacy quote"
    );
}

/// Sharkey's Delete has *no activity id* and wraps the target in a
/// `Tombstone` object; both must be tolerated, idempotently.
#[sqlx::test(migrations = "../db/migrations")]
async fn sharkey_idless_tombstone_delete_is_idempotent(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());
    let create = fixture(include_str!("fixtures/sharkey/create_note.json"));
    let delete = fixture(include_str!("fixtures/sharkey/delete_note_tombstone.json"));
    let uri = create["object"]["id"].as_str().unwrap();

    assert_eq!(
        post_signed(app(), &create, &bob).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        post_signed(app(), &delete, &bob).await,
        StatusCode::ACCEPTED
    );
    assert!(
        status::find_by_uri(&pool, uri).await.unwrap().is_none(),
        "the id-less Tombstone Delete must remove the status"
    );
    assert_eq!(
        post_signed(app(), &delete, &bob).await,
        StatusCode::ACCEPTED,
        "a replayed Delete stays accepted"
    );
}

/// A Sharkey boost (`Announce` with a bare IRI object) must store a reblog
/// row, and Sharkey's `Undo` — which embeds the whole original Announce as
/// its object — must retract it, idempotently.
#[sqlx::test(migrations = "../db/migrations")]
async fn sharkey_announce_and_embedded_undo_roundtrip(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());
    let create = fixture(include_str!("fixtures/sharkey/create_note.json"));
    let announce = fixture(include_str!("fixtures/sharkey/announce_note.json"));
    let undo = fixture(include_str!("fixtures/sharkey/undo_announce.json"));
    let announce_uri = announce["id"].as_str().unwrap();

    assert_eq!(
        post_signed(app(), &create, &bob).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        post_signed(app(), &announce, &bob).await,
        StatusCode::ACCEPTED
    );
    let boosted = stored_object(&pool, &create).await;
    let boost = status::find_by_uri(&pool, announce_uri)
        .await
        .unwrap()
        .expect("the Announce must store a boost");
    assert_eq!(boost.reblog_of_id, Some(boosted.id));

    assert_eq!(post_signed(app(), &undo, &bob).await, StatusCode::ACCEPTED);
    assert!(
        status::find_by_uri(&pool, announce_uri)
            .await
            .unwrap()
            .is_none(),
        "the embedded-object Undo must retract the boost"
    );
    assert_eq!(
        post_signed(app(), &undo, &bob).await,
        StatusCode::ACCEPTED,
        "a replayed Undo stays accepted"
    );
}

/// Sharkey sends every reaction as `Like` + `_misskey_reaction`, custom emoji
/// carrying a `tag` Emoji with the image; the reaction row must store the
/// emoji, and the embedded-Like Undo must clear it.
#[sqlx::test(migrations = "../db/migrations")]
async fn sharkey_custom_emoji_reaction_like_and_undo(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let app = || test_app_with(pool.clone(), stub.clone());
    let create = fixture(include_str!("fixtures/sharkey/create_note.json"));
    let like = fixture(include_str!(
        "fixtures/sharkey/like_reaction_custom_emoji.json"
    ));
    let undo = fixture(include_str!("fixtures/sharkey/undo_like_reaction.json"));

    assert_eq!(
        post_signed(app(), &create, &bob).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(post_signed(app(), &like, &bob).await, StatusCode::ACCEPTED);

    let stored = stored_object(&pool, &create).await;
    let reactions = reaction::for_statuses(&pool, &[stored.id])
        .await
        .unwrap()
        .remove(&stored.id)
        .unwrap_or_default();
    assert_eq!(
        reactions.len(),
        1,
        "the reaction Like must store a reaction"
    );
    assert!(
        reactions[0].name.contains("sharkfire"),
        "the reaction keeps the emoji shortcode: {}",
        reactions[0].name
    );
    assert_eq!(
        reactions[0].custom_emoji_url.as_deref(),
        Some("https://remote.example/files/sharkfire.png"),
        "the tag Emoji icon must be kept for rendering"
    );

    assert_eq!(post_signed(app(), &undo, &bob).await, StatusCode::ACCEPTED);
    let cleared = reaction::for_statuses(&pool, &[stored.id])
        .await
        .unwrap()
        .remove(&stored.id)
        .unwrap_or_default();
    assert!(cleared.is_empty(), "the embedded-Like Undo must clear it");
}

/// A `Question` with an array `type` and a bare `oneOf` option must produce
/// a poll — the ingest gate accepts the array type, so the poll parser has
/// to agree or the status is silently stored poll-less.
#[sqlx::test(migrations = "../db/migrations")]
async fn question_with_array_type_and_singleton_oneof_parses_poll(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let activity = fixture(include_str!(
        "fixtures/as2/create_question_singleton_oneof.json"
    ));

    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub), &activity, &bob).await,
        StatusCode::ACCEPTED
    );
    let stored = stored_object(&pool, &activity).await;
    let stored_poll = poll::find_by_status(&pool, stored.id)
        .await
        .unwrap()
        .expect("the singleton oneOf must parse into a poll");
    assert_eq!(stored_poll.options, ["yes"]);
    assert_eq!(stored_poll.cached_tallies, [3]);
    assert!(!stored_poll.multiple);
    assert_eq!(stored_poll.voters_count, Some(3));
    assert!(stored_poll.expires_at.is_some());
}

/// Sharkey renders poll options as `Note`s whose tallies hide in
/// `replies.totalItems` (no `votersCount`), with `endTime` for open polls;
/// the options, tallies and expiry must all come through.
#[sqlx::test(migrations = "../db/migrations")]
async fn sharkey_question_options_and_tallies_parse(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    let bob = sender();
    let stub = StubFederation::with_actors([bob.actor.clone()]);
    let activity = fixture(include_str!("fixtures/sharkey/create_question.json"));

    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub), &activity, &bob).await,
        StatusCode::ACCEPTED
    );
    let stored = stored_object(&pool, &activity).await;
    let stored_poll = poll::find_by_status(&pool, stored.id)
        .await
        .unwrap()
        .expect("the Sharkey Question must parse into a poll");
    assert_eq!(stored_poll.options, ["sharks", "rays"]);
    assert_eq!(stored_poll.cached_tallies, [4, 1]);
    assert!(!stored_poll.multiple);
    assert!(
        stored_poll.expires_at.is_some(),
        "endTime must map to expiry"
    );
}
