import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "accessibility_evidence", ROOT / "scripts/accessibility-evidence.py"
)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class AccessibilityEvidenceTests(unittest.TestCase):
    def test_wcag_22_aa_inventory_and_manual_coverage_are_complete(self):
        catalog, manual = MODULE.validate_catalogs()
        self.assertEqual(len(catalog["criteria"]), 56)
        self.assertEqual(len(catalog["preferred_aaa"]), 5)
        self.assertEqual(len(manual["checks"]), 13)

    def test_automated_report_requires_every_browser_project(self):
        with tempfile.TemporaryDirectory() as folder:
            report = Path(folder) / "automated.json"
            report.write_text(
                json.dumps(
                    {
                        "schema": 1,
                        "standard": "WCAG 2.2",
                        "status": "passed",
                        "source_revision": "abc",
                        "source_dirty": False,
                        "projects": {},
                        "tests": [{"status": "passed"}],
                    }
                )
            )
            with self.assertRaisesRegex(MODULE.EvidenceError, "complete browser"):
                MODULE.validate_automated(report, "abc", release=True)

    def test_automated_report_rejects_dirty_release_source(self):
        with tempfile.TemporaryDirectory() as folder:
            report = Path(folder) / "automated.json"
            report.write_text(
                json.dumps(
                    {
                        "schema": 1,
                        "standard": "WCAG 2.2",
                        "status": "passed",
                        "source_revision": "abc",
                        "source_dirty": True,
                        "projects": {
                            name: {"passed": 1, "failed": 0, "skipped": 0}
                            for name in MODULE.EXPECTED_PROJECTS
                        },
                        "tests": [{"status": "passed"}],
                    }
                )
            )
            with self.assertRaisesRegex(MODULE.EvidenceError, "dirty working tree"):
                MODULE.validate_automated(report, "abc", release=True)


if __name__ == "__main__":
    unittest.main()
