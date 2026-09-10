"""Federation between Plamenu and a real Mobilizon instance.

Mobilizon is the reference host for federated **events**: `Event` objects plus
the participation verb family (`Join`, `Accept(Join)`, `Reject(Join)`,
`Leave`). Two peer facts shape everything here, both from
`../mobilizon-test/README.md`:

* **A Person cannot be followed** — Mobilizon answers a Follow of a profile
  with `:person_no_follow`, so a group is the only non-relay channel by which
  an event reaches us. Person-organized events arrive only via a relay follow.
* **A group event double-sends** the `Create` and the group's `Announce`, like
  Lemmy; the group path dedups them into one `Event` status plus one boost.

The peer is optional: tests skip when it's down.
"""

import re

import pytest
import requests
from plamenu_e2e import config, unique
from plamenu_e2e import mobilizon as mz
from plamenu_e2e.api import Api
from plamenu_e2e.mobilizon import MobilizonApi
from plamenu_e2e.steps import step, wait_for

CSRF_RE = re.compile(r'name="csrf" value="([^"]+)"')


def _csrf(html: str) -> str:
    match = CSRF_RE.search(html)
    assert match, "no CSRF token on the page"
    return match.group(1)


def _web_login(user) -> requests.Session:
    """A cookie session for Plamenu's first-party web UI (the admin console has
    no client-API equivalent for relays — Mastodon has no relay API either)."""
    session = requests.Session()
    session.verify = False
    resp = session.post(
        f"{config.PLAMENU_URL}/login",
        data={"email": user.email, "password": user.password},
        allow_redirects=False,
    )
    assert resp.status_code == 303, f"web login failed: {resp.status_code}"
    assert session.cookies, "web login set no session cookie"
    return session


# Far enough out that a slow suite never straddles the start time.
BEGINS = "2027-03-14T18:00:00Z"
ENDS = "2027-03-14T20:30:00Z"


def _follow_events_group(plamenu_api: Api, name: str) -> dict:
    """Resolve a Mobilizon group as a Group account and follow it."""
    acct = f"{name}@{config.MOBILIZON_DOMAIN}"
    remote = plamenu_api.resolve_account(acct)
    assert remote, f"Plamenu cannot resolve {acct}"
    assert remote["group"] is True, remote
    plamenu_api.follow(remote["id"])
    wait_for(
        lambda: plamenu_api.relationship(remote["id"])["following"],
        desc="group Accept(Follow) to reach Plamenu",
    )
    return remote


