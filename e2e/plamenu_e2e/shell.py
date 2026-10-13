"""Subprocess plumbing: plain commands and docker compose for the Mastodon stack."""

import functools
import json
import shlex
import subprocess
from pathlib import Path

from . import config


def run(cmd: list[str], *, cwd=None, timeout: float = 300) -> str:
    """Run a command, return stdout; raise with stderr attached on failure."""
    proc = subprocess.run(
        cmd,
        cwd=cwd,
        capture_output=True,
        text=True,
        timeout=timeout,
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"command failed (exit {proc.returncode}): {shlex.join(cmd)}\n"
            f"{proc.stderr.strip()}"
        )
    return proc.stdout


@functools.cache
def _docker_works_directly() -> bool:
    return (
        subprocess.run(["docker", "info"], capture_output=True, check=False).returncode
        == 0
    )


def docker(*args: str) -> list[str]:
    """A docker command line, via `sg docker` when the group isn't active."""
    if _docker_works_directly():
        return ["docker", *args]
    return ["sg", "docker", "-c", shlex.join(["docker", *args])]


@functools.cache
def compose_project(directory: Path, service: str | None = None) -> str | None:
    """Bind helpers to the running fixture, including preserved project names.

    A symlinked fleet directory and its original mount path identify the same
    fixture. Multiple running projects under that directory are ambiguous.
    """
    selected = directory.resolve()
    identifiers = run(
        docker(
            "ps", "--filter", "label=com.docker.compose.project", "--format", "{{.ID}}"
        )
    ).split()
    if not identifiers:
        return None
    labels = run(docker("inspect", "--format", "{{json .Config.Labels}}", *identifiers))
    projects = set()
    for line in labels.splitlines():
        row = json.loads(line)
        working = row.get("com.docker.compose.project.working_dir")
        project = row.get("com.docker.compose.project")
        if (
            working
            and project
            and Path(working).resolve() == selected
            and (service is None or row.get("com.docker.compose.service") == service)
        ):
            projects.add(project)
    if len(projects) > 1:
        raise RuntimeError(
            f"Ambiguous running fixture projects for {selected}: {', '.join(sorted(projects))}"
        )
    return next(iter(projects), None)


def command_service(args: tuple[str, ...]) -> str | None:
    """Find the service before the payload of the fixture's exec/run command."""
    if not args or args[0] not in ("exec", "run"):
        return None
    index = 1
    while index < len(args):
        option = args[index]
        if not option.startswith("-"):
            return option
        if option in (
            "-u",
            "--user",
            "-e",
            "--env",
            "-w",
            "--workdir",
            "--index",
            "--name",
        ):
            index += 2
        else:
            index += 1
    raise RuntimeError("Fixture compose command has no target service")


def fixture_compose(
    directory: Path, *args: str, timeout: float = 300, default_files: bool = False
) -> str:
    options = (
        ["--project-directory", str(directory)]
        if default_files
        else ["-f", str(directory / "compose.dev.yml")]
    )
    service = command_service(args)
    if project := compose_project(directory, service):
        options.extend(["--project-name", project])
    return run(docker("compose", *options, *args), timeout=timeout)


def masto_compose(*args: str, timeout: float = 300) -> str:
    """Docker compose against the configured masto fixture."""
    return fixture_compose(config.MASTO_DIR, *args, timeout=timeout, default_files=True)


def pleroma_compose(*args: str, timeout: float = 300) -> str:
    """Docker compose against the configured pleroma fixture."""
    return fixture_compose(config.PLEROMA_DIR, *args, timeout=timeout)


def sharkey_compose(*args: str, timeout: float = 300) -> str:
    """Docker compose against the configured sharkey fixture."""
    return fixture_compose(config.SHARKEY_DIR, *args, timeout=timeout)


def plup_compose(*args: str, timeout: float = 300) -> str:
    """Docker compose against the configured plup fixture."""
    return fixture_compose(config.PLUP_DIR, *args, timeout=timeout)


def hubzilla_compose(*args: str, timeout: float = 300) -> str:
    """Docker compose against the configured hubzilla fixture."""
    return fixture_compose(config.HUBZILLA_DIR, *args, timeout=timeout)


def funkwhale_compose(*args: str, timeout: float = 300) -> str:
    """Docker compose against the configured funkwhale fixture."""
    return fixture_compose(config.FUNKWHALE_DIR, *args, timeout=timeout)
