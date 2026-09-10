#!/usr/bin/env python3
"""Syntax checks over tracked files only; no Python modules are imported."""

import ast
import subprocess
from pathlib import Path

import tomllib
import yaml

paths = subprocess.check_output(["git", "ls-files", "-z"]).decode().split("\0")
counts = {"python": 0, "toml": 0, "yaml": 0}
for name in filter(None, paths):
    path = Path(name)
    if path.suffix == ".py":
        ast.parse(path.read_bytes(), filename=name)
        counts["python"] += 1
    elif path.suffix == ".toml":
        tomllib.loads(path.read_text())
        counts["toml"] += 1
    elif path.suffix in {".yml", ".yaml"}:
        list(yaml.safe_load_all(path.read_text()))
        counts["yaml"] += 1
print(f"PASS tracked-file syntax: {counts}")
