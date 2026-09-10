"""PeerTube long-form video: HLS plus universal sparse-MP4 compatibility.

Drives a real https://peertube.local video all the way through Plamenu's HLS
proxy: the Video federates, its attachment carries the `hls` extension, the
proxied master playlist is rewritten to same-origin URLs, a rendition playlist
folds PeerTube's byte-ranges into per-segment URLs, and the ordinary Mastodon
attachment URL starts and seeks through a bounded Range gateway without ever
queuing a detached whole-video download. See PEERTUBE_HLS_DESIGN.md.
"""

import struct
import subprocess
import tempfile
import time
from pathlib import Path

import pytest
import requests
import urllib3
from plamenu_e2e import peertube
from plamenu_e2e.api import Api
from plamenu_e2e.db import Db
from plamenu_e2e.steps import log, step, wait_for

urllib3.disable_warnings()

pytestmark = pytest.mark.skipif(
    not peertube.reachable(), reason="peertube.local is not up"
)


def _get(url: str) -> requests.Response:
    """A raw GET of a public Plamenu proxy URL (local Caddy CA → verify off)."""
    return requests.get(url, verify=False, timeout=20)


def _absolute(url_or_path: str) -> str:
    return (
        url_or_path
        if url_or_path.startswith("http")
        else "https://plamenu.local" + url_or_path
    )


def _first_uri(body: str, needle: str):
    """The first *bare* URI line (a child resource) containing `needle` — not a
    `#EXT-X-…:URI="…"` tag line, which would make a `#fragment` URL."""
    return next(
        (
            ln.strip()
            for ln in body.splitlines()
            if ln.strip().startswith("/media/hls/") and needle in ln
        ),
        None,
    )


def _top_level_box(body: bytes, wanted: bytes) -> tuple[int, bytes] | None:
    """Find a complete ordinary-size ISO-BMFF box in a fetched prefix."""
    cursor = 0
    while cursor + 8 <= len(body):
        size, kind = struct.unpack_from(">I4s", body, cursor)
        if size < 8 or cursor + size > len(body):
            return None
        if kind == wanted:
            return cursor, body[cursor : cursor + size]
        cursor += size
    return None


