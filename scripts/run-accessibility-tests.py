#!/usr/bin/env python3
"""Run the browser accessibility matrix against an isolated Plamenu fixture."""

import ipaddress
import hashlib
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


def browser_image():
    """Cache the pinned engines plus a real, distro-packaged test encoder."""
    dockerfile = ROOT / "accessibility/Dockerfile"
    image = "plamenu-accessibility-encoder:" + hashlib.sha256(dockerfile.read_bytes()).hexdigest()[:16]
    present = subprocess.run(["docker", "image", "inspect", image],
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
    if present.returncode:
        run("docker", "build", "--file", dockerfile, "--tag", image, ROOT / "accessibility")
    run("docker", "run", "--rm", image, "ffmpeg", "-version", stdout=subprocess.DEVNULL)
    return image


def free_port(address="127.0.0.1"):
    with socket.socket() as listener:
        listener.bind((address, 0))
        return listener.getsockname()[1]


def seed_native_draft_pages(container, fixture, video):
    """Known persisted states for browser recovery UI; not a worker E2E."""
    media = fixture / "media"
    media.mkdir(exist_ok=True)
    poster = fixture / "draft-poster.avif"
    run("ffmpeg", "-nostdin", "-v", "error", "-i", video, "-frames:v", "1",
        "-c:v", "libaom-av1", "-still-picture", "1", "-cpu-used", "8", poster)
    run("docker", "exec", "-i", container, "psql", "-U", "postgres", "-d", "plamenu_a11y",
        "-v", f"source_bytes={video.stat().st_size}", "-v", f"poster_bytes={poster.stat().st_size}",
        input=(ROOT / "accessibility/fixtures/native-drafts.sql").read_text(), text=True,
        stdout=subprocess.DEVNULL)
    run("docker", "exec", "-i", container, "psql", "-U", "postgres", "-d", "plamenu_a11y",
        input=(ROOT / "accessibility/fixtures/video-catalogue.sql").read_text(), text=True,
        stdout=subprocess.DEVNULL)
    for media_id in range(9001, 9015):
        shutil.copyfile(video, media / f"{media_id}.orig")
        shutil.copyfile(video, media / f"{media_id}.draft.mp4")
        shutil.copyfile(poster, media / f"{media_id}.draft.small.avif")


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
            ["curl", "--noproxy", "*", "-fsS", "--max-time", "2", url + "/health"],
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
            ["curl", "--noproxy", "*", "-kfsS", "--max-time", "2", url + "/health"],
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


def local_browser_preflight():
    """Reject unsupported Linux host WebKit before creating test fixtures."""
    if not sys.platform.startswith("linux"):
        return
    inspector = shutil.which("gst-inspect-1.0")
    if not inspector or subprocess.run(
        [inspector, "autoaudiosink"], capture_output=True, check=False
    ).returncode:
        raise RuntimeError(
            "Host WebKit needs GStreamer autoaudiosink (gst-plugins-good) and "
            "gst-inspect-1.0. Use the default pinned browser image by unsetting "
            "A11Y_LOCAL_BROWSERS, or provide complete host browser test dependencies. "
            "These are test dependencies, not Plamenu runtime requirements."
        )


def main():
    if sys.argv[1:] == ["--prepare-browser-image"]:
        print(browser_image())
        return 0
    for tool in ("cargo", "curl", "docker", "npm", "ffmpeg"):
        if not shutil.which(tool):
            print(f"Accessibility fixture requires {tool}", file=sys.stderr)
            return 1
    name = "plamenu-a11y-db-" + secrets.token_hex(6)
    proxy_name = "plamenu-a11y-tls-" + secrets.token_hex(6)
    password = secrets.token_hex(24)
    server = None
    log_handle = None
    local_browsers = os.environ.get("A11Y_LOCAL_BROWSERS") == "1"
    if local_browsers:
        try:
            local_browser_preflight()
        except RuntimeError as error:
            print(str(error), file=sys.stderr)
            return 1
    encoder_image = BROWSER_IMAGE if local_browsers else browser_image()
    with tempfile.TemporaryDirectory(prefix="plamenu-a11y-") as temporary:
        fixture = Path(temporary)
        try:
            video_fixture = fixture / "native-video.mp4"
            run(
                "ffmpeg",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=640x480:rate=10",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440",
                "-t",
                "3",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-movflags",
                "+faststart",
                str(video_fixture),
            )
            audio_fixture = fixture / "native-audio.m4a"
            run(
                "ffmpeg", "-v", "error", "-f", "lavfi", "-i",
                "sine=frequency=440:duration=9", "-c:a", "aac",
                str(audio_fixture),
            )
            poster_fixture = fixture / "native-poster.png"
            run(
                "ffmpeg", "-v", "error", "-f", "lavfi", "-i",
                "color=c=blue:size=96x96", "-frames:v", "1",
                str(poster_fixture),
            )
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
            # Share the TLS container's network namespace with the browsers.
            # Canonical absolute media URLs must work unchanged on port 443;
            # routing requests to a random port changes origin/cookie semantics.
            # Only the Docker bridge can reach the host-side app listener.
            app_address = "127.0.0.1"
            if not local_browsers:
                app_address = str(
                    ipaddress.IPv4Address(
                        subprocess.check_output(
                            [
                                "docker",
                                "network",
                                "inspect",
                                "bridge",
                                "--format",
                                "{{(index .IPAM.Config 0).Gateway}}",
                            ],
                            text=True,
                        ).strip()
                    )
                )
            app_port = free_port(app_address)
            live_port = free_port(app_address)
            database_url = (
                f"postgres://postgres:{password}@127.0.0.1:{database_port}/plamenu_a11y"
            )
            config = fixture / "plamenu.toml"
            # Explicit local-peer import drills permit local addresses; ordinary
            # browser fixtures retain the production-default SSRF guards.
            import_private_fetch = (
                "allow_private_fetch = true\n" if (os.environ.get("A11Y_IMPORT_SOURCE_URL") or os.environ.get("A11Y_SOURCE_SYNC_URL")) else ""
            )
            config.write_text(
                'domain = "localhost"\n'
                f'{import_private_fetch}'
                f'database_url = "{database_url}"\n'
                f'bind = "{app_address}:{app_port}"\n'
                # External encoder tests run in the browser/TLS namespace.
                # Bind only the same isolated host-side address as HTTP; a
                # container's loopback cannot reach the host's loopback.
                f'live_ingest_bind = "{app_address}:{live_port}"\n'
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
            for username in ("channel_editor",):
                run("cargo", "run", "--locked", "-q", "--", "--config", config,
                    "account", "add", username, "--email", f"{username}@localhost",
                    "--password", "plamenu-development-only")
            seed_native_draft_pages(name, fixture, video_fixture)
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
            base_url = f"http://{app_address}:{app_port}"
            wait_for_server(server, base_url, log)
            tls_port = free_port() if local_browsers else 443
            caddyfile = fixture / "Caddyfile"
            caddyfile.write_text(
                "{\n  admin off\n  auto_https disable_redirects\n}\n"
                f"https://localhost:{tls_port} {{\n"
                "  tls internal\n"
                f"  reverse_proxy http://{app_address}:{app_port}\n"
                "}\n"
            )
            run(
                "docker",
                "run",
                "--detach",
                "--name",
                proxy_name,
                *(
                    ["--network", "host"]
                    if local_browsers
                    else ["--network", "bridge", "--publish", "127.0.0.1::443"]
                ),
                "--volume",
                f"{caddyfile}:/etc/caddy/Caddyfile:ro",
                CADDY_IMAGE,
                stdout=subprocess.DEVNULL,
            )
            if local_browsers:
                base_url = f"https://localhost:{tls_port}"
                readiness_url = base_url
            else:
                mapping = subprocess.check_output(
                    ["docker", "port", proxy_name, "443/tcp"], text=True
                ).strip()
                published_port = int(mapping.splitlines()[0].rsplit(":", 1)[1])
                readiness_url = f"https://localhost:{published_port}"
                base_url = "https://localhost"
            wait_for_https(proxy_name, readiness_url)
            environment = os.environ | {
                "A11Y_BASE_URL": base_url,
                "A11Y_USERNAME": "developer",
                "A11Y_PASSWORD": "plamenu-development-only",
                "A11Y_CANONICAL_ORIGIN": "https://localhost",
                "A11Y_REPORT_PATH": os.environ.get(
                    "A11Y_REPORT_PATH", "target/accessibility/automated.json"
                ),
            }
            if local_browsers:
                environment["A11Y_VIDEO_FIXTURE"] = str(video_fixture)
                environment["A11Y_AUDIO_FIXTURE"] = str(audio_fixture)
                environment["A11Y_POSTER_FIXTURE"] = str(poster_fixture)
                command = [
                    "npm",
                    "--prefix",
                    "accessibility",
                    "test",
                    "--",
                    *sys.argv[1:],
                ]
            else:
                # A linked worktree's .git file points outside the checkout.
                # Preserve its absolute path and expose only the shared Git
                # metadata read-only so the evidence reporter records revision
                # and dirty state without mounting the other working tree.
                git_common = Path(
                    subprocess.check_output(
                        [
                            "git",
                            "rev-parse",
                            "--path-format=absolute",
                            "--git-common-dir",
                        ],
                        cwd=ROOT,
                        text=True,
                    ).strip()
                )
                git_mount = (
                    []
                    if git_common.is_relative_to(ROOT)
                    else [
                        "--volume",
                        f"{git_common}:{git_common}:ro",
                    ]
                )
                command = [
                    "docker",
                    "run",
                    "--rm",
                    "--network",
                    f"container:{proxy_name}",
                    "--ipc",
                    "host",
                    "--user",
                    f"{os.getuid()}:{os.getgid()}",
                    "--env",
                    "HOME=/tmp",
                    "--env",
                    "A11Y_VIDEO_FIXTURE=/tmp/native-video.mp4",
                    "--env",
                    "A11Y_AUDIO_FIXTURE=/tmp/native-audio.m4a",
                    "--env",
                    "A11Y_POSTER_FIXTURE=/tmp/native-poster.png",
                    "--env",
                    f"A11Y_LIVE_INGEST_HOST={app_address}",
                    "--env",
                    "A11Y_BROWSER_IMAGE=" + subprocess.check_output(
                        ["docker", "image", "inspect", "--format", "{{.Id}}", encoder_image], text=True
                    ).strip(),
                    *(["--env", f"A11Y_IMPORT_SOURCE_URL={os.environ['A11Y_IMPORT_SOURCE_URL']}"]
                      if os.environ.get("A11Y_IMPORT_SOURCE_URL") else []),
                    *(["--env", f"A11Y_SOURCE_SYNC_URL={os.environ['A11Y_SOURCE_SYNC_URL']}"]
                      if os.environ.get("A11Y_SOURCE_SYNC_URL") else []),
                    "--volume",
                    f"{video_fixture}:/tmp/native-video.mp4:ro",
                    "--volume",
                    f"{audio_fixture}:/tmp/native-audio.m4a:ro",
                    "--volume",
                    f"{poster_fixture}:/tmp/native-poster.png:ro",
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
                    f"{ROOT}:{ROOT}",
                    *git_mount,
                    "--workdir",
                    str(ROOT),
                    encoder_image,
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
