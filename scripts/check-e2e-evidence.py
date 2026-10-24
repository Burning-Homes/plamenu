#!/usr/bin/env python3
"""Validate complete E2E evidence against this checkout, without starting peers."""

import argparse
import json
import subprocess
import sys
from pathlib import Path

from gate_inventory import ROOT, load_inventory

sys.path.insert(0, str(ROOT / "e2e"))
from plamenu_e2e.release_policy import validate_full_report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report", type=Path)
    parser.add_argument("--release", action="store_true")
    args = parser.parse_args()
    record = json.loads(args.report.read_text())
    validate_full_report(record, load_inventory(ROOT, "e2e"))
    revision = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True
    ).strip()
    if record.get("git_sha") != revision:
        raise ValueError("E2E evidence belongs to another source revision")
    if args.release and (
        record.get("tracked_changes") != ""
        or record["candidate_start"].get("source_dirty") is not False
    ):
        raise ValueError("Release E2E evidence requires clean source")
    print(f"PASS full E2E evidence: {len(record['completed'])} cases")


if __name__ == "__main__":
    main()
