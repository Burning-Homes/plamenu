import hashlib
import importlib.util
import json
import os
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


class ColdRenditionFixtureTests(unittest.TestCase):
    def test_realtime_encoder_is_confined_to_rendition_output(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            encoder = root / "ffmpeg"
            encoder.write_text("#!/usr/bin/python3\nimport json,sys\nprint(json.dumps(sys.argv[1:]))\n")
            encoder.chmod(0o700)
            with patch.object(MODULE.shutil, "which", return_value=str(encoder)):
                wrapper = MODULE.rendition_encoder(root)
            for output, realtime in (("encoded.m4a",True),("encoded.mp3",True),("upload.mp3",False),("poster.jpg",False)):
                actual = json.loads(subprocess.check_output([str(wrapper),"-y","-i","source.wav",output],text=True))
                expected = ["-y", *(["-re"] if realtime else []), "-i", "source.wav", output]
                self.assertEqual(actual, expected)
            # An input with that name cannot slow an ordinary upload encoder.
            actual = json.loads(subprocess.check_output([str(wrapper),"-i","encoded.mp3","upload.mp3"],text=True))
            self.assertEqual(actual, ["-i", "encoded.mp3", "upload.mp3"])


class OfflineProxyFixtureTests(unittest.TestCase):
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
