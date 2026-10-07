"""Real synchronous tracker client with an ordered TLS opening write.

websockets' threaded client starts reading before its HTTP opening request is
written. With the local Python 3.14 / TLS 1.3 stack, that overlap can stall an
opening whose HTTP request never reaches Caddy's native upstream. Keep the
library's protocol, TLS settings, timeouts and subsequent frame handling; start
its receiver after the opening write completes. Failures still propagate once.
"""

import threading

from websockets.sync.client import ClientConnection
from websockets.sync.client import connect as websocket_connect


class OpeningWriteFirstConnection(ClientConnection):
    def __init__(self, *args, **kwargs):
        self._opening_write_finished = threading.Event()
        super().__init__(*args, **kwargs)

    def recv_events(self):
        self._opening_write_finished.wait()
        super().recv_events()

    def send_data(self):
        try:
            super().send_data()
        finally:
            # A failed write must also release the receiver for SDK cleanup.
            self._opening_write_finished.set()

    def close_socket(self):
        # Request construction can fail before send_data is called.
        self._opening_write_finished.set()
        super().close_socket()


def connect(uri, **kwargs):
    return websocket_connect(
        uri, create_connection=OpeningWriteFirstConnection, **kwargs
    )
