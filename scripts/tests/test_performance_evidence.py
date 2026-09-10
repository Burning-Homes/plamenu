"""Check release evidence against small real Git histories, without building."""

import json
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


class ReleasePreflight(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        for directory in (
            "scripts",
            "crates/server/benches/hot_paths",
            "bench/results",
        ):
            (self.root / directory).mkdir(parents=True)
        source = Path(__file__).resolve().parents[2]
        for name in (
            "scripts/check-performance-evidence.py",
            "bench/bench_budgets.py",
            "bench/budgets.toml",
            "bench/release-policy.toml",
            "crates/server/benches/hot_paths/seed.rs",
        ):
            shutil.copy(source / name, self.root / name)
        self.records = {
            "hot": json.loads(
                (
                    source / "bench/results/79f461808e2c-20260905T153248Z.json"
                ).read_text()
            ),
            "contention": json.loads(
                (
                    source
                    / "bench/results/contention-79f461808e2c-20260905T153736Z.json"
                ).read_text()
            ),
        }
        # This temporary repository tests evidence validation, not the current
        # server's performance. Give its historical measurements a matching
        # test calibration so real budget changes cannot break the valid case
        # or make every rejection test pass for an unrelated reason.
        calibration = ["# Synthetic calibration for the test repository.", "[budgets]"]
        for name, measurement in self.records["hot"]["benchmarks"].items():
            fields = [f"ms = {measurement['median_ms'] * 2}"]
            for metric in ("queries", "bytes"):
                if measurement.get(metric) is not None:
                    fields.append(f"{metric} = {measurement[metric]}")
            calibration.append(f"{json.dumps(name)} = {{ {', '.join(fields)} }}")
        (self.root / "bench/budgets.toml").write_text("\n".join(calibration) + "\n")
        public = self.root / "scripts/check-public-tree.sh"
        public.write_text("#!/bin/sh\nexit 0\n")
        public.chmod(0o755)
        (self.root / "Cargo.toml").write_text(
            '[workspace.package]\nversion = "1.2.3"\n'
        )
        (self.root / "SECURITY.md").write_text(
            "Private reporting contact configured.\n"
        )
        (self.root / "crates/lib.rs").write_text("initial code\n")
        self.git("init", "-q")
        self.git("config", "user.email", "test@example.com")
        self.git("config", "user.name", "Test")
        self.commit()
        self.measured = self.git("rev-parse", "HEAD").strip()

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.root, text=True)

    def commit(self):
        self.git("add", ".")
        self.git("commit", "-qm", "fixture")

    def record(self, dirty=False, mutate=None, omit=None, commit=True):
        import copy

        records = copy.deepcopy(self.records)
        for kind, record in records.items():
            record.update(git_sha=self.measured, git_dirty=dirty)
            if mutate:
                mutate(kind, record)
            if kind != omit:
                (self.root / f"bench/results/{kind}.json").write_text(
                    json.dumps(record) + "\n"
                )
        if commit:
            self.commit()

    def check(self):
        return subprocess.run(
            ["python3", "scripts/check-performance-evidence.py"],
            cwd=self.root,
            text=True,
            capture_output=True,
            check=False,
        )

    def test_accepts_clean_ancestor_with_unchanged_code(self):
        self.record()
        result = self.check()
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_rejects_missing_record(self):
        self.assertNotEqual(self.check().returncode, 0)

    def test_rejects_dirty_measurement(self):
        self.record(dirty=True)
        self.assertNotEqual(self.check().returncode, 0)

    def test_rejects_code_changed_after_measurement(self):
        self.record()
        (self.root / "crates/lib.rs").write_text("changed code\n")
        self.commit()
        self.assertNotEqual(self.check().returncode, 0)

    def test_rejects_untracked_records(self):
        self.record(commit=False)
        self.assertNotEqual(self.check().returncode, 0)

    def test_rejects_missing_contention(self):
        self.record(omit="contention")
        self.assertNotEqual(self.check().returncode, 0)

    def test_rejects_changed_budgets(self):
        self.record()
        with (self.root / "bench/budgets.toml").open("a") as handle:
            handle.write("\n# changed calibration\n")
        self.commit()
        self.assertNotEqual(self.check().returncode, 0)

    def test_rejects_invalid_hot_path_evidence(self):
        mutations = {
            "partial": lambda r: r["benchmarks"].pop("ap/actor"),
            "NaN": lambda r: r["benchmarks"]["ap/actor"].update(median_ms=float("nan")),
            "missing queries": lambda r: r["benchmarks"]["ap/actor"].pop("queries"),
            "few samples": lambda r: r["benchmarks"]["ap/actor"].update(samples=10),
            "shortened": lambda r: r.update(measurement_secs=0.1),
            "noise": lambda r: r["benchmarks"]["ap/actor"].update(mad_ms=1.0),
            "wrong machine": lambda r: r["machine"].update(cpu="another CPU"),
            "wrong seed": lambda r: r.update(seed_version="obsolete"),
        }
        for name, mutation in mutations.items():
            with self.subTest(name=name):
                self.record(
                    mutate=lambda kind, r, mutation=mutation: (
                        mutation(r) if kind == "hot" else None
                    )
                )
                self.assertNotEqual(self.check().returncode, 0)

    def test_rejects_invalid_contention_evidence(self):
        mutations = {
            "fewer workers": lambda r: r.update(workers=1),
            "fewer requests": lambda r: r["scenarios"]["credential burst"].update(
                requests=1
            ),
            "over budget": lambda r: r["scenarios"]["credential burst"].update(
                ratio=10
            ),
            "failed requests": lambda r: r["scenarios"]["credential burst"].update(
                failures=1
            ),
            "failed mixed writes": lambda r: r["scenarios"]["mixed read/write"].update(
                failures=6
            ),
            "failed invariant": lambda r: r["scenarios"]["fan-out contention"].update(
                invariant_ok=False
            ),
            "missing scenario": lambda r: r["scenarios"].pop("mixed read/write"),
        }
        for name, mutation in mutations.items():
            with self.subTest(name=name):
                self.record(
                    mutate=lambda kind, r, mutation=mutation: (
                        mutation(r) if kind == "contention" else None
                    )
                )
                self.assertNotEqual(self.check().returncode, 0)
