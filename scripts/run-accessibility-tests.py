#!/usr/bin/env python3
"""Run the browser accessibility matrix against an isolated Plamenu fixture."""

import email.parser
import email.policy
import hashlib
import html
import http.server
import ipaddress
import json
import os
import secrets
import shutil
import signal
import socket
import socketserver
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse
from contextlib import ExitStack
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "e2e"))
sys.path.insert(0, str(ROOT / "scripts"))
from browser_fixture import BrowserFixture, finalize_report
from cold_rendition_fixture import ColdRenditions
from plamenu_e2e.lifecycle import owns_listener, serve_in_thread, stop_process

BROWSER_IMAGE = (
    "mcr.microsoft.com/playwright@"
    "sha256:eff16c30e6f3f4af0a03fa4b706120d5e9b0891c344a27d64559aff5900a4a27"
)
CADDY_IMAGE = (
    "caddy@sha256:77c07d5ebfa5be9fd6c820d2094ae662c9e7eeb9bf98346b7f639900263ee2a2"
)
BROWSER_PROJECTS = (
    "chromium-desktop",
    "chromium-mobile",
    "firefox-desktop",
    "firefox-mobile",
    "webkit-desktop",
    "webkit-mobile",
    "chromium-no-javascript",
)


PARTICIPANT_PREFIX = "peer_"


def tls_configuration(address, app_port, tls_port, *, offline=False):
    """Keep independent fixture clients distinct without relaxing rate limits.

    Only bounded engine/role names map to addresses. Arbitrary forwarded
    headers are replaced. Each client's failed logins still count normally.
    """
    identities = tuple(
        project + suffix
        for suffix in ("", "-participant", "-security", "-recovery", "-throttle")
        for project in BROWSER_PROJECTS
    )
    clients = "".join(
        f"    {identity} 198.18.0.{index}\n"
        for index, identity in enumerate(identities, 1)
    )

    admin = "127.0.0.1:2019" if offline else "off"
    return (
        f"{{\n  admin {admin}\n  auto_https disable_redirects\n}}\n"
        f"https://localhost:{tls_port}, https://*.webxdc.localhost:{tls_port} {{\n"
        "  tls internal\n"
        "  map {http.request.header.X-Plamenu-Fixture-Client} {fixture_client_ip} {\n"
        f"{clients}"
        "    default 198.18.0.100\n"
        "  }\n"
        f"  reverse_proxy http://{address}:{app_port} {{\n"
        "    header_up X-Forwarded-For {fixture_client_ip}\n"
        "  }\n"
        "}\n"
    )


def offline_proxy_control(address, proxy, fixture):
    """Disconnect only this fixture's TLS listener, retaining its network namespace.

    Browser offline emulation does not consistently disconnect service-worker
    fetches. A private nonce endpoint controls this exact disposable proxy; its
    loopback-only admin listener is enabled solely for this opt-in fixture.
    """
    token = secrets.token_urlsafe(24)

    def inspect():
        return json.loads(
            subprocess.check_output(["docker", "inspect", proxy], text=True)
        )[0]

    initial = inspect()
    expected = str((fixture / "Caddyfile").resolve())
    assert initial["Name"] == "/" + proxy and proxy.startswith("plamenu-a11y-tls-")
    assert any(
        m["Source"] == expected and m["Destination"] == "/etc/caddy/Caddyfile"
        for m in initial["Mounts"]
    )

    def active():
        return json.loads(
            subprocess.check_output(
                [
                    "docker",
                    "exec",
                    initial["Id"],
                    "wget",
                    "-qO-",
                    "http://127.0.0.1:2019/config/",
                ],
                text=True,
                timeout=15,
            )
        )

    original = active()
    disconnected = json.loads(json.dumps(original))
    assert list(disconnected["apps"]["http"]["servers"]) == ["srv0"]
    disconnected["apps"]["http"]["servers"] = {}
    current = original

    def change(online):
        nonlocal current
        actual = inspect()
        assert actual["Id"] == initial["Id"] and actual["Image"] == initial["Image"]
        assert (
            actual["State"]["Pid"] == initial["State"]["Pid"]
            and actual["RestartCount"] == initial["RestartCount"]
        )
        assert active() == current
        target = original if online else disconnected
        subprocess.run(
            ["docker", "exec", "-i", initial["Id"], "caddy", "reload", "--config", "-"],
            input=json.dumps(target),
            text=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=True,
            timeout=15,
        )
        assert active() == target
        current = target

    class Handler(http.server.BaseHTTPRequestHandler):
        def setup(self):
            super().setup()
            self.connection.settimeout(15)

        def do_POST(self):
            if self.path not in (f"/{token}/offline", f"/{token}/online"):
                self.send_error(403)
                return
            try:
                change(self.path.endswith("/online"))
            except (AssertionError, OSError, subprocess.SubprocessError):
                self.send_error(500, "Owned proxy transition failed")
                return
            self.send_response(204)
            self.end_headers()

        def log_message(self, *args):
            pass

    control = http.server.HTTPServer((address, 0), Handler)
    stop_control = serve_in_thread(control)

    def close():
        stop_control()
        if current != original:
            change(True)

    close.reset = lambda: change(True) if current != original else None
    return f"http://{address}:{control.server_port}/{token}", close


