"""Plamenu ↔ onion-identity Mitra over Tor (docs/TOR_TRANSPORT.md).

The peer's identity is ``http://<generated>.onion`` — reachable from Plamenu
ONLY through the tor-test SOCKS lane, because nothing maps the name to a
clearnet address by construction. Every Plamenu-side webfinger, actor/object
fetch, media download, and inbox delivery here therefore exercises the
``socks5h`` onion lane end to end, including HTTP signatures over an
``http://`` origin (key ids ``http://…onion/users/nina#main-key``). The
peer's own outbound leg (its fetches of our actors, its Accepts and Creates
toward us) rides the clearnet like any dev peer: onion is an inbound
identity, not an egress requirement.
"""

import pytest
import requests
from plamenu_e2e import config, mitra, onion
from plamenu_e2e.api import Api, ApiError
from plamenu_e2e.media import cached_attachment, make_png
from plamenu_e2e.steps import step, wait_for


def _nina_acct() -> str:
    return f"{onion.NINA_NICK}@{onion.onion_domain()}"


def _resolve_over_tor(plamenu_api: Api, acct: str) -> dict:
    """Resolve with retries: the very first dial through a cold Tor daemon
    (circuit build + hidden-service descriptor fetch) can exceed one API
    call's 30 s budget; the server finishes and caches the actor regardless,
    so a retry lands instantly."""

    def attempt():
        try:
            return plamenu_api.resolve_account(acct)
        except (ApiError, requests.RequestException):
            return None

    return wait_for(attempt, desc=f"Plamenu resolve of {acct} over Tor")


def _follow_nina(plamenu_api: Api) -> dict:
    acct = _nina_acct()
    account = _resolve_over_tor(plamenu_api, acct)
    plamenu_api.follow(account["id"])
    wait_for(
        lambda: plamenu_api.relationship(account["id"])["following"],
        desc="onion Mitra Accept(Follow) to reach Plamenu",
    )
    return account


def _nina_follows(onion_nina: Api, acct: str) -> dict:
    account = onion_nina.resolve_account(acct)
    assert account, f"onion Mitra cannot resolve {acct}"
    onion_nina.follow(account["id"])
    wait_for(
        lambda: onion_nina.relationship(account["id"])["following"],
        desc=f"onion Mitra follow of {acct} to be accepted",
    )
    return account


@pytest.mark.federation(direction="both", peer="onion")
def test_discovery_and_follow_both_directions(
    onion_nina, plamenu_user, plamenu_api, db
):
    with step("Plamenu resolves @nina@<onion> (http webfinger via Tor)"):
        account = _resolve_over_tor(plamenu_api, _nina_acct())
        assert account["acct"] == _nina_acct()
        # The stored actor keeps its native http:// onion identity.
        assert account["url"].startswith("http://"), account["url"]

    with step("Plamenu follows nina: signed Follow rides Tor, Accept returns"):
        _follow_nina(plamenu_api)

    with step("nina's follower list shows the Plamenu author"):
        nina_id = onion_nina.get("/api/v1/accounts/verify_credentials")["id"]
        my_acct = f"{plamenu_user.username}@{config.PLAMENU_DOMAIN}"
        wait_for(
            lambda: any(
                row["acct"] == my_acct for row in onion_nina.followers(nina_id)
            ),
            desc="Plamenu follower row on the onion Mitra",
        )

    with step("nina follows back; her signed Follow verifies (http keyId)"):
        _nina_follows(onion_nina, f"{plamenu_user.username}@{config.PLAMENU_DOMAIN}")
        wait_for(
            lambda: db.follower_count(plamenu_user.username) == 1,
            desc="onion follower row on Plamenu",
        )

    with step("nina's handle stays fully qualified after the inbound refresh"):
        # Regression: the inbound Follow re-stores nina's actor, and the
        # URI-derived domain path once returned '' for http:// onion ids —
        # webfinger had stored the right domain, the refresh wiped it, and
        # every surface rendered "@nina@" from then on.
        refreshed = plamenu_api.get(f"/api/v1/accounts/{account['id']}")
        assert refreshed["acct"] == _nina_acct(), refreshed["acct"]


@pytest.mark.federation(direction="both", peer="onion")
def test_post_media_boost_reply_round_trip(
    onion_nina, plamenu_user, plamenu_api, db, marker
):
    with step("mutual follows"):
        _follow_nina(plamenu_api)
        my_acct = f"{plamenu_user.username}@{config.PLAMENU_DOMAIN}"
        _nina_follows(onion_nina, my_acct)

    with step("a Plamenu post is delivered to nina's inbox over Tor"):
        mine = plamenu_api.post_status(f"clearnet to onion {marker}")
        wait_for(
            lambda: mitra.known_status(onion_nina, mine["uri"]),
            desc="Plamenu post on nina's home timeline",
        )

    with step("nina posts an image; the Create reaches Plamenu"):
        up = onion_nina.upload_media(
            make_png(rgb=(80, 20, 160)),
            filename="onion.png",
            mime="image/png",
            description="an onion picture",
        )
        onion_nina.post_with_media(f"onion to clearnet {marker}", [up["id"]])
        status_id = wait_for(
            lambda: db.status_id_containing(f"onion to clearnet {marker}"),
            desc="onion Mitra post in plamenu statuses",
        )

    with step("the stored status attributes to the fully qualified handle"):
        local = plamenu_api.get_status(str(status_id))
        assert local["account"]["acct"] == _nina_acct(), local["account"]["acct"]

    with step("Plamenu caches the onion-hosted image through the Tor lane"):
        att = wait_for(
            lambda: cached_attachment(plamenu_api, status_id),
            desc="onion attachment proxied onto Plamenu's /media/ route",
        )
        assert att["url"].startswith(f"{config.PLAMENU_URL}/media/"), att["url"]
        assert att["description"] == "an onion picture"
        assert ".onion" not in (att.get("url") or "")

    with step("Plamenu boosts and replies; both are delivered over Tor"):
        local = plamenu_api.get_status(str(status_id))
        plamenu_api.reblog(local["id"])
        plamenu_api.post_status(
            f"reply into the onion {marker}", in_reply_to_id=local["id"]
        )
        wait_for(
            lambda: onion_nina.notifications_from(my_acct, "reblog"),
            desc="Plamenu boost notification on the onion Mitra",
        )
        wait_for(
            lambda: onion_nina.notifications_from(my_acct, "mention"),
            desc="Plamenu reply notification on the onion Mitra",
        )
