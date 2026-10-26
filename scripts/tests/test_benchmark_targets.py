"""Bench-only selection must not disable the normal executable or CLI tests."""

import json
import subprocess
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


class BenchmarkTargetsTests(unittest.TestCase):
    def test_default_build_keeps_cli_and_benchmark_library_does_not_require_it(self):
        metadata = json.loads(
            subprocess.check_output(
                [
                    "cargo",
                    "metadata",
                    "--locked",
                    "--offline",
                    "--no-deps",
                    "--format-version",
                    "1",
                ],
                cwd=ROOT,
                text=True,
            )
        )
        packages = {package["name"]: package for package in metadata["packages"]}
        server = packages["plamenu"]
        self.assertEqual(server["features"], {"default": ["cli"], "cli": []})
        binary = next(t for t in server["targets"] if t["kind"] == ["bin"])
        self.assertEqual(binary["required-features"], ["cli"])
        self.assertTrue(binary["test"])
        benches = [t for t in server["targets"] if t["kind"] == ["bench"]]
        self.assertEqual({t["name"] for t in benches}, {"hot_paths", "contention"})
        self.assertTrue(all(not t.get("required-features") for t in benches))
        dependency = next(
            d
            for d in packages["plamenu-test-support"]["dependencies"]
            if d["name"] == "plamenu"
        )
        self.assertFalse(dependency["uses_default_features"])
        self.assertEqual(dependency["features"], [])


if __name__ == "__main__":
    unittest.main()
