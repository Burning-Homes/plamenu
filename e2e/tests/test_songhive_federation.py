"""Real Songhive Follow/Accept, Audio lifecycle, metadata and playable media."""

import hashlib
import re
import subprocess
from pathlib import Path

import pytest
import requests
from plamenu_e2e import config
from plamenu_e2e.songhive import SONGHIVE_DOMAIN, Songhive
from plamenu_e2e.steps import step, wait_for
from psycopg import sql


@pytest.fixture(scope="module")
def songhive():
    return Songhive()


def _audio(path: Path, marker: str):
    frequency = (
        400 + int.from_bytes(hashlib.sha256(marker.encode()).digest()[:8]) / 2**64 * 800
    )
    subprocess.run(
        [
            "ffmpeg",
            "-nostdin",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            f"sine=frequency={frequency}:duration=3.15",
            "-metadata",
            f"title={marker}",
            "-metadata",
            "artist=Songhive interop artist",
            "-metadata",
            "album=Interop album",
            "-q:a",
            "7",
            str(path),
        ],
        check=True,
    )


def _assert_decoded_audio(content: bytes):
    decoded = subprocess.run(
        [
            "ffmpeg",
            "-nostdin",
            "-v",
            "error",
            "-i",
            "pipe:0",
            "-t",
            "2",
            "-f",
            "s16le",
            "pipe:1",
        ],
        input=content,
        capture_output=True,
        check=True,
        timeout=30,
    )
    assert len(decoded.stdout) > 44100, "Stream contains too little decoded audio"


def _cached_peer_document(songhive, uri):
    row = songhive.cached_resource(uri)
    if not row or row["unavailable"] or not isinstance(row["payload"], dict):
        return None
    payload = row["payload"]
    return payload.get("object", payload)


@pytest.mark.federation(direction="both", peer="songhive")
def test_songhive_signed_follow_lifecycle(songhive, plamenu_api, plamenu_user, db):
    with step("Plamenu discovers and follows Songhive"):
        remote = plamenu_api.resolve_account(f"sylvia@{SONGHIVE_DOMAIN}")
        assert remote
        plamenu_api.follow(remote["id"])
        wait_for(
            lambda: plamenu_api.relationship(remote["id"])["following"],
            desc="Songhive Accept",
        )
    with step("Songhive discovers and follows Plamenu through authorized fetch"):
        account = plamenu_api.get("/api/v1/accounts/verify_credentials")
        target = account["uri"]
        songhive.follow(target)
        wait_for(lambda: songhive.following(target), desc="Plamenu Accept in Songhive")
        wait_for(
            lambda: db.follower_count(plamenu_user.username) == 1,
            desc="Songhive follower row",
        )
    with step("both peers process signed Undo(Follow)"):
        songhive.unfollow(target)
        plamenu_api.unfollow(remote["id"])
        wait_for(
            lambda: db.follower_count(plamenu_user.username) == 0, desc="inbound Undo"
        )
        wait_for(
            lambda: not plamenu_api.relationship(remote["id"])["following"],
            desc="outbound Undo",
        )


