"""Sign and publish the already tested local bundle; never rebuild during upload."""

import json
import os
import re
import shutil
import subprocess
import tempfile
import time
from pathlib import Path
from urllib.parse import quote, urlsplit

from .build import TARGETS, binary_name, source_name
from .core import ReleaseError, Runner, digest, fingerprint, git, write_json
from .forge import Forge


def manifest(folder):
    return "".join(
        f"{digest(p)}  {p.name}\n"
        for p in sorted(folder.iterdir())
        if p.is_file() and p.name not in ("SHA256SUMS", "SHA256SUMS.bundle")
    )


def verify_manifest(folder):
    if (folder / "SHA256SUMS").read_text() != manifest(folder):
        raise ReleaseError(
            "Prepared release files changed; rerun preparation before publishing"
        )


def verify_image(runner, reference, expected):
    actual = json.loads(
        runner.run("docker", "image", "inspect", reference, capture=True)
    )[0]
    if any(
        actual[k] != expected[k] for k in ("Architecture", "Os", "Config", "RootFS")
    ):
        raise ReleaseError("Registry image differs from the tested image")


def registry_digest(runner, image):
    result = subprocess.run(
        [
            "docker",
            "buildx",
            "imagetools",
            "inspect",
            image,
            "--format",
            "{{json .Manifest.Digest}}",
        ],
        env=runner.env,
        capture_output=True,
        check=False,
    )
    if result.returncode:
        error = result.stderr.decode(errors="replace")
        # Authentication and transport failures must never mean "safe to replace".
        if re.search(r"(?i)\b(manifest unknown|name unknown)\b", error) or re.search(
            re.escape(image) + r": not found\s*$", error, re.MULTILINE
        ):
            return None
        raise ReleaseError("Cannot check existing registry image: " + error.strip())
    value = json.loads(result.stdout)
    if not isinstance(value, str) or not re.fullmatch(r"sha256:[0-9a-f]{64}", value):
        raise ReleaseError("Registry did not return an immutable image digest")
    return value


def registry_platforms(runner, reference):
    index = json.loads(
        runner.run(
            "docker",
            "buildx",
            "imagetools",
            "inspect",
            "--raw",
            reference,
            capture=True,
        )
    )
    if index.get("mediaType") not in (
        "application/vnd.oci.image.index.v1+json",
        "application/vnd.docker.distribution.manifest.list.v2+json",
    ):
        raise ReleaseError("Published image must be an OCI index for AMD64 and ARM64")
    platforms = {}
    for descriptor in index.get("manifests", []):
        platform = descriptor.get("platform", {})
        arch = platform.get("architecture")
        digest = descriptor.get("digest", "")
        if (
            platform.get("os") != "linux"
            or arch not in TARGETS
            or arch in platforms
            or not re.fullmatch(r"sha256:[0-9a-f]{64}", digest)
        ):
            raise ReleaseError(
                "Unexpected or duplicate platform in the OCI image index"
            )
        platforms[arch] = digest
    if platforms.keys() != TARGETS.keys():
        raise ReleaseError("OCI image index is missing a release architecture")
    return platforms


