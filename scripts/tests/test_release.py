"""Exercise release failures, resume, worker transport and immutable publication."""

import errno
import importlib.util
import json
import os
import subprocess
import sys
import tarfile
import tempfile
import time
import unittest
import urllib.error
import zipfile
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
from plamenu_release import build, checks, core, publish, worker
from plamenu_release.forge import Forge

SPEC = importlib.util.spec_from_file_location(
    "release_cli", ROOT / "scripts/release.py"
)
CLI = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CLI)


class TemporaryTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.identity = {
            "source": "a" * 40,
            "version": "1.2.3",
            "jobs": 2,
            "arch": "amd64",
            "target": build.TARGETS["amd64"],
        }


class StageTests(TemporaryTest):
    def test_reuses_only_complete_matching_unchanged_outputs(self):
        action = Mock(side_effect=lambda r, p: (p / "result").write_text("tested"))
        args = (self.root, self.root, "build", self.identity, action)
        core.stage(*args)
        core.stage(*args)
        self.assertEqual(action.call_count, 1)
        (self.root / "build/result").write_text("substituted")
        core.stage(*args)
        self.assertEqual(action.call_count, 2)
        core.stage(*args, rerun=True)
        self.assertEqual(action.call_count, 3)
        core.stage(self.root, self.root, "build", self.identity | {"jobs": 4}, action)
        self.assertEqual(action.call_count, 4)
        (self.root / "build/extra").write_text("unexpected")
        core.stage(self.root, self.root, "build", self.identity | {"jobs": 4}, action)
        self.assertEqual(action.call_count, 5)

    def test_failed_and_expired_stages_are_not_reused(self):
        with self.assertRaises(core.ReleaseError):
            core.stage(
                self.root,
                self.root,
                "static",
                self.identity,
                Mock(side_effect=core.ReleaseError("test failed")),
            )
        self.assertEqual(
            json.loads((self.root / "static/stage.json").read_text())["status"],
            "failed",
        )
        action = Mock()
        record = core.stage(self.root, self.root, "static", self.identity, action)
        record["finished"] = time.time() - 86401
        core.write_json(self.root / "static/stage.json", record)
        core.stage(self.root, self.root, "static", self.identity, action, max_age=86400)
        self.assertEqual(action.call_count, 2)

    def test_nested_stage_files_are_hashed_and_symlinks_rejected(self):
        (self.root / "nested").mkdir()
        (self.root / "nested/stage.json").write_text("evidence")
        self.assertIn("nested/stage.json", core.files(self.root))
        (self.root / "link").symlink_to(self.root / "nested/stage.json")
        with self.assertRaises(core.ReleaseError):
            core.files(self.root)

    def test_local_preparation_needs_no_config_and_publish_reports_missing_settings(
        self,
    ):
        args = CLI.parser().parse_args([])
        self.assertEqual(CLI.configuration_errors(args, {}), [])
        args.publish = True
        errors = CLI.configuration_errors(args, {})
        self.assertIn("missing signing_key", errors)
        self.assertIn("missing token_file", errors)

    def test_release_environment_ignores_developer_overrides(self):
        with patch.dict(
            os.environ,
            {
                "DATABASE_URL": "production",
                "PLAMENU_DRILL_IMAGE": "wrong",
                "COSIGN_PASSWORD": "secret",
                "PLAMENU_BENCH_RATIO_BUDGET": "999",
                "CARGO_PROFILE_RELEASE_PANIC": "abort",
            },
        ):
            env = core.environment()
        for name in (
            "DATABASE_URL",
            "PLAMENU_DRILL_IMAGE",
            "COSIGN_PASSWORD",
            "PLAMENU_BENCH_RATIO_BUDGET",
            "CARGO_PROFILE_RELEASE_PANIC",
        ):
            self.assertNotIn(name, env)
        self.assertEqual(env["SQLX_OFFLINE"], "true")

    def test_database_launch_failure_still_cleans_its_own_container(self):
        runner = Mock()
        runner.run.side_effect = core.ReleaseError("lost launch response")
        with (
            patch.object(checks.subprocess, "run") as cleanup,
            self.assertRaises(core.ReleaseError),
            checks.database(runner),
        ):
            self.fail("failed database must not be used")
        command = cleanup.call_args.args[0]
        self.assertEqual(command[:4], ["docker", "rm", "--force", "--volumes"])
        self.assertTrue(command[4].startswith("plamenu-release-db-"))


