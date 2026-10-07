"""Actual WebSocket opening/cleanup contracts, without Caddy or a database."""

import socket
import sys
import threading
import time
import unittest
from pathlib import Path

from websockets.exceptions import ConnectionClosed, WebSocketException
from websockets.server import ServerProtocol
from websockets.sync.server import ServerConnection

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "e2e"))
from plamenu_e2e import tracker


class OpeningSocket:
    def __init__(self, raw, *, fail_write=False):
        self.raw = raw
        self.fail_write = fail_write
        self.write_finished = threading.Event()
        self.read_before_write = False
        self.send_calls = 0

    def __getattr__(self, name):
        return getattr(self.raw, name)

    def sendall(self, data):
        self.send_calls += 1
        if self.fail_write:
            raise OSError("owned opening write failure")
        self.raw.sendall(data)
        self.write_finished.set()

    def recv(self, *args):
        if not self.write_finished.is_set():
            self.read_before_write = True
        return self.raw.recv(*args)


class TrackerOpeningTests(unittest.TestCase):
    def fixture(self, *, fail_write=False):
        listener = socket.socket()
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        self.addCleanup(listener.close)
        errors = []

        def serve():
            raw, _ = listener.accept()
            connection = ServerConnection(raw, ServerProtocol(), close_timeout=0.5)
            try:
                connection.handshake(timeout=1)
                connection.send(connection.recv(timeout=1))
            except (ConnectionClosed, EOFError):
                pass
            except (OSError, WebSocketException) as error:
                errors.append(error)
            finally:
                connection.close_socket()
                connection.recv_events_thread.join(timeout=1)

        worker = threading.Thread(target=serve, daemon=True)
        worker.start()
        raw = socket.create_connection(listener.getsockname(), timeout=1)
        raw.settimeout(None)
        wrapped = OpeningSocket(raw, fail_write=fail_write)
        self.addCleanup(wrapped.close)
        self.addCleanup(worker.join, 2)
        return wrapped, worker, errors

    def test_real_opening_echo_and_receiver_follow_completed_first_write(self):
        wrapped, worker, errors = self.fixture()
        with tracker.connect(
            "ws://owned.local/tracker/socket",
            sock=wrapped,
            open_timeout=1,
            close_timeout=0.5,
        ) as connection:
            self.assertEqual(connection.response.status_code, 101)
            connection.send("actual text frame")
            self.assertEqual(connection.recv(timeout=1), "actual text frame")
        worker.join(timeout=2)
        self.assertFalse(worker.is_alive())
        self.assertFalse(errors)
        self.assertFalse(wrapped.read_before_write)
        self.assertFalse(connection.recv_events_thread.is_alive())

    def test_failed_opening_write_releases_receiver_without_retry(self):
        wrapped, worker, _ = self.fixture(fail_write=True)
        began = time.monotonic()
        with self.assertRaises(ConnectionClosed):
            tracker.connect(
                "ws://owned.local/tracker/socket",
                sock=wrapped,
                open_timeout=1,
                close_timeout=0.5,
            )
        self.assertLess(time.monotonic() - began, 1)
        self.assertEqual(wrapped.send_calls, 1)
        worker.join(timeout=2)
        self.assertFalse(worker.is_alive())

    def test_invalid_opening_headers_release_receiver_before_any_write(self):
        wrapped, worker, _ = self.fixture()
        began = time.monotonic()
        with (
            self.assertLogs("websockets.client", level="ERROR"),
            self.assertRaises(ConnectionClosed) as raised,
        ):
            tracker.connect(
                "ws://owned.local/tracker/socket",
                sock=wrapped,
                additional_headers=object(),
                open_timeout=1,
                close_timeout=0.5,
            )
        self.assertLess(time.monotonic() - began, 1)
        self.assertIsInstance(raised.exception.__cause__, TypeError)
        self.assertFalse(wrapped.write_finished.is_set())
        worker.join(timeout=2)
        self.assertFalse(worker.is_alive())


if __name__ == "__main__":
    unittest.main()
