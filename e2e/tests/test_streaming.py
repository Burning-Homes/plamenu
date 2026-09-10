"""The streaming websocket delivers live events fed by real federation."""

import json
import ssl
import time

import pytest
from plamenu_e2e import config
from plamenu_e2e.steps import log, step, wait_for
from websockets.sync.client import connect


def _open_stream(token: str, stream: str):
    """A websocket on Plamenu's streaming API (Caddy TLS; trust is not
    under test, like api.py)."""
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    base = config.PLAMENU_URL.replace("https://", "wss://")
    return connect(
        f"{base}/api/v1/streaming?access_token={token}&stream={stream}",
        ssl=ctx,
        open_timeout=30,
    )


def _next_event(ws, wanted: str, *, timeout: float = 90.0) -> dict:
    """The next message with event type `wanted`, skipping everything else."""
    deadline = time.monotonic() + timeout
    while True:
        remaining = deadline - time.monotonic()
        assert remaining > 0, f"no '{wanted}' event arrived within {timeout}s"
        message = json.loads(ws.recv(timeout=remaining))
        log(f"<< stream: {message.get('event')} on {message.get('stream')}")
        if message.get("event") == wanted:
            return message


@pytest.mark.federation(
    direction="inbound",
    one_way_reason="inbound streaming: a federated activity surfaces live on the streaming API; a receive-side delivery behavior with no outbound counterpart.",
)
def test_federated_activity_streams_live(
    alice, plamenu_user, plamenu_api, cli, db, marker
):
    """Covers: the /api/v1/streaming websocket end to end — an inbound
    federated Create surfaces as a live `update` on the user stream, an
    inbound Like as a live `notification`, and the wire format is
    Mastodon's (stream name array, event, JSON-string payload)."""
    with step(f"@{plamenu_user.username} follows {config.ALICE}"):
        cli.follow(plamenu_user.username, config.ALICE)
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the outbound follow to be accepted (pending=false)",
        )

    token = plamenu_api.http.headers["Authorization"].removeprefix("Bearer ")
    with _open_stream(token, "user") as ws:
        with step("alice posts; the update must arrive live over the websocket"):
            alice.post_status(f"Streamed from Mastodon! {marker}")
            while True:
                message = _next_event(ws, "update")
                payload = json.loads(message["payload"])
                if marker in payload["content"]:
                    break
            assert message["stream"] == ["user"]
            assert payload["account"]["acct"] == config.ALICE

        with step("alice favourites a Plamenu post; the notification arrives live"):
            mine = plamenu_api.post_status(f"favourite bait {marker}")
            statuses = alice.search(mine["uri"], resolve=True, type="statuses")[
                "statuses"
            ]
            assert statuses, "Mastodon could not resolve the Plamenu post"
            alice.post(f"/api/v1/statuses/{statuses[0]['id']}/favourite")

            message = _next_event(ws, "notification")
            payload = json.loads(message["payload"])
            assert payload["type"] == "favourite"
            assert payload["account"]["acct"] == config.ALICE
            assert payload["status"]["id"] == mine["id"]