@pytest.mark.federation(
    direction="inbound", reverse_of="test_plamenu_event_reaches_mobilizon"
)
def test_group_event_ingests_with_full_sidecar(
    mobilizon_grace: MobilizonApi, mobilizon_group: dict, plamenu_api: Api, marker: str
):
    """A group event arrives once, as an `Event` status whose sidecar carries
    the whole calendar fact set — not just the four fields shipped."""
    with step("Plamenu follows the events group"):
        group = _follow_events_group(plamenu_api, mobilizon_group["preferredUsername"])

    with step("grace publishes a group event"):
        title = f"Interop meetup {marker}"
        event = mobilizon_grace.create_event(
            title,
            BEGINS,
            description=f"<p>Event body {marker}.</p>",
            attributed_to_id=mobilizon_group["id"],
            ends_on=ENDS,
            physical_address={
                "description": "Community Hall",
                "street": "Trg 1",
                "locality": "Ljubljana",
                "region": "Osrednjeslovenska",
                "country": "Slovenia",
                "postalCode": "1000",
            },
            options={"maximumAttendeeCapacity": 40},
        )

    with step("it lands as exactly one boost of one Event status"):
        boost = wait_for(
            lambda: plamenu_api.home_reblog_containing(marker),
            desc="group-announced event on the Plamenu home timeline",
        )
        assert boost["account"]["group"] is True
        assert boost["account"]["acct"] == group["acct"]
        status = boost["reblog"]
        assert status["title"] == title
        # The double-send must dedup to one boost, as with Lemmy.
        wrappers = [
            s
            for s in plamenu_api.home_timeline(limit=40)
            if s.get("reblog") and s["reblog"]["uri"] == status["uri"]
        ]
        assert len(wrappers) == 1, [w["id"] for w in wrappers]

    with step("the event extension carries the calendar facts"):
        ev = status["event"]
        assert ev, status
        assert ev["start_time"].startswith("2027-03-14T18:00:00")
        assert ev["end_time"].startswith("2027-03-14T20:30:00")
        assert ev["status"] == "CONFIRMED"
        assert ev["timezone"], ev
        assert ev["location"] == "Community Hall"
        assert ev["join_mode"] == "free"
        assert ev["category"] == "MEETING"
        assert ev["is_online"] is False
        assert ev["max_attendees"] == 40
        # Mobilizon's wire `participantCount` counts PARTICIPANTs only — the
        # organizer holds the CREATOR role and is excluded, so a brand-new event
        # federates 0 even though its own `participantStats.going` reads 1. We
        # keep the origin's number as sent rather than "correcting" it.
        assert ev["participants_count"] == 0
        # The structured Place, not just its name.
        assert ev["location_street"] == "Trg 1"
        assert ev["location_locality"] == "Ljubljana"
        assert ev["location_country"] == "Slovenia"
        assert ev["location_postal_code"] == "1000"
        assert ev["location_url"], ev

    with step("moving the event updates the sidecar in place"):
        mobilizon_grace.update_event(event["id"], beginsOn="2027-03-15T19:00:00Z")
        wait_for(
            lambda: plamenu_api.get_status(status["id"])["event"][
                "start_time"
            ].startswith("2027-03-15T19:00:00"),
            desc="Update(Event) to move the start time on Plamenu",
        )

    with step("cancelling it flips ical:status"):
        mobilizon_grace.update_event(event["id"], status="CANCELLED")
        wait_for(
            lambda: (
                plamenu_api.get_status(status["id"])["event"]["status"] == "CANCELLED"
            ),
            desc="Update(Event) cancellation to reach Plamenu",
        )

    mobilizon_grace.delete_event(event["id"])


@pytest.mark.federation(
    direction="outbound", reverse_of="test_mobilizon_joins_an_event_plamenu_hosts"
)
def test_rsvp_reaches_mobilizon_and_resolves(
    mobilizon_grace: MobilizonApi,
    mobilizon_group: dict,
    plamenu_api: Api,
    plamenu_user,
    marker: str,
):
    """RSVP a real group event: the `Join` lands as a participant on the origin
    and the origin's `Accept(Join)` settles our row; `Leave` removes it again."""
    with step("Plamenu follows the events group"):
        _follow_events_group(plamenu_api, mobilizon_group["preferredUsername"])

    with step("grace publishes a free-to-join group event"):
        event = mobilizon_grace.create_event(
            f"RSVP target {marker}",
            BEGINS,
            description=f"<p>Join me {marker}.</p>",
            attributed_to_id=mobilizon_group["id"],
            join_options=mz.JOIN_FREE,
        )
        boost = wait_for(
            lambda: plamenu_api.home_reblog_containing(marker),
            desc="the event on the Plamenu home timeline",
        )
        status_id = boost["reblog"]["id"]
        assert boost["reblog"]["event"]["join_mode"] == "free"
        assert boost["reblog"]["event"]["can_participate"] is True

    actor_url = plamenu_api.ap_get(f"/users/{plamenu_user.username}")["id"]
    with step("the RSVP lands as a participant on Mobilizon"):
        plamenu_api.participate(status_id, message=f"see you there {marker}")
        row = wait_for(
            lambda: mobilizon_grace.participant_of(event["uuid"], actor_url),
            desc="our Join to become a participant on the origin",
        )
        # A `free` event auto-accepts, so the origin lands us straight on
        # PARTICIPANT rather than NOT_APPROVED.
        assert row["role"] == mz.ROLE_PARTICIPANT, row
        assert (row.get("metadata") or {}).get("message") == f"see you there {marker}"

    with step("the origin's Accept(Join) settles our row"):
        wait_for(
            lambda: plamenu_api.participation(status_id) == "accepted",
            desc="Mobilizon's Accept(Join) to reach Plamenu",
        )

    with step("the origin's own count moves"):
        wait_for(
            lambda: (
                mobilizon_grace.event(event["uuid"])["participantStats"]["participant"]
                >= 1
            ),
            desc="participantStats.participant to include us",
        )

    with step("withdrawing the RSVP removes the participant"):
        plamenu_api.unparticipate(status_id)
        wait_for(
            lambda: mobilizon_grace.participant_of(event["uuid"], actor_url) is None,
            desc="our Leave to drop the participant on the origin",
        )
        assert plamenu_api.participation(status_id) is None

    mobilizon_grace.delete_event(event["id"])


