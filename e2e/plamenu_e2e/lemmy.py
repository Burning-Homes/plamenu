"""Helpers for the disposable Lemmy peer at lemmy.local.

Lemmy speaks its own HTTP API (v3 on 0.19.x), not Mastodon's, so this is
a standalone client like sharkey.py — every endpoint takes and returns
JSON. Communities are the FEP-1b12 Group actors Plamenu consumes
and the reference consumer for the groups Plamenu will host.
"""

import requests
import urllib3

from . import config

urllib3.disable_warnings(urllib3.exceptions.InsecureRequestWarning)

FRANK_NICK = "frank"
FRANK_PASS = "lemmy-frank-pass-123"
TOKEN_FILE = config.LEMMY_DIR / ".frank-jwt"


class LemmyError(RuntimeError):
    pass


class LemmyApi:
    def __init__(self, base_url: str, jwt: str | None = None):
        self.base_url = base_url.rstrip("/")
        self.http = requests.Session()
        self.http.verify = False
        if jwt:
            self.http.headers["Authorization"] = f"Bearer {jwt}"

    def _request(self, method: str, path: str, *, params=None, json=None):
        r = self.http.request(
            method, self.base_url + path, params=params, json=json, timeout=30
        )
        if not r.ok:
            raise LemmyError(f"{method} {path} -> {r.status_code}: {r.text[:500]}")
        return r.json()

    def get(self, path: str, **params):
        return self._request("GET", path, params=params)

    def post(self, path: str, **json):
        return self._request("POST", path, json=json)

    def put(self, path: str, **json):
        return self._request("PUT", path, json=json)

    # ── domain helpers ────────────────────────────────────────────────

    def login(self, user: str, password: str) -> str:
        return self.post(
            "/api/v3/user/login", username_or_email=user, password=password
        )["jwt"]

    def site(self) -> dict:
        return self.get("/api/v3/site")

    def create_community(self, name: str, title: str | None = None) -> dict:
        """Create a community; returns the inner `community` object."""
        return self.post("/api/v3/community", name=name, title=title or name)[
            "community_view"
        ]["community"]

    def delete_community(self, community_id: int) -> None:
        self.post("/api/v3/community/delete", community_id=community_id, deleted=True)

    def create_post(
        self,
        community_id: int,
        title: str,
        *,
        body: str | None = None,
        url: str | None = None,
    ) -> dict:
        """Create a post (federates as Page); returns the inner `post`."""
        payload: dict = {"community_id": community_id, "name": title}
        if body is not None:
            payload["body"] = body
        if url is not None:
            payload["url"] = url
        return self.post("/api/v3/post", **payload)["post_view"]["post"]

    def edit_post(
        self, post_id: int, *, title: str | None = None, body: str | None = None
    ) -> dict:
        payload: dict = {"post_id": post_id}
        if title is not None:
            payload["name"] = title
        if body is not None:
            payload["body"] = body
        return self.put("/api/v3/post", **payload)["post_view"]["post"]

    def delete_post(self, post_id: int) -> None:
        self.post("/api/v3/post/delete", post_id=post_id, deleted=True)

    def lock_post(self, post_id: int, locked: bool = True) -> dict:
        """Mod-lock (or unlock) a thread; federates as Lock / Undo(Lock).
        Returns the inner `post` (with `locked`)."""
        return self.post("/api/v3/post/lock", post_id=post_id, locked=locked)[
            "post_view"
        ]["post"]

    def remove_post(
        self, post_id: int, removed: bool = True, reason: str | None = None
    ) -> dict:
        """Mod-remove (or restore) a post; federates as Delete(summary) /
        Undo(Delete). Returns the inner `post` (with `removed`)."""
        payload: dict = {"post_id": post_id, "removed": removed}
        if reason is not None:
            payload["reason"] = reason
        return self.post("/api/v3/post/remove", **payload)["post_view"]["post"]

    def edit_community(
        self,
        community_id: int,
        *,
        title: str | None = None,
        description: str | None = None,
    ) -> dict:
        """Rename / re-describe a community; federates as Announce(Update(Group)).
        Returns the inner `community`."""
        payload: dict = {"community_id": community_id}
        if title is not None:
            payload["title"] = title
        if description is not None:
            payload["description"] = description
        return self.put("/api/v3/community", **payload)["community_view"]["community"]

    def post_view(self, post_id: int) -> dict:
        """Full post_view (post + counts) — `counts.score` is the vote sum."""
        return self.get("/api/v3/post", id=post_id)["post_view"]

    def posts_of(self, community_id: int) -> list[dict]:
        """The community's post_views (post + counts), newest first."""
        return self.get(
            "/api/v3/post/list", community_id=community_id, sort="New", type_="All"
        )["posts"]

    def create_comment(
        self, post_id: int, content: str, *, parent_id: int | None = None
    ) -> dict:
        payload: dict = {"post_id": post_id, "content": content}
        if parent_id is not None:
            payload["parent_id"] = parent_id
        return self.post("/api/v3/comment", **payload)["comment_view"]["comment"]

    def comments_of(self, post_id: int) -> list:
        """All comment_views of a post (any nesting), newest Lemmy default."""
        return self.get("/api/v3/comment/list", post_id=post_id, max_depth=8, limit=50)[
            "comments"
        ]

    def vote_post(self, post_id: int, score: int) -> dict:
        """score: 1 upvote, -1 downvote, 0 retract; returns post_view."""
        return self.post("/api/v3/post/like", post_id=post_id, score=score)["post_view"]

    def resolve(self, q: str) -> dict:
        """Dereference a remote object/actor by URL or handle
        (`!group@host`, `@user@host`) — Lemmy's resolve_object endpoint."""
        return self.get("/api/v3/resolve_object", q=q)

    def follow_community(self, community_id: int, follow: bool = True) -> dict:
        """(Un)subscribe; returns the community_view with `subscribed`."""
        return self.post(
            "/api/v3/community/follow", community_id=community_id, follow=follow
        )["community_view"]

    def community_view(self, community_id: int) -> dict:
        """Full community_view — `subscribed` is
        `Subscribed`/`Pending`/`NotSubscribed`."""
        return self.get("/api/v3/community", id=community_id)["community_view"]


def reachable() -> bool:
    try:
        LemmyApi(config.LEMMY_URL).site()
        return True
    except (LemmyError, requests.RequestException):
        return False


def frank() -> LemmyApi:
    """Authenticated client for the standing admin, JWT cached on disk."""
    if TOKEN_FILE.is_file():
        api = LemmyApi(config.LEMMY_URL, jwt=TOKEN_FILE.read_text().strip())
        try:
            if api.site().get("my_user"):  # absent when the JWT is stale
                return api
        except LemmyError:
            pass
    jwt = LemmyApi(config.LEMMY_URL).login(FRANK_NICK, FRANK_PASS)
    TOKEN_FILE.write_text(f"{jwt}\n")
    return LemmyApi(config.LEMMY_URL, jwt=jwt)
