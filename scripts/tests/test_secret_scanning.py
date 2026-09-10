"""Fixture exclusions must survive history changes without hiding new secrets."""

import json
import os
import secrets
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCANNER = os.environ.get("GITLEAKS") or shutil.which("gitleaks")


@unittest.skipUnless(SCANNER, "gitleaks is required for scanner integration tests")
class SecretScanningTests(unittest.TestCase):
    def test_fixture_exclusions_require_both_exact_value_and_path(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            fixture = root / "e2e/peers/dev"
            fixture.parent.mkdir(parents=True)
            login = next(
                line
                for line in (ROOT / "e2e/peers/dev").read_text().splitlines()
                if line.startswith("GTS_DEV_PASSWORD=")
            )
            fixture.write_text(
                "\n\n" + login + "\n" + 'API_KEY="' + secrets.token_hex(24) + '"\n'
            )
            elsewhere = root / "production.env"
            elsewhere.write_text(login + "\n")
            report = root / "findings.json"
            result = subprocess.run(
                [
                    SCANNER,
                    "dir",
                    "--no-banner",
                    "--redact",
                    "--config",
                    str(ROOT / ".gitleaks.toml"),
                    "--report-path",
                    str(report),
                    str(root),
                ],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(result.returncode, 1, result.stderr)
            findings = json.loads(report.read_text())
            self.assertEqual(
                {
                    (Path(f["File"]).relative_to(root).as_posix(), f["StartLine"])
                    for f in findings
                },
                {("e2e/peers/dev", 4), ("production.env", 1)},
            )
