"""Exercise the release policy through real pytest runs, without live peers."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]


class ReleasePolicyTests(unittest.TestCase):
    def run_pytest(self, source, filename="test_required.py", *options):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / filename).write_text(source)
            result = subprocess.run([sys.executable, "-m", "pytest", "-p", "plamenu_e2e.release_policy",
                "--release-matrix", "--matrix-report", str(root / "matrix.json"), *options],
                cwd=root, env={**os.environ, "PYTHONPATH": str(ROOT / "e2e"), "PYTEST_DISABLE_PLUGIN_AUTOLOAD": "1"},
                capture_output=True, text=True)
            report = json.loads((root / "matrix.json").read_text())
            return result, report

    def test_source_evidence_uses_configured_peer_checkout(self):
        spec = importlib.util.spec_from_file_location("release_policy", ROOT / "e2e/plamenu_e2e/release_policy.py")
        policy = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(policy)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            locks = root / "e2e/peers"
            locks.mkdir(parents=True)
            (locks / "sources.lock").write_text("peer|https://example.com/peer.git|expected-revision\n")
            external = root / "external"
            checkout = external / "peer"
            checkout.mkdir(parents=True)
            subprocess.run(["git", "init", "-q", str(checkout)], check=True)
            subprocess.run(["git", "-C", str(checkout), "-c", "user.name=Test", "-c", "user.email=test@example.com",
                            "commit", "-q", "--allow-empty", "-m", "peer fixture"], check=True)
            actual = subprocess.check_output(["git", "-C", str(checkout), "rev-parse", "HEAD"], text=True).strip()
            with mock.patch.dict(os.environ, {"PLAMENU_PEER_ROOT": str(external)}):
                report = policy.source_evidence(root)
            self.assertEqual(report["peer_sources"][0]["checkout_revision"], actual)
            self.assertEqual(report["peer_sources"][0]["configured_revision"], "expected-revision")
            self.assertEqual(report["peer_sources"][0]["checkout_changes"], "")

    def test_required_runtime_skip_fails(self):
        result, report = self.run_pytest('import pytest\ndef test_peer(): pytest.skip("peer went down after preflight")\n')
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertEqual(report["unexpected_skips"], ["test_required.py::test_peer"])

    def test_required_collection_skip_fails(self):
        result, report = self.run_pytest('import pytest\npytest.skip("peer unavailable", allow_module_level=True)\n')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(report["reports"][0]["phase"], "collection")
        self.assertTrue(report["unexpected_skips"])

    def test_optional_skip_and_xfail_are_recorded(self):
        result, report = self.run_pytest('import pytest\ndef test_peer(): pytest.skip("optional peer down")\n@pytest.mark.xfail(reason="known limitation", strict=True)\ndef test_gap(): assert False\n', 'test_plup_follow.py')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse(report["unexpected_skips"])
        self.assertTrue(any(row["allowed_skip"] for row in report["reports"]))
        self.assertTrue(any(row["xfail_reason"] == "known limitation" for row in report["reports"]))

    def test_deselection_is_not_a_full_matrix(self):
        result, report = self.run_pytest('def test_one(): pass\ndef test_two(): pass\n', 'test_required.py', '-k', 'one')
        self.assertEqual(result.returncode, 1)
        self.assertTrue(report["incomplete_selection"])

    def test_selectors_that_bypass_deselection_are_not_a_full_matrix(self):
        for options in [("test_required.py",), ("--ignore", "absent.py"),
                        ("--ignore-glob", "absent*"), ("--lf",), ("--sw",)]:
            with self.subTest(options=options):
                result, report = self.run_pytest('def test_peer(): pass\n', 'test_required.py', *options)
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertTrue(report["incomplete_selection"])
                self.assertTrue(report["selection_restrictions"])

    def test_documented_peer_skips_require_the_exact_test_and_reason(self):
        cases = [
            ('test_pleroma_federation.py', 'test_pleroma_block_reaches_plamenu', 'Akkoma outgoing_blocks=false'),
            ('test_gotosocial_federation.py', 'test_gts_move_reaches_plamenu', 'GtS irreversibly locks a moved account'),
            ('test_mitra_federation.py', 'test_report_flag_unsupported', 'Mitra 5.7.0 has no reports endpoint'),
        ]
        for filename, name, reason in cases:
            with self.subTest(filename=filename):
                result, report = self.run_pytest(f'import pytest\ndef {name}(): pytest.skip({reason!r})\n', filename)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertFalse(report['unexpected_skips'])
                self.assertTrue(any(row['allowed_skip'] for row in report['reports']))
                for wrong_name, wrong_reason in [(name, 'peer unavailable'), ('test_unrelated', reason)]:
                    result, report = self.run_pytest(f'import pytest\ndef {wrong_name}(): pytest.skip({wrong_reason!r})\n', filename)
                    self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                    self.assertEqual(report['unexpected_skips'], [f'{filename}::{wrong_name}'])

    def test_passing_matrix(self):
        result, report = self.run_pytest('def test_peer(): pass\n')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(report["exit_status"], 0)
        self.assertEqual(report["tests_collected"], 1)
        self.assertTrue(report["git_sha"])


if __name__ == "__main__":
    unittest.main()
