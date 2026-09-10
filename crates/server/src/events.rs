//! Event participation — the RSVP service (E2/E3).
//!
//! An RSVP is a *negotiation*, not a toggle: we send a `Join`, and the organizer
//! answers with `Accept(Join)` or `Reject(Join)` — or never answers at all. That
//! last case is ordinary, not a fault: Mobilizon sends **nothing back** when an
//! event is at `maximumAttendeeCapacity`, and a `restricted` event waits on a
//! human. So every state here is a resting state, and nothing retries or times
//! out an RSVP into failure. A vanished organizer leaves a `pending` row, which
//! is exactly what the chaotic-federation rule asks for — never a wedged UI.
//!
//! Two counts, never mixed:
//!
//! * a **remote** event's attendance is the origin's `participant_count`, since
//!   we only ever receive the participation activities addressed to us and
//!   counting our own rows would systematically undercount;
//! * a **local** event's is `status_participations`, which we own completely.

use plamenu_ap::activity::{self, RsvpParams};
use plamenu_db::account::{self, Account};
use plamenu_db::status::{self, Status};
use plamenu_db::status_event::{self, StatusEvent};
use plamenu_db::status_participation::{self as participation, Participation, State};
use plamenu_db::{group, job};
use serde_json::Value;

use crate::entities::{account_uri, status_uri_for_account};
use crate::error::ApiError;
use crate::state::AppState;

/// Longest accepted `participationMessage` on an outbound `Join`. Mobilizon's
/// own composer is a short free-text box; this is generous for a note to an
/// organizer and keeps a pathological body out of a delivery payload.
pub const MAX_PARTICIPATION_MESSAGE: usize = 1000;

/// The organizer learns someone wants in. One kind for both "is attending" and
/// "requested to attend" — the RSVP's own state says which, and splitting it
/// would double the notification-filter surface for no gain.
pub const NOTIFY_PARTICIPATION: &str = "event.participation";
/// Our RSVP was accepted.
pub const NOTIFY_ACCEPTED: &str = "event.accepted";
/// Our RSVP was refused.
pub const NOTIFY_REJECTED: &str = "event.rejected";
/// An event we are attending moved or was cancelled.
pub const NOTIFY_CHANGED: &str = "event.changed";
/// We were invited to an event.
pub const NOTIFY_INVITED: &str = "event.invite";

/// A participation message trimmed to something we are willing to put on the wire.
fn clamp_message(message: Option<&str>) -> Option<String> {
    message
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(|m| m.chars().take(MAX_PARTICIPATION_MESSAGE).collect())
}

/// Records an invitation to a local account and notifies them.
///
/// Idempotent, and never an upgrade: `upsert` leaves a row that has already
/// advanced past `invited` alone, so re-inviting someone who already joined does
/// not reset their RSVP, and inviting someone previously rejected does not
/// silently readmit them.
pub async fn record_invitation(
    state: &AppState,
    item: &Status,
    invitee: &Account,
    inviter_id: i64,
) -> Result<Participation, ApiError> {
    let existing = participation::find(&state.pool, item.id, invitee.id).await?;
    let row =
        participation::upsert(&state.pool, item.id, invitee.id, State::Invited, None, None).await?;
    // Notify only on a genuinely new invitation — a redelivered `Invite` must not
    // ping the invitee again.
    if existing.is_none() {
        plamenu_db::notification::create(
            &state.pool,
            invitee.id,
            inviter_id,
            NOTIFY_INVITED,
            Some(item.id),
        )
        .await?;
    }
    Ok(row)
}

/// Tells a local organizer that someone RSVP'd to their event.
///
/// Silent when the organizer is the attendee (joining your own event notifies
/// nobody) and when the event is remote — a remote organizer learns from the
/// `Join` itself.
/// Takes no RSVP state on purpose: an auto-accepted RSVP on a `free` event and a
/// request awaiting approval on a `restricted` one are the same notification, and
/// the row's own state is what tells the UI which sentence to render.
pub async fn notify_organizer(
    state: &AppState,
    item: &Status,
    attendee: &Account,
) -> Result<(), ApiError> {
    if item.account_id == attendee.id {
        return Ok(());
    }
    plamenu_db::notification::create(
        &state.pool,
        item.account_id,
        attendee.id,
        NOTIFY_PARTICIPATION,
        Some(item.id),
    )
    .await?;
    Ok(())
}