@pytest.mark.federation(
    direction="inbound",
    one_way_reason=(
        "a draft is by definition never published; there is no outbound "
        "counterpart to a suppressed ingest."
    ),
)
def test_draft_event_is_not_ingested(
    mobilizon_grace: MobilizonApi, mobilizon_group: dict, plamenu_api: Api, marker: str
):
    """An unpublished (`draft: true`) event must never become a status.

    Mobilizon does not federate drafts, so this guards our *ingest* side: were
    a draft ever to arrive — forwarded, relayed, or from a laxer dialect — it
    must be dropped, not published to our followers.
    """
    with step("Plamenu follows the events group"):
        _follow_events_group(plamenu_api, mobilizon_group["preferredUsername"])

    with step("grace saves a draft event"):
        draft = mobilizon_grace.create_event(
            f"Draft meetup {marker}",
            BEGINS,
            description=f"<p>Draft body {marker}.</p>",
            attributed_to_id=mobilizon_group["id"],
            draft=True,
        )

    with step("a published event behind it arrives, and the draft did not"):
        # The canary: an ordinary event created *after* the draft. Once it is
        # on our timeline the draft's delivery window has demonstrably passed,
        # so the absence below is a real absence and not a slow queue.
        canary = unique("mzcanary")
        published = mobilizon_grace.create_event(
            f"Published meetup {canary}",
            BEGINS,
            description=f"<p>Canary body {canary}.</p>",
            attributed_to_id=mobilizon_group["id"],
        )
        wait_for(
            lambda: plamenu_api.home_reblog_containing(canary),
            desc="the canary event on the Plamenu home timeline",
        )
        assert plamenu_api.home_reblog_containing(marker) is None, (
            "a draft event was ingested and boosted"
        )
        assert plamenu_api.home_status_containing(marker) is None, (
            "a draft event was ingested as a status"
        )

    mobilizon_grace.delete_event(draft["id"])
    mobilizon_grace.delete_event(published["id"])


