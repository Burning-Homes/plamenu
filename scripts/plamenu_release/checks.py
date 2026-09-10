"""Portable baseline checks; developer fleet and calibrated timings are opt-in."""

import contextlib
import json
import os
import platform
import secrets
import shutil
import subprocess
import time

import tomllib

from .build import verify_build
from .core import ReleaseError, write_json


def static(runner, folder):
    for command in (
        ("cargo", "fmt", "--all", "--", "--check"),
        (
            "cargo",
            "clippy",
            "--locked",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ),
        ("python3", "scripts/ci/check-static.py"),
        ("python3", "-m", "unittest", "discover", "-s", "scripts/tests"),
        ("python3", "-m", "pytest", "bench/test_bench_budgets.py", "-q"),
        (
            "ruff",
            "check",
            "e2e",
            "scripts/plamenu_release",
            "scripts/release.py",
            "scripts/release-worker.py",
            "scripts/docs.py",
        ),
        (
            "ruff",
            "format",
            "--check",
            "e2e",
            "scripts/plamenu_release",
            "scripts/release.py",
            "scripts/release-worker.py",
            "scripts/docs.py",
        ),
        ("cargo", "deny", "--locked", "fetch", "all"),
        ("cargo", "deny", "--locked", "check", "--deny", "warnings"),
        ("bash", "scripts/check-docs.sh"),
        ("sh", "scripts/check-public-tree.sh"),
    ):
        runner.run(*command)
    shell_files = {"dev", "e2e/peers/dev", "e2e/peers/provision.sh"}
    for pattern in (
        "scripts/*.sh",
        "scripts/ci/*.sh",
        "docs/examples/*.sh",
        "e2e/peers/*/entrypoint.sh",
        "e2e/peers/*/entrypoint-seed.sh",
        "e2e/peers/*/docker-entrypoint.sh",
    ):
        shell_files.update(
            str(p.relative_to(runner.source)) for p in runner.source.glob(pattern)
        )
    runner.run("shellcheck", *sorted(shell_files))


