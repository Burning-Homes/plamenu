"""Source identity, logged execution, and resumable local release stages."""

import hashlib
import json
import os
import shlex
import subprocess
import time
from pathlib import Path


class ReleaseError(Exception):
    pass


def digest(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def fingerprint(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True).encode()).hexdigest()


def write_json(path, value):
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, sort_keys=True, indent=2) + "\n")
    temporary.replace(path)


def git(root, *args):
    return subprocess.check_output(["git", *args], cwd=root, text=True).strip()


def clean(root):
    if git(root, "status", "--porcelain", "--untracked-files=normal"):
        raise ReleaseError("Commit or remove pending source changes before releasing.")


def environment():
    env = os.environ.copy()
    for name in (
        "REPO_TOKEN",
        "GITHUB_TOKEN",
        "FORGEJO_TOKEN",
        "ACTIONS_RUNTIME_TOKEN",
        "PACKAGE_PUBLISH_TOKEN",
        "COSIGN_PASSWORD",
        "RELEASE_COSIGN_KEY",
        "RELEASE_COSIGN_PASSWORD",
        "DATABASE_URL",
        "PLAMENU_CONFIG",
        "PLAMENU_DEV_ENV",
        "CARGO_TARGET_DIR",
        "RUSTFLAGS",
        "RUSTC_WRAPPER",
        "RUSTUP_TOOLCHAIN",
    ):
        env.pop(name, None)
    for name in list(env):
        if name.startswith(
            ("PLAMENU_", "PG", "CARGO_PROFILE_", "CARGO_ENCODED_RUSTFLAGS", "SQLX_")
        ):
            env.pop(name)
    env["SQLX_OFFLINE"] = "true"
    return env


class Runner:
    def __init__(self, source, log, extra=None):
        self.source, self.log = Path(source), Path(log)
        self.env = environment() | (extra or {})

    def run(self, *args, capture=False, env=None, data=None):
        args = [str(a) for a in args]
        self.log.parent.mkdir(parents=True, exist_ok=True)
        with self.log.open("ab") as log:
            log.write(("$ " + shlex.join(args) + "\n").encode())
            log.flush()
            result = subprocess.run(
                args,
                cwd=self.source,
                env=self.env | (env or {}),
                input=data,
                stdout=subprocess.PIPE if capture else log,
                stderr=log,
                check=False,
            )
            if capture:
                log.write(result.stdout)
        if result.returncode:
            raise ReleaseError(
                f"{args[0]} failed ({result.returncode}); see {self.log}"
            )
        return result.stdout.decode().strip() if capture else ""


def files(folder):
    result = {}
    for path in sorted(folder.rglob("*")):
        if path.is_symlink():
            raise ReleaseError("Unexpected symlink in release output: " + str(path))
        if path.is_file() and path != folder / "stage.json":
            result[str(path.relative_to(folder))] = digest(path)
    return result


def reusable(folder, key, max_age=None):
    try:
        record = json.loads((folder / "stage.json").read_text())
        return (
            record["status"] == "passed"
            and record["key"] == key
            and (max_age is None or time.time() - record["finished"] < max_age)
            and record["files"] == files(folder)
        )
    except (OSError, ValueError, KeyError, ReleaseError):
        return False


def stage(
    source, root, name, identity, operation, *, rerun=False, max_age=None, env=None
):
    import shutil

    folder = root / name
    key = fingerprint({"identity": identity, "stage": name})
    if not rerun and reusable(folder, key, max_age):
        print(f"{name}: reusing verified results", flush=True)
        return json.loads((folder / "stage.json").read_text())
    if folder.exists():
        shutil.rmtree(folder)
    folder.mkdir(parents=True)
    record = {"key": key, "status": "running", "started": time.time()}
    write_json(folder / "stage.json", record)
    print(f"{name}: running (log: {folder / 'run.log'})", flush=True)
    try:
        operation(
            Runner(
                source,
                folder / "run.log",
                {"CARGO_BUILD_JOBS": str(identity["jobs"])} | (env or {}),
            ),
            folder,
        )
    except BaseException:
        record.update(status="failed", finished=time.time())
        write_json(folder / "stage.json", record)
        raise
    record.update(status="passed", finished=time.time(), files=files(folder))
    write_json(folder / "stage.json", record)
    print(f"{name}: passed", flush=True)
    return record
