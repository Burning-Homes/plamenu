#!/usr/bin/env python3
"""Exit-code contract for bench_budgets.py.

The script decides whether a change ships. It had no test, so every rule it
enforces was one edit away from silently becoming a no-op — which is how it
came to grade four separate run epochs as one green table.

Run: `python3 -m pytest bench/test_bench_budgets.py`
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
from datetime import datetime
from pathlib import Path

import pytest

REPO = Path(__file__).resolve().parent.parent
SCRIPT = REPO / "bench" / "bench_budgets.py"

STARTED = "2026-07-28T00:00:00Z"


def write_criterion(root: Path, benchmarks: dict, *, manifest_extra: dict | None = None) -> Path:
    """Lays out a criterion output tree plus the run manifest the bench writes.

    `benchmarks` maps full id -> dict(median_ms, mad_ms=0, samples=20,
    queries=None, bytes=None, stale=False).
    """
    criterion = root / "criterion"
    criterion.mkdir(parents=True, exist_ok=True)
    # UTC, matching how the script reads `started_at` — not local time.
    started = datetime.fromisoformat(STARTED).timestamp()
    cases = {}
    for full_id, spec in benchmarks.items():
        group, name = full_id.split("/", 1)
        directory = criterion / group / name / "new"
        directory.mkdir(parents=True, exist_ok=True)
        (directory.parent / "new" / "benchmark.json").write_text(
            json.dumps({"full_id": full_id})
        )
        median = spec["median_ms"] * 1e6
        (directory / "estimates.json").write_text(
            json.dumps(
                {
                    "median": {
                        "point_estimate": median,
                        "confidence_interval": {
                            "lower_bound": median * 0.98,
                            "upper_bound": median * 1.02,
                        },
                    },
                    "median_abs_dev": {"point_estimate": spec.get("mad_ms", 0.0) * 1e6},
                }
            )
        )
        (directory / "sample.json").write_text(
            json.dumps({"times": [median] * spec.get("samples", 20)})
        )
        # Freshness is decided by mtime against the manifest's started_at.
        offset = -3600 if spec.get("stale") else 60
        for path in (directory / "estimates.json", directory.parent / "new" / "benchmark.json"):
            os.utime(path, (started + offset, started + offset))
        if spec.get("queries") is not None or spec.get("bytes") is not None:
            cases[full_id] = {"queries": spec.get("queries"), "bytes": spec.get("bytes")}

    manifest = {
        "seed_version": "3",
        "started_at": STARTED,
        "finished_at": "2026-07-28T00:05:00Z",
        "cases": cases,
        **(manifest_extra or {}),
    }
    (criterion / "plamenu-bench-run.json").write_text(json.dumps(manifest))
    return criterion


def write_budgets(root: Path, budgets: dict) -> Path:
    lines = ["[budgets]"]
    for name, entry in budgets.items():
        fields = ", ".join(
            f'{key} = "{value}"' if isinstance(value, str) else f"{key} = {value}"
            for key, value in entry.items()
        )
        lines.append(f'"{name}" = {{ {fields} }}')
    path = root / "budgets.toml"
    path.write_text("\n".join(lines) + "\n")
    return path


def run(criterion: Path, budgets: Path, *args: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        [
            sys.executable,
            str(SCRIPT),
            "--criterion-dir",
            str(criterion),
            "--budgets",
            str(budgets),
            "--no-record",
            *args,
        ],
        capture_output=True,
        text=True,
    )


@pytest.fixture
def clean(tmp_path):
    criterion = write_criterion(
        tmp_path, {"api/home_sparse": {"median_ms": 9.4, "queries": 7, "bytes": 43961}}
    )
    budgets = write_budgets(
        tmp_path, {"api/home_sparse": {"ms": 20.0, "queries": 7, "bytes": 43961}}
    )
    return criterion, budgets


def test_clean_run_passes(clean):
    result = run(*clean)
    assert result.returncode == 0, result.stdout
    assert "within budget" in result.stdout


def test_over_budget_fails(tmp_path):
    criterion = write_criterion(tmp_path, {"api/home_sparse": {"median_ms": 25.0}})
    budgets = write_budgets(tmp_path, {"api/home_sparse": {"ms": 20.0}})
    result = run(criterion, budgets)
    assert result.returncode == 1
    assert "over the 20.0 ms budget" in result.stdout


def test_unbudgeted_benchmark_fails(tmp_path):
    """The old script printed NO BUDGET and exited 0, so a new benchmark could
    ship ungated — and renaming one silently un-gated it."""
    criterion = write_criterion(tmp_path, {"api/brand_new": {"median_ms": 1.0}})
    budgets = write_budgets(tmp_path, {})
    result = run(criterion, budgets)
    assert result.returncode == 1
    assert "no budget entry" in result.stdout


def test_budgeted_but_unmeasured_fails_unless_filtered(tmp_path):
    criterion = write_criterion(tmp_path, {"api/home_sparse": {"median_ms": 9.4}})
    budgets = write_budgets(
        tmp_path, {"api/home_sparse": {"ms": 20.0}, "api/home_dense": {"ms": 239.0}}
    )
    assert run(criterion, budgets).returncode == 1
    assert run(criterion, budgets, "--filtered").returncode == 0


def test_stale_results_are_never_graded(tmp_path):
    """The failure this gate exists for: criterion never removes a benchmark
    directory, so leftovers from an earlier run were graded as fresh — a
    filtered run followed by an unfiltered check reported 45 stale benchmarks
    as evidence about the current tree.

    A leftover that still has a budget must surface as unmeasured, and its old
    number must not be used, even when that number would have passed."""
    criterion = write_criterion(
        tmp_path,
        {
            "api/home_sparse": {"median_ms": 9.4},
            "api/home_dense": {"median_ms": 111.9, "stale": True},
        },
    )
    budgets = write_budgets(
        tmp_path, {"api/home_sparse": {"ms": 20.0}, "api/home_dense": {"ms": 239.0}}
    )
    result = run(criterion, budgets)
    assert result.returncode == 1
    assert "not measured in this run" in result.stdout
    # Under --filtered a stale sibling is expected, not an error.
    assert run(criterion, budgets, "--filtered").returncode == 0


def test_removed_benchmark_residue_is_ignored(tmp_path):
    """A renamed or deleted benchmark leaves its directory behind forever.
    That must not fail the gate for everyone until they `cargo clean`."""
    criterion = write_criterion(
        tmp_path,
        {
            "api/home_sparse": {"median_ms": 9.4},
            "micro/sign_post_rfc9421": {"median_ms": 1.2, "stale": True},
        },
    )
    budgets = write_budgets(tmp_path, {"api/home_sparse": {"ms": 20.0}})
    result = run(criterion, budgets)
    assert result.returncode == 0, result.stdout
    assert "predate this run and were not graded" in result.stdout


def test_revision_mismatch_fails(tmp_path):
    criterion = write_criterion(
        tmp_path,
        {"api/home_sparse": {"median_ms": 9.4}},
        manifest_extra={"git_sha": "0" * 40},
    )
    budgets = write_budgets(tmp_path, {"api/home_sparse": {"ms": 20.0}})
    result = run(criterion, budgets)
    assert result.returncode == 1
    assert "HEAD is" in result.stdout
    assert run(criterion, budgets, "--allow-stale").returncode == 0


def test_extra_query_fails(tmp_path):
    criterion = write_criterion(
        tmp_path, {"api/home_sparse": {"median_ms": 9.4, "queries": 8}}
    )
    budgets = write_budgets(tmp_path, {"api/home_sparse": {"ms": 20.0, "queries": 7}})
    result = run(criterion, budgets)
    assert result.returncode == 1
    assert "SQL round trips" in result.stdout


def test_fewer_queries_warns_but_passes(tmp_path):
    criterion = write_criterion(
        tmp_path, {"api/home_sparse": {"median_ms": 9.4, "queries": 5}}
    )
    budgets = write_budgets(tmp_path, {"api/home_sparse": {"ms": 20.0, "queries": 7}})
    result = run(criterion, budgets)
    assert result.returncode == 0
    assert "ratchet" in result.stdout


def test_payload_bloat_fails(tmp_path):
    criterion = write_criterion(
        tmp_path, {"api/home_sparse": {"median_ms": 9.4, "bytes": 60000}}
    )
    budgets = write_budgets(tmp_path, {"api/home_sparse": {"ms": 20.0, "bytes": 43961}})
    result = run(criterion, budgets)
    assert result.returncode == 1
    assert "byte body over" in result.stdout
    # Inside the 1.15x band it passes.
    criterion = write_criterion(
        tmp_path, {"api/home_sparse": {"median_ms": 9.4, "bytes": 47000}}
    )
    assert run(criterion, budgets).returncode == 0


def test_dispersion_fails(tmp_path):
    criterion = write_criterion(
        tmp_path, {"api/home_sparse": {"median_ms": 9.4, "mad_ms": 3.0}}
    )
    budgets = write_budgets(tmp_path, {"api/home_sparse": {"ms": 20.0}})
    result = run(criterion, budgets)
    assert result.returncode == 1
    assert "MAD/median" in result.stdout


def test_tight_headroom_warns_and_fails_only_under_strict(tmp_path):
    """0% headroom reading `ok` is how a budget drifted to a 1.00x multiplier
    without anyone noticing."""
    criterion = write_criterion(tmp_path, {"api/home_sparse": {"median_ms": 19.5}})
    budgets = write_budgets(tmp_path, {"api/home_sparse": {"ms": 20.0}})
    assert run(criterion, budgets).returncode == 0
    result = run(criterion, budgets, "--strict")
    assert result.returncode == 1
    assert "TIGHT" in result.stdout


def test_known_bad_is_never_counted_green(tmp_path):
    criterion = write_criterion(tmp_path, {"api/broken": {"median_ms": 500.0}})
    budgets = write_budgets(
        tmp_path, {"api/broken": {"ms": 10.0, "status": "known-bad"}}
    )
    result = run(criterion, budgets)
    assert result.returncode == 0
    assert "known-bad" in result.stdout
    assert "1 known-bad benchmark(s)" in result.stdout


def test_json_output_is_machine_readable(clean):
    result = run(*clean, "--json")
    assert result.returncode == 0
    payload = json.loads(result.stdout)
    assert payload["benchmarks"]["api/home_sparse"]["queries"] == 7
    assert payload["run_failures"] == []


def test_missing_manifest_is_not_gradeable(tmp_path):
    criterion = write_criterion(tmp_path, {"api/home_sparse": {"median_ms": 9.4}})
    (criterion / "plamenu-bench-run.json").unlink()
    budgets = write_budgets(tmp_path, {"api/home_sparse": {"ms": 20.0}})
    result = run(criterion, budgets)
    assert result.returncode == 2
    assert "no run manifest" in result.stdout


def test_ratio_budget_fails_when_the_slope_grows(tmp_path):
    """A ratio budget bounds the marginal cost — one more connected client, one
    more follower — where the absolute number is a property of the machine and
    the slope is a property of the code."""
    criterion = write_criterion(
        tmp_path,
        {
            "stream/route_status_1": {"median_ms": 1.0},
            "stream/route_status_200": {"median_ms": 400.0},
        },
    )
    budgets = write_budgets(
        tmp_path,
        {
            "stream/route_status_1": {"ms": 3.0},
            "stream/route_status_200": {
                "ms": 900.0,
                "ratio_of": "stream/route_status_1",
                "ratio": 300.0,
            },
        },
    )
    result = run(criterion, budgets)
    assert result.returncode == 1
    assert "budget 300.0x" in result.stdout


def test_ratio_budget_passes_within_slope(tmp_path):
    criterion = write_criterion(
        tmp_path,
        {
            "stream/route_status_1": {"median_ms": 1.0},
            "stream/route_status_200": {"median_ms": 150.0},
        },
    )
    budgets = write_budgets(
        tmp_path,
        {
            "stream/route_status_1": {"ms": 3.0},
            "stream/route_status_200": {
                "ms": 400.0,
                "ratio_of": "stream/route_status_1",
                "ratio": 300.0,
            },
        },
    )
    result = run(criterion, budgets)
    assert result.returncode == 0, result.stdout


def test_ratio_budget_needs_its_base_measured(tmp_path):
    """A ratio against a benchmark this run did not measure is not a check that
    passed, it is a check that never ran — except under --filtered, where a
    subset is the point."""
    criterion = write_criterion(
        tmp_path, {"stream/route_status_200": {"median_ms": 150.0}}
    )
    budgets = write_budgets(
        tmp_path,
        {
            "stream/route_status_200": {
                "ms": 400.0,
                "ratio_of": "stream/route_status_1",
                "ratio": 300.0,
            },
        },
    )
    result = run(criterion, budgets)
    assert result.returncode == 1
    assert "which this run did not measure" in result.stdout
    assert run(criterion, budgets, "--filtered").returncode == 0
