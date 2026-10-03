import hashlib
import importlib.util
import os
import subprocess
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    "accessibility_runtime",
    Path(__file__).resolve().parents[1] / "run-accessibility-tests.py",
)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class ApplicationBindingTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.binary = self.root / "candidate"
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

    def test_selected_candidate_binds_real_cli_and_running_server_without_cargo(self):
        with (
            patch.dict(os.environ, {"PLAMENU_BINARY": str(self.binary)}),
            patch.object(MODULE, "run") as build,
        ):
            application = MODULE.prepare_application()
            build.assert_not_called()
        config = self.root / "fixture.toml"
        cli = subprocess.check_output(
            application.command(config, "role", "list"), text=True
        )
        self.assertEqual(cli.strip(), str(self.binary))
        process = subprocess.Popen(application.command(config, "serve"))
        try:
            deadline = time.monotonic() + 5
            while Path(f"/proc/{process.pid}/exe").resolve() != self.binary:
                self.assertLess(time.monotonic(), deadline)
                time.sleep(0.005)
            identity = application.running_identity(process)
            self.assertEqual(identity["pid"], process.pid)
            self.assertEqual(
                identity["sha256"], hashlib.sha256(self.binary.read_bytes()).hexdigest()
            )
            self.assertEqual(identity["executable"], str(self.binary))
        finally:
            process.terminate()
            process.wait(timeout=5)

    def test_drift_is_refused_before_another_fixture_command(self):
        application = MODULE.ApplicationBinary(self.binary)
        replacement = self.root / "replacement"
        replacement.write_text("#!/bin/sh\nexit 0\n")
        replacement.chmod(0o700)
        replacement.replace(self.binary)
        with self.assertRaisesRegex(RuntimeError, "changed"):
            application.command(self.root / "fixture.toml", "serve")

    def test_invalid_explicit_candidate_never_builds_or_falls_back(self):
        for value in ["relative-candidate", str(self.root / "missing")]:
            with (
                self.subTest(value=value),
                patch.dict(os.environ, {"PLAMENU_BINARY": value}),
                patch.object(MODULE, "run") as build,
                self.assertRaises((ValueError, RuntimeError)),
            ):
                MODULE.prepare_application()
            build.assert_not_called()

    def test_default_builds_once_then_binds_the_built_bytes(self):
        default = self.root / "target/debug/plamenu"
        default.parent.mkdir(parents=True)
        default.write_bytes(self.binary.read_bytes())
        default.chmod(0o700)
        with (
            patch.dict(os.environ, {"PLAMENU_BINARY": ""}),
            patch.object(MODULE, "ROOT", self.root),
            patch.object(MODULE, "run") as build,
        ):
            application = MODULE.prepare_application()
            self.assertEqual(application.path, default)
            application.command(self.root / "config", "account", "add", "fixture")
            application.command(self.root / "config", "serve")
        build.assert_called_once_with("cargo", "build", "--locked", "-q")


class HostBrowserPreflightTests(unittest.TestCase):
    def test_missing_sink_is_actionable_before_fixture_creation(self):
        with (
            patch.object(MODULE.sys, "platform", "linux"),
            patch.object(
                MODULE.shutil, "which", return_value="/usr/bin/gst-inspect-1.0"
            ),
            patch.object(
                MODULE.subprocess,
                "run",
                return_value=subprocess.CompletedProcess([], 1),
            ),
            self.assertRaisesRegex(RuntimeError, "default pinned browser image"),
        ):
            MODULE.local_browser_preflight()

    def test_missing_inspector_is_actionable(self):
        with (
            patch.object(MODULE.sys, "platform", "linux"),
            patch.object(MODULE.shutil, "which", return_value=None),
            self.assertRaisesRegex(RuntimeError, "test dependencies"),
        ):
            MODULE.local_browser_preflight()

    def test_available_sink_passes(self):
        with (
            patch.object(MODULE.sys, "platform", "linux"),
            patch.object(
                MODULE.shutil, "which", return_value="/usr/bin/gst-inspect-1.0"
            ),
            patch.object(
                MODULE.subprocess,
                "run",
                return_value=subprocess.CompletedProcess([], 0),
            ),
        ):
            MODULE.local_browser_preflight()

    def test_other_platform_does_not_require_linux_plugins(self):
        with (
            patch.object(MODULE.sys, "platform", "darwin"),
            patch.object(MODULE.shutil, "which") as lookup,
        ):
            MODULE.local_browser_preflight()
            lookup.assert_not_called()


if __name__ == "__main__":
    unittest.main()