class PipelineTests(TemporaryTest):
    def setUp(self):
        super().setUp()
        self.source = self.root / "repo"
        self.source.mkdir()

        def git(*args):
            return subprocess.check_output(
                ["git", *args], cwd=self.source, text=True
            ).strip()

        self.git = git
        git("init", "-q")
        git("config", "user.name", "Release Test")
        git("config", "user.email", "test@example.com")
        git("config", "commit.gpgsign", "false")
        (self.source / "Cargo.toml").write_text(
            '[workspace.package]\nversion = "1.2.3"\n'
        )
        (self.source / "CHANGELOG.md").write_text(
            "## [1.2.3] - 2026-09-10\n\nRelease notes.\n"
        )
        (self.source / ".gitignore").write_text("/release-artifacts/\n")
        git("add", ".")
        git("commit", "-qm", "fixture")
        self.original_run = subprocess.run
        self.calls = []

        def host_run(args, **kwargs):
            if args[0] == "docker":
                return subprocess.CompletedProcess(
                    args,
                    0,
                    stdout="aarch64\n" if "linux/arm64" in args else "x86_64\n",
                    stderr="",
                )
            return self.original_run(args, **kwargs)

        self.start_patch(patch.object(CLI.subprocess, "run", side_effect=host_run))
        self.start_patch(patch.object(CLI.Path, "cwd", return_value=self.source))
        self.start_patch(patch.object(CLI, "prerequisites", return_value=[]))
        self.start_patch(patch.object(CLI, "configuration_errors", return_value=[]))
        self.start_patch(patch.object(CLI.platform, "system", return_value="Linux"))
        self.start_patch(patch.object(CLI.platform, "machine", return_value="x86_64"))
        self.static = self.start_patch(
            patch.object(checks, "static", side_effect=self.check("static"))
        )
        self.app = self.start_patch(
            patch.object(checks, "application", side_effect=self.check("application"))
        )
        self.artifacts = self.start_patch(
            patch.object(checks, "artifacts", side_effect=self.check("artifacts"))
        )
        self.build = self.start_patch(
            patch.object(build, "build", side_effect=self.build_files)
        )
        self.start_patch(patch.object(build, "verify_build", return_value={}))
        self.start_patch(patch.object(build, "source_tree", side_effect=core.digest))
        self.publisher = self.start_patch(patch.object(CLI, "publish"))
        self.e2e = self.start_patch(patch.object(checks, "e2e"))
        self.perf = self.start_patch(patch.object(checks, "performance"))

    def start_patch(self, value):
        result = value.start()
        self.addCleanup(value.stop)
        return result

    def check(self, name):
        def run(runner, folder, *args):
            self.calls.append(name)
            self.assertNotEqual(runner.source, self.source)
            self.assertEqual(
                core.git(runner.source, "rev-parse", "HEAD"),
                self.git("rev-parse", "HEAD"),
            )
            (folder / "result.json").write_text('{"status":"passed"}\n')

        return run

    def build_files(self, runner, folder, identity):
        self.calls.append("build-" + identity["arch"])
        (folder / build.binary_name(identity["version"], identity["arch"])).write_bytes(
            f"build {self.build.call_count}".encode()
        )
        (folder / build.source_name(identity["version"])).write_bytes(b"same source")

    def args(self, *flags):
        return CLI.parser().parse_args(
            ["--config", str(self.root / "absent.toml"), *flags]
        )

    def test_prepare_then_publish_automatically_reuses_exact_checked_bytes(self):
        CLI.run(self.args("--skip-e2e"))
        self.assertEqual(
            self.calls,
            [
                "static",
                "application",
                "build-amd64",
                "artifacts",
                "build-arm64",
                "artifacts",
            ],
        )
        self.publisher.assert_not_called()
        assets = next((self.source / "release-artifacts").glob("*/*/assets"))
        report = json.loads((assets / "release.json").read_text())
        self.assertEqual(report["checks"]["e2e"]["status"], "not-run")
        self.assertEqual(report["checks"]["performance"]["status"], "not-run")
        self.e2e.assert_not_called()
        self.perf.assert_not_called()
        before = core.files(assets)
        CLI.run(self.args("--publish"))
        self.assertEqual(self.build.call_count, 2)
        self.assertEqual(self.artifacts.call_count, 2)
        self.publisher.assert_called_once()
        self.assertEqual(before, core.files(assets))

    def test_publication_retry_preserves_check_results_after_a_day(self):
        CLI.run(self.args())
        stage_file = next(
            (self.source / "release-artifacts").glob("*/*/static/stage.json")
        )
        record = json.loads(stage_file.read_text())
        record["finished"] = time.time() - 86401
        core.write_json(stage_file, record)
        (stage_file.parent.parent / "publication").mkdir()
        CLI.run(self.args("--publish"))
        self.assertEqual(self.static.call_count, 1)
        CLI.run(self.args("--rerun"))
        self.assertEqual(self.static.call_count, 2)

    def test_check_failure_prevents_build_and_publication_and_retry_resumes(self):
        self.app.side_effect = core.ReleaseError("application failed")
        with self.assertRaises(core.ReleaseError):
            CLI.run(self.args("--publish"))
        self.build.assert_not_called()
        self.publisher.assert_not_called()
        self.app.side_effect = self.check("application")
        CLI.run(self.args("--publish"))
        self.assertEqual(self.static.call_count, 1)
        self.publisher.assert_called_once()

    def test_corrupt_build_rebuilds_and_retests_new_bytes(self):
        CLI.run(self.args())
        binary = next(
            (self.source / "release-artifacts").glob(
                "*/*/build-amd64/plamenu-*-linux-amd64.tar.gz"
            )
        )
        binary.write_bytes(b"corrupt")
        CLI.run(self.args())
        self.assertEqual(self.static.call_count, 1)
        self.assertEqual(self.build.call_count, 3)
        self.assertEqual(self.artifacts.call_count, 3)

    def test_check_only_never_builds_and_dirty_source_never_runs_checks(self):
        CLI.run(self.args("--check-only"))
        self.build.assert_not_called()
        (self.source / "pending").write_text("uncommitted")
        with self.assertRaisesRegex(core.ReleaseError, "pending source"):
            CLI.run(self.args())
        self.assertEqual(self.static.call_count, 1)

    def test_release_notes_required_and_publish_check_only_rejected(self):
        with self.assertRaisesRegex(core.ReleaseError, "cannot publish"):
            CLI.run(self.args("--check-only", "--publish"))
        (self.source / "CHANGELOG.md").write_text(
            "## [Unreleased]\n\nNot yet versioned.\n"
        )
        with self.assertRaisesRegex(core.ReleaseError, "release notes"):
            CLI.notes_for(self.source, "1.2.3")


