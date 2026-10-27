"""Guard the bounded 0.7 exception against stale source and changed evidence."""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from plamenu_release import acceptance, core
from plamenu_release.core import ReleaseError, digest


class AcceptanceTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.source = self.root / "source"
        self.source.mkdir()
        self.evidence = self.root / "evidence"
        self.evidence.mkdir()
        self.git("init", "-q")
        self.git("config", "user.email", "test@example.com")
        self.git("config", "user.name", "Release Test")
        self.git("config", "commit.gpgsign", "false")
        for name in [
            "Cargo.lock",
            "crates/app.rs",
            "e2e/test.py",
            "docs/examples/install.sh",
        ]:
            file = self.source / name
            file.parent.mkdir(parents=True, exist_ok=True)
            file.write_text("reviewed input\n")
        (self.source / "accessibility").mkdir()
        (self.source / "accessibility/inventory.json").write_text(
            json.dumps({"files": {}, "tests": [{"id": "retained-case"}]})
        )
        self.commit()
        revision = self.git("rev-parse", "HEAD")
        (self.evidence / "audit.json").write_text('{"full_pass":false}')
        self.policy = {
            "schema": 1,
            "version": "0.7.0",
            "full_pass": False,
            "wcag_conformance": False,
            "full_audit_revision": revision,
            "repair_revision": revision,
            "accepted_gaps": {"human": "not performed"},
            "validated_source_fingerprint": acceptance.source_fingerprint(
                self.source, revision
            ),
            "evidence": {"audit.json": digest(self.evidence / "audit.json")},
        }
        path = self.source / acceptance.POLICY
        path.parent.mkdir()
        path.write_text(json.dumps(self.policy))
        self.commit()

    def git(self, *args):
        return subprocess.check_output(
            ["git", *args], cwd=self.source, text=True
        ).strip()

    def commit(self):
        self.git("add", ".")
        self.git("commit", "-qm", "fixture")

    def validate(self, version="0.7.0"):
        return acceptance.validate(
            self.source, self.evidence, version, self.git("rev-parse", "HEAD")
        )

    def test_docs_prose_can_change_without_relabeling_historical_evidence(self):
        (self.source / "docs/guide.md").write_text("Updated guidance\n")
        self.commit()
        result = self.validate()
        self.assertFalse(result["full_pass"])
        self.assertFalse(result["wcag_conformance"])
        self.assertNotEqual(result["source_revision"], result["full_audit_revision"])

    def test_changed_runtime_dependency_test_or_installation_inputs_are_rejected(self):
        baseline = self.git("rev-parse", "HEAD")
        for name in [
            "Cargo.lock",
            "crates/app.rs",
            "e2e/test.py",
            "docs/examples/install.sh",
            "new-runtime.rs",
        ]:
            with self.subTest(name=name):
                (self.source / name).write_text("changed\n")
                self.commit()
                with self.assertRaisesRegex(ReleaseError, "inputs changed"):
                    self.validate()
                self.git("reset", "--hard", baseline)

    def test_missing_or_modified_evidence_is_not_accepted(self):
        (self.evidence / "audit.json").write_text('{"full_pass":true}')
        with self.assertRaisesRegex(ReleaseError, "Missing or changed"):
            self.validate()
        (self.evidence / "audit.json").unlink()
        with self.assertRaisesRegex(ReleaseError, "Missing or changed"):
            self.validate()

    def test_removed_browser_case_is_rejected(self):
        (self.source / "accessibility/inventory.json").write_text(
            json.dumps({"files": {}, "tests": []})
        )
        self.commit()
        with self.assertRaisesRegex(ReleaseError, "inputs changed"):
            self.validate()

    def test_other_versions_cannot_reuse_the_exception(self):
        for version in ["0.7.1", "0.8.0", "0.7.0-rc1"]:
            with (
                self.subTest(version=version),
                self.assertRaisesRegex(ReleaseError, "only to 0.7.0"),
            ):
                self.validate(version)

    def test_policy_cannot_claim_conformance_or_full_pass(self):
        for key in ["full_pass", "wcag_conformance"]:
            with self.subTest(key=key):
                policy = self.policy | {key: True}
                (self.source / acceptance.POLICY).write_text(json.dumps(policy))
                with self.assertRaisesRegex(ReleaseError, "Invalid"):
                    self.validate()

    def prepare_application_reuse(self):
        manifest = self.source / "Cargo.toml"
        manifest.write_text(
            '[workspace.package]\nversion = "0.7.0"\n'
            '[profile.deploy]\ninherits = "release"\nlto = "fat"\ncodegen-units = 1\n'
        )
        self.commit()
        previous = self.root / "previous"
        previous.mkdir()
        identity = {
            "version": "0.7.0",
            "source": self.git("rev-parse", "HEAD"),
            "jobs": 2,
        }
        core.write_json(previous / "identity.json", identity)
        core.stage(
            self.source,
            previous,
            "application",
            identity,
            lambda r, p: (p / "run.log").write_text("3594 passed; 2 opt-in excluded\n"),
        )
        manifest.write_text(
            manifest.read_text()
            .replace('lto = "fat"', 'lto = "thin"')
            .replace("codegen-units = 1\n", "codegen-units = 16\n")
        )
        self.commit()
        return previous

    def test_profile_only_reuse_retains_original_evidence_and_exclusions(self):
        previous = self.prepare_application_reuse()
        revision = self.git("rev-parse", "HEAD")
        receipt = acceptance.application_reuse(self.source, previous, revision)
        self.assertFalse(receipt["full_pass"])
        self.assertNotEqual(receipt["source_revision"], revision)
        destination = self.root / "reused"
        destination.mkdir()
        acceptance.record_application_reuse(
            core.Runner(self.source, destination / "run.log"),
            destination,
            previous,
            revision,
            receipt,
        )
        self.assertEqual(
            (previous / "application/stage.json").read_bytes(),
            (destination / "previous/stage.json").read_bytes(),
        )
        self.assertIn(
            "2 opt-in excluded", (destination / "previous/run.log").read_text()
        )

    def test_application_reuse_rejects_changed_inputs_and_other_profile_changes(self):
        previous = self.prepare_application_reuse()
        baseline = self.git("rev-parse", "HEAD")
        for name, contents in [
            ("Cargo.lock", "changed"),
            ("crates/app.rs", "changed"),
            ("e2e/test.py", "changed"),
            ("docs/examples/install.sh", "changed"),
            ("scripts/plamenu_release/checks.py", "changed"),
            ("accessibility/inventory.json", "{}"),
            (
                "Cargo.toml",
                (self.source / "Cargo.toml").read_text() + 'panic = "abort"\n',
            ),
            (
                "Cargo.toml",
                (self.source / "Cargo.toml")
                .read_text()
                .replace('version = "0.7.0"', 'version = "0.7.1"'),
            ),
        ]:
            with self.subTest(name=name, contents=contents):
                (self.source / name).parent.mkdir(parents=True, exist_ok=True)
                (self.source / name).write_text(contents)
                self.commit()
                with self.assertRaises(ReleaseError):
                    acceptance.application_reuse(
                        self.source, previous, self.git("rev-parse", "HEAD")
                    )
                self.git("reset", "--hard", baseline)

    def test_application_reuse_rejects_failed_missing_and_modified_stage(self):
        previous = self.prepare_application_reuse()
        stage = previous / "application/stage.json"
        original = stage.read_text()
        for status in ("failed", "running"):
            record = json.loads(original)
            record["status"] = status
            stage.write_text(json.dumps(record))
            with self.assertRaises(ReleaseError):
                acceptance.application_reuse(
                    self.source, previous, self.git("rev-parse", "HEAD")
                )
        stage.write_text(original)
        (previous / "application/run.log").write_text("substituted")
        with self.assertRaises(ReleaseError):
            acceptance.application_reuse(
                self.source, previous, self.git("rev-parse", "HEAD")
            )
        stage.unlink()
        with self.assertRaises(ReleaseError):
            acceptance.application_reuse(
                self.source, previous, self.git("rev-parse", "HEAD")
            )


if __name__ == "__main__":
    unittest.main()
