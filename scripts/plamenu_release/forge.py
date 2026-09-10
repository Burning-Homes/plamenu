"""Optional Forgejo transport. No forge credentials are needed for local preparation."""

import json
import secrets
import urllib.error
import urllib.parse
import urllib.request

from ci.range_zip import RangeReader

from .core import ReleaseError, digest


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


class Forge:
    def __init__(self, repository, token, allow_public=False):
        if type(allow_public) is not bool:
            raise ReleaseError("allow_public must be a TOML boolean")
        parsed = urllib.parse.urlsplit(repository)
        parts = parsed.path.strip("/").split("/")
        if (
            parsed.scheme != "https"
            or not parsed.hostname
            or parsed.username
            or parsed.password
            or parsed.query
            or parsed.fragment
            or len(parts) != 2
            or any(not p or p in (".", "..") for p in parts)
        ):
            raise ReleaseError("repository must be an HTTPS owner/repository URL")
        self.origin = f"https://{parsed.netloc}"
        self.repository = self.origin + "/" + "/".join(parts)
        self.base = (
            self.origin
            + "/api/v1/repos/"
            + "/".join(urllib.parse.quote(p, safe="") for p in parts)
        )
        self.headers = {"Authorization": "token " + token}
        self.opener = urllib.request.build_opener(NoRedirect())
        self.allow_public = allow_public

    def request(self, url, method="GET", data=None, content_type="application/json"):
        parsed = urllib.parse.urlsplit(url)
        if (
            f"{parsed.scheme}://{parsed.netloc}" != self.origin
            or parsed.username
            or parsed.password
        ):
            raise ReleaseError("Refusing credentials outside the configured forge")
        return self.opener.open(
            urllib.request.Request(
                url,
                method=method,
                data=data,
                headers=self.headers | {"Content-Type": content_type},
            ),
            timeout=60,
        )

    def api(self, path="", method="GET", body=None):
        if path and (not path.startswith("/") or path.startswith("//")):
            raise ReleaseError("Expected repository-relative API path")
        with self.request(
            self.base + path,
            method,
            None if body is None else json.dumps(body).encode(),
        ) as response:
            data = response.read()
        return json.loads(data) if data else None

    def repository_check(self):
        repo = self.api()
        if not self.allow_public and (
            not repo.get("private")
            or repo.get("owner", {}).get("visibility") != "private"
        ):
            raise ReleaseError(
                "Repository and package owner must be private; public publication requires explicit allow_public configuration"
            )
        return repo

    def tag_release(self, tag):
        try:
            return self.api("/releases/tags/" + urllib.parse.quote(tag, safe=""))
        except urllib.error.HTTPError as error:
            if error.code != 404:
                raise
            return None

    def link_package(self, registry, version):
        host, owner, name = registry.split("/", 2)
        parsed = urllib.parse.urlsplit(self.repository)
        repo_owner, repo_name = parsed.path.strip("/").split("/")
        if host != parsed.netloc or owner != repo_owner:
            return
        package = (
            self.origin
            + "/api/v1/packages/"
            + "/".join(
                urllib.parse.quote(p, safe="") for p in (owner, "container", name)
            )
        )
        version_url = package + "/" + urllib.parse.quote(version, safe="")
        with self.request(version_url) as response:
            linked = json.load(response).get("repository")
        repo = self.repository_check()
        if linked and linked["id"] != repo["id"]:
            raise ReleaseError("Container package is linked to a different repository")
        # Forgejo checks write permission on the Packages unit when linking;
        # even repository owners cannot link while that unit is disabled.
        if not repo.get("has_packages"):
            self.api("", "PATCH", {"has_packages": True})
        # Linking an already linked package is an error, not an idempotent POST.
        if linked:
            return
        with self.request(
            package + "/-/link/" + urllib.parse.quote(repo_name, safe=""), "POST"
        ):
            pass
        with self.request(version_url) as response:
            linked = json.load(response).get("repository")
        if not linked or linked["id"] != repo["id"]:
            raise ReleaseError("Container package repository link was not saved")

    def download(self, url, destination, maximum=400 * 1024**2):
        parsed = urllib.parse.urlsplit(url)
        if (
            f"{parsed.scheme}://{parsed.netloc}" != self.origin
            or parsed.username
            or parsed.password
        ):
            raise ReleaseError(
                "Refusing download credentials outside the configured forge"
            )
        reader = RangeReader(self.opener, url, self.headers, maximum)
        with destination.open("wb") as output:
            while block := reader.read(4 * 1024**2):
                output.write(block)

    def upload(self, release_id, path):
        data = path.read_bytes()
        boundary = "plamenu-" + secrets.token_hex(24)
        body = (
            (
                f'--{boundary}\r\nContent-Disposition: form-data; name="attachment"; filename="{path.name}"\r\n'
                "Content-Type: application/octet-stream\r\n\r\n"
            ).encode()
            + data
            + f"\r\n--{boundary}--\r\n".encode()
        )
        with self.request(
            self.base
            + f"/releases/{release_id}/assets?name="
            + urllib.parse.quote(path.name),
            "POST",
            body,
            "multipart/form-data; boundary=" + boundary,
        ) as response:
            asset = json.load(response)
        if asset["name"] != path.name or asset["size"] != len(data):
            raise ReleaseError("Uploaded asset metadata differs from local file")

    def sync_assets(self, release, folder, verify_dir):
        assets = self.api(f"/releases/{release['id']}/assets")
        names = [a["name"] for a in assets]
        expected = {p.name: p for p in folder.iterdir() if p.is_file()}
        if len(names) != len(set(names)) or set(names) - expected.keys():
            raise ReleaseError("Remote release contains unexpected or duplicate assets")
        if not release["draft"] and set(names) != expected.keys():
            raise ReleaseError("Published release is incomplete; refusing to alter it")
        # Verify existing bytes before uploading anything on a retry.
        for asset in assets:
            path = verify_dir / asset["name"]
            self.download(asset["browser_download_url"], path)
            if digest(path) != digest(expected[asset["name"]]):
                raise ReleaseError(
                    "Remote asset conflicts with prepared bytes: " + asset["name"]
                )
        for name in sorted(expected.keys() - set(names)):
            self.upload(release["id"], expected[name])
        # Every upload is read back, including its signed manifest and signature.
        assets = self.api(f"/releases/{release['id']}/assets")
        if sorted(a["name"] for a in assets) != sorted(expected):
            raise ReleaseError("Remote release asset set is incomplete")
        for asset in assets:
            path = verify_dir / asset["name"]
            self.download(asset["browser_download_url"], path)
            if digest(path) != digest(expected[asset["name"]]):
                raise ReleaseError(
                    "Release readback checksum mismatch: " + asset["name"]
                )
