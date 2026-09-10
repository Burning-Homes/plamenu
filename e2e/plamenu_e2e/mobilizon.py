"""Helpers for the disposable Mobilizon peer at mobilizon.local.

Mobilizon has **no REST API at all** — login, groups, events, participation
and the relay admin surface all go through the single GraphQL endpoint
`POST /api`. So this is a standalone client like lemmy.py, except every call
is one `{"query": …, "variables": …}` envelope.

Mobilizon is the reference host for federated **events**: `Event` objects
plus the participation verb family (`Join`, `Accept(Join)`, `Reject(Join)`,
`Leave`) that the E-track implements. Two facts shape every test written
against it, both from ../mobilizon-test/README.md:

* **A Person cannot be followed** (`:person_no_follow`) — only `Group` actors
  and the instance's `Application` relay actor. Any event that must reach
  Plamenu is therefore either attributed to a group we follow, or arrives via
  a relay follow. The refused Follow still answers 200; it just never gets an
  Accept.
* **A group event double-sends** (the `Create` *and* the group's `Announce`),
  like Lemmy — consumers dedup.

Actor and group ids are integers that do **not** survive a
`docker compose down -v`, so every helper here queries them rather than
hardcoding.
"""

import requests
import urllib3

from . import config

urllib3.disable_warnings(urllib3.exceptions.InsecureRequestWarning)

GRACE_EMAIL = "grace@mobilizon.local"
GRACE_PASS = "mobilizon-grace-pass-123"
GRACE_USERNAME = "grace"
# Standing followable group seeded by `./dev up mobilizon`. Group usernames are
# `[a-z0-9_]` only — no dashes (README gotcha).
EVENTS_GROUP = "plamenu_events"
TOKEN_FILE = config.MOBILIZON_DIR / ".grace-token"

# `EventJoinOptions`; drives which RSVP affordance we are meant to show.
JOIN_FREE = "FREE"
JOIN_RESTRICTED = "RESTRICTED"
JOIN_INVITE = "INVITE"
JOIN_EXTERNAL = "EXTERNAL"

# `ParticipantRoleEnum`. `NOT_APPROVED` is what a RESTRICTED event's join lands
# on until a moderator approves it — the state whose `Accept` may never come.
ROLE_PARTICIPANT = "PARTICIPANT"
ROLE_NOT_APPROVED = "NOT_APPROVED"
ROLE_REJECTED = "REJECTED"

EVENT_FIELDS = """
  id uuid url title description beginsOn endsOn status draft
  joinOptions externalParticipationUrl category language visibility
  onlineAddress phoneAddress
  options { commentModeration anonymousParticipation maximumAttendeeCapacity }
  physicalAddress { id url description street locality region country postalCode geom }
  organizerActor { id url preferredUsername }
  attributedTo { id url preferredUsername }
  participantStats { going participant notApproved rejected }
"""


class MobilizonError(RuntimeError):
    pass