def rendition_encoder(fixture, control):
    """Ordinary encodes run normally; only an opted-in attempt holds input."""
    real = shutil.which("ffmpeg")
    if not real:
        raise RuntimeError("Cold rendition fixture requires FFmpeg")
    script = fixture / "rendition-encoder.py"
    script.write_text(
        f"#!{sys.executable}\n"
        "import sys\n"
        f"sys.path.insert(0,{str(ROOT / 'scripts')!r})\n"
        "from cold_rendition_fixture import encode\n"
        f"encode({real!r},{str(control.active)!r},sys.argv[1:])\n"
    )
    script.chmod(0o700)
    return script


def run(*args, **kwargs):
    return subprocess.run(args, cwd=ROOT, check=True, **kwargs)


class ApplicationBinary:
    """Freeze one executable for every fixture CLI and the browser server."""

    def __init__(self, path):
        path = Path(path)
        if not path.is_absolute():
            raise ValueError("PLAMENU_BINARY must be an absolute executable path")
        self.path = path.resolve()
        if not self.path.is_file() or not os.access(self.path, os.X_OK):
            raise RuntimeError("Plamenu candidate is missing or not executable")
        # Hash once, then bind every command and running process to that exact
        # inode version. Linux ctime cannot be restored with utime: even an
        # equal-size write with a restored mtime invalidates this binding.
        # Reject metadata drift too, rather than accepting a later rehash.
        with self.path.open("rb") as source:
            self.version = self.file_version(os.fstat(source.fileno()))
            self.sha256 = hashlib.file_digest(source, "sha256").hexdigest()
            if self.file_version(os.fstat(source.fileno())) != self.version:
                raise RuntimeError("Plamenu candidate changed while being bound")
        self.verify(self.path)

    @staticmethod
    def file_version(stat):
        return (stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns,
                stat.st_ctime_ns, stat.st_mode)

    def verify(self, path):
        if self.file_version(Path(path).stat()) != self.version:
            raise RuntimeError("Plamenu candidate changed during the browser fixture")

    def command(self, config, *arguments):
        self.verify(self.path)
        return [str(self.path), "--config", str(config), *arguments]

    def running_identity(self, process):
        proc = Path(f"/proc/{process.pid}")
        if (
            process.poll() is not None
            or (proc / "exe").resolve() != self.path
        ):
            raise RuntimeError("Running browser fixture differs from its candidate")
        self.verify(self.path)
        self.verify(proc / "exe")
        return {"pid": process.pid, "executable": str(self.path), "sha256": self.sha256,
                "file_version": self.version}


def prepare_application():
    selected = os.environ.get("PLAMENU_BINARY")
    if selected:
        return ApplicationBinary(selected)
    run("cargo", "build", "--locked", "-q")
    return ApplicationBinary(ROOT / "target/debug/plamenu")


