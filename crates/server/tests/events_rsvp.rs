//! E2/E3: event participation — `Join` out, `Accept(Join)` / `Reject(Join)`
//! back, and the inbound side on events we host.
//!
//! An RSVP is a negotiation, not a toggle, and the tests here pin the parts of
//! that which are easy to get wrong: a `Join` addressed to the deciding party
//! rather than to `Public`; a **bare `Leave`** rather than `Undo(Join)` (the one
//! peer that hosts events has no `Undo(Join)` arm at all); a verdict resolved by
//! the `Join`'s own activity id, since that is the only handle an origin echoes
//! back; and a `pending` row that stays pending forever without anything
//! retrying it, because a full Mobilizon event answers nothing.

mod common;

use std::time::SystemTime;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    RemoteUser, StubFederation, TEST_DOMAIN, create_local_account, test_app_with, test_state_with,
};
use http_body_util::BodyExt;
use plamenu_db::status_participation::{self as participation, State};
use plamenu_db::{PgPool, account, status, status_event};
use serde_json::{Value, json};
use tower::ServiceExt;

const EVENT_URI: &str = "https://mz.example/events/1111";
const GROUP_URI: &str = "https://mz.example/@thegroup";

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
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    if status.is_server_error() {
        let body = response.into_body().collect().await.unwrap().to_bytes();
        eprintln!("inbox error response: {}", String::from_utf8_lossy(&body));
    }
    status
}

/// A Mobilizon-shaped group event: `actor` the organizing Person, `attributedTo`
/// the group, and the join mode on the object.
fn group_event(organizer: &RemoteUser, join_mode: &str, capacity: Option<i64>) -> Value {
    let mut object = json!({
        "id": EVENT_URI,
        "type": "Event",
        "actor": organizer.actor.id,
        "attributedTo": GROUP_URI,
        "name": "Interop meetup",
        "content": "<p>Come along.</p>",
        "startTime": "2027-03-14T18:00:00Z",
        "endTime": "2027-03-14T20:30:00Z",
        "timezone": "Etc/UTC",
        "joinMode": join_mode,
        "participantCount": 3,
        "ical:status": "CONFIRMED",
        "draft": false,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
    });
    if let Some(max) = capacity {
        object["maximumAttendeeCapacity"] = json!(max);
        object["remainingAttendeeCapacity"] = json!(0);
    }
    json!({
        "id": format!("{EVENT_URI}/activity"),
        "type": "Create",
        "actor": organizer.actor.id,
        "attributedTo": GROUP_URI,
        // Mobilizon sends the activity's `to` as a bare STRING while the object's
        // is an array; both must be tolerated.
        "to": "https://www.w3.org/ns/activitystreams#Public",
        "object": object,
    })
}

/// Ingests a remote group event and returns (state, stub, the stored status).
async fn ingest_event(
    pool: &PgPool,
    join_mode: &str,
    capacity: Option<i64>,
) -> (
    plamenu::state::AppState,
    std::sync::Arc<StubFederation>,
    status::Status,
) {
    let organizer = RemoteUser::new("mz.example", "grace");
    ingest_event_from(pool, join_mode, capacity, &organizer).await
}

/// Variant for tests that send a later activity from the organizer. Reusing
/// the same key pair is important: replacing the material behind an existing
/// key ID is a substitution attempt, not a legitimate key rotation.
async fn ingest_event_from(
    pool: &PgPool,
    join_mode: &str,
    capacity: Option<i64>,
    organizer: &RemoteUser,
) -> (
    plamenu::state::AppState,
    std::sync::Arc<StubFederation>,
    status::Status,
) {
    let stub = StubFederation::with_actors([organizer.actor.clone()]);
    let app = test_app_with(pool.clone(), stub.clone());
    assert_eq!(
        post_signed(app, &group_event(organizer, join_mode, capacity), organizer).await,
        StatusCode::ACCEPTED
    );
    let item = status::find_by_uri(pool, EVENT_URI)
        .await
        .unwrap()
        .expect("the event was ingested");
    let state = test_state_with(pool.clone(), stub.clone());
    (state, stub, item)
}