@pytest.mark.federation(
    direction="inbound",
    peer="peertube",
    one_way_reason="Plamenu has no PeerTube-compatible publishing model — its Note builder emits only Note/Question/Page (crates/ap/src/activity.rs:399-404) and never originates a Video/HLS object; PeerTube long-form video is ingest-only, so there is no outbound counterpart to pair.",
)
def test_peertube_video_federates_and_plays_through_the_hls_proxy(
    plamenu_api: Api, db: Db
):
    pt = peertube.root()
    with step("a HLS-transcoded PeerTube video exists"):
        video = peertube.ensure_test_video()
        ap_id = pt.ap_id(video["uuid"])
        log(f"video {video['uuid']} → {ap_id}")

    with step("Plamenu resolves + ingests the PeerTube Video"):
        status = wait_for(
            lambda: plamenu_api.resolve_status(ap_id),
            desc="Plamenu ingests the PeerTube video",
        )
        media = status["media_attachments"]
        assert len(media) == 1, f"exactly one video attachment: {media}"
        att = media[0]
        assert att["type"] == "video", att

    with step("the attachment carries the HLS extension + a progressive url"):
        hls = att.get("hls")
        assert hls, f"hls extension present on a PeerTube video: {att}"
        assert "/media/hls/" in hls["master"], hls
        assert len(hls["renditions"]) >= 1, hls
        # Every quality-blind/no-JS client gets the same stable, seekable MP4
        # facade; HLS-aware third-party clients can use remote_url directly.
        assert "/media/play/" in att["url"] and att["url"].endswith("/video.mp4"), att
        assert att["remote_url"] == hls["master"], att
        # Quality-blind clients get the SAME universally-light rung: 480p is the
        # ceiling the backend picks (the test ladder tops out at 480).
        height = att.get("meta", {}).get("original", {}).get("height")
        assert height is not None and height <= 480, (
            f"progressive url capped at 480p: {att['meta']}"
        )
        log(
            f"{len(hls['renditions'])} rendition(s); progressive rung {height}p; master {hls['master']}"
        )

    with step("the proxied master playlist is rewritten same-origin"):
        master = _get(hls["master"])
        assert master.status_code == 200, master.text[:300]
        assert "mpegurl" in master.headers.get("content-type", ""), master.headers
        body = master.text
        assert "peertube.local" not in body, (
            f"no origin URL leaks to the client: {body[:400]}"
        )
        pl = _first_uri(body, "/pl?u=")
        assert pl and "/media/hls/" in pl, f"a proxied media playlist: {body}"

    with step("a rendition playlist folds byte-ranges into per-segment URLs"):
        rendition = _get(_absolute(pl))
        assert rendition.status_code == 200, rendition.text[:300]
        assert "#EXT-X-BYTERANGE" not in rendition.text, "byte-ranges folded into URLs"
        seg = _first_uri(rendition.text, "/seg?u=")
        assert seg and "s=" in seg and "l=" in seg, (
            f"a proxied ranged segment: {rendition.text}"
        )

    with step("a segment streams from cache — the second fetch reuses it"):
        seg_url = _absolute(seg)
        first = _get(seg_url)
        assert first.status_code == 200 and first.content, first.status_code
        assert first.headers.get("content-type", "").startswith("video/mp4"), (
            first.headers
        )
        second = _get(seg_url)
        assert second.status_code == 200
        assert second.content == first.content, "cache serves identical bytes"
        log(f"segment {len(first.content)} bytes cached + reused (one origin fetch)")

    with step("the compatibility MP4 starts, seeks, and creates no download job"):
        compat = att["url"]
        head = requests.head(compat, verify=False, timeout=20)
        assert head.status_code == 200, head.status_code
        assert head.headers.get("accept-ranges") == "bytes", head.headers
        total = int(head.headers["content-length"])
        assert total > 8192

        prefix_end = min(total - 1, 65535)
        first = requests.get(
            compat,
            headers={"Range": f"bytes=0-{prefix_end}"},
            verify=False,
            timeout=20,
        )
        assert first.status_code == 206 and len(first.content) == prefix_end + 1, (
            first.headers
        )
        assert first.headers["content-range"] == f"bytes 0-{prefix_end}/{total}"
        found = _top_level_box(first.content, b"sidx")
        assert found, "the initial prefix carries a complete global seek index"
        sidx_start, sidx = found
        timescale = struct.unpack_from(">I", sidx, 16)[0]
        count = struct.unpack_from(">H", sidx, 30)[0]
        entries = [struct.unpack_from(">III", sidx, 32 + i * 12) for i in range(count)]
        indexed_duration = sum(duration for _, duration, _ in entries) / timescale
        declared_duration = att.get("meta", {}).get("original", {}).get("duration")
        assert indexed_duration > 1, indexed_duration
        if declared_duration:
            assert abs(indexed_duration - float(declared_duration)) < 1, (
                indexed_duration,
                declared_duration,
            )
        indexed_size = sum(size & 0x7FFFFFFF for size, _, _ in entries)
        assert sidx_start + len(sidx) + indexed_size == total, (
            "the seek index covers every following byte exactly"
        )

        last_start = (
            sidx_start
            + len(sidx)
            + sum(size & 0x7FFFFFFF for size, _, _ in entries[:-1])
        )
        indexed_seek = requests.get(
            compat,
            headers={"Range": f"bytes={last_start}-{last_start + 99}"},
            verify=False,
            timeout=20,
        )
        assert indexed_seek.status_code == 206 and len(indexed_seek.content) == 100
        tail = requests.get(
            compat,
            headers={"Range": f"bytes={total - 4096}-{total - 1}"},
            verify=False,
            timeout=20,
        )
        assert tail.status_code == 206 and len(tail.content) == 4096, tail.headers

        # Abandon a live response after its first chunk, as a client navigating
        # away would. The transport-level test verifies origin chunks stop;
        # here the real stack proves this path never falls back to a worker job.
        abandoned = requests.get(compat, verify=False, timeout=20, stream=True)
        next(abandoned.iter_content(chunk_size=1024))
        abandoned.close()
        time.sleep(1)
        jobs = db.conn.execute(
            "SELECT count(*) FROM media_processing_jobs WHERE media_id = %s",
            (int(att["id"]),),
        ).fetchone()[0]
        assert jobs == 0, "play/seek/disconnect must never queue a whole-file download"
        log(
            f"full {indexed_duration:.3f}s index + far/tail seeks served from "
            f"{total}-byte virtual resource; no job"
        )

    with step("the complete compatibility resource has video and sound"):
        # This intentionally downloads the tiny five-second E2E fixture only;
        # normal playback above was verified with sparse requests.
        full = _get(att["url"])
        assert full.status_code == 200 and len(full.content) == total
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "compat.mp4"
            path.write_bytes(full.content)
            probe = subprocess.run(
                [
                    "ffprobe",
                    "-v",
                    "error",
                    "-show_entries",
                    "stream=codec_type:format=duration",
                    "-of",
                    "default=nw=1",
                    str(path),
                ],
                capture_output=True,
                text=True,
                check=True,
                timeout=20,
            )
        assert "codec_type=video" in probe.stdout, probe.stdout
        assert "codec_type=audio" in probe.stdout, probe.stdout
        probed_duration = float(
            [
                line
                for line in probe.stdout.splitlines()
                if line.startswith("duration=")
            ][-1].split("=", 1)[1]
        )
        assert abs(probed_duration - indexed_duration) < 1, probe.stdout

    with step("the first-party web page renders the HLS player element"):
        page = requests.get(
            f"https://plamenu.local/web/statuses/{status['id']}",
            verify=False,
            timeout=20,
            allow_redirects=True,
        )
        assert page.status_code == 200, page.status_code
        html = page.text
        # The <video> carries the proxied master for app.js/hls.js to take over,
        # inside the media--video wrapper the quality selector positions against.
        assert f'data-hls="{hls["master"]}"' in html, "video[data-hls] rendered"
        assert "media--video" in html
        # The primary element itself carries the compatibility source. This is
        # essential for NoScript/CSP cases where script execution is blocked but
        # the HTML parser still treats <noscript> content as inert. Working JS
        # detaches it immediately and upgrades the same element to HLS.
        marker = f'data-hls="{hls["master"]}"'
        marker_at = html.index(marker)
        player = html[
            html.rfind("<video", 0, marker_at) : html.index(">", marker_at) + 1
        ]
        assert f'src="{att["url"]}"' in player, player
        assert f'data-src="{att["url"]}"' in player, player
        # app.js (which lazy-loads hls.min.js) is linked; hls.js itself is NOT in
        # the initial HTML (loaded on first play).
        assert "/assets/app.js" in html
        log("web player rendered with HLS upgrade data + a script-independent MP4 src")