@pytest.mark.federation(direction="both", peer="songhive")
def test_native_audio_songhive_roundtrip(
    songhive, plamenu_api, plamenu_user, marker, tmp_path
):
    local = plamenu_api.get("/api/v1/accounts/verify_credentials")
    songhive.follow(local["uri"])
    wait_for(
        lambda: songhive.following(local["uri"]), desc="Songhive follows Audio author"
    )
    remote = plamenu_api.resolve_account(f"sylvia@{SONGHIVE_DOMAIN}")
    plamenu_api.follow(remote["id"])
    wait_for(
        lambda: plamenu_api.relationship(remote["id"])["following"],
        desc="Plamenu follows Songhive",
    )
    path = tmp_path / f"{marker}.mp3"
    _audio(path, marker)
    queue_device = "songhive_" + hashlib.sha256(marker.encode()).hexdigest()[:16]
    try:
        with step("Plamenu publishes a native Audio, with structured artist and album"):
            media = plamenu_api.upload_media(
                path.read_bytes(), filename=path.name, mime="audio/mpeg"
            )
            published = plamenu_api._request(
                "POST",
                "/api/v1/statuses",
                json={
                    "status": f"Native Audio {marker}",
                    "title": marker,
                    "post_kind": "audio",
                    "visibility": "public",
                    "media_ids": [media["id"]],
                    "music": {
                        "artist": "Plamenu interop artist",
                        "album": "Interop album",
                    },
                },
            )
            assert published["object_type"] == "Audio"
            received = wait_for(
                lambda: songhive.activity(published["uri"]),
                desc="real inbound Audio in Songhive",
            )
            assert received
        with step("Songhive resolves and exposes playable remote music"):
            lookup = songhive.lookup(published["uri"])
            assert lookup["object"]["object_type"] == "Audio", lookup
            detail = songhive.request(
                "GET", "/api/v1/remote/objects/" + lookup["object"]["id"]
            )
            stream_url = detail["object"].get("stream_url")
            assert stream_url, detail
            audio = songhive.http.get(
                "https://songhive.local" + stream_url
                if stream_url.startswith("/")
                else stream_url,
                timeout=60,
            )
            assert audio.status_code == 200, audio.text[:400]
            _assert_decoded_audio(audio.content)
        with step("Songhive likes and boosts Plamenu Audio, then sends both Undos"):
            for action, field in [
                ("like", "favourites_count"),
                ("boost", "reblogs_count"),
            ]:
                songhive.request(
                    "POST", f"/api/v1/activities/{received['id']}/{action}"
                )
                wait_for(
                    lambda field=field: (
                        plamenu_api.get(f"/api/v1/statuses/{published['id']}")[field]
                        == 1
                    ),
                    desc=f"Songhive {action} on Audio",
                )
                songhive.request(
                    "DELETE", f"/api/v1/activities/{received['id']}/{action}"
                )
                wait_for(
                    lambda field=field: (
                        plamenu_api.get(f"/api/v1/statuses/{published['id']}")[field]
                        == 0
                    ),
                    desc=f"Songhive Undo {action} on Audio",
                )
        with step("Plamenu edits its Audio without changing its identity"):
            plamenu_api._request(
                "PUT",
                f"/api/v1/statuses/{published['id']}",
                json={
                    "status": f"Updated Audio {marker}",
                    "music": {
                        "artist": "Updated interop artist",
                        "album": "Updated album",
                        "release_year": 2026,
                        "track_number": 2,
                        "disc_number": 1,
                    },
                },
            )
            wait_for(
                lambda: (
                    (row := songhive.activity(published["uri"]))
                    and marker in str(row)
                    and "Updated Audio" in str(row)
                    and "Updated interop artist" in str(row)
                    and "Updated album" in str(row)
                ),
                desc="Audio Update at Songhive",
            )
        with step("Songhive publishes its own tagged track as real Audio"):
            track = songhive.upload(path)
            result = songhive.publish(track["id"])
            incoming = wait_for(
                lambda: plamenu_api.resolve_status(result["object_id"]),
                desc="Songhive Audio at Plamenu",
            )
            assert incoming["object_type"] == "Audio"
            assert incoming["music"]["artist"] == "Songhive interop artist", incoming
            assert incoming["music"]["album"] == "Interop album", incoming
            attachment = incoming["media_attachments"][0]
            playable = requests.get(
                attachment["url"],
                verify=str(config.MASTO_DIR / "ca-bundle.crt"),
                timeout=60,
            )
            assert playable.status_code == 200
            _assert_decoded_audio(playable.content)
        with step(
            "Plamenu privately saves Songhive music without sending a social Like"
        ):
            saved = plamenu_api._request(
                "POST", "/api/v1/music/favourites", json={"status_id": incoming["id"]}
            )
            assert saved["track"]["url"] == incoming["url"]
            again = plamenu_api._request(
                "POST", "/api/v1/music/favourites", json={"status_id": incoming["id"]}
            )
            assert again["id"] == saved["id"]
            favourites = plamenu_api.get("/api/v1/music/favourites")
            assert len(favourites["items"]) == 1
            assert favourites["items"][0]["status_id"] == incoming["id"]
            assert not plamenu_api.get(f"/api/v1/statuses/{incoming['id']}")[
                "favourited"
            ]
            assert songhive.activity(result["object_id"])["like_count"] == 0
        with step(
            "Plamenu collects verified Songhive Audio independently of favourites"
        ):
            reference = {"kind": "audio", "id": incoming["id"]}
            bookmark = plamenu_api._request(
                "POST", "/api/v1/music/saved", json=reference
            )
            assert bookmark["reference"] == reference
            assert bookmark["source"]["url"] == incoming["url"]
            again = plamenu_api._request("POST", "/api/v1/music/saved", json=reference)
            assert again["id"] == bookmark["id"]
            collected = plamenu_api.get("/api/v1/music/mine?kind=tracks")
            remote_items = [
                item for item in collected["items"] if item["reference"] == reference
            ]
            assert len(remote_items) == 1
            assert remote_items[0]["membership"] == {
                "owned": False,
                "saved": True,
                "favourite": True,
                "implicit": False,
            }
            assert remote_items[0]["source"]["url"] == incoming["url"]
            plamenu_api._request("DELETE", f"/api/v1/music/saved/{bookmark['id']}")
            assert len(plamenu_api.get("/api/v1/music/favourites")["items"]) == 1
            bookmark = plamenu_api._request(
                "POST", "/api/v1/music/saved", json=reference
            )
            assert songhive.activity(result["object_id"])["like_count"] == 0
        with step("Plamenu queues verified Songhive Audio without changing its origin"):
            queue = plamenu_api._request(
                "POST",
                f"/api/v1/music/queues/{queue_device}",
                json={
                    "revision": "0",
                    "action": "add",
                    "sources": [{"status_id": incoming["id"]}],
                },
            )
            assert len(queue["entries"]) == 1
            queued = queue["entries"][0]
            assert queued["url"] == incoming["url"]
            # The remote-media worker can replace a proxy URL with its cached
            # file between these reads. Origin identity stays canonical.
            queue_stream = requests.get(
                queued["stream_url"],
                verify=str(config.MASTO_DIR / "ca-bundle.crt"),
                timeout=60,
            )
            queue_stream.raise_for_status()
            _assert_decoded_audio(queue_stream.content)
            assert queued["artist"] == incoming["music"]["artist"]
            queue = plamenu_api._request(
                "POST",
                f"/api/v1/music/queues/{queue_device}",
                json={
                    "revision": queue["revision"],
                    "action": "select",
                    "entry_id": queued["entry_id"],
                },
            )
            assert queue["current_entry_id"] == queued["entry_id"]
            assert queue["entries"][0]["url"] == incoming["url"]
        with step("real-time Songhive playback records one explicit private listen"):
            assert plamenu_api.get("/api/v1/music/history")["items"] == []
            preferences = plamenu_api.get("/api/v1/music/listening/preferences")
            plamenu_api._request(
                "PUT",
                "/api/v1/music/listening/preferences",
                json={
                    "revision": preferences["revision"],
                    "history_enabled": True,
                    "minimum_seconds": 1,
                    "minimum_percent": 100,
                },
            )
            session = plamenu_api._request(
                "POST",
                "/api/v1/music/listening/sessions",
                json={
                    "request_key": queue_device,
                    "queue_device": queue_device,
                    "queue_entry_id": queued["entry_id"],
                },
            )
            assert not session["counted"]
            subprocess.run(
                [
                    "ffmpeg",
                    "-nostdin",
                    "-v",
                    "error",
                    "-re",
                    "-i",
                    "pipe:0",
                    "-t",
                    "2",
                    "-f",
                    "null",
                    "-",
                ],
                input=playable.content,
                capture_output=True,
                check=True,
                timeout=30,
            )
            endpoint = f"/api/v1/music/listening/sessions/{session['token']}"
            progress = plamenu_api._request(
                "POST",
                endpoint,
                json={"played_seconds": 2, "finish": True},
            )
            assert progress["counted"]
            history = plamenu_api.get("/api/v1/music/history")
            assert len(history["items"]) == 1
            assert history["items"][0]["url"] == incoming["url"]
            plamenu_api._request(
                "POST", endpoint, json={"played_seconds": 2, "finish": True}
            )
            assert len(plamenu_api.get("/api/v1/music/history")["items"]) == 1
            stats = plamenu_api.get("/api/v1/music/stats")
            assert stats["play_count"] == 1
            assert stats["top"]["tracks"][0]["url"] == incoming["url"]
            assert stats["top"]["artists"][0]["name"] == incoming["music"]["artist"]
            assert stats["top"]["albums"][0]["name"] == incoming["music"]["album"]
            assert sum(bucket["count"] for bucket in stats["plays"]) == 1
            assert len(stats["clock"]) == 24
            assert sum(hour["count"] for hour in stats["clock"]) == 1

        with step(
            "Plamenu favourites and boosts Songhive Audio, then sends both Undos"
        ):
            for apply, undo, field in [
                (plamenu_api.favourite, plamenu_api.unfavourite, "like_count"),
                (plamenu_api.reblog, plamenu_api.unreblog, "boost_count"),
            ]:
                apply(incoming["id"])
                wait_for(
                    lambda field=field: (
                        songhive.activity(result["object_id"])[field] == 1
                    ),
                    desc=f"Plamenu {field} at Songhive",
                )
                undo(incoming["id"])
                wait_for(
                    lambda field=field: (
                        songhive.activity(result["object_id"])[field] == 0
                    ),
                    desc=f"Plamenu Undo {field} at Songhive",
                )
        with step("Songhive edits the same Audio and Plamenu retains music metadata"):
            songhive.request(
                "PATCH",
                f"/api/v1/tracks/{track['id']}",
                json={"title": f"Updated {marker}"},
            )
            wait_for(
                lambda: (
                    (row := plamenu_api.get(f"/api/v1/statuses/{incoming['id']}"))
                    and f"Updated {marker}" in row["title"]
                ),
                desc="Songhive Audio Update in Plamenu",
            )
            queue = plamenu_api.get(f"/api/v1/music/queues/{queue_device}")
            assert f"Updated {marker}" in queue["entries"][0]["title"]
            favourites = plamenu_api.get("/api/v1/music/favourites")
            assert f"Updated {marker}" in favourites["items"][0]["track"]["title"]
            stats = plamenu_api.get("/api/v1/music/stats")
            assert f"Updated {marker}" in stats["top"]["tracks"][0]["name"]
            assert stats["top"]["tracks"][0]["url"] == incoming["url"]

            assert favourites["items"][0]["track"]["url"] == incoming["url"]
            assert songhive.activity(result["object_id"])["like_count"] == 0
            saved_resources = plamenu_api.get("/api/v1/music/saved")["items"]
            assert len(saved_resources) == 1
            assert saved_resources[0]["id"] == bookmark["id"]
            assert saved_resources[0]["source"]["url"] == incoming["url"]
            assert f"Updated {marker}" in saved_resources[0]["source"]["title"]
            collected = plamenu_api.get("/api/v1/music/mine?kind=tracks")
            remote_items = [
                item for item in collected["items"] if item["reference"] == reference
            ]
            assert len(remote_items) == 1
            assert f"Updated {marker}" in remote_items[0]["source"]["title"]

        with step(
            "a local reply keeps the remote tombstone available as a thread stub"
        ):
            reply = plamenu_api.post_status(
                f"Discussing Songhive music {marker}",
                in_reply_to_id=incoming["id"],
                visibility="public",
            )
            assert reply["in_reply_to_id"] == incoming["id"]
        with step("Songhive retracts its Audio through its real track deletion"):
            songhive.request("DELETE", f"/api/v1/tracks/{track['id']}")
            wait_for(
                lambda: (
                    (
                        row := plamenu_api._request(
                            "GET",
                            f"/api/v1/statuses/{incoming['id']}",
                            none_on=(404, 410),
                        )
                    )
                    is None
                    or row.get("deleted") is True
                ),
                desc="Songhive Audio Delete in Plamenu",
            )
            queue = plamenu_api.get(f"/api/v1/music/queues/{queue_device}")
            assert queue["entries"] == []
            assert queue["current_entry_id"] is None
            assert queue["position_seconds"] == 0
            assert plamenu_api.get("/api/v1/music/history")["items"] == []
            assert plamenu_api.get("/api/v1/music/favourites")["items"] == []
            collected = plamenu_api.get("/api/v1/music/mine?kind=tracks")
            assert all(item["reference"] != reference for item in collected["items"])
            unavailable = plamenu_api.get("/api/v1/music/saved")["items"]
            assert len(unavailable) == 1
            assert unavailable[0]["id"] == bookmark["id"]
            assert unavailable[0]["source"] is None
            plamenu_api._request("DELETE", f"/api/v1/music/saved/{bookmark['id']}")
            assert plamenu_api.get("/api/v1/music/saved")["items"] == []
            stats = plamenu_api.get("/api/v1/music/stats")
            assert stats["play_count"] == 0
            assert stats["top"]["tracks"] == []
            assert all(hour["count"] == 0 for hour in stats["clock"])
            plamenu_api.delete_status(reply["id"])

        with step("Plamenu deletes its Audio and Songhive retracts it"):
            plamenu_api.delete_status(published["id"])
            wait_for(
                lambda: songhive.activity(published["uri"]) is None,
                desc="Audio Delete at Songhive",
            )
    finally:
        songhive.unfollow(local["uri"])
        plamenu_api.unfollow(remote["id"])


