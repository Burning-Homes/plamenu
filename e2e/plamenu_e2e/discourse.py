"""Browser-API helpers for Discourse with the official ActivityPub plugin."""

import requests
import urllib3

from . import config

urllib3.disable_warnings(urllib3.exceptions.InsecureRequestWarning)

DIANA_EMAIL = "diana@discourse.local"
DIANA_PASS = "discourse-diana-pass-123"
FEDERATION_CATEGORY_ID = 5
FEDERATION_CATEGORY_SLUG = "federation"


class DiscourseError(RuntimeError):
    pass


class DiscourseApi:
    def __init__(self, base_url: str):
        self.base_url = base_url.rstrip("/")
        self.http = requests.Session()
        self.http.verify = False

    def _request(self, method: str, path: str, *, data=None):
        headers = {}
        if method != "GET":
            headers = {
                "X-CSRF-Token": self.csrf(),
                "X-Requested-With": "XMLHttpRequest",
            }
        response = self.http.request(
            method,
            self.base_url + path,
            data=data,
            headers=headers,
            timeout=30,
        )
        if not response.ok:
            raise DiscourseError(
                f"{method} {path} -> {response.status_code}: {response.text[:500]}"
            )
        if not response.content:
            return None
        return response.json()

    def csrf(self) -> str:
        response = self.http.get(self.base_url + "/session/csrf.json", timeout=30)
        if not response.ok:
            raise DiscourseError(
                f"GET /session/csrf.json -> {response.status_code}: "
                f"{response.text[:500]}"
            )
        return response.json()["csrf"]

    def login(self, email: str, password: str) -> dict:
        body = self._request(
            "POST", "/session", data={"login": email, "password": password}
        )
        if body.get("error"):
            raise DiscourseError(f"login failed: {body['error']}")
        return body["user"]

    def about(self) -> dict:
        return self._request("GET", "/about.json")

    def category_actor_id(self) -> int:
        actors = self._request("GET", "/site.json")["activity_pub_actors"]["category"]
        return next(
            actor["id"]
            for actor in actors
            if actor["model_id"] == FEDERATION_CATEGORY_ID
        )

    def category_followers(self, actor_id: int) -> list[dict]:
        followers = []
        path = f"/ap/local/actor/{actor_id}/followers.json"
        while True:
            page = self._request("GET", path)
            followers.extend(page["actors"])
            if len(followers) >= page["meta"]["total"]:
                return followers
            if not page["actors"]:
                raise DiscourseError(
                    "category follower pagination ended before its total"
                )
            path = page["meta"]["load_more_url"]

    def create_topic(
        self,
        title: str,
        raw: str,
        *,
        category_id: int = FEDERATION_CATEGORY_ID,
    ) -> dict:
        return self._request(
            "POST",
            "/posts.json",
            data={
                "title": title,
                "raw": raw,
                "category": str(category_id),
                "archetype": "regular",
            },
        )

    def topic(self, topic_id: int) -> dict:
        return self._request("GET", f"/t/{topic_id}.json")

    def delete_topic(self, topic_id: int) -> None:
        self._request("DELETE", f"/t/{topic_id}.json")

    def post_containing(self, topic_id: int, marker: str) -> dict | None:
        return next(
            (
                post
                for post in self.topic(topic_id)["post_stream"]["posts"]
                if marker in post["cooked"]
            ),
            None,
        )

    @staticmethod
    def category_url() -> str:
        return (
            f"{config.DISCOURSE_URL}/c/{FEDERATION_CATEGORY_SLUG}/"
            f"{FEDERATION_CATEGORY_ID}"
        )

    def topic_url(self, topic_id: int, post_number: int = 1) -> str:
        topic = self.topic(topic_id)
        return f"{self.base_url}/t/{topic['slug']}/{topic_id}/{post_number}"


def reachable() -> bool:
    try:
        body = DiscourseApi(config.DISCOURSE_URL).about()
        return bool(body.get("about", {}).get("version"))
    except (DiscourseError, requests.RequestException):
        return False


def diana() -> DiscourseApi:
    api = DiscourseApi(config.DISCOURSE_URL)
    api.login(DIANA_EMAIL, DIANA_PASS)
    return api
