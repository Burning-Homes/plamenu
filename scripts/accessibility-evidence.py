#!/usr/bin/env python3
"""Validate WCAG coverage and create or verify accessibility release evidence."""

import argparse
import hashlib
import json
import subprocess
import sys
from datetime import datetime
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CATALOG = ROOT / "accessibility/criteria.json"
MANUAL = ROOT / "accessibility/manual-checks.json"
EXPECTED = {
    "1.1.1": "A",
    "1.2.1": "A",
    "1.2.2": "A",
    "1.2.3": "A",
    "1.2.4": "AA",
    "1.2.5": "AA",
    "1.3.1": "A",
    "1.3.2": "A",
    "1.3.3": "A",
    "1.3.4": "AA",
    "1.3.5": "AA",
    "1.4.1": "A",
    "1.4.2": "A",
    "1.4.3": "AA",
    "1.4.4": "AA",
    "1.4.5": "AA",
    "1.4.10": "AA",
    "1.4.11": "AA",
    "1.4.12": "AA",
    "1.4.13": "AA",
    "2.1.1": "A",
    "2.1.2": "A",
    "2.1.4": "A",
    "2.2.1": "A",
    "2.2.2": "A",
    "2.3.1": "A",
    "2.4.1": "A",
    "2.4.2": "A",
    "2.4.3": "A",
    "2.4.4": "A",
    "2.4.5": "AA",
    "2.4.6": "AA",
    "2.4.7": "AA",
    "2.4.11": "AA",
    "2.5.1": "A",
    "2.5.2": "A",
    "2.5.3": "A",
    "2.5.4": "A",
    "2.5.7": "AA",
    "2.5.8": "AA",
    "3.1.1": "A",
    "3.1.2": "AA",
    "3.2.1": "A",
    "3.2.2": "A",
    "3.2.3": "AA",
    "3.2.4": "AA",
    "3.2.6": "A",
    "3.3.1": "A",
    "3.3.2": "A",
    "3.3.3": "AA",
    "3.3.4": "AA",
    "3.3.7": "A",
    "3.3.8": "AA",
    "4.1.1": "A",
    "4.1.2": "A",
    "4.1.3": "AA",
}
EXPECTED_PROJECTS = {
    "chromium-desktop",
    "chromium-mobile",
    "firefox-desktop",
    "firefox-mobile",
    "webkit-desktop",
    "webkit-mobile",
    "chromium-no-javascript",
}


class EvidenceError(ValueError):
    pass


def read_json(path):
    try:
        return json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as error:
        raise EvidenceError(f"cannot read {path}: {error}") from error


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def revision():
    return subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()


def validate_catalogs():
    catalog = read_json(CATALOG)
    manual = read_json(MANUAL)
    if catalog.get("schema") != 1 or manual.get("schema") != 1:
        raise EvidenceError("unsupported accessibility catalog schema")
    criteria = catalog.get("criteria")
    checks = manual.get("checks")
    if not isinstance(criteria, list) or not isinstance(checks, list):
        raise EvidenceError("accessibility catalogs must contain lists")
    by_id = {item.get("id"): item for item in criteria}
    if len(by_id) != len(criteria) or set(by_id) != set(EXPECTED):
        missing = sorted(set(EXPECTED) - set(by_id))
        extra = sorted(set(by_id) - set(EXPECTED))
        raise EvidenceError(
            f"WCAG 2.2 A/AA inventory mismatch; missing={missing}, extra={extra}"
        )
    manual_by_id = {item.get("id"): item for item in checks}
    if len(manual_by_id) != len(checks) or None in manual_by_id:
        raise EvidenceError("manual check IDs must be present and unique")
    allowed_evidence = set(manual_by_id) | {catalog.get("automated_evidence")}
    preferred = catalog.get("preferred_aaa")
    if not isinstance(preferred, list) or not preferred:
        raise EvidenceError("preferred AAA targets must be recorded")
    preferred_ids = {item.get("id") for item in preferred}
    if len(preferred_ids) != len(preferred) or None in preferred_ids:
        raise EvidenceError("preferred AAA target IDs must be present and unique")
    for item in preferred:
        if item.get("level") != "AAA" or not item.get("name"):
            raise EvidenceError("preferred AAA targets need names and AAA levels")
        evidence = item.get("evidence")
        if (
            not isinstance(evidence, list)
            or not evidence
            or set(evidence) - allowed_evidence
        ):
            raise EvidenceError(
                f"preferred AAA target {item.get('id')} has invalid evidence"
            )
    for criterion, level in EXPECTED.items():
        item = by_id[criterion]
        if item.get("level") != level or not item.get("name"):
            raise EvidenceError(
                f"criterion {criterion} has an incorrect level or missing name"
            )
        applicability = item.get("applicability")
        if applicability not in {"applicable", "conditional", "obsolete"}:
            raise EvidenceError(f"criterion {criterion} has invalid applicability")
        evidence = item.get("evidence")
        if not isinstance(evidence, list) or set(evidence) - allowed_evidence:
            raise EvidenceError(f"criterion {criterion} references unknown evidence")
        if applicability == "obsolete":
            if not item.get("rationale") or evidence:
                raise EvidenceError(
                    f"obsolete criterion {criterion} needs only a rationale"
                )
        elif not evidence or not (set(evidence) & set(manual_by_id)):
            raise EvidenceError(
                f"criterion {criterion} needs reproducible manual evidence"
            )
    covered = set()
    for check_id, check in manual_by_id.items():
        if not check.get("name") or not check.get("procedure"):
            raise EvidenceError(f"manual check {check_id} is incomplete")
        criterion_set = set(check.get("criteria", []))
        if not criterion_set or criterion_set - set(EXPECTED):
            raise EvidenceError(
                f"manual check {check_id} has invalid criterion coverage"
            )
        covered |= criterion_set
    required_manual = {key for key in EXPECTED if key != "4.1.1"}
    if covered != required_manual:
        raise EvidenceError(
            "manual coverage mismatch; "
            f"missing={sorted(required_manual - covered)}, extra={sorted(covered - required_manual)}"
        )
    return catalog, manual