@pytest.mark.federation(direction="both", peer="songhive")
def test_music_library_scoped_follow_without_social_follow(
    songhive, plamenu_api, plamenu_user, marker, tmp_path, db
):
    local = plamenu_api.get("/api/v1/accounts/verify_credentials")
    library = local["uri"] + "/library"
    remote_id = songhive.follow_resource(library)
    remote = plamenu_api.resolve_account(f"sylvia@{SONGHIVE_DOMAIN}")
    path = tmp_path / f"library-{marker}.mp3"
    _audio(path, marker + "library")
    try:
        with step("Songhive follows only the Plamenu music library"):
            wait_for(lambda: songhive.following(library), desc="library Accept")
            assert db.follower_count(plamenu_user.username) == 0
            assert not plamenu_api.relationship(remote["id"])["following"]
        with step("new public Audio reaches the scoped library subscriber"):
            media = plamenu_api.upload_media(
                path.read_bytes(), filename=path.name, mime="audio/mpeg"
            )
            wait_for(
                lambda: plamenu_api.get(f"/api/v1/media/{media['id']}").get("url"),
                desc="processed audio",
            )
            public = plamenu_api._request(
                "POST",
                "/api/v1/statuses",
                json={
                    "status": "Library-scoped recording",
                    "title": marker,
                    "post_kind": "audio",
                    "visibility": "public",
                    "media_ids": [media["id"]],
                },
            )
            received = wait_for(
                lambda: songhive.activity(public["uri"]),
                desc="Audio through library follow",
            )
            assert received
            cached = songhive.lookup(public["uri"])["object"]
            stream = songhive.http.get(
                f"https://{SONGHIVE_DOMAIN}/api/v1/remote/objects/{cached['id']}/stream",
                timeout=60,
            )
            stream.raise_for_status()
            _assert_decoded_audio(stream.content)
        with step("the library contains Audio pages with stable canonical identity"):
            page = songhive.ap_get(library + "?page=1")
            assert page["partOf"] == library
            assert public["uri"] in [item["id"] for item in page["items"]]
        with step(
            "metadata Update and Delete reach the subscriber without account follow"
        ):
            plamenu_api._request(
                "PUT",
                f"/api/v1/statuses/{public['id']}",
                json={
                    "status": "Changed library recording",
                    "title": f"Changed {marker}",
                },
            )
            wait_for(
                lambda: (
                    f"Changed {marker}"
                    in str(songhive.lookup(public["uri"])["object"].get("name"))
                ),
                desc="library Audio Update",
            )
            plamenu_api.delete_status(public["id"])
            wait_for(
                lambda: songhive.activity(public["uri"]) is None,
                desc="library Audio Delete",
            )
        with step("Plamenu follows the Songhive implicit library independently"):
            remote_library = f"https://{SONGHIVE_DOMAIN}/users/sylvia/library"
            plamenu_api._request(
                "POST",
                "/api/v1/music/follows",
                json={"operation": "follow", "url": remote_library},
            )
            wait_for(
                lambda: any(
                    row["target_uri"] == remote_library and not row["pending"]
                    for row in plamenu_api.get("/api/v1/music/follows")
                ),
                desc="Songhive library Accept in Plamenu",
            )
            assert not plamenu_api.relationship(remote["id"])["following"]
            track = songhive.upload(path)
            result = songhive.publish(track["id"])
            # Observe actual delivery in the database before any explicit URL
            # lookup, which would import it and hide a broken subscription.
            wait_for(
                lambda: db.conn.execute(
                    "SELECT id FROM statuses WHERE uri=%s AND deleted_at IS NULL",
                    (result["object_id"],),
                ).fetchone(),
                desc="Songhive Audio delivered through library subscription",
            )
            songhive.request("DELETE", f"/api/v1/tracks/{track['id']}")
            plamenu_api._request(
                "POST",
                "/api/v1/music/follows",
                json={"operation": "unfollow", "url": remote_library},
            )
            assert plamenu_api.get("/api/v1/music/follows") == []
    finally:
        songhive.unfollow_resource(remote_id)


