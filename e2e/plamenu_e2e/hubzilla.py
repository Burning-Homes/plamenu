"""Helpers for Hubzilla's native Image/Audio/Video/Document producer.

Files enter through Hubzilla's public WebDAV surface. Hubzilla separates file
storage from publishing, so ``publish_file`` then invokes the fixture's tiny
CLI adapter around upstream ``attach_store_item()``. The resulting object and
all federation delivery are produced by Hubzilla/PubCrawl itself.
"""

import mimetypes
from pathlib import Path

import requests
import urllib3

from . import config, shell

urllib3.disable_warnings(urllib3.exceptions.InsecureRequestWarning)

HAZEL_USER = "hazel"
HAZEL_EMAIL = "hazel@hubzilla.local"
HAZEL_PASS = "hubzilla-hazel-pass-123"
HAZEL_HANDLE = f"{HAZEL_USER}@{config.HUBZILLA_DOMAIN}"


class HubzillaError(RuntimeError):
    pass


def reachable() -> bool:
    try:
        response = requests.get(
            config.HUBZILLA_URL + "/",
            verify=False,
            timeout=10,
        )
        return response.ok
    except requests.RequestException:
        return False


def actor() -> dict:
    response = requests.get(
        config.HUBZILLA_URL + f"/channel/{HAZEL_USER}",
        headers={"Accept": "application/activity+json"},
        verify=False,
        timeout=20,
    )
    if not response.ok:
        raise HubzillaError(
            f"actor fetch -> {response.status_code}: {response.text[:500]}"
        )
    return response.json()


def upload_file(path: str | Path, *, remote_name: str | None = None) -> str:
    """Upload one file through WebDAV and return its Hubzilla path name."""
    source = Path(path)
    name = remote_name or source.name
    content_type = mimetypes.guess_type(name)[0] or "application/octet-stream"
    with source.open("rb") as stream:
        response = requests.put(
            config.HUBZILLA_URL + f"/dav/{HAZEL_USER}/{name}",
            data=stream,
            auth=(HAZEL_USER, HAZEL_PASS),
            headers={"Content-Type": content_type},
            verify=False,
            timeout=120,
        )
    if response.status_code not in {201, 204}:
        raise HubzillaError(
            f"WebDAV PUT {name!r} -> {response.status_code}: {response.text[:500]}"
        )
    return name


def publish_file(remote_name: str) -> dict:
    """Publish an uploaded file; return ``{type, id}`` from Hubzilla."""
    output = shell.hubzilla_compose(
        "exec",
        "-T",
        "hubzilla",
        "php",
        "/opt/hubzilla-test/publish-file.php",
        HAZEL_USER,
        remote_name,
    ).strip()
    try:
        object_type, object_id = output.rsplit(" ", 1)
    except ValueError as exc:
        raise HubzillaError(f"unexpected publish output: {output!r}") from exc
    return {"type": object_type, "id": object_id}


def upload_and_publish(path: str | Path, *, remote_name: str | None = None) -> dict:
    return publish_file(upload_file(path, remote_name=remote_name))
