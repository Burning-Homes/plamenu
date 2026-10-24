"""Source reuse must retain encoder failures, version binding and copy isolation."""

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "e2e"))
from plamenu_e2e import media_inputs


class SampleInputTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.encoder = self.root / "ffmpeg"
        self.log = self.root / "calls.jsonl"
        self.encoder.write_text(
            f"#!{sys.executable}\n"
            "import json,sys\nfrom pathlib import Path\n"
            "root=Path(__file__).parent\n"
            "with (root/'calls.jsonl').open('a') as log:\n"
            " log.write(json.dumps(sys.argv[1:])+'\\n')\n"
            "Path(sys.argv[-1]).write_bytes(b'input-A:'+json.dumps(sys.argv[1:-1]).encode())\n"
            "if (root/'empty').exists(): Path(sys.argv[-1]).write_bytes(b'')\n"
            "if (root/'fail').exists(): sys.exit(9)\n"
            "if (root/'change').exists():\n"
            " path=Path(__file__); path.write_text(path.read_text()+'\\n')\n"
        )
        self.encoder.chmod(0o700)
        self.lookup = patch.object(
            media_inputs.shutil, "which", return_value=str(self.encoder)
        )
        self.lookup.start()
        self.addCleanup(self.lookup.stop)
        self.cache = media_inputs.SampleVideoInputs()

    def calls(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()]

    def make(self, name="clip.mp4", **kwargs):
        return self.cache.make_mp4(self.root / name, **kwargs)

    def test_each_test_gets_an_independent_writable_file(self):
        first = self.make("one.mp4")
        expected = first.read_bytes()
        second = self.make("two.mp4")
        self.assertEqual(first.read_bytes(), second.read_bytes())
        self.assertNotEqual(first.stat().st_ino, second.stat().st_ino)
        first.write_bytes(b"one test edited its input")
        second.unlink()
        third = self.make("three.mp4")
        self.assertEqual(third.read_bytes(), expected)
        self.assertEqual(len(self.calls()), 1)
        self.assertFalse(Path(self.calls()[0][-1]).exists())

    def test_duration_size_and_container_never_share_an_incorrect_source(self):
        ordinary = self.make().read_bytes()
        longer = self.make("longer.mp4", seconds=8).read_bytes()
        larger = self.make("larger.mp4", size="640x480").read_bytes()
        self.assertEqual(len({ordinary, longer, larger}), 3)
        self.assertEqual(self.make("repeat.mp4").read_bytes(), ordinary)
        self.make("alternate.mov")
        self.make("alternate-again.mov")
        self.assertEqual(len(self.calls()), 5)
        self.assertEqual(self.calls()[-1][-1], str(self.root / "alternate-again.mov"))

    def test_missing_or_replaced_encoder_cannot_reuse_cached_bytes(self):
        original = self.make().read_bytes()
        with (
            patch.object(media_inputs.shutil, "which", return_value=None),
            self.assertRaisesRegex(FileNotFoundError, "ffmpeg is required"),
        ):
            self.make("missing.mp4")
        stat = self.encoder.stat()
        # Same-size rewrite and restored mtime must still invalidate the entry.
        self.encoder.write_text(
            self.encoder.read_text().replace("input-A:", "input-B:")
        )
        os.utime(self.encoder, ns=(stat.st_atime_ns, stat.st_mtime_ns))
        replacement = self.make("replacement.mp4").read_bytes()
        self.assertNotEqual(replacement, original)
        self.assertEqual(len(self.calls()), 2)

    def test_encoder_failure_keeps_its_exit_and_cleans_partial_source(self):
        failure = self.root / "fail"
        failure.touch()
        with self.assertRaises(subprocess.CalledProcessError) as caught:
            self.make("failed.mp4")
        self.assertEqual(caught.exception.returncode, 9)
        self.assertFalse((self.root / "failed.mp4").exists())
        self.assertFalse(Path(self.calls()[-1][-1]).exists())
        self.assertFalse(self.cache._entries)
        failure.unlink()
        self.make("recovered.mp4")
        self.assertEqual(len(self.calls()), 2)

    def test_encoder_change_during_generation_fails_without_a_cache_entry(self):
        (self.root / "change").touch()
        with self.assertRaisesRegex(RuntimeError, "encoder changed"):
            self.make()
        self.assertFalse(self.cache._entries)
        self.assertFalse(Path(self.calls()[-1][-1]).exists())

    def test_empty_encoder_output_fails_even_when_it_would_not_be_cached(self):
        (self.root / "empty").touch()
        for byte_limit in (0, 1024):
            with self.subTest(byte_limit=byte_limit):
                self.cache = media_inputs.SampleVideoInputs(max_bytes=byte_limit)
                with self.assertRaisesRegex(RuntimeError, "empty input"):
                    self.make()
                self.assertFalse(self.cache._entries)
                self.assertFalse(Path(self.calls()[-1][-1]).exists())

    def test_entry_limit_evicts_least_recently_used_source(self):
        self.cache = media_inputs.SampleVideoInputs(max_entries=2)
        self.make(seconds=1)
        self.make(seconds=2)
        self.make(seconds=1)  # First entry is now the most recent.
        self.make(seconds=3)
        self.make(seconds=2)  # Evicted entry must run the real encoder again.
        self.assertEqual(len(self.calls()), 4)
        self.assertEqual(len(self.cache._entries), 2)

    def test_total_byte_limit_evicts_and_oversized_sources_are_not_retained(self):
        length = len(self.make(seconds=1).read_bytes())
        self.cache = media_inputs.SampleVideoInputs(max_bytes=length)
        self.make(seconds=2)
        self.make(seconds=3)
        self.make(seconds=2)
        self.assertEqual(len(self.cache._entries), 1)
        self.assertLessEqual(self.cache._bytes, length)
        self.assertEqual(len(self.calls()), 4)
        self.cache = media_inputs.SampleVideoInputs(max_bytes=1)
        for _ in range(2):
            self.assertTrue(self.make().read_bytes())
        self.assertFalse(self.cache._entries)
        self.assertEqual(self.cache._bytes, 0)
        self.assertEqual(len(self.calls()), 6)


if __name__ == "__main__":
    unittest.main()
