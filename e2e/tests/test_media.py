"""Media metadata federates in both directions: blurhash + focal points on
images, and the full video pipeline (async v2 upload, ffmpeg transcode to
mp4, fan-out to Mastodon)."""

import struct
import subprocess
import tempfile
import zlib
from pathlib import Path

import pytest
from plamenu_e2e import config
from plamenu_e2e.steps import step, wait_for


def make_png(width: int = 64, height: int = 48, rgb=(200, 100, 50)) -> bytes:
    """A minimal valid RGB PNG, no third-party imaging library needed."""

    def chunk(kind: bytes, payload: bytes) -> bytes:
        return (
            struct.pack(">I", len(payload))
            + kind
            + payload
            + struct.pack(">I", zlib.crc32(kind + payload))
        )

    raw = b"".join(b"\x00" + bytes(rgb) * width for _ in range(height))
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw))
        + chunk(b"IEND", b"")
    )


def make_mp4() -> bytes:
    """A one-second soundless 64x64 H.264 clip, generated with the host's
    ffmpeg (the same binary Plamenu itself shells out to)."""
    with tempfile.TemporaryDirectory() as tmp:
        out = Path(tmp) / "clip.mp4"
        subprocess.run(
            [
                "ffmpeg",
                "-nostdin",
                "-loglevel",
                "fatal",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=64x64:rate=10",
                "-pix_fmt",
                "yuv420p",
                "-c:v",
                "libx264",
                "-y",
                str(out),
            ],
            check=True,
        )
        return out.read_bytes()


def follow_from_mastodon(alice, plamenu_user):
    account = alice.resolve_account(plamenu_user.acct)
    assert account, "Mastodon could not resolve the account"
    alice.follow(account["id"])
    wait_for(
        lambda: alice.relationship(account["id"])["following"],
        desc="the follow to be accepted before posting",
    )


@pytest.mark.federation(
    direction="outbound", reverse_of="test_mastodon_media_metadata_lands_in_plamenu"
)
def test_image_blurhash_and_focus_federate_to_mastodon(
    alice, plamenu_user, plamenu_api, marker
):
    """Covers: blurhash computation on upload, focal points, and the
    `blurhash`/`focalPoint` Document attributes on the outgoing Note —
    Mastodon ingests our blurhash verbatim (its parser validates it)."""
    with step(f"alice follows @{plamenu_user.acct}"):
        follow_from_mastodon(alice, plamenu_user)

    with step("upload an image with description + focus on Plamenu"):
        uploaded = plamenu_api.upload_media(
            make_png(),
            filename="pic.png",
            mime="image/png",
            description="an e2e picture",
            focus="-0.5,0.3",
        )
        assert uploaded["blurhash"], "the upload must get a blurhash"
        assert uploaded["meta"]["focus"]["x"] == -0.5
        assert ".small." in uploaded["preview_url"]

    with step("post it; alice's copy carries our blurhash and focus"):
        plamenu_api.post_with_media(f"a picture {marker}", [uploaded["id"]])
        status = wait_for(
            lambda: alice.home_status_containing(marker),
            desc="the media post to appear on alice's home timeline",
        )
        attachment = status["media_attachments"][0]
        # Mastodon re-computes the blurhash once it downloads the file, so
        # presence (not equality) is the meaningful assertion; the exact
        # Document attribute is pinned by the integration tests.
        assert attachment["blurhash"]
        assert attachment["description"] == "an e2e picture"
        focus = attachment["meta"]["focus"]
        assert abs(focus["x"] - (-0.5)) < 1e-6
        assert abs(focus["y"] - 0.3) < 1e-6


@pytest.mark.federation(
    direction="outbound",
    one_way_reason="outbound-only video pipeline: Plamenu transcodes and federates a video attachment; inbound video ingest is covered separately by test_peertube_federation.py.",
)
def test_video_pipeline_federates_to_mastodon(alice, plamenu_user, plamenu_api, marker):
    """Covers: the async v2 upload flow (202 → worker transcode → 200 on
    the polling endpoint) and a video post reaching Mastodon with type,
    blurhash and a fetchable mp4."""
    with step(f"alice follows @{plamenu_user.acct}"):
        follow_from_mastodon(alice, plamenu_user)

    with step("upload an mp4 via /api/v2/media and wait for processing"):
        queued = plamenu_api.upload_media(
            make_mp4(), filename="clip.mp4", mime="video/mp4", v2=True
        )
        assert queued["url"] is None, "a v2 video upload must process async"
        done = wait_for(
            lambda: (m := plamenu_api.get_media(queued["id"])).get("url") and m,
            desc="the transcoding worker to finish the upload",
        )
        # A soundless clip is a gifv, like Mastodon's own pipeline.
        assert done["type"] == "gifv"
        assert done["url"].endswith(".mp4")
        assert done["blurhash"]
        assert done["meta"]["original"]["duration"] > 0.5

    with step("post it; alice sees a playable video attachment"):
        plamenu_api.post_with_media(f"a clip {marker}", [queued["id"]])
        status = wait_for(
            lambda: alice.home_status_containing(marker),
            desc="the video post to appear on alice's home timeline",
        )
        attachment = status["media_attachments"][0]
        assert attachment["type"] in ("video", "gifv"), attachment["type"]
        assert attachment["blurhash"]


