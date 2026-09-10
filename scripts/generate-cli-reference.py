#!/usr/bin/env python3
"""Generate the public CLI reference from the binary's Clap help tree."""

from __future__ import annotations

import os
from pathlib import Path
import re
import subprocess
import sys


SUBCOMMAND = re.compile(r"^  ([a-z][a-z0-9-]*)(?:\s{2,}|$)")


def help_for(binary: Path, path: tuple[str, ...]) -> str:
    environment = os.environ.copy()
    environment["NO_COLOR"] = "1"
    completed = subprocess.run(
        [str(binary), *path, "--help"],
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=environment,
    )
    return "\n".join(line.rstrip() for line in completed.stdout.splitlines())


def child_commands(help_text: str) -> list[str]:
    lines = help_text.splitlines()
    try:
        start = lines.index("Commands:") + 1
    except ValueError:
        return []

    children: list[str] = []
    for line in lines[start:]:
        if not line.strip():
            break
        match = SUBCOMMAND.match(line)
        if match and match.group(1) != "help":
            children.append(match.group(1))
    return children


def render_command(binary: Path, path: tuple[str, ...], output: list[str]) -> None:
    help_text = help_for(binary, path)
    command = " ".join(("plamenu", *path))
    level = min(2 + len(path), 6)
    output.extend(
        [
            f"{'#' * level} `{command}`",
            "",
            "```text",
            f"$ {command} --help",
            help_text,
            "```",
            "",
        ]
    )
    for child in child_commands(help_text):
        render_command(binary, (*path, child), output)


def main() -> int:
    if len(sys.argv) != 3:
        print(
            "usage: generate-cli-reference.py PLAMENU_BINARY OUTPUT",
            file=sys.stderr,
        )
        return 2

    binary = Path(sys.argv[1]).resolve()
    destination = Path(sys.argv[2])
    if not binary.is_file():
        print(f"Plamenu binary does not exist: {binary}", file=sys.stderr)
        return 2
    output = [
        "# Command-line reference",
        "",
        "This page is generated from the running program's Clap help tree. Do not edit",
        "it by hand; run `./dev docs-generate` after changing a command or flag.",
        "All commands except `config generate` read the global `--config` TOML file.",
        "",
    ]
    render_command(binary, (), output)
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_text("\n".join(output), encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
