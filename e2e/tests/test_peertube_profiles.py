"""PeerTube channel profiles and ordinary mentions use the real actor format."""

import tempfile
from pathlib import Path
from urllib.parse import urlsplit

import pytest
from plamenu_e2e import ephemeral, peertube, unique
from plamenu_e2e.steps import step, wait_for

pytestmark = pytest.mark.requires_peer("peertube")


@pytest.mark.federation(
    direction="inbound",
    peer="peertube",
    one_way_reason="PeerTube's untyped Markdown actor summary and Group mention identity are import contracts; native channel publication has separate outbound coverage",
)
def test_peertube_channel_profile_and_mentions_work_without_javascript():
    pt = peertube.root()
    instance = ephemeral.Instance()
    name = unique("profile").replace("-", "_")
    created = False
    try:
        with step(
            "create a disposable PeerTube channel with an ampersand and Markdown"
        ):
            pt.post(
                "/api/v1/video-channels",
                name=name,
                displayName="Forum Demos & Keynotes",
                description=f"Demos **and keynotes**. Each event has [its own playlist](https://peertube.local/c/{name}/video-playlists).",
            )
            created = True
            channel = pt.get(f"/api/v1/video-channels/{name}")
            wait_for(
                lambda: (
                    pt._request(
                        "GET",
                        f"/video-channels/{name}",
                        headers={"Accept": "application/activity+json"},
                    )
                    .get("publicKey", {})
                    .get("publicKeyPem")
                ),
                desc="PeerTube's asynchronous channel key generation",
            )

        with step("mention the previously unknown channel on an isolated Plamenu"):
            instance.start()
            _, api = instance.user_api("channeltester")
            sender_uri = api.get("/api/v1/accounts/verify_credentials")["uri"]
            handle = f"{name}@peertube.local"
            posted = api.post_status(f"Mention @{handle}")
            assert len(posted["mentions"]) == 1, posted
            assert posted["mentions"][0]["acct"] == handle, posted
            assert (
                f'href="https://peertube.local/c/{name}/videos"' in posted["content"]
            ), posted
            account = api.account(posted["mentions"][0]["id"])
            assert account["uri"] == channel["url"], account
            assert account["group"] is True, account
            assert account["display_name"] == "Forum Demos & Keynotes", account
            assert "<strong>and keynotes</strong>" in account["note"], account
            assert (
                f'href="https://peertube.local/c/{name}/video-playlists"'
                in account["note"]
            ), account

        with step("the no-JS profile renders the same name and description"):
            response = api.http.get(f"{instance.url}/!{handle}", timeout=30)
            response.raise_for_status()
            assert "Forum Demos &amp; Keynotes" in response.text
            assert "Forum Demos &amp;amp; Keynotes" not in response.text
            assert "<strong>and keynotes</strong>" in response.text
            assert (
                f'href="https://peertube.local/c/{name}/video-playlists"'
                in response.text
            )

        with step(
            "editing the mention keeps it an ordinary Note with its actual author"
        ):
            edited = api.put(
                f"/api/v1/statuses/{posted['id']}", status=f"Edited @{handle}"
            )
            assert len(edited["mentions"]) == 1, edited
            obj = api.ap_get(urlsplit(edited["uri"]).path)
            assert obj["type"] == "Note", obj
            assert obj["attributedTo"] == sender_uri, obj
            assert obj["attributedTo"] != channel["url"], obj
            assert any(
                tag["type"] == "Mention" and tag["href"] == channel["url"]
                for tag in obj["tag"]
            ), obj
    finally:
        instance.stop()
        if created:
            pt.delete(f"/api/v1/video-channels/{name}")


@pytest.mark.federation(
    direction="inbound",
    peer="peertube",
    one_way_reason="A cached channel profile must include verified Videos without receiving channel Announce wrappers; native channel publication has separate outbound coverage",
)
def test_peertube_channel_profile_lists_videos_imported_without_announces():
    pt = peertube.root()
    instance = ephemeral.Instance()
    name = unique("library").replace("-", "_")
    created = False
    videos = []
    try:
        with step("publish two Videos in a disposable PeerTube channel"):
            pt.post("/api/v1/video-channels", name=name, displayName="Library fixture")
            created = True
            channel = pt.get(f"/api/v1/video-channels/{name}")
            wait_for(
                lambda: (
                    pt._request(
                        "GET",
                        f"/video-channels/{name}",
                        headers={"Accept": "application/activity+json"},
                    )
                    .get("publicKey", {})
                    .get("publicKeyPem")
                ),
                desc="PeerTube channel key generation",
            )
            with tempfile.TemporaryDirectory() as directory:
                sample = peertube.make_sample_mp4(Path(directory) / "film.mp4")
                for index in range(2):
                    video = pt.upload_video(
                        sample, f"{name} film {index}", channel_id=channel["id"]
                    )
                    videos.append(video)
                    pt.wait_for_hls(video["uuid"])

        with step("import original Videos without following their channel"):
            instance.start()
            _, api = instance.user_api("librarytester")
            statuses = [api.resolve_status(pt.ap_id(video["uuid"])) for video in videos]
            assert all(statuses), statuses
            channel_account = api.resolve_account(f"{name}@peertube.local")
            channel_id = channel_account["id"]
            video_ids = {status["id"] for status in statuses}
            author_id = statuses[0]["account"]["id"]
            assert author_id != channel_id
            assert all(status["account"]["id"] == author_id for status in statuses)
            wrappers = instance.db.conn.execute(
                "SELECT count(*) FROM statuses WHERE account_id=%s AND reblog_of_id=ANY(%s)",
                (int(channel_id), [int(value) for value in video_ids]),
            ).fetchone()[0]
            assert wrappers == 0, (
                "this regression requires no imported Announce wrappers"
            )

        with step("uploader and channel profiles expose the same cached publications"):
            uploader = api.get(f"/api/v1/accounts/{author_id}/statuses")
            assert video_ids <= {status["id"] for status in uploader}
            for params in ({}, {"only_media": "true", "exclude_reblogs": "true"}):
                library = api.get(f"/api/v1/accounts/{channel_id}/statuses", **params)
                assert len(library) == 2, library
                assert {status["id"] for status in library} == video_ids, library
                assert all(status["reblog"] is None for status in library)
                assert all(status["account"]["id"] == author_id for status in library)
                assert all(
                    status["video_channels"][0]["id"] == channel_id
                    for status in library
                )
            first = api.get(f"/api/v1/accounts/{channel_id}/statuses", limit=1)
            second = api.get(
                f"/api/v1/accounts/{channel_id}/statuses",
                max_id=first[0]["id"],
                limit=1,
            )
            assert {first[0]["id"], second[0]["id"]} == video_ids
            for suffix in ("", "/media"):
                response = api.http.get(
                    f"{instance.url}/!{name}@peertube.local{suffix}", timeout=30
                )
                assert response.status_code == 200, response.status_code
                for video_id in video_ids:
                    if suffix:
                        assert f'#post-{video_id}"' in response.text
                    else:
                        assert response.text.count(f'id="post-{video_id}"') == 1
    finally:
        instance.stop()
        for video in videos:
            pt.delete_video(video["uuid"])
        if created:
            pt.delete(f"/api/v1/video-channels/{name}")
