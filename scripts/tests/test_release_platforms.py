"""Platform identity, complete image indexes, and usable release downloads."""

import io
import json
import sys
import tarfile
import tempfile
import unittest
from contextlib import redirect_stderr
from pathlib import Path
from unittest.mock import Mock, patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
from plamenu_release import build, core, publish, worker
from plamenu_release.forge import Forge


class PlatformTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def test_package_link_enables_the_unit_first_and_is_safe_to_retry(self):
        forge = Forge("https://forge.invalid/team/repo", "fixture")
        repo = {"id": 7, "has_packages": False}
        package = {"repository": None}
        writes = []

        def api(path, method, body):
            self.assertEqual(
                (path, method, body), ("", "PATCH", {"has_packages": True})
            )
            repo.update(body)
            writes.append("enable")

        def request(url, method="GET"):
            if method == "POST":
                self.assertTrue(repo["has_packages"])
                self.assertIsNone(package["repository"])
                self.assertTrue(url.endswith("/-/link/repo"))
                package["repository"] = {"id": repo["id"]}
                writes.append("link")
                return io.BytesIO()
            self.assertTrue(url.endswith("/container/image/v1.0.0"))
            return io.BytesIO(json.dumps(package).encode())

        with (
            patch.object(forge, "repository_check", return_value=repo),
            patch.object(forge, "api", side_effect=api),
            patch.object(forge, "request", side_effect=request),
        ):
            forge.link_package("forge.invalid/team/image", "v1.0.0")
            forge.link_package("forge.invalid/team/image", "v1.0.0")
            self.assertEqual(writes, ["enable", "link"])
            package["repository"] = {"id": 99}
            with self.assertRaisesRegex(core.ReleaseError, "different repository"):
                forge.link_package("forge.invalid/team/image", "v1.0.0")
            self.assertEqual(writes, ["enable", "link"])

    def test_wrong_architecture_and_non_elf_executables_are_rejected(self):
        binary = self.root / "plamenu"
        for arch, machine in (("amd64", 62), ("arm64", 183)):
            binary.write_bytes(
                b"\x7fELF\x02\x01" + b"\0" * 12 + machine.to_bytes(2, "little")
            )
            build.verify_elf(binary, arch)
            with self.assertRaises(core.ReleaseError):
                build.verify_elf(binary, "arm64" if arch == "amd64" else "amd64")
        binary.write_bytes(b"#!/bin/sh\necho not a release binary\n")
        with self.assertRaises(core.ReleaseError):
            build.verify_elf(binary, "amd64")

    def test_source_comparison_ignores_encoding_but_catches_changed_files(self):
        def archive(name, mode, data, permissions=0o644):
            path = self.root / name
            with tarfile.open(path, mode) as output:
                entry = tarfile.TarInfo("Cargo.toml")
                entry.size, entry.mode = len(data), permissions
                output.addfile(entry, io.BytesIO(data))
            return path

        plain = archive("source.tar", "w", b"source")
        compressed = archive("source.tar.gz", "w:gz", b"source")
        self.assertEqual(build.source_tree(plain), build.source_tree(compressed))
        for content, mode in ((b"different", 0o644), (b"source", 0o755)):
            changed = archive("changed.tar", "w", content, mode)
            self.assertNotEqual(build.source_tree(plain), build.source_tree(changed))

    def test_worker_refuses_emulated_or_wrong_architecture_builds(self):
        identity = {"source": "a" * 40, "arch": "arm64"}
        with (
            patch.object(worker, "git", return_value=identity["source"]),
            patch.object(worker.platform, "machine", return_value="x86_64"),
            patch.object(build, "checked_build") as compile,
            self.assertRaisesRegex(core.ReleaseError, "natively"),
        ):
            worker.execute_worker(self.root, self.root / "out", identity, "build")
        compile.assert_not_called()

    def test_failed_worker_keeps_output_and_exposes_the_failure_in_job_logs(self):
        identity = {"source": "a" * 40, "arch": "arm64", "jobs": 2}
        output = self.root / "worker"

        def fail(runner, folder, inputs):
            runner.log.write_text("compiler error: out of memory\n")
            raise core.ReleaseError("compilation failed")

        log = io.StringIO()
        with (
            patch.object(worker, "git", return_value=identity["source"]),
            patch.object(worker.platform, "machine", return_value="aarch64"),
            patch.object(build, "checked_build", side_effect=fail),
            redirect_stderr(log),
            self.assertRaisesRegex(core.ReleaseError, "compilation failed"),
        ):
            worker.execute_worker(self.root, output, identity, "build")
        self.assertIn("compiler error: out of memory", log.getvalue())
        with tarfile.open(output / "bundle.tar.gz") as archive:
            self.assertEqual(
                json.load(archive.extractfile("stage.json"))["status"], "failed"
            )
            self.assertIn(b"out of memory", archive.extractfile("run.log").read())

    def test_failed_worker_diagnostics_are_downloaded_without_manual_inputs(self):
        identity = {"source": "a" * 40, "arch": "arm64", "jobs": 2}
        output = self.root / "output"
        output.mkdir()
        token = self.root / "token"
        token.write_text("fixture")
        repository = "https://forge.invalid/team/repo"
        state = self.root / "worker-build-arm64.json"
        core.write_json(
            state,
            {
                "key": core.fingerprint({"identity": identity, "stage": "build"}),
                "request": "request",
                "repository": repository,
                "run_id": 9,
            },
        )
        forge = Mock(
            repository=repository, base="https://forge.invalid/api/v1/repos/team/repo"
        )
        forge.api.side_effect = [
            {
                "id": 9,
                "status": "failure",
                "commit_sha": identity["source"],
                "workflow_id": "release-worker.yml",
                "html_url": repository + "/actions/runs/1",
            },
            [{"id": 10, "status": "failure"}],
        ]
        forge.request.return_value = io.BytesIO(b"compiler error: out of memory\n")
        with (
            patch.object(worker, "Forge", return_value=forge),
            self.assertRaisesRegex(core.ReleaseError, "diagnostics:"),
        ):
            worker.remote(
                self.root,
                self.root,
                output,
                identity,
                "build",
                {"repository": repository, "token_file": str(token)},
                "main",
            )
        self.assertIn("out of memory", (output / "worker-failure.log").read_text())
        self.assertTrue(json.loads(state.read_text())["failed"])

    def test_index_requires_exactly_both_linux_architectures(self):
        descriptors = [
            {
                "platform": {"os": "linux", "architecture": arch},
                "digest": "sha256:" + value * 64,
            }
            for arch, value in (("amd64", "a"), ("arm64", "b"))
        ]
        runner = Mock()

        def inspect(entries):
            runner.run.return_value = json.dumps(
                {
                    "mediaType": "application/vnd.oci.image.index.v1+json",
                    "manifests": entries,
                }
            )
            return publish.registry_platforms(runner, "registry.invalid/image:v1")

        self.assertEqual(set(inspect(descriptors)), {"amd64", "arm64"})
        for entries in (
            descriptors[:1],
            descriptors + descriptors[:1],
            [
                descriptors[0],
                descriptors[1] | {"platform": {"os": "linux", "architecture": "arm"}},
            ],
            [descriptors[0], descriptors[1] | {"digest": "not-a-digest"}],
            [
                descriptors[0],
                descriptors[1]
                | {"platform": {"os": "windows", "architecture": "arm64"}},
            ],
        ):
            with self.assertRaises(core.ReleaseError):
                inspect(entries)

    def test_image_retry_validates_both_platforms_without_pushing(self):
        for arch in build.TARGETS:
            folder = self.root / ("build-" + arch)
            folder.mkdir()
            (folder / "image.json").write_text(json.dumps({"Architecture": arch}))
        runner = Mock()
        platforms = {"amd64": "sha256:" + "a" * 64, "arm64": "sha256:" + "b" * 64}
        with (
            patch.object(publish, "registry_digest", return_value="sha256:" + "c" * 64),
            patch.object(publish, "registry_platforms", return_value=platforms),
            patch.object(publish, "verify_image") as verify,
        ):
            reference, actual = publish.publish_images(
                runner, self.root, "registry.invalid/image", "v1", Mock()
            )
            self.assertEqual(actual, platforms)
            self.assertTrue(reference.endswith("c" * 64))
            self.assertEqual(
                [call.args[2]["Architecture"] for call in verify.call_args_list],
                ["amd64", "arm64"],
            )
            self.assertEqual(
                [call.args[:2] for call in runner.run.call_args_list],
                [("docker", "pull"), ("docker", "pull")],
            )
            verify.side_effect = core.ReleaseError("wrong ARM64 image")
            with self.assertRaisesRegex(core.ReleaseError, "wrong ARM64"):
                publish.publish_images(
                    runner, self.root, "registry.invalid/image", "v1", Mock()
                )

    def test_failed_platform_prevents_index_publication(self):
        for arch in build.TARGETS:
            folder = self.root / ("build-" + arch)
            folder.mkdir()
            (folder / "image.json").write_text(json.dumps({"Architecture": arch}))
            (folder / "build.json").write_text(json.dumps({"image": "local:" + arch}))
        runner = Mock()
        runner.run.side_effect = lambda *args, **kw: (
            (_ for _ in ()).throw(core.ReleaseError("failed ARM64 upload"))
            if args[:2] == ("docker", "push") and str(args[-1]).endswith("arm64")
            else ""
        )
        with (
            patch.object(
                publish,
                "registry_digest",
                side_effect=[None, None, "sha256:" + "a" * 64, None],
            ),
            patch.object(publish, "verify_image"),
            patch.object(publish.time, "sleep"),
            self.assertRaisesRegex(core.ReleaseError, "failed ARM64"),
        ):
            publish.publish_images(
                runner, self.root, "registry.invalid/image", "v1", Mock()
            )
        self.assertFalse(
            any(
                call.args[:4] == ("docker", "buildx", "imagetools", "create")
                for call in runner.run.call_args_list
            )
        )

    def test_release_description_identifies_downloads_contents_and_container(self):
        text = publish.release_description(
            "Changes.",
            {"version": "1.2.3"},
            "https://forge.invalid/team/repo",
            "forge.invalid/team/image",
            "forge.invalid/team/image@sha256:" + "a" * 64,
        )
        for expected in (
            "plamenu-1.2.3-linux-amd64.tar.gz",
            "plamenu-1.2.3-linux-arm64.tar.gz",
            "plamenu-1.2.3-source.tar.gz",
            "one static `plamenu` executable",
            "docker pull forge.invalid/team/image:v1.2.3",
            "https://forge.invalid/team/-/packages/container/image/v1.2.3",
            "linux/amd64",
            "linux/arm64",
            "SHA256SUMS.bundle",
        ):
            self.assertIn(expected, text)
        self.assertNotIn("unstripped", text)
        self.assertNotIn("binaries.tar.gz", text)


if __name__ == "__main__":
    unittest.main()