def browser_image():
    """Cache the pinned engines plus a real, distro-packaged test encoder."""
    dockerfile = ROOT / "accessibility/Dockerfile"
    image = (
        "plamenu-accessibility-encoder:"
        + hashlib.sha256(dockerfile.read_bytes()).hexdigest()[:16]
    )
    present = subprocess.run(
        ["docker", "image", "inspect", image],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    if present.returncode:
        run(
            "docker",
            "build",
            "--file",
            dockerfile,
            "--tag",
            image,
            ROOT / "accessibility",
        )
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
    run(
        "ffmpeg",
        "-nostdin",
        "-v",
        "error",
        "-i",
        video,
        "-frames:v",
        "1",
        "-c:v",
        "libaom-av1",
        "-still-picture",
        "1",
        "-cpu-used",
        "8",
        poster,
    )
    run(
        "docker",
        "exec",
        "-i",
        container,
        "psql",
        "-U",
        "postgres",
        "-d",
        "plamenu_a11y",
        "-v",
        f"source_bytes={video.stat().st_size}",
        "-v",
        f"poster_bytes={poster.stat().st_size}",
        input=(ROOT / "accessibility/fixtures/native-drafts.sql").read_text(),
        text=True,
        stdout=subprocess.DEVNULL,
    )
    run(
        "docker",
        "exec",
        "-i",
        container,
        "psql",
        "-U",
        "postgres",
        "-d",
        "plamenu_a11y",
        input=(ROOT / "accessibility/fixtures/video-catalogue.sql").read_text(),
        text=True,
        stdout=subprocess.DEVNULL,
    )
    for media_id in range(9001, 9015):
        shutil.copyfile(video, media / f"{media_id}.orig")
        shutil.copyfile(video, media / f"{media_id}.draft.mp4")
        shutil.copyfile(poster, media / f"{media_id}.draft.small.avif")


def wait_for_database(name):
    # initdb briefly exposes a Unix-only temporary server. Require the final
    # TCP listener used by the application before starting migrations/CLI.
    for _ in range(60):
        ready = subprocess.run(
            ["docker", "exec", name, "pg_isready", "-h", "127.0.0.1", "-U", "postgres"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        if ready.returncode == 0:
            return
        time.sleep(1)
    raise RuntimeError("accessibility PostgreSQL fixture did not become ready")


def wait_for_server(server, url, log):
    deadline = time.monotonic() + 180
    while time.monotonic() < deadline:
        if server.poll() is not None:
            raise RuntimeError(f"Plamenu fixture exited; see {log}")
        healthy = subprocess.run(
            ["curl", "--noproxy", "*", "-fsS", "--max-time", "2", url + "/health"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        if healthy.returncode == 0 and owns_listener(server, urllib.parse.urlsplit(url).port):
            return
        if server.poll() is not None:
            raise RuntimeError(f"Plamenu fixture exited; see {log}")
        time.sleep(0.1)
    raise RuntimeError(f"Plamenu fixture timed out; see {log}")


def wait_for_https(name, url, certificate):
    for _ in range(60):
        healthy = subprocess.run(
            [
                "curl",
                "--noproxy",
                "*",
                "--cacert",
                str(certificate),
                "-fsS",
                "--max-time",
                "2",
                url + "/health",
            ],
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
    if (
        not inspector
        or subprocess.run(
            [inspector, "autoaudiosink"], capture_output=True, check=False
        ).returncode
    ):
        raise RuntimeError(
            "Host WebKit needs GStreamer autoaudiosink (gst-plugins-good) and "
            "gst-inspect-1.0. Use the default pinned browser image by unsetting "
            "A11Y_LOCAL_BROWSERS, or provide complete host browser test dependencies. "
            "These are test dependencies, not Plamenu runtime requirements."
        )


def podcast_producer(address, audio, poster):
    """A real bounded RSS/enclosure origin confined to the disposable fixture."""

    class Handler(http.server.BaseHTTPRequestHandler):
        def setup(self):
            super().setup()
            self.connection.settimeout(15)

        def do_GET(self):
            parsed = urllib.parse.urlsplit(self.path)
            if parsed.path == "/show.xml":
                title = urllib.parse.parse_qs(parsed.query).get(
                    "title", ["Fixture show"]
                )[0][:100]
                base = f"http://{address}:{self.server.server_port}"
                items = "".join(
                    f"<item><guid>episode-{index}</guid><title>{html.escape(title)} {index}</title>"
                    f"<description>Episode {index} notes</description>"
                    f'<enclosure url="{base}/audio.m4a" type="audio/mp4" length="{audio.stat().st_size}"/>'
                    "<i:duration>9</i:duration></item>"
                    for index in (1, 2)
                )
                body = (
                    '<rss xmlns:i="http://www.itunes.com/dtds/podcast-1.0.dtd"><channel>'
                    f"<title>{html.escape(title)}</title><description>Podcast browser fixture</description>"
                    f'<i:image href="{base}/cover.png"/>{items}</channel></rss>'
                ).encode()
                content_type = "application/rss+xml"
            elif parsed.path == "/show":
                feed_path = "/show.xml" + ("?" + parsed.query if parsed.query else "")
                body = (
                    '<!doctype html><html><head><link rel="alternate" '
                    f'type="application/rss+xml" href="{html.escape(feed_path, quote=True)}">'
                    "</head><body>Podcast website fixture</body></html>"
                ).encode()
                content_type = "text/html; charset=utf-8"
            elif parsed.path in ("/audio.m4a", "/cover.png"):
                body = (audio if parsed.path == "/audio.m4a" else poster).read_bytes()
                content_type = (
                    "audio/mp4" if parsed.path == "/audio.m4a" else "image/png"
                )
            else:
                self.send_error(404)
                return
            self.send_response(200)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *args):
            pass

    producer = http.server.ThreadingHTTPServer((address, 0), Handler)
    producer.close_fixture = serve_in_thread(producer)
    return producer


def fixture_mailbox(address):
    """Bounded local SMTP sink with a nonce-protected, read-only test inbox."""
    nonce = secrets.token_urlsafe(24)
    messages = []
    lock = threading.Lock()

    class SMTP(socketserver.StreamRequestHandler):
        def handle(self):
            try:
                self.session()
            except (TimeoutError, ConnectionError):
                # An idle pooled connection or a client going away is normal.
                pass

        def session(self):
            self.connection.settimeout(15)
            self.wfile.write(b"220 localhost fixture SMTP\r\n")
            while line := self.rfile.readline(65537):
                command = line.split(b" ", 1)[0].strip().upper()
                if command in (b"EHLO", b"HELO"):
                    self.wfile.write(b"250-localhost\r\n250 SIZE 1048576\r\n")
                elif command == b"DATA":
                    self.wfile.write(b"354 End with a dot\r\n")
                    body = bytearray()
                    while chunk := self.rfile.readline(65537):
                        if chunk == b".\r\n":
                            break
                        body.extend(chunk[1:] if chunk.startswith(b"..") else chunk)
                        if len(body) > 1048576:
                            self.wfile.write(b"552 Message too large\r\n")
                            return
                    message = email.parser.BytesParser(policy=email.policy.default).parsebytes(body)
                    with lock:
                        messages.append({
                            "to": str(message.get("To", "")),
                            "subject": str(message.get("Subject", "")),
                            "body": message.get_content(),
                        })
                        del messages[:-256]
                    self.wfile.write(b"250 Accepted\r\n")
                elif command == b"QUIT":
                    self.wfile.write(b"221 Bye\r\n")
                    return
                elif command in (b"MAIL", b"RCPT", b"RSET", b"NOOP"):
                    self.wfile.write(b"250 OK\r\n")
                else:
                    self.wfile.write(b"502 Unsupported\r\n")

    class Inbox(http.server.BaseHTTPRequestHandler):
        def setup(self):
            super().setup()
            self.connection.settimeout(15)

        def do_GET(self):
            parsed = urllib.parse.urlsplit(self.path)
            if parsed.path != f"/{nonce}":
                self.send_error(403)
                return
            recipient = urllib.parse.parse_qs(parsed.query).get("recipient", [""])[0]
            with lock:
                body = json.dumps([m for m in messages if m["to"] == recipient]).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *args):
            pass

    class SMTPServer(socketserver.ThreadingTCPServer):
        def __init__(self, *args):
            self.connections = set()
            self.changed = threading.Condition()
            super().__init__(*args)

        def get_request(self):
            connection, address = super().get_request()
            with self.changed:
                self.connections.add(connection)
            return connection, address

        def shutdown_request(self, connection):
            try:
                super().shutdown_request(connection)
            finally:
                with self.changed:
                    self.connections.discard(connection)
                    self.changed.notify_all()

        def drain(self):
            # Called after the application stops. Flush every accepted SMTP
            # handler before clearing the mailbox, including incomplete DATA.
            with self.changed:
                for connection in self.connections:
                    try:
                        connection.shutdown(socket.SHUT_RDWR)
                    except OSError:
                        pass
                if not self.changed.wait_for(lambda: not self.connections, timeout=20):
                    raise RuntimeError("Fixture SMTP requests did not stop")

    with ExitStack() as setup:
        smtp = SMTPServer(("127.0.0.1", 0), SMTP)
        setup.callback(smtp.server_close)
        inbox = http.server.ThreadingHTTPServer((address, 0), Inbox)
        setup.callback(inbox.server_close)
        setup.callback(serve_in_thread(smtp))
        setup.callback(serve_in_thread(inbox))
        setup.callback(smtp.drain)
        services = setup.pop_all()

    def close():
        services.close()

    def reset():
        smtp.drain()
        with lock:
            messages.clear()

    close.reset = reset

    return smtp.server_address[1], f"http://{address}:{inbox.server_port}/{nonce}", close


def remove_container(name, owner):
    def owned_ids():
        return subprocess.check_output(
            ["docker", "ps", "--all", "--no-trunc", "--filter", f"name=^/{name}$",
             "--filter", f"label=plamenu.fixture.run={owner}", "--format", "{{.ID}}"],
            text=True, timeout=30,
        ).splitlines()

    for identifier in owned_ids():
        result = subprocess.run(
            ["docker", "rm", "--force", "--volumes", identifier],
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True, timeout=30,
            check=False,
        )
        # --rm may remove the browser between listing and removal.
        if result.returncode and identifier in owned_ids():
            raise RuntimeError(f"Owned container cleanup failed: {result.stderr.strip()}")


def main():
    if sys.argv[1:] == ["--prepare-browser-image"]:
        print(browser_image())
        return 0
    tools = ("curl", "docker", "npm", "ffmpeg")
    if not os.environ.get("PLAMENU_BINARY"):
        tools += ("cargo",)
    for tool in tools:
        if not shutil.which(tool):
            print(f"Accessibility fixture requires {tool}", file=sys.stderr)
            return 1
    run_dir = ROOT / "target/accessibility/runs" / secrets.token_hex(16)
    report_path = Path(os.environ.get("A11Y_REPORT_PATH", run_dir / "automated.json"))
    if not report_path.is_absolute():
        report_path = ROOT / report_path
    if report_path.exists():
        print("Accessibility report already exists; choose a fresh path", file=sys.stderr)
        return 1
    run_dir.mkdir(parents=True, mode=0o700)
    print(f"Accessibility run: {run_dir}", flush=True)
    name = "plamenu-a11y-db-" + secrets.token_hex(6)
    proxy_name = "plamenu-a11y-tls-" + secrets.token_hex(6)
    browser_name = "plamenu-a11y-browser-" + secrets.token_hex(6)
    password = secrets.token_hex(24)
    server = None
    podcasts = None
    offline_close = None
    mailbox_close = None
    log_handle = None
    local_browsers = os.environ.get("A11Y_LOCAL_BROWSERS") == "1"
    if local_browsers:
        print(
            "Strict fixture TLS trust is confined to disposable containers; unset A11Y_LOCAL_BROWSERS.",
            file=sys.stderr,
        )
        return 1
    if local_browsers:
        try:
            local_browser_preflight()
        except RuntimeError as error:
            print(str(error), file=sys.stderr)
            return 1
    try:
        run("python3", "scripts/update-gate-inventory.py", "--suite", "browser")
        application = prepare_application()
    except (OSError, ValueError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"Accessibility candidate failed: {error}", file=sys.stderr)
        return 1
    encoder_image = BROWSER_IMAGE if local_browsers else browser_image()
    with ExitStack() as cleanup:
        temporary = cleanup.enter_context(tempfile.TemporaryDirectory(prefix="plamenu-a11y-"))
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
                "ffmpeg",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=9",
                "-c:a",
                "aac",
                str(audio_fixture),
            )
            poster_fixture = fixture / "native-poster.png"
            run(
                "ffmpeg",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:size=96x96",
                "-frames:v",
                "1",
                str(poster_fixture),
            )
            cleanup.callback(remove_container, name, run_dir.name)
            run(
                "docker",
                "run",
                "--detach",
                "--label", f"plamenu.fixture.run={run_dir.name}",
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
            if os.environ.get("A11Y_PODCAST_FIXTURE") == "1":
                podcasts = podcast_producer(app_address, audio_fixture, poster_fixture)
                cleanup.callback(podcasts.close_fixture)
            app_port = free_port(app_address)
            live_port = free_port(app_address)
            tls_port = free_port() if local_browsers else 443
            caddyfile = fixture / "Caddyfile"
            caddyfile.write_text(
                tls_configuration(
                    app_address,
                    app_port,
                    tls_port,
                    offline=os.environ.get("A11Y_OFFLINE_FIXTURE", "1") == "1",
                )
            )
            cleanup.callback(remove_container, proxy_name, run_dir.name)
            run(
                "docker",
                "run",
                "--detach",
                "--label", f"plamenu.fixture.run={run_dir.name}",
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
            # Trust only this disposable reverse proxy, never all Docker peers.
            proxy_address = (
                "127.0.0.1"
                if local_browsers
                else str(
                    ipaddress.IPv4Address(
                        subprocess.check_output(
                            [
                                "docker",
                                "inspect",
                                "--format",
                                '{{(index .NetworkSettings.Networks "bridge").IPAddress}}',
                                proxy_name,
                            ],
                            text=True,
                        ).strip()
                    )
                )
            )
            database_url = (
                f"postgres://postgres:{password}@127.0.0.1:{database_port}/plamenu_a11y"
            )
            config = fixture / "plamenu.toml"
            cold_renditions = ColdRenditions(fixture / "cold-renditions") if os.environ.get("A11Y_COLD_RENDITION_FIXTURE") == "1" else None
            rendition_config = (
                f"ffmpeg_path = {json.dumps(str(rendition_encoder(fixture, cold_renditions)))}\n"
                if cold_renditions
                else ""
            )
            # Explicit local-peer import drills permit local addresses; ordinary
            # browser fixtures retain the production-default SSRF guards.
            import_private_fetch = (
                "allow_private_fetch = true\n"
                if (
                    os.environ.get("A11Y_IMPORT_SOURCE_URL")
                    or os.environ.get("A11Y_SOURCE_SYNC_URL")
                    or os.environ.get("A11Y_PODCAST_FIXTURE") == "1"
                )
                else ""
            )
            smtp_port, mailbox_url, mailbox_close = fixture_mailbox(app_address)
            cleanup.callback(mailbox_close)
            config.write_text(
                'domain = "localhost"\n'
                f'trusted_proxies = ["{proxy_address}/32"]\n'
                f"{import_private_fetch}"
                f"{rendition_config}"
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
                f"port = {smtp_port}\n"
                'from_address = "Plamenu <notifications@localhost>"\n'
            )
            run(*application.command(config, "account", "add", "developer"))
            run(
                *application.command(
                    config,
                    "account",
                    "passwd",
                    "developer",
                    "--email",
                    "developer@localhost",
                    "--password",
                    "plamenu-development-only",
                ),
            )
            # Each engine creates real channels under its own finite quota;
            # persisted private draft/catalogue fixtures keep their seed owner.
            creator_projects = BROWSER_PROJECTS
            creator_users = tuple(
                "a11y_" + project.replace("-", "_") for project in creator_projects
            )
            music_viewers = tuple(
                "music_viewer_" + project.replace("-", "_")
                for project in creator_projects
            )
            music_erasure_users = tuple(
                "music_erasure_" + project.replace("-", "_")
                for project in creator_projects
            )
            for username in (
                "channel_editor",
                "a11y_moderator",
                "recovery_no_javascript",
                *creator_users,
                *("security_" + project.replace("-", "_") for project in creator_projects),
                *(PARTICIPANT_PREFIX + project.replace("-", "_") for project in creator_projects),
                *("limit_" + project.replace("-", "_") for project in creator_projects),
                *music_viewers,
                *music_erasure_users,
            ):
                run(
                    *application.command(
                        config,
                        "account",
                        "add",
                        username,
                        "--email",
                        f"{username}@localhost",
                        "--password",
                        "plamenu-development-only",
                    ),
                )
            seed_native_draft_pages(name, fixture, video_fixture)
            # Exercise the actual registration form, including conditional age
            # input, rather than only scanning the closed-registration notice.
            # This changes only the disposable fixture, never instance policy.
            run(
                "docker",
                "exec",
                "-i",
                name,
                "psql",
                "-U",
                "postgres",
                "-d",
                "plamenu_a11y",
                "-v",
                "ON_ERROR_STOP=1",
                # Keep the real inactive-moderator safeguard enabled. A fixture
                # moderator prevents it closing signup during a long matrix;
                # all browser journey accounts retain ordinary member roles.
                input=(
                    "UPDATE users SET role_id = (SELECT id FROM user_roles WHERE name = 'Admin'), "
                    "current_sign_in_at = now() WHERE account_id = "
                    "(SELECT id FROM accounts WHERE username = 'a11y_moderator' AND domain IS NULL); "
                    "UPDATE instance_settings SET registrations_mode = 'open', min_age = 18;"
                ),
                text=True,
                stdout=subprocess.DEVNULL,
            )
            # Seed once, then clone this offline baseline for every test attempt.
            # No application connection or background worker can reach the template.
            def database(sql):
                run("docker", "exec", "-i", name, "psql", "-U", "postgres",
                    "-d", "postgres", "-v", "ON_ERROR_STOP=1", input=sql,
                    text=True, stdout=subprocess.DEVNULL)

            database("CREATE DATABASE plamenu_a11y_baseline TEMPLATE plamenu_a11y; "
                     "ALTER DATABASE plamenu_a11y_baseline ALLOW_CONNECTIONS false;")
            baseline_media = fixture / "baseline-media"
            shutil.copytree(fixture / "media", baseline_media)
            attempt = 0
            base_url = f"http://{app_address}:{app_port}"

            def stop_server():
                nonlocal server, log_handle
                process, server = server, None
                try:
                    stop_process(process, timeout=2)
                finally:
                    if log_handle:
                        log_handle.close()
                        log_handle = None

            cleanup.callback(stop_server)

            def reset_server():
                nonlocal server, log_handle, attempt
                phases = {}
                checkpoint = time.monotonic()

                def measured(name):
                    nonlocal checkpoint
                    now = time.monotonic()
                    phases[name] = now - checkpoint
                    checkpoint = now

                stop_server()
                measured("stop")
                if offline_close:
                    offline_close.reset()
                mailbox_close.reset()
                measured("auxiliary_reset")
                database("DROP DATABASE plamenu_a11y WITH (FORCE); "
                         "CREATE DATABASE plamenu_a11y TEMPLATE plamenu_a11y_baseline;")
                measured("database_clone")
                shutil.rmtree(fixture / "media")
                shutil.copytree(baseline_media, fixture / "media")
                measured("media_copy")
                attempt += 1
                log = run_dir / f"server-{attempt}.log"
                log_handle = log.open("wb")
                command = application.command(config, "serve")
                measured("candidate_validation")
                server = subprocess.Popen(
                    command, cwd=ROOT,
                    stdout=log_handle, stderr=subprocess.STDOUT, start_new_session=True,
                )
                wait_for_server(server, f"http://{app_address}:{app_port}", log)
                measured("process_readiness")
                (run_dir / f"application-{attempt}.json").write_text(
                    json.dumps(application.running_identity(server), indent=2) + "\n"
                )
                measured("running_identity")
                return {"log": str(log), "identity": str(run_dir / f"application-{attempt}.json"),
                        "phase_seconds": phases}

            reset_server()
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
            # Export only the ephemeral CA certificate, never its private key.
            certificate = fixture / "fixture-ca.crt"
            for _ in range(60):
                copied = subprocess.run(
                    [
                        "docker",
                        "cp",
                        f"{proxy_name}:/data/caddy/pki/authorities/local/root.crt",
                        str(certificate),
                    ],
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    check=False,
                )
                if copied.returncode == 0:
                    break
                time.sleep(1)
            else:
                raise RuntimeError("Disposable TLS fixture did not issue its CA")
            wait_for_https(proxy_name, readiness_url, certificate)
            # Chromium's brand-new NSS database is generated by the browser
            # image's own library, never host ~/.pki or a host-version certutil.
            # Firefox imports the same CA by its explicit automation policy.
            nss = fixture / "nssdb"
            nss.mkdir()
            policies = fixture / "policies.json"
            policies.write_text(
                '{"policies":{"Certificates":{"Install":["/tmp/plamenu-fixture-ca.crt"]}}}\n'
            )
            environment = os.environ | {
                "A11Y_BASE_URL": base_url,
                "A11Y_USERNAME": "developer",
                "A11Y_CREATOR_PREFIX": "a11y_",
                "A11Y_PARTICIPANT_PREFIX": PARTICIPANT_PREFIX,
                "A11Y_PASSWORD": "plamenu-development-only",
                "A11Y_CANONICAL_ORIGIN": "https://localhost",
                "A11Y_MAILBOX_URL": mailbox_url,
                "A11Y_REPORT_PATH": str(report_path),
                "A11Y_RUN_DIR": str(run_dir),
                "A11Y_OUTPUT_DIR": str(run_dir / "test-results"),
            }
            if os.environ.get("A11Y_OFFLINE_FIXTURE", "1") == "1":
                control_url, offline_close = offline_proxy_control(
                    app_address, proxy_name, fixture
                )
                environment["A11Y_OFFLINE_CONTROL_URL"] = control_url
            if offline_close:
                cleanup.callback(offline_close)
            control = BrowserFixture(app_address, reset=reset_server, stop=stop_server,
                                     report=run_dir / "fixtures.json", cold_renditions=cold_renditions)
            cleanup.callback(control.close)
            environment["A11Y_FIXTURE_CONTROL_URL"] = control.url
            if podcasts:
                environment["A11Y_PODCAST_SOURCE_URL"] = (
                    f"http://{app_address}:{podcasts.server_port}/show.xml"
                )
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
                    "--name", browser_name,
                    "--label", f"plamenu.fixture.run={run_dir.name}",
                    "--network",
                    f"container:{proxy_name}",
                    "--ipc",
                    "host",
                    "--user",
                    "0:0",
                    "--env",
                    "A11Y_DISPOSABLE_TRUST=1",
                    "--env",
                    f"A11Y_RUN_UID={os.getuid()}",
                    "--env",
                    f"A11Y_RUN_GID={os.getgid()}",
                    "--env",
                    "NODE_EXTRA_CA_CERTS=/tmp/plamenu-fixture-ca.crt",
                    "--volume",
                    f"{certificate}:/tmp/plamenu-fixture-ca.crt:ro",
                    "--volume",
                    f"{nss}:/tmp/.pki/nssdb",
                    "--env",
                    "PLAYWRIGHT_FIREFOX_POLICIES_JSON=/tmp/plamenu-fixture-policies.json",
                    "--volume",
                    f"{policies}:/tmp/plamenu-fixture-policies.json:ro",
                    "--env",
                    "HOME=/tmp",
                    *(
                        ["--env", "DEBUG=pw:browser"]
                        if os.environ.get("A11Y_BROWSER_DEBUG") == "1"
                        else []
                    ),
                    "--env",
                    "A11Y_NULL_AUDIO=1",
                    "--env",
                    "A11Y_VIDEO_FIXTURE=/tmp/native-video.mp4",
                    "--env",
                    "A11Y_AUDIO_FIXTURE=/tmp/native-audio.m4a",
                    "--env",
                    "A11Y_POSTER_FIXTURE=/tmp/native-poster.png",
                    "--env",
                    f"A11Y_LIVE_INGEST_HOST={app_address}",
                    "--env",
                    "A11Y_BROWSER_IMAGE="
                    + subprocess.check_output(
                        [
                            "docker",
                            "image",
                            "inspect",
                            "--format",
                            "{{.Id}}",
                            encoder_image,
                        ],
                        text=True,
                    ).strip(),
                    *(
                        [
                            "--env",
                            f"A11Y_IMPORT_SOURCE_URL={os.environ['A11Y_IMPORT_SOURCE_URL']}",
                        ]
                        if os.environ.get("A11Y_IMPORT_SOURCE_URL")
                        else []
                    ),
                    *(
                        [
                            "--env",
                            f"A11Y_SOURCE_SYNC_URL={os.environ['A11Y_SOURCE_SYNC_URL']}",
                        ]
                        if os.environ.get("A11Y_SOURCE_SYNC_URL")
                        else []
                    ),
                    *(
                        [
                            "--env",
                            f"A11Y_PODCAST_SOURCE_URL={environment['A11Y_PODCAST_SOURCE_URL']}",
                        ]
                        if podcasts
                        else []
                    ),
                    *(
                        [
                            "--env",
                            f"A11Y_OFFLINE_CONTROL_URL={environment['A11Y_OFFLINE_CONTROL_URL']}",
                        ]
                        if "A11Y_OFFLINE_CONTROL_URL" in environment
                        else []
                    ),
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
                            "A11Y_CREATOR_PREFIX",
                            "A11Y_PARTICIPANT_PREFIX",
                            "A11Y_PASSWORD",
                            "A11Y_CANONICAL_ORIGIN",
                            "A11Y_REPORT_PATH",
                            "A11Y_RUN_DIR",
                            "A11Y_OUTPUT_DIR",
                            "A11Y_MAILBOX_URL",
                            "A11Y_FIXTURE_CONTROL_URL",
                        )
                        for item in ("--env", f"{name}={environment[name]}")
                    ),
                    "--volume",
                    f"{ROOT}:{ROOT}",
                    *git_mount,
                    "--workdir",
                    str(ROOT),
                    encoder_image,
                    "bash",
                    str(ROOT / "scripts/accessibility-container.sh"),
                    "npm",
                    "--prefix",
                    "accessibility",
                    "test",
                    "--",
                    *sys.argv[1:],
                ]
            cleanup.callback(remove_container, browser_name, run_dir.name)
            result = subprocess.run(command, cwd=ROOT, env=environment, check=False)
            outcome = result.returncode
        except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
            print(f"Accessibility fixture failed: {error}", file=sys.stderr)
            outcome = 1
    finalize_report(report_path, control.events if 'control' in locals() else [], outcome)
    return outcome



if __name__ == "__main__":
    def interrupted(signum, frame):
        raise KeyboardInterrupt(f"Fixture runner received signal {signum}")

    signal.signal(signal.SIGTERM, interrupted)
    sys.exit(main())
