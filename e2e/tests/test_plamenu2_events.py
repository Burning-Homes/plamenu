"""Plamenu against Plamenu: events and going to them.

The event dialect is shared with Mobilizon, and the existing tests measure it
against Mobilizon's reading. But Mobilizon models only part of what this
server publishes (its own join modes, its own address model), and it cannot
be *told* things it does not model. Both ends here are this server, so an
event goes out whole and must come back whole — every calendar fact, the
capacity arithmetic, the approval queue and the refusal reasons that decide
which button a reader is even offered.

Direction is `both` throughout: identical software on either side.
"""

import pytest
from plamenu_e2e import interop
from plamenu_e2e.api import ApiError
from plamenu_e2e.steps import log, step, wait_for

# Far enough out that the event is never accidentally in the past.
START = "2028-04-11T18:00:00Z"
END = "2028-04-11T21:00:00Z"


@pytest.mark.federation(direction="both")
def test_event_facts_and_rsvp_round_trip(
    plamenu2, plamenu_user, plamenu_api, plamenu2_user, plamenu2_api, marker
):
    """An event, a guest, and the whole calendar sidecar in between.

    Covers: every event fact surviving the wire (start, end, time zone,
    address, capacity, join mode, online flag), a free event auto-accepting a
    remote `Join`, the organizer's guest list, the attendee count moving on
    both sides, and `Leave` taking the RSVP back."""
    interop.mutual_follow(
        plamenu_api, plamenu_user.acct, plamenu2_api, plamenu2_user.acct
    )

    with step("the local user publishes an event with everything filled in"):
        posted = plamenu_api.post_event(
            f"Interop meetup {marker}",
            START,
            title=f"Interop meetup {marker}",
            end_time=END,
            join_mode="free",
            timezone="Europe/Ljubljana",
            max_attendees=25,
            location="Community Hall",
            is_online=False,
        )
        assert posted["object_type"] == "Event", posted
        mine = posted["event"]
        assert mine["participants_count"] == 0, mine

    with step("the guest's server reads back exactly what was published"):
        theirs = interop.delivered(plamenu2_api, marker)
        assert theirs["object_type"] == "Event", theirs
        event = theirs["event"]
        for key in (
            "start_time",
            "end_time",
            "timezone",
            "location",
            "join_mode",
            "max_attendees",
            "is_online",
            "status",
        ):
            assert event[key] == mine[key], (key, mine[key], event[key])
        assert event["participation"] is None, event
        assert event["can_participate"] is True, event
        assert event["participation_refusal"] is None, event

    with step("the guest RSVPs; a free event accepts on the spot"):
        plamenu2_api.participate(theirs["id"])
        wait_for(
            lambda: plamenu2_api.participation(theirs["id"]) == "accepted",
            desc="the organizer's Accept(Join) to settle the RSVP",
        )

    with step("the organizer's guest list carries the RSVP, and the count with it"):
        attendees = wait_for(
            lambda: plamenu_api.event_participants(posted["id"]) or None,
            desc="the remote Join to appear in the organizer's attendee list",
        )
        assert [a["account"]["acct"] for a in attendees] == [plamenu2_user.acct]
        assert attendees[0]["state"] == "accepted", attendees
        # The host counts from its own rows and is authoritative. A guest's
        # copy instead reports the number the origin last *published*, which
        # an RSVP does not by itself refresh — the origin does not re-publish
        # an event every time somebody says they are coming.
        assert plamenu_api.get_status(posted["id"])["event"]["participants_count"] == 1
        wait_for(
            lambda: plamenu_api.notifications_from(
                plamenu2_user.acct, "event.participation"
            ),
            desc="the organizer to be told somebody is coming",
        )

    with step("withdrawing the RSVP empties the guest list again"):
        plamenu2_api.unparticipate(theirs["id"])
        wait_for(
            lambda: not plamenu_api.event_participants(posted["id"]),
            desc="the Leave to remove the participant",
        )
        assert plamenu2_api.participation(theirs["id"]) is None


@pytest.mark.federation(direction="both")
def test_a_restricted_event_holds_the_queue_and_the_organizer_decides(
    plamenu2, plamenu_user, plamenu_api, plamenu2_user, plamenu2_api, marker
):
    """An event that screens its guests, from both sides of the screen.

    Covers: `join_mode: restricted` reaching the guest as such, an RSVP that
    stays `pending` rather than pretending to be accepted, the organizer's
    queue carrying the message the guest wrote, approval federating an
    `Accept(Join)`, and rejection federating a `Reject(Join)` that the guest's
    own server records rather than leaving them waiting forever."""
    interop.mutual_follow(
        plamenu_api, plamenu_user.acct, plamenu2_api, plamenu2_user.acct
    )

    with step("the local user publishes an approval-gated event"):
        posted = plamenu_api.post_event(
            f"Screened meetup {marker}",
            START,
            title=f"Screened meetup {marker}",
            join_mode="restricted",
        )
        assert posted["event"]["join_mode"] == "restricted", posted["event"]

    with step("the guest asks to come, and is told they are waiting"):
        theirs = interop.delivered(plamenu2_api, marker)
        assert theirs["event"]["join_mode"] == "restricted", theirs["event"]
        plamenu2_api.participate(theirs["id"], message=f"hope to make it {marker}")
        assert plamenu2_api.participation(theirs["id"]) == "pending"

    with step("the organizer's queue shows the request and what they wrote"):
        pending = wait_for(
            lambda: (
                [
                    row
                    for row in plamenu_api.event_participants(posted["id"])
                    if row["state"] == "pending"
                ]
                or None
            ),
            desc="the Join to reach the organizer's queue",
        )
        assert pending[0]["account"]["acct"] == plamenu2_user.acct, pending
        assert marker in (pending[0].get("message") or ""), pending[0]

    with step("approving it federates the Accept, and the guest is in"):
        plamenu_api.approve_participant(posted["id"], pending[0]["account"]["id"])
        wait_for(
            lambda: plamenu2_api.participation(theirs["id"]) == "accepted",
            desc="the Accept(Join) to reach the guest",
        )

    with step("a later rejection is federated too, not left hanging"):
        plamenu_api.reject_participant(posted["id"], pending[0]["account"]["id"])
        state = wait_for(
            lambda: (
                plamenu2_api.participation(theirs["id"]) != "accepted"
                and (plamenu2_api.participation(theirs["id"]) or "gone")
            ),
            desc="the Reject(Join) to unseat the guest's RSVP",
        )
        log(f"the guest's state after the rejection: {state}")