@pytest.mark.federation(
    direction="outbound",
    peer="songhive",
    one_way_reason="Plamenu account erasure and cached music retractions for object-only subscribers",
)
def test_music_account_erasure_retracts_songhive_scoped_cache(
    songhive, plamenu_api, plamenu_user, marker, tmp_path, db
):
    local = plamenu_api.get("/api/v1/accounts/verify_credentials")
    library = local["uri"] + "/library"
    subscriber = songhive.follow_resource(library)
    path = tmp_path / f"erasure-{marker}.mp3"
    _audio(path, marker + "erasure")
    try:
        wait_for(lambda: songhive.following(library), desc="erasure Library Accept")
        assert db.follower_count(plamenu_user.username) == 0
        with step("retain and publish music plus unpublished artist and album notes"):
            asset = plamenu_api._request(
                "POST",
                "/api/v1/music/assets",
                files={"file": (path.name, path.read_bytes(), "audio/mpeg")},
            )
            asset = plamenu_api._request(
                "PUT",
                f"/api/v1/music/assets/{asset['id']}",
                json={
                    "title": marker,
                    "music": asset["music"],
                    "visibility": "public",
                    "revision": asset["revision"],
                },
            )
            artist = plamenu_api._request(
                "POST",
                "/api/v1/music/artists",
                json={
                    "name": f"Empty {marker}",
                    "description": "Unpublished biography",
                },
            )
            plamenu_api._request(
                "POST",
                "/api/v1/music/albums",
                json={
                    "name": f"Empty {marker}",
                    "artist_id": artist["id"],
                    "description": "Unpublished album notes",
                },
            )
            published = plamenu_api._request(
                "POST", f"/api/v1/music/assets/{asset['id']}/publication"
            )
            wait_for(
                lambda: songhive.activity(published["uri"]),
                desc="scoped Audio delivered before any explicit lookup",
            )
            cached = songhive.lookup(published["uri"])["object"]
            assert not cached.get("unavailable_at")
        with step("delete the throwaway account through the authenticated native form"):
            web = requests.Session()
            web.verify = str(config.MASTO_DIR / "ca-bundle.crt")
            login = web.post(
                config.PLAMENU_URL + "/login",
                data={"email": plamenu_user.email, "password": plamenu_user.password},
                allow_redirects=False,
                timeout=30,
            )
            assert login.status_code == 303 and web.cookies
            page = web.get(config.PLAMENU_URL + "/settings/account", timeout=30)
            page.raise_for_status()
            csrf = re.search(r'name="csrf" value="([^"]+)"', page.text)
            assert csrf, "Account settings did not provide a CSRF token"
            result = web.post(
                config.PLAMENU_URL + "/web/settings/account/delete",
                data={
                    "csrf": csrf.group(1),
                    "current_password": plamenu_user.password,
                    "confirm_handle": "@" + plamenu_user.username,
                },
                allow_redirects=False,
                timeout=60,
            )
            assert result.status_code == 303 and result.headers["Location"] == "/"
        with step(
            "source bytes and all music metadata are erased, with cleanup durable"
        ):
            for table in (
                "music_assets",
                "music_artists",
                "music_albums",
                "music_collections",
                "music_artworks",
                "music_radios",
            ):
                # Identifiers come solely from the constant inventory above.
                assert (
                    db.conn.execute(
                        sql.SQL("SELECT count(*) FROM {} WHERE owner_id=%s").format(
                            sql.Identifier(table)
                        ),
                        (int(local["id"]),),
                    ).fetchone()[0]
                    == 0
                )
            account = db.conn.execute(
                "SELECT deleted_at, uri FROM accounts WHERE id=%s", (int(local["id"]),)
            ).fetchone()
            assert account and account[0] and account[1] == local["uri"]
            for key in ("stream_url", "download_url"):
                response = web.get(config.PLAMENU_URL + asset[key], timeout=30)
                assert response.status_code == 404
        with step("Songhive retracts its cached Audio without a refresh lookup"):
            wait_for(
                lambda: songhive.activity(published["uri"]) is None,
                desc="account erasure Audio retraction at object-only Songhive subscriber",
            )
    finally:
        songhive.unfollow_resource(subscriber)