class WorkerTests(TemporaryTest):
    def test_unpack_rejects_traversal_symlinks_duplicate_names_and_oversize(self):
        for mode in (
            "traversal",
            "absolute",
            "symlink",
            "hardlink",
            "duplicate",
            "oversize",
        ):
            with self.subTest(mode=mode):
                archive = self.root / "bundle.tar"
                with tarfile.open(archive, "w") as tar:
                    info = tarfile.TarInfo(
                        {"traversal": "../escape", "absolute": "/escape"}.get(
                            mode, "result"
                        )
                    )
                    if mode in ("symlink", "hardlink"):
                        info.type = (
                            tarfile.SYMTYPE if mode == "symlink" else tarfile.LNKTYPE
                        )
                        info.linkname = "../escape"
                    if mode == "oversize":
                        # Header alone lets validation reject before reading content.
                        info.size = 601 * 1024**2
                        archive_bytes = info.tobuf()
                    else:
                        tar.addfile(info)
                        if mode == "duplicate":
                            tar.addfile(info)
                if mode == "oversize":
                    # Sparse the payload so the fixture consumes no physical 600 MiB.
                    with archive.open("wb") as stream:
                        stream.write(archive_bytes)
                        stream.seek(512 + info.size)
                        stream.write(b"\0" * 1024)
                with self.assertRaises((core.ReleaseError, tarfile.ReadError)):
                    worker.unpack_bundle(archive, self.root / "out")
                self.assertFalse((self.root.parent / "escape").exists())

    def test_worker_result_is_downloaded_verified_and_pending_request_is_reused(self):
        key = core.fingerprint({"identity": self.identity, "stage": "build"})
        core.write_json(
            self.root / "worker-build-amd64.json",
            {
                "key": key,
                "request": "request1",
                "repository": "https://forge.invalid/team/repo",
                "run_id": 9,
            },
        )
        token = self.root / "token"
        token.write_text("fixture")
        evidence = self.root / "evidence"
        evidence.mkdir()
        (evidence / "result").write_text("built bytes")
        core.write_json(
            evidence / "stage.json",
            {"key": key, "status": "passed", "files": core.files(evidence)},
        )
        bundle = self.root / "bundle.tar.gz"
        with tarfile.open(bundle, "w:gz") as tar:
            for file in evidence.iterdir():
                tar.add(file, arcname=file.name)

        def download(url, target, maximum):
            with zipfile.ZipFile(target, "w") as archive:
                archive.write(bundle, "bundle.tar.gz")

        forge = Mock(repository="https://forge.invalid/team/repo")
        responses = [
            {
                "id": 9,
                "commit_sha": self.identity["source"],
                "workflow_id": "release-worker.yml",
                "status": "success",
                "html_url": "https://forge.invalid/run/9",
            },
            [
                {
                    "name": "release-request1",
                    "run_id": 9,
                    "archive_download_url": "https://forge.invalid/artifact",
                }
            ],
        ]
        forge.api.side_effect = responses
        forge.download.side_effect = download
        output = self.root / "output"
        output.mkdir()
        with patch.object(worker, "Forge", return_value=forge):
            worker.remote(
                self.root,
                self.root,
                output,
                self.identity,
                "build",
                {"repository": forge.repository, "token_file": str(token)},
                "main",
            )
        self.assertEqual((output / "result").read_text(), "built bytes")
        self.assertEqual(forge.api.call_args_list[0].args, ("/actions/runs/9",))
        self.assertTrue(
            json.loads((self.root / "worker-build-amd64.json").read_text())["complete"]
        )
        self.assertFalse((output / "stage.json").exists())

        # Corrupt evidence is rejected, and retry must request a new worker result.
        state_path = self.root / "worker-build-amd64.json"
        state = json.loads(state_path.read_text())
        state.pop("complete")
        core.write_json(state_path, state)
        (evidence / "result").write_text("substituted worker bytes")
        with tarfile.open(bundle, "w:gz") as tar:
            for file in evidence.iterdir():
                tar.add(file, arcname=file.name)
        forge.api.side_effect = responses
        with (
            patch.object(worker, "Forge", return_value=forge),
            self.assertRaisesRegex(core.ReleaseError, "Invalid worker result"),
        ):
            worker.remote(
                self.root,
                self.root,
                output,
                self.identity,
                "build",
                {"repository": forge.repository, "token_file": str(token)},
                "main",
            )
        self.assertTrue(json.loads(state_path.read_text())["failed"])

    def test_dispatch_rejection_allows_a_new_attempt(self):
        token = self.root / "token"
        token.write_text("fixture")
        forge = Mock(repository="https://forge.invalid/team/repo")
        forge.api.side_effect = urllib.error.HTTPError(
            "https://forge.invalid", 422, "bad ref", {}, None
        )
        with (
            patch.object(worker, "Forge", return_value=forge),
            self.assertRaises(urllib.error.HTTPError),
        ):
            worker.remote(
                self.root,
                self.root,
                self.root / "out",
                self.identity,
                "build",
                {"repository": forge.repository, "token_file": str(token)},
                "main",
            )
        self.assertTrue(
            json.loads((self.root / "worker-build-amd64.json").read_text())["failed"]
        )


