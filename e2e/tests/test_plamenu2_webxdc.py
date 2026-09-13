"""Plamenu against Plamenu: a reusable Webxdc app over federation.

The Rust integration tests cover protocol validation and every individual
state transition.  This test keeps the browser-facing workflow honest across
two real HTTPS instances: save once in the new app library, create without a
second upload, explicitly fetch and join from a peer, then exchange a durable
update through ActivityPub.
"""

import io
import re
import zipfile

import pytest
import requests
from plamenu_e2e import config
from plamenu_e2e.steps import log, step, wait_for

CSRF_RE = re.compile(r'name="csrf" value="([^"]+)"')


def _csrf(html: str) -> str:
    match = CSRF_RE.search(html)
    assert match, "no CSRF token in the Webxdc page"
    return match.group(1)


def _web_login(user, base_url: str) -> requests.Session:
    session = requests.Session()
    session.verify = False
    response = session.post(
        f"{base_url}/login",
        data={"email": user.email, "password": user.password},
        allow_redirects=False,
    )
    assert response.status_code == 303, f"web login failed: {response.status_code}"
    assert session.cookies, "web login set no session cookie"
    return session


def _package(marker: str) -> bytes:
    bundle = io.BytesIO()
    with zipfile.ZipFile(bundle, "w") as archive:
        archive.writestr(
            "manifest.toml",
            f'name = "Federated library check {marker}"\nversion = "1.0-e2e"\n',
        )
        archive.writestr(
            "index.html",
            "<!doctype html><meta charset=utf-8><h1>Federated library check</h1>"
            '<script src="webxdc.js"></script>',
        )
    return bundle.getvalue()


def _value(conn, sql: str, *params):
    row = conn.execute(sql, params).fetchone()
    return row[0] if row else None


