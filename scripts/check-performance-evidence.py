#!/usr/bin/env python3
"""Require full, strictly passing, committed performance evidence for this source."""

import importlib.util
import json
import math
import re
import subprocess
import sys
from pathlib import Path

import tomllib

ROOT = Path(__file__).resolve().parents[1]
INPUTS = [
    "crates",
    "Cargo.toml",
    "Cargo.lock",
    "bench/budgets.toml",
    "bench/bench_budgets.py",
]
spec = importlib.util.spec_from_file_location(
    "release_bench_grader", ROOT / "bench/bench_budgets.py"
)
grader = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = grader
spec.loader.exec_module(grader)


def require(condition, message):
    if not condition:
        raise ValueError(message)


def number(value, *, positive=False):
    require(
        type(value) in (int, float) and math.isfinite(value),
        "non-finite or missing measurement",
    )
    require(value > 0 if positive else value >= 0, "negative or zero measurement")
    return value


def valid_source(record, policy, seed_version):
    require(record.get("git_dirty") is False, "measurement used a dirty tree")
    sha = record.get("git_sha", "")
    require(re.fullmatch(r"[0-9a-f]{40}", sha), "missing source SHA")
    for args in [
        ("merge-base", "--is-ancestor", sha, "HEAD"),
        ("diff", "--quiet", sha, "HEAD", "--", *INPUTS),
    ]:
        result = subprocess.run(
            ["git", *args], cwd=ROOT, capture_output=True, check=False
        )
        require(
            result.returncode == 0,
            "source/budget/grader inputs differ or history is missing",
        )
    require(record.get("seed_version") == seed_version, "seed version differs")
    require(
        all(
            record.get("machine", {}).get(k) == v for k, v in policy["machine"].items()
        ),
        "calibration machine differs",
    )


def validate_hot(record, budgets, policy):
    measurements = record.get("benchmarks", {})
    require(
        set(measurements) == set(budgets),
        "hot-path case set is incomplete or differs from budgets",
    )
    for field in ["warm_up_secs", "measurement_secs"]:
        require(
            number(record.get(field)) >= policy["hot_paths"]["minimum_" + field],
            "shortened hot-path measurement",
        )
    rows = []
    for name, budget in budgets.items():
        raw = measurements[name]
        require(
            type(raw.get("samples")) is int
            and raw["samples"]
            >= policy["hot_paths"]["minimum_samples"][name.split("/", 1)[0]],
            f"too few samples for {name}",
        )
        ci = raw.get("ci_ms", [])
        require(len(ci) == 2, "missing confidence interval")
        median = number(raw.get("median_ms"), positive=True)
        require(number(ci[0]) <= median <= number(ci[1]), "invalid confidence interval")
        for field in ["queries", "bytes"]:
            if field in budget:
                require(
                    type(raw.get(field)) is int and raw[field] >= 0,
                    "missing query/body measurement",
                )
        rows.append(
            grader.Row(
                name,
                grader.Measurement(
                    median,
                    number(raw.get("mad_ms")),
                    ci[0],
                    ci[1],
                    raw["samples"],
                    0,
                    raw.get("queries"),
                    raw.get("bytes"),
                ),
                budget,
            )
        )
    grader.grade(rows, filtered=False, strict=True)
    bad = [row.name for row in rows if row.known_bad or row.failures or row.warnings]
    require(not bad, "strict hot-path grading failed: " + ", ".join(bad))


def validate_contention(record, policy):
    expected = policy["contention"]
    for field in ["workers", "fanout_width", "ratio_budget"]:
        require(
            record.get(field) == expected[field], "contention workload/budget differs"
        )
    scenarios = record.get("scenarios", {})
    require(
        set(scenarios) == set(expected["requests"]), "contention scenarios incomplete"
    )
    for name, requests in expected["requests"].items():
        result = scenarios[name]
        require(
            type(result.get("requests")) is int and result["requests"] == requests,
            "shortened contention workload",
        )
        require(
            type(result.get("failures")) is int and result["failures"] == 0,
            "contention requests failed",
        )
        require(
            result.get("ratio_budget") == expected["ratio_budget"],
            "scenario budget differs",
        )
        serial = number(result.get("serial_p50_ms"), positive=True)
        concurrent = number(result.get("concurrent_p50_ms"), positive=True)
        ratio = number(result.get("ratio"), positive=True)
        require(
            math.isclose(ratio, concurrent / serial, rel_tol=1e-8),
            "inconsistent contention ratio",
        )
        require(ratio <= expected["ratio_budget"], "contention ratio exceeded")
        require(
            concurrent
            <= number(result.get("concurrent_p99_ms"))
            <= number(result.get("concurrent_max_ms")),
            "invalid contention quantiles",
        )
        number(result.get("wall_secs"), positive=True)
        if name == "fan-out contention":
            require(
                result.get("invariant_ok") is True,
                "fan-out invariant failed or missing",
            )


def main():
    policy = tomllib.loads((ROOT / "bench/release-policy.toml").read_text())
    budgets = tomllib.loads((ROOT / "bench/budgets.toml").read_text())["budgets"]
    seed = (ROOT / "crates/server/benches/hot_paths/seed.rs").read_text()
    seed_version = re.search(r'pub const SEED_VERSION: &str = "([^"]+)"', seed).group(1)
    files = (
        subprocess.check_output(
            [
                "git",
                "ls-tree",
                "-r",
                "--name-only",
                "-z",
                "HEAD",
                "--",
                "bench/results",
            ],
            cwd=ROOT,
        )
        .decode()
        .split("\0")
    )
    accepted = {}
    rejected = []
    for name in (name for name in files if name.endswith(".json")):
        try:
            record = json.loads(
                subprocess.check_output(["git", "show", f"HEAD:{name}"], cwd=ROOT)
            )
            kind = "contention" if record.get("kind") == "contention" else "hot_paths"
            if kind in accepted:
                continue
            valid_source(record, policy, seed_version)
            if kind == "contention":
                validate_contention(record, policy)
            else:
                validate_hot(record, budgets, policy)
            accepted[kind] = name
        except (ValueError, KeyError, TypeError, AttributeError) as error:
            rejected.append(f"{name}: {error}")
    if set(accepted) != {"hot_paths", "contention"}:
        print(
            "Missing full, strictly passing committed performance evidence",
            file=sys.stderr,
        )
        print("\n".join(rejected), file=sys.stderr)
        return 1
    for kind, name in accepted.items():
        print(f"PASS {kind} evidence: {name}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