/// Tells a local attendee that their RSVP was answered.
pub async fn notify_attendee(
    state: &AppState,
    item: &Status,
    attendee_id: i64,
    accepted: bool,
) -> Result<(), ApiError> {
    let kind = if accepted {
        NOTIFY_ACCEPTED
    } else {
        NOTIFY_REJECTED
    };
    plamenu_db::notification::create(
        &state.pool,
        attendee_id,
        item.account_id,
        kind,
        Some(item.id),
    )
    .await?;
    Ok(())
}

/// Tells everyone with a live RSVP that the event moved or was cancelled.
///
/// Only `accepted` and `pending` rows are told: a rejected attendee is not
/// coming, and an invitee who never acted has nothing to re-plan. Called from the
/// `Update(Event)` ingest path, which is the only place that can see a changed
/// `startTime` or an `ical:status` flip.
pub async fn notify_event_changed(state: &AppState, item: &Status) -> Result<(), ApiError> {
    let recipients: Vec<(i64, i64)> = participation::live_local_account_ids(&state.pool, item.id)
        .await?
        .into_iter()
        .map(|account_id| (account_id, item.id))
        .collect();
    plamenu_db::notification::create_ungroupable_many(
        &state.pool,
        &recipients,
        item.account_id,
        NOTIFY_CHANGED,
    )
    .await?;
    Ok(())
}

/// Why an RSVP cannot be offered. Each variant is a *different sentence* in the
/// UI, because "you can't join this" and "this event is full" send the viewer to
/// very different next actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RsvpRefusal {
    /// `joinMode: external` — attendance happens on the origin's own site, so
    /// there is nothing for us to send. The UI links out instead.
    External,
    /// `joinMode: invite` — we were not invited. An accepted `Invite` lifts this
    /// (E3), which is why invitation state is checked before this refusal.
    InviteOnly,
    /// The event is cancelled; joining it would be meaningless.
    Cancelled,
    /// The origin says capacity is exhausted. Advisory for a remote event (the
    /// origin decides), enforced for a local one.
    Full,
}

impl RsvpRefusal {
    /// A stable slug for the client and the web UI's message lookup.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::External => "external",
            Self::InviteOnly => "invite_only",
            Self::Cancelled => "cancelled",
            Self::Full => "full",
        }
    }
}

/// Whether an RSVP can be offered for this event, and if not, why.
///
/// `invited` is the escape hatch for `joinMode: invite`: an event we were invited
/// to is joinable even though the general public cannot join it.
#[must_use]
pub fn rsvp_refusal(event: &StatusEvent, invited: bool) -> Option<RsvpRefusal> {
    if event.is_cancelled() {
        return Some(RsvpRefusal::Cancelled);
    }
    match event.join_mode.as_deref() {
        Some("external") => return Some(RsvpRefusal::External),
        Some("invite") if !invited => return Some(RsvpRefusal::InviteOnly),
        _ => {}
    }
    // Capacity last: "full" is the more useful message than "invite only" when
    // both are true and we *were* invited.
    if event.is_full() {
        return Some(RsvpRefusal::Full);
    }
    None
}

/// The state a fresh RSVP to *our own* event lands in. `free` auto-accepts;
/// everything else waits for a moderator, which is what makes the approval queue
/// meaningful (§9.4: local events get the full matrix).
///
/// An attendee we **invited** auto-accepts whatever the join mode says: the
/// organizer already decided when they sent the `Invite`, and asking them to
/// approve the same person twice is a queue item that carries no information.
#[must_use]
pub fn initial_state_for_local_event(event: &StatusEvent, invited: bool) -> State {
    if invited {
        return State::Accepted;
    }
    match event.join_mode.as_deref() {
        // No stated mode on an event we host means the composer default, `free`.
        Some("free") | None => State::Accepted,
        _ => State::Pending,
    }
}

/// The sidecar of a status, or `NotFound` when it is not an event. Every entry
/// point needs this, and treating a non-event as 404 (rather than 422) keeps the
/// RSVP routes from confirming that some unrelated status id exists.
async fn event_of(state: &AppState, item: &Status) -> Result<StatusEvent, ApiError> {
    status_event::find(&state.pool, item.id)
        .await?
        .ok_or(ApiError::NotFound)
}