@pytest.mark.federation(
    direction="outbound",
    peer="songhive",
    one_way_reason="Plamenu retained-asset publication and private-byte ACL workflow; bidirectional ordinary Audio and library follows are covered separately",
)
def test_retained_music_named_library_songhive_exchange(
    songhive, plamenu_api, plamenu_user, marker, tmp_path, db
):
    path = tmp_path / f"retained-{marker}.mp3"
    _audio(path, marker + "retained")
    asset = plamenu_api._request(
        "POST",
        "/api/v1/music/assets",
        files={"file": (path.name, path.read_bytes(), "audio/mpeg")},
    )
    library = plamenu_api._request(
        "POST",
        "/api/v1/music/collections",
        json={"name": marker, "kind": "library", "visibility": "public"},
    )
    library_url = f"https://{config.PLAMENU_DOMAIN}/ap/music/libraries/{library['id']}"
    subscriber_id = None
    album_subscriber_id = None
    try:
        with step("private original refuses anonymous download and playback"):
            for key in ("stream_url", "download_url"):
                result = requests.get(
                    f"https://{config.PLAMENU_DOMAIN}" + asset[key],
                    verify=str(config.MASTO_DIR / "ca-bundle.crt"),
                    timeout=30,
                )
                assert result.status_code == 404
        with step("public source joins a named library without automatic publication"):
            asset = plamenu_api._request(
                "PUT",
                f"/api/v1/music/assets/{asset['id']}",
                json={
                    "title": marker,
                    "music": asset["music"],
                    "visibility": "public",
                    "revision": asset["revision"],
                },
            )
            plamenu_api._request(
                "POST",
                f"/api/v1/music/collections/{library['id']}/entries",
                json={"asset_id": asset["id"], "revision": library["revision"]},
            )
            asset = plamenu_api._request(
                "PUT",
                f"/api/v1/music/assets/{asset['id']}/details",
                json={
                    "revision": asset["revision"],
                    "description": f"Retained **track notes** {marker}",
                    "content_type": "text/markdown",
                    "tags": ["interop"],
                    "extra_artists": ["Guest Performer"],
                },
            )
            cover_path = tmp_path / "album-cover.png"
            subprocess.run(
                [
                    "ffmpeg",
                    "-nostdin",
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=c=blue:s=160x120",
                    "-frames:v",
                    "1",
                    "-threads",
                    "1",
                    str(cover_path),
                ],
                check=True,
                timeout=30,
            )
            album = plamenu_api.get(f"/api/v1/music/albums/{asset['album_id']}")
            artwork = plamenu_api._request(
                "POST",
                f"/api/v1/music/albums/{album['id']}/artworks/cover",
                data={"revision": album["revision"]},
                files={"file": (cover_path.name, cover_path.read_bytes(), "image/png")},
            )
            asset = plamenu_api.get(f"/api/v1/music/assets/{asset['id']}")
            assert songhive.ap_get(library_url)["totalItems"] == 0
            subscriber_id = songhive.follow_resource(library_url)
            wait_for(
                lambda: songhive.following(library_url), desc="named Library Accept"
            )
            assert db.follower_count(plamenu_user.username) == 0
        with step(
            "publishing retained Audio reaches the exact named-library subscriber"
        ):
            published = plamenu_api._request(
                "POST",
                f"/api/v1/music/assets/{asset['id']}/publication",
            )
            received = wait_for(
                lambda: songhive.activity(published["uri"]),
                desc="named-library Audio delivery before explicit lookup",
            )
            assert received
            page = songhive.ap_get(library_url + "?page=1")
            assert page["partOf"] == library_url
            assert [item["id"] for item in page["items"]] == [published["uri"]]
            audio = songhive.ap_get(published["uri"])
            for entity in [audio["track"]["artists"][0], audio["track"]["album"]]:
                canonical = songhive.ap_get(entity["id"])
                assert canonical["id"] == entity["id"]
                assert canonical["name"] == entity["name"]
            assert "<strong>track notes</strong>" in audio["content"]
            assert audio["track"]["summary"] == audio["content"]
            assert audio["track"]["tag"][0]["name"] == "#interop"
            assert audio["image"]["url"].endswith(artwork["url"])
            assert audio["track"]["album"]["image"] == audio["image"]
            cached = songhive.lookup(published["uri"])["object"]
            assert cached["image_url"] == audio["image"]["url"]
            cover = requests.get(
                cached["image_url"],
                verify=str(config.MASTO_DIR / "ca-bundle.crt"),
                timeout=30,
            )
            cover.raise_for_status()
            assert cover.headers["Cache-Control"] == "private, no-store"
            decoded = subprocess.run(
                [
                    "ffmpeg",
                    "-nostdin",
                    "-v",
                    "error",
                    "-i",
                    "pipe:0",
                    "-frames:v",
                    "1",
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                    "rgb24",
                    "pipe:1",
                ],
                input=cover.content,
                capture_output=True,
                check=True,
                timeout=30,
            )
            assert len(decoded.stdout) == 160 * 120 * 3
            stream = songhive.http.get(
                f"https://{SONGHIVE_DOMAIN}/api/v1/remote/objects/{cached['id']}/stream",
                timeout=60,
            )
            stream.raise_for_status()
            _assert_decoded_audio(stream.content)
        with step(
            "an album privacy change retracts its scope and updates live Audio credits"
        ):
            album_url = audio["track"]["album"]["id"]
            album_subscriber_id = songhive.follow_resource(album_url)
            wait_for(
                lambda: songhive.following(album_url), desc="canonical Album Accept"
            )
            album = plamenu_api.get(f"/api/v1/music/albums/{asset['album_id']}")
            journal = f"Album journal {marker}"
            album = plamenu_api._request(
                "PUT",
                f"/api/v1/music/albums/{album['id']}",
                json={
                    "name": album["name"],
                    "description": journal,
                    "tags": ["album-journal"],
                    "genres": album["genres"],
                    "release_year": album["release_year"],
                    "musicbrainz_id": album["musicbrainz_id"],
                    "revision": album["revision"],
                },
            )
            wait_for(
                lambda: (
                    (document := _cached_peer_document(songhive, published["uri"]))
                    and document["track"]["album"].get("summary") == journal
                ),
                desc="public album notes reach actual Songhive Audio cache",
            )
            wait_for(
                lambda: (
                    (document := _cached_peer_document(songhive, album_url))
                    and document.get("summary") == journal
                ),
                desc="actual Album Update at Songhive scope",
            )
            album = plamenu_api._request(
                "PUT",
                f"/api/v1/music/albums/{album['id']}/access",
                json={
                    "revision": album["revision"],
                    "visibility": "private",
                    "share_tracks": False,
                },
            )
            assert songhive.ap_unavailable(album_url)
            wait_for(
                lambda: (
                    (document := _cached_peer_document(songhive, published["uri"]))
                    and document["track"]["album"].get("name") == album["name"]
                    and "summary" not in document["track"]["album"]
                    and "tag" not in document["track"]["album"]
                ),
                desc="actual live Audio Update removes private album notes and tags",
            )
            # Songhive 0.4.9 applies inbound Deletes to materialized Activity
            # rows. Plain Album cache rows have no Activity; its guarded refresh
            # marks them gone after confirming the origin's 404/410. Check the
            # live Audio Update before that refresh so it cannot mask a missed
            # inbox update of the embedded credits.
            cached_album = songhive.cached_resource(album_url)
            assert cached_album and not cached_album["materialized"]
            refreshed = songhive.request(
                "GET",
                f"/api/v1/remote/objects/{album_subscriber_id}",
                params={"refresh": "true"},
            )
            assert refreshed["object"]["unavailable"]
            assert songhive.cached_resource(album_url)["unavailable"]
            assert (
                db.conn.execute(
                    "SELECT count(*) FROM music_follows WHERE owner_id=%s AND target_uri=%s",
                    (int(asset["owner_id"]), album_url),
                ).fetchone()[0]
                == 0
            )
            for url in (
                f"https://{config.PLAMENU_DOMAIN}" + asset["stream_url"],
                audio["image"]["url"],
            ):
                response = requests.get(
                    url, verify=str(config.MASTO_DIR / "ca-bundle.crt"), timeout=30
                )
                assert response.status_code == 200
            asset = plamenu_api.get(f"/api/v1/music/assets/{asset['id']}")
        with step("stored metadata edits update the same remote Audio"):
            asset = plamenu_api._request(
                "PUT",
                f"/api/v1/music/assets/{asset['id']}",
                json={
                    "title": f"Updated retained {marker}",
                    "music": asset["music"],
                    "visibility": "public",
                    "revision": asset["revision"],
                },
            )
            wait_for(
                lambda: "Updated retained" in str(songhive.activity(published["uri"])),
                desc="retained Audio Update at Songhive",
            )
            caption = f"A **revised caption** {marker}"
            plamenu_api._request(
                "PUT",
                f"/api/v1/statuses/{published['id']}",
                json={"status": caption, "content_type": "text/markdown"},
            )
            asset = plamenu_api.get(f"/api/v1/music/assets/{asset['id']}")
            assert asset["description"] == caption
            assert asset["description_type"] == "text/markdown"
            wait_for(
                lambda: "revised caption" in str(songhive.activity(published["uri"])),
                desc="same canonical Audio receives the reverse caption edit",
            )
        with step(
            "withdrawal keeps original bytes and republishing uses a new identity"
        ):
            plamenu_api._request(
                "DELETE", f"/api/v1/music/assets/{asset['id']}/publication"
            )
            wait_for(
                lambda: songhive.activity(published["uri"]) is None,
                desc="named-library Audio withdrawal",
            )
            assert (
                plamenu_api.get(f"/api/v1/music/assets/{asset['id']}")["id"]
                == asset["id"]
            )
            again = plamenu_api._request(
                "POST", f"/api/v1/music/assets/{asset['id']}/publication"
            )
            assert again["uri"] != published["uri"]
            wait_for(
                lambda: songhive.activity(again["uri"]),
                desc="republished named-library Audio",
            )
        with step(
            "making source private retracts remote Audio and revokes bytes immediately"
        ):
            asset = plamenu_api._request(
                "PUT",
                f"/api/v1/music/assets/{asset['id']}",
                json={
                    "title": asset["title"],
                    "music": asset["music"],
                    "visibility": "private",
                    "revision": asset["revision"],
                },
            )
            wait_for(
                lambda: songhive.activity(again["uri"]) is None,
                desc="private retained source retraction",
            )
            assert (
                plamenu_api.ap_get(f"/ap/music/libraries/{library['id']}")["totalItems"]
                == 0
            )
            response = requests.get(
                f"https://{config.PLAMENU_DOMAIN}" + asset["stream_url"],
                verify=str(config.MASTO_DIR / "ca-bundle.crt"),
                timeout=30,
            )
            assert response.status_code == 404
            cover = requests.get(
                audio["image"]["url"],
                verify=str(config.MASTO_DIR / "ca-bundle.crt"),
                timeout=30,
            )
            assert cover.status_code == 404, (
                "A cached remote artwork URL cannot bypass source revocation"
            )
        with step(
            "explicit collection sharing serves owned private bytes without republishing"
        ):
            current = plamenu_api.get(f"/api/v1/music/collections/{library['id']}")
            enabled = plamenu_api._request(
                "PUT",
                f"/api/v1/music/collections/{library['id']}/contents-sharing",
                json={"revision": current["revision"], "share_tracks": True},
            )
            assert enabled["share_tracks"] is True
            stream = f"https://{config.PLAMENU_DOMAIN}" + asset["stream_url"]
            shared = requests.get(
                stream,
                headers={"Range": "bytes=0-31"},
                verify=str(config.MASTO_DIR / "ca-bundle.crt"),
                timeout=30,
            )
            assert shared.status_code == 206 and len(shared.content) == 32
            assert shared.headers["Cache-Control"] == "private, no-store"
            assert (
                requests.get(
                    audio["image"]["url"],
                    verify=str(config.MASTO_DIR / "ca-bundle.crt"),
                    timeout=30,
                ).status_code
                == 200
            )
            assert songhive.ap_get(library_url)["totalItems"] == 0
            assert songhive.activity(again["uri"]) is None
            assert (
                plamenu_api.get(f"/api/v1/music/assets/{asset['id']}")["visibility"]
                == "private"
            )
            plamenu_api._request(
                "PUT",
                f"/api/v1/music/collections/{library['id']}/contents-sharing",
                json={"revision": enabled["revision"], "share_tracks": False},
            )
            assert (
                requests.get(
                    stream,
                    headers={"Range": "bytes=0-31"},
                    verify=str(config.MASTO_DIR / "ca-bundle.crt"),
                    timeout=30,
                ).status_code
                == 404
            )
            assert (
                requests.get(
                    audio["image"]["url"],
                    verify=str(config.MASTO_DIR / "ca-bundle.crt"),
                    timeout=30,
                ).status_code
                == 404
            )
    finally:
        if subscriber_id is not None:
            songhive.unfollow_resource(subscriber_id)
        if album_subscriber_id is not None:
            songhive.unfollow_resource(album_subscriber_id)
        latest = plamenu_api.get(f"/api/v1/music/collections/{library['id']}")
        plamenu_api._request(
            "DELETE",
            f"/api/v1/music/collections/{library['id']}",
            json={"revision": latest["revision"]},
        )
        plamenu_api._request("DELETE", f"/api/v1/music/assets/{asset['id']}")