class PublicationTests(TemporaryTest):
    def test_registry_errors_cannot_be_mistaken_for_an_unused_tag(self):
        runner = SimpleNamespace(env={})
        for message in (
            b"unauthorized",
            b"connection refused",
            b"TLS error",
            b"repository access denied",
            b"authorization endpoint: not found",
        ):
            with (
                self.subTest(message=message),
                patch.object(
                    publish.subprocess,
                    "run",
                    return_value=SimpleNamespace(returncode=1, stderr=message),
                ),
                self.assertRaises(core.ReleaseError),
            ):
                publish.registry_digest(runner, "registry.invalid/project:v1")
        with patch.object(
            publish.subprocess,
            "run",
            return_value=SimpleNamespace(
                returncode=1, stderr=b"ERROR: registry.invalid/project:v1: not found\n"
            ),
        ):
            self.assertIsNone(
                publish.registry_digest(runner, "registry.invalid/project:v1")
            )
        with patch.object(
            publish.subprocess,
            "run",
            return_value=SimpleNamespace(
                returncode=0, stdout=json.dumps("sha256:" + "b" * 64).encode()
            ),
        ):
            self.assertEqual(
                publish.registry_digest(runner, "image"), "sha256:" + "b" * 64
            )

    def test_manifest_detects_changed_missing_and_added_assets(self):
        (self.root / "binary").write_text("tested")
        (self.root / "SHA256SUMS").write_text(publish.manifest(self.root))
        publish.verify_manifest(self.root)
        (self.root / "extra").write_text("unsigned")
        with self.assertRaises(core.ReleaseError):
            publish.verify_manifest(self.root)
        (self.root / "extra").unlink()
        (self.root / "binary").unlink()
        with self.assertRaises(core.ReleaseError):
            publish.verify_manifest(self.root)

    def test_forge_never_sends_credentials_to_another_origin(self):
        forge = Forge("https://forge.invalid/team/repo", "fixture")
        for url in (
            "https://other.invalid/asset",
            "http://forge.invalid/asset",
            "https://user@forge.invalid/asset",
        ):
            with self.assertRaises(core.ReleaseError):
                forge.request(url)
            with self.assertRaises(core.ReleaseError):
                forge.download(url, self.root / "download")

    def test_asset_retry_reads_matching_files_and_never_overwrites_a_conflict(self):
        folder = self.root / "assets"
        folder.mkdir()
        (folder / "binary").write_text("tested")
        (folder / "report").write_text("passed")
        readback = self.root / "readback"
        readback.mkdir()
        remote = {"binary": b"tested"}
        forge = Forge("https://forge.invalid/team/repo", "fixture")
        forge.api = Mock(
            side_effect=lambda *args: [
                {"name": name, "browser_download_url": "https://forge.invalid/" + name}
                for name in remote
            ]
        )
        forge.download = Mock(
            side_effect=lambda url, dest: dest.write_bytes(
                remote[url.rsplit("/", 1)[1]]
            )
        )
        forge.upload = Mock(
            side_effect=lambda release, path: remote.update(
                {path.name: path.read_bytes()}
            )
        )
        forge.sync_assets({"id": 1, "draft": True}, folder, readback)
        self.assertEqual(forge.upload.call_count, 1)
        self.assertEqual(remote["report"], b"passed")
        forge.upload.reset_mock()
        remote["binary"] = b"conflicting"
        with self.assertRaisesRegex(core.ReleaseError, "conflicts"):
            forge.sync_assets({"id": 1, "draft": True}, folder, readback)
        forge.upload.assert_not_called()
        del remote["report"]
        with self.assertRaisesRegex(core.ReleaseError, "incomplete"):
            forge.sync_assets({"id": 1, "draft": False}, folder, readback)


