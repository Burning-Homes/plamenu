"""Public-API client for Funkwhale's native ActivityStreams Audio producer."""

import json
import mimetypes
import time
import uuid
from pathlib import Path

import requests
import urllib3

from . import config

urllib3.disable_warnings(urllib3.exceptions.InsecureRequestWarning)

FIONA_USER = "fiona"
FIONA_EMAIL = "fiona@funkwhale.local"
FIONA_PASS = "funkwhale-fiona-pass-123"
CHANNEL_USERNAME = "plamenu_audio"
CHANNEL_HANDLE = f"{CHANNEL_USERNAME}@{config.FUNKWHALE_DOMAIN}"


class FunkwhaleError(RuntimeError):
    pass


class FunkwhaleApi:
    def __init__(self, base_url: str = config.FUNKWHALE_URL):
        self.base_url = base_url.rstrip("/")
        self.http = requests.Session()
        self.http.verify = False

    def _request(self, method: str, path: str, **kwargs):
        if method.upper() not in {"GET", "HEAD", "OPTIONS"}:
            csrf = self.http.cookies.get("csrftoken")
            if csrf:
                kwargs.setdefault("headers", {})["X-CSRFToken"] = csrf
            kwargs.setdefault("headers", {})["Referer"] = self.base_url + "/"
        response = self.http.request(
            method, self.base_url + path, timeout=120, **kwargs
        )
        if not response.ok:
            raise FunkwhaleError(
                f"{method} {path} -> {response.status_code}: {response.text[:500]}"
            )
        return response.json() if response.content else None

    def instance(self) -> dict:
        return self._request("GET", "/api/v2/instance/nodeinfo/2.1/")

    def login(self, username: str = FIONA_USER, password: str = FIONA_PASS) -> None:
        # This endpoint supplies Django's CSRF cookie before session login.
        self.instance()
        self._request(
            "POST",
            "/api/v2/users/login",
            data={"username": username, "password": password},
        )

    def channels(self) -> list[dict]:
        body = self._request("GET", "/api/v2/channels/", params={"scope": "me"})
        return body["results"]

    def channel(self, username: str = CHANNEL_USERNAME) -> dict:
        for channel in self.channels():
            actor = channel.get("actor") or {}
            if actor.get("preferred_username") == username:
                return channel
        raise FunkwhaleError(f"owned channel {username!r} was not found")

    def upload_audio(
        self,
        path: str | Path,
        title: str,
        *,
        channel_uuid: str | None = None,
    ) -> dict:
        source = Path(path)
        channel_uuid = channel_uuid or self.channel()["uuid"]
        content_type = mimetypes.guess_type(source.name)[0] or "audio/mpeg"
        reference = f"plamenu-{uuid.uuid4()}"
        with source.open("rb") as stream:
            return self._request(
                "POST",
                "/api/v2/uploads/",
                files={"audio_file": (source.name, stream, content_type)},
                data={
                    "source": f"upload://{source.name}",
                    "import_reference": reference,
                    "channel": channel_uuid,
                    "import_metadata": json.dumps({"title": title}),
                },
            )

    def upload(self, upload_uuid: str, *, import_reference: str) -> dict:
        # Funkwhale's detail endpoint deliberately hides an upload until it is
        # playable, so a newly queued upload returns 404 there. The owner list
        # endpoint exposes pending imports and is what the built-in UI polls.
        body = self._request(
            "GET",
            "/api/v2/uploads/",
            params={
                "import_reference": import_reference,
                "include_channels": "true",
            },
        )
        for upload in body["results"]:
            if upload["uuid"] == upload_uuid:
                return upload
        raise FunkwhaleError(f"owned upload {upload_uuid!r} was not found")

    def wait_for_import(
        self,
        upload_uuid: str,
        *,
        import_reference: str,
        timeout: float = 180,
        interval: float = 2,
    ) -> dict:
        deadline = time.monotonic() + timeout
        last = None
        while time.monotonic() < deadline:
            last = self.upload(upload_uuid, import_reference=import_reference)
            status = last["import_status"]
            if status == "finished":
                return last
            if status in {"errored", "skipped"}:
                raise FunkwhaleError(
                    f"audio import {upload_uuid} ended as {status}: "
                    f"{last.get('import_details')}"
                )
            time.sleep(interval)
        raise FunkwhaleError(
            f"audio import {upload_uuid} did not finish within {timeout}s "
            f"(last={last and last.get('import_status')})"
        )

    def activitypub_audio(self, upload_uuid: str) -> dict:
        """Fetch the canonical top-level Audio object for an upload."""
        return self._request(
            "GET",
            f"/federation/music/uploads/{upload_uuid}",
            headers={"Accept": "application/activity+json"},
        )

    def upload_and_wait(self, path: str | Path, title: str) -> dict:
        created = self.upload_audio(path, title)
        finished = self.wait_for_import(
            created["uuid"], import_reference=created["import_reference"]
        )
        finished["activitypub_id"] = (
            self.base_url + f"/federation/music/uploads/{finished['uuid']}"
        )
        return finished


def reachable() -> bool:
    try:
        FunkwhaleApi().instance()
        return True
    except (FunkwhaleError, requests.RequestException):
        return False


def fiona() -> FunkwhaleApi:
    api = FunkwhaleApi()
    api.login()
    return api
