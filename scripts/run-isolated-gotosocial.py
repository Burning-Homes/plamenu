#!/usr/bin/env python3
"""Run E2E with an owned GoToSocial when the main test identity is recreated."""

import argparse
import os
import signal
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "e2e"))
from plamenu_e2e.isolated_gotosocial import IsolatedGoToSocial


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state-dir", required=True, type=Path)
    parser.add_argument("--peer-root", required=True, type=Path)
    parser.add_argument("--proxy-container", required=True)
    parser.add_argument("--standing-container", required=True)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command
    if command[:1] == ["--"]:
        command = command[1:]
    if not command:
        parser.error("provide the test command after --")

    def interrupted(signum, frame):
        raise KeyboardInterrupt("Isolated GoToSocial test command interrupted")

    signal.signal(signal.SIGTERM, interrupted)
    os.umask(0o077)
    state = args.state_dir.resolve()
    state.mkdir(mode=0o700, exist_ok=False)
    environment = os.environ.copy()
    with (state / "fixture-private.log").open("x") as output:
        peer = IsolatedGoToSocial(
            state,
            environment,
            output,
            peer_root=args.peer_root,
            proxy=args.proxy_container,
            standing=args.standing_container,
        )
        try:
            peer.start()
            return subprocess.run(command, env=environment, check=False).returncode
        finally:
            peer.close()


if __name__ == "__main__":
    raise SystemExit(main())