@pytest.mark.federation(direction="both")
def test_library_app_session_fetch_join_and_update_between_plamenu_instances(
    plamenu2, plamenu_user, plamenu2_user, db, marker
):
    """A saved package stays byte-identical through a two-instance session.

    Covers the personal-library upload and version selection, public FEP-752d
    actor/package fetch, peer cache, Follow/Accept admission, isolated runtime
    origin, and a participant update sequenced and announced by the remote
    coordinator.
    """
    local_web = _web_login(plamenu_user, config.PLAMENU_URL)
    peer_web = _web_login(plamenu2_user, plamenu2.url)
    package = _package(marker)

    with step("save a reusable app in the local user's personal library"):
        page = local_web.get(f"{config.PLAMENU_URL}/webxdc/library")
        response = local_web.post(
            f"{config.PLAMENU_URL}/web/webxdc/library",
            data={
                "csrf": _csrf(page.text),
                "summary": "Two-instance Webxdc library verification",
                "category": "E2E",
            },
            files={
                "bundle": (
                    "federated-library.xdc",
                    package,
                    "application/webxdc+zip",
                )
            },
            allow_redirects=False,
        )
        assert response.status_code == 303, response.text
        account_id = _value(
            db.conn,
            "SELECT id FROM accounts WHERE username = %s AND domain IS NULL",
            plamenu_user.username,
        )
        assert account_id is not None
        version_id, digest = db.conn.execute(
            "SELECT v.id, v.digest_multibase FROM webxdc_app_versions v"
            " JOIN webxdc_apps a ON a.id = v.app_id"
            " WHERE a.owner_account_id = %s AND v.current",
            (account_id,),
        ).fetchone()
        log(f"saved immutable version {version_id}")

    with step("create a session by reference, without uploading the package again"):
        page = local_web.get(
            f"{config.PLAMENU_URL}/webxdc/new", params={"version": version_id}
        )
        response = local_web.post(
            f"{config.PLAMENU_URL}/web/webxdc",
            data={
                "csrf": _csrf(page.text),
                "version_id": str(version_id),
                "name": f"Library federation {marker}",
                "summary": "Created from an immutable saved version",
                "membership_policy": "open",
                "send_update_interval": "0",
                "send_update_max_size": "32768",
            },
            files={"bundle": ("", b"", "application/octet-stream")},
            allow_redirects=False,
        )
        assert response.status_code == 303, response.text
        coordinator_path = response.headers["location"]
        coordinator_id = int(coordinator_path.rsplit("/", 1)[-1])
        coordinator_uri = f"{config.PLAMENU_URL}/webxdc/{coordinator_id}"
        session_digest, stored_bundle = db.conn.execute(
            "SELECT s.digest_multibase, p.bundle_bytes FROM webxdc_sessions s"
            " JOIN webxdc_packages p USING (digest_multibase) WHERE s.id = %s",
            (coordinator_id,),
        ).fetchone()
        assert session_digest == digest
        assert bytes(stored_bundle) == package

    with step("the peer explicitly fetches the session actor and immutable package"):
        page = peer_web.get(f"{plamenu2.url}/webxdc/open")
        response = peer_web.post(
            f"{plamenu2.url}/web/webxdc/open",
            data={"csrf": _csrf(page.text), "url": coordinator_uri},
            allow_redirects=False,
        )
        assert response.status_code == 303, response.text
        peer_path = response.headers["location"]
        peer_session_id = int(peer_path.rsplit("/", 1)[-1])
        peer_digest, peer_bundle = plamenu2.db.conn.execute(
            "SELECT s.digest_multibase, p.bundle_bytes FROM webxdc_sessions s"
            " JOIN webxdc_packages p USING (digest_multibase) WHERE s.id = %s",
            (peer_session_id,),
        ).fetchone()
        assert peer_digest == digest
        assert bytes(peer_bundle) == package
        log(f"peer cached the verified package as session {peer_session_id}")

    with step("the peer joins and both halves settle the Follow/Accept handshake"):
        landing = peer_web.get(f"{plamenu2.url}{peer_path}")
        response = peer_web.post(
            f"{plamenu2.url}/web/webxdc/{peer_session_id}/join",
            data={"csrf": _csrf(landing.text)},
            allow_redirects=False,
        )
        assert response.status_code == 303, response.text
        wait_for(
            lambda: plamenu2.db_value(
                "SELECT accepted FROM webxdc_memberships m"
                " JOIN accounts a ON a.id = m.participant_account_id"
                " WHERE m.session_id = %s AND a.username = %s AND a.domain IS NULL",
                peer_session_id,
                plamenu2_user.username,
            ),
            desc="the coordinator's Accept(Follow) to settle on the peer",
        )
        wait_for(
            lambda: _value(
                db.conn,
                "SELECT m.accepted FROM webxdc_memberships m"
                " JOIN accounts a ON a.id = m.participant_account_id"
                " WHERE m.session_id = %s AND a.username = %s AND a.domain = %s",
                coordinator_id,
                plamenu2_user.username,
                plamenu2.domain,
            ),
            desc="the remote participant to be accepted by the coordinator",
        )

    with step("the joined app is framed on its dedicated cookie-less origin"):
        play = peer_web.get(f"{plamenu2.url}/webxdc/session/{peer_session_id}/play")
        assert play.status_code == 200, play.text
        runtime_origin = f"https://{peer_session_id}.webxdc.{plamenu2.domain}"
        assert f'src="{runtime_origin}/index.html?' in play.text
        assert (
            'sandbox="allow-scripts allow-same-origin allow-pointer-lock"' in play.text
        )
        runtime = requests.get(f"{runtime_origin}/index.html", verify=False, timeout=30)
        assert runtime.status_code == 200
        assert "Federated library check" in runtime.text
        assert "set-cookie" not in runtime.headers

    with step("a peer update is sequenced by the coordinator and announced back"):
        landing = peer_web.get(f"{plamenu2.url}{peer_path}")
        response = peer_web.post(
            f"{plamenu2.url}/web/webxdc/{peer_session_id}/updates",
            headers={"x-csrf-token": _csrf(landing.text)},
            json={"payload": {"check": marker}},
        )
        assert response.status_code == 202, response.text
        wait_for(
            lambda: (
                _value(
                    db.conn,
                    "SELECT webxdc_update->'payload'->>'check'"
                    " FROM webxdc_updates WHERE session_id = %s AND serial = 1",
                    coordinator_id,
                )
                == marker
            ),
            desc="the participant Create to be sequenced by the coordinator",
        )
        wait_for(
            lambda: (
                plamenu2.db_value(
                    "SELECT webxdc_update->'payload'->>'check' FROM webxdc_updates"
                    " WHERE session_id = %s AND serial = 1",
                    peer_session_id,
                )
                == marker
            ),
            desc="the coordinator's Announce(WebxdcUpdate) to return to the peer",
        )
