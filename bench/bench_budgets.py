#!/usr/bin/env python3
"""Grade a benchmark run against bench/budgets.toml and record it.

Run after `cargo bench -p plamenu` (or via `./dev bench-check`):

    python3 bench/bench_budgets.py                 # grade the last run
    python3 bench/bench_budgets.py --filtered      # after `cargo bench -- <filter>`
    python3 bench/bench_budgets.py --strict        # also fail on TIGHT headroom
    python3 bench/bench_budgets.py --json          # machine-readable, no table

What it checks, per benchmark:

  ms       criterion's median against the budget ceiling
  queries  SQL statements per request, exact — the one machine-independent cap
  bytes    response body size, ceiling 1.15x the budget
  ratio    this benchmark's median over a named sibling's, for the cases where
           the cost that matters is marginal (one more connected client, one
           more follower) rather than absolute
  MAD      dispersion (MAD/median); over 15% means the median is not describing
           a single population, so the budget comparison is not meaningful

And, per run:

  freshness    every measurement must postdate the run manifest the bench
               wrote. Criterion never removes a benchmark directory, so
               without this the script grades whatever happens to be on disk —
               it was reporting results from four separate run epochs as one
               green table.
  revision     the manifest's git sha must equal HEAD, so a green result is
               evidence about the tree you are looking at
  completeness a benchmark with no budget entry fails; a budget entry with no
               measurement fails (both relax under --filtered)

A passing run is recorded to bench/results/<sha>-<timestamp>.json, which is
committed. That file is the project's performance history: /target is
gitignored, so before it existed a `cargo clean` erased every measurement ever
taken and no second machine could compare against anything.

See bench/README.md for how budgets are calibrated and when to change them.
Requires Python 3.11+ (tomllib).
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import tomllib
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
MANIFEST = "plamenu-bench-run.json"

#: Response bodies are allowed to drift this far above their budget before it
#: counts as bloat. Deterministic output, so the band only needs to absorb
#: dataset jitter, not measurement noise.
BYTES_TOLERANCE = 1.15
#: MAD/median above this means a bimodal or noisy population — the median is
#: not summarising anything, so neither is the budget comparison.
MAX_DISPERSION = 0.15
#: Headroom below this is reported TIGHT: within noise of breaching, and a
#: budget at 0% headroom reading "ok" is how drift goes unnoticed.
TIGHT_HEADROOM = 0.25


@dataclass
class Measurement:
    median: float
    mad: float
    ci_low: float
    ci_high: float
    samples: int
    mtime: float
    queries: int | None = None
    body_bytes: int | None = None


@dataclass
class Row:
    name: str
    measurement: Measurement | None
    budget: dict | None
    failures: list[str] = field(default_factory=list)
    warnings: list[str] = field(default_factory=list)

    @property
    def known_bad(self) -> bool:
        return bool(self.budget) and self.budget.get("status") == "known-bad"


def git(*args: str) -> str:
    try:
        return subprocess.run(
            ["git", *args], cwd=REPO, capture_output=True, text=True, check=True
        ).stdout.strip()
    except (subprocess.CalledProcessError, FileNotFoundError):
        return ""


def machine_fingerprint() -> dict:
    """Enough to know whether two records are comparable at all."""
    fingerprint: dict[str, object] = {}
    try:
        with open("/proc/cpuinfo") as handle:
            models = [
                line.split(":", 1)[1].strip()
                for line in handle
                if line.startswith("model name")
            ]
        fingerprint["cpu"] = models[0] if models else "unknown"
        fingerprint["cores"] = len(models)
    except OSError:
        pass
    try:
        with open("/proc/meminfo") as handle:
            for line in handle:
                if line.startswith("MemTotal"):
                    fingerprint["memory_kb"] = int(line.split()[1])
                    break
    except OSError:
        pass
    fingerprint["platform"] = sys.platform
    return fingerprint


def read_manifest(criterion_dir: Path) -> dict | None:
    path = criterion_dir / MANIFEST
    if not path.exists():
        return None
    return json.loads(path.read_text())


def measured(criterion_dir: Path, manifest: dict | None) -> dict[str, Measurement]:
    """Benchmark full id -> what this run measured."""
    cases = (manifest or {}).get("cases", {})
    out: dict[str, Measurement] = {}
    for estimates_path in sorted(criterion_dir.glob("*/*/new/estimates.json")):
        benchmark_json = estimates_path.parent / "benchmark.json"
        if not benchmark_json.exists():
            continue
        full_id = json.loads(benchmark_json.read_text())["full_id"]
        estimates = json.loads(estimates_path.read_text())
        median = estimates["median"]
        sample_path = estimates_path.parent / "sample.json"
        samples = 0
        if sample_path.exists():
            samples = len(json.loads(sample_path.read_text()).get("times", []))
        extra = cases.get(full_id, {})
        out[full_id] = Measurement(
            median=median["point_estimate"] / 1e6,
            mad=estimates.get("median_abs_dev", {}).get("point_estimate", 0.0) / 1e6,
            ci_low=median["confidence_interval"]["lower_bound"] / 1e6,
            ci_high=median["confidence_interval"]["upper_bound"] / 1e6,
            samples=samples,
            mtime=estimates_path.stat().st_mtime,
            queries=extra.get("queries"),
            body_bytes=extra.get("bytes"),
        )
    return out


def grade(rows: list[Row], *, filtered: bool, strict: bool) -> None:
    medians = {r.name: r.measurement.median for r in rows if r.measurement is not None}
    for row in rows:
        budget, m = row.budget, row.measurement

        if budget is None:
            row.failures.append("no budget entry — add one to bench/budgets.toml")
            continue
        if m is None:
            if not filtered:
                row.failures.append("budgeted but not measured in this run")
            continue

        ceiling = budget.get("ms")
        if ceiling is None:
            row.failures.append("budget entry has no `ms`")
        elif m.median > ceiling:
            row.failures.append(f"median {m.median:.3f} ms over the {ceiling} ms budget")
        elif 1 - m.median / ceiling < TIGHT_HEADROOM:
            row.warnings.append(
                f"TIGHT: {(1 - m.median / ceiling) * 100:.0f}% headroom"
            )

        want_queries = budget.get("queries")
        if want_queries is not None and m.queries is not None:
            if m.queries > want_queries:
                row.failures.append(
                    f"{m.queries} SQL round trips, budget {want_queries} — a new "
                    "per-row lookup is invisible in a latency budget and costs an "
                    "order of magnitude more against a networked Postgres"
                )
            elif m.queries < want_queries:
                row.warnings.append(
                    f"{m.queries} SQL round trips, budget {want_queries} — ratchet "
                    "the budget down in this PR"
                )
        elif want_queries is None and m.queries is not None and strict:
            row.warnings.append(f"uncalibrated: measured {m.queries} queries")

        want_bytes = budget.get("bytes")
        if want_bytes is not None and m.body_bytes is not None:
            ceiling_bytes = want_bytes * BYTES_TOLERANCE
            if m.body_bytes > ceiling_bytes:
                row.failures.append(
                    f"{m.body_bytes} byte body over the {ceiling_bytes:.0f} byte ceiling"
                )
        elif want_bytes is None and m.body_bytes is not None and strict:
            row.warnings.append(f"uncalibrated: measured {m.body_bytes} bytes")

        # A ratio budget, for benchmarks whose absolute number is a property of
        # the machine but whose *slope* is a property of the code: routing one
        # status to 200 live streams against routing it to one, for instance,
        # is the per-recipient cost and it is the same everywhere.
        base_name = budget.get("ratio_of")
        want_ratio = budget.get("ratio")
        if base_name is not None and want_ratio is not None:
            base = medians.get(base_name)
            if base is None:
                if not filtered:
                    row.failures.append(
                        f"ratio budget names {base_name}, which this run did not measure"
                    )
            elif base <= 0:
                row.failures.append(f"ratio budget base {base_name} measured 0 ms")
            else:
                ratio = m.median / base
                if ratio > want_ratio:
                    row.failures.append(
                        f"{ratio:.1f}x {base_name}, budget {want_ratio}x — the marginal "
                        "cost this benchmark exists to bound has grown"
                    )
                elif 1 - ratio / want_ratio < TIGHT_HEADROOM:
                    row.warnings.append(
                        f"TIGHT: {ratio:.1f}x {base_name} against a {want_ratio}x budget"
                    )

        if m.median > 0 and m.mad / m.median > MAX_DISPERSION:
            row.failures.append(
                f"MAD/median {m.mad / m.median * 100:.0f}% over {MAX_DISPERSION:.0%} — "
                "the median is not describing one population"
            )


def format_ms(value: float) -> str:
    return f"{value:.3f} ms" if value < 1 else f"{value:.1f} ms"


def print_table(rows: list[Row], *, color: bool) -> None:
    def paint(text: str, code: str) -> str:
        return f"\x1b[{code}m{text}\x1b[0m" if color else text

    width = max((len(row.name) for row in rows), default=10) + 2
    header = f"{'benchmark':<{width}}{'median':>12}{'budget':>12}{'headroom':>10}"
    print(f"{header}{'queries':>9}{'bytes':>9}  status")
    for row in rows:
        m, budget = row.measurement, row.budget
        if m is None:
            ceiling = format_ms(budget["ms"]) if budget and "ms" in budget else "—"
            note = "not measured" if row.failures else "not measured (filtered)"
            print(f"{row.name:<{width}}{'—':>12}{ceiling:>12}{'':>10}{'':>9}{'':>9}  {note}")
            continue
        ceiling = budget.get("ms") if budget else None
        headroom = f"{(1 - m.median / ceiling) * 100:.0f}%" if ceiling else ""
        queries = str(m.queries) if m.queries is not None else "—"
        body = str(m.body_bytes) if m.body_bytes is not None else "—"
        if row.failures:
            status = paint("FAILED" if ceiling else "NO BUDGET", "31")
        elif row.known_bad:
            status = paint("known-bad", "35")
        elif row.warnings:
            status = paint(row.warnings[0], "33")
        else:
            status = paint("ok", "32")
        print(
            f"{row.name:<{width}}{format_ms(m.median):>12}"
            f"{format_ms(ceiling) if ceiling else '—':>12}{headroom:>10}"
            f"{queries:>9}{body:>9}  {status}"
        )


def record(rows: list[Row], manifest: dict, sha: str) -> Path:
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    path = REPO / "bench" / "results" / f"{sha[:12]}-{stamp}.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = {
        "git_sha": sha,
        "git_dirty": False,
        "git_describe": git("describe", "--tags", "--always", "--dirty"),
        "recorded_at": datetime.now(timezone.utc).isoformat(),
        "seed_version": manifest.get("seed_version"),
        "started_at": manifest.get("started_at"),
        "finished_at": manifest.get("finished_at"),
        "warm_up_secs": manifest.get("warm_up_secs"),
        "measurement_secs": manifest.get("measurement_secs"),
        "machine": machine_fingerprint(),
        "benchmarks": {
            row.name: {
                "median_ms": row.measurement.median,
                "mad_ms": row.measurement.mad,
                "ci_ms": [row.measurement.ci_low, row.measurement.ci_high],
                "samples": row.measurement.samples,
                "queries": row.measurement.queries,
                "bytes": row.measurement.body_bytes,
            }
            for row in rows
            if row.measurement is not None
        },
    }
    path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n")
    return path


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--criterion-dir", type=Path, default=REPO / "target" / "criterion")
    parser.add_argument("--budgets", type=Path, default=REPO / "bench" / "budgets.toml")
    parser.add_argument(
        "--filtered",
        action="store_true",
        help="the run measured a subset; do not fail on unmeasured budgets or stale siblings",
    )
    parser.add_argument(
        "--strict", action="store_true", help="also fail on TIGHT headroom and uncalibrated caps"
    )
    parser.add_argument(
        "--allow-stale",
        action="store_true",
        help="grade a run whose manifest sha is not HEAD (a rebase, a dirty tree)",
    )
    parser.add_argument("--json", action="store_true", help="machine-readable output only")
    parser.add_argument("--no-record", action="store_true", help="do not write bench/results/")
    args = parser.parse_args()

    if not args.criterion_dir.is_dir():
        print(f"no criterion output at {args.criterion_dir} — run `cargo bench -p plamenu` first")
        return 2

    budgets: dict[str, dict] = tomllib.loads(args.budgets.read_text())["budgets"]
    manifest = read_manifest(args.criterion_dir)
    if manifest is None:
        print(
            f"no run manifest at {args.criterion_dir / MANIFEST} — the criterion output "
            "predates this harness, so there is no way to tell which of it is fresh. "
            "Re-run `cargo bench -p plamenu`."
        )
        return 2

    measurements = measured(args.criterion_dir, manifest)
    started = datetime.fromisoformat(manifest["started_at"]).timestamp()

    run_failures: list[str] = []
    head = git("rev-parse", "HEAD")
    sha = manifest.get("git_sha") or head
    if head and manifest.get("git_sha") and manifest["git_sha"] != head and not args.allow_stale:
        run_failures.append(
            f"manifest was measured at {manifest['git_sha'][:12]}, HEAD is {head[:12]} — "
            "a green result here is not evidence about this tree (--allow-stale to override)"
        )
    # A dirty measurement names a revision it does not describe. Harmless to
    # grade — you asked about the tree in front of you — but it must never be
    # filed as that revision's performance record.
    dirty = bool(manifest.get("git_dirty"))

    # Anything criterion left on disk from an earlier run is not this run's
    # evidence, so it is never graded — that is the whole fix for a gate that
    # was reporting four run epochs as one green table. A leftover that still
    # has a budget then surfaces as "budgeted but not measured"; a leftover
    # with no budget is the residue of a renamed or deleted benchmark and is
    # ignored (criterion never removes a benchmark directory).
    graded = {name: m for name, m in measurements.items() if m.mtime >= started}
    leftovers = sorted(set(measurements) - set(graded))

    rows = [
        Row(name=name, measurement=graded.get(name), budget=budgets.get(name))
        for name in sorted({*budgets, *graded})
    ]
    grade(rows, filtered=args.filtered, strict=args.strict)

    failed = [r for r in rows if r.failures and not r.known_bad]
    warned = [r for r in rows if r.warnings and not r.failures]
    if args.strict:
        failed += [r for r in warned if not r.known_bad]

    if args.json:
        print(
            json.dumps(
                {
                    "git_sha": sha,
                    "seed_version": manifest.get("seed_version"),
                    "run_failures": run_failures,
                    "benchmarks": {
                        r.name: {
                            "median_ms": r.measurement.median if r.measurement else None,
                            "queries": r.measurement.queries if r.measurement else None,
                            "bytes": r.measurement.body_bytes if r.measurement else None,
                            "failures": r.failures,
                            "warnings": r.warnings,
                            "known_bad": r.known_bad,
                        }
                        for r in rows
                    },
                },
                indent=2,
            )
        )
        return 1 if failed or run_failures else 0

    print_table(rows, color=sys.stdout.isatty())

    for row in rows:
        for problem in row.failures:
            marker = "known-bad" if row.known_bad else "FAIL"
            print(f"\n  {marker} {row.name}: {problem}")
        if not row.failures:
            for warning in row.warnings[1:]:
                print(f"\n  warn {row.name}: {warning}")

    if leftovers:
        print(
            f"\n{len(leftovers)} result(s) on disk predate this run and were not graded: "
            + ", ".join(leftovers[:8])
            + ("…" if len(leftovers) > 8 else "")
        )

    known_bad = [r for r in rows if r.known_bad]
    if known_bad:
        print(
            f"\n{len(known_bad)} known-bad benchmark(s), excluded from the result: "
            + ", ".join(r.name for r in known_bad)
        )
    for problem in run_failures:
        print(f"\nRUN {problem}")

    if failed or run_failures:
        print(f"\n{len(failed)} benchmark(s) failed, {len(run_failures)} run-level problem(s)")
        return 1

    print(f"\nall {len([r for r in rows if r.measurement])} measured benchmarks within budget")
    if args.no_record or args.filtered:
        return 0
    if dirty:
        print(
            "\nnot recorded: the working tree was dirty during the run, so these "
            f"numbers do not describe {sha[:12]}. Commit first, then re-run — the "
            "record is the release baseline and has to name a revision it is true of."
        )
        return 0
    print(f"recorded {record(rows, manifest, sha).relative_to(REPO)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