def validate_automated(path, required_revision=None, release=False):
    report = read_json(path)
    if report.get("schema") != 1 or report.get("standard") != "WCAG 2.2":
        raise EvidenceError("automated report has an unsupported schema or standard")
    if required_revision and report.get("source_revision") != required_revision:
        raise EvidenceError("automated report was produced from a different revision")
    if release and report.get("source_dirty"):
        raise EvidenceError("release evidence cannot come from a dirty working tree")
    if report.get("status") != "passed":
        raise EvidenceError("automated accessibility suite did not pass")
    projects = report.get("projects")
    if not isinstance(projects, dict) or set(projects) != EXPECTED_PROJECTS:
        raise EvidenceError(
            "automated report does not contain the complete browser/project matrix"
        )
    for name, counts in projects.items():
        if counts.get("failed") or counts.get("skipped") or not counts.get("passed"):
            raise EvidenceError(f"automated project {name} is incomplete or failed")
    if not report.get("tests"):
        raise EvidenceError("automated report contains no test results")
    return report


def initialize(args):
    _, manual = validate_catalogs()
    automated = args.automated.resolve()
    report = validate_automated(automated, revision(), release=not args.allow_dirty)
    try:
        automated_ref = str(automated.relative_to(args.output.resolve().parent))
    except ValueError:
        automated_ref = str(automated)
    record = {
        "schema": 1,
        "standard": "WCAG 2.2",
        "level": "AA",
        "source_revision": report["source_revision"],
        "scope": "Plamenu first-party web interface and complete first-party processes; third-party federated and Webxdc content must be bounded per the recorded procedure.",
        "automated_report": {
            "path": automated_ref,
            "sha256": digest(automated),
        },
        "checks": [
            {
                "id": check["id"],
                "status": "pending",
                "tester": "",
                "checked_at": "",
                "environments": [],
                "artifacts": [],
                "notes": "",
            }
            for check in manual["checks"]
        ],
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(record, indent=2) + "\n")
    print(f"Wrote pending accessibility evidence: {args.output}")


def validate_evidence(args):
    _, manual = validate_catalogs()
    record = read_json(args.evidence)
    required_revision = args.revision or revision()
    if record.get("schema") != 1 or record.get("standard") != "WCAG 2.2":
        raise EvidenceError("release evidence has an unsupported schema or standard")
    if (
        record.get("level") != "AA"
        or record.get("source_revision") != required_revision
    ):
        raise EvidenceError(
            "release evidence does not target this revision at WCAG 2.2 AA"
        )
    automated = record.get("automated_report", {})
    automated_path = Path(automated.get("path", ""))
    if not automated_path.is_absolute():
        automated_path = (args.evidence.parent / automated_path).resolve()
    if not automated_path.is_file() or digest(automated_path) != automated.get(
        "sha256"
    ):
        raise EvidenceError("automated report is missing or its digest changed")
    validate_automated(automated_path, required_revision, release=args.release)
    expected_checks = {check["id"] for check in manual["checks"]}
    entries = record.get("checks")
    if not isinstance(entries, list):
        raise EvidenceError("release evidence checks must be a list")
    by_id = {entry.get("id"): entry for entry in entries}
    if len(by_id) != len(entries) or set(by_id) != expected_checks:
        raise EvidenceError(
            "release evidence does not contain each manual check exactly once"
        )
    if args.release:
        for check_id, entry in by_id.items():
            if entry.get("status") != "pass":
                raise EvidenceError(f"manual check {check_id} has not passed")
            if not entry.get("tester") or not entry.get("checked_at"):
                raise EvidenceError(f"manual check {check_id} lacks tester or date")
            try:
                datetime.fromisoformat(entry["checked_at"].replace("Z", "+00:00"))
            except (TypeError, ValueError) as error:
                raise EvidenceError(
                    f"manual check {check_id} has an invalid date"
                ) from error
            if not entry.get("environments") or not entry.get("artifacts"):
                raise EvidenceError(
                    f"manual check {check_id} lacks environment or artifact evidence"
                )
    print(
        f"Accessibility evidence is {'release-complete' if args.release else 'structurally valid'} "
        f"for {required_revision[:12]}"
    )


def parser():
    result = argparse.ArgumentParser(description=__doc__)
    commands = result.add_subparsers(dest="command", required=True)
    commands.add_parser(
        "matrix", help="validate the criterion and manual-check catalogs"
    )
    init = commands.add_parser("init", help="create a pending manual evidence record")
    init.add_argument("--automated", type=Path, required=True)
    init.add_argument("--allow-dirty", action="store_true")
    init.add_argument("output", type=Path)
    validate = commands.add_parser("validate", help="validate a filled evidence record")
    validate.add_argument("--release", action="store_true")
    validate.add_argument("--revision")
    validate.add_argument("evidence", type=Path)
    return result


def main():
    args = parser().parse_args()
    try:
        if args.command == "matrix":
            validate_catalogs()
            print(f"WCAG 2.2 A/AA matrix is complete: {len(EXPECTED)} criteria")
        elif args.command == "init":
            initialize(args)
        else:
            validate_evidence(args)
    except (EvidenceError, OSError, subprocess.CalledProcessError) as error:
        print(f"Accessibility evidence error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
