"""Minimal Mastodon-API client.

Plamenu speaks the Mastodon client API, so the same client works against
both instances; only token acquisition differs (see mastodon.py/plamenu.py).
"""

import requests
import urllib3

from . import apsign, config

# The dev stack terminates TLS at Caddy with a local CA; certificate trust
# is not what these tests exercise.
urllib3.disable_warnings(urllib3.exceptions.InsecureRequestWarning)


class ApiError(RuntimeError):
    pass


class Api:
    def __init__(self, base_url: str, token: str | None = None):
        self.base_url = base_url.rstrip("/")
        self.http = requests.Session()
        self.http.verify = False
        if token:
            self.http.headers["Authorization"] = f"Bearer {token}"

    def _request(
        self,
        method: str,
        path: str,
        *,
        params=None,
        data=None,
        json=None,
        files=None,
        none_on=(),
    ):
        r = self.http.request(
            method,
            self.base_url + path,
            params=params,
            data=data,
            json=json,
            files=files,
            timeout=30,
        )
        if r.status_code in none_on:
            return None
        if not r.ok:
            raise ApiError(f"{method} {path} -> {r.status_code}: {r.text[:500]}")
        # Some endpoints (e.g. DELETE /api/v1/collections/{id}) answer 200 with
        # an empty body — there is nothing to decode.
        if not r.content:
            return None
        return r.json()

    def get(self, path: str, **params):
        return self._request("GET", path, params=params)

    def ap_get(self, path: str):
        """Fetch an ActivityPub document the way a federated peer would.

        Fetches of Plamenu are HTTP-signed (as alice@mastodon.local) so they
        satisfy authorized fetch — the product default — exactly as a real peer
        does; other targets are fetched anonymously. To assert the *unsigned*
        rejection path deliberately, issue the GET directly (see
        `test_visibility.ap_get_status_code`)."""
        url = self.base_url + path
        if any(
            self.base_url.startswith(f"https://{domain}")
            for domain in (config.PLAMENU_DOMAIN, config.PLAMENU2_DOMAIN)
        ):
            r = apsign.signed_ap_get(url)
        else:
            r = self.http.get(
                url, headers={"Accept": "application/activity+json"}, timeout=30
            )
        if not r.ok:
            raise ApiError(f"AP GET {path} -> {r.status_code}: {r.text[:500]}")
        return r.json()

    def post(self, path: str, **data):
        return self._request("POST", path, data=data)

    def patch(self, path: str, files=None, **data):
        return self._request("PATCH", path, data=data, files=files)

    def put(self, path: str, **data):
        return self._request("PUT", path, data=data)

    def delete(self, path: str, **data):
        return self._request("DELETE", path, data=data)

    # ── domain helpers ────────────────────────────────────────────────

    def search(self, q: str, *, resolve: bool = False, type: str | None = None) -> dict:
        params = {"q": q, "resolve": str(resolve).lower()}
        if type:
            params["type"] = type
        return self.get("/api/v2/search", **params)

    def resolve_account(self, acct: str) -> dict | None:
        """Find `user@domain` with resolve=true (webfinger + actor fetch)."""
        accounts = self.search(f"@{acct}", resolve=True, type="accounts")["accounts"]
        return accounts[0] if accounts else None

    def account(self, account_id: str) -> dict | None:
        """GET /accounts/{id}; None when the account is hidden/absent (404)
        or tombstoned (410) — e.g. behind a domain suspension."""
        return self._request(
            "GET", f"/api/v1/accounts/{account_id}", none_on=(404, 410)
        )

    def lookup(self, acct: str) -> dict | None:
        """/accounts/lookup — local knowledge only, never webfingers."""
        return self._request(
            "GET", "/api/v1/accounts/lookup", params={"acct": acct}, none_on=(404,)
        )

    def relationship(self, account_id: str) -> dict:
        return self.get("/api/v1/accounts/relationships", **{"id[]": account_id})[0]

    def follow(self, account_id: str) -> dict:
        return self.post(f"/api/v1/accounts/{account_id}/follow")

    def unfollow(self, account_id: str) -> dict:
        return self.post(f"/api/v1/accounts/{account_id}/unfollow")

    def block(self, account_id: str) -> dict:
        return self.post(f"/api/v1/accounts/{account_id}/block")

    def unblock(self, account_id: str) -> dict:
        return self.post(f"/api/v1/accounts/{account_id}/unblock")

    def blocks(self) -> list:
        return self.get("/api/v1/blocks")

    def report(
        self,
        account_id: str,
        *,
        comment: str | None = None,
        category: str | None = None,
        forward: bool | None = None,
        status_ids: list[str] | None = None,
    ) -> dict:
        """File a report (POST /api/v1/reports); `forward` asks the origin
        server to be notified for a remote target."""
        data: dict = {"account_id": account_id}
        if comment is not None:
            data["comment"] = comment
        if category is not None:
            data["category"] = category
        if forward is not None:
            data["forward"] = str(forward).lower()
        if status_ids:
            data["status_ids[]"] = status_ids
        return self.post("/api/v1/reports", **data)

    def account_statuses(self, account_id: str, **params) -> list:
        return self.get(f"/api/v1/accounts/{account_id}/statuses", **params)

    def followers(self, account_id: str, **params) -> list:
        return self.get(f"/api/v1/accounts/{account_id}/followers", **params)

    def following(self, account_id: str, **params) -> list:
        return self.get(f"/api/v1/accounts/{account_id}/following", **params)

    def post_status(self, text: str, **params) -> dict:
        return self.post("/api/v1/statuses", status=text, **params)

    def post_poll(
        self,
        text: str,
        options: list[str],
        *,
        expires_in: int = 3600,
        multiple: bool = False,
    ) -> dict:
        """Post a status with a poll (Rails-style nested form keys)."""
        return self.post_status(
            text,
            **{
                "poll[options][]": options,
                "poll[expires_in]": str(expires_in),
                "poll[multiple]": str(multiple).lower(),
            },
        )

    def get_poll(self, poll_id: str) -> dict:
        return self.get(f"/api/v1/polls/{poll_id}")

    def vote(self, poll_id: str, choices: list[int]) -> dict:
        return self.post(
            f"/api/v1/polls/{poll_id}/votes", **{"choices[]": [str(c) for c in choices]}
        )

    def get_status(self, status_id: str) -> dict:
        return self.get(f"/api/v1/statuses/{status_id}")

    def get_status_or_none(self, status_id: str) -> dict | None:
        """GET /statuses/{id}; None when deleted or not visible (404/410)."""
        return self._request("GET", f"/api/v1/statuses/{status_id}", none_on=(404, 410))

    def delete_status(self, status_id: str) -> dict:
        return self.delete(f"/api/v1/statuses/{status_id}")

    def favourite(self, status_id: str) -> dict:
        return self.post(f"/api/v1/statuses/{status_id}/favourite")

    def unfavourite(self, status_id: str) -> dict:
        return self.post(f"/api/v1/statuses/{status_id}/unfavourite")

    def downvote(self, status_id: str) -> dict:
        """Plamenu extension: downvote a group post. The upvote verb
        is `favourite`, as everywhere in the ecosystem."""
        return self.post(f"/api/v1/statuses/{status_id}/downvote")

    def undownvote(self, status_id: str) -> dict:
        return self.post(f"/api/v1/statuses/{status_id}/undownvote")

    def post_event(
        self,
        text: str,
        start_time: str,
        *,
        title: str | None = None,
        end_time: str | None = None,
        join_mode: str = "free",
        timezone: str | None = None,
        max_attendees: int | None = None,
        status: str = "CONFIRMED",
        location: str | None = None,
        is_online: bool = False,
        external_participation_url: str | None = None,
        **params,
    ) -> dict:
        """Plamenu extension (E4): publish an event (Rails-style `event[...]`
        keys, like the poll parameters).

        The event kind is explicit here for the same reason it is in the
        composer: an `Event` object is truncated to a title-plus-link stub by
        Mastodon and its forks, so it is never inferred from a post that merely
        contains a date."""
        fields: dict = {
            # Required: an event with no name is unreadable in every list view,
            # and Mobilizon's own model refuses one.
            "title": title or text,
            "event[start_time]": start_time,
            "event[join_mode]": join_mode,
            "event[status]": status,
            "event[is_online]": str(is_online).lower(),
        }
        for key, value in (
            ("event[end_time]", end_time),
            ("event[timezone]", timezone),
            ("event[location]", location),
            ("event[external_participation_url]", external_participation_url),
            ("event[max_attendees]", max_attendees),
        ):
            if value is not None:
                fields[key] = str(value)
        return self.post_status(text, **fields, **params)

    def cancel_event(self, status_id: str) -> dict:
        """Call off a local event: flips `ical:status` to CANCELLED and
        federates the `Update(Event)`. Every attendee is notified."""
        return self.put(
            f"/api/v1/statuses/{status_id}", **{"event[status]": "CANCELLED"}
        )

    def event_participants(self, status_id: str) -> list:
        """The organizer's attendee list — each entry an account plus its RSVP
        state. Organizer-only: a guest list is not public information."""
        return self.get(f"/api/v1/statuses/{status_id}/participants")

    def approve_participant(self, status_id: str, account_id: str) -> dict:
        return self.post(
            f"/api/v1/statuses/{status_id}/participants/{account_id}/approve"
        )

    def reject_participant(self, status_id: str, account_id: str) -> dict:
        return self.post(
            f"/api/v1/statuses/{status_id}/participants/{account_id}/reject"
        )

    def participate(self, status_id: str, message: str | None = None) -> dict:
        """Plamenu extension (E2): RSVP to an event. Returns the status, whose
        `event.participation` is the resulting state — `pending` until the
        organizer's `Accept(Join)` arrives, which for a `free` event is prompt
        and for a `restricted` one may never come at all."""
        payload = {"message": message} if message is not None else {}
        return self.post(f"/api/v1/statuses/{status_id}/participate", **payload)

    def unparticipate(self, status_id: str) -> dict:
        """Withdraw an RSVP (federates as a bare `Leave`, not `Undo(Join)`)."""
        return self.post(f"/api/v1/statuses/{status_id}/unparticipate")

    def participation(self, status_id: str) -> str | None:
        """The viewer's own RSVP state on an event status, or None."""
        event = self.get_status(status_id).get("event") or {}
        return event.get("participation")

    def reblog(self, status_id: str) -> dict:
        return self.post(f"/api/v1/statuses/{status_id}/reblog")

    def unreblog(self, status_id: str) -> dict:
        return self.post(f"/api/v1/statuses/{status_id}/unreblog")

    def favourited_by(self, status_id: str) -> list:
        return self.get(f"/api/v1/statuses/{status_id}/favourited_by")

    def reblogged_by(self, status_id: str) -> list:
        return self.get(f"/api/v1/statuses/{status_id}/reblogged_by")

    def context(self, status_id: str) -> dict:
        return self.get(f"/api/v1/statuses/{status_id}/context")

    def notifications(self, **params) -> list:
        return self.get("/api/v1/notifications", **params)

    def notifications_from(self, acct: str, kind: str) -> list:
        """Notifications of `kind` whose actor is `acct` (bare username for
        same-instance actors, user@domain for remote ones)."""
        return [
            n
            for n in self.notifications()
            if n["type"] == kind and n["account"]["acct"] == acct
        ]

    def public_timeline(self, *, local: bool | None = None, limit: int = 40) -> list:
        params: dict = {"limit": limit}
        if local is not None:
            params["local"] = str(local).lower()
        return self.get("/api/v1/timelines/public", **params)

    def resolve_status(self, uri: str) -> dict | None:
        """Search-by-URL with resolve=true; the ingested status or None."""
        found = self.search(uri, resolve=True, type="statuses")["statuses"]
        return found[0] if found else None

    def emoji_reactions(self, status_id: str) -> list:
        """Pleroma-style reactions of a status (`pleroma.emoji_reactions`)."""
        return (self.get_status(status_id).get("pleroma") or {}).get(
            "emoji_reactions", []
        )

    def react(self, status_id: str, emoji: str) -> dict:
        return self.put(f"/api/v1/pleroma/statuses/{status_id}/reactions/{emoji}")

    def unreact(self, status_id: str, emoji: str) -> dict:
        return self.delete(f"/api/v1/pleroma/statuses/{status_id}/reactions/{emoji}")

    def quote_state(self, status_id: str) -> str:
        """FEP-044f quote state of a status ('' when it has no quote)."""
        return (self.get_status(status_id).get("quote") or {}).get("state", "")

    def home_timeline(self, limit: int = 20) -> list:
        return self.get("/api/v1/timelines/home", limit=limit)

    def conversations(self, limit: int = 20) -> list:
        return self.get("/api/v1/conversations", limit=limit)

    def conversation_containing(self, text: str) -> dict | None:
        """The conversation whose last status contains `text`, if any."""
        return next(
            (
                c
                for c in self.conversations()
                if text in ((c.get("last_status") or {}).get("content") or "")
            ),
            None,
        )

    def read_conversation(self, conversation_id: str) -> dict:
        return self.post(f"/api/v1/conversations/{conversation_id}/read")

    def home_status_containing(self, text: str) -> dict | None:
        return next((s for s in self.home_timeline() if text in s["content"]), None)

    def home_reblog_containing(self, text: str) -> dict | None:
        """The home-timeline boost wrapper whose boosted status contains
        `text` (reblog entries carry the content on the nested status)."""
        return next(
            (
                s
                for s in self.home_timeline()
                if s.get("reblog") and text in s["reblog"]["content"]
            ),
            None,
        )

    def update_profile(self, files=None, **data) -> dict:
        return self.patch("/api/v1/accounts/update_credentials", files=files, **data)

    def edit_status(self, status_id: str, text: str) -> dict:
        return self.put(f"/api/v1/statuses/{status_id}", status=text)

    def upload_media(
        self,
        content: bytes,
        *,
        filename: str = "upload.bin",
        mime: str = "application/octet-stream",
        description: str | None = None,
        focus: str | None = None,
        v2: bool = False,
    ) -> dict:
        """Upload a media file (v1 is synchronous; v2 may answer 202)."""
        data = {}
        if description is not None:
            data["description"] = description
        if focus is not None:
            data["focus"] = focus
        path = "/api/v2/media" if v2 else "/api/v1/media"
        return self._request(
            "POST", path, data=data, files={"file": (filename, content, mime)}
        )

    def get_media(self, media_id: str) -> dict:
        """Poll a (possibly still-processing) upload; 206 also parses."""
        return self.get(f"/api/v1/media/{media_id}")

    def post_with_media(self, text: str, media_ids: list[str], **params) -> dict:
        return self.post_status(text, **{"media_ids[]": media_ids}, **params)

    def create_collection(self, name: str, **params) -> dict:
        """POST /api/v1/collections (FEP-7aa9). Returns the `{'collection': …}`
        envelope. Mastodon requires a `sensitive` param (ERR_INCLUSION without
        it); pass `discoverable='true'` to make the collection AP-fetchable."""
        return self.post("/api/v1/collections", name=name, **params)
