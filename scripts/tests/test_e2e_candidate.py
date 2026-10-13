"""Candidate selection and real /proc identity checks without live services."""

import ast
import hashlib
import importlib.util
import shutil
import subprocess
import sys
import tempfile
import time
import types
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "e2e"))
from plamenu_e2e import candidate, config


class CandidateTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.binary = self.root / "portable-candidate"
        self.binary.write_text("#!/bin/sh\nexit 0\n")
        self.binary.chmod(0o700)

    def test_prebuilt_candidate_never_invokes_cargo(self):
        with (
            patch.object(config, "PLAMENU_BINARY", str(self.binary)),
            patch.object(candidate.subprocess, "run") as run,
        ):
            self.assertEqual(candidate.prepare_binary(), self.binary)
            run.assert_not_called()

    def test_missing_prebuilt_does_not_fall_back_or_build(self):
        with (
            patch.object(config, "PLAMENU_BINARY", str(self.root / "absent")),
            patch.object(candidate.subprocess, "run") as run,
        ):
            with self.assertRaises(RuntimeError):
                candidate.prepare_binary()
            run.assert_not_called()

    def test_relative_candidate_is_rejected_before_build(self):
        with (
            patch.object(config, "PLAMENU_BINARY", "target/plamenu"),
            patch.object(candidate.subprocess, "run") as run,
        ):
            with self.assertRaises(ValueError):
                candidate.prepare_binary()
            run.assert_not_called()

    def test_default_development_build_is_retained(self):
        default = self.root / "target/debug/plamenu"
        default.parent.mkdir(parents=True)
        default.write_bytes(self.binary.read_bytes())
        default.chmod(0o700)
        with (
            patch.object(config, "PLAMENU_BINARY", None),
            patch.object(config, "PLAMENU_DIR", self.root),
            patch.object(candidate.subprocess, "run") as run,
        ):
            self.assertEqual(candidate.prepare_binary(), default)
            run.assert_called_once_with(
                ["cargo", "build", "-q"], cwd=self.root, check=True, timeout=1800
            )

    def test_crash_guard_requires_explicit_candidate(self):
        with (
            patch.object(config, "PLAMENU_BINARY", None),
            self.assertRaises(AssertionError),
        ):
            candidate.exact_local_server(-1)

    def test_running_inode_freeze_binds_instance_restart_and_cli(self):
        # Import the real process owner without optional DB/HTTP dependencies;
        # this test launches native processes, but never touches a database.
        stubs = {
            name: types.ModuleType(name)
            for name in (
                "psycopg",
                "requests",
                "plamenu_e2e.plamenu",
                "plamenu_e2e.api",
                "plamenu_e2e.db",
            )
        }
        stubs["psycopg"].sql = MagicMock()
        stubs["psycopg"].connect = MagicMock()
        stubs["requests"].get = lambda *a, **kw: types.SimpleNamespace(ok=True)
        stubs["requests"].RequestException = RuntimeError
        stubs["plamenu_e2e.api"].Api = object
        stubs["plamenu_e2e.db"].Db = object
        spec = importlib.util.spec_from_file_location(
            "plamenu_e2e._process_binding_test",
            ROOT / "e2e/plamenu_e2e/ephemeral.py",
        )
        ephemeral = importlib.util.module_from_spec(spec)
        with patch.dict(sys.modules, stubs):
            spec.loader.exec_module(ephemeral)

        # Execute the existing drill's freeze function itself. Its enclosing
        # module needs the entire developer fleet; the function needs only
        # these real filesystem operations and pytest's setattr contract.
        path = ROOT / "e2e/tests/test_video_offload.py"
        freeze = next(
            node
            for node in ast.parse(path.read_text()).body
            if isinstance(node, ast.FunctionDef) and node.name == "_freeze"
        )
        namespace = {
            "shutil": shutil,
            "ephemeral": ephemeral,
            "_digest": lambda p: hashlib.sha256(p.read_bytes()).hexdigest(),
        }
        exec(  # noqa: S102 — execute the checked-in drill, not a duplicate of it
            compile(ast.Module(body=[freeze], type_ignores=[]), str(path), "exec"),
            namespace,
        )
        patches = []

        def setattr_for_drill(module, name, value):
            binding = patch.object(module, name, value)  # requires an existing name
            binding.start()
            patches.append(binding)

        subprocess.run(
            ["cc", "-x", "c", "-", "-o", str(self.binary)],
            input=(
                "#include <unistd.h>\n#include <stdio.h>\n#include <string.h>\n"
                'int main(int argc, char **argv) { if (argc == 4 && !strcmp(argv[3], "serve"))'
                " { for (;;) pause(); } puts(argv[0]); return 0; }\n"
            ),
            text=True,
            check=True,
            capture_output=True,
        )
        instance = ephemeral.Instance()
        try:
            with (
                patch.object(config, "PLAMENU_BINARY", str(self.binary)),
                patch.object(config, "PLAMENU_DIR", self.root),
                # This test exercises executable binding, not HTTP/DB readiness.
                patch.object(ephemeral.Instance, "_ready", return_value=True),
            ):
                instance.start(timeout=5)
                deadline = time.monotonic() + 5
                while Path(f"/proc/{instance._proc.pid}/exe").resolve() != self.binary:
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(0.005)
                checksum = namespace["_freeze"](
                    instance,
                    self.root,
                    types.SimpleNamespace(setattr=setattr_for_drill),
                )
                frozen = self.root / "candidate-plamenu"
                self.assertEqual(
                    checksum, hashlib.sha256(self.binary.read_bytes()).hexdigest()
                )
                # Replace the original pathname. Recovery must still use the
                # running inode's saved copy, including CLI subprocesses.
                replacement = self.root / "replacement"
                replacement.write_text("#!/bin/sh\necho wrong-candidate\n")
                replacement.chmod(0o700)
                replacement.replace(self.binary)
                instance.restart_process_after_crash(timeout=5)
                self.assertEqual(instance.cli("role", "list").strip(), str(frozen))
                deadline = time.monotonic() + 5
                while Path(f"/proc/{instance._proc.pid}/exe").resolve() != frozen:
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(0.005)
        finally:
            instance.stop()
            for binding in reversed(patches):
                binding.stop()

    def test_real_process_requires_matching_candidate_config_cwd_and_argv(self):
        subprocess.run(
            ["cc", "-x", "c", "-", "-o", str(self.binary)],
            input="#include <unistd.h>\nint main(void) { for (;;) pause(); }\n",
            text=True,
            check=True,
            capture_output=True,
        )
        fixture = str(self.root / "fixture.toml")
        process = subprocess.Popen(
            [str(self.binary), "--config", fixture, "serve"], cwd=self.root
        )
        try:
            with (
                patch.object(config, "PLAMENU_BINARY", str(self.binary)),
                patch.object(config, "PLAMENU_CONFIG", fixture),
                patch.object(config, "PLAMENU_DIR", self.root),
            ):
                self.assertEqual(candidate.exact_local_server(process.pid), self.binary)
                with (
                    patch.object(config, "PLAMENU_BINARY", sys.executable),
                    self.assertRaises(AssertionError),
                ):
                    candidate.exact_local_server(process.pid)
                with (
                    patch.object(config, "PLAMENU_CONFIG", fixture + ".other"),
                    self.assertRaises(AssertionError),
                ):
                    candidate.exact_local_server(process.pid)
                with (
                    patch.object(config, "PLAMENU_DIR", self.root.parent),
                    self.assertRaises(AssertionError),
                ):
                    candidate.exact_local_server(process.pid)
                other = self.root / "different-executable"
                other.write_bytes(self.binary.read_bytes())
                other.chmod(0o700)
                spoofed = subprocess.Popen(
                    [str(self.binary), "--config", fixture, "serve"],
                    executable=str(other),
                    cwd=self.root,
                )
                try:
                    with self.assertRaises(AssertionError):
                        candidate.exact_local_server(spoofed.pid)
                finally:
                    spoofed.terminate()
                    spoofed.wait(timeout=10)
        finally:
            process.terminate()
            process.wait(timeout=10)
