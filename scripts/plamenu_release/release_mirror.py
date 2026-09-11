"""Mirror immutable Codefloe release assets to a GitHub release."""

import json
import re
import tempfile
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

from ci.range_zip import RangeReader

from .core import ReleaseError, digest
from .forge import NoRedirect

MAXIMUM_ASSET_SIZE = 400 * 1024**2


def repository_url(repository, required_host=None):
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
        or any(not part or part in (".", "..") for part in parts)
        or required_host is not None
        and parsed.netloc.lower() != required_host
    ):
        destination = (
            f"an HTTPS {required_host} owner/repository URL"
            if required_host
            else "an HTTPS owner/repository URL"
        )
        raise ReleaseError("repository must be " + destination)
    origin = f"https://{parsed.netloc}"
    return origin + "/" + "/".join(parts), origin, parts


def expected_asset_names(tag):
    match = re.fullmatch(r"v(\d+\.\d+\.\d+(?:-[a-z0-9.-]+)?)", tag)
    if not match:
        raise ReleaseError("Release tag must be vVERSION")
    # Import here so publication can import this module after it initializes.
    from .publish import prepared_names

    return prepared_names(match.group(1)) | {
        "cosign.pub",
        "image.txt",
        "SHA256SUMS.bundle",
    }


def mirror_body(body, source_repository, destination_repository, tag):
    source_download = (
        source_repository
        + "/releases/download/"
        + urllib.parse.quote(tag, safe="")
        + "/"
    )
    destination_download = (
        destination_repository
        + "/releases/download/"
        + urllib.parse.quote(tag, safe="")
        + "/"
    )
    notice = (
        "> Read-only mirror of the canonical "
        f"[Codefloe release]({source_repository}/releases/tag/"
        f"{urllib.parse.quote(tag, safe='')})."
    )
    return notice + "\n\n" + body.replace(source_download, destination_download)


class PublicForgejoRelease:
    """Read a public Forgejo release without accepting or sending credentials."""

    def __init__(self, repository):
        self.repository, self.origin, parts = repository_url(repository)
        self.base = (
            self.origin
            + "/api/v1/repos/"
            + "/".join(urllib.parse.quote(part, safe="") for part in parts)
        )
        self.opener = urllib.request.build_opener(NoRedirect())

    def release(self, tag):
        url = self.base + "/releases/tags/" + urllib.parse.quote(tag, safe="")
        with self.opener.open(
            urllib.request.Request(
                url,
                headers={"Accept": "application/json", "User-Agent": "plamenu"},
            ),
            timeout=60,
        ) as response:
            return json.load(response)

    def download(self, url, destination, maximum=MAXIMUM_ASSET_SIZE):
        parsed = urllib.parse.urlsplit(url)
        if (
            f"{parsed.scheme}://{parsed.netloc}" != self.origin
            or parsed.username
            or parsed.password
        ):
            raise ReleaseError("Refusing a release download outside Codefloe")
        reader = RangeReader(self.opener, url, {"User-Agent": "plamenu"}, maximum)
        with destination.open("wb") as output:
            while block := reader.read(4 * 1024**2):
                output.write(block)


