"""Owncast go-live federation, discovery, HLS playback and lifecycle."""

import time

import pytest
import requests
import urllib3
from plamenu_e2e import config, owncast, plamenu
from plamenu_e2e.api import Api
from plamenu_e2e.steps import log, step, wait_for

urllib3.disable_warnings()

pytestmark = pytest.mark.skipif(
    not owncast.reachable(), reason="owncast.local is not up"
)


def _get(url: str, **kwargs) -> requests.Response:
    return requests.get(url, verify=False, timeout=30, **kwargs)


def _absolute(path_or_url: str) -> str:
    return (
        path_or_url
        if path_or_url.startswith("http")
        else config.PLAMENU_URL + path_or_url
    )


def _first_resource(playlist: str) -> str | None:
    return next(
        (
            line.strip()
            for line in playlist.splitlines()
            if line.strip() and not line.lstrip().startswith("#")
        ),
        None,
    )


@pytest.mark.federation(
    direction="inbound",
    peer="owncast",
    one_way_reason="Owncast originates the Service actor, public live status, HLS endpoint and go-live Note; Plamenu has no broadcaster/RTMP publishing model to emit the reciprocal Owncast protocol.",
)
def test_owncast_stream_is_discoverable_federated_playable_and_ends(
    cli: plamenu.Cli,
    plamenu_user: plamenu.User,
    plamenu_api: Api,
):
    with step("the Owncast actor resolves and follow notifications are enabled"):
        account = wait_for(
            lambda: plamenu_api.resolve_account(owncast.ACTOR),
            desc="Owncast Service actor resolution",
        )
        plamenu_api.post(f"/api/v1/accounts/{account['id']}/follow", notify="true")
        wait_for(
            lambda: plamenu_api.relationship(account["id"])["following"],
            desc="Owncast Follow/Accept",
        )
        prior_announcement_ids = {
            item["id"]
            for item in plamenu_api.home_timeline(limit=40)
            if item["account"]["id"] == account["id"]
            and "Plamenu Owncast is live" in item["content"]
        }

    with owncast.broadcast():
        with step("an unfollowed account finds the already-running stream by homepage"):
            viewer = plamenu.User()
            cli.account_add(viewer, display_name="Owncast discovery viewer")
            viewer_api = plamenu.login(viewer)
            found = wait_for(
                lambda: viewer_api.search(
                    config.OWNCAST_URL, resolve=True, type="accounts"
                )["accounts"],
                desc="Owncast root URL account discovery",
            )[0]
            assert found["id"] == account["id"], found
            assert viewer_api.relationship(found["id"])["following"] is False
            profile = _get(f"{config.PLAMENU_URL}/@{owncast.ACTOR}")
            assert profile.status_code == 200
            assert "<video" in profile.text and "data-hls=" in profile.text, (
                "an already-running unfollowed Owncast stream has an account-level player"
            )

            with step(
                "the delayed go-live Note arrives once as a playable notification"
            ):
                status = wait_for(
                    lambda: next(
                        (
                            item
                            for item in plamenu_api.home_timeline(limit=40)
                            if item["id"] not in prior_announcement_ids
                            and item["account"]["id"] == account["id"]
                            and "Plamenu Owncast is live" in item["content"]
                        ),
                        None,
                    ),
                    desc="Owncast's two-minute go-live Note",
                    timeout=190,
                    interval=2,
                )
            matching = [
                item
                for item in plamenu_api.home_timeline(limit=40)
                if item["uri"] == status["uri"]
            ]
            assert len(matching) == 1, f"one feed row for the Note: {matching}"
            media = status["media_attachments"]
            assert len(media) == 1 and media[0]["type"] == "video", media
            attachment = media[0]
            assert attachment["live"] == {"state": "live", "permanent": True}
            assert attachment.get("hls", {}).get("master"), attachment
            assert "/media/live/" in attachment["url"], attachment
            notification = wait_for(
                lambda: next(
                    (
                        item
                        for item in plamenu_api.notifications_from(
                            owncast.ACTOR, "status"
                        )
                        if (item.get("status") or {}).get("id") == status["id"]
                    ),
                    None,
                ),
                desc="Owncast go-live status notification",
            )
            assert (
                notification["status"]["media_attachments"][0]["live"]["state"]
                == "live"
            )

        with step("Plamenu's HLS proxy serves a rewritten playlist and segment"):
            master = _get(attachment["hls"]["master"])
            assert master.status_code == 200, master.text[:500]
            assert "owncast.local" not in master.text, master.text[:500]
            resource = _first_resource(master.text)
            assert resource, master.text
            if "#EXT-X-STREAM-INF" in master.text:
                child = _get(_absolute(resource))
                assert child.status_code == 200, child.text[:500]
                segment = _first_resource(child.text)
            else:
                segment = resource
            assert segment, master.text[:500]
            packet = _get(_absolute(segment))
            assert packet.status_code == 200 and packet.content
            log(f"proxied live segment: {len(packet.content)} bytes")

        with step("the first-party status page renders the same live player"):
            page = _get(f"{config.PLAMENU_URL}/web/statuses/{status['id']}")
            assert page.status_code == 200
            assert 'data-live="live"' in page.text and "data-hls=" in page.text

    with step("the next lazy feed render observes that the stream ended"):
        # The render refresh is deliberately herd-throttled at 20 seconds.
        time.sleep(21)
        ended = wait_for(
            lambda: next(
                (
                    item
                    for item in plamenu_api.home_timeline(limit=40)
                    if item["id"] == status["id"]
                    and item["media_attachments"][0]["live"]["state"] == "ended"
                ),
                None,
            ),
            desc="Owncast live Note to become ended",
        )
        assert ended["media_attachments"][0]["url"] is None
