"""Fixture exclusions must survive history changes without hiding new secrets."""

import json
import os
import re
import secrets
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

import tomllib

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

    def test_documented_access_token_placeholder_is_path_scoped(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            example = root / "docs/api/authentication.md"
            example.parent.mkdir(parents=True)

            def command(token):
                return (
                    "curl https://social.example.com/api/v1/accounts/verify_credentials \\\n  --header 'Authorization: Bearer "
                    + token
                    + "'\n"
                )

            example.write_text(command("ACCESS_TOKEN") + command(secrets.token_hex(24)))
            (root / "production.sh").write_text(command("ACCESS_TOKEN"))
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
                {("docs/api/authentication.md", 3), ("production.sh", 1)},
            )

    def test_historical_non_credentials_remain_value_and_path_scoped(self):
        config = tomllib.loads((ROOT / ".gitleaks.toml").read_text())
        rule = next(rule for rule in config["rules"] if rule["id"] == "generic-api-key")
        reviewed = [
            item
            for item in rule["allowlists"]
            if item["description"].startswith("Reviewed historical")
        ]
        self.assertEqual(len(reviewed), 3)
        for item in reviewed:
            self.assertEqual(item["condition"], "AND")
            self.assertEqual(item["regexTarget"], "secret")
            expression = item["regexes"][0]
            value = re.sub(r"\\(.)", r"\1", expression[1:-1])
            path_expression = item["paths"][0]
            path = re.sub(r"\\(.)", r"\1", path_expression[len("(^|/)") : -1])
            with tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                fixture = root / path
                fixture.parent.mkdir(parents=True)
                assignment = 'API_KEY="' + value + '"\n'
                fixture.write_text(
                    assignment + 'API_KEY="' + secrets.token_hex(24) + '"\n'
                )
                (root / "production.env").write_text(assignment)
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
                    {(path, 2), ("production.env", 1)},
                )
