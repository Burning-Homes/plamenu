"""Exercise release mirroring, retries, and credential boundaries."""

import sys
import tempfile
import unittest
import urllib.error
from pathlib import Path
from unittest.mock import Mock, patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from plamenu_release import publish, release_mirror
from plamenu_release.core import ReleaseError, digest


class MirrorTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.assets = self.root / "assets"
        self.assets.mkdir()
        for name in release_mirror.expected_asset_names("v1.2.3") - {"SHA256SUMS"}:
            (self.assets / name).write_text("fixture " + name)
        (self.assets / "SHA256SUMS").write_text(publish.manifest(self.assets))
        self.token = self.root / "github-token"
        self.token.write_text("fixture-token")
        self.token.chmod(0o600)
        self.config = {
            "github": {
                "repository": "https://github.com/team/repo",
                "token_file": str(self.token),
            }
        }
        self.source = {
            "tag_name": "v1.2.3",
            "target_commitish": "a" * 40,
            "name": "Plamenu v1.2.3",
            "body": (
                "[binary](https://codefloe.invalid/team/repo/releases/"
                "download/v1.2.3/plamenu-1.2.3-linux-amd64.tar.gz)"
            ),
            "draft": False,
            "prerelease": False,
            "html_url": "https://codefloe.invalid/team/repo/releases/tag/v1.2.3",
        }

    def test_body_points_downloads_at_github_and_keeps_canonical_notice(self):
        body = release_mirror.mirror_body(
            self.source["body"],
            "https://codefloe.invalid/team/repo",
            "https://github.com/team/repo",
            "v1.2.3",
        )
        self.assertIn("canonical [Codefloe release]", body)
        self.assertIn("https://github.com/team/repo/releases/download/", body)
        self.assertNotIn("codefloe.invalid/team/repo/releases/download", body)

    def test_credentials_are_limited_to_github_api_hosts(self):
        github = release_mirror.GitHub("https://github.com/team/repo", "secret")
        for url in (
            "https://github.com/team/repo",
            "https://api.github.com.evil.invalid/repos/team/repo",
            "http://api.github.com/repos/team/repo",
            "https://user@api.github.com/repos/team/repo",
        ):
            with self.assertRaises(ReleaseError):
                github.request(url)
        with self.assertRaises(ReleaseError):
            github.download(
                {"url": "https://api.github.com/asset"}, self.root / "download"
            )
        with self.assertRaisesRegex(ReleaseError, "upload URL"):
            github.upload(
                {
                    "id": 1,
                    "upload_url": "https://uploads.github.com/repos/other/repo/1",
                },
                self.assets / "cosign.pub",
            )

    def test_draft_asset_redirect_uses_an_unauthenticated_opener(self):
        github = release_mirror.GitHub("https://github.com/team/repo", "secret")
        url = github.base + "/releases/assets/9"
        github.opener.open = Mock(
            side_effect=urllib.error.HTTPError(
                url,
                302,
                "Found",
                {"Location": "https://release-assets.githubusercontent.com/file"},
                None,
            )
        )
        reader = Mock()
        reader.read.side_effect = [b"exact", b""]
        with patch.object(release_mirror, "RangeReader", return_value=reader) as make:
            github.download({"url": url, "size": 5}, self.root / "download")
        self.assertEqual((self.root / "download").read_bytes(), b"exact")
        self.assertIs(make.call_args.args[0], github.public_opener)
        self.assertEqual(make.call_args.args[2], {"User-Agent": "plamenu"})

    def test_draft_is_finalized_only_after_readback_and_retry_is_idempotent(self):
        events = []
        release = {
            "id": 7,
            "tag_name": "v1.2.3",
            "name": self.source["name"],
            "body": release_mirror.mirror_body(
                self.source["body"],
                "https://codefloe.invalid/team/repo",
                "https://github.com/team/repo",
                "v1.2.3",
            ),
            "draft": True,
            "prerelease": False,
        }
        github = Mock(repository="https://github.com/team/repo")
        github.tag_commit.return_value = "a" * 40
        github.tag_release.return_value = release

        def sync(current, folder, readback):
            events.append("readback")
            for path in folder.iterdir():
                (readback / path.name).write_bytes(path.read_bytes())

        github.sync_assets.side_effect = sync

        def finalize(current):
            events.append("finalize")
            return current | {
                "draft": False,
                "html_url": "https://github.com/team/repo/releases/tag/v1.2.3",
            }

        github.finalize.side_effect = finalize
        with patch.object(release_mirror, "GitHub", return_value=github):
            url = release_mirror.mirror_release_assets(
                "https://codefloe.invalid/team/repo",
                self.source,
                self.assets,
                self.config,
            )
        self.assertEqual(events, ["readback", "finalize"])
        self.assertEqual(url, "https://github.com/team/repo/releases/tag/v1.2.3")
        github.create_release.assert_not_called()

    def test_readback_failure_keeps_draft_unpublished(self):
        release = {
            "id": 7,
            "tag_name": "v1.2.3",
            "name": self.source["name"],
            "body": release_mirror.mirror_body(
                self.source["body"],
                "https://codefloe.invalid/team/repo",
                "https://github.com/team/repo",
                "v1.2.3",
            ),
            "draft": True,
            "prerelease": False,
        }
        github = Mock(repository="https://github.com/team/repo")
        github.tag_commit.return_value = "a" * 40
        github.tag_release.return_value = release
        github.sync_assets.side_effect = ReleaseError("readback failed")
        with (
            patch.object(release_mirror, "GitHub", return_value=github),
            self.assertRaisesRegex(ReleaseError, "readback failed"),
        ):
            release_mirror.mirror_release_assets(
                "https://codefloe.invalid/team/repo",
                self.source,
                self.assets,
                self.config,
            )
        github.finalize.assert_not_called()

    def test_asset_retry_fills_a_draft_and_never_changes_a_published_release(self):
        folder = self.root / "small-assets"
        folder.mkdir()
        (folder / "binary").write_bytes(b"tested")
        (folder / "report").write_bytes(b"passed")
        readback = self.root / "readback"
        readback.mkdir()
        remote = {"binary": b"tested"}
        github = release_mirror.GitHub("https://github.com/team/repo", "fixture-token")
        github.api = Mock(
            side_effect=lambda *args: [
                {
                    "name": name,
                    "size": len(content),
                    "browser_download_url": "https://github.com/team/repo/" + name,
                }
                for name, content in remote.items()
            ]
        )
        github.download = Mock(
            side_effect=lambda asset, target: target.write_bytes(remote[asset["name"]])
        )
        github.upload = Mock(
            side_effect=lambda release, path: remote.update(
                {path.name: path.read_bytes()}
            )
        )
        github.sync_assets({"id": 1, "draft": True}, folder, readback)
        self.assertEqual(remote, {"binary": b"tested", "report": b"passed"})
        github.upload.reset_mock()
        remote["binary"] = b"changed"
        with self.assertRaisesRegex(ReleaseError, "metadata conflicts"):
            github.sync_assets({"id": 1, "draft": True}, folder, readback)
        github.upload.assert_not_called()
        remote["binary"] = b"tested"
        del remote["report"]
        with self.assertRaisesRegex(ReleaseError, "incomplete"):
            github.sync_assets({"id": 1, "draft": False}, folder, readback)

    def test_backfill_downloads_public_assets_before_calling_github(self):
        source = Mock(repository="https://codefloe.invalid/team/repo")
        source.release.return_value = self.source | {
            "assets": [
                {
                    "name": path.name,
                    "size": path.stat().st_size,
                    "browser_download_url": (
                        "https://codefloe.invalid/team/repo/releases/download/"
                        "v1.2.3/" + path.name
                    ),
                }
                for path in self.assets.iterdir()
            ]
        }

        def download(url, destination):
            destination.write_bytes((self.assets / url.rsplit("/", 1)[1]).read_bytes())

        source.download.side_effect = download

        def verify_forwarded(repository, release, folder, config):
            self.assertEqual(repository, source.repository)
            self.assertEqual(release["tag_name"], "v1.2.3")
            self.assertEqual(
                {path.name: digest(path) for path in folder.iterdir()},
                {path.name: digest(path) for path in self.assets.iterdir()},
            )
            self.assertIs(config, self.config)
            return "https://github.com/team/repo/releases/tag/v1.2.3"

        with (
            patch.object(release_mirror, "PublicForgejoRelease", return_value=source),
            patch.object(
                release_mirror,
                "mirror_release_assets",
                side_effect=verify_forwarded,
            ) as mirror,
        ):
            url = release_mirror.mirror_published_release(
                source.repository, "v1.2.3", self.config
            )
        self.assertEqual(url, "https://github.com/team/repo/releases/tag/v1.2.3")
        self.assertEqual(source.download.call_count, len(list(self.assets.iterdir())))
        mirror.assert_called_once()

    def test_token_file_must_be_owner_only(self):
        self.token.chmod(0o644)
        with self.assertRaisesRegex(ReleaseError, "owner-only"):
            release_mirror.github_settings(self.config)


if __name__ == "__main__":
    unittest.main()