class GitHub:
    def __init__(self, repository, token):
        self.repository, _, self.parts = repository_url(repository, "github.com")
        encoded = "/".join(urllib.parse.quote(part, safe="") for part in self.parts)
        self.base = "https://api.github.com/repos/" + encoded
        self.upload_base = "https://uploads.github.com/repos/" + encoded
        self.headers = {
            "Accept": "application/vnd.github+json",
            "Authorization": "Bearer " + token,
            "User-Agent": "plamenu",
            "X-GitHub-Api-Version": "2022-11-28",
        }
        self.opener = urllib.request.build_opener(NoRedirect())
        # Asset readback is public and deliberately uses no authorization.
        self.public_opener = urllib.request.build_opener()

    def request(self, url, method="GET", data=None, content_type="application/json"):
        parsed = urllib.parse.urlsplit(url)
        origin = f"{parsed.scheme}://{parsed.netloc}"
        if (
            parsed.scheme != "https"
            or origin not in ("https://api.github.com", "https://uploads.github.com")
            or parsed.username
            or parsed.password
        ):
            raise ReleaseError("Refusing GitHub credentials outside its API hosts")
        return self.opener.open(
            urllib.request.Request(
                url,
                method=method,
                data=data,
                headers=self.headers | {"Content-Type": content_type},
            ),
            timeout=120,
        )

    def api(self, path="", method="GET", body=None):
        if path and (not path.startswith("/") or path.startswith("//")):
            raise ReleaseError("Expected repository-relative GitHub API path")
        with self.request(
            self.base + path,
            method,
            None if body is None else json.dumps(body).encode(),
        ) as response:
            data = response.read()
        return json.loads(data) if data else None

    def repository_check(self):
        repository = self.api()
        expected = "/".join(self.parts).casefold()
        if (
            repository.get("full_name", "").casefold() != expected
            or repository.get("private") is not False
            or repository.get("archived")
        ):
            raise ReleaseError(
                "GitHub destination must be the configured public, writable repository"
            )
        return repository

    def tag_release(self, tag):
        try:
            return self.api("/releases/tags/" + urllib.parse.quote(tag, safe=""))
        except urllib.error.HTTPError as error:
            if error.code != 404:
                raise
            return None

    def tag_commit(self, tag):
        reference = self.api("/git/ref/tags/" + urllib.parse.quote(tag, safe=""))[
            "object"
        ]
        for _ in range(5):
            if reference.get("type") == "commit":
                return reference["sha"]
            if reference.get("type") != "tag":
                break
            reference = self.api("/git/tags/" + reference["sha"])["object"]
        raise ReleaseError("GitHub tag does not resolve to a commit")

    def create_release(self, source, body):
        return self.api(
            "/releases",
            "POST",
            {
                "tag_name": source["tag_name"],
                "target_commitish": source["target_commitish"],
                "name": source["name"],
                "body": body,
                "draft": True,
                "prerelease": bool(source.get("prerelease")),
            },
        )

    def finalize(self, release):
        return self.api("/releases/" + str(release["id"]), "PATCH", {"draft": False})

    def download(self, asset, destination, maximum=MAXIMUM_ASSET_SIZE):
        """Read a draft asset through the API without forwarding its token."""
        url = asset.get("url", "")
        prefix = self.base + "/releases/assets/"
        if not url.startswith(prefix) or not url.removeprefix(prefix).isdigit():
            raise ReleaseError("GitHub returned an unexpected asset API URL")
        request = urllib.request.Request(
            url,
            headers=self.headers | {"Accept": "application/octet-stream"},
        )
        try:
            response = self.opener.open(request, timeout=120)
        except urllib.error.HTTPError as error:
            if error.code not in (301, 302, 303, 307, 308):
                raise
            location = urllib.parse.urljoin(url, error.headers.get("Location", ""))
            error.close()
            parsed = urllib.parse.urlsplit(location)
            if (
                parsed.scheme != "https"
                or not parsed.hostname
                or parsed.username
                or parsed.password
            ):
                raise ReleaseError("GitHub returned an unsafe asset redirect")
            # This separate opener receives no Authorization header.
            reader = RangeReader(
                self.public_opener, location, {"User-Agent": "plamenu"}, maximum
            )
            with destination.open("wb") as output:
                while block := reader.read(4 * 1024**2):
                    output.write(block)
            return
        with response:
            size = asset.get("size")
            if not isinstance(size, int) or not 0 < size <= maximum:
                raise ReleaseError("GitHub asset has an invalid size")
            with destination.open("wb") as output:
                remaining = maximum + 1
                while block := response.read(min(4 * 1024**2, remaining)):
                    output.write(block)
                    remaining -= len(block)
                    if remaining <= 0:
                        raise ReleaseError("GitHub asset exceeds the download limit")

    def upload(self, release, path):
        template = release.get("upload_url", "")
        base = template.split("{", 1)[0]
        expected = self.upload_base + "/releases/" + str(release["id"]) + "/assets"
        if base != expected:
            raise ReleaseError("GitHub returned an unexpected release upload URL")
        data = path.read_bytes()
        with self.request(
            base + "?name=" + urllib.parse.quote(path.name, safe=""),
            "POST",
            data,
            "application/octet-stream",
        ) as response:
            asset = json.load(response)
        if asset["name"] != path.name or asset["size"] != len(data):
            raise ReleaseError("Uploaded GitHub asset metadata differs from local file")

    def sync_assets(self, release, folder, verify_dir):
        assets = self.api("/releases/" + str(release["id"]) + "/assets")
        names = [asset["name"] for asset in assets]
        expected = {path.name: path for path in folder.iterdir() if path.is_file()}
        if len(names) != len(set(names)) or set(names) - expected.keys():
            raise ReleaseError("GitHub release contains unexpected or duplicate assets")
        if not release["draft"] and set(names) != set(expected):
            raise ReleaseError(
                "Published GitHub release is incomplete; refusing to alter it"
            )
        for asset in assets:
            local = expected[asset["name"]]
            if asset.get("size") != local.stat().st_size:
                raise ReleaseError(
                    "GitHub asset metadata conflicts with prepared bytes: "
                    + asset["name"]
                )
            path = verify_dir / asset["name"]
            self.download(asset, path)
            if digest(path) != digest(local):
                raise ReleaseError(
                    "GitHub asset conflicts with prepared bytes: " + asset["name"]
                )
        for name in sorted(expected.keys() - set(names)):
            self.upload(release, expected[name])
        assets = self.api("/releases/" + str(release["id"]) + "/assets")
        if sorted(asset["name"] for asset in assets) != sorted(expected):
            raise ReleaseError("GitHub release asset set is incomplete")
        for asset in assets:
            local = expected[asset["name"]]
            if asset.get("size") != local.stat().st_size:
                raise ReleaseError(
                    "GitHub release readback size mismatch: " + asset["name"]
                )
            path = verify_dir / asset["name"]
            self.download(asset, path)
            if digest(path) != digest(local):
                raise ReleaseError(
                    "GitHub release readback checksum mismatch: " + asset["name"]
                )


