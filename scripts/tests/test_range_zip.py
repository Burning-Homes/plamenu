"""Range recovery refuses ignored, shifted, truncated and oversized responses."""

import io
import re
import runpy
import unittest
from pathlib import Path
from types import SimpleNamespace

RangeReader = runpy.run_path(
    str(Path(__file__).resolve().parents[1] / "ci/range_zip.py")
)["RangeReader"]


class Response(io.BytesIO):
    def __init__(self, data, status, content_range):
        super().__init__(data)
        self.status = status
        self.headers = {"Content-Range": content_range}


class RangeTests(unittest.TestCase):
    def reader(self, *, status=206, shifted=False, truncate=False, limit=10):
        self.requests = []

        def open_request(request, timeout):
            start, end = map(
                int,
                re.fullmatch(
                    r"bytes=(\d+)-(\d+)", request.get_header("Range")
                ).groups(),
            )
            self.requests.append((start, end))
            data = b"0123456789"[start : end + 1]
            if truncate and end > start:
                data = data[:-1]
            return Response(data, status, f"bytes {start + int(shifted)}-{end}/10")

        return RangeReader(
            SimpleNamespace(open=open_request),
            "https://codefloe.com/example",
            {},
            limit,
        )

    def test_seeks_without_fetching_omitted_bytes(self):
        with self.reader() as reader:
            reader.seek(7)
            self.assertEqual(reader.read(2), b"78")
            reader.seek(-2, io.SEEK_END)
            self.assertEqual(reader.read(), b"89")
        self.assertEqual(self.requests, [(0, 0), (7, 8), (9, 9)])

    def test_rejects_ignored_range(self):
        with self.assertRaisesRegex(ValueError, "exact byte ranges"):
            self.reader(status=200)

    def test_rejects_shifted_range(self):
        with self.assertRaisesRegex(ValueError, "exact byte ranges"):
            self.reader(shifted=True)

    def test_rejects_oversized_artifact(self):
        with self.assertRaisesRegex(ValueError, "size limit"):
            self.reader(limit=9)

    def test_rejects_truncated_range(self):
        with (
            self.reader(truncate=True) as reader,
            self.assertRaisesRegex(ValueError, "Truncated"),
        ):
            reader.read(5)

    def test_rejects_out_of_bounds_seek(self):
        with self.reader() as reader, self.assertRaisesRegex(ValueError, "outside"):
            reader.seek(11)
