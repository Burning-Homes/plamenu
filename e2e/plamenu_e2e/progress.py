"""Flushed execution events, independent of test selection and release policy."""

import datetime
import hashlib
import json
import os
import subprocess
import sys
import time
import uuid
from pathlib import Path

import pytest


def pytest_addoption(parser):
    parser.addoption(
        "--progress-events",
        help="write fresh, flushed E2E execution events as JSON Lines",
    )


def pytest_configure(config):
    destination = config.getoption("progress_events")
    if destination:
        config.pluginmanager.register(ExecutionProgress(destination), "e2e-progress")


def source_identity():
    root = Path(__file__).resolve().parents[2]

    def git(*args):
        try:
            return subprocess.check_output(
                ["git", *args],
                cwd=root,
                stderr=subprocess.DEVNULL,
                text=True,
                timeout=10,
            ).strip()
        except (OSError, subprocess.SubprocessError):
            return None

    dirty = git("status", "--porcelain")
    identity = {
        "source_revision": git("rev-parse", "HEAD"),
        "source_dirty": dirty != "" if dirty is not None else None,
        "candidate_sha256": None,
    }
    candidate = os.environ.get("PLAMENU_BINARY")
    if candidate:
        try:
            with Path(candidate).open("rb") as stream:
                identity["candidate_sha256"] = hashlib.file_digest(
                    stream, "sha256"
                ).hexdigest()
        except OSError:
            pass
    return identity


class ExecutionProgress:
    def __init__(self, destination):
        self.destination = Path(destination)
        try:
            self.destination.parent.mkdir(parents=True, exist_ok=True)
            descriptor = os.open(
                self.destination, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600
            )
            self.stream = os.fdopen(descriptor, "w", encoding="utf-8")
        except OSError as error:
            raise pytest.UsageError(
                "Cannot create a fresh E2E progress journal."
            ) from error
        self.run_id = str(uuid.uuid4())
        self.began = time.monotonic()
        self.sequence = 0
        self.total = None
        self.completed = 0
        self.current = None
        self.counts = dict.fromkeys(
            ("passed", "failed", "skipped", "xfail", "xpass"), 0
        )
        self.reports = {}
        self.starts = {}
        self.finished = set()
        self.identity = source_identity()

    def emit(self, event, **detail):
        if self.stream is None:
            return
        self.sequence += 1
        record = {
            "schema_version": 1,
            "run_id": self.run_id,
            "sequence": self.sequence,
            "event": event,
            "utc": datetime.datetime.now(datetime.UTC).isoformat(),
            "elapsed_seconds": time.monotonic() - self.began,
            "current_nodeid": self.current,
            "completed": self.completed,
            "total": self.total,
            "counts": dict(self.counts),
            **detail,
        }
        try:
            self.stream.write(json.dumps(record) + "\n")
            self.stream.flush()
        except OSError:
            # Observing execution must not change assertions or test outcomes.
            # The caller can reject missing/incomplete monitoring evidence.
            sys.stderr.write("E2E progress journal unavailable; execution continues.\n")
            try:
                self.stream.close()
            except OSError:
                pass
            self.stream = None

    def pytest_sessionstart(self, session):
        self.emit("session_start", **self.identity)

    def pytest_collection_finish(self, session):
        self.total = len(session.items)
        self.emit("collection_finish", nodeids=[item.nodeid for item in session.items])

    def pytest_runtest_logstart(self, nodeid, location):
        self.current = nodeid
        self.starts[nodeid] = time.monotonic()
        self.emit("test_start", nodeid=nodeid)

    def pytest_runtest_logreport(self, report):
        phase = {
            "phase": report.when,
            "outcome": report.outcome,
            "xfail": bool(getattr(report, "wasxfail", None)),
            "duration_seconds": report.duration,
        }
        phases = self.reports.setdefault(report.nodeid, [])
        phases.append(phase)
        self.emit("phase_finish", nodeid=report.nodeid, **phase)
        if report.when != "teardown" or report.nodeid in self.finished:
            return
        if any(p["outcome"] == "failed" for p in phases):
            outcome = "failed"
        elif any(p["outcome"] == "skipped" and p["xfail"] for p in phases):
            outcome = "xfail"
        elif any(p["outcome"] == "skipped" for p in phases):
            outcome = "skipped"
        elif any(p["outcome"] == "passed" and p["xfail"] for p in phases):
            outcome = "xpass"
        else:
            outcome = "passed"
        self.finished.add(report.nodeid)
        self.completed += 1
        self.counts[outcome] += 1
        self.current = None
        self.emit(
            "test_finish",
            nodeid=report.nodeid,
            outcome=outcome,
            duration_seconds=sum(p["duration_seconds"] for p in phases),
            wall_seconds=time.monotonic() - self.starts[report.nodeid],
        )

    @pytest.hookimpl(trylast=True)
    def pytest_sessionfinish(self, session, exitstatus):
        self.emit(
            "session_finish",
            exit_status=int(session.exitstatus),
            execution_complete=self.completed == self.total,
        )
        if self.stream is not None:
            self.stream.close()
            self.stream = None

    def pytest_unconfigure(self, config):
        if self.stream is not None:
            self.stream.close()
            self.stream = None
