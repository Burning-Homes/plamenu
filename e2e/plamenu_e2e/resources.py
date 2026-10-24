"""Linux resource sampling for owned media fixtures, including process restarts."""

import re
import threading
import time
from pathlib import Path


class Resources:
    """Sample only the disposable instance and its own encoder children."""

    def __init__(self, instance, *, connect):
        self.instance = instance
        self.connect = connect
        self.parent_peak_kib = self.child_peak_kib = self.disk_peak = 0
        self.jobs_peak = self.uploaders_peak = 0
        self.stop = threading.Event()
        self.thread = threading.Thread(target=self.sample, daemon=True)
        self.error = None

    def sample(self):
        try:
            with self.connect(self.instance.dsn, autocommit=True) as connection:
                self.sample_process(connection)
        except Exception as error:  # noqa: BLE001 -- thread boundary; raised by __exit__
            self.error = error

    def sample_process(self, connection):
        while not self.stop.is_set():
            try:
                # A crash drill deliberately has no process between reaping
                # the old child and launching its replacement. Snapshot the
                # handle once so that gap cannot kill the sampling thread.
                child = self.instance._proc
                if child is None or child.poll() is not None:
                    self.stop.wait(0.05)
                    continue
                process = child.pid
                status = Path(f"/proc/{process}/status").read_text()
                rss = int(re.search(r"VmRSS:\s+(\d+)", status)[1])
                self.parent_peak_kib = max(self.parent_peak_kib, rss)
                # Tokio may fork from any worker thread; the leader's children
                # file alone would falsely report zero encoder memory.
                children = set()
                for task in Path(f"/proc/{process}/task").glob("*/children"):
                    try:
                        children.update(task.read_text().split())
                    except FileNotFoundError:
                        pass
                child_rss = 0
                for child in children:
                    try:
                        value = re.search(
                            r"VmRSS:\s+(\d+)", Path(f"/proc/{child}/status").read_text()
                        )
                        child_rss += int(value[1]) if value else 0
                    except FileNotFoundError:
                        pass
                self.child_peak_kib = max(self.child_peak_kib, child_rss)
                root = Path(self.instance._dir.name) / "media"
                self.disk_peak = max(
                    self.disk_peak,
                    sum(p.stat().st_size for p in root.rglob("*") if p.is_file()),
                )
                jobs, uploaders = connection.execute(
                    "SELECT count(*),count(DISTINCT account_id) FROM video_drafts WHERE state='processing' AND lease_until>now()"
                ).fetchone()
                self.jobs_peak = max(self.jobs_peak, jobs)
                self.uploaders_peak = max(self.uploaders_peak, uploaders)
            except (FileNotFoundError, TypeError):
                pass  # A deliberately killed process/renamed scratch is transient.
            self.stop.wait(0.05)

    def __enter__(self):
        self.sample_once = int(
            re.search(
                r"VmRSS:\s+(\d+)",
                Path(f"/proc/{self.instance._proc.pid}/status").read_text(),
            )[1]
        )
        self.started = time.monotonic()
        self.thread.start()
        return self

    def __exit__(self, exc_type, *_):
        self.stop.set()
        self.thread.join(timeout=3)
        if exc_type is None:
            if self.thread.is_alive():
                raise RuntimeError("resource sampler did not stop")
            if self.error is not None:
                raise RuntimeError("resource sampler failed") from self.error