def _follow_alice(plamenu_user, cli, db):
    cli.follow(plamenu_user.username, config.ALICE)
    wait_for(
        lambda: db.outbound_follow_pending(plamenu_user.username) is False,
        desc="the outbound follow to be accepted (pending=false)",
    )


def _cached_attachment(plamenu_api, status_id, *, kind=None):
    """The status' first attachment once Plamenu has cached it locally
    (its `url` points at our own /media/ route), else None — for `wait_for`."""
    atts = plamenu_api.get_status(str(status_id)).get("media_attachments") or []
    if not atts:
        return None
    att = atts[0]
    local = (att.get("url") or "").startswith(f"{config.PLAMENU_URL}/media/")
    if local and (kind is None or att["type"] == kind):
        return att
    return None


@pytest.mark.federation(
    direction="inbound",
    one_way_reason="inbound media-proxy refinement of test_mastodon_media_metadata_lands_in_plamenu: the remote image is cached and re-served from Plamenu's /media/ route; receive-side only.",
)
def test_mastodon_image_is_cached_and_served_by_plamenu(
    alice, plamenu_user, plamenu_api, cli, db, marker
):
    """Covers: a federated image is downloaded, re-processed and served from
    Plamenu's own domain (not hot-linked from the origin), with a generated
    preview and blurhash — the privacy/availability win of proxying."""
    with step(f"@{plamenu_user.username} follows alice"):
        _follow_alice(plamenu_user, cli, db)

    with step("alice posts an image"):
        uploaded = alice.upload_media(
            make_png(), filename="masto.png", mime="image/png", description="proxied"
        )
        alice.post_with_media(f"proxy image {marker}", [uploaded["id"]])

    with step("Plamenu downloads it and serves it from its own /media/ route"):
        status_id = wait_for(
            lambda: db.status_id_containing(marker),
            desc="the Mastodon media post to arrive in plamenu",
        )
        att = wait_for(
            lambda: _cached_attachment(plamenu_api, status_id),
            desc="the attachment to be cached and served locally",
        )
        assert att["type"] == "image"
        assert att["url"].startswith(f"{config.PLAMENU_URL}/media/")
        assert f"{config.PLAMENU_URL}/media/" in att["preview_url"]
        assert att["blurhash"], "Plamenu computes its own blurhash"

    with step("the cached file actually serves from Plamenu"):
        resp = plamenu_api.http.get(att["url"], timeout=30)
        assert resp.ok and resp.headers["content-type"].startswith("image/")


@pytest.mark.federation(
    direction="inbound",
    one_way_reason="inbound media-proxy refinement: a remote GIFV is detected and re-typed when proxied; receive-side only, no outbound counterpart.",
)
def test_mastodon_gifv_is_detected_when_proxied(
    alice, plamenu_user, plamenu_api, cli, db, marker
):
    """Covers the headline interop fix: a soundless clip federates as a plain
    video Document, but downloading + re-probing detects the missing audio
    stream and classifies it `gifv`, so clients loop it (they did not before
    Plamenu cached and re-probed remote media)."""
    with step(f"@{plamenu_user.username} follows alice"):
        _follow_alice(plamenu_user, cli, db)

    with step("alice posts a soundless clip (a gifv on Mastodon too)"):
        queued = alice.upload_media(
            make_mp4(), filename="clip.mp4", mime="video/mp4", v2=True
        )
        wait_for(
            lambda: (m := alice.get_media(queued["id"])).get("url") and m,
            desc="Mastodon to finish transcoding the clip",
        )
        alice.post_with_media(f"proxy gifv {marker}", [queued["id"]])

    with step("Plamenu downloads it, re-probes, and serves a looping gifv"):
        status_id = wait_for(
            lambda: db.status_id_containing(marker),
            desc="the Mastodon clip post to arrive in plamenu",
        )
        att = wait_for(
            lambda: _cached_attachment(plamenu_api, status_id, kind="gifv"),
            desc="the clip to be cached locally and detected as gifv",
        )
        assert att["type"] == "gifv"
        assert att["url"].startswith(f"{config.PLAMENU_URL}/media/")
        assert att["url"].endswith(".mp4")


