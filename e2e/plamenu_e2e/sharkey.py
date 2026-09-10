"""Sharkey-side helpers: the disposable Misskey-family peer at sharkey.local.

The peer is provided by the external live-test harness and federates over its
local Caddy CA. See e2e/README.md for the public harness status. Unlike
Pleroma, Sharkey speaks the Misskey
client API (bare JSON POSTs to /api/<endpoint>, token in the `i` body field),
not the Mastodon API, so it gets its own tiny client instead of reusing Api.

The harness creates the standing admin account `carol` as the instance's first
(root) account and exposes its native API token to this helper.
"""

import time

import requests
import urllib3

from . import config, shell

urllib3.disable_warnings(urllib3.exceptions.InsecureRequestWarning)

# Standing Sharkey account, created by `./dev up sharkey` (mirrors Pleroma's
# `bob`). Must match SHARKEY_DEV_USER/_PASSWORD in the top-level dev script.
CAROL_NICK = "carol"
CAROL_PASS = "shrkpass123"


class SharkeyError(RuntimeError):
    pass


class Sharkey:
    """Minimal Misskey-API client (every endpoint is a JSON POST)."""

    def __init__(self, base_url: str, token: str | None = None):
        self.base_url = base_url.rstrip("/")
        self.token = token
        self.http = requests.Session()
        self.http.verify = False

    def call(self, endpoint: str, **params):
        body = dict(params)
        if self.token:
            body["i"] = self.token
        for _ in range(10):
            r = self.http.post(f"{self.base_url}/api/{endpoint}", json=body, timeout=30)
            # Sharkey rate-limits per-endpoint with sub-second resets; tests
            # poll, so waiting the reset out beats surfacing the 429.
            if r.status_code != 429:
                break
            time.sleep(1)
        if r.status_code == 204 or not r.content:
            return None
        if not r.ok:
            raise SharkeyError(f"{endpoint} -> {r.status_code}: {r.text[:500]}")
        return r.json()

    def ap_get(self, path: str):
        """Fetch an ActivityPub document (server-to-server GET, no auth)."""
        r = self.http.get(
            self.base_url + path,
            headers={"Accept": "application/activity+json"},
            timeout=30,
        )
        if not r.ok:
            raise SharkeyError(f"AP GET {path} -> {r.status_code}: {r.text[:500]}")
        return r.json()

    # ── domain helpers ────────────────────────────────────────────────

    def me(self) -> dict:
        return self.call("i")

    def post_note(self, text: str, *, visibility: str = "public", **params) -> dict:
        """Create a note; visibility is public/home/followers/specified."""
        return self.call("notes/create", text=text, visibility=visibility, **params)[
            "createdNote"
        ]

    def show_note(self, note_id: str) -> dict:
        return self.call("notes/show", noteId=note_id)

    def delete_note(self, note_id: str) -> None:
        self.call("notes/delete", noteId=note_id)

    def edit_note(self, note_id: str, text: str, **params) -> dict:
        """Edit a note. This build edits via `notes/edit` (arg `editId`, not
        `noteId`); the edit sets `updatedAt` and the AP object's `updated`."""
        return self.call("notes/edit", editId=note_id, text=text, **params)[
            "createdNote"
        ]

    def renote(self, note_id: str, **params) -> dict:
        return self.call("notes/create", renoteId=note_id, **params)["createdNote"]

    def unrenote(self, note_id: str) -> None:
        """Undo a renote; the arg is the ORIGINAL note's id (not the renote's)."""
        self.call("notes/unrenote", noteId=note_id)

    def renotes(self, note_id: str, limit: int = 20) -> list:
        """The renote notes of a note (each entry carries `user`). The reliable
        signal an inbound Announce landed — `renoteCount` does not increment
        for a renote by the note's own author."""
        return self.call("notes/renotes", noteId=note_id, limit=limit)

    def reply(self, note_id: str, text: str, **params) -> dict:
        return self.call("notes/create", replyId=note_id, text=text, **params)[
            "createdNote"
        ]

    def children(self, note_id: str, limit: int = 20) -> list:
        """Direct replies threaded under a note (`notes/children`)."""
        return self.call("notes/children", noteId=note_id, limit=limit)

    def poll_note(
        self,
        text: str,
        choices: list[str],
        *,
        multiple: bool = False,
        expires_ms: int = 3_600_000,
        **params,
    ) -> dict:
        """Post a note carrying a poll (Misskey nested `poll` object)."""
        return self.call(
            "notes/create",
            text=text,
            poll={"choices": choices, "multiple": multiple, "expiredAfter": expires_ms},
            **params,
        )["createdNote"]

    def vote(self, note_id: str, choice: int) -> None:
        """Cast ONE integer choice on a poll note (204/None on success)."""
        self.call("notes/polls/vote", noteId=note_id, choice=choice)

    def poll_of(self, note_id: str) -> dict | None:
        """A note's poll: {multiple, expiresAt, choices:[{text, votes, isVoted}]}."""
        return self.call("notes/show", noteId=note_id).get("poll")

    def update_profile(self, **params) -> dict:
        """Edit the account profile (`i/update`: name, description, avatarId,
        isBot, isLocked, isCat …)."""
        return self.call("i/update", **params)

    def react(self, note_id: str, reaction: str) -> None:
        """React to a note; `reaction` is a unicode emoji or `:shortcode:`."""
        self.call("notes/reactions/create", noteId=note_id, reaction=reaction)

    def unreact(self, note_id: str) -> None:
        self.call("notes/reactions/delete", noteId=note_id)

    def note_reactions(self, note_id: str) -> dict:
        """A note's reaction tally: `{emoji: count}`; a remote custom emoji
        keys as `:shortcode@host:`."""
        return self.call("notes/show", noteId=note_id).get("reactions", {})

    def upload(self, content: bytes, filename: str = "upload.png") -> dict:
        """Upload a file to the drive (multipart; token rides in the form)."""
        r = self.http.post(
            f"{self.base_url}/api/drive/files/create",
            data={"i": self.token, "name": filename},
            files={"file": (filename, content)},
            timeout=30,
        )
        if not r.ok:
            raise SharkeyError(f"drive/files/create -> {r.status_code}: {r.text[:500]}")
        return r.json()

    def ensure_custom_emoji(self, shortcode: str, png: bytes) -> None:
        """Create a local custom emoji (admin drive upload + emoji/add);
        idempotent — an existing shortcode is tolerated."""
        listed = self.call("emojis")["emojis"]
        if any(e["name"] == shortcode for e in listed):
            return
        file = self.upload(png, f"{shortcode}.png")
        try:
            self.call("admin/emoji/add", name=shortcode, fileId=file["id"])
        except SharkeyError as e:
            if "DUPLICATE_NAME" not in str(e):
                raise

    def show_user(self, username: str, host: str | None = None) -> dict:
        """Look up a user known to this instance (host=None for local)."""
        return self.call("users/show", username=username, host=host)

    def resolve(self, uri: str) -> dict:
        """ap/show — webfinger/fetch a remote object or actor by URI/URL.
        Answers {"type": "User"|"Note", "object": …}."""
        return self.call("ap/show", uri=uri)

    def follow(self, user_id: str) -> dict:
        return self.call("following/create", userId=user_id)

    def unfollow(self, user_id: str) -> dict:
        return self.call("following/delete", userId=user_id)

    def notifications(self, **params) -> list:
        return self.call("i/notifications", **params)

    def user_notes(self, user_id: str, **params) -> list:
        return self.call("users/notes", userId=user_id, **params)

    def timeline_note_containing(self, text: str) -> dict | None:
        """The (home) timeline note whose text contains `text`, if any."""
        return next(
            (
                n
                for n in self.call("notes/timeline", limit=40)
                if text in (n.get("text") or "")
            ),
            None,
        )


def reachable() -> bool:
    """Whether the Sharkey test instance answers (tests skip when it doesn't)."""
    try:
        Sharkey(config.SHARKEY_URL).call("meta", detail=False)
        return True
    except (SharkeyError, requests.RequestException):
        return False


def _carol_token() -> str:
    """Carol's native API token: the file `./dev up sharkey` records, falling
    back to reading it straight out of the instance's `user` table."""
    token_file = config.SHARKEY_DIR / ".admin-token"
    if token_file.is_file() and (token := token_file.read_text().strip()):
        return token
    token = shell.sharkey_compose(
        "exec",
        "-T",
        "db",
        "psql",
        "-U",
        "sharkey",
        "-d",
        "sharkey",
        "-tA",
        "-c",
        f'select token from "user" where "usernameLower" = \'{CAROL_NICK}\'',
    ).strip()
    if not token:
        raise SharkeyError(
            f"no token for {CAROL_NICK} — run `./dev up sharkey` to create her"
        )
    token_file.write_text(token + "\n")
    return token


def carol() -> Sharkey:
    """Authenticated client for the standing `carol` (root/admin) account."""
    return Sharkey(config.SHARKEY_URL, token=_carol_token())