@pytest.mark.federation(
    direction="outbound", reverse_of="test_group_event_ingests_with_full_sidecar"
)
def test_plamenu_event_reaches_mobilizon(
    mobilizon_grace: MobilizonApi, plamenu_api: Api, plamenu_user, marker: str
):
    """An event Plamenu authors is fetchable by Mobilizon as a real `Event`.

    `interact` is the only way a Mobilizon-side test reaches a remote event —
    there is no "join by URL" — and it is also the honest test of our emission:
    Mobilizon parses the object with its own converter, so anything it cannot read
    simply does not come back as an `Event`.
    """
    with step("Plamenu publishes an event"):
        posted = plamenu_api.post_event(
            f"Plamenu-hosted meetup {marker}",
            "2027-06-10T18:00:00Z",
            end_time="2027-06-10T20:00:00Z",
            # The AP spelling. `mz.JOIN_*` are Mobilizon's GraphQL enum values
            # (uppercase) and belong only in calls to the peer.
            join_mode="free",
            timezone="Europe/Ljubljana",
            max_attendees=25,
            location="Community Hall",
        )
        assert posted["event"]["join_mode"] == "free"
        assert posted["event"]["max_attendees"] == 25
        # A local event counts from our own rows, and nobody has joined yet.
        assert posted["event"]["participants_count"] == 0
        uri = posted["uri"]

    with step("Mobilizon fetches it and reads it as an Event"):
        found = wait_for(
            lambda: mobilizon_grace.interact(uri) or None,
            desc="Mobilizon to dereference our event",
        )
        assert found["__typename"] == "Event", found
        assert marker in found["title"]
        assert found["joinOptions"] == "FREE", found

    mz_event_id = found["id"]
    with step("a Mobilizon profile joins our event and we accept it"):
        heidi = mobilizon_grace.ensure_person("heidi", "Heidi")
        mobilizon_grace.join_event(mz_event_id, actor_id=heidi)
        # `free` auto-accepts on our side, so the RSVP lands accepted and our
        # `Accept(Join)` goes straight back.
        attendees = wait_for(
            lambda: (
                [
                    row
                    for row in plamenu_api.event_participants(posted["id"])
                    if row["account"]["acct"] == f"heidi@{config.MOBILIZON_DOMAIN}"
                ]
                or None
            ),
            desc="the remote Join to appear in our attendee list",
        )
        assert attendees[0]["state"] == "accepted", attendees
        # Our own accepted rows are what a local event counts.
        wait_for(
            lambda: (
                plamenu_api.get_status(posted["id"])["event"]["participants_count"] == 1
            ),
            desc="our attendee count to include the remote RSVP",
        )

    with step("cancelling flips ical:status and is delivered to the attendee"):
        plamenu_api.cancel_event(posted["id"])
        status = plamenu_api.get_status(posted["id"])
        assert status["event"]["status"] == "CANCELLED"
        # A cancelled event can no longer be joined by anyone.
        assert status["event"]["can_participate"] is False
        assert status["event"]["participation_refusal"] == "cancelled"
        # The `Update(Event)` is addressed to the remote **attendee**, who follows
        # nobody here: someone who RSVP'd has organized their day around this post,
        # so a cancellation that only reached our followers would leave every
        # attendee who found the event by URL believing the old time. That
        # delivery is asserted at the unit level (`events_rsvp.rs`), where the
        # queued inbox set is observable.
        #
        # KNOWN PEER LIMITATION (Mobilizon 5.2.4): it answers our `Update(Event)`
        # with **HTTP 500**, not a refusal — its `handle_incoming` returns a bare
        # `:error` for anything its transmogrifier declines and the controller has
        # no clause for that, so the cancellation is not applied on the origin's
        # copy. Verified from its own debug log: our activity's signature and actor
        # both validate, and the `Update`/`Event` arm bails before its origin check
        # (which never logs). We keep sending the Update — it is correct, and the
        # 500 is retried like any transient failure — but the origin-side assertion
        # is deliberately not made here. See plamenu/E_TRACK_DESIGN.md §11.

    with step("deleting it removes it from the origin's view"):
        plamenu_api.delete_status(posted["id"])
        assert plamenu_api.get_status_or_none(posted["id"]) is None


@pytest.mark.federation(
    direction="inbound", reverse_of="test_rsvp_reaches_mobilizon_and_resolves"
)
def test_mobilizon_joins_an_event_plamenu_hosts(
    mobilizon_grace: MobilizonApi, plamenu_api: Api, plamenu_user, marker: str
):
    """The inbound RSVP path on a `restricted` event we host: the remote Join
    waits for us, and our approval federates back as `Accept(Join)`."""
    with step("Plamenu publishes an approval-gated event"):
        posted = plamenu_api.post_event(
            f"Approval-gated meetup {marker}",
            "2027-06-11T18:00:00Z",
            join_mode="restricted",
        )
        assert posted["event"]["join_mode"] == "restricted"

    with step("Mobilizon fetches it"):
        found = wait_for(
            lambda: mobilizon_grace.interact(posted["uri"]) or None,
            desc="Mobilizon to dereference our event",
        )
        assert found["__typename"] == "Event"

    with step("a remote Join waits for our approval"):
        heidi = mobilizon_grace.ensure_person("heidi", "Heidi")
        mobilizon_grace.join_event(found["id"], actor_id=heidi)
        pending = wait_for(
            lambda: next(
                (
                    row
                    for row in plamenu_api.event_participants(posted["id"])
                    if row["account"]["acct"] == f"heidi@{config.MOBILIZON_DOMAIN}"
                ),
                None,
            ),
            desc="the remote Join to reach our approval queue",
        )
        # A restricted event waits for a human — guessing a verdict for the
        # organizer would be worse than silence.
        assert pending["state"] == "pending", pending

    with step("approving it federates Accept(Join)"):
        plamenu_api.approve_participant(posted["id"], pending["account"]["id"])
        assert (
            next(
                row
                for row in plamenu_api.event_participants(posted["id"])
                if row["account"]["acct"] == f"heidi@{config.MOBILIZON_DOMAIN}"
            )["state"]
            == "accepted"
        )

    plamenu_api.delete_status(posted["id"])


