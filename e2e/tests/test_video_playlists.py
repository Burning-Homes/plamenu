"""Ordered, clipped Video collections against actual PeerTube 8.3."""
import tempfile
from pathlib import Path

import pytest
import requests
from plamenu_e2e import config, peertube, plamenu, unique
from plamenu_e2e.steps import wait_for


def change(api, playlist, operation, **fields):
    result = api.post(f"/api/v1/video_playlists/{playlist['id']}",
                     operation=operation, revision=playlist["revision"], **fields)
    return result if operation == "delete" else api.get(f"/api/v1/video_playlists/{playlist['id']}")


def test_playlist_api_scopes_conflicts_and_no_js_csrf_confirmation(plamenu_api, plamenu_user, cli):
    import re
    name = unique("playlistweb")[:30]
    channel = plamenu_api.post("/api/v1/video_channels", username=name, display_name="Web playlists")
    playlist = plamenu_api.post("/api/v1/video_playlists", channel_id=channel["id"], name="Private HTML library")
    bob = plamenu.User()
    cli.account_add(bob)
    bob_api = plamenu.login(bob)
    read_only = plamenu.login(plamenu_user, scopes="read:statuses")
    path = f"/api/v1/video_playlists/{playlist['id']}"
    try:
        catalogue = plamenu_api.http.get(f"{config.PLAMENU_URL}/api/v1/video_playlists?mine=true", timeout=30)
        assert catalogue.headers["Cache-Control"] == "private, no-store"
        assert {"authorization", "cookie"} <= {v.strip().lower() for v in catalogue.headers["Vary"].split(",")}
        assert bob_api.http.get(f"{config.PLAMENU_URL}{path}", timeout=30).status_code == 404
        denied = read_only.http.post(f"{config.PLAMENU_URL}{path}", data={"operation": "profile", "revision": playlist["revision"], "name": "Denied"}, timeout=30)
        assert denied.status_code == 403
        assert read_only.get(path)["name"] == "Private HTML library"
        changed = change(plamenu_api, playlist, "profile", description="Accessible HTML-first library")
        stale = plamenu_api.http.post(f"{config.PLAMENU_URL}{path}", data={"operation": "profile", "revision": playlist["revision"], "name": "Stale"}, timeout=30)
        assert stale.status_code == 409
        session = requests.Session()
        session.verify = False
        session.post(f"{config.PLAMENU_URL}/login", data={"email": plamenu_user.email, "password": plamenu_user.password}, timeout=30)
        page = session.get(f"{config.PLAMENU_URL}/videos/playlists/{playlist['id']}", timeout=30)
        page.raise_for_status()
        assert page.headers["Cache-Control"] == "private, no-store" and page.headers["Vary"] == "Cookie"
        csrf = re.search(r'name="csrf" value="([^"]+)"', page.text)[1]
        endpoint = f"{config.PLAMENU_URL}/web/video-playlists/{playlist['id']}"
        fields = {"operation": "delete", "revision": changed["revision"], "confirm": changed["name"]}
        assert session.post(endpoint, data=fields, timeout=30).status_code == 403
        fields["csrf"] = csrf
        fields["confirm"] = "Incorrect title"
        response = session.post(endpoint, data=fields, timeout=30)
        assert response.status_code == 422 and 'role="alert"' in response.text
        assert plamenu_api.get(path)["id"] == playlist["id"]
        fields["confirm"] = changed["name"]
        assert session.post(endpoint, data=fields, allow_redirects=False, timeout=30).status_code == 303
        assert session.get(f"{config.PLAMENU_URL}/videos/playlists/{playlist['id']}", timeout=30).status_code == 404
    finally:
        latest = plamenu_api.http.get(f"{config.PLAMENU_URL}{path}", timeout=30)
        if latest.status_code == 200:
            latest = latest.json()
            change(plamenu_api, latest, "delete", confirm=latest["name"])
        plamenu_api.post(f"/api/v1/video_channels/{channel['id']}", operation="delete", confirm=name)


