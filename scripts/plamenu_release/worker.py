"""Optional worker dispatch and automatic result transport for build/static stages."""

import json
import platform
import secrets
import shutil
import sys
import tarfile
import tempfile
import time
import urllib.error
import zipfile
from pathlib import Path, PurePosixPath

from .core import ReleaseError, files, fingerprint, git, stage, write_json
from .forge import Forge


def unpack_bundle(archive, destination):
    with tarfile.open(archive) as tar:
        members = tar.getmembers()
        names = [m.name for m in members]
        if (
            len(names) != len(set(names))
            or sum(m.size for m in members) > 600 * 1024**2
        ):
            raise ReleaseError(
                "Worker bundle has duplicate members or exceeds the size limit"
            )
        for member in members:
            path = PurePosixPath(member.name)
            if (
                path.is_absolute()
                or ".." in path.parts
                or not (member.isfile() or member.isdir())
            ):
                raise ReleaseError("Unsafe worker bundle member")
        tar.extractall(destination, filter="data")


def execute_worker(source, output, identity, phase):
    from . import build, checks

    if (
        phase not in ("build", "static")
        or git(source, "rev-parse", "HEAD") != identity["source"]
    ):
        raise ReleaseError("Worker checkout must match the requested source and phase")
    if (
        phase == "build"
        and platform.machine()
        != {"amd64": "x86_64", "arm64": "aarch64"}[identity["arch"]]
    ):
        raise ReleaseError(
            "The build worker must run natively on the requested architecture"
        )
    operation = (
        checks.static
        if phase == "static"
        else lambda r, p: build.checked_build(r, p, identity)
    )
    try:
        stage(source, output, phase, identity, operation)
    except BaseException:
        log = output / phase / "run.log"
        if log.exists():
            print("\nWorker failure (last 100 log lines):", file=sys.stderr, flush=True)
            with log.open("rb") as stream:
                stream.seek(max(0, log.stat().st_size - 64 * 1024))
                print(
                    "\n".join(
                        stream.read().decode(errors="replace").splitlines()[-100:]
                    ),
                    file=sys.stderr,
                    flush=True,
                )
        raise
    finally:
        with tarfile.open(output / "bundle.tar.gz", "w:gz") as archive:
            for path in sorted((output / phase).rglob("*")):
                if path.is_file():
                    archive.add(path, arcname=str(path.relative_to(output / phase)))


