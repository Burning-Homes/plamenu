"""A nonce-protected, serial lifecycle for whole browser test attempts."""

import http.server
import json
import secrets
import threading
import time


def validate_lifecycle(record):
    """Full evidence must pair every executed case with its owned teardown."""
    events = record.get("fixture_lifecycle")
    if not isinstance(events, list) or len(events) % 2:
        raise ValueError("Incomplete browser fixture lifecycle")
    completed = set()
    selected = set(record["selected"])
    for begin, end in zip(events[::2], events[1::2], strict=True):
        case = begin.get("case", "")
        nodeid, retry, nonce = case.split(":")
        if (
            begin.get("action") != "begin"
            or end.get("action") != "end"
            or end.get("case") != case
            or begin.get("passed") is not True
            or end.get("passed") is not True
            or retry != "0"
            or not nonce
            or nodeid not in selected
            or nodeid in completed
        ):
            raise ValueError("Invalid browser fixture ownership or teardown")
        completed.add(nodeid)
        if begin.get("cold_rendition"):
            encoder = (end.get("resource") or {}).get("cold_encoder", {})
            if encoder.get("state") != "complete" or not encoder.get("held_frames"):
                raise ValueError("Cold browser case lacks completed encoder barrier evidence")
    executed = {
        test["id"]
        for test in record["tests"]
        if any(attempt["status"] == "passed" for attempt in test["attempts"])
    }
    if not executed <= completed:
        raise ValueError("Browser case lacks fixture teardown evidence")


def finalize_report(path, events, returncode):
    """Only called after every runner cleanup has succeeded."""
    if not path.exists():
        return
    record = json.loads(path.read_text())
    if record.get("fixture_cleanup_pending") is not True:
        return
    record["fixture_cleanup_pending"] = False
    record["fixture_cleanup_complete"] = True
    record["fixture_lifecycle"] = events
    record["full_pass"] = (
        record.pop("fixture_tests_pass", False) is True and returncode == 0
    )
    if record["full_pass"]:
        validate_lifecycle(record)
    if record["status"] == "pending-cleanup":
        record["status"] = "passed" if record["full_pass"] else "failed"
    temporary = path.with_suffix(".cleanup.tmp")
    with temporary.open("x") as output:
        temporary.chmod(0o600)
        json.dump(record, output, indent=2)
        output.write("\n")
    temporary.replace(path)


class BrowserFixture:
    def __init__(self, address, *, reset, stop, report, cold_renditions=None):
        self.reset = reset
        self.stop = stop
        self.report = report
        self.cold_renditions = cold_renditions
        self.active = None
        self.failed = False
        self.events = []
        self.lock = threading.Lock()
        token = secrets.token_urlsafe(32)
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def setup(self):
                super().setup()
                self.connection.settimeout(15)

            def do_POST(self):
                if self.path not in (f"/{token}/begin", f"/{token}/end", f"/{token}/release-encoder"):
                    self.send_error(403)
                    return
                try:
                    size = int(self.headers.get("Content-Length", "0"))
                    if not 0 < size < 4096:
                        raise ValueError("Invalid request size")
                    body = json.loads(self.rfile.read(size))
                    case = body["case"]
                    if not isinstance(case, str) or not case or len(case) > 2048:
                        raise ValueError("Invalid case")
                    cold = body.get("cold_rendition", False)
                    if not isinstance(cold, bool):
                        raise ValueError("Invalid cold rendition option")
                    owner.transition(self.path.rsplit("/", 1)[1], case, cold=cold)
                except Exception as error:  # noqa: BLE001 -- HTTP boundary; transition records failure
                    self.send_error(409, str(error))
                    return
                self.send_response(204)
                self.end_headers()

            def log_message(self, *args):
                pass

        self.server = http.server.HTTPServer((address, 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.url = f"http://{address}:{self.server.server_port}/{token}"

    def transition(self, action, case, *, cold=False):
        with self.lock:
            if self.failed:
                raise RuntimeError("Fixture lifecycle failed; start a fresh run")
            if action == "begin" and self.active is not None:
                raise RuntimeError(
                    "Browser fixture already owned; parallelism is disabled"
                )
            if action in ("end", "release-encoder") and self.active != case:
                raise RuntimeError("Browser fixture owner mismatch")
            if action == "release-encoder":
                if self.cold_renditions is None:
                    raise RuntimeError("Cold encoder fixture is not configured")
                self.cold_renditions.release(case)
                return
            if cold and self.cold_renditions is None:
                raise RuntimeError("Cold encoder fixture is required for this case")
            started = time.monotonic()
            resource = None
            try:
                if action == "begin":
                    self.active = case
                    if self.cold_renditions:
                        self.cold_renditions.begin(case, cold)
                    resource = self.reset()
                else:
                    self.stop()
                    if self.cold_renditions:
                        resource = {"cold_encoder": self.cold_renditions.end()}
                    self.active = None
            except BaseException:
                self.failed = True
                raise
            finally:
                self.events.append(
                    {
                        "case": case,
                        "action": action,
                        "seconds": time.monotonic() - started,
                        "passed": not self.failed,
                        "resource": resource,
                        **({"cold_rendition": cold} if action == "begin" else {}),
                    }
                )
                self.report.write_text(json.dumps(self.events, indent=2) + "\n")

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)
        self.stop()
        if self.active is not None or self.failed:
            raise RuntimeError("Browser fixture ended without successful teardown")