@pytest.mark.federation(
    direction="inbound",
    one_way_reason=(
        "a relay follow is a subscription to someone else's firehose; the "
        "reverse (Mobilizon following our relay) is Mobilizon's own admin "
        "action against a Plamenu relay actor, not a Plamenu behaviour."
    ),
)
def test_instance_relay_follow_carries_person_organized_events(
    mobilizon_grace: MobilizonApi, plamenu_admin, marker: str
):
    """Following a Mobilizon instance's relay actor delivers the events a group
    follow cannot reach (§9.2).

    This is the *only* channel for a **person-organized** Mobilizon event:
    Mobilizon refuses to be followed as a Person (`:person_no_follow`), so an
    event with no `attributedTo` group reaches nobody through an ordinary follow.
    The admin form takes a bare instance address for exactly this reason —
    requiring an operator to know that Mobilizon's relay actor lives at `/relay`,
    and that its inbox is a different URL again, turns a one-line decision into a
    research task.
    """
    admin_user, admin_api = plamenu_admin
    session = _web_login(admin_user)

    with step("the admin subscribes to the instance by bare address"):
        # The console page is at /admin/relays; only the mutations live under /web.
        page = session.get(f"{config.PLAMENU_URL}/admin/relays")
        assert page.status_code == 200, page.status_code
        resp = session.post(
            f"{config.PLAMENU_URL}/web/admin/relays",
            data={"csrf": _csrf(page.text), "inbox_url": config.MOBILIZON_DOMAIN},
            allow_redirects=False,
        )
        assert resp.status_code == 303, resp.text[:300]
        # A relay subscription is instance-wide state, not per-test: a previous
        # run may already have added this one, which answers `duplicate`. Either
        # outcome means the address resolved and the row exists.
        flash = resp.headers.get("location", "")
        assert "flash=applied" in flash or "flash=duplicate" in flash, resp.headers
        # Resolved to the relay actor's own inbox, not the address we typed.
        listing = session.get(f"{config.PLAMENU_URL}/admin/relays").text
        assert f"{config.MOBILIZON_URL}/inbox" in listing, listing[:500]

    with step("Mobilizon sees the pending follow and accepts it"):
        follower = wait_for(
            lambda: next(
                (
                    row
                    for row in mobilizon_grace.relay_followers()
                    if (row.get("actor") or {}).get("domain") == config.PLAMENU_DOMAIN
                ),
                None,
            ),
            desc="our relay Follow to reach Mobilizon",
        )
        if not follower["approved"]:
            mobilizon_grace.accept_relay(
                mobilizon_grace.relay_follower_address(follower)
            )

    with step("a person-organized event now reaches us"):
        # No `attributed_to_id`: organized by grace herself, so no group announces
        # it and no ordinary follow could ever deliver it.
        event = mobilizon_grace.create_event(
            f"Person-organized meetup {marker}",
            BEGINS,
            description=f"<p>Just me {marker}.</p>",
        )
        found = wait_for(
            lambda: (
                admin_api.search(marker, resolve=False, type="statuses")["statuses"]
                or None
            ),
            desc="the person-organized event to arrive via the relay",
        )
        status = found[0]
        assert status["object_type"] == "Event", status
        assert status["event"]["start_time"].startswith("2027-03-14T18:00:00")
        assert status["account"]["acct"] == f"grace@{config.MOBILIZON_DOMAIN}"

    mobilizon_grace.delete_event(event["id"])
