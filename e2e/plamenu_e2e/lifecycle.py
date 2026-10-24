"""Ownership primitives for serial, disposable test fixtures (stdlib only)."""

import fcntl
import hashlib
import os
import signal
import subprocess
import tempfile
import threading
from pathlib import Path


def serve_in_thread(server):
    """Own a fixture listener and join both its accept loop and requests."""
    server.daemon_threads = False
    thread = threading.Thread(
        target=lambda: server.serve_forever(poll_interval=0.05), daemon=True
    )
    try:
        thread.start()
    except BaseException:
        server.server_close()
        raise

    def close():
        try:
            server.shutdown()
        finally:
            try:
                server.server_close()
            finally:
                thread.join(timeout=5)
                if thread.is_alive():
                    raise RuntimeError("Fixture listener did not stop")

    return close


def owns_listener(process, port):
    """Readiness belongs to this child, never a coincidental healthy service."""
    if process is None or process.poll() is not None:
        return False
    proc = Path(f"/proc/{process.pid}")
    sockets = set()
    for descriptor in (proc / "fd").glob("*"):
        try:
            target = descriptor.readlink().as_posix()
        except OSError:
            continue
        if target.startswith("socket:[") and target.endswith("]"):
            sockets.add(target[8:-1])
    for protocol in ("tcp", "tcp6"):
        try:
            rows = (proc / "net" / protocol).read_text().splitlines()[1:]
        except OSError:
            continue
        for row in rows:
            fields = row.split()
            if (
                len(fields) > 9
                and fields[3] == "0A"
                and int(fields[1].rsplit(":", 1)[1], 16) == port
                and fields[9] in sockets
            ):
                return True
    return False


class Lease:
    """Fail before mutation if another local harness owns this resource.

    Locks live outside the checkout, so separate worktrees cannot overlap.
    Never unlink a lock file: waiters must continue to share the same inode.
    """

    def __init__(self, resource):
        directory = Path(tempfile.gettempdir()) / f"plamenu-fixture-locks-{os.getuid()}"
        directory.mkdir(mode=0o700, exist_ok=True)
        name = hashlib.sha256(resource.encode()).hexdigest()
        self.file = (directory / name).open("a+")
        try:
            fcntl.flock(self.file, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            self.file.close()
            raise RuntimeError(f"Fixture resource already owned: {resource}") from error

    def close(self):
        self.file.close()

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.close()


def stop_process(process, timeout=2):
    """Reap a child and its encoder descendants, including after parent exit.

    Callers must create this exact child with start_new_session=True. Its PID
    is also the owned process-group ID; never target a standing server. These
    fixtures are discarded after assertions; graceful-shutdown drills own
    their separate wait/assertion and must not use this teardown as evidence.
    """
    if process is None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        pass
    # The parent may exit before its descendants. Do not let an encoder keep
    # writing into media that is about to be restored or removed.
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.wait(timeout=timeout)
