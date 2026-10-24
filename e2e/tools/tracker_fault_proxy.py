"""Owned test-only TCP bridge: actually interrupt tracker sockets, not browser routes.

No payload/header logging. Plain HTTP is only the existing local TLS proxy's
backend; canonical HTTPS, real native tracker and media paths stay unchanged.
Non-upgrades close after one request so pooled sockets cannot hide an upgrade.
"""

import argparse
import asyncio
import ipaddress
import json
import os
import signal
from pathlib import Path


class Bridge:
    def __init__(
        self, upstream, token, bandwidth=0, latency_ms=0, tracker_upstream=None
    ):
        self.upstream = upstream
        self.tracker_upstream = tracker_upstream
        self.token = token
        self.blocked = False
        self.trackers = set()
        self.slots = asyncio.Semaphore(256)
        self.bandwidth = bandwidth
        self.latency_ms = latency_ms
        self.constrained = False
        self.next_delivery = 0
        self.delivery_lock = asyncio.Lock()
        self.metrics = {
            "tracker_opened": 0,
            "tracker_interrupted": 0,
            "tracker_refused": 0,
            "forwarded_bytes": 0,
            "connections": 0,
            "origin_media_body_bytes": 0,
            "origin_media_responses": 0,
        }

    async def downstream(self, payload, writer):
        if self.constrained and self.bandwidth:
            async with self.delivery_lock:
                now = asyncio.get_running_loop().time()
                self.next_delivery = (
                    max(now, self.next_delivery) + len(payload) / self.bandwidth
                )
                due = self.next_delivery
            await asyncio.sleep(max(0, due - asyncio.get_running_loop().time()))
        self.metrics["forwarded_bytes"] += len(payload)
        writer.write(payload)
        await asyncio.wait_for(writer.drain(), 30)

    async def response(self, reader, writer, media):
        header = await asyncio.wait_for(reader.readuntil(b"\r\n\r\n"), 10)
        if len(header) > 16384:
            return
        if self.constrained:
            await asyncio.sleep(self.latency_ms / 1000)
        await self.downstream(header, writer)
        rows = header.lower().split(b"\r\n")
        chunked = any(
            row.startswith(b"transfer-encoding:") and b"chunked" in row for row in rows
        )
        status = int(rows[0].split()[1])
        measured = media and 200 <= status < 300
        if measured:
            self.metrics["origin_media_responses"] += 1
        counter = ChunkCounter() if chunked else None
        while payload := await reader.read(16384):
            if measured:
                self.metrics["origin_media_body_bytes"] += (
                    counter.feed(payload) if counter else len(payload)
                )
            await self.downstream(payload, writer)

    async def pipe(self, reader, writer):
        while payload := await reader.read(65536):
            self.metrics["forwarded_bytes"] += len(payload)
            writer.write(payload)
            await asyncio.wait_for(writer.drain(), 30)

    async def accept(self, reader, writer):
        upstream_writer = None
        tasks = []
        registered = False
        admitted = False
        try:
            await asyncio.wait_for(self.slots.acquire(), 2)
            admitted = True
            self.metrics["connections"] += 1
            header = await asyncio.wait_for(reader.readuntil(b"\r\n\r\n"), 10)
            if len(header) > 16384:
                return
            lines = header.split(b"\r\n")
            words = lines[0].split()
            tracker = len(words) == 3 and words[1].split(b"?")[0] == b"/tracker/socket"
            media = (
                len(words) == 3
                and words[0] == b"GET"
                and words[1].split(b"?")[0].startswith(b"/media/")
            )
            if tracker and self.blocked:
                self.metrics["tracker_refused"] += 1
                return
            if not tracker:
                lines = [
                    line
                    for line in lines[:-2]
                    if not line.lower().startswith((b"connection:", b"keep-alive:"))
                ]
                header = b"\r\n".join([*lines, b"Connection: close", b"", b""])
            if self.constrained:
                await asyncio.sleep(self.latency_ms / 1000)
            upstream = (
                self.tracker_upstream
                if tracker and self.tracker_upstream
                else self.upstream
            )
            if tracker and self.tracker_upstream:
                # Only the owned browser's tracker connection is endpoint-
                # mapped through this bridge. Its real PeerTube tracker,
                # infohashes and signaling payloads remain untouched.
                lines = [
                    line
                    for line in header.split(b"\r\n")[:-2]
                    if not line.lower().startswith(b"host:")
                ]
                header = b"\r\n".join([*lines, b"Host: peertube.local", b"", b""])
            upstream_reader, upstream_writer = await asyncio.wait_for(
                asyncio.open_connection(*upstream), 5
            )
            if tracker:
                self.metrics["tracker_opened"] += 1
                self.trackers.add((writer, upstream_writer))
                registered = True
            upstream_writer.write(header)
            await upstream_writer.drain()
            tasks = [
                asyncio.create_task(self.pipe(reader, upstream_writer)),
                asyncio.create_task(
                    self.pipe(upstream_reader, writer)
                    if tracker
                    else self.response(upstream_reader, writer, media)
                ),
            ]
            await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
        except (
            TimeoutError,
            OSError,
            asyncio.IncompleteReadError,
            asyncio.LimitOverrunError,
        ):
            pass
        finally:
            if registered:
                self.trackers.discard((writer, upstream_writer))
            for task in tasks:
                task.cancel()
            if tasks:
                await asyncio.gather(*tasks, return_exceptions=True)
            for stream in [writer, upstream_writer]:
                if stream:
                    stream.close()
                    try:
                        await stream.wait_closed()
                    except (OSError, asyncio.CancelledError):
                        pass
            if admitted:
                self.slots.release()

    async def control(self, reader, writer):
        try:
            header = await asyncio.wait_for(reader.readuntil(b"\r\n\r\n"), 5)
            rows = header.decode("ascii").split("\r\n")
            supplied = next(
                (
                    row.partition(":")[2].strip()
                    for row in rows[1:]
                    if row.lower().startswith("x-ap09-control:")
                ),
                "",
            )
            if supplied != self.token:
                writer.write(
                    b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                await writer.drain()
                return
            action = rows[0].split()[1]
            if action == "/block":
                self.blocked = True
                for client, upstream in list(self.trackers):
                    self.metrics["tracker_interrupted"] += 1
                    client.transport.abort()
                    upstream.transport.abort()
            elif action == "/restore":
                self.blocked = False
            elif action == "/constrain":
                self.constrained = True
                self.next_delivery = asyncio.get_running_loop().time()
            elif action == "/unconstrain":
                self.constrained = False
            elif action != "/status":
                return
            body = json.dumps(
                {
                    **self.metrics,
                    "blocked": self.blocked,
                    "active_trackers": len(self.trackers),
                    "constrained": self.constrained,
                    "downstream_bytes_per_second": self.bandwidth
                    if self.constrained
                    else 0,
                    "one_way_header_delay_ms": self.latency_ms
                    if self.constrained
                    else 0,
                }
            ).encode()
            writer.write(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: "
                + str(len(body)).encode()
                + b"\r\n\r\n"
                + body
            )
            await writer.drain()
        except (
            TimeoutError,
            OSError,
            UnicodeError,
            asyncio.IncompleteReadError,
            asyncio.LimitOverrunError,
        ):
            pass
        finally:
            writer.close()


class ChunkCounter:
    """Bounded incremental HTTP/1 chunk parser: count payload, not framing."""

    def __init__(self):
        self.line = bytearray()
        self.remaining = 0
        self.delimiter = 0
        self.ended = False

    def feed(self, payload):
        view = memoryview(payload)
        counted = 0
        while view and not self.ended:
            if self.remaining:
                size = min(self.remaining, len(view))
                counted += size
                self.remaining -= size
                view = view[size:]
                if not self.remaining:
                    self.delimiter = 2
            elif self.delimiter:
                expected = b"\r\n"[2 - self.delimiter]
                if view[0] != expected:
                    raise ValueError("invalid upstream chunk delimiter")
                view = view[1:]
                self.delimiter -= 1
            else:
                self.line.append(view[0])
                view = view[1:]
                if len(self.line) > 1024:
                    raise ValueError("upstream chunk header exceeds fixture bound")
                if self.line.endswith(b"\r\n"):
                    size = self.line[:-2].split(b";", 1)[0]
                    if (
                        not size
                        or len(size) > 16
                        or any(value not in b"0123456789abcdefABCDEF" for value in size)
                    ):
                        raise ValueError("invalid upstream chunk length")
                    self.remaining = int(size, 16)
                    self.line.clear()
                    if not self.remaining:
                        self.ended = True
        return counted


async def main(args):
    bridge = Bridge(
        ("127.0.0.1", args.upstream),
        os.environ["AP09_PROXY_TOKEN"],
        args.bandwidth,
        args.latency_ms,
        tracker_target(args.tracker_host, args.tracker_port),
    )
    stopped = asyncio.Event()
    loop = asyncio.get_running_loop()
    for sig in [signal.SIGTERM, signal.SIGINT]:
        loop.add_signal_handler(sig, stopped.set)
    serving = await asyncio.start_server(
        bridge.accept, "0.0.0.0", args.listen, limit=16385
    )
    control = await asyncio.start_server(
        bridge.control, "127.0.0.1", args.control, limit=4096
    )
    async with serving, control:
        await stopped.wait()
    for client, upstream in list(bridge.trackers):
        client.transport.abort()
        upstream.transport.abort()
    args.result.write_text(json.dumps(bridge.metrics, indent=2))
    args.result.chmod(0o600)


def tracker_target(host, port):
    if host is None:
        return None
    address = ipaddress.ip_address(host)
    if not (address.is_private or address.is_loopback) or not 1 <= port <= 65535:
        raise ValueError(
            "owned tracker backend must be a literal private/loopback address and valid port"
        )
    return str(address), port


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--listen", type=int, required=True)
    parser.add_argument("--upstream", type=int, required=True)
    parser.add_argument("--control", type=int, required=True)
    parser.add_argument("--result", type=Path, required=True)
    parser.add_argument(
        "--bandwidth",
        type=int,
        default=0,
        help="shared downstream wire bytes/s when constrained",
    )
    parser.add_argument(
        "--latency-ms",
        type=int,
        default=0,
        help="each direction's header delay when constrained",
    )
    parser.add_argument(
        "--tracker-host",
        help="owned PeerTube container's literal private address; tracker path only",
    )
    parser.add_argument("--tracker-port", type=int, default=9000)
    asyncio.run(main(parser.parse_args()))
