"""Socket-level guard tests for the test-only fault bridge."""
import asyncio
import json
import unittest
from tracker_fault_proxy import Bridge


class BridgeTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.upstream = await asyncio.start_server(self.origin, "127.0.0.1", 0)
        self.bridge = Bridge(("127.0.0.1", self.upstream.sockets[0].getsockname()[1]), "test-only-token")
        self.proxy = await asyncio.start_server(self.bridge.accept, "127.0.0.1", 0, limit=16385)
        self.admin = await asyncio.start_server(self.bridge.control, "127.0.0.1", 0, limit=4096)

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
                writer.write(b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n")
                await writer.drain()
                while payload := await reader.read(65536):
                    writer.write(payload)
                    await writer.drain()
            else:
                self.assertIn(b"Connection: close", header)
                writer.write(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nmedia")
                await writer.drain()
        finally:
            writer.close()

    async def connect(self, path):
        reader, writer = await asyncio.open_connection("127.0.0.1", self.proxy.sockets[0].getsockname()[1])
        writer.write(f"GET {path} HTTP/1.1\r\nHost: fixture.local\r\nConnection: keep-alive\r\n\r\n".encode())
        await writer.drain()
        return reader, writer

    async def control(self, action, token="test-only-token"):
        reader, writer = await asyncio.open_connection("127.0.0.1", self.admin.sockets[0].getsockname()[1])
        writer.write(f"POST /{action} HTTP/1.1\r\nHost: localhost\r\nX-AP09-Control: {token}\r\n\r\n".encode())
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


if __name__ == "__main__":
    unittest.main()