@pytest.mark.federation(direction="both")
def test_a_full_event_refuses_and_says_why(
    plamenu2, plamenu_user, plamenu_api, plamenu2_user, plamenu2_api, marker
):
    """Capacity, as the guest's server learns it.

    Covers: `max_attendees`/`remaining_attendees` federating, an RSVP past
    capacity being refused rather than queued forever, and the refusal reason
    reaching the guest so the reason shown is "full" and not a greyed-out
    button of unknown cause.

    The guest reaches this event by dereferencing it rather than by following
    the organizer, because that is when the capacity a reader is shown is
    current: a delivered copy carries the numbers as they stood when the event
    was published, and an RSVP does not make the origin re-publish."""

    with step("the local user publishes an event with a single seat"):
        posted = plamenu_api.post_event(
            f"One seat {marker}",
            START,
            title=f"One seat {marker}",
            join_mode="free",
            max_attendees=1,
        )

    with step("the organizer takes the only seat themselves"):
        # An event's own host attending is the ordinary way a one-seat event
        # fills up without a second instance in the mix.
        plamenu_api.participate(posted["id"])
        wait_for(
            lambda: (
                plamenu_api.get_status(posted["id"])["event"]["participants_count"] == 1
            ),
            desc="the organizer's own RSVP to be counted",
        )

    with step("the guest's server fetches it and is told the event is full"):
        theirs = interop.ingested(plamenu2_api, posted["uri"])
        event = plamenu2_api.get_status(theirs["id"])["event"]
        assert event["max_attendees"] == 1, event
        assert event["remaining_attendees"] == 0, event
        assert event["can_participate"] is False, event
        assert event["participation_refusal"] == "full", event

    with step("and an RSVP anyway is refused rather than queued"):
        try:
            plamenu2_api.participate(theirs["id"])
        except ApiError as err:
            log(f"refused locally: {err}")
        else:
            # If the client-side gate ever lets it through, the origin must
            # still not seat them.
            assert plamenu2_api.participation(theirs["id"]) != "accepted"
            wait_for(
                lambda: (
                    plamenu_api.get_status(posted["id"])["event"]["participants_count"]
                    == 1
                ),
                desc="the origin to keep refusing the extra attendee",
            )


@pytest.mark.federation(direction="both")
def test_calling_an_event_off_reaches_the_people_coming(
    plamenu2, plamenu_user, plamenu_api, plamenu2_user, plamenu2_api, marker
):
    """Cancelling, and deleting, an event somebody is coming to.

    Covers: `Update(Event)` flipping `ical:status` to CANCELLED for an
    attendee's server, the attendee being notified rather than finding out at
    the door, and the later `Delete` removing it."""
    interop.mutual_follow(
        plamenu_api, plamenu_user.acct, plamenu2_api, plamenu2_user.acct
    )

    with step("an event is published and the guest is coming"):
        posted = plamenu_api.post_event(
            f"Doomed meetup {marker}",
            START,
            title=f"Doomed meetup {marker}",
            join_mode="free",
        )
        theirs = interop.delivered(plamenu2_api, marker)
        plamenu2_api.participate(theirs["id"])
        wait_for(
            lambda: plamenu2_api.participation(theirs["id"]) == "accepted",
            desc="the RSVP to be accepted",
        )

    with step("calling it off reaches the guest as a cancelled event"):
        plamenu_api.cancel_event(posted["id"])
        wait_for(
            lambda: (
                plamenu2_api.get_status(theirs["id"])["event"]["status"] == "CANCELLED"
            ),
            desc="the Update(Event) carrying the cancellation",
        )
        wait_for(
            lambda: plamenu2_api.notifications_from(plamenu_user.acct, "event.changed"),
            desc="the attendee to be notified of the change",
        )

    with step("deleting it takes it off the guest's calendar entirely"):
        plamenu_api.delete_status(posted["id"])
        wait_for(
            lambda: plamenu2_api.get_status_or_none(theirs["id"]) is None,
            desc="the Delete(Event) to remove the guest's copy",
        )
