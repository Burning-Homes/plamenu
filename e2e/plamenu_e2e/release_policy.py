"""Strict full-matrix policy, also loadable without the live-peer fixtures."""

import datetime
import hashlib
import json
import os
import pathlib
import subprocess
import sys
import uuid

import pytest

ROOT = pathlib.Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
from gate_inventory import inventory_digest, load_inventory

from plamenu_e2e.progress import source_identity

INVENTORY_ROOT = ROOT

# These peers are deliberately outside `up all`. Only the explicit standard
# profile excludes them; a full pass requires them even after shell preflight.
OPTIONAL_MODULES = {
    "test_tor_federation.py": "optional Tor peer",
    "test_plup_follow.py": "optional Plup peer",
    "test_plup_federation.py": "optional Plup peer",
    "test_plup_reactions.py": "optional Plup peer",
    "test_mobilizon_federation.py": "optional Mobilizon peer",
}
# Explicit, already documented limitations. Match both the test and its reason:
# a missing required peer must still fail even in one of these modules.
KNOWN_SKIPS = {
    "test_pleroma_federation.py::test_pleroma_block_reaches_plamenu": (
        "Akkoma outgoing_blocks=false",
        "configured peer does not federate blocks",
    ),
    "test_gotosocial_federation.py::test_gts_move_reaches_plamenu": (
        "GtS irreversibly locks a moved account",
        "standing GtS fixture cannot originate a destructive Move",
    ),
    "test_mitra_federation.py::test_report_flag_unsupported": (
        "Mitra 5.7.0 has no reports endpoint",
        "configured Mitra peer does not support reports",
    ),
}


def pytest_addoption(parser):
    parser.addoption(
        "--release-matrix",
        action="store_true",
        help="fail unexpected skips and incomplete selection",
    )
    parser.addoption("--matrix-report", help="write evidence to a NEW JSON path")
    parser.addoption(
        "--matrix-profile",
        choices=("full", "standard"),
        default="full",
        help="standard excludes declared opt-in drills and optional peers; it never qualifies as full",
    )


def pytest_configure(config):
    config.addinivalue_line(
        "markers", "requires_peer(name): live readiness checked during execution"
    )
    config.addinivalue_line(
        "markers", "capability(name): explicit opt-in environment variable"
    )
    config.addinivalue_line(
        "markers",
        "nested_capability(name): explicit nested journey required for full evidence",
    )
    if config.getoption("release_matrix") or config.getoption("matrix_report"):
        config.pluginmanager.register(ReleaseMatrix(config), "release-matrix-policy")


def pytest_runtest_setup(item):
    reporting = item.config.getoption("release_matrix") or item.config.getoption(
        "matrix_report"
    )
    if reporting and item.config.getoption("matrix_profile") == "full":
        for marker in item.iter_markers("nested_capability"):
            if os.environ.get(marker.args[0]) != "1":
                pytest.fail(
                    f"Full E2E requires the nested capability {marker.args[0]}=1",
                    pytrace=False,
                )
    for marker in item.iter_markers("capability"):
        if os.environ.get(marker.args[0]) != "1":
            pytest.skip(f"explicit capability is disabled: {marker.args[0]}")
    health = item.config.__dict__.setdefault("_plamenu_peer_health", {})
    for marker in item.iter_markers("requires_peer"):
        name = marker.args[0]
        if name not in {"peertube", "owncast", "hubzilla", "funkwhale"}:
            pytest.fail(f"Unknown peer requirement: {name}")
        if name not in health:
            import importlib

            health[name] = importlib.import_module(f"plamenu_e2e.{name}").reachable()
        if not health[name]:
            pytest.skip(f"required peer is unavailable: {name}")


def command(cwd, *args):
    try:
        return subprocess.check_output(
            args, cwd=cwd, text=True, stderr=subprocess.DEVNULL, timeout=10
        ).strip()
    except (OSError, subprocess.SubprocessError):
        return None