def publish_images(runner, root, registry, tag, forge):
    image = registry + ":" + tag
    expected = {
        arch: json.loads((root / ("build-" + arch) / "image.json").read_text())
        for arch in TARGETS
    }

    def readback(arch, digest):
        reference = registry + "@" + digest
        runner.run("docker", "pull", "--platform", "linux/" + arch, reference)
        verify_image(runner, reference, expected[arch])

    existing = registry_digest(runner, image)
    if existing is not None:
        platforms = registry_platforms(runner, registry + "@" + existing)
        for arch, digest in platforms.items():
            readback(arch, digest)
        return registry + "@" + existing, platforms

    platforms = {}
    for arch in TARGETS:
        folder = root / ("build-" + arch)
        platform_tag = image + "-" + arch
        digest = registry_digest(runner, platform_tag)
        if digest is None:
            runner.run("docker", "load", "--input", folder / "image.tar.gz")
            local = json.loads((folder / "build.json").read_text())["image"]
            verify_image(runner, local, expected[arch])
            runner.run("docker", "tag", local, platform_tag)
            forge.repository_check()
            for attempt in range(3):
                try:
                    runner.run("docker", "push", platform_tag)
                    break
                except ReleaseError:
                    if attempt == 2:
                        raise
                    print(
                        f"{arch}: retrying image upload ({attempt + 2}/3)", flush=True
                    )
                    time.sleep(2 ** (attempt + 1))
            digest = registry_digest(runner, platform_tag)
            if digest is None:
                raise ReleaseError("Pushed image cannot be read back: " + arch)
        readback(arch, digest)
        platforms[arch] = digest

    # Refuse a conflicting version that appeared during the platform uploads.
    existing = registry_digest(runner, image)
    if existing is None:
        forge.repository_check()
        runner.run(
            "docker",
            "buildx",
            "imagetools",
            "create",
            "--tag",
            image,
            *(registry + "@" + platforms[arch] for arch in TARGETS),
        )
        existing = registry_digest(runner, image)
    if (
        existing is None
        or registry_platforms(runner, registry + "@" + existing) != platforms
    ):
        raise ReleaseError(
            "Published OCI index differs from the tested architecture images"
        )
    return registry + "@" + existing, platforms


def prepared_names(version):
    return {binary_name(version, arch) for arch in TARGETS} | {
        source_name(version),
        "release.json",
        "SHA256SUMS",
    }


def release_description(notes, identity, repository, registry, reference):
    version = identity["version"]
    tag = "v" + version
    download = repository + "/releases/download/" + quote(tag, safe="") + "/"
    lines = [
        notes,
        "",
        "## Linux downloads",
        "",
        "| Platform | Installation archive |",
        "| --- | --- |",
    ]
    for arch, label in (("amd64", "AMD64 / x86_64"), ("arm64", "ARM64 / aarch64")):
        name = binary_name(version, arch)
        lines.append(f"| {label} | [{name}]({download}{name}) |")
    source = source_name(version)
    lines.extend(
        [
            "",
            "Each installation archive contains one static `plamenu` executable, the systemd unit, Plamenu and Caddy configuration examples, and the license.",
            "",
            f"[Source archive]({download}{source}) · [Checks and build information]({download}release.json)",
            "",
            "## Container image (OCI)",
            "",
            "The image includes `linux/amd64` and `linux/arm64`; Docker selects the matching architecture.",
            "",
            f"```sh\ndocker pull {registry}:{tag}\n```",
            "",
            f"Immutable image: `{reference}`.",
            "",
            "Private packages require `docker login "
            + registry.split("/", 1)[0]
            + "`.",
            "",
            "## Verification",
            "",
            f"[Checksums]({download}SHA256SUMS) · [Signature bundle]({download}SHA256SUMS.bundle) · [Public key]({download}cosign.pub)",
            "",
            "Use the public key from independently trusted source history to verify the signed checksums and image; see the release guide.",
        ]
    )
    registry_parts = registry.split("/", 2)
    if len(registry_parts) == 3 and registry_parts[0] == urlsplit(repository).netloc:
        package = f"https://{registry_parts[0]}/{registry_parts[1]}/-/packages/container/{quote(registry_parts[2], safe='')}/{quote(tag, safe='')}"
        lines.insert(
            lines.index("## Container image (OCI)") + 2,
            f"[Open the container package]({package})\n",
        )
    return "\n".join(lines) + "\n"


def prepare_tag(runner, tag, revision, allowed, remote):
    source = runner.source
    existing = subprocess.run(
        ["git", "show-ref", "--verify", "--quiet", "refs/tags/" + tag],
        cwd=source,
        check=False,
    )
    if existing.returncode == 1:
        # Reuse an existing remote signature after an interrupted/different local run.
        remote_tag = git(source, "ls-remote", "--tags", remote, "refs/tags/" + tag)
        if remote_tag:
            runner.run("git", "fetch", remote, "refs/tags/" + tag + ":refs/tags/" + tag)
        else:
            runner.run(
                "git",
                "-c",
                "gpg.format=ssh",
                "tag",
                "-s",
                "-m",
                "Plamenu " + tag,
                tag,
                revision,
            )
    elif existing.returncode != 0:
        raise ReleaseError("Cannot inspect the existing release tag")
    if git(source, "rev-parse", "refs/tags/" + tag + "^{commit}") != revision:
        raise ReleaseError("Version tag already points at another source revision")
    runner.run(
        "git",
        "-c",
        "gpg.format=ssh",
        "-c",
        f"gpg.ssh.allowedSignersFile={allowed}",
        "verify-tag",
        tag,
    )
    remote_tag = git(source, "ls-remote", "--tags", remote, "refs/tags/" + tag)
    if remote_tag and remote_tag.split()[0] != git(
        source, "rev-parse", "refs/tags/" + tag
    ):
        raise ReleaseError("Remote version tag conflicts with the signed local tag")