@pytest.mark.skipif(not peertube.reachable(), reason="peertube.local is not up")
@pytest.mark.federation(direction="both", peer="peertube")
def test_native_playlist_round_trip_duplicates_clips_reorder_visibility_delete(plamenu_api, plamenu_user, cli, db):
    pt = peertube.root()
    name = unique("playlist")[:30]
    channel = plamenu_api.post("/api/v1/video_channels", username=name, display_name="Playlist films")
    handle = f"{name}@{config.PLAMENU_DOMAIN}"
    playlist = None
    deleted = False
    active_api = plamenu_api
    pt.post("/api/v1/users/me/subscriptions", uri=handle)
    try:
        wait_for(lambda: db.follower_count(name) > 0, desc="PeerTube follows playlist channel")
        with tempfile.TemporaryDirectory() as directory:
            sample = peertube.make_sample_mp4(Path(directory) / "clip.mp4", seconds=5)
            media = plamenu_api.upload_media(sample.read_bytes(), filename="clip.mp4", mime="video/mp4",
                                            description="Moving test pattern with a steady audible tone")
        video = plamenu_api.post("/api/v1/videos", channel_id=channel["id"], media_id=media["id"], title=f"Film {name}")
        playlist = plamenu_api.post("/api/v1/video_playlists", channel_id=channel["id"], name=f"Clips {name}", visibility="public")
        def imported():
            data = pt.get("/api/v1/search/video-playlists", search=playlist["uri"])["data"]
            return data[0] if data else None
        remote = wait_for(imported, desc="PeerTube imports Plamenu Playlist collection")
        def videos():
            # PeerTube intentionally refuses numeric IDs for unlisted
            # collections; use its stable public/unlisted UUID route.
            return pt.get(f"/api/v1/video-playlists/{remote['uuid']}/videos", count=100)["data"]
        def first_start(seconds):
            items = videos()
            return bool(items) and items[0]["startTimestamp"] == seconds
        playlist = change(plamenu_api, playlist, "add", status_id=video["id"], start_seconds=1, stop_seconds=2)
        wait_for(lambda: len(videos()) == 1, desc="PeerTube receives first clipped entry")
        playlist = change(plamenu_api, playlist, "add", status_id=video["id"], start_seconds=3, stop_seconds=4)
        entries = playlist["entries"]
        received = wait_for(lambda: videos() if len(videos()) == 2 else None, desc="PeerTube preserves duplicate Video occurrences")
        assert [v["startTimestamp"] for v in received] == [1, 3]
        assert [v["stopTimestamp"] for v in received] == [2, 4]
        assert received[0]["video"]["id"] == received[1]["video"]["id"]
        playlist = change(plamenu_api, playlist, "move", entry_id=entries[1]["id"], position=1)
        wait_for(lambda: first_start(3), desc="PeerTube receives stable entry reorder")
        assert playlist["entries"][0]["id"] == entries[1]["id"]
        playlist = change(plamenu_api, playlist, "clip", entry_id=entries[1]["id"], start_seconds=2, stop_seconds=3)
        wait_for(lambda: first_start(2), desc="PeerTube receives clip Update")
        original_uri = playlist["uri"]
        playlist = change(plamenu_api, playlist, "profile", name=f"Revised {name}")
        wait_for(lambda: pt.get(f"/api/v1/video-playlists/{remote['uuid']}")["displayName"] == f"Revised {name}", desc="PeerTube receives playlist profile Update")
        # Reading PeerTube's imported collection is the round trip: redirects
        # retain Plamenu's canonical Playlist and stable element/video identities.
        remote_detail = pt.get(f"/api/v1/video-playlists/{remote['uuid']}")
        assert remote_detail["url"] == original_uri
        assert remote_detail["ownerAccount"]["name"] == video["account"]["username"]
        playlist = change(plamenu_api, playlist, "profile", visibility="private")
        wait_for(lambda: pt.http.get(f"{pt.base_url}/api/v1/video-playlists/{remote['uuid']}", timeout=30).status_code == 404,
                 desc="Private transition retracts remote Playlist")
        assert requests.get(f"{config.PLAMENU_URL}/api/v1/video_playlists/{playlist['id']}", verify=False, timeout=30).status_code == 404
        assert requests.get(original_uri, headers={"Accept": "application/activity+json"}, verify=False, timeout=30).status_code == 404
        playlist = change(plamenu_api, playlist, "profile", visibility="unlisted")
        assert playlist["uri"] == original_uri
        remote = wait_for(imported, desc="PeerTube imports newly shared unlisted Playlist")
        assert remote["privacy"]["id"] == 2
        playlist = change(plamenu_api, playlist, "remove", entry_id=entries[0]["id"])
        wait_for(lambda: len(videos()) == 1, desc="PeerTube receives entry removal")
        # Ownership is channel authority, not the original collection creator.
        bob = plamenu.User()
        cli.account_add(bob)
        bob_api = plamenu.login(bob)
        plamenu_api.post(f"/api/v1/video_channels/{channel['id']}", operation="transfer", username=bob.username)
        bob_api.post(f"/api/v1/video_channels/{channel['id']}", operation="accept_transfer", confirm=name)
        active_api = bob_api
        wait_for(lambda: pt.get(f"/api/v1/video-playlists/{remote['uuid']}")["ownerAccount"]["name"] == bob.username,
                 desc="PeerTube transfers existing Playlist to accepted channel owner")
        denied = plamenu_api.http.post(f"{config.PLAMENU_URL}/api/v1/video_playlists/{playlist['id']}",
                                      data={"operation": "profile", "revision": playlist["revision"], "name": "Former owner"}, timeout=30)
        assert denied.status_code == 404
        playlist = change(bob_api, playlist, "profile", description="Managed after handoff")
        assert playlist["uri"] == original_uri
        wait_for(lambda: pt.get(f"/api/v1/video-playlists/{remote['uuid']}")["description"] == "Managed after handoff",
                 desc="PeerTube accepts new owner's signed Playlist Update")
        change(bob_api, playlist, "delete", confirm=playlist["name"])
        deleted = True
        wait_for(lambda: pt.http.get(f"{pt.base_url}/api/v1/video-playlists/{remote['uuid']}", timeout=30).status_code == 404,
                 desc="PeerTube receives final Playlist Delete")
        assert plamenu_api.get_status(video["id"])["id"] == video["id"], "Deleting a Playlist does not delete its Videos"
        # Standalone deletion, not channel/account cleanup: the original
        # uploader and current channel owner are now different Persons.
        bob_api.delete(f"/api/v1/statuses/{video['id']}")
        wait_for(lambda: pt.http.get(f"{pt.base_url}/api/v1/videos/{received[0]['video']['uuid']}", timeout=30).status_code == 404,
                 desc="PeerTube accepts current owner's standalone Video Delete after handoff")
        assert bob_api.http.get(f"{config.PLAMENU_URL}/api/v1/statuses/{video['id']}", timeout=30).status_code == 404
    finally:
        if playlist and not deleted:
            latest = active_api.get(f"/api/v1/video_playlists/{playlist['id']}")
            change(active_api, latest, "delete", confirm=latest["name"])
        pt.http.delete(f"{pt.base_url}/api/v1/users/me/subscriptions/{handle}", timeout=30)
        active_api.post(f"/api/v1/video_channels/{channel['id']}", operation="delete", confirm=name)


