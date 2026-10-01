"""Verify real, recoverable I/O faults on a uniquely owned FUSE mount."""
import errno
import os
from pathlib import Path
import subprocess
import tempfile
import time
import unittest


class PrimaryIoFaultTest(unittest.TestCase):
    def test_open_writer_receives_enospc_and_eio_without_filling_real_disk(self):
        repository = Path(__file__).resolve().parents[2]
        parent = repository/"target/e2e/primary-io-fault"
        parent.mkdir(parents=True,exist_ok=True)
        root = Path(tempfile.mkdtemp(prefix="owned-",dir=parent))
        root.chmod(0o700)
        backing, mount = root/"backing", root/"mount"
        backing.mkdir(mode=0o700)
        mount.mkdir(mode=0o700)
        control, binary = root/"fault", root/"primary-io-fault"
        flags = subprocess.check_output(["pkg-config","--cflags","--libs","fuse3"],text=True).split()
        subprocess.run(["cc","-Wall","-Wextra","-Werror","-O2",str(repository/"e2e/tools/primary_io_fault.c"),*flags,"-o",str(binary)],check=True,timeout=30)
        mounted = False
        descriptor = None
        with (root/"filesystem.log").open("w") as output:
            process = subprocess.Popen([str(binary),str(backing),str(control),str(mount)],stdout=output,stderr=subprocess.STDOUT)
            try:
                deadline = time.monotonic()+10
                while time.monotonic() < deadline:
                    mounted = f" {mount} " in Path("/proc/self/mountinfo").read_text()
                    if mounted: break
                    self.assertIsNone(process.poll(),"owned FUSE fixture exited; inspect its protected log")
                    time.sleep(0.1)
                self.assertTrue(mounted,"owned mount not established")
                descriptor = os.open(mount/"writer.bin",os.O_CREAT|os.O_WRONLY,0o600)
                self.assertEqual(os.write(descriptor,b"before"),6)
                control.write_bytes(b"ENOSPC")
                with self.assertRaises(OSError) as result: os.write(descriptor,b"lost")
                self.assertEqual(result.exception.errno,errno.ENOSPC)
                self.assertEqual(os.statvfs(mount).f_bavail,0)
                control.write_bytes(b"EIO")
                with self.assertRaises(OSError) as result: os.write(descriptor,b"lost")
                self.assertEqual(result.exception.errno,errno.EIO)
                with self.assertRaises(OSError) as result: os.stat(mount/"writer.bin")
                self.assertEqual(result.exception.errno,errno.EIO)
                control.unlink()
                self.assertEqual(os.write(descriptor,b"after"),5)
                os.fsync(descriptor)
                os.close(descriptor)
                descriptor = None
                self.assertEqual((backing/"writer.bin").read_bytes(),b"beforeafter")
            finally:
                if control.exists(): control.unlink()
                if descriptor is not None: os.close(descriptor)
                if mounted:
                    detached = subprocess.run(["fusermount3","-u",str(mount)],capture_output=True,timeout=10)
                    if detached.returncode:
                        subprocess.run(["fusermount3","-u","-z",str(mount)],check=True,capture_output=True,timeout=10)
                if process.poll() is None: process.terminate()
                process.wait(timeout=10)
                for path in root.rglob("*"):
                    if path.is_file(): path.chmod(0o600)


if __name__ == "__main__": unittest.main()
