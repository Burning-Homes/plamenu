#!/usr/bin/env python3
"""Mirror an already-published Codefloe release to GitHub."""

import argparse
import os
import subprocess
import sys
import urllib.error
from pathlib import Path

import tomllib
from plamenu_release.core import ReleaseError
from plamenu_release.release_mirror import mirror_published_release


def parser():
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("tag", help="Published release tag, for example v0.6.0")
    result.add_argument(
        "--config",
        type=Path,
        default=Path(
            os.environ.get("PLAMENU_RELEASE_CONFIG", "~/.config/plamenu/release.toml")
        ).expanduser(),
        help="Local release settings containing repository and [github]",
    )
    return result


def run(args):
    if not args.config.is_file():
        raise ReleaseError("Release configuration does not name a regular file")
    config = tomllib.loads(args.config.read_text())
    if not config.get("repository"):
        raise ReleaseError("Release configuration is missing repository")
    url = mirror_published_release(config["repository"], args.tag, config)
    print("Mirrored " + url, flush=True)


def main():
    try:
        run(parser().parse_args())
    except (
        ReleaseError,
        OSError,
        subprocess.CalledProcessError,
        urllib.error.URLError,
        ValueError,
    ) as error:
        print("Release mirror stopped: " + str(error), file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print(
            "Release mirror interrupted; rerun the command to resume.", file=sys.stderr
        )
        return 130
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