@pytest.mark.skipif(not peertube.reachable(), reason="peertube.local is not up")
@pytest.mark.federation(direction="both", peer="peertube")
def test_peertube_playlist_import_signed_updates_and_delete(plamenu_api, plamenu_user, db):
    pt = peertube.root()
    title = unique("ptplaylist")
    created_video = None
    created_playlist = None
    owner = plamenu_api.resolve_account("root@peertube.local")
    plamenu_api.follow(owner["id"])
    try:
        wait_for(lambda: plamenu_api.relationship(owner["id"])["following"], desc="Plamenu follows PeerTube playlist owner")
        with tempfile.TemporaryDirectory() as directory:
            sample = peertube.make_sample_mp4(Path(directory) / "playlist.mp4", seconds=5)
            created_video = pt.upload_video(sample, title)
        pt.wait_for_hls(created_video["uuid"])
        created_playlist = pt.post("/api/v1/video-playlists", displayName=title, privacy=1,
                                   videoChannelId=pt.default_channel_id())["videoPlaylist"]
        url = f"{pt.base_url}/video-playlists/{created_playlist['uuid']}"
        first = pt.post(f"/api/v1/video-playlists/{created_playlist['id']}/videos", videoId=created_video["id"], startTimestamp=1, stopTimestamp=2)["videoPlaylistElement"]
        second = pt.post(f"/api/v1/video-playlists/{created_playlist['id']}/videos", videoId=created_video["id"], startTimestamp=3, stopTimestamp=4)["videoPlaylistElement"]
        imported = plamenu_api.post("/api/v1/video_playlists/import", url=url)
        assert imported["name"] == title and len(imported["entries"]) == 2
        entries = imported["entries"]
        assert [e["start_seconds"] for e in entries] == [1, 3]
        assert [e["stop_seconds"] for e in entries] == [2, 4]
        assert imported["can_edit"] is False and imported["remote"] is True
        # The complete remote import/refresh is also a normal HTML POST, with
        # no script or JSON client required and the same canonical identity.
        import re
        session = requests.Session()
        session.verify = False
        session.post(f"{config.PLAMENU_URL}/login", data={"email": plamenu_user.email, "password": plamenu_user.password}, timeout=30)
        library = session.get(f"{config.PLAMENU_URL}/videos/playlists", timeout=30)
        library.raise_for_status()
        csrf = re.search(r'name="csrf" value="([^"]+)"', library.text)[1]
        endpoint = f"{config.PLAMENU_URL}/web/video-playlists/import"
        assert session.post(endpoint, data={"url": url}, timeout=30).status_code == 403
        refreshed = session.post(endpoint, data={"csrf": csrf, "url": url}, allow_redirects=False, timeout=60)
        assert refreshed.status_code == 303 and refreshed.headers["Location"].endswith(f"/videos/playlists/{imported['id']}")
        def read():
            return plamenu_api.get(f"/api/v1/video_playlists/{imported['id']}")
        pt.post(f"/api/v1/video-playlists/{created_playlist['id']}/videos/reorder", startPosition=2, insertAfterPosition=0)
        wait_for(lambda: read()["entries"][0]["start_seconds"] == 3, desc="Signed PeerTube Playlist Update reorders entries in Plamenu")
        assert read()["entries"][0]["id"] == entries[1]["id"]
        pt.put(f"/api/v1/video-playlists/{created_playlist['id']}/videos/{second['id']}", startTimestamp=2, stopTimestamp=3)
        wait_for(lambda: read()["entries"][0]["start_seconds"] == 2, desc="Signed PeerTube clip Update retains native entry id")
        pt.put(f"/api/v1/video-playlists/{created_playlist['id']}", displayName=f"Updated {title}")
        wait_for(lambda: read()["name"] == f"Updated {title}", desc="PeerTube playlist profile Update")
        html = requests.get(f"{config.PLAMENU_URL}/videos/playlists/{imported['id']}?entry={entries[1]['id']}", verify=False, timeout=60)
        html.raise_for_status()
        assert '#t=2,3' in html.text and '<video' in html.text and ' controls' in html.text
        resolved = read()["entries"][0]
        assert resolved["status_id"] and resolved["available"]
        # Add the imported PeerTube Video to a native collection: a canonical
        # source reference, not a copied/re-encoded media file.
        name = unique("ptlibrary")[:30]
        channel = plamenu_api.post("/api/v1/video_channels", username=name, display_name="Imported films")
        local = plamenu_api.post("/api/v1/video_playlists", channel_id=channel["id"], name="Remote film clips", visibility="unlisted")
        local = change(plamenu_api, local, "add", status_id=resolved["status_id"], start_seconds=1, stop_seconds=2)
        assert local["entries"][0]["video_uri"] == pt.ap_id(created_video["uuid"])
        change(plamenu_api, local, "delete", confirm=local["name"])
        plamenu_api.post(f"/api/v1/video_channels/{channel['id']}", operation="delete", confirm=name)
        pt.delete(f"/api/v1/videos/{created_video['uuid']}")
        created_video = None
        wait_for(lambda: not read()["entries"][0]["available"], desc="Deleted PeerTube Video becomes a stable unavailable Playlist entry")
        unavailable = read()["entries"][0]
        assert unavailable["id"] == entries[1]["id"] and unavailable["video_uri"] is None
        missing = requests.get(f"{config.PLAMENU_URL}/videos/playlists/{imported['id']}?entry={entries[1]['id']}", verify=False, timeout=60)
        missing.raise_for_status()
        assert 'Video unavailable or restricted' in missing.text and '<video' not in missing.text
        pt.delete(f"/api/v1/video-playlists/{created_playlist['id']}")
        created_playlist = None
        wait_for(lambda: plamenu_api.http.get(f"{config.PLAMENU_URL}/api/v1/video_playlists/{imported['id']}", timeout=30).status_code == 404,
                 desc="Signed PeerTube Playlist Delete retracts imported library")
        assert db._value("SELECT count(*) FROM video_playlist_entries WHERE playlist_id=%s", int(imported["id"])) == 0
    finally:
        plamenu_api.unfollow(owner["id"])
        if created_playlist:
            pt.delete(f"/api/v1/video-playlists/{created_playlist['id']}")
        if created_video:
            pt.delete(f"/api/v1/videos/{created_video['uuid']}")
