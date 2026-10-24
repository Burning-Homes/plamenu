import copy
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "accessibility_evidence", ROOT / "scripts/accessibility-evidence.py"
)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class AccessibilityEvidenceTests(unittest.TestCase):
    def setUp(self):
        self.inventory = {
            "schema": 1,
            "suite": "browser",
            "tests": [
                {
                    "id": name,
                    "project": name,
                    "file": "fixture.spec.mjs",
                    "title": "required journey",
                }
                for name in sorted(MODULE.EXPECTED_PROJECTS)
            ],
        }
        self.report = {
            "schema": 2,
            "standard": "WCAG 2.2",
            "status": "passed",
            "runner_status": "passed",
            "source_revision": "candidate",
            "source_dirty": False,
            "full_pass": True,
            "complete": True,
            "errors": [],
            "failures": [],
            "exclusions": [],
            "projects": sorted(MODULE.EXPECTED_PROJECTS),
            "selected": [case["id"] for case in self.inventory["tests"]],
            "tests": [
                {
                    **case,
                    "expected_status": "passed",
                    "annotations": [],
                    "attempts": [{"retry": 0, "status": "passed", "duration_ms": 1}],
                }
                for case in self.inventory["tests"]
            ],
        }

    def validate(self, report=None, inventory=None, release=True):
        record = copy.deepcopy(report or self.report)
        inventory = inventory or self.inventory
        record.setdefault("inventory_sha256", MODULE.inventory_digest(inventory))
        with (
            tempfile.TemporaryDirectory() as directory,
            patch.object(MODULE, "load_inventory", return_value=inventory),
        ):
            path = Path(directory) / "report.json"
            path.write_text(json.dumps(record))
            return MODULE.validate_automated(path, "candidate", release=release)

    def test_wcag_22_aa_inventory_and_manual_coverage_are_complete(self):
        catalog, manual = MODULE.validate_catalogs()
        self.assertEqual(len(catalog["criteria"]), 56)
        self.assertEqual(len(catalog["preferred_aaa"]), 5)
        self.assertEqual(len(manual["checks"]), 13)

    def test_complete_matching_inventory_passes(self):
        self.assertTrue(self.validate()["full_pass"])

    def test_old_counter_only_report_is_rejected(self):
        self.report["schema"] = 1
        with self.assertRaisesRegex(MODULE.EvidenceError, "schema 2"):
            self.validate()

    def test_missing_selection_missing_result_and_duplicate_result_fail(self):
        for mutation in ("selected", "tests", "duplicate"):
            with self.subTest(mutation=mutation):
                record = copy.deepcopy(self.report)
                if mutation == "duplicate":
                    record["tests"][-1] = record["tests"][0]
                else:
                    record[mutation].pop()
                with self.assertRaises(MODULE.EvidenceError):
                    self.validate(record)

    def test_small_complete_report_cannot_replace_full_expected_inventory(self):
        inventory = copy.deepcopy(self.inventory)
        inventory["tests"].append(
            {
                "id": "missing",
                "project": "chromium-desktop",
                "file": "fixture.spec.mjs",
                "title": "missing journey",
            }
        )
        with self.assertRaisesRegex(MODULE.EvidenceError, "partial"):
            self.validate(inventory=inventory)

    def test_source_matrix_inventory_and_runner_status_are_binding(self):
        for field, value in [
            ("source_dirty", True),
            ("source_dirty", None),
            ("source_revision", "older"),
            ("projects", []),
            ("inventory_sha256", "wrong"),
            ("runner_status", "interrupted"),
            ("errors", ["collection failed"]),
            ("full_pass", False),
        ]:
            with self.subTest(field=field, value=value):
                record = copy.deepcopy(self.report)
                record[field] = value
                with self.assertRaises(MODULE.EvidenceError):
                    self.validate(record)

    def test_retry_pass_is_not_a_clean_full_pass(self):
        self.report["tests"][0]["attempts"] = [
            {"retry": 0, "status": "failed"},
            {"retry": 1, "status": "passed"},
        ]
        with self.assertRaisesRegex(MODULE.EvidenceError, "retries"):
            self.validate()

    def test_fixture_skips_and_known_defects_remain_uncovered(self):
        for annotation in (
            {"type": "skip", "description": "missing fixture"},
            {"type": "fixme", "description": "known defect"},
        ):
            with self.subTest(annotation=annotation):
                record = copy.deepcopy(self.report)
                record["tests"][0].update(
                    annotations=[annotation],
                    expected_status="skipped",
                    attempts=[{"retry": 0, "status": "skipped"}],
                )
                with self.assertRaisesRegex(
                    MODULE.EvidenceError, "required browser case"
                ):
                    self.validate(record)

    def test_declared_exclusion_requires_exact_case_project_and_reason(self):
        rule = MODULE.read_json(ROOT / "accessibility/coverage-policy.json")[
            "exclusions"
        ][0]
        project = rule["projects"][0]
        index = next(
            i
            for i, case in enumerate(self.inventory["tests"])
            if case["project"] == project
        )
        case = self.inventory["tests"][index]
        case.update(file=rule["file"], title=rule["title"])
        self.report["tests"][index].update(
            **case,
            expected_status="skipped",
            annotations=[{"type": "skip", "description": rule["reason"]}],
            attempts=[{"retry": 0, "status": "skipped"}],
        )
        self.report["exclusions"] = [case["id"]]
        self.assertTrue(self.validate()["full_pass"])
        self.report["tests"][index]["annotations"][0]["description"] = "different cause"
        with self.assertRaises(MODULE.EvidenceError):
            self.validate()


if __name__ == "__main__":
    unittest.main()