@pytest.mark.federation(
    direction="inbound",
    peer="peertube",
    one_way_reason="Plamenu has no PeerTube-compatible publishing model — its Note builder emits only Note/Question/Page (crates/ap/src/activity.rs:399-404) and never originates a Video/HLS object; a live broadcast is ingest-only, so there is no outbound counterpart to pair.",
)
def test_peertube_live_federates_and_plays_in_every_client(
    plamenu_api: Api, db: Db, request
):
    """A PeerTube live broadcast, from announcement to air to over.

    A live is the awkward shape in the whole media stack: it federates as a
    Video that has no media at all, gains a playlist only once RTMP is
    flowing, publishes MPEG-TS segments the origin then deletes, and keeps
    advertising its master after it ends. This drives the real thing through
    every lane a viewer could use — the HLS proxy, the shared remuxing
    gateway, and the server-rendered page — and checks that each one reports
    "not playable" when the broadcast is not on air, rather than handing out a
    URL that fails.
    """
    pt = peertube.root()

    with step("the viewer follows the PeerTube account with notifications on"):
        root_actor = wait_for(
            lambda: plamenu_api.resolve_account("root@peertube.local"),
            desc="Plamenu resolves the PeerTube account",
        )
        plamenu_api.post(f"/api/v1/accounts/{root_actor['id']}/follow", notify=True)
        wait_for(
            lambda: plamenu_api.relationship(root_actor["id"])["following"],
            desc="PeerTube accepts the viewer's follow",
        )

    with step("PeerTube announces a live that has not started"):
        live_name = f"Plamenu e2e live {time.time_ns()}"
        video = pt.create_live(live_name, latency_mode=2)
        # An announced live counts against the per-user live limit until it is
        # deleted, so a run that failed midway must not poison the next one.
        request.addfinalizer(lambda: pt.delete_video(video["id"]))
        announced = pt.fetch_ap(video["uuid"])
        assert announced["isLiveBroadcast"] is True, announced
        assert not [
            entry
            for entry in announced["url"]
            if entry.get("mediaType") == "application/x-mpegURL"
        ], "an announced live publishes no playlist at all"
        log(f"live {video['uuid']} announced (state {announced['state']})")

    with step("Plamenu delivers it into home as a waiting broadcast"):

        def delivered_live():
            for item in plamenu_api.home_timeline(limit=40):
                candidate = item.get("reblog") or item
                if candidate.get("title") == live_name:
                    return candidate
            return None

        status = wait_for(
            delivered_live,
            desc="the announced live to reach the viewer's home timeline",
        )
        att = status["media_attachments"][0]
        assert att["type"] == "video", att
        assert att["live"] == {"state": "waiting", "permanent": False}, att
        assert att["url"] is None, "nothing is playable before the stream starts"
        assert att.get("hls") is None, att
        assert att["preview_url"], "the poster is available from the start"
        # Off air, the HLS lane refuses rather than proxying a dead playlist.
        assert (
            _get(f"https://plamenu.local/media/hls/{att['id']}/master.m3u8").status_code
            == 404
        )
        assert (
            _get(f"https://plamenu.local/media/live/{att['id']}/stream.mp4").status_code
            == 404
        )

    with step("the page shows the poster and says the stream has not started"):
        page = _get(f"https://plamenu.local/web/statuses/{status['id']}")
        assert page.status_code == 200
        assert "media--live-offline" in page.text, "offline tile rendered"
        assert "<video" not in page.text, "no player with nothing to play"

    with peertube.broadcast(pt, video["id"]):
        with step("going on air delivers a live notification and flips state"):
            notification = wait_for(
                lambda: next(
                    (
                        notice
                        for notice in plamenu_api.notifications(limit=40)
                        if notice["type"] == "live"
                        and (notice.get("status") or {}).get("id") == status["id"]
                    ),
                    None,
                ),
                desc="the go-live notification to arrive from federation delivery",
            )
            assert notification["account"]["acct"] == "root@peertube.local"
            att = wait_for(
                lambda: _live_attachment(plamenu_api, status["id"], "live"),
                desc="Plamenu sees the broadcast go on air",
            )
            assert att["url"].endswith("/stream.mp4"), att
            assert "/media/hls/" in att["hls"]["master"], att
            fresh = plamenu_api.get_status(status["id"])
            assert fresh["edited_at"] is None, (
                "a stream starting is not an edit of the post"
            )
            log(f"on air: {att['url']}")

        with step("the HLS lane serves a live window of MPEG-TS segments"):
            master = _get(att["hls"]["master"])
            assert master.status_code == 200, master.text[:300]
            assert "peertube.local" not in master.text, "no origin leak"
            pl_line = _first_uri(master.text, "/live/")
            assert pl_line, f"live children carry their extension: {master.text}"
            rendition = _get(_absolute(pl_line))
            assert rendition.status_code == 200
            # A live playlist is a sliding window: no end marker, and the
            # client must never be told to reuse it.
            assert "#EXT-X-ENDLIST" not in rendition.text, rendition.text
            assert rendition.headers["cache-control"] == "no-store", rendition.headers
            seg_line = _first_uri(rendition.text, "/live/")
            assert seg_line and seg_line.endswith(".ts"), (
                f"live segments are whole MPEG-TS files: {rendition.text}"
            )
            first = _get(_absolute(seg_line))
            assert first.status_code == 200 and first.content
            assert first.headers["content-type"] == "video/mp2t", first.headers
            second = _get(_absolute(seg_line))
            assert second.content == first.content, "cached: one origin fetch"
            log(f"segment {len(first.content)} bytes cached + reused")

        with step("the gateway remuxes one shared stream for every dumb client"):
            viewers = [
                requests.get(att["url"], verify=False, timeout=60, stream=True)
                for _ in range(2)
            ]
            try:
                for viewer in viewers:
                    assert viewer.status_code == 200, viewer.status_code
                    assert viewer.headers["content-type"] == "video/mp4"
                    assert "content-length" not in viewer.headers, (
                        "an endless stream has no length"
                    )
                    assert viewer.headers["accept-ranges"] == "none", viewer.headers
                bodies = [_read_for(viewer, seconds=10) for viewer in viewers]
            finally:
                for viewer in viewers:
                    viewer.close()
            for body in bodies:
                assert body.startswith(b"\x00\x00\x00") or b"ftyp" in body[:64], (
                    "every viewer is given the initialization section first"
                )
                assert len(body) > 32 * 1024, f"only {len(body)} bytes arrived"
            # Both viewers were served by ONE ffmpeg: more would mean the
            # gateway multiplies origin load by the number of viewers.
            assert _remux_processes(att["id"]) == 1, "one remux per broadcast"
            _assert_decodes(bodies[0])
            log(f"2 viewers, 1 remux, {len(bodies[0])} bytes of playable fMP4")

        with step("the page renders a live player with the LIVE badge"):
            page = _get(f"https://plamenu.local/web/statuses/{status['id']}")
            assert f'data-hls="{att["hls"]["master"]}"' in page.text
            assert 'data-live="live"' in page.text
            assert "media__badge--live" in page.text
            # The bare src is the no-JS/CSP path: the gateway, not a playlist.
            assert f'src="{att["url"]}"' in page.text

    with step("the broadcast ends and stops being playable"):
        att = wait_for(
            lambda: _live_attachment(plamenu_api, status["id"], "ended"),
            desc="Plamenu sees the broadcast end",
        )
        assert att["url"] is None, "an ended stream offers nothing to play"
        assert att.get("hls") is None, att
        assert (
            _get(f"https://plamenu.local/media/live/{att['id']}/stream.mp4").status_code
            == 404
        )
        # PeerTube still advertises the master it is about to delete — "ended"
        # must come from the state, never from a fetch failing.
        ended = pt.fetch_ap(video["uuid"])
        assert [
            e for e in ended["url"] if e.get("mediaType") == "application/x-mpegURL"
        ], "the origin still lists a master after the stream ends"
        log("ended: read from state, not from a dead playlist")


