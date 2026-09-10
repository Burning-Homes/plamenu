#!/usr/bin/env python3
"""Build docs locally or deploy a captured source revision to Codefloe Pages."""

import argparse
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

import tomllib
from plamenu_release import docs
from plamenu_release.core import ReleaseError, clean, git


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    build_parser = commands.add_parser("build", help="Build a local preview")
    build_parser.add_argument(
        "--label", default="dev", help="Local output directory label"
    )
    build_parser.add_argument("--output", type=Path, help="Local build destination")
    deploy_parser = commands.add_parser(
        "deploy",
        prog="./dev docs-deploy",
        help="Publish and verify a captured source revision",
    )
    deploy_parser.add_argument(
        "--ref", default="HEAD", help="Source commit or tag (default: HEAD)"
    )
    deploy_parser.add_argument(
        "--config",
        type=Path,
        default=Path(
            os.environ.get("PLAMENU_RELEASE_CONFIG", "~/.config/plamenu/release.toml")
        ).expanduser(),
        help="Local release configuration",
    )
    deploy_parser.add_argument(
        "--timeout",
        type=int,
        default=300,
        help="Hosted verification timeout in seconds (default: 300)",
    )
    args = parser.parse_args()
    try:
        root = Path(git(Path.cwd(), "rev-parse", "--show-toplevel"))
        if args.command == "build":
            if not re.fullmatch(r"[A-Za-z0-9._-]+", args.label) or args.label in (
                ".",
                "..",
            ):
                raise ReleaseError("Invalid documentation directory label")
            output = args.output or root / "target/docs/site" / args.label
            docs.build_site(root, output, root / "target/docs/build.log")
            print("Documentation site: " + str(output / "index.html"))
            return 0
        if args.timeout <= 0:
            raise ReleaseError("--timeout must be positive")
        clean(root)
        config = tomllib.loads(args.config.read_text())
        docs.settings(config)
        revision = git(
            root, "rev-parse", "--verify", "--end-of-options", args.ref + "^{commit}"
        )
        with tempfile.TemporaryDirectory(prefix="plamenu-docs-") as temporary:
            source = Path(temporary) / "source"
            subprocess.run(
                [
                    "git",
                    "worktree",
                    "add",
                    "--quiet",
                    "--detach",
                    str(source),
                    revision,
                ],
                cwd=root,
                check=True,
            )
            try:
                docs.deploy(
                    source, root / "target/docs/deploy" / revision, config, args.timeout
                )
            finally:
                subprocess.run(
                    ["git", "worktree", "remove", "--force", str(source)],
                    cwd=root,
                    check=True,
                )
        return 0
    except (ReleaseError, OSError, ValueError, subprocess.CalledProcessError) as error:
        print("Docs stopped: " + str(error), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
