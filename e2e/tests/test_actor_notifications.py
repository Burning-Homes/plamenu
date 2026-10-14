"""Real PeerTube interactions reach people with explicit channel context."""

import contextlib
import tempfile
from pathlib import Path

import pytest
import requests
from plamenu_e2e import ephemeral, peertube, unique
from plamenu_e2e.steps import wait_for


def events(api, kind, status=None):
    return [
        item
        for item in api.get("/api/v1/notifications")
        if item["type"] == f"plamenu:actor.{kind}"
        and (status is None or item.get("status", {}).get("id") == str(status))
    ]


@pytest.mark.skipif(not peertube.reachable(), reason="peertube.local is not up")
@pytest.mark.federation(direction="both", peer="peertube")
def test_peertube_channel_notifications_have_context_and_do_not_fabricate_mentions():
    # The harness owns this disposable endpoint and database; standing servers
    # are neither reset nor used as an accidentally stale candidate.
    peer = ephemeral.Instance(conversation_containers=True)
    pt = peertube.root()
    name = unique("actorfilm")[:30]
    handle = f"{name}@{peer.domain}"
    subscribed = False
    video = None
    try:
        peer.start()
        peer.apply_suite_settings()
        _owner, api = peer.user_api("actorowner")
        editor, editor_api = peer.user_api("actoreditor")
        publisher, publisher_api = peer.user_api("actorpublisher")
        pending, pending_api = peer.user_api("actorpending")
        channel = api.post(
            "/api/v1/video_channels", username=name, display_name="Team films"
        )
        path = f"/api/v1/video_channels/{channel['id']}"
        for person, client, role, accepted in [
            (editor, editor_api, "editor", True),
            (publisher, publisher_api, "publisher", True),
            (pending, pending_api, "editor", False),
        ]:
            api.post(path, operation="invite", username=person.username, role=role)
            assert len(events(client, "invitation")) == 1
            if accepted:
                client.post(path, operation="accept")
                assert not events(client, "invitation")
        pt.post("/api/v1/users/me/subscriptions", uri=handle)
        subscribed = True
        wait_for(lambda: events(api, "follow"), desc="channel Follow reaches its owner")
        wait_for(
            lambda: events(editor_api, "follow"),
            desc="channel Follow reaches accepted editor",
        )
        assert not events(publisher_api, "follow")
        assert not events(pending_api, "follow")
        for client in [api, editor_api]:
            event = events(client, "follow")
            assert len(event) == 1
            assert event[0]["actor_contexts"][0]["actor"]["id"] == channel["id"]
            assert "Team films" in event[0]["fallback"]["title"]

        with tempfile.TemporaryDirectory() as directory:
            sample = peertube.make_sample_mp4(Path(directory) / "sample.mp4", seconds=2)
            media = api.upload_media(
                sample.read_bytes(), filename="sample.mp4", mime="video/mp4"
            )
        title = f"Actor notification {name}"
        video = api.post(
            "/api/v1/videos",
            channel_id=channel["id"],
            media_id=media["id"],
            title=title,
            description="Local notification regression",
        )

        def imported():
            return next(
                (
                    v
                    for v in pt.get(
                        "/api/v1/users/me/subscriptions/videos",
                        count=100,
                        sort="-publishedAt",
                        nsfw="both",
                    )["data"]
                    if v["name"] == title
                ),
                None,
            )

        remote = wait_for(imported, desc="native Video delivered to PeerTube")
        uuid = remote["uuid"]
        comment = pt.post(
            f"/api/v1/videos/{uuid}/comment-threads",
            text=f"Untagged channel comment {name}",
        )["comment"]
        uri = f"{pt.base_url}/videos/watch/{uuid}/comments/{comment['id']}"
        wire = requests.get(
            uri,
            headers={"Accept": "application/activity+json"},
            verify=False,
            timeout=30,
        )
        wire.raise_for_status()
        assert not any(
            tag.get("type") == "Mention" for tag in wire.json().get("tag", [])
        )
        event = wait_for(
            lambda: events(api, "comment"),
            desc="untagged PeerTube comment notifies video channel owner",
        )
        reply_id = event[0]["status"]["id"]
        assert event[0]["status"]["in_reply_to_id"] == video["id"]
        assert not event[0]["status"]["mentions"]
        wait_for(
            lambda: events(editor_api, "comment", reply_id),
            desc="PeerTube comment reaches accepted editor",
        )
        assert not events(publisher_api, "comment")
        assert not events(pending_api, "comment")
        grouped = api.get("/api/v2/notifications", **{"supported_types[]": "mention"})
        group = next(
            g for g in grouped["notification_groups"] if g.get("status_id") == reply_id
        )
        assert group["type"] == "plamenu:actor.comment"
        assert group["fallback"] == event[0]["fallback"]

        # True Mention + channel comment remains one event for the owner.
        owner_reply = api.post_status(
            f"@root@peertube.local Owner response {name}", in_reply_to_id=reply_id
        )

        def owner_comment_arrived():
            tree = pt.get(f"/api/v1/videos/{uuid}/comment-threads/{comment['id']}")

            def search(node):
                value = node.get("comment", {})
                if value.get("url") == owner_reply["uri"]:
                    return value
                for child in node.get("children", []):
                    found = search(child)
                    if found:
                        return found
                return None

            return search(tree)

        imported_owner = wait_for(
            owner_comment_arrived, desc="owner reply reaches PeerTube"
        )
        nested = pt.post(
            f"/api/v1/videos/{uuid}/comments/{imported_owner['id']}",
            text=f"Nested reply to owner {name}",
        )["comment"]
        nested_uri = f"{pt.base_url}/videos/watch/{uuid}/comments/{nested['id']}"
        nested_events = wait_for(
            lambda: [
                n for n in events(api, "comment") if n["status"]["uri"] == nested_uri
            ],
            desc="tagged nested reply coalesces with channel notification",
        )
        assert len(nested_events) == 1
        assert (
            len(
                [
                    n
                    for n in api.get("/api/v1/notifications")
                    if n.get("status", {}).get("uri") == nested_uri
                ]
            )
            == 1
        )

        # Revocation changes future recipients; public past activity remains
        # readable, but cannot confer authority to manage the channel.
        api.post(path, operation="revoke", username=editor.username)
        newer = pt.post(
            f"/api/v1/videos/{uuid}/comment-threads",
            text=f"After editor revocation {name}",
        )["comment"]
        newer_uri = f"{pt.base_url}/videos/watch/{uuid}/comments/{newer['id']}"
        wait_for(
            lambda: [
                n for n in events(api, "comment") if n["status"]["uri"] == newer_uri
            ],
            desc="owner receives comment after editor revocation",
        )
        assert not any(
            n["status"]["uri"] == newer_uri for n in events(editor_api, "comment")
        )
        pt.delete(f"/api/v1/users/me/subscriptions/{handle}")
        subscribed = False
        wait_for(
            lambda: not events(api, "follow") and not events(editor_api, "follow"),
            desc="PeerTube Undo clears all channel Follow copies",
        )
    finally:
        if subscribed:
            with contextlib.suppress(Exception):
                pt.delete(f"/api/v1/users/me/subscriptions/{handle}")
        if video:
            with contextlib.suppress(Exception):
                api.delete(f"/api/v1/statuses/{video['id']}")
        peer.stop()
