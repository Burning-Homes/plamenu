"""Strict full-matrix policy, also loadable without the live-peer fixtures."""

import datetime
import hashlib
import json
import os
import pathlib
import subprocess

# These peers are deliberately outside `up all`. Every other missing peer is
# a failed release run, even if it was healthy during the shell preflight.
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
    parser.addoption(
        "--matrix-report", help="write full-matrix JSON evidence to this path"
    )


def pytest_configure(config):
    if config.getoption("release_matrix"):
        config.pluginmanager.register(ReleaseMatrix(config), "release-matrix-policy")


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


class ReleaseMatrix:
    def __init__(self, config):
        self.config = config
        self.started = datetime.datetime.now(datetime.timezone.utc).isoformat()
        self.root = pathlib.Path(__file__).resolve().parents[2]
        self.evidence = source_evidence(self.root)
        self.reports = []
        self.deselected = []
        self.unexpected = []
        # Positional paths and ignored files bypass pytest_deselected entirely.
        # Release mode must use the configured full testpaths, including when
        # pytest's last-failed/stepwise cache would otherwise narrow collection.
        self.selection_restrictions = {
            name: getattr(config.option, name, None)
            for name in ("file_or_dir", "ignore", "ignore_glob", "lf", "stepwise")
            if getattr(config.option, name, None)
        }

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
            name = report.nodeid.split("/")[-1]
            allowed = OPTIONAL_MODULES.get(name.split("::")[0])
            known = KNOWN_SKIPS.get(name)
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

    def pytest_sessionfinish(self, session, exitstatus):
        incomplete = bool(
            self.selection_restrictions
            or self.deselected
            or self.config.option.collectonly
            or not session.testscollected
        )
        if (self.unexpected or incomplete) and int(exitstatus) == 0:
            session.exitstatus = 1
        stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
        default = self.root / "target/e2e" / f"release-{stamp}.json"
        destination = pathlib.Path(self.config.getoption("matrix_report") or default)
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(
            json.dumps(
                {
                    "schema_version": 1,
                    "started_at": self.started,
                    "finished_at": datetime.datetime.now(
                        datetime.timezone.utc
                    ).isoformat(),
                    **self.evidence,
                    "exit_status": int(session.exitstatus),
                    "tests_collected": session.testscollected,
                    "incomplete_selection": incomplete,
                    "deselected": self.deselected,
                    "selection_restrictions": self.selection_restrictions,
                    "unexpected_skips": self.unexpected,
                    "reports": self.reports,
                },
                indent=2,
            )
            + "\n"
        )
        terminal = self.config.pluginmanager.get_plugin("terminalreporter")
        if terminal:
            terminal.write_sep("=", f"release matrix: {destination}")
            if self.unexpected or incomplete:
                terminal.write_line(
                    f"Release validation failed: {len(self.unexpected)} unexpected skips; incomplete selection: {incomplete}",
                    red=True,
                )
