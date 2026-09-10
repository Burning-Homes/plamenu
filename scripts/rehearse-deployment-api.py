#!/usr/bin/env python3
"""Client-visible recovery assertions for the isolated deployment drill.

Use only the private Caddy CA and network endpoint supplied by the orchestrator;
canonical URLs retain their normal HTTPS host/port for federation.
"""
import argparse
import hashlib
import html
import http.client
import json
from pathlib import Path
import re
import socket
import ssl
import struct
import time
import urllib.parse
import zlib

SOURCE = "restore-drill.test"
PEER = "peer.restore-drill.test"
PASSWORD = "deployment-drill-password"
OOB = "urn:ietf:wg:oauth:2.0:oob"


class LocalHTTPS(http.client.HTTPSConnection):
    def __init__(self, *args, connect_host="127.0.0.1", **kwargs):
        super().__init__(*args, **kwargs)
        self.connect_host = connect_host

    def connect(self):
        sock = socket.create_connection((self.connect_host, self.port), self.timeout)
        self.sock = self._context.wrap_socket(sock, server_hostname=self.host)


class Api:
    def __init__(self, host, port, ca, token=None, connect_host="127.0.0.1"):
        self.host, self.port, self.token = host, port, token
        self.connect_host = connect_host
        self.context = ssl.create_default_context(cafile=ca)

    def request(self, method, path, data=None, body=None, content_type=None):
        headers = {"Host": self.host}
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        if data is not None:
            body, content_type = json.dumps(data).encode(), "application/json"
        if content_type:
            headers["Content-Type"] = content_type
        conn = LocalHTTPS(self.host, self.port, context=self.context, timeout=15, connect_host=self.connect_host)
        try:
            conn.request(method, path, body=body, headers=headers)
            response = conn.getresponse()
            payload = response.read()
            if not 200 <= response.status < 300:
                # Do not include response bodies: login responses may carry secrets.
                raise RuntimeError(f"{self.host} {method} {path}: HTTP {response.status}")
            return payload
        finally:
            conn.close()

    def get(self, path):
        return json.loads(self.request("GET", path))

    def post(self, path, data):
        return json.loads(self.request("POST", path, data=data))

    def login(self):
        app = self.post("/api/v1/apps", {
            "client_name": "deployment-drill", "redirect_uris": [OOB], "scopes": "read write follow",
        })
        page = self.request("POST", "/oauth/authorize", body=urllib.parse.urlencode({
            "client_id": app["client_id"], "redirect_uri": OOB, "scope": "read write follow",
            "email": f"drill@{self.host}", "password": PASSWORD,
        }).encode(), content_type="application/x-www-form-urlencoded").decode()
        code = re.search(r'<pre[^>]*>(.+?)</pre>', page)
        require(code is not None, "password authentication produced no authorization code")
        self.token = self.post("/oauth/token", {
            "grant_type": "authorization_code", "code": html.unescape(code.group(1)),
            "client_id": app["client_id"], "client_secret": app["client_secret"], "redirect_uri": OOB,
        })["access_token"]
        return self.get("/api/v1/accounts/verify_credentials")

    def media_bytes(self, url):
        parsed = urllib.parse.urlsplit(url)
        require(parsed.scheme == "https" and parsed.netloc == self.host, "unexpected media origin")
        return self.request("GET", parsed.path + (f"?{parsed.query}" if parsed.query else ""))

    def upload(self, label):
        # A small deterministic PNG with actual pixel data, no external fixtures.
        def chunk(kind, data):
            return struct.pack("!I", len(data)) + kind + data + struct.pack("!I", zlib.crc32(kind + data))
        png = b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('!2I5B', 16, 16, 8, 2, 0, 0, 0))
        png += chunk(b'IDAT', zlib.compress((b'\0' + b'\x40\x80\xc0' * 16) * 16)) + chunk(b'IEND', b'')
        boundary = "plamenu-deployment-drill-boundary"
        body = (f'--{boundary}\r\nContent-Disposition: form-data; name="description"\r\n\r\n{label}\r\n'
                f'--{boundary}\r\nContent-Disposition: form-data; name="file"; filename="drill.png"\r\n'
                'Content-Type: image/png\r\n\r\n').encode() + png + f'\r\n--{boundary}--\r\n'.encode()
        media = json.loads(self.request("POST", "/api/v1/media", body=body,
                                       content_type=f"multipart/form-data; boundary={boundary}"))
        require(media["type"] == "image" and media["description"] == label, "uploaded media metadata differs")
        return media

    def post_with_media(self, text):
        media = self.upload(text)
        status = self.post("/api/v1/statuses", {"status": text, "visibility": "public", "media_ids": [media["id"]]})
        require(text in status["content"], "posted content missing")
        require(len(status["media_attachments"]) == 1, "posted media missing")
        return status


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def wait_for(label, check, timeout=120):
    deadline = time.monotonic() + timeout
    while True:
        result = check()
        if result:
            return result
        if time.monotonic() >= deadline:
            raise RuntimeError(f"timed out: {label}")
        time.sleep(1)