def publish(source, root, identity, config, notes):
    required = ("repository", "registry", "username", "token_file", "signing_key")
    missing = [name for name in required if not config.get(name)]
    if missing:
        raise ReleaseError(
            "Publication configuration is missing: " + ", ".join(missing)
        )
    repository, registry = config["repository"], config["registry"]
    if (
        not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9.:/_-]+", registry)
        or "/" not in registry
    ):
        raise ReleaseError("registry must name a registry host and image repository")
    parsed = urlsplit(repository)
    owner = parsed.path.strip("/").split("/")[0]
    if not config.get("allow_public", False) and not registry.startswith(
        parsed.netloc + "/" + owner + "/"
    ):
        raise ReleaseError(
            "Private publication requires the configured forge owner's registry namespace so its visibility can be verified"
        )
    public = (
        Path(config.get("public_key", source / "release/cosign.pub"))
        .expanduser()
        .resolve()
    )
    key = Path(config["signing_key"]).expanduser().resolve()
    allowed = (
        Path(config.get("allowed_signers", source / "release/allowed-signers"))
        .expanduser()
        .resolve()
    )
    token = Path(config["token_file"]).expanduser().read_text().strip()
    forge = Forge(repository, token, config.get("allow_public", False))
    forge.repository_check()
    remote = config.get("git_remote", "codefloe")
    expected_remotes = (
        repository + ".git",
        f"git@{parsed.netloc}:{parsed.path.lstrip('/')}.git",
        f"ssh://git@{parsed.netloc}{parsed.path}.git",
    )
    if git(source, "remote", "get-url", "--push", remote) not in expected_remotes:
        raise ReleaseError(
            "Git push remote differs from the configured publication repository"
        )
    verify_manifest(root / "assets")
    if {p.name for p in (root / "assets").iterdir()} != prepared_names(
        identity["version"]
    ):
        raise ReleaseError("Unexpected prepared asset set")
    tag = "v" + identity["version"]
    target = {
        "repository": repository,
        "registry": registry,
        "public_key": digest(public),
        "prepared_manifest": digest(root / "assets/SHA256SUMS"),
    }
    output = root / "publication" / fingerprint(target)[:16]
    output.mkdir(parents=True, exist_ok=True)
    runner = Runner(source, output / "publish.log")
    folder = output / "assets"
    folder.mkdir(exist_ok=True)
    originals = [p for p in (root / "assets").iterdir() if p.name != "SHA256SUMS"]
    for original, name in [(p, p.name) for p in originals] + [(public, "cosign.pub")]:
        destination = folder / name
        if destination.exists():
            if digest(original) != digest(destination):
                raise ReleaseError(
                    "Publication retry contains changed prepared assets: " + name
                )
        else:
            pending = output / (name + ".pending")
            shutil.copy2(original, pending)
            pending.replace(destination)

    def write_asset(path, text):
        pending = output / (path.name + ".pending")
        pending.write_text(text)
        pending.replace(path)

    with tempfile.TemporaryDirectory(prefix="plamenu-publish-") as temp:
        private = Path(temp)
        docker_config = private / "docker"
        # Keep rootless/remote daemon selection while isolating registry credentials.
        context = runner.run("docker", "context", "show", capture=True)
        if context != "default":
            runner.run("docker", "context", "export", context, private / "context.tar")
        runner.env["DOCKER_CONFIG"] = str(docker_config)
        if context != "default":
            runner.run("docker", "context", "import", context, private / "context.tar")
            runner.run("docker", "context", "use", context)
        signing_env = {"COSIGN_PASSWORD": os.environ.get("COSIGN_PASSWORD", "")}
        offline = private / "offline.json"
        offline.write_text(
            '{"mediaType":"application/vnd.dev.sigstore.signingconfig.v0.2+json"}\n'
        )

        def sign_blob(path, bundle):
            # Atomic replacement requires the destination filesystem; /tmp may
            # be a separate mount. Keep incomplete signatures out of the assets.
            with tempfile.TemporaryDirectory(
                prefix=".sign-", dir=bundle.parent
            ) as staging:
                pending = Path(staging) / bundle.name
                runner.run(
                    "cosign",
                    "sign-blob",
                    "--yes",
                    "--key",
                    key,
                    "--signing-config",
                    offline,
                    "--bundle",
                    pending,
                    path,
                    env=signing_env,
                )
                verify_blob(path, pending)
                pending.replace(bundle)

        def verify_blob(path, bundle):
            runner.run(
                "cosign",
                "verify-blob",
                "--key",
                public,
                "--insecure-ignore-tlog",
                "--bundle",
                bundle,
                path,
            )

        # Verify configuration and tag conflicts before the first upload.
        prepare_tag(runner, tag, identity["source"], allowed, remote)
        existing_release = forge.tag_release(tag)
        if existing_release is not None:
            remote_assets = forge.api(f"/releases/{existing_release['id']}/assets")
            for asset in remote_assets:
                if asset["name"] in prepared_names(identity["version"]) - {
                    "SHA256SUMS"
                }:
                    fetched = private / asset["name"]
                    forge.download(asset["browser_download_url"], fetched)
                    if digest(fetched) != digest(folder / asset["name"]):
                        raise ReleaseError(
                            "Existing version release contains different prepared bytes"
                        )
        sign_blob(public, private / "probe.bundle")
        verify_blob(public, private / "probe.bundle")
        runner.run(
            "docker",
            "login",
            registry.split("/", 1)[0],
            "--username",
            config["username"],
            "--password-stdin",
            data=token.encode(),
        )
        reference, platforms = publish_images(runner, root, registry, tag, forge)
        forge.link_package(registry, tag)
        image_file = folder / "image.txt"
        if image_file.exists() and image_file.read_text().strip() != reference:
            raise ReleaseError("Publication retry refers to a different image digest")
        write_asset(image_file, reference + "\n")
        sums = folder / "SHA256SUMS"
        text = manifest(folder)
        if sums.exists() and sums.read_text() != text:
            raise ReleaseError("Publication assets changed after signing")
        write_asset(sums, text)
        bundle = folder / "SHA256SUMS.bundle"
        if not bundle.exists():
            sign_blob(sums, bundle)
        verify_blob(sums, bundle)
        runner.run(
            "cosign",
            "sign",
            "--yes",
            "--key",
            key,
            "--signing-config",
            offline,
            reference,
            env=signing_env,
        )
        runner.run(
            "cosign", "verify", "--key", public, "--insecure-ignore-tlog", reference
        )
        runner.run("git", "push", remote, "refs/tags/" + tag)
        release = forge.tag_release(tag)
        if release is None:
            forge.repository_check()
            release = forge.api(
                "/releases",
                "POST",
                {
                    "tag_name": tag,
                    "target_commitish": identity["source"],
                    "name": "Plamenu " + tag,
                    "body": release_description(
                        notes, identity, repository, registry, reference
                    ),
                    "draft": True,
                    "prerelease": "-" in tag,
                },
            )
        if release["tag_name"] != tag:
            raise ReleaseError("Unexpected release returned by the forge")
        readback = private / "readback"
        readback.mkdir()
        forge.sync_assets(release, folder, readback)
        verify_manifest(readback)
        verify_blob(readback / "SHA256SUMS", readback / "SHA256SUMS.bundle")
        if release["draft"]:
            forge.repository_check()
            release = forge.api(f"/releases/{release['id']}", "PATCH", {"draft": False})
        if release["draft"]:
            raise ReleaseError("Forge did not finalize the release")
        write_json(
            output / "receipt.json",
            {
                "source": identity["source"],
                "image": reference,
                "platforms": platforms,
                "release": release["html_url"],
                "manifest_sha256": digest(sums),
            },
        )
        print("Published " + release["html_url"], flush=True)
