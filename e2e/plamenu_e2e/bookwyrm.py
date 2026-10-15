"""Drive real BookWyrm web forms; inspect its database only for assertions.

BookWyrm has no general client API. Login, discovery, follows and publication
use the same HTTP endpoints and CSRF protection as its browser UI. No mocked
federation or upstream source patches are involved.
"""

import json
from urllib.parse import urlparse

import requests

from . import config, shell


def reachable():
    try:
        return requests.get(
            config.BOOKWYRM_URL + "/.well-known/nodeinfo",
            verify=str(config.MASTO_DIR / "ca-bundle.crt"),
            timeout=5,
        ).ok
    except requests.RequestException:
        return False


class BookWyrm:
    def __init__(self):
        self.base = config.BOOKWYRM_URL
        self.http = requests.Session()
        self.http.verify = str(config.MASTO_DIR / "ca-bundle.crt")
        self.request("GET", "/login")
        self.request(
            "POST",
            "/login",
            data={"localname": "lena", "password": "bookwyrm-lena-pass-123"},
        )
        if not self.http.cookies.get("sessionid"):
            raise RuntimeError("BookWyrm fixture login did not establish a session")
        self.info = self.inspect(
            "{'user_id': models.User.objects.get(localname='lena').id, "
            "'book_id': models.Edition.objects.get(title='Plamenu federation fixture book').id}"
        )
        self.actor = f"{self.base}/user/lena"
        self.acct = f"lena@{config.BOOKWYRM_DOMAIN}"

    def request(self, method, path, **kwargs):
        headers = kwargs.pop("headers", {})
        if method not in {"GET", "HEAD"}:
            headers.update(
                {
                    "X-CSRFToken": self.http.cookies.get("csrftoken", ""),
                    "Origin": self.base,
                    "Referer": self.base + "/",
                }
            )
        response = self.http.request(
            method, self.base + path, headers=headers, timeout=45, **kwargs
        )
        if not response.ok:
            raise RuntimeError(f"BookWyrm {method} {path}: HTTP {response.status_code}")
        return response

    def inspect(self, expression):
        """Read-only model queries, never used to create a federation event."""
        script = (
            "import json; from bookwyrm import models; "
            f"print('PLAMENU_BOOKWYRM='+json.dumps({expression}, default=str))"
        )
        output = shell.fixture_compose(
            config.BOOKWYRM_DIR,
            "exec",
            "-T",
            "bookwyrm",
            "python",
            "manage.py",
            "shell",
            "-c",
            script,
        )
        for line in output.splitlines():
            if line.startswith("PLAMENU_BOOKWYRM="):
                return json.loads(line.removeprefix("PLAMENU_BOOKWYRM="))
        raise RuntimeError("BookWyrm model query returned no result")

    def resolve(self, handle):
        handle = handle.removeprefix("@")
        self.request("GET", "/search", params={"q": handle, "type": "user"})
        return self.inspect(
            f"models.User.objects.filter(username__iexact={handle!r})"
            ".values('id','username','remote_id','local','bookwyrm_user').first()"
        )

    def follow(self, handle):
        return self.request(
            "POST",
            "/follow",
            data={"user": handle},
            headers={"Accept": "application/json"},
        )

    def unfollow(self, handle):
        return self.request(
            "POST",
            "/unfollow",
            data={"user": handle},
            headers={"Accept": "application/json"},
        )

    def following(self, uri):
        return self.inspect(
            "models.UserFollows.objects.filter(user_subject__localname='lena',"
            f"user_object__remote_id={uri!r}).exists()"
        )

    def followed_by(self, uri):
        return self.inspect(
            "models.UserFollows.objects.filter(user_object__localname='lena',"
            f"user_subject__remote_id={uri!r}).exists()"
        )

    def status(self, uri):
        return self.inspect(
            f"models.Status.objects.filter(remote_id={uri!r})"
            ".values('id','remote_id','content','privacy','deleted',"
            "'reply_parent__remote_id','user__remote_id').first()"
        )

    def post(
        self, kind, content, *, reply_to=None, privacy="public", existing=None, **fields
    ):
        payload = {
            "user": self.info["user_id"],
            "content": content,
            "privacy": privacy,
            **fields,
        }
        if kind in {"review", "comment", "quotation", "rating"}:
            payload.setdefault("book", self.info["book_id"])
        previous_id = (
            0
            if existing
            else self.inspect(
                "models.Status.objects.order_by('-id').values_list('id', flat=True).first() or 0"
            )
        )
        if reply_to is not None:
            parent = self.status(reply_to)
            if parent is None:
                raise RuntimeError(
                    "BookWyrm cannot reply to a status it has not received"
                )
            payload["reply_parent"] = parent["id"]
        self.request(
            "POST",
            f"/post/{kind}" + (f"/{existing['id']}" if existing else ""),
            data=payload,
            headers={"Accept": "application/json"},
        )
        lookup = f"id={existing['id']}" if existing else f"id__gt={previous_id}"
        post = self.inspect(
            "models.Status.objects.filter(user__localname='lena',"
            f"{lookup}).order_by('-id')"
            ".values('id','remote_id','content','privacy','reply_parent__remote_id').first()"
        )
        if post is None:
            raise RuntimeError("BookWyrm form succeeded without persisting the post")
        return post

    def delete(self, post):
        self.request("POST", f"/delete-status/{post['id']}")

    def ap_document(self, uri, *, native=False):
        if urlparse(uri).netloc != urlparse(self.base).netloc:
            raise ValueError(
                "This helper only fetches fixture-owned ActivityPub documents"
            )
        headers = {"Accept": "application/activity+json"}
        if native:
            headers["User-Agent"] = f"BookWyrm (BookWyrm/0.9.3; +{self.base})"
        return self.request("GET", uri.removeprefix(self.base), headers=headers).json()