def remote(source, root, folder, identity, phase, config, ref):
    if not config.get("repository") or not config.get("token_file"):
        raise ReleaseError(
            "Worker execution needs repository and token_file in the release configuration"
        )
    forge = Forge(
        config["repository"],
        Path(config["token_file"]).expanduser().read_text().strip(),
        config.get("allow_public", False),
    )
    forge.repository_check()
    if not ref:
        raise ReleaseError(
            "Optional worker execution needs a pushed branch or tag at this commit"
        )
    key = fingerprint({"identity": identity, "stage": phase})
    path = root / (
        "worker-"
        + phase
        + ("-" + identity["arch"] if phase == "build" else "")
        + ".json"
    )
    saved = json.loads(path.read_text()) if path.exists() else {}
    if (
        saved.get("key") != key
        or saved.get("repository") != forge.repository
        or saved.get("failed")
        or saved.get("complete")
    ):
        request = secrets.token_hex(16)
        # A unique request identifies this dispatch even when multiple releases run concurrently.
        saved = {
            "key": key,
            "request": request,
            "repository": forge.repository,
            "submitted": time.time(),
        }
        write_json(path, saved)
        try:
            forge.api(
                "/actions/workflows/release-worker.yml/dispatches",
                "POST",
                {
                    "ref": ref,
                    "inputs": {
                        "request": request,
                        "phase": phase,
                        "identity": json.dumps(identity),
                        "arch": identity.get("arch", "amd64"),
                    },
                },
            )
        except urllib.error.HTTPError as error:
            if 400 <= error.code < 500:
                saved["failed"] = True
                write_json(path, saved)
            raise
    request = saved["request"]
    print(f"{phase}: waiting for optional worker request {request[:8]}", flush=True)
    deadline = time.monotonic() + 100 * 60
    run = None
    while time.monotonic() < deadline:
        if saved.get("run_id"):
            run = forge.api(f"/actions/runs/{saved['run_id']}")
        else:
            page = 1
            while True:
                listing = forge.api(
                    f"/actions/runs?workflow_id=release-worker.yml&limit=50&page={page}"
                )["workflow_runs"]
                for item in listing:
                    payload = json.loads(item.get("event_payload") or "{}")
                    if payload.get("inputs", {}).get("request") == request:
                        run = item
                        break
                if run is not None or len(listing) < 50:
                    break
                page += 1
            if run is not None:
                saved["run_id"] = run["id"]
                write_json(path, saved)
                print("Worker run: " + run["html_url"], flush=True)
        if run is not None:
            if (
                run["commit_sha"] != identity["source"]
                or run["workflow_id"] != "release-worker.yml"
                or run.get("is_fork_pull_request")
            ):
                saved["failed"] = True
                write_json(path, saved)
                raise ReleaseError(
                    "Worker ran a different source revision; push the selected source and retry"
                )
            if run["status"] == "success":
                break
            if run["status"] in ("failure", "cancelled", "skipped", "blocked"):
                saved["failed"] = True
                write_json(path, saved)
                log = folder / "worker-failure.log"
                try:
                    with log.open("wb") as output:
                        for job in forge.api(f"/actions/runs/{run['id']}/jobs"):
                            if job["status"] == "failure":
                                with forge.request(
                                    forge.base + f"/actions/jobs/{job['id']}/logs"
                                ) as response:
                                    output.write(response.read(2 * 1024**2))
                except OSError as error:
                    print(
                        "Could not retrieve the worker failure log: " + str(error),
                        flush=True,
                    )
                raise ReleaseError(
                    "Worker did not succeed: "
                    + run["html_url"]
                    + f"; diagnostics: {log}; rerun locally or retry the worker"
                )
        time.sleep(5)
    else:
        raise ReleaseError(
            "Worker is still pending; rerun this command to reconnect to the same request"
        )
    artifacts = forge.api(f"/actions/runs/{run['id']}/artifacts")
    matches = [
        a
        for a in artifacts
        if a["name"] == "release-" + request and a["run_id"] == run["id"]
    ]
    if len(matches) != 1 or matches[0].get("expired"):
        saved["failed"] = True
        write_json(path, saved)
        raise ReleaseError(
            "Worker result is missing or expired; rerun this stage locally"
        )
    with tempfile.TemporaryDirectory(prefix="plamenu-worker-") as temp:
        tmp = Path(temp)
        forge.download(
            matches[0]["archive_download_url"], tmp / "result.zip", 600 * 1024**2
        )
        try:
            with zipfile.ZipFile(tmp / "result.zip") as archive:
                if (
                    archive.namelist() != ["bundle.tar.gz"]
                    or archive.infolist()[0].file_size > 600 * 1024**2
                ):
                    raise ReleaseError("Unexpected worker result archive")
                with (
                    archive.open("bundle.tar.gz") as src,
                    (tmp / "bundle.tar.gz").open("wb") as dst,
                ):
                    shutil.copyfileobj(src, dst, 4 * 1024**2)
            extracted = tmp / "extracted"
            extracted.mkdir()
            unpack_bundle(tmp / "bundle.tar.gz", extracted)
            record = json.loads((extracted / "stage.json").read_text())
            if (
                record["status"] != "passed"
                or record["key"] != key
                or record["files"] != files(extracted)
            ):
                raise ReleaseError("Worker result identity or file checksums differ")
        except (
            zipfile.BadZipFile,
            tarfile.TarError,
            ValueError,
            KeyError,
            ReleaseError,
        ) as error:
            saved["failed"] = True
            write_json(path, saved)
            raise ReleaseError(
                "Invalid worker result; retry to request a fresh run: " + str(error)
            ) from error
        (extracted / "stage.json").unlink()
        shutil.copytree(extracted, folder, dirs_exist_ok=True)
        write_json(
            folder / "worker.json",
            {"url": run["html_url"], "source": run["commit_sha"]},
        )
    saved["complete"] = True
    write_json(path, saved)
