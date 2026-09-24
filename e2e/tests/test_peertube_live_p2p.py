"""PeerTube-origin live segments shared with Plamenu's first-party player."""

import json
import os
import subprocess
from urllib.parse import urlparse

import pytest
import requests
from plamenu_e2e import config, peertube, unique
from plamenu_e2e.api import Api
from plamenu_e2e.steps import step, wait_for


@pytest.mark.skipif(not peertube.reachable(), reason="peertube.local is not up")
@pytest.mark.federation(direction="inbound", peer="peertube")
def test_peertube_live_swarm_in_both_first_party_players(plamenu_api: Api):
    pt = peertube.root()
    video = pt.create_live(unique("pt-live-peers"), latency_mode=1)
    try:
        with peertube.broadcast(pt, video["id"]):
            with step("PeerTube's on-air Video advertises a rolling checksum swarm"):
                def origin_hints():
                    ap = pt.fetch_ap(video["uuid"])
                    playlist = next((entry for entry in ap["url"]
                                     if entry.get("mediaType") == "application/x-mpegURL"), None)
                    if not playlist:
                        return None
                    hashes = [tag["name"] for tag in playlist.get("tag", [])
                              if tag.get("type") == "Infohash"]
                    checksum = next((tag.get("href") for tag in playlist.get("tag", [])
                                     if tag.get("name") == "sha256"), None)
                    trackers = [entry.get("href") for entry in ap["url"]
                                if entry.get("name") == "tracker-websocket"]
                    return (hashes, checksum, trackers) if hashes and checksum and trackers else None

                hashes, checksum_url, trackers = wait_for(
                    origin_hints, desc="live PeerTube infohashes and checksum Link", timeout=90,
                )
                assert checksum_url.startswith(f"{config.PEERTUBE_URL}/")
                assert any(urlparse(url).scheme == "wss" and
                           urlparse(url).hostname == config.PEERTUBE_DOMAIN
                           for url in trackers), trackers

            with step("Plamenu retains a safe same-origin live checksum proxy"):
                def remote_live():
                    item = plamenu_api.resolve_status(pt.ap_id(video["uuid"]))
                    attachment = item["media_attachments"][0]
                    return item if (attachment.get("live", {}).get("state") == "live"
                                    and attachment.get("hls", {}).get("p2p")) else None
                status = wait_for(
                    remote_live, desc="PeerTube live swarm resolves in Plamenu", timeout=90,
                )
                attachment = status["media_attachments"][0]
                assert attachment["live"]["state"] == "live", attachment
                peers = attachment.get("hls", {}).get("p2p")
                assert peers and peers["video_uuid"] == video["uuid"], attachment
                assert {(urlparse(url).hostname, urlparse(url).path)
                        for url in peers["trackers"]} == {
                            (urlparse(url).hostname, urlparse(url).path)
                            for url in trackers
                        }, peers
                assert peers["sha256_url"].startswith(f"{config.PLAMENU_URL}/media/hls/")
                def checksum_with_segment():
                    response = requests.get(peers["sha256_url"], verify=False, timeout=30)
                    return response if response.status_code == 200 and response.json() else None
                response = wait_for(
                    checksum_with_segment, desc="first PeerTube live checksum", timeout=45,
                )
                assert response.headers["Cache-Control"] == "no-store"
                first = response.json()
                wait_for(
                    lambda: (updated if (updated := requests.get(
                        peers["sha256_url"], verify=False, timeout=30,
                    ).json()) != first else None),
                    desc="remote live checksum manifest rolls", timeout=45,
                )

            with step("both real first-party players exchange checksum-validated live bytes"):
                subprocess.run(
                    ["node", "verify-peertube-live-p2p.mjs",
                     f"{config.PLAMENU_URL}/web/statuses/{status['id']}",
                     f"{config.PEERTUBE_URL}/w/{video['uuid']}", json.dumps(hashes)],
                    cwd=config.PLAMENU_DIR / "accessibility", check=True, timeout=210,
                    env={**os.environ, "PEERTUBE_E2E_USER": peertube.ROOT_USER,
                         "PEERTUBE_E2E_PASSWORD": peertube.ROOT_PASS},
                )
        with step("ending the live removes its peer controls and checksum document"):
            ended = wait_for(
                lambda: (item if (item := plamenu_api.get_status(status["id"])["media_attachments"][0])
                         .get("live", {}).get("state") == "ended" else None),
                desc="Plamenu sees PeerTube's live end", timeout=120,
            )
            assert ended.get("hls") is None or "p2p" not in ended["hls"]
            assert requests.get(peers["sha256_url"], verify=False, timeout=30).status_code == 404
    finally:
        pt.delete_video(video["id"])
