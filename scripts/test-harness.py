#!/usr/bin/env python3
"""Run every standalone regression suite; full additionally exercises owned FUSE I/O."""

import argparse
import hashlib
import json
import os
import subprocess
import sys
import time
import uuid
import xml.etree.ElementTree as ET
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def input_files(suites):
    files = {}
    for _, command in suites:
        for argument in command:
            path = Path(argument)
            if (
                path.suffix in {".py", ".mjs", ".cjs", ".ini"}
                and path.is_relative_to(ROOT)
                and path.is_file()
            ):
                files[str(path.relative_to(ROOT))] = hashlib.sha256(
                    path.read_bytes()
                ).hexdigest()
    return files


def commands(profile, folder):
    local_python = ROOT / ".dev/gate-tools/bin/python"
    python_command = os.environ.get("PLAMENU_TEST_PYTHON") or (
        str(local_python) if local_python.exists() else sys.executable
    )
    python = [
        *sorted((ROOT / "scripts/tests").glob("test_*.py")),
        *sorted((ROOT / "bench").glob("test_*.py")),
        *sorted((ROOT / "e2e/unit").glob("test_*.py")),
        *sorted((ROOT / "e2e/tools").glob("test_*.py")),
    ]
    excluded = []
    if profile == "portable":
        excluded = [ROOT / "e2e/tools/test_primary_io_fault.py"]
        python = [p for p in python if p not in excluded]
    javascript = [
        *sorted((ROOT / "accessibility").glob("*.test.mjs")),
        *sorted((ROOT / "accessibility/unit").glob("*.test.mjs")),
        *sorted((ROOT / "scripts/tests").glob("*.test.cjs")),
    ]
    if not python or not javascript:
        raise ValueError("Standalone test discovery is empty")
    return [
        (
            "python",
            [
                python_command,
                "-m",
                "pytest",
                "-c",
                str(ROOT / "scripts/harness-pytest.ini"),
                "--noconftest",
                "--import-mode=importlib",
                "-q",
                "--maxfail=0",
                "--continue-on-collection-errors",
                "--junitxml",
                str(folder / "python.xml"),
                *map(str, python),
            ],
        ),
        (
            "javascript",
            [
                "node",
                "--test",
                "--test-reporter=junit",
                "--test-reporter-destination=" + str(folder / "javascript.xml"),
                *map(str, javascript),
            ],
        ),
    ], [str(p.relative_to(ROOT)) for p in excluded]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", choices=("portable", "full"), default="portable")
    args = parser.parse_args()
    folder = ROOT / "target/gates" / uuid.uuid4().hex
    folder.mkdir(parents=True)
    suites, exclusions = commands(args.profile, folder)
    record = {
        "schema": 1,
        "profile": args.profile,
        "status": "running",
        "full_pass": False,
        "started_at": time.time(),
        "profile_exclusions": exclusions,
        "suites": [],
        "input_files": input_files(suites),
    }
    report = folder / "report.json"
    report.write_text(json.dumps(record, indent=2) + "\n")
    environment = {
        k: v
        for k, v in os.environ.items()
        if k not in {"PYTEST_ADDOPTS", "PYTEST_PLUGINS", "NODE_OPTIONS"}
    }
    environment.update(PYTHONPATH=str(ROOT / "e2e"), PYTEST_DISABLE_PLUGIN_AUTOLOAD="1")
    for name, command in suites:
        print(f"{name}: running (log: {folder / (name + '.log')})", flush=True)
        detail = {"name": name, "command": command, "passed": False}
        try:
            with (folder / (name + ".log")).open("w") as log:
                result = subprocess.run(
                    command,
                    cwd=ROOT,
                    env=environment,
                    stdout=log,
                    stderr=subprocess.STDOUT,
                    check=False,
                )
            cases = ET.parse(folder / (name + ".xml")).findall(".//testcase")
            skipped = [
                case.find("skipped").get("message", "")
                for case in cases
                if case.find("skipped") is not None
            ]
            allowed = args.profile == "portable" and all(
                "gitleaks is required for scanner integration tests" in reason
                for reason in skipped
            )
            detail.update(
                exit_status=result.returncode,
                tests=len(cases),
                skips=skipped,
                passed=result.returncode == 0
                and bool(cases)
                and (not skipped or allowed)
                and not any(
                    case.find("failure") is not None or case.find("error") is not None
                    for case in cases
                ),
            )
        except (OSError, ET.ParseError) as error:
            detail["error"] = str(error)
        record["suites"].append(detail)
    current_suites, current_exclusions = commands(args.profile, folder)
    record["inputs_unchanged"] = (
        record["input_files"] == input_files(current_suites)
        and current_exclusions == exclusions
        and current_suites == suites
    )
    passed = record["inputs_unchanged"] and all(
        suite["passed"] for suite in record["suites"]
    )
    record.update(
        status="passed" if passed else "failed",
        finished_at=time.time(),
        full_pass=passed and args.profile == "full",
    )
    report.write_text(json.dumps(record, indent=2) + "\n")
    print(
        f"{args.profile} standalone gate: {record['status']}; full pass: {record['full_pass']}\n{report}"
    )
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
