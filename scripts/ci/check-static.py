#!/usr/bin/env python3
# ruff: noqa: N999 -- executable CLI names use hyphens; these are not importable modules.
"""Syntax checks over non-ignored source files; no Python modules are imported."""

import ast
import subprocess
import tomllib
from pathlib import Path

import yaml

paths = (
    subprocess.check_output(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"]
    )
    .decode()
    .split("\0")
)
counts = {"python": 0, "toml": 0, "yaml": 0}
for name in sorted(set(filter(None, paths))):
    path = Path(name)
    # Pending moves/deletions still appear in the index until staged. New files
    # above must be checked too, so local validation matches the eventual commit.
    if not path.exists():
        continue
    if path.suffix == ".py":
        ast.parse(path.read_bytes(), filename=name)
        counts["python"] += 1
    elif path.suffix == ".toml":
        tomllib.loads(path.read_text())
        counts["toml"] += 1
    elif path.suffix in {".yml", ".yaml"}:
        list(yaml.safe_load_all(path.read_text()))
        counts["yaml"] += 1
print(f"PASS source syntax: {counts}")
