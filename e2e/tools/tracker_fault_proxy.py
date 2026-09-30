"""Owned test-only TCP bridge: actually interrupt tracker sockets, not browser routes.

No payload/header logging. Plain HTTP is only the existing local TLS proxy's
backend; canonical HTTPS, real native tracker and media paths stay unchanged.
Non-upgrades close after one request so pooled sockets cannot hide an upgrade.
"""
import argparse
import asyncio
import json
import os
import signal
from pathlib import Path


class Bridge:
    def __init__(self, upstream, token):
        self.upstream = upstream
        self.token = token
        self.blocked = False
        self.trackers = set()
        self.slots = asyncio.Semaphore(256)
        self.metrics = {"tracker_opened": 0, "tracker_interrupted": 0,
                        "tracker_refused": 0, "forwarded_bytes": 0, "connections": 0}

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
            if tracker and self.blocked:
                self.metrics["tracker_refused"] += 1
                return
            if not tracker:
                lines = [line for line in lines[:-2] if not line.lower().startswith((b"connection:", b"keep-alive:"))]
                header = b"\r\n".join([*lines, b"Connection: close", b"", b""])
            upstream_reader, upstream_writer = await asyncio.wait_for(asyncio.open_connection(*self.upstream), 5)
            if tracker:
                self.metrics["tracker_opened"] += 1
                self.trackers.add((writer, upstream_writer))
                registered = True
            upstream_writer.write(header)
            await upstream_writer.drain()
            tasks = [asyncio.create_task(self.pipe(reader, upstream_writer)),
                     asyncio.create_task(self.pipe(upstream_reader, writer))]
            await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
        except (OSError, asyncio.TimeoutError, asyncio.IncompleteReadError, asyncio.LimitOverrunError):
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
            supplied = next((row.partition(":")[2].strip() for row in rows[1:]
                             if row.lower().startswith("x-ap09-control:")), "")
            if supplied != self.token:
                writer.write(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
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
            elif action != "/status":
                return
            body = json.dumps({**self.metrics, "blocked": self.blocked,
                               "active_trackers": len(self.trackers)}).encode()
            writer.write(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: "
                         + str(len(body)).encode() + b"\r\n\r\n" + body)
            await writer.drain()
        except (OSError, UnicodeError, asyncio.TimeoutError, asyncio.IncompleteReadError, asyncio.LimitOverrunError):
            pass
        finally:
            writer.close()


async def main(args):
    bridge = Bridge(("127.0.0.1", args.upstream), os.environ["AP09_PROXY_TOKEN"])
    stopped = asyncio.Event()
    loop = asyncio.get_running_loop()
    for sig in [signal.SIGTERM, signal.SIGINT]:
        loop.add_signal_handler(sig, stopped.set)
    serving = await asyncio.start_server(bridge.accept, "0.0.0.0", args.listen, limit=16385)
    control = await asyncio.start_server(bridge.control, "127.0.0.1", args.control, limit=4096)
    async with serving, control:
        await stopped.wait()
    for client, upstream in list(bridge.trackers):
        client.transport.abort()
        upstream.transport.abort()
    args.result.write_text(json.dumps(bridge.metrics, indent=2))
    args.result.chmod(0o600)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--listen", type=int, required=True)
    parser.add_argument("--upstream", type=int, required=True)
    parser.add_argument("--control", type=int, required=True)
    parser.add_argument("--result", type=Path, required=True)
    asyncio.run(main(parser.parse_args()))
