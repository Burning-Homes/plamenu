"""Exercise the release policy through real pytest runs, without live peers."""

import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]


class ReleasePolicyTests(unittest.TestCase):
    def run_pytest(
        self, source, filename="test_required.py", *options, reuse_report=False
    ):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            import ast

            sources = source if isinstance(source, dict) else {filename: source}
            cases = []
            for name, text in sources.items():
                (root / name).write_text(text)
                for node in ast.parse(text).body:
                    if isinstance(
                        node, (ast.FunctionDef, ast.AsyncFunctionDef)
                    ) and node.name.startswith("test"):
                        capabilities = [
                            ast.literal_eval(d.args[0])
                            for d in node.decorator_list
                            if isinstance(d, ast.Call)
                            and ast.unparse(d.func) == "pytest.mark.capability"
                        ]
                        nested_capabilities = [
                            ast.literal_eval(d.args[0])
                            for d in node.decorator_list
                            if isinstance(d, ast.Call)
                            and ast.unparse(d.func) == "pytest.mark.nested_capability"
                        ]
                        cases.append(
                            {
                                "id": name + "::" + node.name,
                                "capabilities": capabilities,
                                "nested_capabilities": nested_capabilities,
                            }
                        )
            inventory = root / "inventory.json"
            inventory.write_text(
                json.dumps({"schema": 1, "suite": "e2e", "files": {}, "tests": cases})
            )
            (root / "conftest.py").write_text(
                "import json\nfrom pathlib import Path\nfrom plamenu_e2e import release_policy as policy\n"
                f"policy.load_inventory = lambda *_: json.loads(Path({str(inventory)!r}).read_text())\n"
            )

            result = subprocess.run(
                [
                    sys.executable,
                    "-m",
                    "pytest",
                    "-p",
                    "plamenu_e2e.release_policy",
                    "--release-matrix",
                    "--matrix-report",
                    str(root / "matrix.json"),
                    *options,
                ],
                cwd=root,
                env={
                    **os.environ,
                    "PYTHONPATH": str(root) + os.pathsep + str(ROOT / "e2e"),
                    "PYTEST_DISABLE_PLUGIN_AUTOLOAD": "1",
                },
                capture_output=True,
                text=True,
                check=False,
            )
            report = json.loads((root / "matrix.json").read_text())
            if reuse_report:
                previous = (root / "matrix.json").read_bytes()
                result = subprocess.run(
                    result.args,
                    cwd=root,
                    env={
                        **os.environ,
                        "PYTHONPATH": str(root) + os.pathsep + str(ROOT / "e2e"),
                        "PYTEST_DISABLE_PLUGIN_AUTOLOAD": "1",
                    },
                    capture_output=True,
                    text=True,
                    check=False,
                )
                self.assertEqual((root / "matrix.json").read_bytes(), previous)
            return result, report

    def test_source_evidence_uses_configured_peer_checkout(self):
        spec = importlib.util.spec_from_file_location(
            "release_policy", ROOT / "e2e/plamenu_e2e/release_policy.py"
        )
        policy = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(policy)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            locks = root / "e2e/peers"
            locks.mkdir(parents=True)
            (locks / "sources.lock").write_text(
                "peer|https://example.com/peer.git|expected-revision\n"
            )
            external = root / "external"
            checkout = external / "peer"
            checkout.mkdir(parents=True)
            subprocess.run(["git", "init", "-q", str(checkout)], check=True)
            subprocess.run(
                [
                    "git",
                    "-C",
                    str(checkout),
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.com",
                    "commit",
                    "-q",
                    "--allow-empty",
                    "-m",
                    "peer fixture",
                ],
                check=True,
            )
            actual = subprocess.check_output(
                ["git", "-C", str(checkout), "rev-parse", "HEAD"], text=True
            ).strip()
            with mock.patch.dict(os.environ, {"PLAMENU_PEER_ROOT": str(external)}):
                report = policy.source_evidence(root)
            self.assertEqual(report["peer_sources"][0]["checkout_revision"], actual)
            self.assertEqual(
                report["peer_sources"][0]["configured_revision"], "expected-revision"
            )
            self.assertEqual(report["peer_sources"][0]["checkout_changes"], "")

    def test_disabled_nested_coverage_cannot_qualify_as_full(self):
        source = (
            "import pytest\n"
            "@pytest.mark.nested_capability('TEST_NESTED_COVERAGE')\n"
            "def test_journey():\n"
            "    pass  # Missing nested coverage must fail before this body.\n"
        )
        with mock.patch.dict(os.environ, {"TEST_NESTED_COVERAGE": "0"}):
            result, report = self.run_pytest(source)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertFalse(report["full_pass"])
        self.assertIn("Full E2E requires", result.stdout)

    def test_enabled_nested_coverage_is_executed(self):
        source = (
            "import pytest\n"
            "@pytest.mark.nested_capability('TEST_NESTED_COVERAGE')\n"
            "def test_journey():\n"
            "    assert False, 'nested assertion executed'\n"
        )
        with mock.patch.dict(os.environ, {"TEST_NESTED_COVERAGE": "1"}):
            result, report = self.run_pytest(source)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertFalse(report["full_pass"])
        self.assertIn("nested assertion executed", result.stdout)

    def test_standard_profile_retains_core_journey_without_nested_opt_in(self):
        source = (
            "import os, pytest\n"
            "@pytest.mark.nested_capability('TEST_NESTED_COVERAGE')\n"
            "def test_journey():\n"
            "    assert os.environ['TEST_NESTED_COVERAGE'] == '0'\n"
        )
        with mock.patch.dict(os.environ, {"TEST_NESTED_COVERAGE": "0"}):
            result, report = self.run_pytest(
                source, "test_required.py", "--matrix-profile", "standard"
            )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse(report["full_pass"])
        self.assertEqual(report["deselected"], [])

    def test_required_runtime_skip_fails(self):
        result, report = self.run_pytest(
            'import pytest\ndef test_peer(): pytest.skip("peer went down after preflight")\n'
        )
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertEqual(report["unexpected_skips"], ["test_required.py::test_peer"])

    def test_existing_report_is_refused_without_overwriting_it(self):
        result, report = self.run_pytest("def test_peer(): pass\n", reuse_report=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("already exists", result.stdout + result.stderr)
        self.assertTrue(report["full_pass"])

    def test_candidate_change_invalidates_otherwise_complete_execution(self):
        result, report = self.run_pytest(
            "def test_peer():\n"
            "    from plamenu_e2e import release_policy as policy\n"
            "    policy.source_identity = lambda: {'source_revision': 'changed'}\n"
        )
        self.assertEqual(result.returncode, 1)
        self.assertFalse(report["full_pass"])
        self.assertIn("Source or candidate changed during execution", report["errors"])

    def test_required_collection_skip_fails(self):
        result, report = self.run_pytest(
            'import pytest\npytest.skip("peer unavailable", allow_module_level=True)\n'
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(report["reports"][0]["phase"], "collection")
        self.assertTrue(report["unexpected_skips"])

    def test_optional_skip_and_xfail_are_recorded(self):
        result, report = self.run_pytest(
            'import pytest\ndef test_peer(): pytest.skip("optional peer down")\n@pytest.mark.xfail(reason="known limitation", strict=True)\ndef test_gap(): assert False\n',
            "test_plup_follow.py",
        )
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertFalse(report["full_pass"])
        self.assertTrue(report["unexpected_skips"])
        self.assertTrue(
            any(row["xfail_reason"] == "known limitation" for row in report["reports"])
        )

    def test_deselection_is_not_a_full_matrix(self):
        result, report = self.run_pytest(
            "def test_one(): pass\ndef test_two(): pass\n",
            "test_required.py",
            "-k",
            "one",
        )
        self.assertEqual(result.returncode, 1)
        self.assertTrue(report["incomplete_selection"])

    def test_selectors_that_bypass_deselection_are_not_a_full_matrix(self):
        for options in [
            ("test_required.py",),
            ("--ignore", "absent.py"),
            ("--ignore-glob", "absent*"),
            ("--lf",),
            ("--sw",),
        ]:
            with self.subTest(options=options):
                result, report = self.run_pytest(
                    "def test_peer(): pass\n", "test_required.py", *options
                )
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertTrue(report["incomplete_selection"])
                self.assertTrue(report["selection_restrictions"])

    def test_documented_peer_skips_require_the_exact_test_and_reason(self):
        cases = [
            (
                "test_pleroma_federation.py",
                "test_pleroma_block_reaches_plamenu",
                "Akkoma outgoing_blocks=false",
            ),
            (
                "test_gotosocial_federation.py",
                "test_gts_move_reaches_plamenu",
                "GtS irreversibly locks a moved account",
            ),
            (
                "test_mitra_federation.py",
                "test_report_flag_unsupported",
                "Mitra 5.7.0 has no reports endpoint",
            ),
        ]
        for filename, name, reason in cases:
            with self.subTest(filename=filename):
                result, report = self.run_pytest(
                    f"import pytest\ndef {name}(): pytest.skip({reason!r})\n", filename
                )
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertFalse(report["unexpected_skips"])
                self.assertTrue(any(row["allowed_skip"] for row in report["reports"]))
                for wrong_name, wrong_reason in [
                    (name, "peer unavailable"),
                    ("test_unrelated", reason),
                ]:
                    result, report = self.run_pytest(
                        f"import pytest\ndef {wrong_name}(): pytest.skip({wrong_reason!r})\n",
                        filename,
                    )
                    self.assertEqual(
                        result.returncode, 1, result.stdout + result.stderr
                    )
                    self.assertEqual(
                        report["unexpected_skips"], [f"{filename}::{wrong_name}"]
                    )

    def test_passing_matrix(self):
        result, report = self.run_pytest("def test_peer(): pass\n")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(report["exit_status"], 0)
        self.assertEqual(report["tests_collected"], 1)
        self.assertTrue(report["full_pass"])
        self.assertEqual(report["expected"], report["completed"])
        self.assertTrue(report["git_sha"])

    def test_consumer_recomputes_coverage_instead_of_trusting_full_pass(self):
        import copy

        from plamenu_e2e.release_policy import validate_full_report

        _, report = self.run_pytest("def test_peer(): pass\n")
        inventory = {
            "schema": 1,
            "suite": "e2e",
            "files": {},
            "tests": [{"id": "test_required.py::test_peer", "capabilities": [], "nested_capabilities": []}],
        }
        validate_full_report(report, inventory)
        for mutation in ("completed", "reports", "profile"):
            with self.subTest(mutation=mutation):
                changed = copy.deepcopy(report)
                if mutation == "profile":
                    changed[mutation] = "standard"
                else:
                    changed[mutation].pop()
                with self.assertRaises(ValueError):
                    validate_full_report(changed, inventory)

    def test_standard_profile_is_explicitly_partial_even_when_it_passes(self):
        result, report = self.run_pytest(
            "import pytest\ndef test_core(): pass\n@pytest.mark.capability('OWNED_DRILL')\ndef test_drill(): assert False\n",
            "test_required.py",
            "--matrix-profile",
            "standard",
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue(report["profile_pass"])
        self.assertFalse(report["full_pass"])
        self.assertEqual(report["profile_exclusions"], ["test_required.py::test_drill"])

    def test_disabled_opt_in_cannot_be_a_full_pass(self):
        result, report = self.run_pytest(
            "import pytest\n@pytest.mark.capability('PLAMENU_GATE_TEST_DISABLED')\ndef test_drill(): pass\n"
        )
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertFalse(report["full_pass"])
        self.assertTrue(report["unexpected_skips"])

    def test_collection_only_and_clean_early_exit_are_incomplete(self):
        for source, options in [
            ("def test_one(): pass\n", ("--collect-only",)),
            (
                "import pytest\ndef test_one(): pytest.exit('stopped', returncode=0)\ndef test_two(): pass\n",
                (),
            ),
        ]:
            with self.subTest(options=options):
                result, report = self.run_pytest(source, "test_required.py", *options)
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertFalse(report["full_pass"])
                self.assertTrue(report["incomplete_selection"])

    def test_dropped_items_without_deselection_are_detected(self):
        source = "def test_one(): pass\ndef test_two(): pass\n"
        # A third-party collection hook silently removes a case after canonical discovery.
        result, report = self.run_pytest(
            {
                "test_required.py": source,
                "dropper.py": "def pytest_collection_modifyitems(items): items.pop()\n",
            },
            "test_required.py",
            "-p",
            "dropper",
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(report["full_pass"])

    def test_unknown_xfail_and_non_strict_xpass_do_not_qualify(self):
        for expression in ("False", "True"):
            result, report = self.run_pytest(
                "import pytest\n@pytest.mark.xfail(reason='unresolved', strict=False)\n"
                f"def test_gap(): assert {expression}\n"
            )
            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
            self.assertFalse(report["full_pass"])


if __name__ == "__main__":
    unittest.main()