def _live_attachment(api: Api, status_id: str, want_state: str):
    """The status' live attachment once it reaches `want_state`, else None.

    Fetching the status by id is what refreshes a resolve-only broadcast, so
    this doubles as the driver for the state transition.
    """
    att = api.get_status(status_id)["media_attachments"][0]
    return att if att.get("live", {}).get("state") == want_state else None


def _read_for(response: requests.Response, *, seconds: float) -> bytes:
    deadline = time.monotonic() + seconds
    body = b""
    for chunk in response.iter_content(64 * 1024):
        body += chunk
        if time.monotonic() > deadline:
            break
    return body


def _remux_processes(media_id: str) -> int:
    """How many gateway ffmpeg processes are remuxing this broadcast."""
    listing = subprocess.run(
        ["pgrep", "-af", "ffmpeg"], capture_output=True, text=True, check=False
    ).stdout
    return sum(1 for line in listing.splitlines() if f"/media/hls/{media_id}/" in line)


def _assert_decodes(body: bytes) -> None:
    """The bytes must be a real fragmented MP4 with video and audio."""
    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / "live.mp4"
        path.write_bytes(body)
        probe = subprocess.run(
            [
                "ffprobe",
                "-v",
                "error",
                "-show_entries",
                "stream=codec_type",
                "-of",
                "default=nw=1",
                str(path),
            ],
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
    assert "codec_type=video" in probe.stdout, probe.stdout or probe.stderr
    assert "codec_type=audio" in probe.stdout, probe.stdout or probe.stderr
