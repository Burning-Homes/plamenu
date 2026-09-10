"""Bounded, authenticated HTTP range reader for selected ZIP members."""

import io
import re
import urllib.request


class RangeReader(io.RawIOBase):
    def __init__(self, opener, url, headers, limit):
        self.opener, self.url, self.headers = opener, url, headers
        self.position = 0
        self.cache_start, self.cache = 0, b""
        request = urllib.request.Request(url, headers={**headers, "Range": "bytes=0-0"})
        with opener.open(request, timeout=30) as response:
            match = re.fullmatch(
                r"bytes 0-0/(\d+)", response.headers.get("Content-Range", "")
            )
            if response.status != 206 or not match or response.read(1) == b"":
                raise ValueError("Artifact server does not support exact byte ranges")
            self.size = int(match[1])
        if not 0 < self.size <= limit:
            raise ValueError("Artifact exceeds download size limit")

    def seekable(self):
        return True

    def readable(self):
        return True

    def tell(self):
        return self.position

    def seek(self, offset, whence=io.SEEK_SET):
        if whence not in (io.SEEK_SET, io.SEEK_CUR, io.SEEK_END):
            raise ValueError("Invalid seek mode")
        position = offset + (
            self.position
            if whence == io.SEEK_CUR
            else self.size
            if whence == io.SEEK_END
            else 0
        )
        if not 0 <= position <= self.size:
            raise ValueError("Seek outside artifact")
        self.position = position
        return position

    def read(self, count=-1):
        remaining = min(count if count >= 0 else self.size, self.size - self.position)
        parts = []
        while remaining:
            offset = self.position - self.cache_start
            if not 0 <= offset < len(self.cache):
                start = self.position
                # Do not fetch unrelated bytes beyond this request. A selected
                # member must not depend on availability of an omitted member.
                end = min(self.size, start + min(remaining, 4 * 1024 * 1024)) - 1
                request = urllib.request.Request(
                    self.url, headers={**self.headers, "Range": f"bytes={start}-{end}"}
                )
                with self.opener.open(request, timeout=120) as response:
                    if (
                        response.status != 206
                        or response.headers.get("Content-Range")
                        != f"bytes {start}-{end}/{self.size}"
                    ):
                        raise ValueError("Artifact range response differs from request")
                    self.cache = response.read(end - start + 1)
                if len(self.cache) != end - start + 1:
                    raise ValueError("Truncated artifact range")
                self.cache_start, offset = start, 0
            chunk = self.cache[offset : offset + remaining]
            parts.append(chunk)
            self.position += len(chunk)
            remaining -= len(chunk)
        return b"".join(parts)
