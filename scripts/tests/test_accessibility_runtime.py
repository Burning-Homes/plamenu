import hashlib
import importlib.util
import json
import os
import smtplib
import socket
import subprocess
import tempfile
import time
import unittest
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET
from pathlib import Path
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    "accessibility_runtime",
    Path(__file__).resolve().parents[1] / "run-accessibility-tests.py",
)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class ContainerCleanupTests(unittest.TestCase):
    def test_cleanup_requires_run_label_and_uses_immutable_container_id(self):
        with patch.object(MODULE.subprocess, "check_output", return_value="owned-id\n") as listing, patch.object(MODULE.subprocess, "run", return_value=subprocess.CompletedProcess([], 0)) as remove:
            MODULE.remove_container("fixture-name", "run-token")
        self.assertIn("label=plamenu.fixture.run=run-token", listing.call_args.args[0])
        self.assertEqual(remove.call_args.args[0], ["docker", "rm", "--force", "--volumes", "owned-id"])
        with patch.object(MODULE.subprocess, "check_output", return_value=""), patch.object(MODULE.subprocess, "run") as remove:
            MODULE.remove_container("foreign-name", "run-token")
            remove.assert_not_called()

    def test_auto_removal_race_is_ok_but_surviving_container_is_an_error(self):
        failed = subprocess.CompletedProcess([], 1, stderr="failed removal")
        with patch.object(MODULE.subprocess, "check_output", side_effect=["owned-id\n", ""]), patch.object(MODULE.subprocess, "run", return_value=failed):
            MODULE.remove_container("fixture-name", "run-token")
        with patch.object(MODULE.subprocess, "check_output", return_value="owned-id\n"), patch.object(MODULE.subprocess, "run", return_value=failed), self.assertRaisesRegex(RuntimeError, "cleanup failed"):
            MODULE.remove_container("fixture-name", "run-token")


class MailboxFixtureTests(unittest.TestCase):
    def test_reset_drains_incomplete_smtp_before_clearing_previous_attempt(self):
        port, url, close = MODULE.fixture_mailbox("127.0.0.1")
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=5) as stalled:
                stalled.recv(4096)
                stalled.sendall(b"DATA\r\n")
                stalled.recv(4096)
                stalled.sendall(b"To: reader@example.test\r\nSubject: stale\r\n\r\nstale\r\n")
                close.reset()
            with urllib.request.urlopen(url + "?recipient=reader%40example.test") as response:
                self.assertEqual(json.load(response), [])
            with smtplib.SMTP("127.0.0.1", port, timeout=5) as smtp:
                smtp.sendmail("sender@example.test", ["reader@example.test"],
                              "To: reader@example.test\r\nSubject: new\r\n\r\nnew\r\n")
            with urllib.request.urlopen(url + "?recipient=reader%40example.test") as response:
                self.assertEqual([m['subject'] for m in json.load(response)], ['new'])
        finally:
            close()

    def test_real_smtp_delivery_recipient_isolation_and_nonce(self):
        port, url, close = MODULE.fixture_mailbox("127.0.0.1")
        try:
            with smtplib.SMTP("127.0.0.1", port, timeout=5) as smtp:
                smtp.sendmail("sender@example.test", ["reader@example.test"],
                              "To: reader@example.test\r\nSubject: Test\r\n\r\nA local reset link.\r\n")
            with urllib.request.urlopen(url + "?recipient=reader%40example.test") as response:
                messages = json.load(response)
                self.assertEqual(response.headers["Cache-Control"], "no-store")
            self.assertEqual(len(messages), 1)
            self.assertIn("A local reset link.", messages[0]["body"])
            with urllib.request.urlopen(url + "?recipient=other%40example.test") as response:
                self.assertEqual(json.load(response), [])
            try:
                urllib.request.urlopen(url.rsplit("/", 1)[0] + "/wrong")
                self.fail("Mailbox accepted a wrong nonce")
            except urllib.error.HTTPError as error:
                self.assertEqual(error.code, 403)
                error.close()
        finally:
            close()


