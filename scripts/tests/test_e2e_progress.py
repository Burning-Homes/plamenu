"""Observe actual pytest phases, including results unavailable until teardown."""

import hashlib
import json
import os
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


class ExecutionProgressTests(unittest.TestCase):
    def command(self, root):
        return [
            sys.executable,
            "-m",
            "pytest",
            "-p",
            "plamenu_e2e.progress",
            "--progress-events",
            str(root / "events.jsonl"),
            "--maxfail=0",
            "-q",
        ]

    def environment(self):
        return {
            **os.environ,
            "PYTHONPATH": str(ROOT / "e2e"),
            "PYTEST_DISABLE_PLUGIN_AUTOLOAD": "1",
        }

    def events(self, root):
        return [
            json.loads(line)
            for line in (root / "events.jsonl").read_text().splitlines()
        ]

    def run_pytest(self, root, *, options=(), environment=None):
        return subprocess.run(
            [*self.command(root), *options],
            cwd=root,
            env={**self.environment(), **(environment or {})},
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )

    def test_actual_case_results_count_once_after_teardown(self):
        source = """import pytest
@pytest.fixture
def broken_setup(): pytest.fail("synthetic setup failure")
@pytest.fixture
def broken_teardown():
    yield
    pytest.fail("synthetic teardown failure")
@pytest.fixture
def skipped_setup(): pytest.skip("synthetic setup skip")
def test_pass(): pass
def test_fail(): assert False
def test_setup_error(broken_setup): pass
def test_teardown_error(broken_teardown): pass
def test_setup_skip(skipped_setup): pass
def test_call_skip(): pytest.skip("synthetic call skip")
@pytest.mark.xfail(reason="synthetic expected failure", strict=True)
def test_expected_failure(): assert False
@pytest.mark.xfail(reason="synthetic unexpected pass", strict=False)
def test_unexpected_pass(): pass
"""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "test_results.py").write_text(source)
            result = self.run_pytest(root)
            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
            events = self.events(root)
            collected = next(e for e in events if e["event"] == "collection_finish")
            finished = [e for e in events if e["event"] == "test_finish"]
            self.assertEqual(collected["total"], 8)
            self.assertEqual({e["nodeid"] for e in finished}, set(collected["nodeids"]))
            self.assertEqual(len(finished), 8)
            self.assertEqual([e["completed"] for e in finished], list(range(1, 9)))
            self.assertTrue(all(e["current_nodeid"] is None for e in finished))
            self.assertTrue(all(e["duration_seconds"] >= 0 for e in finished))
            self.assertTrue(all(e["wall_seconds"] >= 0 for e in finished))
            self.assertEqual(
                [e["sequence"] for e in events], list(range(1, len(events) + 1))
            )
            for event in finished:
                previous = events[event["sequence"] - 2]
                self.assertEqual(previous["event"], "phase_finish")
                self.assertEqual(previous["phase"], "teardown")
                self.assertEqual(previous["nodeid"], event["nodeid"])
            terminal = events[-1]
            self.assertEqual(terminal["event"], "session_finish")
            self.assertTrue(terminal["execution_complete"])
            self.assertEqual(terminal["exit_status"], 1)
            self.assertEqual(
                terminal["counts"],
                {"passed": 1, "failed": 3, "skipped": 2, "xfail": 1, "xpass": 1},
            )
            self.assertEqual((root / "events.jsonl").stat().st_mode & 0o777, 0o600)

    def test_current_nodeid_is_flushed_before_the_test_finishes(self):
        source = """from pathlib import Path
import time
def test_blocked():
    Path("entered").touch()
    deadline = time.monotonic() + 15
    while not Path("release").exists():
        assert time.monotonic() < deadline
        time.sleep(.02)
def test_after(): pass
"""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "test_live.py").write_text(source)
            process = subprocess.Popen(
                self.command(root),
                cwd=root,
                env=self.environment(),
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            try:
                deadline = time.monotonic() + 10
                while not (root / "entered").exists():
                    self.assertIsNone(process.poll())
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(0.02)
                live = self.events(root)[-1]
                self.assertEqual(live["current_nodeid"], "test_live.py::test_blocked")
                self.assertEqual(live["completed"], 0)
                self.assertEqual(live["total"], 2)
                self.assertFalse(any(live["counts"].values()))
                self.assertIsNone(process.poll())
                (root / "release").touch()
                stdout, stderr = process.communicate(timeout=20)
                self.assertEqual(process.returncode, 0, stdout + stderr)
                terminal = self.events(root)[-1]
                self.assertEqual(terminal["completed"], 2)
                self.assertEqual(terminal["counts"]["passed"], 2)
                self.assertTrue(terminal["execution_complete"])
            finally:
                if process.poll() is None:
                    process.kill()
                process.communicate()

    def test_existing_journal_is_preserved_without_running_tests(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "test_pass.py").write_text("def test_pass(): pass\n")
            (root / "events.jsonl").write_text("original receipt\n")
            result = self.run_pytest(root)
            self.assertEqual(result.returncode, 4, result.stdout + result.stderr)
            self.assertEqual((root / "events.jsonl").read_text(), "original receipt\n")

    def test_failed_journal_does_not_change_a_real_test_outcome(self):
        source = """import os
def test_logger_failure(request):
    observer = request.config.pluginmanager.get_plugin("e2e-progress")
    os.close(observer.stream.fileno())
"""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "test_logging_failure.py").write_text(source)
            result = self.run_pytest(root)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("E2E progress journal unavailable", result.stderr)
            self.assertNotEqual(self.events(root)[-1]["event"], "session_finish")

    def test_monitoring_preserves_incomplete_matrix_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "test_selection.py").write_text(
                "def test_one(): pass\ndef test_two(): pass\n"
            )
            result = self.run_pytest(
                root,
                options=(
                    "-p",
                    "plamenu_e2e.release_policy",
                    "--release-matrix",
                    "--matrix-report",
                    str(root / "matrix.json"),
                    "-k",
                    "one",
                ),
            )
            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
            matrix = json.loads((root / "matrix.json").read_text())
            self.assertTrue(matrix["incomplete_selection"])
            self.assertEqual(matrix["deselected"], ["test_selection.py::test_two"])
            terminal = self.events(root)[-1]
            self.assertEqual(terminal["completed"], 1)
            self.assertEqual(terminal["total"], 1)
            self.assertEqual(terminal["exit_status"], 1)
            self.assertTrue(terminal["execution_complete"])

    def test_session_identity_hashes_the_selected_artifact(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            candidate = root / "candidate"
            candidate.write_bytes(b"synthetic identity fixture, never executed")
            (root / "test_identity.py").write_text("def test_identity(): pass\n")
            result = self.run_pytest(
                root, environment={"PLAMENU_BINARY": str(candidate)}
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            start = self.events(root)[0]
            self.assertEqual(start["event"], "session_start")
            self.assertEqual(
                start["candidate_sha256"],
                hashlib.sha256(candidate.read_bytes()).hexdigest(),
            )
            self.assertRegex(start["source_revision"], r"^[0-9a-f]{40}$")
            self.assertIsInstance(start["source_dirty"], bool)


if __name__ == "__main__":
    unittest.main()