@contextlib.contextmanager
def database(runner):
    name = "plamenu-release-db-" + secrets.token_hex(6)
    password = secrets.token_hex(24)
    # Local loopback only. No standing development database or fixed port is used.
    try:
        runner.run(
            "docker",
            "run",
            "--detach",
            "--name",
            name,
            "--publish",
            "127.0.0.1::5432",
            "--env",
            "POSTGRES_PASSWORD",
            "--env",
            "POSTGRES_DB=plamenu_release",
            "postgres:18-alpine@sha256:9a8afca54e7861fd90fab5fdf4c42477a6b1cb7d293595148e674e0a3181de15",
            "postgres",
            "-c",
            "max_connections=300",
            "-c",
            "fsync=off",
            "-c",
            "synchronous_commit=off",
            "-c",
            "full_page_writes=off",
            env={"POSTGRES_PASSWORD": password},
        )
        for _ in range(60):
            ready = subprocess.run(
                ["docker", "exec", name, "pg_isready", "-U", "postgres"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
            )
            if ready.returncode == 0:
                break
            time.sleep(1)
        else:
            raise ReleaseError("Disposable PostgreSQL did not become ready")
        mapping = runner.run("docker", "port", name, "5432/tcp", capture=True)
        port = int(mapping.splitlines()[0].rsplit(":", 1)[1])
        yield f"postgres://postgres:{password}@127.0.0.1:{port}/plamenu_release"
    finally:
        subprocess.run(
            ["docker", "rm", "--force", "--volumes", name],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )


def application(runner, folder):
    with database(runner) as url:
        # Keep template1 empty: migration tests deliberately start at older schemas.
        runner.run(
            "cargo",
            "sqlx",
            "migrate",
            "run",
            "--source",
            "crates/db/migrations",
            env={"DATABASE_URL": url},
        )
        runner.run(
            "cargo",
            "nextest",
            "run",
            "--no-fail-fast",
            "--locked",
            "--workspace",
            "--test-threads",
            str(min(os.cpu_count() or 2, 8)),
            env={"DATABASE_URL": url},
        )
        from . import sqlx

        sqlx.check(runner, folder, url)


def performance_machine(source):
    from pathlib import Path

    policy = tomllib.loads((source / "bench/release-policy.toml").read_text())[
        "machine"
    ]
    cpu = next(
        (
            line.split(":", 1)[1].strip()
            for line in Path("/proc/cpuinfo").read_text().splitlines()
            if line.startswith("model name")
        ),
        "unknown",
    )
    actual = {
        "cpu": cpu,
        "cores": os.cpu_count(),
        "platform": platform.system().lower(),
    }
    if actual != policy:
        raise ReleaseError(
            "Calibrated timing benchmarks require the machine in bench/release-policy.toml; omit --with-performance here."
        )


def _performance(runner, folder):
    performance_machine(runner.source)
    previous = set((runner.source / "bench/results").glob("*.json"))
    with database(runner) as url:
        for args in (
            ("cargo", "bench", "--locked", "-p", "plamenu", "--bench", "hot_paths"),
            ("python3", "bench/bench_budgets.py", "--strict"),
            ("cargo", "bench", "--locked", "-p", "plamenu", "--bench", "contention"),
        ):
            runner.run(*args, env={"DATABASE_URL": url})
    reports = set((runner.source / "bench/results").glob("*.json")) - previous
    if len(reports) < 2:
        raise ReleaseError("Performance checks did not produce both fresh reports")
    # Validate the complete workloads and calibration, without requiring reports
    # to be committed or manually carried between commands.
    import importlib.util
    import re

    spec = importlib.util.spec_from_file_location(
        "performance_evidence", runner.source / "scripts/check-performance-evidence.py"
    )
    validator = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(validator)
    policy = tomllib.loads((runner.source / "bench/release-policy.toml").read_text())
    budgets = tomllib.loads((runner.source / "bench/budgets.toml").read_text())[
        "budgets"
    ]
    seed = (runner.source / "crates/server/benches/hot_paths/seed.rs").read_text()
    seed_version = re.search(r'pub const SEED_VERSION: &str = "([^"]+)"', seed)[1]
    kinds = set()
    try:
        for path in reports:
            record = json.loads(path.read_text())
            validator.valid_source(record, policy, seed_version)
            kind = "contention" if record.get("kind") == "contention" else "hot_paths"
            if kind == "contention":
                validator.validate_contention(record, policy)
            else:
                validator.validate_hot(record, budgets, policy)
            kinds.add(kind)
            shutil.copy2(path, folder / path.name)
        if kinds != {"hot_paths", "contention"}:
            raise ReleaseError("Missing a complete performance workload")
    finally:
        for path in reports:
            path.unlink()


def performance(runner, folder):
    previous = set((runner.source / "bench/results").glob("*.json"))
    try:
        _performance(runner, folder)
    finally:
        for path in set((runner.source / "bench/results").glob("*.json")) - previous:
            path.unlink()


def e2e(runner, folder, origin):
    # The existing harness owns its developer-specific URLs and running peers.
    # Keep its local configuration out of the source archive and release assets.
    before = set((runner.source / "target/e2e").glob("release-*.json"))
    runner.run(
        "./dev",
        "e2e-full",
        env={
            "PLAMENU_DEV_ENV": str(origin / ".env"),
            "PLAMENU_PEER_ROOT": str(origin / "e2e/peers"),
            "PLAMENU_PEER_DEV": str(origin / "e2e/peers/dev"),
        },
    )
    reports = set((runner.source / "target/e2e").glob("release-*.json")) - before
    if not reports:
        raise ReleaseError("The developer E2E suite produced no release report")
    for path in reports:
        shutil.copy2(path, folder / path.name)


def artifacts(runner, folder, build_folder, identity):
    record = verify_build(build_folder, identity)
    arch = identity["arch"]
    runner.run("docker", "load", "--input", build_folder / "image.tar.gz")
    image = record["image"]
    actual = json.loads(runner.run("docker", "image", "inspect", image, capture=True))[
        0
    ]
    expected = json.loads((build_folder / "image.json").read_text())
    if any(
        actual[k] != expected[k] for k in ("Architecture", "Os", "Config", "RootFS")
    ):
        raise ReleaseError("Loaded image differs from the built image")
    name = "plamenu-release-inspect-" + secrets.token_hex(6)
    try:
        runner.run(
            "docker", "create", "--platform", "linux/" + arch, "--name", name, image
        )
        from .core import digest

        binary = folder / "image-plamenu"
        runner.run("docker", "cp", name + ":/usr/local/bin/plamenu", binary)
        if digest(binary) != record["binaries"]["plamenu"]["sha256"]:
            raise ReleaseError("Runtime executable differs from the packaged binary")
        if "INTERP" in runner.run("readelf", "-hl", binary, capture=True):
            raise ReleaseError("Packaged executable is not static")
        binary.unlink()
    finally:
        subprocess.run(
            ["docker", "rm", "--force", "--volumes", name],
            stdout=subprocess.DEVNULL,
            check=False,
        )
    # The restore drill includes clean installation and exercises these same bytes.
    runner.run(
        "bash",
        "scripts/rehearse-deployment.sh",
        "restore",
        env={
            "PLAMENU_DRILL_SKIP_BUILD": "true",
            "PLAMENU_DRILL_IMAGE": image,
            "PLAMENU_DRILL_ARCH": arch,
        },
    )
    write_json(
        folder / "tested-image.json",
        {
            "image_id": expected["Id"],
            "binary_sha256": record["binaries"]["plamenu"]["sha256"],
            "architecture": arch,
            "test_host": platform.machine(),
        },
    )