class ColdRenditionFixtureTests(unittest.TestCase):
    def test_ordinary_encoders_are_not_paced_and_cold_option_only_gates_renditions(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            encoder = root / "ffmpeg"
            encoder.write_text("#!/usr/bin/python3\nimport json,sys\nprint(json.dumps(sys.argv[1:]))\n")
            encoder.chmod(0o700)
            control = MODULE.ColdRenditions(root / "control")
            with patch.object(MODULE.shutil, "which", return_value=str(encoder)):
                wrapper = MODULE.rendition_encoder(root, control)
            for output in ("encoded.m4a", "encoded.mp3", "upload.mp3", "poster.jpg"):
                actual = json.loads(subprocess.check_output([str(wrapper),"-y","-i","source.wav",output],text=True))
                expected = ["-y", "-i", "source.wav", output]
                self.assertEqual(actual, expected)
            control.begin("cold-case", True)
            # An input with that name cannot slow an ordinary upload encoder.
            actual = json.loads(subprocess.check_output([str(wrapper),"-i","encoded.mp3","upload.mp3"],text=True))
            self.assertEqual(actual, ["-i", "encoded.mp3", "upload.mp3"])
            control.end()


class OfflineProxyFixtureTests(unittest.TestCase):
    def test_participant_handles_fit_the_registered_local_mention_limit(self):
        for project in MODULE.BROWSER_PROJECTS:
            username = MODULE.PARTICIPANT_PREFIX + project.replace("-", "_")
            self.assertLessEqual(len(username), 30)

    def test_independent_fixture_roles_have_bounded_distinct_clients(self):
        configuration = MODULE.tls_configuration("127.0.0.1", 8000, 443)
        mappings = [line.split() for line in configuration.splitlines()
                    if line.strip().startswith(tuple(MODULE.BROWSER_PROJECTS))]
        self.assertEqual(len(mappings), len(MODULE.BROWSER_PROJECTS) * 5)
        self.assertEqual(len({address for _, address in mappings}), len(mappings))
        self.assertIn("default 198.18.0.100", configuration)
        self.assertIn("header_up X-Forwarded-For {fixture_client_ip}", configuration)

    def test_session_runtime_origins_use_the_same_ephemeral_trusted_ca(self):
        configuration = MODULE.tls_configuration("127.0.0.1", 8000, 443)
        self.assertIn("https://localhost:443, https://*.webxdc.localhost:443", configuration)
        self.assertIn("tls internal", configuration)
        self.assertIn("reverse_proxy http://127.0.0.1:8000", configuration)

    def test_default_proxy_has_no_admin_listener_and_opt_in_is_loopback_only(self):
        self.assertIn("admin off", MODULE.tls_configuration("127.0.0.1", 8000, 443))
        self.assertIn("admin 127.0.0.1:2019", MODULE.tls_configuration("127.0.0.1", 8000, 443, offline=True))

    def test_nonce_and_exact_process_identity_fence_listener_changes_and_restore(self):
        with tempfile.TemporaryDirectory() as folder:
            fixture = Path(folder)
            original = {"apps": {"http": {"servers": {"srv0": {"listen": [":443"]}}}, "tls": {"retained": True}}}
            active = json.loads(json.dumps(original))
            identity = {"Id": "owned-container", "Name": "/plamenu-a11y-tls-owned", "Image": "owned-image", "State": {"Pid": 123}, "RestartCount": 0, "Mounts": [{"Source": str(fixture / "Caddyfile"), "Destination": "/etc/caddy/Caddyfile"}]}

            def output(command, **kwargs):
                return json.dumps([identity] if command[1] == "inspect" else active)

            def reload(command, **kwargs):
                nonlocal active
                self.assertEqual(command[3], "owned-container")
                active = json.loads(kwargs["input"])

            with patch.object(MODULE.subprocess, "check_output", side_effect=output), patch.object(MODULE.subprocess, "run", side_effect=reload) as mutate:
                url, close = MODULE.offline_proxy_control("127.0.0.1", "plamenu-a11y-tls-owned", fixture)
                try:
                    wrong = urllib.request.Request(url.rsplit("/", 1)[0] + "/wrong/offline", method="POST")
                    with self.assertRaises(urllib.error.HTTPError) as rejected:
                        urllib.request.urlopen(wrong, timeout=5)
                    self.assertEqual(rejected.exception.code, 403)
                    rejected.exception.close()
                    mutate.assert_not_called()
                    request = urllib.request.Request(url + "/offline", method="POST")
                    with urllib.request.urlopen(request, timeout=5) as response:
                        self.assertEqual(response.status, 204)
                    self.assertEqual(active["apps"]["http"]["servers"], {})
                    self.assertEqual(active["apps"]["tls"], original["apps"]["tls"])
                    identity["State"]["Pid"] = 456
                    with self.assertRaises(urllib.error.HTTPError) as rejected:
                        urllib.request.urlopen(urllib.request.Request(url + "/online", method="POST"), timeout=5)
                    self.assertEqual(rejected.exception.code, 500)
                    rejected.exception.close()
                    self.assertEqual(mutate.call_count, 1)
                    identity["State"]["Pid"] = 123
                finally:
                    close()
                self.assertEqual(active, original)


class ApplicationBindingTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.binary = self.root / "candidate"
        subprocess.run(
            ["cc", "-x", "c", "-", "-o", str(self.binary)],
            input=(
                "#include <unistd.h>\n#include <stdio.h>\n#include <string.h>\n"
                'int main(int argc, char **argv) { if (argc == 4 && !strcmp(argv[3], "serve"))'
                " { for (;;) pause(); } puts(argv[0]); return 0; }\n"
            ),
            text=True,
            check=True,
            capture_output=True,
        )

    def test_selected_candidate_binds_real_cli_and_running_server_without_cargo(self):
        with (
            patch.dict(os.environ, {"PLAMENU_BINARY": str(self.binary)}),
            patch.object(MODULE, "run") as build,
        ):
            application = MODULE.prepare_application()
            build.assert_not_called()
        config = self.root / "fixture.toml"
        cli = subprocess.check_output(
            application.command(config, "role", "list"), text=True
        )
        self.assertEqual(cli.strip(), str(self.binary))
        process = subprocess.Popen(application.command(config, "serve"))
        try:
            deadline = time.monotonic() + 5
            while Path(f"/proc/{process.pid}/exe").resolve() != self.binary:
                self.assertLess(time.monotonic(), deadline)
                time.sleep(0.005)
            identity = application.running_identity(process)
            self.assertEqual(identity["pid"], process.pid)
            self.assertEqual(
                identity["sha256"], hashlib.sha256(self.binary.read_bytes()).hexdigest()
            )
            self.assertEqual(identity["executable"], str(self.binary))
        finally:
            process.terminate()
            process.wait(timeout=5)

    def test_drift_is_refused_before_another_fixture_command(self):
        application = MODULE.ApplicationBinary(self.binary)
        replacement = self.root / "replacement"
        replacement.write_text("#!/bin/sh\nexit 0\n")
        replacement.chmod(0o700)
        replacement.replace(self.binary)
        with self.assertRaisesRegex(RuntimeError, "changed"):
            application.command(self.root / "fixture.toml", "serve")

    def test_equal_size_write_with_restored_mtime_is_not_a_cached_pass(self):
        application = MODULE.ApplicationBinary(self.binary)
        original = self.binary.stat()
        with self.binary.open("r+b") as stream:
            stream.seek(-1, os.SEEK_END)
            byte = stream.read(1)
            stream.seek(-1, os.SEEK_END)
            stream.write(bytes([byte[0] ^ 1]))
        os.utime(self.binary, ns=(original.st_atime_ns, original.st_mtime_ns))
        self.assertEqual(self.binary.stat().st_size, original.st_size)
        self.assertEqual(self.binary.stat().st_mtime_ns, original.st_mtime_ns)
        with self.assertRaisesRegex(RuntimeError, "changed"):
            application.command(self.root / "fixture.toml", "serve")

    def test_unchanged_commands_do_not_read_the_entire_binary_again(self):
        with patch.object(MODULE.hashlib, "file_digest", wraps=hashlib.file_digest) as digest:
            application = MODULE.ApplicationBinary(self.binary)
            for _ in range(3):
                application.command(self.root / "fixture.toml", "serve")
        self.assertEqual(digest.call_count, 1)

    def test_invalid_explicit_candidate_never_builds_or_falls_back(self):
        for value in ["relative-candidate", str(self.root / "missing")]:
            with (
                self.subTest(value=value),
                patch.dict(os.environ, {"PLAMENU_BINARY": value}),
                patch.object(MODULE, "run") as build,
                self.assertRaises((ValueError, RuntimeError)),
            ):
                MODULE.prepare_application()
            build.assert_not_called()

    def test_default_builds_once_then_binds_the_built_bytes(self):
        default = self.root / "target/debug/plamenu"
        default.parent.mkdir(parents=True)
        default.write_bytes(self.binary.read_bytes())
        default.chmod(0o700)
        with (
            patch.dict(os.environ, {"PLAMENU_BINARY": ""}),
            patch.object(MODULE, "ROOT", self.root),
            patch.object(MODULE, "run") as build,
        ):
            application = MODULE.prepare_application()
            self.assertEqual(application.path, default)
            application.command(self.root / "config", "account", "add", "fixture")
            application.command(self.root / "config", "serve")
        build.assert_called_once_with("cargo", "build", "--locked", "-q")


class HostBrowserPreflightTests(unittest.TestCase):
    def test_missing_sink_is_actionable_before_fixture_creation(self):
        with (
            patch.object(MODULE.sys, "platform", "linux"),
            patch.object(
                MODULE.shutil, "which", return_value="/usr/bin/gst-inspect-1.0"
            ),
            patch.object(
                MODULE.subprocess,
                "run",
                return_value=subprocess.CompletedProcess([], 1),
            ),
            self.assertRaisesRegex(RuntimeError, "default pinned browser image"),
        ):
            MODULE.local_browser_preflight()

    def test_missing_inspector_is_actionable(self):
        with (
            patch.object(MODULE.sys, "platform", "linux"),
            patch.object(MODULE.shutil, "which", return_value=None),
            self.assertRaisesRegex(RuntimeError, "test dependencies"),
        ):
            MODULE.local_browser_preflight()

    def test_available_sink_passes(self):
        with (
            patch.object(MODULE.sys, "platform", "linux"),
            patch.object(
                MODULE.shutil, "which", return_value="/usr/bin/gst-inspect-1.0"
            ),
            patch.object(
                MODULE.subprocess,
                "run",
                return_value=subprocess.CompletedProcess([], 0),
            ),
        ):
            MODULE.local_browser_preflight()

    def test_other_platform_does_not_require_linux_plugins(self):
        with (
            patch.object(MODULE.sys, "platform", "darwin"),
            patch.object(MODULE.shutil, "which") as lookup,
        ):
            MODULE.local_browser_preflight()
            lookup.assert_not_called()


class PodcastProducerTests(unittest.TestCase):
    def test_rss_escapes_metadata_and_serves_its_real_enclosures(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            audio = root / "audio.m4a"
            poster = root / "cover.png"
            audio.write_bytes(b"fixture-audio")
            poster.write_bytes(b"fixture-image")
            server = MODULE.podcast_producer("127.0.0.1", audio, poster)
            try:
                base = f"http://127.0.0.1:{server.server_port}"
                title = 'Show <&> "name"'
                with urllib.request.urlopen(
                    base + "/show.xml?" + urllib.parse.urlencode({"title": title}),
                    timeout=2,
                ) as response:
                    self.assertEqual(
                        response.headers["Content-Type"], "application/rss+xml"
                    )
                    feed = ET.fromstring(response.read())
                self.assertEqual(feed.findtext("channel/title"), title)
                episodes = feed.findall("channel/item")
                self.assertEqual(len(episodes), 2)
                for episode in episodes:
                    enclosure = episode.find("enclosure")
                    with urllib.request.urlopen(
                        enclosure.attrib["url"], timeout=2
                    ) as response:
                        self.assertEqual(response.headers["Content-Type"], "audio/mp4")
                        self.assertEqual(response.read(), audio.read_bytes())
                image = feed.find(
                    "channel/{http://www.itunes.com/dtds/podcast-1.0.dtd}image"
                )
                with urllib.request.urlopen(
                    image.attrib["href"], timeout=2
                ) as response:
                    self.assertEqual(response.read(), poster.read_bytes())
            finally:
                server.shutdown()
                server.server_close()


if __name__ == "__main__":
    unittest.main()
