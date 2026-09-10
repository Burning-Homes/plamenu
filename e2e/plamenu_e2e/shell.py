"""Subprocess plumbing: plain commands and docker compose for the Mastodon stack."""

import functools
import shlex
import subprocess

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


def masto_compose(*args: str, timeout: float = 300) -> str:
    """docker compose against the mastodon-test stack."""
    cmd = docker("compose", "--project-directory", str(config.MASTO_DIR), *args)
    return run(cmd, timeout=timeout)


def pleroma_compose(*args: str, timeout: float = 300) -> str:
    """docker compose against the pleroma-test stack (compose.dev.yml)."""
    cmd = docker("compose", "-f", str(config.PLEROMA_DIR / "compose.dev.yml"), *args)
    return run(cmd, timeout=timeout)


def sharkey_compose(*args: str, timeout: float = 300) -> str:
    """docker compose against the sharkey-test stack (compose.dev.yml)."""
    cmd = docker("compose", "-f", str(config.SHARKEY_DIR / "compose.dev.yml"), *args)
    return run(cmd, timeout=timeout)


def plup_compose(*args: str, timeout: float = 300) -> str:
    """docker compose against the upstream-Pleroma (plup) stack (compose.dev.yml)."""
    cmd = docker("compose", "-f", str(config.PLUP_DIR / "compose.dev.yml"), *args)
    return run(cmd, timeout=timeout)


def hubzilla_compose(*args: str, timeout: float = 300) -> str:
    """docker compose against the Hubzilla stack (compose.dev.yml)."""
    cmd = docker("compose", "-f", str(config.HUBZILLA_DIR / "compose.dev.yml"), *args)
    return run(cmd, timeout=timeout)


def funkwhale_compose(*args: str, timeout: float = 300) -> str:
    """docker compose against the Funkwhale stack (compose.dev.yml)."""
    cmd = docker("compose", "-f", str(config.FUNKWHALE_DIR / "compose.dev.yml"), *args)
    return run(cmd, timeout=timeout)