class SignedTagTests(TemporaryTest):
    def test_normal_signed_tag_reuses_signature_and_rejects_source_conflict(self):
        def command(*args):
            return subprocess.check_output(
                [str(a) for a in args], cwd=self.root, stderr=subprocess.PIPE, text=True
            ).strip()

        key = self.root / "signer"
        command("ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", key)
        allowed = self.root / "allowed"
        allowed.write_text("test@example.com " + key.with_suffix(".pub").read_text())
        command("git", "init", "-q")
        command("git", "config", "user.name", "Release Test")
        command("git", "config", "user.email", "test@example.com")
        command("git", "config", "user.signingkey", key)
        command(
            "git",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-qm",
            "fixture",
        )
        sha = command("git", "rev-parse", "HEAD")
        remote = self.root / "remote.git"
        command("git", "init", "--bare", "-q", remote)
        command("git", "remote", "add", "release", remote)
        runner = core.Runner(self.root, self.root / "signing.log")
        publish.prepare_tag(runner, "v1.2.3", sha, allowed, "release")
        tag = command("git", "rev-parse", "refs/tags/v1.2.3")
        command("git", "push", "release", "refs/tags/v1.2.3")
        publish.prepare_tag(runner, "v1.2.3", sha, allowed, "release")
        self.assertEqual(command("git", "rev-parse", "refs/tags/v1.2.3"), tag)
        command("git", "tag", "-d", "v1.2.3")
        publish.prepare_tag(runner, "v1.2.3", sha, allowed, "release")
        self.assertEqual(command("git", "rev-parse", "refs/tags/v1.2.3"), tag)
        command(
            "git",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-qm",
            "changed",
        )
        with self.assertRaisesRegex(core.ReleaseError, "another source"):
            publish.prepare_tag(
                runner,
                "v1.2.3",
                command("git", "rev-parse", "HEAD"),
                allowed,
                "release",
            )
        allowed.write_text("")
        with self.assertRaises(core.ReleaseError):
            publish.prepare_tag(runner, "v1.2.3", sha, allowed, "release")


