#!/usr/bin/env python3
"""Use the release gate's SQLx comparison for the developer database."""

import os
import sys
from pathlib import Path

from plamenu_release.core import ReleaseError, Runner
from plamenu_release.sqlx import check


def main():
    root = Path(__file__).resolve().parent.parent
    url = os.environ.get("DATABASE_URL")
    if not url:
        print("DATABASE_URL is required; use ./dev sqlx-check", file=sys.stderr)
        return 2
    folder = root / "target/sqlx-check"
    runner = Runner(root, folder / "check.log", extra={"CARGO_INCREMENTAL": "0"})
    try:
        check(runner, folder, url)
    except (ReleaseError, OSError) as error:
        print(error, file=sys.stderr)
        return 1
    print(f"PASS SQLx effective metadata; evidence: {folder / 'sqlx.json'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
