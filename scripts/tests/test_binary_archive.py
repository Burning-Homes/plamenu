"""Release archives include verified binaries and matching installation files."""

import io
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
from ci.binary_archive import create_binary_archive, verify_binary_archive
from plamenu_release.core import digest


class BinaryArchiveTests(unittest.TestCase):
    def test_binary_archive_includes_matching_installation_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = root / "source.tar"
            install_files = {
                "plamenu.service": "deploy/plamenu.service",
                "plamenu.host.toml.example": "deploy/plamenu.host.toml.example",
                "Caddyfile.host.example": "deploy/Caddyfile.host.example",
                "LICENSE": "LICENSE",
            }
            with tarfile.open(source, "w") as archive:
                for name in install_files.values():
                    archive.add(ROOT / name, arcname=name)
            binaries = {}
            for name in ("plamenu",):
                path = root / name
                path.write_bytes(b"binary fixture: " + name.encode())
                path.chmod(0o755)
                binaries[name] = {
                    "sha256": digest(path),
                    "bytes": path.stat().st_size,
                }
            candidate = root / "binaries.tar.gz"
            create_binary_archive(candidate, root, source)
            verify_binary_archive(candidate, source, binaries)
            unpacked = root / "unpacked"
            unpacked.mkdir()
            subprocess.run(
                ["tar", "-xzf", str(candidate), "-C", str(unpacked)], check=True
            )
            self.assertEqual(
                {p.name for p in unpacked.iterdir()},
                set(install_files) | set(binaries),
            )
            for name, original in install_files.items():
                self.assertEqual(
                    (unpacked / name).read_bytes(), (ROOT / original).read_bytes()
                )
            self.assertTrue((unpacked / "plamenu").stat().st_mode & 0o111)

            for fault in (
                "missing",
                "duplicate",
                "config",
                "binary",
                "symlink",
                "mode",
            ):
                with self.subTest(fault=fault):
                    damaged = root / "damaged.tar.gz"
                    with (
                        tarfile.open(candidate) as original,
                        tarfile.open(damaged, "w:gz") as archive,
                    ):
                        for member in original.getmembers():
                            data = original.extractfile(member).read()
                            if member.name == "plamenu.service":
                                if fault == "missing":
                                    continue
                                if fault == "duplicate":
                                    archive.addfile(member, io.BytesIO(data))
                                if fault == "config":
                                    data = b"X" + data[1:]
                                if fault == "symlink":
                                    member.type = tarfile.SYMTYPE
                                    member.linkname = "/etc/passwd"
                                    member.size = 0
                            if member.name == "plamenu":
                                if fault == "binary":
                                    data = b"X" + data[1:]
                                if fault == "mode":
                                    member.mode = 0o644
                            archive.addfile(member, io.BytesIO(data))
                    with self.assertRaises(ValueError):
                        verify_binary_archive(damaged, source, binaries)
