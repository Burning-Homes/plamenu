#!/usr/bin/env python3
"""Prepare a tested local release, optionally using workers or publishing it."""

import argparse
import fcntl
import os
import platform
import re
import shutil
import subprocess
import sys
from pathlib import Path

import tomllib
from plamenu_release import build, checks, docs, worker
from plamenu_release.core import (
    ReleaseError,
    clean,
    digest,
    fingerprint,
    git,
    stage,
    write_json,
)
from plamenu_release.publish import manifest, publish, verify_manifest


def notes_for(source, version):
    text = (source / "CHANGELOG.md").read_text()
    match = re.search(
        r"^## \[?" + re.escape(version) + r"\]?(?:\s[^\n]*)?\n(.*?)(?=^## |\Z)",
        text,
        re.MULTILINE | re.DOTALL,
    )
    if not match or not match[1].strip():
        raise ReleaseError(
            f"Add the {version} release notes to CHANGELOG.md before preparing a release"
        )
    return match[1].strip()


def prerequisites(args):
    tools = {
        "git",
        "ssh-keygen",
        "docker",
        "cargo",
        "cargo-nextest",
        "cargo-sqlx",
        "ffmpeg",
        "ffprobe",
        "cc",
        "pkg-config",
        "perl",
    }
    if not args.check_only:
        tools.add("readelf")
    if args.checks_on == "local":
        tools.update(("mdbook", "cargo-deny", "ruff", "shellcheck"))
    if args.publish:
        tools.add("cosign")
    if args.with_e2e:
        tools.add("uv")
    missing = sorted(t for t in tools if not shutil.which(t))
    if args.checks_on == "local":
        for module in ("yaml", "pytest"):
            if subprocess.run(
                [sys.executable, "-c", "import " + module],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
            ).returncode:
                missing.append("Python " + module)
    return missing


def configuration_errors(args, config):
    required = set()
    if (
        args.publish
        or args.checks_on == "codefloe"
        or (not args.check_only and "codefloe" in (args.amd64_on, args.arm64_on))
    ):
        required.update(("repository", "token_file"))
    if args.publish:
        required.update(("registry", "username", "signing_key"))
    errors = ["missing " + name for name in sorted(required) if not config.get(name)]
    for name in required & {"token_file", "signing_key"}:
        if config.get(name) and not Path(config[name]).expanduser().is_file():
            errors.append(name + " does not name a regular file")
    if args.publish:
        github = config.get("github")
        if github is not None:
            if not isinstance(github, dict):
                errors.append("github must be a TOML table")
            else:
                for name in ("repository", "token_file"):
                    if not github.get(name):
                        errors.append("missing github." + name)
                token_file = github.get("token_file")
                if token_file and not Path(token_file).expanduser().is_file():
                    errors.append("github.token_file does not name a regular file")
        try:
            if docs.enabled(config):
                docs.settings(config)
                if not shutil.which("mdbook"):
                    errors.append("docs publication requires local mdbook")
        except ReleaseError as error:
            errors.append(str(error))
    return errors


def parser():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument(
        "--publish",
        action="store_true",
        help="Sign and publish the prepared bytes using the configured destination",
    )
    p.add_argument(
        "--plan",
        action="store_true",
        help="Show stages and missing prerequisites without running anything",
    )
    p.add_argument(
        "--check-only",
        action="store_true",
        help="Run source checks without building or publishing",
    )
    p.add_argument(
        "--rerun",
        action="store_true",
        help="Rerun stages instead of reusing verified results",
    )
    p.add_argument(
        "--jobs",
        type=int,
        default=2,
        help="Compilation concurrency (default: 2; no required CPU model or quota)",
    )
    for arch in build.TARGETS:
        p.add_argument(
            "--" + arch + "-on",
            choices=("local", "codefloe"),
            default="local",
            help=f"Where the {arch} executable and image are built and tested (default: local)",
        )
    p.add_argument(
        "--checks-on",
        choices=("local", "codefloe"),
        default="local",
        help="Where static checks run; application/database checks run locally",
    )
    e = p.add_mutually_exclusive_group()
    e.add_argument(
        "--with-e2e",
        action="store_true",
        help="Run the developer-specific live federation fleet",
    )
    e.add_argument(
        "--skip-e2e",
        action="store_true",
        help="Explicitly omit the nonportable developer E2E fleet (also the default)",
    )
    p.add_argument(
        "--with-performance",
        action="store_true",
        help="Run calibrated timing benchmarks on their designated machine",
    )
    p.add_argument(
        "--config",
        type=Path,
        default=Path(
            os.environ.get("PLAMENU_RELEASE_CONFIG", "~/.config/plamenu/release.toml")
        ).expanduser(),
        help="Optional local worker/publication settings",
    )
    return p