def source_evidence(root):
    lock = root / "e2e/peers/sources.lock"
    peer_root = pathlib.Path(os.environ.get("PLAMENU_PEER_ROOT", root / "e2e/peers"))
    if not peer_root.is_absolute():
        peer_root = root / peer_root
    peers = []
    if lock.exists():
        for line in lock.read_text().splitlines():
            if not line or line.startswith("#"):
                continue
            destination, repository, revision = line.split("|")
            checkout = peer_root / destination
            peers.append(
                {
                    "source": destination,
                    "repository": repository,
                    "configured_revision": revision,
                    "checkout_revision": command(checkout, "git", "rev-parse", "HEAD")
                    if checkout.is_dir()
                    else None,
                    "checkout_changes": command(
                        checkout, "git", "status", "--porcelain", "--untracked-files=no"
                    )
                    if checkout.is_dir()
                    else None,
                }
            )
    # Record image references/digests from the actual running fleet, without
    # inspecting environment variables (which contain peer credentials).
    containers = command(
        root, "docker", "ps", "--format", "{{.ID}} {{.Names}} {{.Image}}"
    )
    images = []
    for line in (containers or "").splitlines():
        container_id, name, reference = line.split(maxsplit=2)
        digest = command(
            root, "docker", "inspect", "--format", "{{.Image}}", container_id
        )
        images.append(
            {"container": name, "image_reference": reference, "image_id": digest}
        )
    return {
        "git_sha": command(root, "git", "rev-parse", "HEAD"),
        "tracked_changes": command(
            root, "git", "status", "--porcelain", "--untracked-files=no"
        ),
        "configured_sources_sha256": hashlib.sha256(lock.read_bytes()).hexdigest()
        if lock.exists()
        else None,
        "peer_root": str(peer_root),
        "peer_sources": peers,
        "running_images": images,
    }


def validate_full_report(record, inventory):
    """Recompute qualification at consumption; never trust a producer's green flag."""
    expected = {case["id"] for case in inventory["tests"]}
    candidate = record.get("candidate_start")
    if (
        not isinstance(candidate, dict)
        or not candidate.get("source_revision")
        or candidate["source_revision"] != record.get("git_sha")
    ):
        raise ValueError("E2E evidence has no consistent source identity")
    if (
        record.get("schema_version") != 2
        or record.get("profile") != "full"
        or record.get("status") != "passed"
        or record.get("full_pass") is not True
        or record.get("exit_status") != 0
        or record.get("errors") != []
        or record.get("inventory_sha256") != inventory_digest(inventory)
        or record.get("selection_restrictions") != {}
        or record.get("deselected") != []
        or record.get("profile_exclusions") != []
        or record.get("unexpected_skips") != []
        or record.get("candidate_start") != record.get("candidate_end")
    ):
        raise ValueError("E2E evidence is not a complete full pass")
    for field in ("expected", "selected", "completed"):
        ids = record.get(field, [])
        if not expected or len(ids) != len(expected) or set(ids) != expected:
            raise ValueError(f"Incomplete E2E {field} inventory")
    phases = {}
    for row in record.get("reports", []):
        nodeid = row["nodeid"]
        if (
            nodeid not in expected
            or row.get("xfail_reason")
            or row["outcome"] not in {"passed", "skipped"}
        ):
            raise ValueError("Unqualified E2E outcome")
        if row["outcome"] == "skipped":
            known = KNOWN_SKIPS.get(nodeid.split("/")[-1])
            if not known or known[0] not in (row.get("reason") or ""):
                raise ValueError("Unqualified E2E skip")
        phases.setdefault(nodeid, []).append(row)
    for nodeid in expected:
        rows = phases.get(nodeid, [])
        names = [row["phase"] for row in rows]
        setup = next((row for row in rows if row["phase"] == "setup"), None)
        required = {"setup", "teardown"}
        if setup and setup["outcome"] == "passed":
            required.add("call")
        if set(names) != required or len(names) != len(required):
            raise ValueError("Missing or repeated E2E execution phase")