@pytest.mark.federation(direction="incoming", peer="songhive")
def test_songhive_unpublished_public_album_native_playback(
    songhive, plamenu_api, tmp_path
):
    """A listener opens public catalogue audio even when no AP post exists."""
    from plamenu_e2e.songhive import SONGHIVE_URL

    path = tmp_path / "unpublished-catalogue.mp3"
    _audio(path, "Unpublished catalogue acceptance")
    track = songhive.upload(path)
    try:
        detail = songhive.request(
            "GET",
            f"/api/v1/tracks/{track['id']}",
            params={"include": "artist,album,owner"},
        )
        assert detail["album_id"]
        # Do not publish or Follow: this is the anonymous catalogue journey.
        ap = requests.get(
            f"{SONGHIVE_URL}/tracks/{track['id']}",
            headers={"Accept": "application/activity+json"},
            verify=str(config.MASTO_DIR / "ca-bundle.crt"),
            timeout=30,
        )
        assert ap.status_code == 404
        album = plamenu_api._request(
            "POST",
            "/api/v1/music/remote/import",
            json={"url": f"{SONGHIVE_URL}/albums/{detail['album_id']}"},
        )
        contents = plamenu_api.get(f"/api/v1/music/remote/{album['id']}/contents")
        item = next(
            item
            for item in contents["items"]
            if item["uri"] == f"{SONGHIVE_URL}/api/v1/tracks/{track['id']}"
        )
        stream = plamenu_api.http.get(
            config.PLAMENU_URL + item["stream_url"], timeout=30
        )
        stream.raise_for_status()
        _assert_decoded_audio(stream.content)
        songhive.request(
            "PATCH",
            f"/api/v1/tracks/{track['id']}",
            json={"visibility": "private"},
        )
        denied = plamenu_api.http.get(
            config.PLAMENU_URL + item["stream_url"], timeout=30
        )
        assert denied.status_code in (403, 404)
    finally:
        songhive.request("DELETE", f"/api/v1/tracks/{track['id']}")