@pytest.mark.federation(
    direction="inbound", reverse_of="test_image_blurhash_and_focus_federate_to_mastodon"
)
def test_mastodon_media_metadata_lands_in_plamenu(alice, plamenu_user, cli, db, marker):
    """Covers: inbound attachment parsing — Mastodon's blurhash, focal
    point, dimensions and alt text land on the stored media row."""
    with step(f"@{plamenu_user.username} follows alice"):
        cli.follow(plamenu_user.username, config.ALICE)
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the outbound follow to be accepted (pending=false)",
        )

    with step("alice posts an image with description + focus"):
        uploaded = alice.upload_media(
            make_png(),
            filename="masto.png",
            mime="image/png",
            description="a mastodon picture",
            focus="0.4,-0.2",
        )
        alice.post_with_media(f"mastodon media {marker}", [uploaded["id"]])

    with step("the stored Plamenu row carries the federated metadata"):
        status_id = wait_for(
            lambda: db.status_id_containing(marker),
            desc="the Mastodon media post to arrive in plamenu's statuses table",
        )
        rows = wait_for(
            lambda: db.media_for_status(status_id) or None,
            desc="the attachment row to be stored",
        )
        content_type, description, blurhash, focus_x, focus_y, width, height = rows[0]
        assert content_type.startswith("image/")
        assert description == "a mastodon picture"
        assert blurhash, "Mastodon's blurhash must be ingested"
        assert abs(focus_x - 0.4) < 1e-6
        assert abs(focus_y - (-0.2)) < 1e-6
        assert width and height, "federated dimensions must be stored"


def _offsite_media_urls(status) -> list[str]:
    """Every client-facing media URL a status carries — attachments, the
    author's avatar/header, custom emoji and the link-preview image — that
    points off our own instance. Empty means nothing leaks."""
    prefix = f"{config.PLAMENU_URL}/"
    leaks: list[str] = []

    def check(url):
        if url and not url.startswith(prefix):
            leaks.append(url)

    for att in status.get("media_attachments") or []:
        for key in ("url", "preview_url", "remote_url", "preview_remote_url"):
            check(att.get(key))
    account = status.get("account") or {}
    for key in ("avatar", "avatar_static", "header", "header_static"):
        check(account.get(key))
    for emoji in (account.get("emojis") or []) + (status.get("emojis") or []):
        check(emoji.get("url"))
        check(emoji.get("static_url"))
    card = status.get("card")
    if card:
        check(card.get("image"))
    return leaks


@pytest.mark.federation(
    direction="inbound",
    one_way_reason="inbound media-proxy guarantee: remote media is always re-served from Plamenu and never hot-linked off-instance; receive-side only, no outbound counterpart.",
)
def test_remote_media_never_leaks_off_instance(
    alice, plamenu_user, plamenu_api, cli, db, marker
):
    """The safety guarantee: by default a client is never handed a URL that
    points off the instance — attachments, the remote author's avatar/header
    and any emoji all route through Plamenu, even before the file is cached."""
    with step(f"@{plamenu_user.username} follows alice"):
        _follow_alice(plamenu_user, cli, db)

    with step("alice posts an image"):
        uploaded = alice.upload_media(
            make_png(), filename="leak.png", mime="image/png", description="no leaks"
        )
        alice.post_with_media(f"leak check {marker}", [uploaded["id"]])

    with step("no media URL Plamenu serves points at Mastodon"):
        status_id = wait_for(
            lambda: db.status_id_containing(marker),
            desc="the Mastodon media post to arrive in plamenu",
        )
        # Assert on first sight — before the file is necessarily cached — so we
        # catch the pre-download window that used to hot-link the origin.
        status = plamenu_api.get_status(str(status_id))
        assert status["media_attachments"], "the image attachment must be present"
        leaks = _offsite_media_urls(status)
        assert not leaks, f"these media URLs leak off-instance: {leaks}"
        assert config.MASTODON_DOMAIN not in "\n".join(
            (att.get("url") or "") + (att.get("remote_url") or "")
            for att in status["media_attachments"]
        )