def github_settings(config):
    github = config.get("github")
    if not isinstance(github, dict):
        raise ReleaseError("Release configuration is missing the [github] table")
    missing = [name for name in ("repository", "token_file") if not github.get(name)]
    if missing:
        raise ReleaseError(
            "GitHub mirror configuration is missing: " + ", ".join(missing)
        )
    token_file = Path(github["token_file"]).expanduser()
    if not token_file.is_file():
        raise ReleaseError("github.token_file does not name a regular file")
    if token_file.stat().st_mode & 0o077:
        raise ReleaseError("github.token_file must have owner-only permissions")
    token = token_file.read_text().strip()
    if not token:
        raise ReleaseError("github.token_file is empty")
    return github["repository"], token


def validate_source_release(release, repository, tag):
    if (
        release.get("tag_name") != tag
        or release.get("draft") is not False
        or not isinstance(release.get("target_commitish"), str)
        or not isinstance(release.get("name"), str)
        or not isinstance(release.get("body"), str)
    ):
        raise ReleaseError("Codefloe did not return the expected published release")
    expected_url = repository + "/releases/tag/" + urllib.parse.quote(tag, safe="")
    if release.get("html_url") != expected_url:
        raise ReleaseError("Codefloe release URL differs from the configured source")


def mirror_release_assets(source_repository, source_release, folder, config):
    """Mirror a verified local copy of a published release to GitHub."""
    from .publish import verify_manifest

    source_repository, _, _ = repository_url(source_repository)
    tag = source_release.get("tag_name", "")
    validate_source_release(source_release, source_repository, tag)
    expected = expected_asset_names(tag)
    if any(path.is_symlink() for path in folder.iterdir()):
        raise ReleaseError("Release assets must not contain symbolic links")
    actual = {path.name for path in folder.iterdir() if path.is_file()}
    if actual != expected:
        raise ReleaseError("Unexpected release asset set; refusing to mirror it")
    verify_manifest(folder)
    destination_repository, token = github_settings(config)
    github = GitHub(destination_repository, token)
    github.repository_check()
    target = source_release["target_commitish"]
    if (
        re.fullmatch(r"[0-9a-fA-F]{40}", target)
        and github.tag_commit(tag).lower() != target.lower()
    ):
        raise ReleaseError("GitHub release tag points at another source revision")
    body = mirror_body(
        source_release["body"], source_repository, github.repository, tag
    )
    release = github.tag_release(tag)
    if release is None:
        release = github.create_release(source_release, body)
    if (
        release.get("tag_name") != tag
        or release.get("name") != source_release["name"]
        or release.get("body") != body
        or bool(release.get("prerelease")) != bool(source_release.get("prerelease"))
    ):
        raise ReleaseError("Existing GitHub release metadata conflicts with Codefloe")
    with tempfile.TemporaryDirectory(prefix="plamenu-github-readback-") as temp:
        readback = Path(temp)
        github.sync_assets(release, folder, readback)
        verify_manifest(readback)
    if release["draft"]:
        release = github.finalize(release)
    if release.get("draft"):
        raise ReleaseError("GitHub did not finalize the mirrored release")
    return release["html_url"]


def mirror_published_release(source_repository, tag, config):
    """Download a public Codefloe release, verify it, and mirror exact bytes."""
    # Fail before downloading large assets when local GitHub settings are absent.
    github_settings(config)
    source = PublicForgejoRelease(source_repository)
    release = source.release(tag)
    validate_source_release(release, source.repository, tag)
    assets = release.get("assets")
    if not isinstance(assets, list):
        raise ReleaseError("Codefloe release did not include an asset list")
    names = [asset.get("name") for asset in assets]
    if len(names) != len(set(names)) or set(names) != expected_asset_names(tag):
        raise ReleaseError("Codefloe release has an unexpected or duplicate asset set")
    with tempfile.TemporaryDirectory(prefix="plamenu-release-import-") as temp:
        folder = Path(temp)
        for asset in assets:
            size = asset.get("size")
            if not isinstance(size, int) or not 0 < size <= MAXIMUM_ASSET_SIZE:
                raise ReleaseError("Codefloe release asset has an invalid size")
            path = folder / asset["name"]
            source.download(asset.get("browser_download_url", ""), path)
            if path.stat().st_size != size:
                raise ReleaseError(
                    "Codefloe release asset size differs from metadata: "
                    + asset["name"]
                )
        return mirror_release_assets(source.repository, release, folder, config)