#[sqlx::test(migrations = "../db/migrations")]
async fn rsvp_emits_a_join_addressed_to_the_decider(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let (state, stub, item) = ingest_event(&pool, "free", None).await;

    let row = plamenu::events::rsvp(&state, &alice, item.id, Some("  may I come?  "))
        .await
        .unwrap();
    // Someone else's event: the origin decides, so our row starts pending no
    // matter how permissive the join mode looks from here.
    assert_eq!(row.state, State::Pending);
    assert_eq!(
        row.message.as_deref(),
        Some("may I come?"),
        "the participation message is trimmed"
    );

    assert_eq!(plamenu::delivery::run_due(&state).await, 1);
    let sent = &stub.deliveries()[0];
    let join = &sent.activity;
    assert_eq!(join["type"], "Join");
    assert_eq!(join["object"], EVENT_URI, "the event as a bare IRI");
    assert_eq!(join["participationMessage"], "may I come?");
    // Addressed to us and the deciding party — never Public. An RSVP is not an
    // announcement, and widening it would leak attendance to our followers.
    let to: Vec<&str> = join["to"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(to.iter().any(|t| t.ends_with("/users/alice")), "{to:?}");
    assert!(
        !to.iter().any(|t| t.contains("activitystreams#Public")),
        "an RSVP must not be public: {to:?}"
    );
    assert!(join["cc"].is_null(), "no cc widening either");
    // The activity id embeds our row, which is how the echoed verdict resolves.
    assert_eq!(
        join["id"].as_str().unwrap(),
        row.uri.as_deref().unwrap(),
        "the stored uri is the activity id we sent"
    );
    assert!(join["id"].as_str().unwrap().contains(&row.id.to_string()));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn a_repeat_rsvp_sends_no_second_join(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let (state, stub, item) = ingest_event(&pool, "free", None).await;

    let first = plamenu::events::rsvp(&state, &alice, item.id, None)
        .await
        .unwrap();
    let second = plamenu::events::rsvp(&state, &alice, item.id, None)
        .await
        .unwrap();
    assert_eq!(first.id, second.id, "the same participation row");
    // A double-tapped button must not produce two participations on the origin.
    assert_eq!(plamenu::delivery::run_due(&state).await, 1);
    assert_eq!(stub.deliveries().len(), 1);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn accept_join_resolves_the_pending_rsvp_and_notifies(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let (state, _stub, item) = ingest_event(&pool, "restricted", None).await;
    let row = plamenu::events::rsvp(&state, &alice, item.id, None)
        .await
        .unwrap();
    let join_uri = row.uri.clone().unwrap();

    // Mobilizon's shape: the verdict comes from a group MODERATOR, with the group
    // in `attributedTo` — so the signer is neither the event's author nor the
    // group. It is honoured because it comes from the event's own origin host,
    // which already authors and deletes this event at will; what must not be
    // possible is a *third* host deciding (see the next test).
    let moderator = RemoteUser::new("mz.example", "mod");
    let stub = StubFederation::with_actors([moderator.actor.clone()]);
    let accept = json!({
        "id": "https://mz.example/accept/join/9",
        "type": "Accept",
        "actor": moderator.actor.id,
        "attributedTo": GROUP_URI,
        "to": [format!("https://{TEST_DOMAIN}/users/alice")],
        "object": {
            "id": join_uri,
            "type": "Join",
            "actor": format!("https://{TEST_DOMAIN}/users/alice"),
            "object": EVENT_URI,
        },
    });
    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub), &accept, &moderator).await,
        StatusCode::ACCEPTED
    );

    let settled = participation::find(&pool, item.id, alice.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(settled.state, State::Accepted);
    let notes = plamenu_db::notification::list(
        &pool,
        alice.id,
        None,
        None,
        None,
        plamenu_db::notification::NotificationFilter::default(),
        10,
    )
    .await
    .unwrap();
    assert!(
        notes.iter().any(|n| n.kind == "event.accepted"),
        "the attendee learns their RSVP was accepted: {:?}",
        notes.iter().map(|n| &n.kind).collect::<Vec<_>>()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn a_verdict_from_an_unrelated_actor_is_ignored(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let (state, _stub, item) = ingest_event(&pool, "restricted", None).await;
    let row = plamenu::events::rsvp(&state, &alice, item.id, None)
        .await
        .unwrap();
    let join_uri = row.uri.clone().unwrap();

    // A stranger on another host claims the group's authority by quoting its id
    // in `attributedTo`. Without a host bound on that claim, a group's authority
    // would be a free-floating string anyone could invoke.
    let stranger = RemoteUser::new("evil.example", "impostor");
    let stub = StubFederation::with_actors([stranger.actor.clone()]);
    let accept = json!({
        "id": "https://evil.example/accept/join/1",
        "type": "Accept",
        "actor": stranger.actor.id,
        "attributedTo": GROUP_URI,
        "object": {"id": join_uri, "type": "Join", "object": EVENT_URI},
    });
    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub), &accept, &stranger).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        participation::find(&pool, item.id, alice.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        State::Pending,
        "an unrelated actor cannot accept an RSVP"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn reject_join_is_kept_so_the_button_is_not_offered_again(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let organizer = RemoteUser::new("mz.example", "grace");
    let (state, _stub, item) = ingest_event_from(&pool, "restricted", None, &organizer).await;
    let row = plamenu::events::rsvp(&state, &alice, item.id, None)
        .await
        .unwrap();
    let join_uri = row.uri.clone().unwrap();

    let stub = StubFederation::with_actors([organizer.actor.clone()]);
    let reject = json!({
        "id": "https://mz.example/reject/join/9",
        "type": "Reject",
        "actor": organizer.actor.id,
        "object": {"id": join_uri, "type": "Join", "object": EVENT_URI},
    });
    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub), &reject, &organizer).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        participation::find(&pool, item.id, alice.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        State::Rejected
    );

    // A redelivered Join must not launder the refusal back into `pending` — the
    // row is kept precisely so a refused attendee isn't offered the button again
    // as if nothing had happened.
    plamenu::events::rsvp(&state, &alice, item.id, None)
        .await
        .unwrap();
    assert_eq!(
        participation::find(&pool, item.id, alice.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        State::Rejected,
        "a settled verdict survives a repeat RSVP"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn cancelling_an_rsvp_emits_a_bare_leave(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let (state, stub, item) = ingest_event(&pool, "free", None).await;
    plamenu::events::rsvp(&state, &alice, item.id, None)
        .await
        .unwrap();
    assert_eq!(plamenu::delivery::run_due(&state).await, 1);

    plamenu::events::cancel_rsvp(&state, &alice, item.id)
        .await
        .unwrap();
    assert_eq!(plamenu::delivery::run_due(&state).await, 1);
    let leave = &stub.deliveries()[1].activity;
    // A bare `Leave`, NOT `Undo(Join)`: Mobilizon's transmogrifier has a `Leave`
    // arm and no `Undo(Join)` arm at all, so an `Undo` would be dropped by the
    // one peer in the fleet that hosts events.
    assert_eq!(leave["type"], "Leave");
    assert_eq!(leave["object"], EVENT_URI);
    assert!(
        leave["object"]["type"].is_null(),
        "the event is named, not embedded"
    );
    assert!(
        participation::find(&pool, item.id, alice.id)
            .await
            .unwrap()
            .is_none(),
        "leaving drops the row rather than settling it"
    );

    // Leaving again is a no-op, not a second Leave.
    assert!(
        plamenu::events::cancel_rsvp(&state, &alice, item.id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(plamenu::delivery::run_due(&state).await, 0);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn a_full_remote_event_still_accepts_an_rsvp_that_stays_pending(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let (state, _stub, item) = ingest_event(&pool, "free", Some(3)).await;
    let event = status_event::find(&pool, item.id).await.unwrap().unwrap();
    assert!(event.is_full(), "the origin says there is no room");

    // On someone *else's* event our capacity read is advisory: the count may be
    // stale, and the origin is the one that decides. So the Join goes out and the
    // row stays pending — which is where it will stay forever, because Mobilizon
    // sends no rejection activity when an event is full. Nothing may retry or
    // expire it into a failure.
    let row = plamenu::events::rsvp(&state, &alice, item.id, None)
        .await
        .unwrap();
    assert_eq!(row.state, State::Pending);
    assert_eq!(plamenu::delivery::run_due(&state).await, 1);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn an_external_event_offers_no_rsvp(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let (state, stub, item) = ingest_event(&pool, "external", None).await;

    // `external` attendance happens on the origin's own site; there is no
    // activity we could meaningfully send, so this is refused rather than
    // half-performed.
    let refused = plamenu::events::rsvp(&state, &alice, item.id, None).await;
    assert!(refused.is_err(), "an external event cannot be joined");
    assert_eq!(plamenu::delivery::run_due(&state).await, 0);
    assert!(stub.deliveries().is_empty());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn the_entity_carries_the_viewers_own_rsvp_state(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let (state, _stub, item) = ingest_event(&pool, "restricted", None).await;

    let before = plamenu::entities::render_status(&pool, TEST_DOMAIN, &item, Some(alice.id))
        .await
        .unwrap();
    assert_eq!(before["event"]["participation"], Value::Null);
    assert_eq!(before["event"]["can_participate"], true);
    assert_eq!(before["event"]["participation_refusal"], Value::Null);
    // A remote event reports the ORIGIN's count, not our own row count.
    assert_eq!(before["event"]["participants_count"], 3);

    plamenu::events::rsvp(&state, &alice, item.id, None)
        .await
        .unwrap();
    let after = plamenu::entities::render_status(&pool, TEST_DOMAIN, &item, Some(alice.id))
        .await
        .unwrap();
    assert_eq!(after["event"]["participation"], "pending");
    assert_eq!(
        after["event"]["participants_count"], 3,
        "our own RSVP does not move a remote origin's count"
    );

    // An anonymous viewer is never offered an RSVP — there is no account to send
    // one with, and a control that only leads to a login prompt is noise.
    let anon = plamenu::entities::render_status(&pool, TEST_DOMAIN, &item, None)
        .await
        .unwrap();
    assert_eq!(anon["event"]["can_participate"], false);
    assert_eq!(anon["event"]["participation"], Value::Null);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn rsvping_to_a_non_event_is_a_404(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let stub = StubFederation::with_actors([]);
    let state = test_state_with(pool.clone(), stub);
    let (note, _) = plamenu::actions::post_status(
        &state,
        plamenu::actions::PostParams {
            username: "alice",
            text: "an ordinary note",
            visibility: "public",
            ..Default::default()
        },
    )
    .await
    .unwrap();
    // Not 422: the RSVP routes must not confirm that some unrelated status id
    // exists just because it isn't an event.
    let refused = plamenu::events::rsvp(&state, &alice, note.id, None).await;
    assert!(matches!(refused, Err(plamenu::error::ApiError::NotFound)));
    let _ = account::find_by_id(&pool, alice.id).await.unwrap();
}

// ---------------------------------------------------------------------------
// E3: inbound participation on events we host
// ---------------------------------------------------------------------------

/// A local event with a sidecar, authored by `organizer_id`.
async fn local_event(
    pool: &PgPool,
    organizer_id: i64,
    join_mode: &str,
    capacity: Option<i32>,
) -> status::Status {
    let item = status::create_local(
        pool,
        status::NewLocalStatus {
            object_type: Some("Event"),
            ..status::NewLocalStatus::new(organizer_id, "<p>our own event</p>", "public", None)
        },
    )
    .await
    .unwrap();
    let mut sidecar = status_event::StatusEvent::empty(item.id);
    sidecar.join_mode = Some(join_mode.to_owned());
    sidecar.max_attendees = capacity;
    status_event::upsert(pool, &sidecar).await.unwrap();
    item
}

/// A remote attendee's `Join` of one of our events.
fn inbound_join(attendee: &RemoteUser, event_uri: &str, message: Option<&str>) -> Value {
    let mut join = json!({
        "id": format!("{}/participation/1", attendee.actor.id),
        "type": "Join",
        "actor": attendee.actor.id,
        "object": event_uri,
        "to": [attendee.actor.id, event_uri],
    });
    if let Some(message) = message {
        join["participationMessage"] = json!(message);
    }
    join
}

#[sqlx::test(migrations = "../db/migrations")]
async fn a_free_local_event_auto_accepts_and_answers(pool: PgPool) {
    let grace = create_local_account(&pool, "grace", "Grace").await;
    let item = local_event(&pool, grace.id, "free", None).await;
    let event_uri = format!("https://{TEST_DOMAIN}/users/grace/statuses/{}", item.id);

    let heidi = RemoteUser::new("mz.example", "heidi");
    let stub = StubFederation::with_actors([heidi.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &inbound_join(&heidi, &event_uri, Some("bringing cake")),
            &heidi,
        )
        .await,
        StatusCode::ACCEPTED
    );

    let attendee = account::find_by_uri(&pool, &heidi.actor.id)
        .await
        .unwrap()
        .unwrap();
    let row = participation::find(&pool, item.id, attendee.id)
        .await
        .unwrap()
        .expect("the RSVP was recorded");
    assert_eq!(row.state, State::Accepted, "a free event auto-accepts");
    assert_eq!(row.message.as_deref(), Some("bringing cake"));
    // The participation url we were given is what our Accept must echo back.
    assert_eq!(
        row.uri.as_deref(),
        Some(&*format!("{}/participation/1", heidi.actor.id))
    );

    // The Accept(Join) goes straight back to the attendee.
    assert_eq!(plamenu::delivery::run_due(&state).await, 1);
    let sent = &stub.deliveries()[0];
    assert_eq!(sent.inbox_url, heidi.actor.inbox);
    assert_eq!(sent.activity["type"], "Accept");
    assert_eq!(sent.activity["object"]["type"], "Join");
    assert_eq!(
        sent.activity["object"]["id"],
        row.uri.clone().unwrap(),
        "the echoed Join carries the participation url the origin will match on"
    );

    // The organizer hears about it.
    let notes = plamenu_db::notification::list(
        &pool,
        grace.id,
        None,
        None,
        None,
        plamenu_db::notification::NotificationFilter::default(),
        10,
    )
    .await
    .unwrap();
    assert!(notes.iter().any(|n| n.kind == "event.participation"));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn a_restricted_local_event_waits_then_the_organizer_decides(pool: PgPool) {
    let grace = create_local_account(&pool, "grace", "Grace").await;
    let item = local_event(&pool, grace.id, "restricted", None).await;
    let event_uri = format!("https://{TEST_DOMAIN}/users/grace/statuses/{}", item.id);

    let heidi = RemoteUser::new("mz.example", "heidi");
    let stub = StubFederation::with_actors([heidi.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &inbound_join(&heidi, &event_uri, Some("may I?")),
            &heidi,
        )
        .await,
        StatusCode::ACCEPTED
    );
    let attendee = account::find_by_uri(&pool, &heidi.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        participation::find(&pool, item.id, attendee.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        State::Pending,
        "a restricted event waits for a human"
    );
    // Nothing is answered yet — a pending request is a resting state, and
    // guessing a verdict for the organizer would be worse than silence.
    assert_eq!(plamenu::delivery::run_due(&state).await, 0);

    // The organizer approves; now the Accept goes out.
    plamenu::events::decide_rsvp(&state, &grace, item.id, attendee.id, true)
        .await
        .unwrap();
    assert_eq!(plamenu::delivery::run_due(&state).await, 1);
    let sent = &stub.deliveries()[0];
    assert_eq!(sent.activity["type"], "Accept");
    assert_eq!(sent.activity["object"]["participationMessage"], "may I?");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn only_the_organizer_may_decide(pool: PgPool) {
    let grace = create_local_account(&pool, "grace", "Grace").await;
    let stranger = create_local_account(&pool, "stranger", "Stranger").await;
    let item = local_event(&pool, grace.id, "restricted", None).await;
    let event_uri = format!("https://{TEST_DOMAIN}/users/grace/statuses/{}", item.id);
    let heidi = RemoteUser::new("mz.example", "heidi");
    let stub = StubFederation::with_actors([heidi.actor.clone()]);
    let state = test_state_with(pool.clone(), stub);
    post_signed(
        test_app_with(
            pool.clone(),
            StubFederation::with_actors([heidi.actor.clone()]),
        ),
        &inbound_join(&heidi, &event_uri, None),
        &heidi,
    )
    .await;
    let attendee = account::find_by_uri(&pool, &heidi.actor.id)
        .await
        .unwrap()
        .unwrap();

    let refused = plamenu::events::decide_rsvp(&state, &stranger, item.id, attendee.id, true).await;
    assert!(
        matches!(refused, Err(plamenu::error::ApiError::Forbidden(_))),
        "a passer-by cannot approve someone else's guest list"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn a_full_local_event_refuses_with_a_reject(pool: PgPool) {
    let grace = create_local_account(&pool, "grace", "Grace").await;
    // Capacity one, already taken by an accepted attendee.
    let item = local_event(&pool, grace.id, "free", Some(1)).await;
    let event_uri = format!("https://{TEST_DOMAIN}/users/grace/statuses/{}", item.id);
    let first = create_local_account(&pool, "first", "First").await;
    participation::upsert(&pool, item.id, first.id, State::Accepted, None, None)
        .await
        .unwrap();

    let heidi = RemoteUser::new("mz.example", "heidi");
    let stub = StubFederation::with_actors([heidi.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &inbound_join(&heidi, &event_uri, None),
            &heidi,
        )
        .await,
        StatusCode::ACCEPTED
    );
    let attendee = account::find_by_uri(&pool, &heidi.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        participation::find(&pool, item.id, attendee.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        State::Rejected
    );
    // We are the origin here, and we already know the answer. Mobilizon stays
    // silent on a full event; leaving an attendee waiting forever when the
    // verdict is knowable would be the unkind version of that.
    assert_eq!(plamenu::delivery::run_due(&state).await, 1);
    assert_eq!(stub.deliveries()[0].activity["type"], "Reject");

    // And the organizer cannot approve past their own advertised limit.
    let over = plamenu::events::decide_rsvp(&state, &grace, item.id, attendee.id, true).await;
    assert!(matches!(
        over,
        Err(plamenu::error::ApiError::Unprocessable(_))
    ));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn an_invite_only_local_event_admits_only_the_invited(pool: PgPool) {
    let grace = create_local_account(&pool, "grace", "Grace").await;
    let item = local_event(&pool, grace.id, "invite", None).await;
    let event_uri = format!("https://{TEST_DOMAIN}/users/grace/statuses/{}", item.id);

    let heidi = RemoteUser::new("mz.example", "heidi");
    let stub = StubFederation::with_actors([heidi.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &inbound_join(&heidi, &event_uri, None),
            &heidi,
        )
        .await,
        StatusCode::ACCEPTED
    );
    let attendee = account::find_by_uri(&pool, &heidi.actor.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        participation::find(&pool, item.id, attendee.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        State::Rejected,
        "an uninvited Join to an invite-only event is refused"
    );

    // Invited, the same Join is accepted.
    plamenu::events::record_invitation(&state, &item, &attendee, grace.id)
        .await
        .unwrap();
    // The refusal must be cleared first — a settled verdict is deliberately
    // sticky, so an invitation after a refusal needs the organizer to say so.
    participation::delete(&pool, item.id, attendee.id)
        .await
        .unwrap();
    plamenu::events::record_invitation(&state, &item, &attendee, grace.id)
        .await
        .unwrap();
    assert_eq!(
        participation::find(&pool, item.id, attendee.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        State::Invited
    );
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &inbound_join(&heidi, &event_uri, None),
            &heidi,
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        participation::find(&pool, item.id, attendee.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        State::Accepted,
        "an invitation is consumed by the Join it exists for"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn an_inbound_leave_and_an_undo_join_both_withdraw(pool: PgPool) {
    let grace = create_local_account(&pool, "grace", "Grace").await;
    let item = local_event(&pool, grace.id, "free", None).await;
    let event_uri = format!("https://{TEST_DOMAIN}/users/grace/statuses/{}", item.id);
    let heidi = RemoteUser::new("mz.example", "heidi");

    for withdrawal in [
        // What Mobilizon actually sends.
        json!({
            "id": format!("{}/leave/event/1", heidi.actor.id),
            "type": "Leave",
            "actor": heidi.actor.id,
            "object": event_uri,
        }),
        // Leniency: no peer in the fleet sends this, but it costs one match arm.
        json!({
            "id": format!("{}/undo/1", heidi.actor.id),
            "type": "Undo",
            "actor": heidi.actor.id,
            "object": {
                "id": format!("{}/participation/1", heidi.actor.id),
                "type": "Join",
                "actor": heidi.actor.id,
                "object": event_uri,
            },
        }),
    ] {
        let stub = StubFederation::with_actors([heidi.actor.clone()]);
        post_signed(
            test_app_with(pool.clone(), stub.clone()),
            &inbound_join(&heidi, &event_uri, None),
            &heidi,
        )
        .await;
        let attendee = account::find_by_uri(&pool, &heidi.actor.id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            participation::find(&pool, item.id, attendee.id)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            post_signed(test_app_with(pool.clone(), stub), &withdrawal, &heidi).await,
            StatusCode::ACCEPTED
        );
        assert!(
            participation::find(&pool, item.id, attendee.id)
                .await
                .unwrap()
                .is_none(),
            "{} must withdraw the RSVP",
            withdrawal["type"]
        );
    }
}

#[sqlx::test(migrations = "../db/migrations")]
async fn a_join_naming_a_remote_event_is_dropped(pool: PgPool) {
    create_local_account(&pool, "grace", "Grace").await;
    let (_state, _stub, remote_event) = ingest_event(&pool, "free", None).await;

    // We are not the origin of this event, so an attendance we recorded would be
    // one the origin never has. Dropped rather than half-recorded.
    let heidi = RemoteUser::new("mz.example", "heidi");
    let stub = StubFederation::with_actors([heidi.actor.clone()]);
    assert_eq!(
        post_signed(
            test_app_with(pool.clone(), stub),
            &inbound_join(&heidi, EVENT_URI, None),
            &heidi,
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert!(
        participation::for_status(&pool, remote_event.id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn an_invite_from_a_stranger_is_ignored(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let (_state, _stub, item) = ingest_event(&pool, "invite", None).await;

    let stranger = RemoteUser::new("evil.example", "impostor");
    let stub = StubFederation::with_actors([stranger.actor.clone()]);
    let invite = json!({
        "id": "https://evil.example/invite/1",
        "type": "Invite",
        "actor": stranger.actor.id,
        "object": EVENT_URI,
        "target": format!("https://{TEST_DOMAIN}/users/alice"),
    });
    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub), &invite, &stranger).await,
        StatusCode::ACCEPTED
    );
    assert!(
        participation::find(&pool, item.id, alice.id)
            .await
            .unwrap()
            .is_none(),
        "a third party cannot conjure an invitation to someone else's event"
    );
}

#[sqlx::test(migrations = "../db/migrations")]
async fn an_invite_from_the_organizer_is_recorded_and_notified(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let organizer = RemoteUser::new("mz.example", "grace");
    let (_state, _stub, item) = ingest_event_from(&pool, "invite", None, &organizer).await;
    let stub = StubFederation::with_actors([organizer.actor.clone()]);
    let invite = json!({
        "id": "https://mz.example/invite/1",
        "type": "Invite",
        "actor": organizer.actor.id,
        "object": EVENT_URI,
        "target": format!("https://{TEST_DOMAIN}/users/alice"),
    });
    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub), &invite, &organizer).await,
        StatusCode::ACCEPTED
    );
    let row = participation::find(&pool, item.id, alice.id)
        .await
        .unwrap()
        .expect("the invitation was recorded");
    assert_eq!(row.state, State::Invited);
    let notes = plamenu_db::notification::list(
        &pool,
        alice.id,
        None,
        None,
        None,
        plamenu_db::notification::NotificationFilter::default(),
        10,
    )
    .await
    .unwrap();
    assert!(notes.iter().any(|n| n.kind == "event.invite"));

    // And the invitation lifts the invite-only refusal for this viewer only.
    let entity = plamenu::entities::render_status(&pool, TEST_DOMAIN, &item, Some(alice.id))
        .await
        .unwrap();
    assert_eq!(entity["event"]["participation"], "invited");
    assert_eq!(entity["event"]["can_participate"], true);
    let bob = create_local_account(&pool, "bob", "Bob").await;
    let others = plamenu::entities::render_status(&pool, TEST_DOMAIN, &item, Some(bob.id))
        .await
        .unwrap();
    assert_eq!(others["event"]["can_participate"], false);
    assert_eq!(others["event"]["participation_refusal"], "invite_only");
}

// ---------------------------------------------------------------------------
// E4: authoring events
// ---------------------------------------------------------------------------

fn authored_event(start: &str) -> plamenu::actions::EventParams {
    plamenu::actions::EventParams {
        start_time: start.to_owned(),
        end_time: Some("2027-05-01T21:00:00Z".to_owned()),
        timezone: Some("Europe/Ljubljana".to_owned()),
        join_mode: "restricted".to_owned(),
        external_participation_url: None,
        max_attendees: Some(30),
        status: "CONFIRMED".to_owned(),
        is_online: false,
        location_name: Some("Community Hall".to_owned()),
        location_street: Some("Trg 1".to_owned()),
        location_locality: Some("Ljubljana".to_owned()),
        location_region: None,
        location_country: Some("Slovenia".to_owned()),
        location_postal_code: Some("1000".to_owned()),
    }
}

async fn post_event(
    state: &plamenu::state::AppState,
    event: plamenu::actions::EventParams,
) -> status::Status {
    plamenu::actions::post_status(
        state,
        plamenu::actions::PostParams {
            username: "grace",
            text: "Come to our meetup",
            visibility: "public",
            title: Some("Interop meetup"),
            event: Some(event),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .0
}

#[sqlx::test(migrations = "../db/migrations")]
async fn authoring_an_event_emits_a_create_event_with_the_full_shape(pool: PgPool) {
    create_local_account(&pool, "grace", "Grace").await;
    // A remote follower, so the Create actually goes out and can be inspected.
    let heidi = RemoteUser::new("mz.example", "heidi");
    let stub = StubFederation::with_actors([heidi.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let follower = plamenu::remote::store_remote_actor(&pool, &heidi.actor)
        .await
        .unwrap();
    let grace = account::find_local_by_username(&pool, "grace")
        .await
        .unwrap()
        .unwrap();
    plamenu_db::follow::create(
        &pool,
        follower.id,
        grace.id,
        Some("https://mz.example/users/heidi#follows/1"),
    )
    .await
    .unwrap();

    let item = post_event(&state, authored_event("2027-05-01T18:00:00Z")).await;
    // The status is typed on the row, which is what every representation reads.
    assert_eq!(item.object_type.as_deref(), Some("Event"));

    assert!(plamenu::delivery::run_due(&state).await >= 1);
    let create = &stub
        .deliveries()
        .iter()
        .find(|d| d.activity["type"] == "Create")
        .expect("the Create went out")
        .activity
        .clone();
    let object = &create["object"];
    assert_eq!(object["type"], "Event");
    assert_eq!(object["startTime"], "2027-05-01T18:00:00Z");
    assert_eq!(object["endTime"], "2027-05-01T21:00:00Z");
    assert_eq!(object["timezone"], "Europe/Ljubljana");
    assert_eq!(object["joinMode"], "restricted");
    assert_eq!(object["maximumAttendeeCapacity"], 30);
    // Derived, never stored — nobody has joined yet.
    assert_eq!(object["remainingAttendeeCapacity"], 30);
    assert_eq!(object["participantCount"], 0);
    assert_eq!(object["isOnline"], false);
    // Duplicated deliberately: Mobilizon emits both and reads either, and a
    // consumer that only knows the ical term must still see a cancellation.
    assert_eq!(object["status"], "CONFIRMED");
    assert_eq!(object["ical:status"], "CONFIRMED");
    // Never `true` on the wire: an unpublished event is not federated at all.
    assert_eq!(object["draft"], false);
    // The structured Place, with its address nested.
    assert_eq!(object["location"]["type"], "Place");
    assert_eq!(object["location"]["name"], "Community Hall");
    assert_eq!(object["location"]["address"]["streetAddress"], "Trg 1");
    assert_eq!(
        object["location"]["address"]["addressLocality"],
        "Ljubljana"
    );
    assert_eq!(object["location"]["address"]["postalCode"], "1000");
    assert_eq!(object["location"]["address"]["addressCountry"], "Slovenia");
    // A region we never set must be absent, not an empty string.
    assert!(object["location"]["address"]["addressRegion"].is_null());

    // The object served at the post's own URL agrees with the Create — a
    // consumer that re-fetches (Lemmy, Mobilizon) must not see a different event.
    let served = plamenu::note::note_for_status(&state, &item, &grace)
        .await
        .unwrap();
    assert_eq!(served["type"], "Event");
    assert_eq!(served["startTime"], object["startTime"]);
    assert_eq!(served["joinMode"], object["joinMode"]);
    assert_eq!(served["location"], object["location"]);
}

#[sqlx::test(migrations = "../db/migrations")]
async fn an_event_without_a_start_time_is_refused(pool: PgPool) {
    create_local_account(&pool, "grace", "Grace").await;
    let stub = StubFederation::with_actors([]);
    let state = test_state_with(pool.clone(), stub);
    for bad in ["", "not a date", "2027-05-01"] {
        let mut params = authored_event("2027-05-01T18:00:00Z");
        params.start_time = bad.to_owned();
        let refused = plamenu::actions::post_status(
            &state,
            plamenu::actions::PostParams {
                username: "grace",
                text: "when?",
                visibility: "public",
                title: Some("When?"),
                event: Some(params),
                ..Default::default()
            },
        )
        .await;
        assert!(
            matches!(refused, Err(plamenu::error::ApiError::Unprocessable(_))),
            "{bad:?} is not a start time"
        );
    }

    // An end before the start is incoherent rather than merely odd.
    let mut backwards = authored_event("2027-05-01T18:00:00Z");
    backwards.end_time = Some("2027-05-01T17:00:00Z".to_owned());
    assert!(matches!(
        plamenu::actions::post_status(
            &state,
            plamenu::actions::PostParams {
                username: "grace",
                text: "backwards",
                visibility: "public",
                title: Some("Backwards"),
                event: Some(backwards),
                ..Default::default()
            },
        )
        .await,
        Err(plamenu::error::ApiError::Unprocessable(_))
    ));

    // An `external` event with nowhere to send people offers no way to attend.
    let mut external = authored_event("2027-05-01T18:00:00Z");
    external.join_mode = "external".to_owned();
    external.external_participation_url = None;
    assert!(matches!(
        plamenu::actions::post_status(
            &state,
            plamenu::actions::PostParams {
                username: "grace",
                text: "elsewhere",
                visibility: "public",
                title: Some("Elsewhere"),
                event: Some(external),
                ..Default::default()
            },
        )
        .await,
        Err(plamenu::error::ApiError::Unprocessable(_))
    ));
}

#[sqlx::test(migrations = "../db/migrations")]
async fn an_ordinary_post_never_becomes_an_event(pool: PgPool) {
    create_local_account(&pool, "grace", "Grace").await;
    let stub = StubFederation::with_actors([]);
    let state = test_state_with(pool.clone(), stub);
    // Text full of dates, no event kind chosen. Mastodon truncates an `Event` to
    // a title-plus-link stub, so an implicit upgrade would silently cost the
    // author most of their readers.
    let (item, _) = plamenu::actions::post_status(
        &state,
        plamenu::actions::PostParams {
            username: "grace",
            text: "See you 2027-05-01T18:00:00Z at the Community Hall!",
            visibility: "public",
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(item.object_type, None);
    assert!(
        status_event::find(&pool, item.id).await.unwrap().is_none(),
        "no sidecar without the event kind"
    );
    let grace = account::find_local_by_username(&pool, "grace")
        .await
        .unwrap()
        .unwrap();
    let served = plamenu::note::note_for_status(&state, &item, &grace)
        .await
        .unwrap();
    assert_eq!(served["type"], "Note");
    assert!(served["startTime"].is_null());
}

#[sqlx::test(migrations = "../db/migrations")]
async fn moving_and_cancelling_an_event_notifies_every_live_rsvp(pool: PgPool) {
    create_local_account(&pool, "grace", "Grace").await;
    let attendee = create_local_account(&pool, "heidi", "Heidi").await;
    let refused = create_local_account(&pool, "ivan", "Ivan").await;
    let stub = StubFederation::with_actors([]);
    let state = test_state_with(pool.clone(), stub);
    let grace = account::find_local_by_username(&pool, "grace")
        .await
        .unwrap()
        .unwrap();
    let item = post_event(&state, authored_event("2027-05-01T18:00:00Z")).await;
    participation::upsert(&pool, item.id, attendee.id, State::Accepted, None, None)
        .await
        .unwrap();
    participation::upsert(&pool, item.id, refused.id, State::Rejected, None, None)
        .await
        .unwrap();

    let notes_of = |account_id: i64| {
        let pool = pool.clone();
        async move {
            plamenu_db::notification::list(
                &pool,
                account_id,
                None,
                None,
                None,
                plamenu_db::notification::NotificationFilter::default(),
                20,
            )
            .await
            .unwrap()
        }
    };

    // Moved — the end moves with it, or the event would end before it starts.
    let moved = plamenu::actions::EventPatch {
        start_time: Some("2027-05-02T19:00:00Z".to_owned()),
        end_time: Some("2027-05-02T22:00:00Z".to_owned()),
        ..Default::default()
    };
    plamenu::actions::edit_status(
        &state,
        &grace,
        item.id,
        plamenu::actions::EditParams {
            title: None,
            text: None,
            content_type: None,
            spoiler_text: None,
            sensitive: None,
            language: None,
            media_ids: None,
            quote_approval_policy: None,
            media_attributes: Vec::new(),
            event: Some(moved),
        },
    )
    .await
    .unwrap();
    let sidecar = status_event::find(&pool, item.id).await.unwrap().unwrap();
    assert_eq!(
        sidecar
            .start_time
            .unwrap()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap(),
        "2027-05-02T19:00:00Z"
    );
    assert!(
        notes_of(attendee.id)
            .await
            .iter()
            .any(|n| n.kind == "event.changed"),
        "an attendee organized their day around this"
    );
    assert!(
        !notes_of(refused.id)
            .await
            .iter()
            .any(|n| n.kind == "event.changed"),
        "a refused attendee is not coming and has nothing to re-plan"
    );

    // Cancelled — the flip that matters most.
    // A patch of one field: everything else is carried over by the merge.
    let cancelled = plamenu::actions::EventPatch {
        status: Some("CANCELLED".to_owned()),
        ..Default::default()
    };
    plamenu::actions::edit_status(
        &state,
        &grace,
        item.id,
        plamenu::actions::EditParams {
            title: None,
            text: None,
            content_type: None,
            spoiler_text: None,
            sensitive: None,
            language: None,
            media_ids: None,
            quote_approval_policy: None,
            media_attributes: Vec::new(),
            event: Some(cancelled),
        },
    )
    .await
    .unwrap();
    let sidecar = status_event::find(&pool, item.id).await.unwrap().unwrap();
    assert!(sidecar.is_cancelled());
    assert_eq!(
        notes_of(attendee.id)
            .await
            .iter()
            .filter(|n| n.kind == "event.changed")
            .count(),
        2,
        "the move and the cancellation are two separate things to know"
    );
    // A cancelled event cannot be joined.
    assert_eq!(
        plamenu::events::rsvp_refusal(&sidecar, true),
        Some(plamenu::events::RsvpRefusal::Cancelled)
    );

    // And the served object says so in both spellings.
    let served = plamenu::note::note_for_status(&state, &item, &grace)
        .await
        .unwrap();
    assert_eq!(served["status"], "CANCELLED");
    assert_eq!(served["ical:status"], "CANCELLED");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn an_edit_never_promotes_a_note_into_an_event(pool: PgPool) {
    create_local_account(&pool, "grace", "Grace").await;
    let stub = StubFederation::with_actors([]);
    let state = test_state_with(pool.clone(), stub);
    let grace = account::find_local_by_username(&pool, "grace")
        .await
        .unwrap()
        .unwrap();
    let (note, _) = plamenu::actions::post_status(
        &state,
        plamenu::actions::PostParams {
            username: "grace",
            text: "an ordinary note",
            visibility: "public",
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // The wire type is part of what the audience already received; changing it
    // would strand every consumer that stored this as a Note.
    plamenu::actions::edit_status(
        &state,
        &grace,
        note.id,
        plamenu::actions::EditParams {
            title: None,
            text: Some("an ordinary note, edited"),
            content_type: None,
            spoiler_text: None,
            sensitive: None,
            language: None,
            media_ids: None,
            quote_approval_policy: None,
            media_attributes: Vec::new(),
            event: Some(plamenu::actions::EventPatch {
                start_time: Some("2027-05-01T18:00:00Z".to_owned()),
                ..Default::default()
            }),
        },
    )
    .await
    .unwrap();
    assert!(
        status_event::find(&pool, note.id).await.unwrap().is_none(),
        "an edit must not conjure an event sidecar onto a Note"
    );
    let served = plamenu::note::note_for_status(&state, &note, &grace)
        .await
        .unwrap();
    assert_eq!(served["type"], "Note");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn an_event_update_reaches_remote_attendees_who_follow_nobody(pool: PgPool) {
    create_local_account(&pool, "grace", "Grace").await;
    let heidi = RemoteUser::new("mz.example", "heidi");
    let stub = StubFederation::with_actors([heidi.actor.clone()]);
    let state = test_state_with(pool.clone(), stub.clone());
    let grace = account::find_local_by_username(&pool, "grace")
        .await
        .unwrap()
        .unwrap();
    let attendee = plamenu::remote::store_remote_actor(&pool, &heidi.actor)
        .await
        .unwrap();
    let item = post_event(&state, authored_event("2027-05-01T18:00:00Z")).await;
    // An attendee and nothing else: no follow, no mention. Someone who found the
    // event by URL and RSVP'd has organized their day around it.
    participation::upsert(&pool, item.id, attendee.id, State::Accepted, None, None)
        .await
        .unwrap();
    plamenu::delivery::run_due(&state).await;
    let before = stub.deliveries().len();

    plamenu::actions::edit_status(
        &state,
        &grace,
        item.id,
        plamenu::actions::EditParams {
            title: None,
            text: None,
            content_type: None,
            spoiler_text: None,
            sensitive: None,
            language: None,
            media_ids: None,
            quote_approval_policy: None,
            media_attributes: Vec::new(),
            event: Some(plamenu::actions::EventPatch {
                status: Some("CANCELLED".to_owned()),
                ..Default::default()
            }),
        },
    )
    .await
    .unwrap();
    plamenu::delivery::run_due(&state).await;

    let sent = stub.deliveries();
    let update = sent[before..]
        .iter()
        .find(|d| d.activity["type"] == "Update")
        .expect("the cancellation federates");
    // Addressed via the host's SHARED inbox, like every other fan-out: building
    // this list from personal inboxes would turn 200 attendees on one server into
    // 200 signed POSTs.
    assert_eq!(
        update.inbox_url,
        heidi
            .actor
            .endpoints
            .as_ref()
            .unwrap()
            .shared_inbox
            .clone()
            .unwrap(),
        "the attendee is addressed even though they follow nobody"
    );
    assert_eq!(update.activity["object"]["status"], "CANCELLED");
    assert_eq!(update.activity["object"]["ical:status"], "CANCELLED");
}

/// Subscribing to a general-purpose server's relay actor must not swallow that
/// server's ordinary group traffic.
///
/// Mobilizon's relay actor lives at `/relay` but advertises the instance's
/// **shared** inbox (`/inbox`) — the same one every group and person on the host
/// advertises. Identifying a relay by inbox URL therefore made *every* actor on
/// that host look like the relay, and a relay's `Announce` is deliberately a
/// delivery hint rather than a boost: following a Mobilizon instance silently
/// stopped its groups' events from reaching any follower's timeline. The
/// subscription broke the ordinary follow it was meant to complement.
#[sqlx::test(migrations = "../db/migrations")]
async fn a_relay_on_a_shared_inbox_does_not_capture_the_hosts_other_actors(pool: PgPool) {
    create_local_account(&pool, "alice", "Alice").await;
    // The relay actor and a group, both on one host, both advertising `/inbox`.
    let relay_actor = RemoteUser::new("mz.example", "relay");
    let mut group = RemoteUser::new("mz.example", "thegroup");
    "Group".clone_into(&mut group.actor.kind);
    let stub = StubFederation::with_actors([relay_actor.actor.clone(), group.actor.clone()]);
    let state = test_state_with(pool.clone(), stub);

    let relay = plamenu_db::relay::create(
        &pool,
        "https://mz.example/inbox",
        Some(&relay_actor.actor.id),
    )
    .await
    .unwrap()
    .unwrap();
    plamenu_db::relay::mark_pending(&pool, relay.id, "https://plamenu.test/payloads/1")
        .await
        .unwrap();
    plamenu_db::relay::resolve_follow_response(&pool, "https://plamenu.test/payloads/1", true)
        .await
        .unwrap();

    let stored_relay = plamenu::remote::store_remote_actor(&pool, &relay_actor.actor)
        .await
        .unwrap();
    let stored_group = plamenu::remote::store_remote_actor(&pool, &group.actor)
        .await
        .unwrap();
    // Both advertise the same shared inbox — which is the whole problem.
    assert_eq!(stored_relay.shared_inbox_url, stored_group.shared_inbox_url);

    assert_eq!(
        plamenu::relays::enabled_relay_for_sender(&state, &stored_relay)
            .await
            .unwrap(),
        Some(relay.id),
        "the relay actor is recognised by its own identity"
    );
    assert_eq!(
        plamenu::relays::enabled_relay_for_sender(&state, &stored_group)
            .await
            .unwrap(),
        None,
        "a group sharing the relay's inbox is not the relay"
    );
}

/// An invitation must be *usable*: clicking Attend on an invite-only event we
/// were invited to has to emit a real `Join`.
///
/// Regression test. `rsvp()` used to early-return on any existing participation
/// row, and an invitation *is* a row — so the button returned the invitation
/// unchanged and federated nothing. The inbound half of this transition was
/// covered; this outbound half was not, which is exactly why it went unnoticed.
#[sqlx::test(migrations = "../db/migrations")]
async fn an_invitation_lets_us_join_an_invite_only_event(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let (state, stub, item) = ingest_event(&pool, "invite", None).await;

    // Uninvited, the event refuses us outright and nothing goes out.
    assert!(
        plamenu::events::rsvp(&state, &alice, item.id, None)
            .await
            .is_err()
    );
    assert_eq!(plamenu::delivery::run_due(&state).await, 0);

    // The organizer invites us.
    plamenu::events::record_invitation(&state, &item, &alice, item.account_id)
        .await
        .unwrap();
    assert_eq!(
        participation::find(&pool, item.id, alice.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        State::Invited
    );

    // Now Attend must produce a Join — not hand the invitation back.
    let row = plamenu::events::rsvp(&state, &alice, item.id, None)
        .await
        .unwrap();
    assert_eq!(row.state, State::Pending, "the invitation was acted on");
    assert!(row.uri.is_some(), "a Join id was minted");
    assert_eq!(plamenu::delivery::run_due(&state).await, 1);
    let join = &stub.deliveries()[0].activity;
    assert_eq!(join["type"], "Join");
    assert_eq!(join["object"], EVENT_URI);
}

/// An `Invite` is authorized more narrowly than a verdict, and deliberately so.
///
/// A verdict only applies to a `Join` we minted and stored, so trusting the
/// event's whole origin host there costs at most the resolution of one RSVP we
/// started. An `Invite` creates state for an account that has done nothing — a
/// notification in their list, and an invite-only event opened to them — so
/// "somebody else on the organizer's server" is not good enough.
#[sqlx::test(migrations = "../db/migrations")]
async fn only_the_organizer_or_its_group_may_invite(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let organizer = RemoteUser::new("mz.example", "grace");
    let (_state, _stub, item) = ingest_event_from(&pool, "invite", None, &organizer).await;
    let target = format!("https://{TEST_DOMAIN}/users/alice");

    // A stranger sharing the event's origin host: enough for a verdict, not for
    // an invitation.
    let neighbour = RemoteUser::new("mz.example", "neighbour");
    let stub = StubFederation::with_actors([neighbour.actor.clone()]);
    let invite = json!({
        "id": "https://mz.example/invite/1",
        "type": "Invite",
        "actor": neighbour.actor.id,
        "object": EVENT_URI,
        "target": target,
    });
    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub), &invite, &neighbour).await,
        StatusCode::ACCEPTED
    );
    assert!(
        participation::find(&pool, item.id, alice.id)
            .await
            .unwrap()
            .is_none(),
        "a same-host stranger cannot conjure an invitation"
    );

    // The organizer can.
    let stub = StubFederation::with_actors([organizer.actor.clone()]);
    let invite = json!({
        "id": "https://mz.example/invite/2",
        "type": "Invite",
        "actor": organizer.actor.id,
        "object": EVENT_URI,
        "target": target,
    });
    assert_eq!(
        post_signed(test_app_with(pool.clone(), stub), &invite, &organizer).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        participation::find(&pool, item.id, alice.id)
            .await
            .unwrap()
            .expect("the organizer's invitation is recorded")
            .state,
        State::Invited
    );
}

async fn reject_outbox(pool: &PgPool) {
    sqlx::query("CREATE OR REPLACE FUNCTION reject_delivery() RETURNS trigger AS $$ BEGIN RAISE EXCEPTION 'injected outbox failure'; END; $$ LANGUAGE plpgsql").execute(pool).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_delivery BEFORE INSERT ON delivery_jobs FOR EACH ROW EXECUTE FUNCTION reject_delivery()").execute(pool).await.unwrap();
}

async fn restore_outbox(pool: &PgPool) {
    sqlx::query("DROP TRIGGER reject_delivery ON delivery_jobs")
        .execute(pool)
        .await
        .unwrap();
}

#[sqlx::test(migrations = "../db/migrations")]
async fn rsvp_and_cancellation_roll_back_when_outbox_fails(pool: PgPool) {
    let alice = create_local_account(&pool, "alice", "Alice").await;
    let (state, stub, item) = ingest_event(&pool, "free", None).await;
    reject_outbox(&pool).await;
    let error = plamenu::events::rsvp(&state, &alice, item.id, None)
        .await
        .unwrap_err();
    assert!(format!("{error:?}").contains("injected outbox failure"));
    assert!(
        participation::find(&pool, item.id, alice.id)
            .await
            .unwrap()
            .is_none()
    );
    restore_outbox(&pool).await;
    let joined = plamenu::events::rsvp(&state, &alice, item.id, None)
        .await
        .unwrap();
    assert_eq!(plamenu::delivery::run_due(&state).await, 1);
    assert_eq!(stub.deliveries()[0].activity["type"], "Join");
    reject_outbox(&pool).await;
    let error = plamenu::events::cancel_rsvp(&state, &alice, item.id)
        .await
        .unwrap_err();
    assert!(format!("{error:?}").contains("injected outbox failure"));
    let retained = participation::find(&pool, item.id, alice.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(retained.id, joined.id);
    assert_eq!(retained.state, joined.state);
    restore_outbox(&pool).await;
    plamenu::events::cancel_rsvp(&state, &alice, item.id)
        .await
        .unwrap();
    assert!(
        participation::find(&pool, item.id, alice.id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(plamenu::delivery::run_due(&state).await, 1);
    assert_eq!(stub.deliveries()[1].activity["type"], "Leave");
}

#[sqlx::test(migrations = "../db/migrations")]
async fn rsvp_verdict_rolls_back_when_outbox_fails(pool: PgPool) {
    let grace = create_local_account(&pool, "grace", "Grace").await;
    let item = local_event(&pool, grace.id, "restricted", None).await;
    let heidi = RemoteUser::new("mz.example", "heidi");
    let attendee = plamenu::remote::store_remote_actor(&pool, &heidi.actor)
        .await
        .unwrap();
    let state = test_state_with(
        pool.clone(),
        StubFederation::with_actors([heidi.actor.clone()]),
    );
    participation::upsert(
        &pool,
        item.id,
        attendee.id,
        State::Pending,
        Some("https://mz.example/join/1"),
        None,
    )
    .await
    .unwrap();
    reject_outbox(&pool).await;
    let error = plamenu::events::decide_rsvp(&state, &grace, item.id, attendee.id, true)
        .await
        .unwrap_err();
    assert!(format!("{error:?}").contains("injected outbox failure"));
    assert_eq!(
        participation::find(&pool, item.id, attendee.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        State::Pending
    );
    restore_outbox(&pool).await;
    plamenu::events::decide_rsvp(&state, &grace, item.id, attendee.id, true)
        .await
        .unwrap();
    assert_eq!(
        participation::find(&pool, item.id, attendee.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        State::Accepted
    );
    assert_eq!(plamenu::delivery::run_due(&state).await, 1);
}
