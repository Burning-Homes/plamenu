import importlib.util
import subprocess
import unittest
from pathlib import Path
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    "accessibility_runtime", Path(__file__).resolve().parents[1] / "run-accessibility-tests.py"
)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class HostBrowserPreflightTests(unittest.TestCase):
    def test_missing_sink_is_actionable_before_fixture_creation(self):
        with patch.object(MODULE.sys, "platform", "linux"), patch.object(
            MODULE.shutil, "which", return_value="/usr/bin/gst-inspect-1.0"
        ), patch.object(MODULE.subprocess, "run", return_value=subprocess.CompletedProcess([], 1)):
            with self.assertRaisesRegex(RuntimeError, "default pinned browser image"):
                MODULE.local_browser_preflight()

    def test_missing_inspector_is_actionable(self):
        with patch.object(MODULE.sys, "platform", "linux"), patch.object(
            MODULE.shutil, "which", return_value=None
        ):
            with self.assertRaisesRegex(RuntimeError, "test dependencies"):
                MODULE.local_browser_preflight()

    def test_available_sink_passes(self):
        with patch.object(MODULE.sys, "platform", "linux"), patch.object(
            MODULE.shutil, "which", return_value="/usr/bin/gst-inspect-1.0"
        ), patch.object(MODULE.subprocess, "run", return_value=subprocess.CompletedProcess([], 0)):
            MODULE.local_browser_preflight()

    def test_other_platform_does_not_require_linux_plugins(self):
        with patch.object(MODULE.sys, "platform", "darwin"), patch.object(MODULE.shutil, "which") as lookup:
            MODULE.local_browser_preflight()
            lookup.assert_not_called()


if __name__ == "__main__":
    unittest.main()
