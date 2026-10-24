import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
import gate_inventory

SPEC = importlib.util.spec_from_file_location(
    "standalone_gate", ROOT / "scripts/test-harness.py"
)
HARNESS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(HARNESS)


class InventoryTests(unittest.TestCase):
    def test_static_checks_include_new_files_and_allow_pending_moves(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            old = root / "old.py"
            old.write_text("pass\n")
            subprocess.run(["git", "add", "old.py"], cwd=root, check=True)
            old.unlink()
            new = root / "new.py"
            new.write_text("def broken(\n")
            command = [sys.executable, str(ROOT / "scripts/ci/check-static.py")]
            rejected = subprocess.run(
                command, cwd=root, capture_output=True, text=True, check=False
            )
            self.assertNotEqual(rejected.returncode, 0)
            self.assertIn("new.py", rejected.stderr)
            new.write_text("pass\n")
            accepted = subprocess.run(
                command, cwd=root, capture_output=True, text=True, check=False
            )
            self.assertEqual(accepted.returncode, 0, accepted.stderr)
            self.assertIn("'python': 1", accepted.stdout)

    def test_added_deleted_or_changed_source_invalidates_inventory(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tests = root / "e2e/tests"
            tests.mkdir(parents=True)
            (root / "e2e/conftest.py").write_text("")
            (root / "e2e/pyproject.toml").write_text("")
            case = tests / "test_owned.py"
            case.write_text("def test_one(): pass\n")
            record = {
                "schema": 1,
                "suite": "e2e",
                "files": gate_inventory.source_files(root, "e2e"),
                "tests": [{"id": "tests/test_owned.py::test_one", "capabilities": []}],
            }
            gate_inventory.inventory_path(root, "e2e").write_text(json.dumps(record))
            self.assertEqual(gate_inventory.load_inventory(root, "e2e"), record)
            case.write_text("def test_one(): assert False\n")
            with self.assertRaisesRegex(ValueError, "Stale"):
                gate_inventory.load_inventory(root, "e2e")
            case.write_text("def test_one(): pass\n")
            extra = tests / "test_added.py"
            extra.write_text("def test_new(): pass\n")
            with self.assertRaisesRegex(ValueError, "Stale"):
                gate_inventory.load_inventory(root, "e2e")
            extra.unlink()
            case.unlink()
            with self.assertRaisesRegex(ValueError, "Stale"):
                gate_inventory.load_inventory(root, "e2e")


class StandaloneGateTests(unittest.TestCase):
    def run_gate(self, profile, first_exit=0, skipped=False, missing=False):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)

            def commands(_profile, folder):
                result = []
                for name, code in [("python", first_exit), ("javascript", 0)]:
                    inner = (
                        '<skipped message="fixture unavailable"/>'
                        if skipped and name == "python"
                        else ""
                    )
                    xml = (
                        '<testsuites><testsuite><testcase name="owned">'
                        + inner
                        + "</testcase></testsuite></testsuites>"
                    )
                    script = (
                        "from pathlib import Path\n"
                        f"Path({str(root / (name + '.ran'))!r}).touch()\n"
                        + (
                            ""
                            if missing and name == "python"
                            else f"Path({str(folder / (name + '.xml'))!r}).write_text({xml!r})\n"
                        )
                        + f"raise SystemExit({code})\n"
                    )
                    result.append((name, [sys.executable, "-c", script]))
                return result, ["owned-io"] if profile == "portable" else []

            with (
                patch.object(HARNESS, "ROOT", root),
                patch.object(HARNESS, "commands", commands),
                patch.object(sys, "argv", ["test-harness", "--profile", profile]),
                patch.dict(os.environ, {"PYTEST_ADDOPTS": "-k nonexistent"}),
            ):
                code = HARNESS.main()
            report = json.loads(
                next((root / "target/gates").glob("*/report.json")).read_text()
            )
            self.assertTrue((root / "python.ran").exists())
            self.assertTrue((root / "javascript.ran").exists())
            return code, report

    def test_all_suites_finish_and_failed_suite_cannot_be_hidden(self):
        code, report = self.run_gate("full", first_exit=1)
        self.assertEqual(code, 1)
        self.assertFalse(report["full_pass"])

    def test_missing_evidence_and_unexpected_skips_fail(self):
        for args in ({"missing": True}, {"skipped": True}):
            with self.subTest(args=args):
                code, report = self.run_gate("full", **args)
                self.assertEqual(code, 1)
                self.assertFalse(report["full_pass"])

    def test_portable_success_is_never_full(self):
        code, report = self.run_gate("portable")
        self.assertEqual(code, 0)
        self.assertEqual(report["status"], "passed")
        self.assertFalse(report["full_pass"])
        code, report = self.run_gate("full")
        self.assertEqual(code, 0)
        self.assertTrue(report["full_pass"])