def follow(api, remote):
    result = api.get("/api/v2/search?" + urllib.parse.urlencode({"q": f"drill@{remote}", "type": "accounts", "resolve": "true"}))
    accounts = [a for a in result["accounts"] if a["acct"] == f"drill@{remote}"]
    require(len(accounts) == 1, "remote account resolution failed")
    account_id = accounts[0]["id"]
    api.post(f"/api/v1/accounts/{account_id}/follow", {})
    def accepted():
        rel = api.get(f"/api/v1/accounts/relationships?id[]={account_id}")[0]
        return rel["following"] and not rel["requested"]
    wait_for("signed Follow/Accept exchange", accepted)


def received(api, status):
    # Read the home timeline, never resolve the object: resolution could hide a
    # broken delivery queue by pulling the post into the peer on demand.
    def find_status():
        return next((s for s in api.get("/api/v1/timelines/home?limit=40") if s["uri"] == status["uri"]), None)
    result = wait_for("pushed post " + status["uri"], find_status)
    require(result["content"] == status["content"], "federated content differs")
    require(len(result["media_attachments"]) == len(status["media_attachments"]), "federated media missing")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("phase", choices=["prepare", "queue", "restore"])
    parser.add_argument("--connect-host", default="127.0.0.1")
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--ca", required=True)
    parser.add_argument("--state", type=Path, required=True)
    args = parser.parse_args()
    source = Api(SOURCE, args.port, args.ca, connect_host=args.connect_host)
    peer = Api(PEER, args.port, args.ca, connect_host=args.connect_host)
    if args.phase == "prepare":
        account = source.login()
        peer.login()
        follow(peer, SOURCE)
        follow(source, PEER)
        old = source.post_with_media("Before backup: preserved post and image")
        received(peer, old)
        media = old["media_attachments"][0]
        state = {"account_id": account["id"], "token": source.token, "peer_token": peer.token,
                 "old": old, "media_sha256": hashlib.sha256(source.media_bytes(media["url"])).hexdigest()}
        print("PASS password login, real media/post, and bidirectional Follow/Accept before backup")
    else:
        state = json.loads(args.state.read_text())
        source.token, peer.token = state["token"], state["peer_token"]
        if args.phase == "queue":
            state["queued"] = source.post_with_media("Before backup: queued while peer is stopped")
            print("PASS created post and media while recipient is stopped")
        else:
            require(source.get("/api/v1/accounts/verify_credentials")["id"] == state["account_id"], "restored token/account changed")
            require(source.login()["id"] == state["account_id"], "fresh password login changed account")
            old = source.get("/api/v1/statuses/" + state["old"]["id"])
            for field in ("id", "uri", "content", "visibility", "created_at"):
                require(old[field] == state["old"][field], f"restored post changed {field}")
            media = old["media_attachments"][0]
            require(media["id"] == state["old"]["media_attachments"][0]["id"], "restored attachment ID changed")
            require(media["description"] == state["old"]["media_attachments"][0]["description"], "restored alt text changed")
            require(hashlib.sha256(source.media_bytes(media["url"])).hexdigest() == state["media_sha256"], "restored image bytes differ")
            received(peer, state["queued"])
            new = source.post_with_media("After restore: new post and image")
            require(new["id"] not in (old["id"], state["queued"]["id"]), "new post reused a restored ID")
            require(source.media_bytes(new["media_attachments"][0]["url"]), "new image is unreadable")
            received(peer, new)
            incoming = peer.post_with_media("After restore: incoming peer post and image")
            received(source, incoming)
            print("PASS restored token, fresh password login, old post identity/content, alt text and exact image bytes")
            print("PASS restored pending delivery, new post/media, outbound and inbound signed federation")
    args.state.write_text(json.dumps(state, indent=2) + "\n")
    args.state.chmod(0o600)


if __name__ == "__main__":
    main()
