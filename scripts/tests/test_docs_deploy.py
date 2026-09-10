"""Check docs identity, signed Git publication, races, and hosted verification."""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
from plamenu_release import docs
from plamenu_release.core import ReleaseError, git


class DocsTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.site = self.root / "site"
        self.site.mkdir()
        (self.site / "nested").mkdir()
        (self.site / "index.html").write_text("<html><main>Home</main></html>")
        (self.site / "nested/page.html").write_text("<html><main>Nested</main></html>")
        self.build = {
            "version": "1.2.3",
            "source": "a" * 40,
            "build_number": "1234",
            "release_tag": None,
            "dirty": False,
        }

    def test_visible_identity_nested_marker_and_deterministic_manifest(self):
        record = docs.stamp(self.site, self.build)
        page = (self.site / "nested/page.html").read_text()
        self.assertIn('href="../build.json"', page)
        self.assertIn("Build 1234", page)
        self.assertIn("Source aaaaaaaaaaaa", page)
        self.assertIn("development", page)
        self.assertNotIn(docs.MARKER, record["files"])
        self.assertEqual(json.loads((self.site / docs.MARKER).read_text()), record)

    def test_dirty_preview_does_not_claim_clean_release(self):
        docs.stamp(self.site, self.build | {"dirty": True, "release_tag": "v1.2.3"})
        self.assertIn("local changes", (self.site / "index.html").read_text())

    def test_signed_push_retry_and_concurrent_push_are_safe(self):
        source = self.root / "source"
        remote = self.root / "remote.git"
        source.mkdir()

        def run(*args):
            return subprocess.run(
                args, cwd=source, capture_output=True, text=True, check=True
            )

        run("git", "init", "--quiet")
        run("git", "init", "--bare", "--quiet", str(remote))
        key = self.root / "signer"
        run("ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(key))
        allowed = self.root / "allowed"
        allowed.write_text("docs@example.test " + key.with_suffix(".pub").read_text())
        for key_name, value in {
            "user.name": "Docs test",
            "user.email": "docs@example.test",
            "gpg.format": "ssh",
            "user.signingkey": str(key),
            "gpg.ssh.allowedSignersFile": str(allowed),
        }.items():
            run("git", "config", key_name, value)
            run("git", "--git-dir=" + str(remote), "config", key_name, value)
        forge = Mock()
        forge.api.return_value = {"commit": {"verification": {"verified": True}}}
        docs.stamp(self.site, self.build)
        args = (
            source,
            self.site,
            str(remote),
            "pages",
            self.root / "publish.log",
            forge,
        )
        first = docs.publish_tree(*args)
        self.assertEqual(docs.publish_tree(*args), first)
        message = run(
            "git", "--git-dir=" + str(remote), "show", "-s", "--format=%B", first
        ).stdout
        self.assertIn("Signed-off-by: Docs test <docs@example.test>", message)
        # A publisher advances the branch after our fetch; a normal push must reject it.
        (self.site / "index.html").write_text("changed")
        original = docs.Runner.run

        def race(runner, *command, **kwargs):
            if command[:2] == ("git", "push"):
                tree = git(runner.source, "rev-parse", first + "^{tree}")
                advanced = run(
                    "git",
                    "--git-dir=" + str(remote),
                    "commit-tree",
                    tree,
                    "-p",
                    first,
                    "-S",
                    "-m",
                    "Concurrent publisher\n\nSigned-off-by: Docs test <docs@example.test>",
                ).stdout.strip()
                run(
                    "git",
                    "--git-dir=" + str(remote),
                    "update-ref",
                    "refs/heads/pages",
                    advanced,
                )
            return original(runner, *command, **kwargs)

        with patch.object(docs.Runner, "run", race), self.assertRaises(ReleaseError):
            docs.publish_tree(*args)
        self.assertNotEqual(
            git(source, "ls-remote", str(remote), "refs/heads/pages").split()[0], first
        )

    def test_hosted_timeout_is_retryable_and_does_not_send_credentials(self):
        record = docs.stamp(self.site, self.build)
        requests = []

        def response(request, **kwargs):
            requests.append(request)
            result = Mock()
            result.__enter__ = Mock(return_value=result)
            result.__exit__ = Mock(return_value=False)
            result.read.return_value = b"{}"
            return result

        opener = Mock()
        opener.open.side_effect = response
        with (
            patch.object(docs.urllib.request, "build_opener", return_value=opener),
            patch.object(docs.time, "sleep"),
            patch.object(docs.time, "monotonic", side_effect=[0, 0, 2, 2]),
        ):
            with self.assertRaisesRegex(
                ReleaseError, "Docs pushed; hosted verification timed out"
            ):
                docs.verify_site("https://docs.example/", record, timeout=1)
        self.assertTrue(requests)
        self.assertNotIn("Authorization", requests[0].headers)

    def test_hosted_verification_checks_all_bytes(self):
        record = docs.stamp(self.site, self.build)
        requested = []

        def response(request, **kwargs):
            path = docs.urllib.parse.urlsplit(request.full_url).path.lstrip("/")
            requested.append(path)
            result = Mock()
            result.__enter__ = Mock(return_value=result)
            result.__exit__ = Mock(return_value=False)
            result.read.return_value = (self.site / path).read_bytes()
            return result

        opener = Mock()
        opener.open.side_effect = response
        with patch.object(docs.urllib.request, "build_opener", return_value=opener):
            docs.verify_site("https://docs.example/", record)
        self.assertEqual(set(requested), set(record["files"]) | {docs.MARKER})

    def test_invalid_config_and_source_output_are_rejected(self):
        for config in ({"docs": "yes"}, {"docs": {"enabled": "true"}}):
            with self.assertRaises(ReleaseError):
                docs.enabled(config)
        with self.assertRaises(ReleaseError):
            docs.settings(
                {
                    "docs": {
                        "enabled": True,
                        "url": "https://docs.example/",
                        "branch": "main",
                    }
                }
            )
        with self.assertRaises(ReleaseError):
            docs.build_site(self.root, self.root / "docs", self.root / "log")

    def test_separate_repository_uses_its_main_without_source_remote_changes(self):
        token = self.root / "token"
        token.write_text("test-placeholder")
        config = {
            "repository": "https://codefloe.com/plamenu/plamenu",
            "token_file": str(token),
            "docs": {
                "enabled": True,
                "repository": "https://codefloe.com/plamenu/docs",
                "branch": "main",
                "url": "https://docs.plamenu.codefloe.page/",
            },
        }
        self.assertEqual(
            docs.settings(config), ("https://docs.plamenu.codefloe.page/", "main")
        )
        self.assertEqual(
            docs.remote_url(self.root, config), "git@codefloe.com:plamenu/docs.git"
        )
        config["docs"]["repository"] = config["repository"]
        with self.assertRaises(ReleaseError):
            docs.settings(config)
        config["docs"]["repository"] = "https://another.example/plamenu/docs"
        with self.assertRaisesRegex(ReleaseError, "same forge"):
            docs.settings(config)

    def test_release_docs_failure_preserves_successful_release(self):
        with patch.object(
            docs, "deploy", side_effect=ReleaseError("host offline")
        ) as deploy:
            with self.assertRaisesRegex(
                ReleaseError, "Release published; docs pending"
            ):
                docs.after_release(
                    self.root, self.root, self.build, {"docs": {"enabled": True}}
                )
            self.assertEqual(deploy.call_args.kwargs["expected"], self.build)
        with patch.object(docs, "deploy") as deploy:
            docs.after_release(self.root, self.root, self.build, {})
            deploy.assert_not_called()


if __name__ == "__main__":
    unittest.main()