class MobilizonApi:
    def __init__(self, base_url: str, token: str | None = None):
        self.base_url = base_url.rstrip("/")
        self.http = requests.Session()
        self.http.verify = False
        if token:
            self.http.headers["Authorization"] = f"Bearer {token}"

    # ── transport ─────────────────────────────────────────────────────

    def gql(self, query: str, **variables):
        """One GraphQL round trip. GraphQL answers 200 with an `errors` array,
        so a bare `r.ok` proves nothing — both layers are checked."""
        payload: dict = {"query": query}
        if variables:
            payload["variables"] = variables
        r = self.http.post(self.base_url + "/api", json=payload, timeout=30)
        if not r.ok:
            raise MobilizonError(f"POST /api -> {r.status_code}: {r.text[:500]}")
        body = r.json()
        if body.get("errors"):
            raise MobilizonError(f"GraphQL: {body['errors']}")
        return body["data"]

    # ── account ───────────────────────────────────────────────────────

    def login(self, email: str, password: str) -> str:
        return self.gql(
            """mutation($email: String!, $password: String!) {
                 login(email: $email, password: $password) { accessToken }
               }""",
            email=email,
            password=password,
        )["login"]["accessToken"]

    def instance_config(self) -> dict:
        """Unauthenticated — the reachability probe."""
        return self.gql("{ config { name version registrationsOpen } }")["config"]

    def actors(self) -> list[dict]:
        """The logged user's profiles (Person actors). Ids are integers that
        change across a volume wipe."""
        return self.gql(
            "{ loggedUser { id email actors { id preferredUsername url type } } }"
        )["loggedUser"]["actors"]

    def actor_id(self, preferred_username: str = GRACE_USERNAME) -> str:
        for actor in self.actors():
            if actor["preferredUsername"] == preferred_username:
                return actor["id"]
        raise MobilizonError(f"no profile {preferred_username!r} on this account")

    def ensure_person(self, preferred_username: str, name: str | None = None) -> str:
        """Id of an extra profile on this account, created if absent. A second
        profile is what lets one Mobilizon account both host an event and RSVP
        to it — the organizer is already CREATOR and cannot join its own."""
        for actor in self.actors():
            if actor["preferredUsername"] == preferred_username:
                return actor["id"]
        return self.gql(
            """mutation($username: String!, $name: String) {
                 createPerson(preferredUsername: $username, name: $name) { id url }
               }""",
            username=preferred_username,
            name=name or preferred_username,
        )["createPerson"]["id"]

    # ── groups ────────────────────────────────────────────────────────

    def group(self, preferred_username: str = EVENTS_GROUP) -> dict:
        return self.gql(
            """query($name: String!) {
                 group(preferredUsername: $name) {
                   id preferredUsername url type membersCount followersCount
                 }
               }""",
            name=preferred_username,
        )["group"]

    def group_id(self, preferred_username: str = EVENTS_GROUP) -> str:
        return self.group(preferred_username)["id"]

    def create_group(self, preferred_username: str, name: str | None = None) -> dict:
        """Create a group. `preferred_username` must match `[a-z0-9_]+` —
        a dash is rejected outright."""
        return self.gql(
            """mutation($username: String!, $name: String) {
                 createGroup(preferredUsername: $username, name: $name,
                             visibility: PUBLIC, openness: OPEN) {
                   id preferredUsername url type
                 }
               }""",
            username=preferred_username,
            name=name or preferred_username,
        )["createGroup"]

    def delete_group(self, group_id: str) -> None:
        self.gql("mutation($id: ID!) { deleteGroup(groupId: $id) { id } }", id=group_id)

    # ── events ────────────────────────────────────────────────────────

    def create_event(
        self,
        title: str,
        begins_on: str,
        *,
        description: str = "<p>An e2e event.</p>",
        organizer_actor_id: str | None = None,
        attributed_to_id: str | None = None,
        ends_on: str | None = None,
        join_options: str = JOIN_FREE,
        category: str = "MEETING",
        visibility: str = "PUBLIC",
        status: str = "CONFIRMED",
        external_participation_url: str | None = None,
        online_address: str | None = None,
        physical_address: dict | None = None,
        options: dict | None = None,
        draft: bool = False,
        tags: list[str] | None = None,
    ) -> dict:
        """Create an event and return it with `EVENT_FIELDS` filled in.

        Pass `attributed_to_id` (a group id) for an event that reaches our
        followers — a group-less event is only ever seen through a relay
        follow. `begins_on`/`ends_on` are RFC3339 strings.
        """
        variables = {
            "title": title,
            "description": description,
            "beginsOn": begins_on,
            "organizerActorId": organizer_actor_id or self.actor_id(),
            "joinOptions": join_options,
            "category": category,
            "visibility": visibility,
            "status": status,
            "draft": draft,
        }
        for key, value in (
            ("attributedToId", attributed_to_id),
            ("endsOn", ends_on),
            ("externalParticipationUrl", external_participation_url),
            ("onlineAddress", online_address),
            ("physicalAddress", physical_address),
            ("options", options),
            ("tags", tags),
        ):
            if value is not None:
                variables[key] = value
        return self.gql(
            """mutation($title: String!, $description: String!, $beginsOn: DateTime!,
                        $endsOn: DateTime, $organizerActorId: ID!, $attributedToId: ID,
                        $joinOptions: EventJoinOptions, $category: EventCategory,
                        $visibility: EventVisibility, $status: EventStatus,
                        $externalParticipationUrl: String, $onlineAddress: String,
                        $physicalAddress: AddressInput, $options: EventOptionsInput,
                        $draft: Boolean, $tags: [String]) {
                 createEvent(title: $title, description: $description,
                             beginsOn: $beginsOn, endsOn: $endsOn,
                             organizerActorId: $organizerActorId,
                             attributedToId: $attributedToId,
                             joinOptions: $joinOptions, category: $category,
                             visibility: $visibility, status: $status,
                             externalParticipationUrl: $externalParticipationUrl,
                             onlineAddress: $onlineAddress,
                             physicalAddress: $physicalAddress, options: $options,
                             draft: $draft, tags: $tags) {"""
            + EVENT_FIELDS
            + "} }",
            **variables,
        )["createEvent"]

    def update_event(self, event_id: str, **fields) -> dict:
        """Patch an event; federates as `Update(Event)`. Accepts the same
        camelCase keys `create_event` sends (`title`, `beginsOn`, `status`,
        `joinOptions`, …). Moving `beginsOn` or flipping `status` to
        `CANCELLED` are the two cases the E-track notifies on."""
        decl = {
            "title": "String",
            "description": "String",
            "beginsOn": "DateTime",
            "endsOn": "DateTime",
            "status": "EventStatus",
            "joinOptions": "EventJoinOptions",
            "category": "EventCategory",
            "visibility": "EventVisibility",
            "externalParticipationUrl": "String",
            "onlineAddress": "String",
            "physicalAddress": "AddressInput",
            "options": "EventOptionsInput",
            "draft": "Boolean",
            "tags": "[String]",
        }
        unknown = set(fields) - set(decl)
        if unknown:
            raise MobilizonError(f"update_event: unknown field(s) {sorted(unknown)}")
        args = "".join(f", ${k}: {decl[k]}" for k in fields)
        passthrough = "".join(f", {k}: ${k}" for k in fields)
        return self.gql(
            f"mutation($eventId: ID!{args}) {{ updateEvent(eventId: $eventId"
            f"{passthrough}) {{{EVENT_FIELDS}}} }}",
            eventId=event_id,
            **fields,
        )["updateEvent"]

    def delete_event(self, event_id: str) -> None:
        self.gql("mutation($id: ID!) { deleteEvent(eventId: $id) { id } }", id=event_id)

    def event(self, uuid: str) -> dict:
        """Read an event by uuid — this is the origin-side assertion surface
        (`participantStats.going` moves when a remote Join is accepted)."""
        return self.gql(
            "query($uuid: UUID!) { event(uuid: $uuid) {" + EVENT_FIELDS + "} }",
            uuid=uuid,
        )["event"]

    def interact(self, uri: str) -> dict:
        """Dereference a **remote** event or group by URI and return it as a
        local record (`__typename` plus the local integer `id`).

        This is the only way a Mobilizon-side test reaches an event we host:
        Mobilizon has no "join by URL" — you fetch first, then `join_event`
        with the id this returns. Returns `{}` when the URI resolves to
        nothing Mobilizon models as interactable (its `Interactable` union is
        `Event | Group` only, so a Note or a Person yields nothing)."""
        found = self.gql(
            """query($uri: String!) {
                 interact(uri: $uri) {
                   __typename
                   ... on Event { id uuid url title joinOptions
                                  participantStats { going notApproved } }
                   ... on Group { id url preferredUsername }
                 }
               }""",
            uri=uri,
        )["interact"]
        return found or {}

    # ── participation ─────────────────────────────────────────────────

    def participants(self, uuid: str, *, roles: str | None = None) -> list[dict]:
        """The event's participants (newest page). `roles` is a
        comma-separated `ParticipantRoleEnum` filter, e.g.
        `"participant,not_approved"`; omitted means every role, including the
        CREATOR — so a "did our RSVP land" assertion must look for the actor,
        not the count."""
        variables: dict = {"uuid": uuid}
        role_arg = ""
        if roles is not None:
            variables["roles"] = roles
            role_arg = ", roles: $roles"
        return self.gql(
            f"""query($uuid: UUID!{", $roles: String" if roles else ""}) {{
                  event(uuid: $uuid) {{
                    participants(limit: 100{role_arg}) {{
                      total
                      elements {{ id role metadata {{ message }}
                                  actor {{ id url preferredUsername domain }} }}
                    }}
                  }}
                }}""",
            **variables,
        )["event"]["participants"]["elements"]

    def participant_of(self, uuid: str, actor_url: str) -> dict | None:
        """The participation row of one actor url, or None. Remote attendees
        are matched on their AP actor url, the only stable key across peers."""
        for row in self.participants(uuid):
            if (row.get("actor") or {}).get("url") == actor_url:
                return row
        return None

    def join_event(
        self, event_id: str, *, actor_id: str | None = None, message: str | None = None
    ) -> dict:
        """RSVP as one of our own profiles; federates as `Join`. On a FREE
        event the role comes back `PARTICIPANT` already; on RESTRICTED it is
        `NOT_APPROVED` until `update_participation` approves it."""
        variables: dict = {
            "eventId": event_id,
            "actorId": actor_id or self.actor_id(),
        }
        if message is not None:
            variables["message"] = message
        return self.gql(
            """mutation($eventId: ID!, $actorId: ID!, $message: String) {
                 joinEvent(eventId: $eventId, actorId: $actorId, message: $message) {
                   id role metadata { message } actor { id url preferredUsername }
                 }
               }""",
            **variables,
        )["joinEvent"]

    def leave_event(self, event_id: str, *, actor_id: str | None = None) -> dict:
        """Cancel a participation; federates as a bare `Leave` — Mobilizon
        never emits `Undo(Join)` and its inbound handler has no arm for one."""
        return self.gql(
            """mutation($eventId: ID!, $actorId: ID!) {
                 leaveEvent(eventId: $eventId, actorId: $actorId) {
                   actor { id url } event { id uuid }
                 }
               }""",
            eventId=event_id,
            actorId=actor_id or self.actor_id(),
        )["leaveEvent"]

    def update_participation(self, participant_id: str, role: str) -> dict:
        """Approve (`PARTICIPANT`) or refuse (`REJECTED`) a pending
        participation; federates as `Accept(Join)` / `Reject(Join)`. For a
        group event the emitted activity's `actor` is a group moderator and
        `attributedTo` is the group — the Lemmy-shaped mod action."""
        return self.gql(
            """mutation($id: ID!, $role: ParticipantRoleEnum!) {
                 updateParticipation(id: $id, role: $role) {
                   id role actor { id url preferredUsername }
                 }
               }""",
            id=participant_id,
            role=role,
        )["updateParticipation"]

    # ── relay / instance follows ──────────────────────────────────────
    #
    # The only channel that carries *person-organized* events (§9.2): the
    # instance's `Application` relay actor at /relay.

    def add_instance(self, domain: str) -> dict:
        """Follow a remote instance's relay actor (Mobilizon-side outbound
        follow) — `Admin → Federation → Followings`."""
        return self.gql(
            "mutation($domain: String!) { addInstance(domain: $domain) { domain } }",
            domain=domain,
        )["addInstance"]

    def accept_relay(self, address: str) -> dict:
        """Accept a pending relay follow *of us*, e.g. `plamenu.local` or an
        actor address. This is what makes our relay follow deliver."""
        return self.gql(
            "mutation($address: String!) { acceptRelay(address: $address) { id } }",
            address=address,
        )["acceptRelay"]

    def relay_followers(self) -> list[dict]:
        """Who follows our relay actor — a pending Plamenu relay follow shows
        up here with `approved: false`."""
        return self.gql(
            """{ relayFollowers(limit: 100) {
                   elements { id approved actor { url preferredUsername domain } }
                 } }"""
        )["relayFollowers"]["elements"]

    def relay_followings(self) -> list[dict]:
        return self.gql(
            """{ relayFollowings(limit: 100) {
                   elements { id approved targetActor { url preferredUsername domain } }
                 } }"""
        )["relayFollowings"]["elements"]


def reachable() -> bool:
    try:
        MobilizonApi(config.MOBILIZON_URL).instance_config()
        return True
    except (MobilizonError, requests.RequestException):
        return False


def grace() -> MobilizonApi:
    """Authenticated client for the standing admin, token cached on disk
    (`./dev up mobilizon` refreshes the same file)."""
    if TOKEN_FILE.is_file():
        api = MobilizonApi(config.MOBILIZON_URL, token=TOKEN_FILE.read_text().strip())
        try:
            api.actors()  # 401s / errors out when the token has expired
            return api
        except MobilizonError:
            pass
    token = MobilizonApi(config.MOBILIZON_URL).login(GRACE_EMAIL, GRACE_PASS)
    TOKEN_FILE.write_text(f"{token}\n")
    return MobilizonApi(config.MOBILIZON_URL, token=token)
