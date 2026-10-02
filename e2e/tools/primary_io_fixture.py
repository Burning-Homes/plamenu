"""An isolated real filesystem fault; never touch a standing mount/root."""

import subprocess
import time
from pathlib import Path


class PrimaryFilesystem:
    def __init__(self, root):
        self.root = root
        root.mkdir(mode=0o700)
        self.backing, self.mount = root / "backing", root / "mount"
        self.backing.mkdir(mode=0o700)
        self.mount.mkdir(mode=0o700)
        self.control, self.binary = root / "fault", root / "primary-io-fault"
        repository = Path(__file__).resolve().parents[2]
        flags = subprocess.check_output(
            ["pkg-config", "--cflags", "--libs", "fuse3"], text=True
        ).split()
        subprocess.run(
            [
                "cc",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-O2",
                str(repository / "e2e/tools/primary_io_fault.c"),
                *flags,
                "-o",
                str(self.binary),
            ],
            check=True,
            timeout=30,
        )
        self.log = (root / "filesystem.log").open("w")
        self.process = subprocess.Popen(
            [str(self.binary), str(self.backing), str(self.control), str(self.mount)],
            stdout=self.log,
            stderr=subprocess.STDOUT,
        )
        try:
            deadline = time.monotonic() + 10
            while not self.mounted():
                if self.process.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError(
                        "owned primary filesystem did not mount; inspect its private log"
                    )
                time.sleep(0.1)
        except BaseException:
            self.stop()
            raise

    def mounted(self):
        return f" {self.mount} " in Path("/proc/self/mountinfo").read_text()

    def set_fault(self, kind):
        assert kind in ("EIO", "ENOSPC", None)
        if kind is None:
            self.control.unlink(missing_ok=True)
        else:
            self.control.write_text(kind)

    def stop(self):
        self.set_fault(None)
        if self.mounted():
            result = subprocess.run(
                ["fusermount3", "-u", str(self.mount)],
                capture_output=True,
                timeout=10,
                check=False,
            )
            if result.returncode:
                subprocess.run(
                    ["fusermount3", "-u", "-z", str(self.mount)],
                    check=True,
                    capture_output=True,
                    timeout=10,
                )
        if self.process.poll() is None:
            self.process.terminate()
        self.process.wait(timeout=10)
        self.log.close()
