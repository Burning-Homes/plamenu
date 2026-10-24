#!/usr/bin/env python3
"""Collect canonical inventories without executing tests or starting services."""

import argparse
import json
import os
import subprocess
import sys
import tempfile

from gate_inventory import ROOT, inventory_path, load_inventory, source_files


def collect_browser():
    result = subprocess.run(
        [
            "node",
            "accessibility/node_modules/@playwright/test/cli.js",
            "test",
            "--config",
            "accessibility/playwright.config.mjs",
            "--list",
            "--reporter=json",
        ],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    report = json.loads(result.stdout)
    if report["errors"]:
        raise ValueError("Browser inventory collection failed")
    cases = []

    def visit(suite, parents):
        for spec in suite.get("specs", []):
            for test in spec["tests"]:
                cases.append(
                    {
                        "id": spec["id"],
                        "file": spec["file"],
                        "title": " > ".join([*parents, spec["title"]]),
                        "project": test["projectName"],
                    }
                )
        for child in suite.get("suites", []):
            visit(child, [*parents, child["title"]])

    for suite in report["suites"]:
        visit(suite, [])
    return cases


def collect_e2e():
    python = ROOT / "e2e/.venv/bin/python"
    with tempfile.TemporaryDirectory() as folder:
        output = os.path.join(folder, "inventory.json")
        environment = {
            k: v
            for k, v in os.environ.items()
            if k not in {"PYTEST_ADDOPTS", "PYTEST_PLUGINS"}
        }
        environment.update(
            PYTHONPATH=str(ROOT / "e2e"), PYTEST_DISABLE_PLUGIN_AUTOLOAD="1"
        )
        subprocess.run(
            [
                str(python) if python.exists() else sys.executable,
                "-m",
                "pytest",
                "--collect-only",
                "-q",
                "-p",
                "plamenu_e2e.inventory",
                "--inventory-output",
                output,
            ],
            cwd=ROOT / "e2e",
            env=environment,
            check=True,
            stdout=subprocess.DEVNULL,
        )
        with open(output) as stream:
            return json.load(stream)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--write", action="store_true")
    parser.add_argument(
        "--check-sources",
        action="store_true",
        help="static check only; no test imports or Node dependencies",
    )
    parser.add_argument("--suite", choices=("browser", "e2e"))
    args = parser.parse_args()
    if args.write and args.check_sources:
        parser.error("--write and --check-sources are mutually exclusive")
    for suite in [args.suite] if args.suite else ["browser", "e2e"]:
        if args.check_sources:
            record = load_inventory(ROOT, suite)
        else:
            before = source_files(ROOT, suite)
            tests = collect_browser() if suite == "browser" else collect_e2e()
            if before != source_files(ROOT, suite):
                raise ValueError("Sources changed during inventory collection")
            record = {
                "schema": 1,
                "suite": suite,
                "files": before,
                "tests": sorted(tests, key=lambda item: item["id"]),
            }
            if args.write:
                header = json.dumps(
                    {key: value for key, value in record.items() if key != "tests"},
                    indent=2,
                )
                cases = ",\n".join(
                    "    " + json.dumps(case, ensure_ascii=False)
                    for case in record["tests"]
                )
                inventory_path(ROOT, suite).write_text(
                    header[:-2] + ',\n  "tests": [\n' + cases + "\n  ]\n}\n"
                )
            elif record != load_inventory(ROOT, suite):
                raise ValueError(f"{suite} collection differs from its inventory")
        print(f"PASS {suite} inventory: {len(record['tests'])} cases")


if __name__ == "__main__":
    main()
