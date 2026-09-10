"""Package binaries and matching host-installation files in one archive."""

import copy
import hashlib
import tarfile

BINARIES = ("plamenu",)
INSTALL_FILES = {
    "plamenu.service": "deploy/plamenu.service",
    "plamenu.host.toml.example": "deploy/plamenu.host.toml.example",
    "Caddyfile.host.example": "deploy/Caddyfile.host.example",
    "LICENSE": "LICENSE",
}


def create_binary_archive(destination, binary_dir, source_tar):
    with (
        tarfile.open(destination, "w:gz") as archive,
        tarfile.open(source_tar) as source,
    ):
        for name in BINARIES:
            archive.add(binary_dir / name, arcname=name)
        for name, source_name in INSTALL_FILES.items():
            member = copy.copy(source.getmember(source_name))
            if not member.isfile():
                raise ValueError(
                    "Installation file must be a regular file: " + source_name
                )
            with source.extractfile(member) as stream:
                member.name = name
                member.mode = 0o644
                member.pax_headers = {}
                archive.addfile(member, stream)


def verify_binary_archive(binary_tar, source_tar, binary_evidence):
    with tarfile.open(binary_tar) as archive, tarfile.open(source_tar) as source:
        members = archive.getmembers()
        if sorted(m.name for m in members) != sorted((*BINARIES, *INSTALL_FILES)):
            raise ValueError("Unexpected binary archive members")
        for member in members:
            limit = 250 * 1024**2 if member.name in BINARIES else 1024**2
            if not member.isfile() or member.size > limit:
                raise ValueError("Invalid binary archive member: " + member.name)
            with archive.extractfile(member) as stream:
                digest = hashlib.file_digest(stream, "sha256").hexdigest()
            if member.name in BINARIES:
                if not member.mode & 0o111 or binary_evidence[member.name] != {
                    "sha256": digest,
                    "bytes": member.size,
                }:
                    raise ValueError("Executable differs from build evidence")
            else:
                original = source.getmember(INSTALL_FILES[member.name])
                if not original.isfile() or original.size != member.size:
                    raise ValueError("Installation file differs from source")
                with source.extractfile(original) as stream:
                    expected = hashlib.file_digest(stream, "sha256").hexdigest()
                if digest != expected:
                    raise ValueError("Installation file differs from source")
