"""Real subprocess/pipe barriers; codec behavior is covered by browser journeys."""

import json
import os
import subprocess
import sys
import tempfile
import time
import unittest
import wave
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from cold_rendition_fixture import ColdRenditions


class ColdBarrierTests(unittest.TestCase):
    def exercise(self, release):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source.wav"
            pcm = bytes(range(256)) * 4000
            with wave.open(str(source), "wb") as output:
                output.setparams((1, 2, 16000, 0, "NONE", "not compressed"))
                output.writeframes(pcm)
            # Copy streaming input, flushing the prefix before EOF. This makes
            # the barrier observable without requiring FFmpeg in portable tests.
            encoder = root / "encoder"
            encoder.write_text(
                f"#!{sys.executable}\nimport os,sys\n"
                "assert sys.argv[sys.argv.index('-protocol_whitelist')+1]=='file'\n"
                "source=sys.argv[sys.argv.index('-i')+1]\n"
                "assert not source.startswith('pipe:')\n"
                "with open(source,'rb',buffering=0) as source, open(sys.argv[-1], 'wb', buffering=0) as output:\n"
                " while chunk := source.read(65536): output.write(chunk)\n"
            )
            encoder.chmod(0o700)
            control = ColdRenditions(root / "control", timeout=5 if release else 1)
            control.begin("first", True)
            destination = root / "encoded.mp3"
            child = subprocess.Popen(
                [
                    sys.executable,
                    "-c",
                    "from cold_rendition_fixture import encode; import sys; encode(*sys.argv[1:3],sys.argv[3:])",
                    str(encoder),
                    str(control.active),
                    "-protocol_whitelist",
                    "file",
                    "-i",
                    str(source),
                    str(destination),
                ],
                env=os.environ
                | {"PYTHONPATH": str(Path(__file__).resolve().parents[1])},
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            try:
                record_path = control.attempt / "encoder.json"
                deadline = time.monotonic() + 5
                while not record_path.exists():
                    self.assertIsNone(child.poll())
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(0.01)
                record = json.loads(record_path.read_text())
                self.assertEqual(record["state"], "held")
                self.assertEqual(record["held_frames"], 32000)
                self.assertIsNone(child.poll())
                self.assertLess(destination.stat().st_size, source.stat().st_size)
                with self.assertRaisesRegex(RuntimeError, "matching fixture owner"):
                    control.release("another-attempt")
                if release:
                    control.release("first")
                    with self.assertRaises(RuntimeError):
                        control.release("first")
                _, error = child.communicate(timeout=6)
                self.assertEqual(child.returncode == 0, release, error.decode())
                record = control.end()
                self.assertEqual(record["state"], "complete" if release else "failed")
                if release:
                    with wave.open(str(destination), "rb") as decoded:
                        self.assertEqual(decoded.readframes(decoded.getnframes()), pcm)
                else:
                    self.assertIn("never released", record["error"])
                self.assertFalse(control.active.exists())
                control.begin("ordinary-next-attempt", False)
                self.assertFalse(control.active.exists())
                control.end()
            finally:
                if child.poll() is None:
                    child.terminate()
                child.communicate(timeout=5)

    def test_real_input_is_incomplete_until_matching_browser_releases_it(self):
        self.exercise(True)

    def test_missing_browser_release_fails_and_reaps_the_encoder(self):
        self.exercise(False)