class PublishFlowTests(TemporaryTest):
    def test_readback_verification_precedes_finalization_and_can_resume(self):
        assets = self.root / "assets"
        assets.mkdir()
        for name in publish.prepared_names(self.identity["version"]) - {"SHA256SUMS"}:
            (assets / name).write_text("prepared " + name)
        (assets / "SHA256SUMS").write_text(publish.manifest(assets))
        (self.root / "build").mkdir()
        (self.root / "build/image.json").write_text("{}")
        (self.root / "token").write_text("fixture-token")
        (self.root / "public").write_text("public fixture")
        config = {
            "repository": "https://forge.invalid/team/repo",
            "registry": "forge.invalid/team/image",
            "username": "test",
            "token_file": str(self.root / "token"),
            "signing_key": str(self.root / "key"),
            "public_key": str(self.root / "public"),
        }
        forge = Mock()
        release = {
            "id": 1,
            "tag_name": "v1.2.3",
            "draft": True,
            "html_url": "https://forge.invalid/release/1",
        }
        forge.tag_release.return_value = None
        events = []
        fail_readback = True

        def api(path, method="GET", body=None):
            if path == "/releases":
                return release
            if method == "PATCH":
                events.append("finalize")
                return release | {"draft": False}
            return []

        forge.api.side_effect = api

        def copy_assets(release, source, target):
            events.append("readback")
            for file in source.iterdir():
                (target / file.name).write_bytes(file.read_bytes())

        forge.sync_assets.side_effect = copy_assets
        runner = Mock(env={})

        def run(*args, **kwargs):
            nonlocal fail_readback
            words = [str(a) for a in args]
            if words == ["docker", "context", "show"]:
                return "default"
            if words[:2] == ["cosign", "sign-blob"]:
                Path(words[words.index("--bundle") + 1]).write_text("signature fixture")
            if (
                words[:2] == ["cosign", "verify-blob"]
                and Path(words[-1]).parent.name == "readback"
            ):
                if fail_readback:
                    raise core.ReleaseError("readback signature failed")
                events.append("verified")
            return ""

        runner.run.side_effect = run
        replace = Path.replace

        def separate_filesystems(path, destination):
            # Model a checkout on a different mount from the system temp dir.
            if path.is_relative_to(self.root) != Path(destination).is_relative_to(
                self.root
            ):
                raise OSError(errno.EXDEV, "Invalid cross-device link")
            return replace(path, destination)

        with (
            patch.object(Path, "replace", separate_filesystems),
            patch.object(publish, "Forge", return_value=forge),
            patch.object(publish, "Runner", return_value=runner),
            patch.object(publish, "git", return_value=config["repository"] + ".git"),
            patch.object(publish, "prepare_tag"),
            patch.object(publish, "registry_digest", return_value="sha256:" + "b" * 64),
            patch.object(publish, "verify_image"),
            patch.object(
                publish,
                "publish_images",
                return_value=(
                    "forge.invalid/team/image@sha256:" + "b" * 64,
                    {"amd64": "sha256:" + "c" * 64, "arm64": "sha256:" + "d" * 64},
                ),
            ),
        ):
            with self.assertRaisesRegex(core.ReleaseError, "signature failed"):
                publish.publish(self.root, self.root, self.identity, config, "Notes")
            self.assertNotIn("finalize", events)
            fail_readback = False
            publish.publish(self.root, self.root, self.identity, config, "Notes")
            self.assertEqual(events[-3:], ["readback", "verified", "finalize"])
            self.assertEqual(
                len(list((self.root / "publication").glob("*/receipt.json"))), 1
            )
            # A tampered retry must stop before any new upload/signature.
            retry_assets = next((self.root / "publication").glob("*/assets"))
            (
                retry_assets / build.binary_name(self.identity["version"], "amd64")
            ).write_text("substitution")
            runner.reset_mock()
            with self.assertRaisesRegex(core.ReleaseError, "changed prepared assets"):
                publish.publish(self.root, self.root, self.identity, config, "Notes")
            runner.run.assert_not_called()


if __name__ == "__main__":
    unittest.main()
