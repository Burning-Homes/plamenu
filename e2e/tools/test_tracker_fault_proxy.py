import asyncio
import json
import unittest

try:
    from .tracker_fault_proxy import Bridge, ChunkCounter, tracker_target
except ImportError:
    from tracker_fault_proxy import Bridge, ChunkCounter, tracker_target


class BridgeTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.upstream = await asyncio.start_server(self.origin, "127.0.0.1", 0)
        self.bridge = Bridge(
            ("127.0.0.1", self.upstream.sockets[0].getsockname()[1]), "test-only-token"
        )
        self.proxy = await asyncio.start_server(
            self.bridge.accept, "127.0.0.1", 0, limit=16385
        )
        self.admin = await asyncio.start_server(
            self.bridge.control, "127.0.0.1", 0, limit=4096
        )

    async def asyncTearDown(self):
        for client, upstream in list(self.bridge.trackers):
            client.transport.abort()
            upstream.transport.abort()
        for server in [self.proxy, self.admin, self.upstream]:
            server.close()
            await server.wait_closed()

    async def origin(self, reader, writer):
        try:
            header = await reader.readuntil(b"\r\n\r\n")
            if header.startswith(b"GET /tracker/socket"):
                writer.write(
                    b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n"
                )
                await writer.drain()
                while payload := await reader.read(65536):
                    writer.write(payload)
                    await writer.drain()
            else:
                self.assertIn(b"Connection: close", header)
                writer.write(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nmedia"
                )
                await writer.drain()
        finally:
            writer.close()

    async def connect(self, path):
        reader, writer = await asyncio.open_connection(
            "127.0.0.1", self.proxy.sockets[0].getsockname()[1]
        )
        writer.write(
            f"GET {path} HTTP/1.1\r\nHost: fixture.local\r\nConnection: keep-alive\r\n\r\n".encode()
        )
        await writer.drain()
        return reader, writer

    async def control(self, action, token="test-only-token"):
        reader, writer = await asyncio.open_connection(
            "127.0.0.1", self.admin.sockets[0].getsockname()[1]
        )
        writer.write(
            f"POST /{action} HTTP/1.1\r\nHost: localhost\r\nX-AP09-Control: {token}\r\n\r\n".encode()
        )
        await writer.drain()
        result = await reader.read()
        writer.close()
        await writer.wait_closed()
        return result

    async def test_actual_tracker_interruption_preserves_media_and_reconnects(self):
        reader, writer = await self.connect("/tracker/socket")
        self.assertIn(b"101 Switching Protocols", await reader.readuntil(b"\r\n\r\n"))
        writer.write(b"actual peer bytes")
        await writer.drain()
        self.assertEqual(await reader.readexactly(17), b"actual peer bytes")
        reply = await self.control("block")
        body = json.loads(reply.split(b"\r\n\r\n")[1])
        self.assertTrue(body["blocked"])
        self.assertEqual(body["tracker_interrupted"], 1)
        self.assertEqual(await asyncio.wait_for(reader.read(), 2), b"")
        writer.close()
        media, media_writer = await self.connect("/media/hls/1/index.m3u8")
        self.assertTrue((await media.read()).endswith(b"media"))
        media_writer.close()
        refused, refused_writer = await self.connect("/tracker/socket")
        self.assertEqual(await refused.read(), b"")
        refused_writer.close()
        await self.control("restore")
        restored, restored_writer = await self.connect("/tracker/socket")
        self.assertIn(b"101 Switching Protocols", await restored.readuntil(b"\r\n\r\n"))
        restored_writer.close()

    async def test_unauthorized_control_cannot_change_fault_state(self):
        self.assertIn(b"403 Forbidden", await self.control("block", "wrong"))
        self.assertFalse(self.bridge.blocked)

    async def test_separate_real_tracker_backend_is_scoped_to_tracker_path(self):
        seen = []

        async def tracker(reader, writer):
            header = await reader.readuntil(b"\r\n\r\n")
            seen.append(header)
            self.assertIn(b"Host: peertube.local\r\n", header)
            writer.write(
                b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\n\r\n"
            )
            await writer.drain()
            try:
                while payload := await reader.read(65536):
                    writer.write(payload)
                    await writer.drain()
            finally:
                writer.close()

        server = await asyncio.start_server(tracker, "127.0.0.1", 0)
        async with server:
            self.bridge.tracker_upstream = (
                "127.0.0.1",
                server.sockets[0].getsockname()[1],
            )
            reader, writer = await self.connect("/tracker/socket")
            self.assertIn(b"101", await reader.readuntil(b"\r\n\r\n"))
            writer.write(b"unchanged real wire")
            await writer.drain()
            self.assertEqual(await reader.readexactly(19), b"unchanged real wire")
            await self.control("block")
            self.assertEqual(await asyncio.wait_for(reader.read(), 2), b"")
            writer.close()
            media, media_writer = await self.connect("/media/owned.mp4")
            self.assertTrue((await media.read()).endswith(b"media"))
            media_writer.close()
            self.assertEqual(len(seen), 1)


