"""Check fresh query metadata without depending on a populated developer database."""

import copy
import json
import os
import re
import shutil
from pathlib import Path

from .core import ReleaseError, write_json


def effective_metadata(record):
    result = copy.deepcopy(record)
    describe = result["describe"]
    if len(describe["columns"]) != len(describe["nullable"]):
        raise ReleaseError("Malformed SQLx column/nullability metadata")
    for index, column in enumerate(describe["columns"]):
        # SQLx's ColumnOverride parser gives !/? precedence over inferred
        # nullability, including forms such as `value?: MyType`.
        override = re.fullmatch(r"[^!?:]+([!?])(?:\s*:\s*.+)?", column["name"])
        if override:
            describe["nullable"][index] = override[1] == "?"
    return result


def compare(expected, generated):
    old = {p.name: p for p in expected.glob("query-*.json")}
    new = {p.name: p for p in generated.glob("query-*.json")}
    if not new or old.keys() != new.keys():
        raise ReleaseError(
            "SQLx query set changed or generation is incomplete; regenerate .sqlx for the current source"
        )
    overridden = []
    for name in sorted(old):
        before, after = (
            json.loads(old[name].read_text()),
            json.loads(new[name].read_text()),
        )
        if effective_metadata(before) != effective_metadata(after):
            raise ReleaseError(
                "SQLx query metadata changed: " + name + "; inspect " + str(generated)
            )
        if before != after:
            overridden.append(name)
    return {"queries": len(new), "explicit_nullability_overrides": overridden}


def check(runner, folder, url):
    generated = folder / "sqlx-generated"
    if generated.exists():
        shutil.rmtree(generated)
    generated.mkdir(parents=True)
    metadata = json.loads(
        runner.run(
            "cargo",
            "metadata",
            "--locked",
            "--no-deps",
            "--format-version",
            "1",
            capture=True,
        )
    )
    # Like cargo-sqlx, refresh workspace target mtimes so cached compilation
    # cannot make an empty/partial query collection look like a successful check.
    # File contents and the committed .sqlx cache remain untouched.
    for package in metadata["packages"]:
        if package["id"] in metadata["workspace_members"]:
            for target in package["targets"]:
                path = Path(target["src_path"])
                path.relative_to(runner.source)
                os.utime(path, None)
    runner.run(
        "cargo",
        "check",
        "--locked",
        "--workspace",
        "--all-targets",
        env={
            "DATABASE_URL": url,
            "SQLX_OFFLINE": "false",
            "SQLX_OFFLINE_DIR": str(generated.resolve()),
        },
    )
    result = compare(runner.source / ".sqlx", generated)
    write_json(folder / "sqlx.json", result)
    shutil.rmtree(generated)
