"""Drive the real Songhive peer through its public client APIs."""

import json
from pathlib import Path

import requests

from . import config, shell
from .steps import wait_for

SONGHIVE_URL = getattr(config, "SONGHIVE_URL", "https://songhive.local")
SONGHIVE_DOMAIN = SONGHIVE_URL.removeprefix("https://")


class Songhive:
    def __init__(self):
        self.http = requests.Session()
        self.http.verify = str(config.MASTO_DIR / "ca-bundle.crt")
        response = self.request(
            "POST",
            "/api/v1/auth/login",
            json={
                "username": "sylvia",
                "password": "songhive-sylvia-pass-123",
            },
        )
        self.http.headers["Authorization"] = "Bearer " + response["access_token"]

    def request(self, method, path, *, none_on=(), **kwargs):
        if method not in {"GET", "HEAD"}:
            csrf = self.http.cookies.get("csrf_token")
            if csrf:
                kwargs.setdefault("headers", {})["X-CSRF-Token"] = csrf
        response = self.http.request(method, SONGHIVE_URL + path, timeout=90, **kwargs)
        if response.status_code in none_on:
            return None
        if not response.ok:
            raise RuntimeError(
                f"Songhive {method} {path}: {response.status_code} {response.text[:400]}"
            )
        return response.json() if response.content else None

    def follow(self, target):
        return self.request(
            "POST", "/api/v1/users/me/follows", json={"actor_url": target}
        )

    def unfollow(self, target):
        return self.request(
            "DELETE", "/api/v1/users/me/follows", json={"actor_url": target}
        )

    def following(self, target):
        return next(
            (
                row
                for row in self.request("GET", "/api/v1/users/me/follows")
                if row.get("actor_url") == target and row["state"] == "accepted"
            ),
            None,
        )

    def follow_resource(self, uri):
        resource = self.lookup(uri)["object"]
        self.request("POST", f"/api/v1/remote/objects/{resource['id']}/follow")
        return resource["id"]

    def unfollow_resource(self, resource_id):
        return self.request("DELETE", f"/api/v1/remote/objects/{resource_id}/follow")

    def activity(self, uri):
        return self.request(
            "GET", "/api/v1/activities/lookup", params={"url": uri}, none_on=(404,)
        )

    def lookup(self, uri):
        return self.request("GET", "/api/v1/remote/lookup", params={"input": uri})

    def ap_get(self, uri: str):
        """Use the running peer's guarded client and real instance signature."""
        script = (
            "import asyncio,json,os,sys; from pathlib import Path; "
            "os.environ.setdefault('SONGHIVE_AUTH__SECRET_KEY',Path('/data/secret_key').read_text().strip()); "
            "from songhive.config.loader import load_config; "
            "from songhive.services.remote_content import fetch_remote_document; "
            "result=asyncio.run(fetch_remote_document(sys.argv[1],load_config([]))); "
            "print('PLAMENU_AP_DOCUMENT='+json.dumps(result.json()))"
        )
        output = shell.fixture_compose(
            config.PEER_ROOT / "songhive-test",
            "exec",
            "-T",
            "songhive",
            "python",
            "-c",
            script,
            uri,
        )
        document = next(
            (
                line.removeprefix("PLAMENU_AP_DOCUMENT=")
                for line in output.splitlines()
                if line.startswith("PLAMENU_AP_DOCUMENT=")
            ),
            None,
        )
        if document is None:
            raise RuntimeError(
                "Songhive guarded fetch returned no ActivityPub document"
            )
        return json.loads(document)

    def ap_unavailable(self, uri: str):
        """Probe 404/410 through the same real signed, guarded peer client."""
        script = (
            "import asyncio,os,sys; from pathlib import Path; "
            "os.environ.setdefault('SONGHIVE_AUTH__SECRET_KEY',Path('/data/secret_key').read_text().strip()); "
            "from songhive.config.loader import load_config; "
            "from songhive.services.remote_content import fetch_remote_document; "
            "from songhive.federation.fetch import FetchNotFound;\n"
            "try:\n asyncio.run(fetch_remote_document(sys.argv[1],load_config([])))\n"
            "except FetchNotFound:\n print('PLAMENU_AP_UNAVAILABLE=true')\n"
            "else:\n print('PLAMENU_AP_UNAVAILABLE=false')\n"
        )
        output = shell.fixture_compose(
            config.PEER_ROOT / "songhive-test",
            "exec",
            "-T",
            "songhive",
            "python",
            "-c",
            script,
            uri,
        )
        return "PLAMENU_AP_UNAVAILABLE=true" in output.splitlines()

    def cached_resource(self, uri: str):
        """Read the actual peer cache without fetching or modifying an origin."""
        script = (
            "import asyncio,json,os,sys; "
            "from sqlalchemy import text; from sqlalchemy.ext.asyncio import create_async_engine; "
            "from sqlalchemy.pool import NullPool;\n"
            "async def read():\n"
            " engine=create_async_engine(os.environ['SONGHIVE_DATABASE__URL'],poolclass=NullPool)\n"
            " try:\n"
            "  async with engine.connect() as conn:\n"
            "   row=(await conn.execute(text('''SELECT r.payload,r.unavailable_at,EXISTS(SELECT 1 FROM activities a WHERE a.source_type='remote' AND a.source_id=r.canonical_url AND a.source_actor=r.actor_url) FROM remote_objects r WHERE r.canonical_url=:uri'''),{'uri':sys.argv[1]})).first()\n"
            "   result=None if row is None else {'payload':row[0],'unavailable':row[1] is not None,'materialized':row[2]}\n"
            "   print('PLAMENU_PEER_CACHE='+json.dumps(result))\n"
            " finally:\n  await engine.dispose()\n"
            "asyncio.run(read())\n"
        )
        output = shell.fixture_compose(
            config.PEER_ROOT / "songhive-test",
            "exec",
            "-T",
            "songhive",
            "python",
            "-c",
            script,
            uri,
        )
        record = next(
            (
                line.removeprefix("PLAMENU_PEER_CACHE=")
                for line in output.splitlines()
                if line.startswith("PLAMENU_PEER_CACHE=")
            ),
            None,
        )
        if record is None:
            raise RuntimeError("Songhive cache probe returned no record")
        return json.loads(record)

    def upload(self, path: Path):
        with path.open("rb") as stream:
            result = self.request(
                "POST",
                "/api/v1/files/upload",
                params={"visibility": "public"},
                files={"file": (path.name, stream, "audio/mpeg")},
            )

        if "title" in result:
            return result
        tracks = result.get("tracks") or wait_for(
            lambda: self.request("GET", f"/api/v1/files/{result['id']}").get("tracks"),
            desc="Songhive to import the uploaded audio",
        )
        if len(tracks) != 1:
            raise RuntimeError("Songhive upload did not import exactly one track")
        return tracks[0]

    def publish(self, track_id):
        return self.request(
            "POST", f"/api/v1/tracks/{track_id}/publish", json={"object_type": "audio"}
        )
