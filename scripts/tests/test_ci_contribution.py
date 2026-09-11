"""Contribution checks must handle initial history without bypassing DCO or seed checks."""

import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "ci/check-contribution.py"
SEED_CHECK = SCRIPT.parents[1] / "check-seed-version.sh"


class ContributionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.git("init", "-q")
        self.git("config", "user.name", "CI Test")
        self.git("config", "user.email", "ci@example.org")
        (self.root / "scripts").mkdir()
        shutil.copy2(SEED_CHECK, self.root / "scripts/check-seed-version.sh")
        self.seed = self.root / "crates/server/benches/hot_paths/seed.rs"
        self.seed.parent.mkdir(parents=True)
        self.seed.write_text('pub const SEED_VERSION: &str = "1";\n// initial shape\n')
        self.git("add", ".")
        self.git("commit", "-qm", "Initial", "--signoff")
        self.base = self.git("rev-parse", "HEAD").strip()
        self.main_branch = self.git("branch", "--show-current").strip()

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.root, text=True)

    def change(self, signed=True):
        (self.root / "change").write_text("changed\n")
        self.git("add", ".")
        self.git("commit", "-qm", "Change", *(["--signoff"] if signed else []))

    def merge_change(self, signed=True):
        self.git("switch", "-qc", "feature")
        self.change(signed=signed)
        feature = self.git("rev-parse", "HEAD").strip()
        self.git("switch", "-q", self.main_branch)
        self.git("merge", "--no-ff", "-qm", "Merge feature", "feature")
        return feature

    def check(self, base=None, kind="push", manual_base=""):

        event = self.root / "event.json"
        event.write_text(
            json.dumps(
                {
                    "before": base or self.base,
                    "after": self.git("rev-parse", "HEAD").strip(),
                }
            )
        )
        return subprocess.run(
            ["python3", str(SCRIPT)],
            cwd=self.root,
            check=False,
            capture_output=True,
            text=True,
            env={
                **os.environ,
                "GITHUB_EVENT_PATH": str(event),
                "GITHUB_EVENT_NAME": kind,
                "CHECK_BASE": manual_base,
            },
        )

    def test_signed_change_passes(self):
        self.change()
        result = self.check()
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_unsigned_change_rejected(self):
        self.change(signed=False)
        self.assertIn("Missing DCO sign-off", self.check().stderr)

    def test_unsigned_merge_commit_is_exempt(self):
        self.merge_change()
        result = self.check()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("1 non-merge DCO commits, 2 total commits", result.stdout)

    def test_unsigned_commit_behind_merge_is_rejected(self):
        unsigned = self.merge_change(signed=False)
        result = self.check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(f"Missing DCO sign-off: {unsigned}", result.stderr)

    def test_missing_base_rejected(self):
        self.change()
        result = self.check("unknown")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("nonzero full commit SHA", result.stderr)

    def test_seed_change_without_version_rejected(self):
        self.seed.write_text(self.seed.read_text() + "// changed dataset\n")
        self.change()
        result = self.check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("SEED_VERSION did not", result.stderr)

    def test_signoff_in_body_is_not_a_trailer(self):
        self.git(
            "commit",
            "--allow-empty",
            "-qm",
            "Change\n\nSigned-off-by: Someone <ci@example.org>\n\nTrailing prose",
        )
        self.assertIn("Missing DCO sign-off", self.check().stderr)

    def test_initial_root_push_and_manual_dispatch_pass(self):
        for kind in ("push", "workflow_dispatch"):
            result = self.check("0" * 40, kind=kind)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("root..", result.stdout)

    def test_new_branch_checks_all_history_for_signoffs(self):
        self.change(signed=False)
        self.git("commit", "--allow-empty", "-qm", "Signed tip", "--signoff")
        self.assertIn("Missing DCO sign-off", self.check("0" * 40).stderr)

    def test_rewritten_root_push_checks_complete_history(self):
        self.git("commit", "--amend", "-qm", "Replacement root", "--signoff")
        for before in (self.base, "f" * 40):
            result = self.check(before)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("root..", result.stdout)
        self.assertNotEqual(self.check("unknown").returncode, 0)

    def test_rewritten_root_still_requires_signoff(self):
        self.git("commit", "--amend", "-qm", "Replacement without signoff")
        self.assertIn("Missing DCO sign-off", self.check(self.base).stderr)

    def test_unsigned_root_is_rejected(self):
        self.git("commit", "--amend", "-qm", "Root without signoff")
        self.assertIn("Missing DCO sign-off", self.check("0" * 40).stderr)

    def test_new_branch_checks_intermediate_seed_changes(self):
        self.seed.write_text(self.seed.read_text() + "// changed dataset\n")
        self.change()
        self.seed.write_text('pub const SEED_VERSION: &str = "2";\n')
        self.git("add", ".")
        self.git("commit", "-qm", "Later bump", "--signoff")
        result = self.check("0" * 40)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("SEED_VERSION did not", result.stderr)

    def test_shallow_initial_history_is_rejected(self):
        (self.root / ".git/shallow").write_text(self.base + "\n")
        self.assertIn("complete Git history", self.check("0" * 40).stderr)

    def test_manual_base_and_normal_pull_request(self):
        self.change()
        result = self.check(kind="workflow_dispatch", manual_base=self.base)
        self.assertEqual(result.returncode, 0, result.stderr)
        event = self.root / "pr.json"
        event.write_text(
            json.dumps(
                {
                    "pull_request": {
                        "head": {"sha": self.git("rev-parse", "HEAD").strip()},
                        "base": {"sha": self.base},
                    }
                }
            )
        )
        result = subprocess.run(
            ["python3", str(SCRIPT)],
            cwd=self.root,
            check=False,
            capture_output=True,
            text=True,
            env={
                **os.environ,
                "GITHUB_EVENT_PATH": str(event),
                "GITHUB_EVENT_NAME": "pull_request",
            },
        )
        self.assertEqual(result.returncode, 0, result.stderr)