def run(args):
    if args.jobs <= 0 or args.jobs > 256 or args.check_only and args.publish:
        raise ReleaseError(
            "Use a positive job count (at most 256); --check-only cannot publish"
        )
    origin = Path(git(Path.cwd(), "rev-parse", "--show-toplevel"))
    version = tomllib.loads((origin / "Cargo.toml").read_text())["workspace"][
        "package"
    ]["version"]
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[a-z0-9.-]+)?", version):
        raise ReleaseError("Unsupported release version")
    identity = {
        "schema": 2,
        "source": git(origin, "rev-parse", "HEAD"),
        "version": version,
        "architectures": list(build.TARGETS),
        "jobs": args.jobs,
        "build_number": git(origin, "show", "-s", "--format=%ct", "HEAD"),
        "e2e": args.with_e2e,
        "performance": args.with_performance,
    }
    if args.with_e2e:
        identity["e2e_config"] = (
            digest(origin / ".env") if (origin / ".env").exists() else "absent"
        )
    config = tomllib.loads(args.config.read_text()) if args.config.exists() else {}
    missing = prerequisites(args)
    config_errors = configuration_errors(args, config)
    print(
        f"Plamenu {version}, {identity['source'][:12]}, Linux AMD64 + ARM64", flush=True
    )
    print(
        f"Static checks: {args.checks_on}; application/database: local; "
        + (
            "build/package: skipped"
            if args.check_only
            else f"build/restore: AMD64 {args.amd64_on}, ARM64 {args.arm64_on}"
        )
    )
    print(
        "Developer E2E: "
        + (
            "enabled"
            if args.with_e2e
            else "not run (developer-specific setup; --with-e2e to enable)"
        )
    )
    print(
        "Calibrated timings: "
        + (
            "enabled"
            if args.with_performance
            else "not run (machine-specific budgets; --with-performance to enable)"
        )
    )
    if args.plan:
        print("Missing prerequisites: " + (", ".join(missing) or "none"))
        print("Configuration: " + ("; ".join(config_errors) or "ready"))
        print("Docs: " + ("after publication" if docs.enabled(config) else "disabled"))
        print("Publish: " + ("requested" if args.publish else "no; outputs stay local"))
        return
    if platform.system() != "Linux" or platform.machine() not in ("x86_64", "aarch64"):
        raise ReleaseError("Run release preparation on Linux AMD64 or ARM64")
    if missing:
        raise ReleaseError(
            "Install prerequisites before releasing: "
            + ", ".join(missing)
            + "; see docs/RELEASING.md"
        )
    if config_errors:
        raise ReleaseError(
            "Release configuration: "
            + "; ".join(config_errors)
            + "; see docs/RELEASING.md"
        )
    clean(origin)
    notes = notes_for(origin, version)
    if args.with_performance:
        checks.performance_machine(origin)
    subprocess.run(
        ["docker", "info"],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    if not args.check_only:
        subprocess.run(
            ["docker", "compose", "version"], check=True, stdout=subprocess.DEVNULL
        )
        subprocess.run(
            ["docker", "buildx", "version"], check=True, stdout=subprocess.DEVNULL
        )
        # Fail before lengthy checks when a local target cannot execute. Workers
        # test their images natively and do not require emulation on this host.
        for arch, machine in (("amd64", "x86_64"), ("arm64", "aarch64")):
            if getattr(args, arch + "_on") != "local":
                continue
            probe = subprocess.run(
                [
                    "docker",
                    "run",
                    "--rm",
                    "--platform",
                    "linux/" + arch,
                    "alpine:3.22@sha256:14358309a308569c32bdc37e2e0e9694be33a9d99e68afb0f5ff33cc1f695dce",
                    "uname",
                    "-m",
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            if probe.returncode or probe.stdout.strip() != machine:
                raise ReleaseError(
                    f"Docker cannot execute linux/{arch}; configure local emulation "
                    f"(docs/RELEASING.md) or use --{arch}-on codefloe. "
                    + probe.stderr.strip()
                )
    root = origin / "release-artifacts" / ("v" + version) / fingerprint(identity)[:20]
    root.mkdir(parents=True, exist_ok=True, mode=0o700)
    with (root / "lock").open("w") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise ReleaseError(
                "This release is already running in another process"
            ) from error
        write_json(root / "identity.json", identity)
        source = root / "source"
        if not source.exists():
            subprocess.run(
                ["git", "worktree", "add", "--detach", str(source), identity["source"]],
                cwd=origin,
                check=True,
            )
        if git(source, "rev-parse", "HEAD") != identity["source"]:
            raise ReleaseError("Release worktree moved; refusing to reuse it")
        clean(source)
        ref = subprocess.run(
            ["git", "symbolic-ref", "--quiet", "--short", "HEAD"],
            cwd=origin,
            text=True,
            capture_output=True,
            check=False,
        )
        if ref.returncode:
            ref = subprocess.run(
                ["git", "describe", "--exact-match", "--tags", "HEAD"],
                cwd=origin,
                text=True,
                capture_output=True,
                check=False,
            )
        ref = ref.stdout.strip() if not ref.returncode else None

        def operation(phase, where, local, inputs=identity):
            if where == "local":
                return local
            return lambda r, folder: worker.remote(
                source, root, folder, inputs, phase, config, ref
            )

        statuses = {}
        cargo_env = {"CARGO_TARGET_DIR": str(origin / "release-artifacts/cargo-target")}
        statuses["static"] = stage(
            source,
            root,
            "static",
            identity,
            operation("static", args.checks_on, checks.static),
            rerun=args.rerun,
            # Once publication starts, retries must retain the same signed reports.
            max_age=None if (root / "publication").exists() else 86400,
            env=cargo_env,
        )
        statuses["application"] = stage(
            source,
            root,
            "application",
            identity,
            checks.application,
            rerun=args.rerun,
            env=cargo_env,
        )
        for name, enabled, action in (
            ("e2e", args.with_e2e, lambda r, p: checks.e2e(r, p, origin)),
            ("performance", args.with_performance, checks.performance),
        ):
            statuses[name] = (
                stage(source, root, name, identity, action, rerun=args.rerun)
                if enabled
                else {
                    "status": "not-run",
                    "reason": "developer-specific environment"
                    if name == "e2e"
                    else "machine-specific timing calibration",
                }
            )
        if args.check_only:
            print("Source checks passed. Logs: " + str(root))
            return
        for arch, target in build.TARGETS.items():
            where = getattr(args, arch + "_on")
            inputs = identity | {"arch": arch, "target": target, "executor": where}
            phase = "build-" + arch
            statuses[phase] = stage(
                source,
                root,
                phase,
                inputs,
                operation(
                    "build",
                    where,
                    lambda r, p, i=inputs: build.checked_build(r, p, i),
                    inputs,
                ),
                rerun=args.rerun,
            )
            build.verify_build(root / phase, inputs)
        source_archive = build.source_name(version)
        if build.source_tree(
            root / "build-amd64" / source_archive
        ) != build.source_tree(root / "build-arm64" / source_archive):
            raise ReleaseError("Architecture builds returned different source archives")
        report = {"identity": identity, "checks": statuses}
        assets = root / "assets"
        if assets.exists():
            shutil.rmtree(assets)
        assets.mkdir()
        for arch in build.TARGETS:
            name = build.binary_name(version, arch)
            shutil.copy2(root / ("build-" + arch) / name, assets / name)
        shutil.copy2(root / "build-amd64" / source_archive, assets / source_archive)
        write_json(assets / "release.json", report)
        (assets / "SHA256SUMS").write_text(manifest(assets))
        verify_manifest(assets)
        print("Release prepared: " + str(assets), flush=True)
        if args.publish:
            publish(source, root, identity, config, notes)
            docs.after_release(source, root, identity, config)


def main():
    args = parser().parse_args()
    try:
        run(args)
    except (ReleaseError, OSError, subprocess.CalledProcessError, ValueError) as error:
        print("Release stopped: " + str(error), file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print(
            "Release interrupted. Rerun the same command to resume completed stages.",
            file=sys.stderr,
        )
        return 130
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