class ReleaseMatrix:
    def __init__(self, config):
        self.config = config
        self.started = datetime.datetime.now(datetime.UTC).isoformat()
        self.root = ROOT
        self.evidence = source_evidence(self.root)
        self.candidate_start = source_identity()
        self.patch_start = command(self.root, "git", "diff", "--binary", "HEAD")
        self.profile = config.getoption("matrix_profile")
        self.strict = config.getoption("release_matrix")
        self.reports = []
        self.deselected = []
        self.unexpected = []
        self.selected = []
        self.excluded = []
        self.errors = []
        self.inventory = None
        self.expected = []
        try:
            self.inventory = load_inventory(INVENTORY_ROOT, "e2e")
            self.expected = [
                case["id"] for case in self.inventory["tests"] if self.required(case)
            ]
        except (OSError, ValueError, KeyError) as error:
            self.errors.append(str(error))
        self.selection_restrictions = {
            name: getattr(config.option, name, None)
            for name in (
                "file_or_dir",
                "ignore",
                "ignore_glob",
                "lf",
                "stepwise",
                "pyargs",
            )
            if getattr(config.option, name, None)
        }
        self.destination = pathlib.Path(
            config.getoption("matrix_report")
            or self.root / "target/e2e" / f"release-{uuid.uuid4().hex}.json"
        )
        self.destination.parent.mkdir(parents=True, exist_ok=True)
        # Reserve the path before testing. An interrupted run leaves a non-passing record.
        try:
            with self.destination.open("x") as output:
                json.dump(
                    {
                        "schema_version": 2,
                        "status": "running",
                        "full_pass": False,
                        "started_at": self.started,
                    },
                    output,
                )
            self.destination.chmod(0o600)
        except FileExistsError as error:
            raise pytest.UsageError(
                "Matrix report already exists; choose a fresh path"
            ) from error

    def required(self, case):
        return self.profile == "full" or (
            not case.get("capabilities")
            and pathlib.Path(case["id"].split("::")[0]).name not in OPTIONAL_MODULES
        )

    @pytest.hookimpl(tryfirst=True)
    def pytest_collection_modifyitems(self, items):
        if self.profile == "standard" and self.inventory:
            excluded = {
                case["id"]
                for case in self.inventory["tests"]
                if not self.required(case)
            }
            self.excluded = sorted(excluded)
            items[:] = [item for item in items if item.nodeid not in excluded]

    def pytest_collection_finish(self, session):
        self.selected = [item.nodeid for item in session.items]

    def pytest_deselected(self, items):
        self.deselected.extend(item.nodeid for item in items)

    def pytest_collectreport(self, report):
        if report.skipped or report.failed:
            self.record(report, "collection")

    def pytest_runtest_logreport(self, report):
        self.record(report, report.when)

    def record(self, report, phase):
        reason = str(report.longrepr) if report.longrepr else None
        expected_failure = getattr(report, "wasxfail", None)
        allowed = None
        if report.skipped and expected_failure is None:
            known = KNOWN_SKIPS.get(report.nodeid.split("/")[-1])
            if known and reason and known[0] in reason:
                allowed = known[1]
            if allowed is None:
                self.unexpected.append(report.nodeid)
        self.reports.append(
            {
                "nodeid": report.nodeid,
                "phase": phase,
                "outcome": report.outcome,
                "duration_seconds": getattr(report, "duration", None),
                "reason": reason,
                "xfail_reason": expected_failure,
                "allowed_skip": allowed,
            }
        )

    @pytest.hookimpl(trylast=True)
    def pytest_sessionfinish(self, session, exitstatus):
        candidate_end = source_identity()
        if candidate_end != self.candidate_start or self.patch_start != command(
            self.root, "git", "diff", "--binary", "HEAD"
        ):
            self.errors.append("Source or candidate changed during execution")
        phases = {}
        for report in self.reports:
            phases.setdefault(report["nodeid"], []).append(report)
        completed = []
        for nodeid in self.selected:
            rows = phases.get(nodeid, [])
            names = [row["phase"] for row in rows]
            setup = next((row for row in rows if row["phase"] == "setup"), None)
            required = {"setup", "teardown"}
            if setup and setup["outcome"] == "passed":
                required.add("call")
            if set(names) == required and len(names) == len(required):
                completed.append(nodeid)
        try:
            if load_inventory(INVENTORY_ROOT, "e2e") != self.inventory:
                self.errors.append("Inventory changed during execution")
        except (OSError, ValueError, KeyError) as error:
            self.errors.append(str(error))
        incomplete = bool(
            self.errors
            or self.selection_restrictions
            or self.deselected
            or self.config.option.collectonly
            or not self.expected
            or len(self.selected) != len(set(self.selected))
            or set(self.selected) != set(self.expected)
            or set(completed) != set(self.expected)
        )
        failures = [
            r for r in self.reports if r["outcome"] == "failed" or r["xfail_reason"]
        ]
        passed = not (incomplete or self.unexpected or failures or int(exitstatus))
        if self.strict and not passed and int(session.exitstatus) == 0:
            session.exitstatus = 1
        record = {
            "schema_version": 2,
            "started_at": self.started,
            "finished_at": datetime.datetime.now(datetime.UTC).isoformat(),
            **self.evidence,
            "candidate_start": self.candidate_start,
            "candidate_end": candidate_end,
            "exit_status": int(session.exitstatus),
            "profile": self.profile,
            "profile_pass": passed,
            "full_pass": passed and self.profile == "full",
            "status": "incomplete" if incomplete else "passed" if passed else "failed",
            "inventory_sha256": inventory_digest(self.inventory)
            if self.inventory
            else None,
            "expected": self.expected,
            "selected": self.selected,
            "completed": completed,
            "profile_exclusions": self.excluded,
            "errors": self.errors,
            "tests_collected": session.testscollected,
            "incomplete_selection": incomplete,
            "deselected": self.deselected,
            "selection_restrictions": self.selection_restrictions,
            "unexpected_skips": self.unexpected,
            "reports": self.reports,
        }
        temporary = self.destination.with_suffix(".tmp")
        temporary.write_text(json.dumps(record, indent=2) + "\n")
        temporary.chmod(0o600)
        temporary.replace(self.destination)
        terminal = self.config.pluginmanager.get_plugin("terminalreporter")
        if terminal:
            terminal.write_sep(
                "=",
                f"{self.profile} matrix: {record['status']}; full pass: {record['full_pass']}",
            )
            terminal.write_line(str(self.destination))