/// Who decides on an RSVP to this event, and where to send it.
///
/// For a group event that is the group (Mobilizon routes the `Join` to a group
/// moderator); otherwise the organizer. Both are returned because the activity
/// addresses the deciding party in `to` while delivery goes to an inbox — and for
/// a group event on the same host those are the same shared inbox anyway.
async fn rsvp_target(
    state: &AppState,
    item: &Status,
) -> Result<(Option<String>, Option<String>), ApiError> {
    let organizer = account::find_by_id(&state.pool, item.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    // A remote group that announced the event is the deciding party.
    for group_id in group::boosting_group_ids(&state.pool, item.id).await? {
        if let Some(group_account) = account::find_by_id(&state.pool, group_id).await?
            && !group_account.is_local()
        {
            return Ok((
                Some(account_uri(&state.config.domain, &group_account)),
                Some(group_account.inbox_url.clone()),
            ));
        }
    }
    let uri = account_uri(&state.config.domain, &organizer);
    let inbox = (!organizer.is_local()).then(|| organizer.inbox_url.clone());
    Ok((Some(uri), inbox))
}

/// RSVPs to an event: records the participation and emits the `Join`.
///
/// Idempotent — a repeat call on an existing row re-reads it rather than sending
/// a second `Join`, so a double-tapped button cannot produce two participations
/// on the origin. A row that was already `rejected` stays rejected: re-asking is
/// not something a click may silently do.
pub async fn rsvp(
    state: &AppState,
    actor: &Account,
    status_id: i64,
    message: Option<&str>,
) -> Result<Participation, ApiError> {
    let item = status::find_by_id(&state.pool, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if item.reblog_of_id.is_some()
        || !crate::entities::can_view(&state.pool, &item, Some(actor.id)).await?
    {
        return Err(ApiError::NotFound);
    }
    let event = event_of(state, &item).await?;

    // An existing row is returned untouched — EXCEPT an invitation, which exists
    // precisely so the invitee can act on it: falling through here is what turns
    // "you're invited" into an actual `Join`. Returning it (as this once did) made
    // the Attend button on an invite-only event silently federate nothing.
    let existing = participation::find(&state.pool, item.id, actor.id).await?;
    let invited = existing
        .as_ref()
        .is_some_and(|row| row.state == State::Invited);
    if let Some(existing) = existing
        && !invited
    {
        return Ok(existing);
    }

    let organizer = account::find_by_id(&state.pool, item.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let local_event = organizer.is_local();
    // On someone else's event the origin decides, so our capacity read is only
    // advisory and we let a `Full` event through rather than second-guessing a
    // stale count. On our own it is authoritative and enforced (§9.5).
    if let Some(refusal) = rsvp_refusal(&event, invited) {
        let enforced = local_event || refusal != RsvpRefusal::Full;
        if enforced {
            return Err(ApiError::Unprocessable(format!(
                "Validation failed: this event cannot be joined ({})",
                refusal.as_str()
            )));
        }
    }
    if local_event && local_event_is_full(state, &item, &event).await? {
        return Err(ApiError::Unprocessable(
            "Validation failed: this event is full".into(),
        ));
    }

    let message = clamp_message(message);

    // Our own event resolves immediately — there is no round trip to wait for.
    let initial = if local_event {
        initial_state_for_local_event(&event, invited)
    } else {
        State::Pending
    };
    // The activity id embeds the row id, so the origin's echoed `Accept` resolves
    // straight back to this row. Row ids are client-side snowflakes, so the uri is
    // minted before the insert — one write, rather than insert-then-restamp.
    let row_id = plamenu_db::id::next();
    let join_uri = activity::join_uri_for_actor(&account_uri(&state.config.domain, actor), row_id);
    let (target_uri, inbox) = rsvp_target(state, &item).await?;
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let row = participation::upsert_with_id(
        &mut *tx,
        row_id,
        item.id,
        actor.id,
        initial,
        Some(&join_uri),
        message.as_deref(),
    )
    .await?;

    if local_event {
        // Local organizer: notify instead of federating to ourselves.
        tx.commit().await.map_err(plamenu_db::DbError::from)?;
        if let Err(error) = notify_organizer(state, &item, actor).await {
            tracing::warn!(%error, "post-commit RSVP notification failed");
        }
        return Ok(row);
    }
    if let Some(inbox) = inbox {
        let join = activity::join_event(&RsvpParams {
            domain: &state.config.domain,
            username: &actor.username,
            marker: row.id,
            event_uri: &status_uri_for_account(&state.config.domain, &item, &organizer),
            target_uri: target_uri.as_deref(),
            message: row.message.as_deref(),
        });
        job::enqueue(&mut *tx, actor.id, &inbox, &join).await?;
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(row)
}

/// Withdraws an RSVP: drops the row and emits `Leave`.
///
/// A bare `Leave`, never `Undo(Join)` — Mobilizon's transmogrifier has no
/// `Undo(Join)` arm, so an `Undo` would be dropped by the one peer that hosts
/// events. Idempotent: leaving an event we never joined is a no-op.
pub async fn cancel_rsvp(
    state: &AppState,
    actor: &Account,
    status_id: i64,
) -> Result<Option<Participation>, ApiError> {
    let item = status::find_by_id(&state.pool, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let organizer = account::find_by_id(&state.pool, item.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let (target_uri, inbox) = rsvp_target(state, &item).await?;
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let Some(row) = participation::delete(&mut *tx, item.id, actor.id).await? else {
        return Ok(None);
    };
    if let Some(inbox) = inbox {
        let leave = activity::leave_event(&RsvpParams {
            domain: &state.config.domain,
            username: &actor.username,
            marker: row.id,
            event_uri: &status_uri_for_account(&state.config.domain, &item, &organizer),
            target_uri: target_uri.as_deref(),
            message: None,
        });
        job::enqueue(&mut *tx, actor.id, &inbox, &leave).await?;
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(Some(row))
}

/// Whether `object_id` is the id of a `Join` we sent.
///
/// The cheap disambiguator for an inbound `Accept`/`Reject` that names its object
/// by bare IRI instead of embedding it — without this, such a verdict would fall
/// through to the follow-response handler and be lost.
pub async fn names_our_join(state: &AppState, object_id: Option<&str>) -> Result<bool, ApiError> {
    let Some(uri) = object_id else {
        return Ok(false);
    };
    Ok(participation::find_by_uri(&state.pool, uri)
        .await?
        .is_some())
}

/// Whether `sender` has any claim to decide an RSVP to `item`.
///
/// Four shapes are legitimate, and the last three are why this cannot simply
/// compare the sender to the event's author:
///
/// 1. **the organizer** — the event's own author answers.
/// 2. **the announcing group** — a group event's verdict may come from the group
///    actor itself.
/// 3. **a moderator of the announcing group** — Mobilizon's actual shape: `actor`
///    is `get_single_group_moderator_actor(group_id)` and `attributedTo` is the
///    group. This reuses the group rule (key on `attributedTo`, then verify the
///    actor stands behind it) rather than inventing a second trust model.
/// 4. **any actor on the event's own origin host**, which is what makes case 3
///    work when the group's `Announce` never reached us (deduped away, dropped,
///    or the event arrived through the relay instead). We cannot check a group's
///    membership without knowing the group, and the group is only known from its
///    boost row.
///
/// Case 4 grants nothing new: the origin host already authors, edits and deletes
/// this event at will — a server that wanted to fake an `Accept` for its own
/// event could simply re-`Create` it. What must not happen is a **third** host
/// deciding, and every case here is bounded to the event's origin or to a group
/// on its own host. A claimed `attributedTo` is never believed across hosts;
/// otherwise a group's authority would be a free-floating string anyone may quote.
pub async fn verdict_is_trusted(
    state: &AppState,
    item: &Status,
    sender: &Account,
    raw: &Value,
) -> Result<bool, ApiError> {
    if sender.id == item.account_id {
        return Ok(true);
    }
    let Some(sender_uri) = sender.uri.as_deref() else {
        // A local actor answering a remote event's RSVP is nonsense.
        return Ok(false);
    };
    let announcing = group::boosting_group_ids(&state.pool, item.id).await?;
    if announcing.contains(&sender.id) {
        return Ok(true);
    }
    // Case 4: the origin host decides its own events.
    if let Some(event_uri) = item.uri.as_deref()
        && same_host(event_uri, sender_uri)
    {
        return Ok(true);
    }
    // Case 3, for a group hosted elsewhere than the event itself.
    let Some(claimed) = raw
        .get("attributedTo")
        .and_then(plamenu_ap::activity::id_of)
    else {
        return Ok(false);
    };
    for group_id in announcing {
        let Some(group_account) = account::find_by_id(&state.pool, group_id).await? else {
            continue;
        };
        let group_uri = account_uri(&state.config.domain, &group_account);
        if group_uri == claimed && same_host(&group_uri, sender_uri) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether two absolute URIs share an authority.
///
/// Delegates to [`crate::remote::host_of`] rather than splitting by hand: this is
/// a *trust* comparison, and the shared parser is the one that requires https and
/// strips query/fragment. Four hand-rolled spellings of host comparison already
/// exist in this crate; a security bound must not become a fifth.
fn same_host(a: &str, b: &str) -> bool {
    matches!(
        (crate::remote::host_of(a), crate::remote::host_of(b)),
        (Some(left), Some(right)) if left == right
    )
}

/// Whether `sender` may invite one of our accounts to `item`.
///
/// Deliberately **narrower** than [`verdict_is_trusted`], and not a reuse of it.
/// A verdict is bounded by something we already did: it only applies to a `Join`
/// we minted and stored, so the worst a mistaken verdict can do is resolve one
/// RSVP we ourselves initiated. An `Invite` has no such precondition — it creates
/// state for an account that has done nothing. Honouring "any actor on the
/// event's origin host" there would let any actor on any host that happens to
/// host an event we ingested put a notification in a named local user's list and
/// open an invite-only event to them.
///
/// So only the two actors that can actually mean "the organizer asked you" count:
/// the event's author, or a group that announced it.
async fn may_invite(state: &AppState, item: &Status, sender: &Account) -> Result<bool, ApiError> {
    if sender.id == item.account_id {
        return Ok(true);
    }
    Ok(group::boosting_group_ids(&state.pool, item.id)
        .await?
        .contains(&sender.id))
}

// ---------------------------------------------------------------------------
// E3: inbound participation on events we host
// ---------------------------------------------------------------------------

/// The event a participation activity targets, when it is one **we host**.
///
/// A `Join` naming a remote event is not ours to answer — we would be inventing
/// an attendance the origin never recorded — so it is dropped. Same for a status
/// that isn't an event at all.
async fn local_event_target(
    state: &AppState,
    object_uri: Option<&str>,
) -> Result<Option<(Status, StatusEvent)>, ApiError> {
    let Some(uri) = object_uri else {
        return Ok(None);
    };
    let Some(item) = crate::ingest::resolve_status_ref(state, uri).await? else {
        return Ok(None);
    };
    let Some(organizer) = account::find_by_id(&state.pool, item.account_id).await? else {
        return Ok(None);
    };
    if !organizer.is_local() {
        return Ok(None);
    }
    let Some(event) = status_event::find(&state.pool, item.id).await? else {
        return Ok(None);
    };
    Ok(Some((item, event)))
}

/// A minimal `Join` reconstructed from a stored participation row, for echoing
/// back inside our `Accept`/`Reject`.
///
/// Rebuilt rather than stored: a receiver keys on the participation url (`id`),
/// the attendee and the event, and those are exactly the three facts the row
/// already holds. A moderator approving a request days later has no raw activity
/// left to quote, so reconstruction is the only shape that works for both the
/// immediate auto-accept and the deferred human one.
fn reconstruct_join(row: &Participation, attendee_uri: &str, event_uri: &str) -> Value {
    let mut join = serde_json::json!({
        "type": "Join",
        "actor": attendee_uri,
        "object": event_uri,
    });
    if let Some(uri) = row.uri.as_deref() {
        join["id"] = Value::String(uri.to_owned());
    }
    if let Some(message) = row.message.as_deref() {
        join["participationMessage"] = Value::String(message.to_owned());
    }
    join
}

/// Sends our verdict on an RSVP to an event we host.
///
/// For a group-attributed event the activity carries the group in `attributedTo`
/// — the Lemmy-shaped mod action — so the receiver can check our
/// affiliation rather than having to trust the bare actor. Local attendees are
/// notified instead of delivered to.
pub async fn send_join_verdict(
    state: &AppState,
    item: &Status,
    row: &Participation,
    accept: bool,
) -> Result<(), ApiError> {
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let local = send_join_verdict_conn(state, &mut conn, item, row, accept).await?;
    drop(conn);
    if let Some(attendee) = local {
        notify_attendee(state, item, attendee, accept).await?;
    }
    Ok(())
}

pub async fn send_join_verdict_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    item: &Status,
    row: &Participation,
    accept: bool,
) -> Result<Option<i64>, ApiError> {
    let Some(attendee) = account::find_by_id(&mut *conn, row.account_id).await? else {
        return Ok(None);
    };
    if attendee.is_local() {
        return Ok(Some(attendee.id));
    }
    let Some(organizer) = account::find_by_id(&mut *conn, item.account_id).await? else {
        return Ok(None);
    };
    let attendee_uri = account_uri(&state.config.domain, &attendee);
    let event_uri = status_uri_for_account(&state.config.domain, item, &organizer);
    // A local group hosting the event vouches for the verdict.
    let group_uri = local_group_uri_of_conn(state, conn, item).await?;
    let verdict = activity::join_verdict(
        &activity::JoinVerdictParams {
            domain: &state.config.domain,
            username: &organizer.username,
            marker: row.id,
            group_uri: group_uri.as_deref(),
            attendee_uri: &attendee_uri,
            join: reconstruct_join(row, &attendee_uri, &event_uri),
        },
        accept,
    );
    job::enqueue(&mut *conn, organizer.id, &attendee.inbox_url, &verdict).await?;
    Ok(None)
}

/// The URI of a **local** group this event belongs to, if any — the
/// `attributedTo` claim on a moderator verdict.
///
/// Connection-scoped delivery for an enclosing mutation transaction.
async fn local_group_uri_of_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    item: &Status,
) -> Result<Option<String>, ApiError> {
    for g in group::groups_of_status(&mut *conn, item.id).await? {
        if let Some(group_account) = account::find_by_id(&mut *conn, g.account_id).await?
            && group_account.is_local()
        {
            return Ok(Some(account_uri(&state.config.domain, &group_account)));
        }
    }
    Ok(None)
}

/// Inbound `Join`: a remote attendee RSVPs to an event **we** host.
///
/// The join mode decides what happens next, and this is the point where §9.4's
/// full matrix and §9.5's capacity enforcement actually bite:
///
/// * `free` — accepted at once, and the `Accept(Join)` goes straight back;
/// * `restricted` — recorded `pending` for a moderator, who answers later;
/// * `invite` — accepted only from someone we invited, refused otherwise;
/// * `external` — we never advertised a local RSVP, so a `Join` is meaningless;
/// * over capacity — **refused with a `Reject`**. Mobilizon stays silent here,
///   but we are the origin: leaving an attendee waiting forever on a full event
///   when we already know the answer would be the unkind version of the same
///   behaviour we tolerate from peers.
pub async fn handle_inbound_join(
    state: &AppState,
    sender: &Account,
    join_uri: Option<&str>,
    object_uri: Option<&str>,
    message: Option<&str>,
) -> Result<(), ApiError> {
    let Some((item, event)) = local_event_target(state, object_uri).await? else {
        return Ok(());
    };
    // A redelivered Join keeps whatever verdict it already got. One lookup answers
    // both that and whether we invited this attendee.
    let existing = participation::find(&state.pool, item.id, sender.id).await?;
    if existing.as_ref().is_some_and(|row| row.state.is_settled()) {
        return Ok(());
    }
    let invited = existing
        .as_ref()
        .is_some_and(|row| row.state == State::Invited);
    let message = clamp_message(message);

    // A refusal we can state now is stated now, rather than parked as a pending
    // row that will never resolve.
    if let Some(refusal) = rsvp_refusal(&event, invited) {
        let row = participation::upsert(
            &state.pool,
            item.id,
            sender.id,
            State::Rejected,
            join_uri,
            message.as_deref(),
        )
        .await?;
        tracing::debug!(
            event = %item.id, actor = %sender.inbox_url, refusal = refusal.as_str(),
            "refusing an inbound Join"
        );
        return send_join_verdict(state, &item, &row, false).await;
    }
    if local_event_is_full(state, &item, &event).await? {
        let row = participation::upsert(
            &state.pool,
            item.id,
            sender.id,
            State::Rejected,
            join_uri,
            message.as_deref(),
        )
        .await?;
        return send_join_verdict(state, &item, &row, false).await;
    }

    let initial = initial_state_for_local_event(&event, invited);
    let row = participation::upsert(
        &state.pool,
        item.id,
        sender.id,
        initial,
        join_uri,
        message.as_deref(),
    )
    .await?;
    notify_organizer(state, &item, sender).await?;
    if row.state == State::Accepted {
        send_join_verdict(state, &item, &row, true).await?;
    }
    Ok(())
}

/// Inbound `Leave` (or, leniently, `Undo(Join)`): the attendee withdraws.
///
/// Resolved by the `Join`'s id when we have one and by (event, actor) otherwise —
/// Mobilizon's `Leave` names the *event*, not the participation. Nothing is
/// notified: a withdrawal is not news the organizer needs pushed at them, and it
/// is visible in the attendee list.
pub async fn handle_inbound_leave(
    state: &AppState,
    sender: &Account,
    join_uri: Option<&str>,
    object_uri: Option<&str>,
) -> Result<(), ApiError> {
    if let Some(uri) = join_uri
        && let Some(row) = participation::find_by_uri(&state.pool, uri).await?
        && row.account_id == sender.id
    {
        participation::delete(&state.pool, row.status_id, sender.id).await?;
        return Ok(());
    }
    let Some((item, _)) = local_event_target(state, object_uri).await? else {
        return Ok(());
    };
    participation::delete(&state.pool, item.id, sender.id).await?;
    Ok(())
}

/// Inbound `Invite`: an organizer invites one of our local accounts to their
/// event (§9.3).
///
/// Only the party that could decide an RSVP may invite — the organizer, the
/// announcing group, or an actor on the event's own origin host — so this reuses
/// [`verdict_is_trusted`] rather than inventing a third rule. An invitation to a
/// *local* event from anyone but us is refused outright: our own events' guest
/// lists are not remotely writable.
pub async fn handle_inbound_invite(
    state: &AppState,
    sender: &Account,
    object_uri: Option<&str>,
    target_uri: Option<&str>,
) -> Result<(), ApiError> {
    let (Some(event_uri), Some(target_uri)) = (object_uri, target_uri) else {
        return Ok(());
    };
    let Some(item) = crate::ingest::resolve_status_ref(state, event_uri).await? else {
        return Ok(());
    };
    if status_event::find(&state.pool, item.id).await?.is_none() {
        return Ok(()); // not an event
    }
    let Some(invitee) =
        crate::local_identity::find_actor(&state.pool, &state.config.domain, target_uri).await?
    else {
        return Ok(());
    };
    // Only local accounts can be invited *into* our database in a way that means
    // anything; a remote-to-remote invitation is none of our business.
    if !invitee.is_local() {
        return Ok(());
    }
    if !may_invite(state, &item, sender).await? {
        tracing::info!(
            event = %item.id, actor = %sender.inbox_url,
            "ignoring an event Invite from an actor with no claim on the event"
        );
        return Ok(());
    }
    record_invitation(state, &item, &invitee, sender.id).await?;
    Ok(())
}

/// A local organizer's (or group moderator's) verdict on a pending RSVP to their
/// own event — the approve/reject buttons behind the attendee list.
pub async fn decide_rsvp(
    state: &AppState,
    actor: &Account,
    status_id: i64,
    attendee_id: i64,
    accept: bool,
) -> Result<Participation, ApiError> {
    let item = status::find_by_id(&state.pool, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !may_moderate_event(state, actor, &item).await? {
        return Err(ApiError::Forbidden(
            "You are not the organizer of this event".into(),
        ));
    }
    let event = status_event::find(&state.pool, item.id)
        .await?
        .ok_or(ApiError::NotFound)?;
    // Approving past capacity would make our own advertised limit a lie.
    if accept && local_event_is_full(state, &item, &event).await? {
        return Err(ApiError::Unprocessable(
            "Validation failed: this event is full".into(),
        ));
    }
    let next = if accept {
        State::Accepted
    } else {
        State::Rejected
    };
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let row = participation::settle(&mut *tx, item.id, attendee_id, next)
        .await?
        .ok_or(ApiError::NotFound)?;
    let local = send_join_verdict_conn(state, &mut tx, &item, &row, accept).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    if let Some(attendee) = local
        && let Err(error) = notify_attendee(state, &item, attendee, accept).await
    {
        tracing::warn!(%error, "post-commit RSVP verdict notification failed");
    }
    Ok(row)
}

/// Whether `actor` may decide RSVPs to `item`: its organizer, or a moderator of
/// a local group the event belongs to (the same elevation the group path uses for every
/// other group moderation action).
pub async fn may_moderate_event(
    state: &AppState,
    actor: &Account,
    item: &Status,
) -> Result<bool, ApiError> {
    if item.account_id == actor.id {
        return Ok(true);
    }
    let group_ids: Vec<i64> = group::groups_of_status(&state.pool, item.id)
        .await?
        .into_iter()
        .map(|g| g.account_id)
        .collect();
    let affiliations = group::affiliations_of(&state.pool, &group_ids, actor.id).await?;
    Ok(group_ids.iter().any(|group_id| {
        matches!(
            affiliations.get(group_id),
            Some(group::Affiliation::Owner | group::Affiliation::Moderator)
        )
    }))
}

/// The inboxes of remote accounts with a live RSVP to this event.
///
/// `accepted` and `pending` only: a refused attendee is not coming, and an
/// invitee who never acted has nothing to re-plan. Deduped, because several
/// attendees on one host share a shared inbox.
pub async fn remote_attendee_inboxes(
    state: &AppState,
    item: &Status,
) -> Result<Vec<String>, ApiError> {
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    remote_attendee_inboxes_conn(state, &mut conn, item).await
}

/// Connection-scoped delivery for an enclosing mutation transaction.
pub async fn remote_attendee_inboxes_conn(
    _state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    item: &Status,
) -> Result<Vec<String>, ApiError> {
    // Only an event has attendees; every other edit would pay for the query and
    // get an empty list.
    if item.object_type.as_deref() != Some("Event") {
        return Ok(Vec::new());
    }
    Ok(participation::live_remote_inboxes(&mut *conn, item.id).await?)
}

/// Whether a **local** event has no room left. Counts accepted attendees from
/// our own sidecar (authoritative here) against the capacity the organizer set.
/// An event with no stated capacity is never full.
pub async fn local_event_is_full(
    state: &AppState,
    item: &Status,
    event: &StatusEvent,
) -> Result<bool, ApiError> {
    let Some(max) = event.max_attendees else {
        return Ok(false);
    };
    let accepted = participation::accepted_count(&state.pool, item.id).await?;
    Ok(accepted >= i64::from(max))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event_with(join_mode: Option<&str>) -> StatusEvent {
        let mut event = StatusEvent::empty(1);
        event.join_mode = join_mode.map(str::to_owned);
        event
    }

    #[test]
    fn external_and_invite_only_events_offer_no_rsvp() {
        assert_eq!(
            rsvp_refusal(&event_with(Some("external")), false),
            Some(RsvpRefusal::External)
        );
        assert_eq!(
            rsvp_refusal(&event_with(Some("invite")), false),
            Some(RsvpRefusal::InviteOnly)
        );
        // An invitation lifts the invite-only refusal, and nothing else.
        assert_eq!(rsvp_refusal(&event_with(Some("invite")), true), None);
        assert_eq!(
            rsvp_refusal(&event_with(Some("external")), true),
            Some(RsvpRefusal::External),
            "an invitation does not make an externally-hosted RSVP possible"
        );
        assert_eq!(rsvp_refusal(&event_with(Some("free")), false), None);
        assert_eq!(rsvp_refusal(&event_with(Some("restricted")), false), None);
        // A dialect that names no join mode is joinable — the origin refuses if
        // it wants to. Guessing `free` is fine here; guessing it in the *entity*
        // would be a lie to the client.
        assert_eq!(rsvp_refusal(&event_with(None), false), None);
    }

    #[test]
    fn a_cancelled_event_outranks_every_other_refusal() {
        let mut event = event_with(Some("free"));
        event.event_status = Some("CANCELLED".into());
        assert_eq!(rsvp_refusal(&event, true), Some(RsvpRefusal::Cancelled));
    }

    #[test]
    fn capacity_is_read_from_whichever_field_the_origin_sent() {
        // `remainingAttendeeCapacity` is authoritative when present.
        let mut event = event_with(Some("free"));
        event.remaining_attendees = Some(0);
        assert_eq!(rsvp_refusal(&event, false), Some(RsvpRefusal::Full));
        event.remaining_attendees = Some(3);
        assert_eq!(rsvp_refusal(&event, false), None);

        // Otherwise the count/capacity pair.
        let mut pair = event_with(Some("free"));
        pair.participant_count = Some(40);
        pair.max_attendees = Some(40);
        assert_eq!(rsvp_refusal(&pair, false), Some(RsvpRefusal::Full));
        pair.participant_count = Some(39);
        assert_eq!(rsvp_refusal(&pair, false), None);

        // A capacity we were never told is not "full" — the common case for
        // every dialect but Mobilizon.
        let unknown = event_with(Some("free"));
        assert_eq!(rsvp_refusal(&unknown, false), None);
    }

    #[test]
    fn only_a_free_local_event_auto_accepts() {
        assert_eq!(
            initial_state_for_local_event(&event_with(Some("free")), false),
            State::Accepted
        );
        assert_eq!(
            initial_state_for_local_event(&event_with(None), false),
            State::Accepted,
            "our composer's default is free"
        );
        for gated in ["restricted", "invite", "external"] {
            assert_eq!(
                initial_state_for_local_event(&event_with(Some(gated)), false),
                State::Pending,
                "{gated} must wait for the organizer"
            );
            // An invitation is the organizer's decision already made; asking them
            // to approve the same person twice is a queue item with no content.
            assert_eq!(
                initial_state_for_local_event(&event_with(Some(gated)), true),
                State::Accepted,
                "{gated} auto-accepts someone we invited"
            );
        }
    }
}
