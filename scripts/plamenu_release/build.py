"""Build and test one architecture, including its installation archive and image."""

import gzip
import hashlib
import json
import shutil
import tarfile
import tempfile
from pathlib import Path

from ci.binary_archive import create_binary_archive, verify_binary_archive

from .core import ReleaseError, digest, fingerprint, write_json

TARGETS = {
    "amd64": "x86_64-unknown-linux-musl",
    "arm64": "aarch64-unknown-linux-musl",
}


def binary_name(version, arch):
    return f"plamenu-{version}-linux-{arch}.tar.gz"


def source_name(version):
    return f"plamenu-{version}-source.tar.gz"


def source_tree(path):
    # Git/gzip versions may encode the same archive differently. Compare its
    # files and modes rather than requiring identical compressed bytes.
    records = []
    with tarfile.open(path) as archive:
        for member in archive:
            if member.isfile():
                with archive.extractfile(member) as stream:
                    content = hashlib.file_digest(stream, "sha256").hexdigest()
            elif member.isdir() or member.issym():
                content = member.linkname
            else:
                raise ReleaseError("Unexpected source archive member")
            records.append((member.name, member.mode, member.type.decode(), content))
    if len({r[0] for r in records}) != len(records):
        raise ReleaseError("Duplicate source archive member")
    return fingerprint(sorted(records))


def verify_elf(binary, arch):
    with binary.open("rb") as stream:
        header = stream.read(20)
    if (
        len(header) != 20
        or header[:6] != b"\x7fELF\x02\x01"
        or int.from_bytes(header[18:20], "little") != {"amd64": 62, "arm64": 183}[arch]
    ):
        raise ReleaseError(f"Executable is not a Linux {arch} ELF binary")


def runtime_dockerfile(text):
    stages = [line for line in text.splitlines() if line.startswith("FROM alpine:")]
    if len(stages) != 1:
        raise ReleaseError("Expected one maintained Alpine runtime stage")
    runtime = text[text.index(stages[0]) :]
    original = "COPY --from=builder /out/plamenu /usr/local/bin/plamenu"
    if runtime.count(original) != 1:
        raise ReleaseError("Review the runtime binary COPY before releasing")
    return "# syntax=docker/dockerfile:1.7\n" + runtime.replace(
        original, "COPY plamenu /usr/local/bin/plamenu"
    )


def build(runner, folder, identity):
    arch = identity["arch"]
    if identity["target"] != TARGETS[arch]:
        raise ReleaseError("Build target does not match its architecture")
    raw = folder / "raw"
    runner.run(
        "docker",
        "buildx",
        "build",
        "--platform",
        "linux/" + arch,
        "--file",
        "release/Dockerfile.build",
        "--target",
        "output",
        "--output",
        f"type=local,dest={raw}",
        "--build-arg",
        f"PLAMENU_VERSION={identity['version']}",
        "--build-arg",
        f"PLAMENU_GIT_SHA={identity['source']}",
        "--build-arg",
        f"PLAMENU_BUILD_NUMBER={identity['build_number']}",
        "--build-arg",
        f"PLAMENU_BUILD_JOBS={identity['jobs']}",
        ".",
    )
    binaries = {
        name: {"sha256": digest(raw / name), "bytes": (raw / name).stat().st_size}
        for name in ("plamenu",)
    }
    verify_elf(raw / "plamenu", arch)
    source = folder / source_name(identity["version"])
    runner.run(
        "git", "archive", "--format=tar.gz", "--output", source, identity["source"]
    )
    archive = folder / binary_name(identity["version"], arch)
    create_binary_archive(archive, raw, source)
    verify_binary_archive(archive, source, binaries)
    image = "plamenu-release:" + fingerprint(identity)[:32]
    with tempfile.TemporaryDirectory(prefix="plamenu-runtime-") as tmp:
        context = Path(tmp)
        (context / "Dockerfile").write_text(
            runtime_dockerfile((runner.source / "Dockerfile").read_text())
        )
        shutil.copy2(raw / "plamenu", context / "plamenu")
        runner.run(
            "docker",
            "buildx",
            "build",
            "--platform",
            "linux/" + arch,
            "--provenance=false",
            "--load",
            "--tag",
            image,
            "--build-arg",
            f"PLAMENU_VERSION={identity['version']}",
            "--build-arg",
            "PLAMENU_BUILD_CHANNEL=release",
            "--build-arg",
            f"PLAMENU_GIT_SHA={identity['source']}",
            "--build-arg",
            f"PLAMENU_BUILD_NUMBER={identity['build_number']}",
            context,
        )
    info = json.loads(runner.run("docker", "image", "inspect", image, capture=True))[0]
    if info["Architecture"] != arch or info["Os"] != "linux":
        raise ReleaseError("Unexpected runtime image architecture")
    write_json(folder / "image.json", info)
    exported = folder / "image.tar"
    runner.run("docker", "save", "--output", exported, image)
    with (
        exported.open("rb") as src,
        gzip.GzipFile(filename=str(folder / "image.tar.gz"), mode="wb", mtime=0) as dst,
    ):
        shutil.copyfileobj(src, dst, 4 * 1024**2)
    exported.unlink()
    write_json(
        folder / "build.json",
        {"identity": identity, "binaries": binaries, "image": image},
    )
    # Every remaining file belongs to the durable output, not a compiler cache.
    shutil.rmtree(raw)


def verify_build(folder, identity):
    record = json.loads((folder / "build.json").read_text())
    if record["identity"] != identity:
        raise ReleaseError("Build output belongs to different release inputs")
    source = folder / source_name(identity["version"])
    binary = folder / binary_name(identity["version"], identity["arch"])
    verify_binary_archive(binary, source, record["binaries"])
    with tarfile.open(binary) as archive:
        header = archive.extractfile("plamenu").read(20)
        if (
            len(header) != 20
            or header[:6] != b"\x7fELF\x02\x01"
            or int.from_bytes(header[18:20], "little")
            != {"amd64": 62, "arm64": 183}[identity["arch"]]
        ):
            raise ReleaseError(
                "Packaged executable architecture differs from the build"
            )
    with tarfile.open(source) as archive:
        if archive.pax_headers.get("comment") != identity["source"]:
            raise ReleaseError("Source archive commit differs from the release")
    return record


def checked_build(runner, folder, identity):
    from . import checks

    build(runner, folder, identity)
    validation = folder / "validation"
    validation.mkdir()
    checks.artifacts(runner, validation, folder, identity)