class CounterTest(unittest.TestCase):
    def test_tracker_target_cannot_be_a_public_host_or_url(self):
        self.assertIsNone(tracker_target(None, 9000))
        self.assertEqual(tracker_target("127.0.0.1", 9000), ("127.0.0.1", 9000))
        for host, port in [
            ("8.8.8.8", 9000),
            ("peertube.local", 9000),
            ("http://127.0.0.1", 9000),
            ("127.0.0.1", 0),
        ]:
            with self.assertRaises(ValueError):
                tracker_target(host, port)

    def test_split_chunk_payload_not_framing(self):
        source = (
            b"3;extension=yes\r\nabc\r\n5\r\ndefgh\r\n0\r\nTrailer: ignored\r\n\r\n"
        )
        for width in (1, 2, 7, 4096):
            counter = ChunkCounter()
            self.assertEqual(
                sum(
                    counter.feed(source[n : n + width])
                    for n in range(0, len(source), width)
                ),
                8,
            )
            self.assertTrue(counter.ended)

    def test_invalid_framing_is_not_accepted(self):
        for source in (b"nope\r\n", b"-1\r\n", b"1\r\naX", b"a" * 1025):
            with self.assertRaises(ValueError):
                ChunkCounter().feed(source)


class ResponseTest(unittest.IsolatedAsyncioTestCase):
    async def test_real_wire_body_counts_and_shared_pacing(self):
        bridge = Bridge(("127.0.0.1", 1), "private", bandwidth=2000)
        bridge.constrained = True

        async def serve(reader, writer):
            request = await reader.readuntil(b"\r\n\r\n")
            self.assertEqual(request, b"GET / HTTP/1.1\r\n\r\n")
            upstream = asyncio.StreamReader()
            upstream.feed_data(
                b"HTTP/1.1 206 Partial Content\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n"
            )
            upstream.feed_eof()
            await bridge.response(upstream, writer, True)
            writer.close()
            await writer.wait_closed()

        server = await asyncio.start_server(serve, "127.0.0.1", 0)
        async with server:
            began = asyncio.get_running_loop().time()

            async def client():
                reader, writer = await asyncio.open_connection(
                    "127.0.0.1", server.sockets[0].getsockname()[1]
                )
                writer.write(b"GET / HTTP/1.1\r\n\r\n")
                await writer.drain()
                response = await reader.read()
                writer.close()
                await writer.wait_closed()
                self.assertIn(b"abc", response)
                return len(response)

            sizes = await asyncio.gather(client(), client())
            self.assertGreaterEqual(
                asyncio.get_running_loop().time() - began, sum(sizes) / 2000
            )
            self.assertEqual(bridge.metrics["origin_media_body_bytes"], 6)
            self.assertEqual(bridge.metrics["origin_media_responses"], 2)


if __name__ == "__main__":
    unittest.main()
