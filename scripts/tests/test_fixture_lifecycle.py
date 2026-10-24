"""Failure paths use real child processes, HTTP control and OS locks."""

import ast
import json
import os
import socket
import subprocess
import sys
import tempfile
import types
import unittest
import urllib.error
import urllib.request
from pathlib import Path
from unittest.mock import Mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "e2e"))
sys.path.insert(0, str(ROOT / "scripts"))
from browser_fixture import BrowserFixture, finalize_report, validate_lifecycle
from plamenu_e2e.lifecycle import Lease, stop_process


class LifecycleTests(unittest.TestCase):
    def test_lease_is_exclusive_across_processes_and_released(self):
        resource = f"test:{os.getpid()}"
        script = f"from plamenu_e2e.lifecycle import Lease; Lease({resource!r}).close()"
        with Lease(resource):
            result = subprocess.run(
                [sys.executable, "-c", script],
                env=os.environ | {"PYTHONPATH": str(ROOT / "e2e")},
                capture_output=True,
                check=False,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(b"already owned", result.stderr)
        with Lease(resource):
            pass

    def test_descendant_is_stopped_even_when_parent_has_already_exited(self):
        with tempfile.TemporaryDirectory() as directory:
            marker = Path(directory) / "port"
            child = subprocess.Popen(
                [
                    sys.executable,
                    "-c",
                    (
                        "import subprocess,sys; subprocess.Popen([sys.executable,'-c',"
                        f"\"import socket,time,pathlib; s=socket.socket(); s.bind(('127.0.0.1',0)); s.listen(); pathlib.Path({str(marker)!r}).write_text(str(s.getsockname()[1])); time.sleep(60)\"])"
                    ),
                ],
                start_new_session=True,
            )
            try:
                child.wait(timeout=5)
                import time

                deadline = time.monotonic() + 5
                while not marker.exists():
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(0.01)
                port = int(marker.read_text())
                stop_process(child)
                deadline = time.monotonic() + 5
                while True:
                    with socket.socket() as probe:
                        if probe.connect_ex(("127.0.0.1", port)) != 0:
                            break
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(0.01)
            finally:
                stop_process(child)

    def test_browser_repeated_attempts_reset_state_and_enforce_owner(self):
        with tempfile.TemporaryDirectory() as directory:
            state = {"writes": [], "running": False}

            def reset():
                state.update(writes=[], running=True)

            def stop():
                state["running"] = False

            owner = BrowserFixture(
                "127.0.0.1",
                reset=reset,
                stop=stop,
                report=Path(directory) / "events.json",
            )

            def post(path, case):
                request = urllib.request.Request(
                    owner.url + path,
                    data=json.dumps({"case": case}).encode(),
                    method="POST",
                )
                with urllib.request.urlopen(request, timeout=5) as response:
                    self.assertEqual(response.status, 204)

            def rejected(path, case):
                with self.assertRaises(urllib.error.HTTPError) as error:
                    post(path, case)
                error.exception.close()

            try:
                rejected("/wrong", "first")
                for case in ("first", "retry", "next"):
                    post("/begin", case)
                    self.assertEqual(state["writes"], [])
                    state["writes"].append("password changed; notification left behind")
                    rejected("/begin", "intruder")
                    rejected("/end", "intruder")
                    self.assertTrue(state["running"])
                    post("/end", case)
                    self.assertFalse(state["running"])
            finally:
                owner.close()
            self.assertEqual(len(owner.events), 6)
            self.assertFalse(owner.thread.is_alive())

    def test_failed_setup_is_poisoned_and_cannot_be_promoted(self):
        with tempfile.TemporaryDirectory() as directory:
            stop = Mock()
            owner = BrowserFixture(
                "127.0.0.1",
                reset=Mock(side_effect=RuntimeError("start failed")),
                stop=stop,
                report=Path(directory) / "events.json",
            )
            with self.assertRaisesRegex(RuntimeError, "start failed"):
                owner.transition("begin", "first")
            with self.assertRaisesRegex(RuntimeError, "fresh run"):
                owner.transition("begin", "retry")
            with self.assertRaisesRegex(RuntimeError, "successful teardown"):
                owner.close()
            stop.assert_called_once()
            self.assertFalse(owner.events[0]["passed"])

    def test_passing_tests_remain_unqualified_until_cleanup_and_subset_stays_partial(
        self,
    ):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "report.json"
            for complete in (True, False):
                path.write_text(
                    json.dumps(
                        {
                            "fixture_cleanup_pending": True,
                            "fixture_tests_pass": complete,
                            "full_pass": False,
                            "selected": ["case"],
                            "tests": [
                                {"id": "case", "attempts": [{"status": "passed"}]}
                            ],
                            "status": "pending-cleanup" if complete else "incomplete",
                        }
                    )
                )
                self.assertFalse(json.loads(path.read_text())["full_pass"])
                events = [
                    {"case": "case:0:nonce", "action": action, "passed": True}
                    for action in ("begin", "end")
                ]
                finalize_report(path, events, 0)
                record = json.loads(path.read_text())
                self.assertEqual(record["full_pass"], complete)
                self.assertTrue(record["fixture_cleanup_complete"])

    def test_lifecycle_evidence_rejects_missing_duplicate_and_wrong_owner(self):
        record = {
            "selected": ["case"],
            "tests": [{"id": "case", "attempts": [{"status": "passed"}]}],
        }
        valid = [
            {"case": "case:0:nonce", "action": action, "passed": True}
            for action in ("begin", "end")
        ]
        for invalid in (
            [],
            valid[:1],
            valid * 2,
            [valid[0], {**valid[1], "case": "other:0:nonce"}],
            [valid[0], {**valid[1], "passed": False}],
            list(reversed(valid)),
        ):
            with self.subTest(events=invalid), self.assertRaises(ValueError):
                validate_lifecycle(record | {"fixture_lifecycle": invalid})
        validate_lifecycle(record | {"fixture_lifecycle": valid})
        cold = [{**valid[0], "cold_rendition": True}, valid[1]]
        with self.assertRaisesRegex(ValueError, "encoder barrier"):
            validate_lifecycle(record | {"fixture_lifecycle": cold})
        cold[1] = {**valid[1], "resource": {"cold_encoder": {"state": "complete", "held_frames": 88200}}}
        validate_lifecycle(record | {"fixture_lifecycle": cold})

    def test_settings_restore_is_registered_before_setup_can_fail(self):
        tree = ast.parse((ROOT / "e2e/conftest.py").read_text())
        function = next(
            n
            for n in tree.body
            if isinstance(n, ast.FunctionDef) and n.name == "_suite_instance_settings"
        )
        function.decorator_list = []
        clock = Mock()
        clock.sleep.side_effect = [KeyboardInterrupt(), None]
        namespace = {"time": clock}
        exec(  # noqa: S102 -- execute only this repository fixture to inject interrupted setup
            compile(ast.Module(body=[function], type_ignores=[]), "fixture", "exec"),
            namespace,
        )
        previous = (True, False, False, False, False)
        db = Mock()
        db.conn.execute.return_value.fetchone.return_value = previous
        finalizers = []
        request = types.SimpleNamespace(addfinalizer=finalizers.append)
        with self.assertRaises(KeyboardInterrupt):
            namespace["_suite_instance_settings"](db, None, request)
        self.assertEqual(len(finalizers), 1)
        finalizers[0]()
        self.assertEqual(db.conn.execute.call_args.args[1], previous)
