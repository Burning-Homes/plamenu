#!/usr/bin/env python3
"""Run the browser accessibility matrix against an isolated Plamenu fixture."""

import os
import secrets
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BROWSER_IMAGE = (
    "mcr.microsoft.com/playwright@"
    "sha256:eff16c30e6f3f4af0a03fa4b706120d5e9b0891c344a27d64559aff5900a4a27"
)
CADDY_IMAGE = (
    "caddy@sha256:77c07d5ebfa5be9fd6c820d2094ae662c9e7eeb9bf98346b7f639900263ee2a2"
)


def run(*args, **kwargs):
    return subprocess.run(args, cwd=ROOT, check=True, **kwargs)


def free_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def wait_for_database(name):
    for _ in range(60):
        ready = subprocess.run(
            ["docker", "exec", name, "pg_isready", "-U", "postgres"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        if ready.returncode == 0:
            return
        time.sleep(1)
    raise RuntimeError("accessibility PostgreSQL fixture did not become ready")


def wait_for_server(server, url, log):
    for _ in range(180):
        healthy = subprocess.run(
            ["curl", "-fsS", "--max-time", "2", url + "/health"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        if healthy.returncode == 0:
            return
        if server.poll() is not None:
            raise RuntimeError(f"Plamenu fixture exited; see {log}")
        time.sleep(1)
    raise RuntimeError(f"Plamenu fixture timed out; see {log}")


def wait_for_https(name, url):
    for _ in range(60):
        healthy = subprocess.run(
            ["curl", "-kfsS", "--max-time", "2", url + "/health"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        if healthy.returncode == 0:
            return
        running = subprocess.run(
            ["docker", "inspect", "--format", "{{.State.Running}}", name],
            capture_output=True,
            text=True,
            check=False,
        )
        if running.returncode or running.stdout.strip() != "true":
            logs = subprocess.run(
                ["docker", "logs", name],
                capture_output=True,
                text=True,
                check=False,
            ).stderr.strip()
            raise RuntimeError(f"accessibility TLS fixture exited: {logs}")
        time.sleep(1)
    raise RuntimeError("accessibility TLS fixture did not become ready")


def main():
    for tool in ("cargo", "curl", "docker", "npm"):
        if not shutil.which(tool):
            print(f"Accessibility fixture requires {tool}", file=sys.stderr)
            return 1
    name = "plamenu-a11y-db-" + secrets.token_hex(6)
    proxy_name = "plamenu-a11y-tls-" + secrets.token_hex(6)
    password = secrets.token_hex(24)
    server = None
    log_handle = None
    with tempfile.TemporaryDirectory(prefix="plamenu-a11y-") as temporary:
        fixture = Path(temporary)
        try:
            run(
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
                "POSTGRES_DB=plamenu_a11y",
                "postgres:18-alpine@sha256:9a8afca54e7861fd90fab5fdf4c42477a6b1cb7d293595148e674e0a3181de15",
                "postgres",
                "-c",
                "fsync=off",
                "-c",
                "synchronous_commit=off",
                env=os.environ | {"POSTGRES_PASSWORD": password},
                stdout=subprocess.DEVNULL,
            )
            wait_for_database(name)
            mapping = subprocess.check_output(
                ["docker", "port", name, "5432/tcp"], text=True
            ).strip()
            database_port = int(mapping.splitlines()[0].rsplit(":", 1)[1])
            app_port = free_port()
            database_url = (
                f"postgres://postgres:{password}@127.0.0.1:{database_port}/plamenu_a11y"
            )
            config = fixture / "plamenu.toml"
            config.write_text(
                'domain = "localhost"\n'
                f'database_url = "{database_url}"\n'
                f'bind = "127.0.0.1:{app_port}"\n'
                f'media_dir = "{fixture / "media"}"\n'
                'encryption_secret = "plamenu-accessibility-fixture-only"\n'
                "encryption_secret_version = 1\n"
                "\n[smtp]\n"
                'server = "127.0.0.1"\n'
                "port = 9\n"
                'from_address = "Plamenu <notifications@localhost>"\n'
            )
            run(
                "cargo",
                "run",
                "--locked",
                "-q",
                "--",
                "--config",
                config,
                "account",
                "add",
                "developer",
            )
            run(
                "cargo",
                "run",
                "--locked",
                "-q",
                "--",
                "--config",
                config,
                "account",
                "passwd",
                "developer",
                "--email",
                "developer@localhost",
                "--password",
                "plamenu-development-only",
            )
            log = ROOT / "target/accessibility/server.log"
            log.parent.mkdir(parents=True, exist_ok=True)
            log_handle = log.open("wb")
            server = subprocess.Popen(
                [
                    "cargo",
                    "run",
                    "--locked",
                    "-q",
                    "--",
                    "--config",
                    str(config),
                    "serve",
                ],
                cwd=ROOT,
                stdout=log_handle,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
            base_url = f"http://localhost:{app_port}"
            wait_for_server(server, base_url, log)
            tls_port = free_port()
            caddyfile = fixture / "Caddyfile"
            caddyfile.write_text(
                "{\n  admin off\n  auto_https disable_redirects\n}\n"
                f"https://localhost:{tls_port} {{\n"
                "  tls internal\n"
                f"  reverse_proxy http://127.0.0.1:{app_port}\n"
                "}\n"
            )
            run(
                "docker",
                "run",
                "--detach",
                "--name",
                proxy_name,
                "--network",
                "host",
                "--volume",
                f"{caddyfile}:/etc/caddy/Caddyfile:ro",
                CADDY_IMAGE,
                stdout=subprocess.DEVNULL,
            )
            base_url = f"https://localhost:{tls_port}"
            wait_for_https(proxy_name, base_url)
            environment = os.environ | {
                "A11Y_BASE_URL": base_url,
                "A11Y_USERNAME": "developer",
                "A11Y_PASSWORD": "plamenu-development-only",
                "A11Y_CANONICAL_ORIGIN": "https://localhost",
                "A11Y_REPORT_PATH": os.environ.get(
                    "A11Y_REPORT_PATH", "target/accessibility/automated.json"
                ),
            }
            if os.environ.get("A11Y_LOCAL_BROWSERS") == "1":
                command = [
                    "npm",
                    "--prefix",
                    "accessibility",
                    "test",
                    "--",
                    *sys.argv[1:],
                ]
            else:
                command = [
                    "docker",
                    "run",
                    "--rm",
                    "--network",
                    "host",
                    "--ipc",
                    "host",
                    "--user",
                    f"{os.getuid()}:{os.getgid()}",
                    "--env",
                    "HOME=/tmp",
                    *(
                        item
                        for name in (
                            "A11Y_BASE_URL",
                            "A11Y_USERNAME",
                            "A11Y_PASSWORD",
                            "A11Y_CANONICAL_ORIGIN",
                            "A11Y_REPORT_PATH",
                        )
                        for item in ("--env", f"{name}={environment[name]}")
                    ),
                    "--volume",
                    f"{ROOT}:/work",
                    "--workdir",
                    "/work",
                    BROWSER_IMAGE,
                    "npm",
                    "--prefix",
                    "accessibility",
                    "test",
                    "--",
                    *sys.argv[1:],
                ]
            result = subprocess.run(command, cwd=ROOT, env=environment, check=False)
            return result.returncode
        except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
            print(f"Accessibility fixture failed: {error}", file=sys.stderr)
            return 1
        finally:
            if server and server.poll() is None:
                os.killpg(server.pid, signal.SIGTERM)
                try:
                    server.wait(timeout=30)
                except subprocess.TimeoutExpired:
                    os.killpg(server.pid, signal.SIGKILL)
                    server.wait()
            if log_handle:
                log_handle.close()
            subprocess.run(
                ["docker", "rm", "--force", proxy_name],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
            )
            subprocess.run(
                ["docker", "rm", "--force", "--volumes", name],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
            )


if __name__ == "__main__":
    sys.exit(main())
