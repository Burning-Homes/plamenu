#!/usr/bin/env python3
"""Generate Plamenu's compact reaction catalog from Unicode emoji-test.txt.

Usage: scripts/generate-emoji-catalog.py INPUT OUTPUT

INPUT may be a local path or an https URL.  The generated JSON contains the
RGI emoji set (fully-qualified emoji plus isolated emoji components), in the
group/subgroup order recommended by Unicode.
"""

from __future__ import annotations

import json
import pathlib
import re
import sys
import urllib.request

EXPECTED_VERSION = "17.0"
SOURCE_URL = "https://www.unicode.org/Public/17.0.0/emoji/emoji-test.txt"


def read_source(source: str) -> str:
    if source.startswith("https://"):
        with urllib.request.urlopen(source) as response:
            return response.read().decode("utf-8")
    return pathlib.Path(source).read_text(encoding="utf-8")


def main() -> None:
    if len(sys.argv) != 3:
        raise SystemExit("usage: generate-emoji-catalog.py INPUT OUTPUT")

    source, output = sys.argv[1:]
    text = read_source(source)
    version_match = re.search(r"^# Version: ([0-9.]+)$", text, re.MULTILINE)
    version = version_match.group(1) if version_match else ""
    if version != EXPECTED_VERSION:
        raise SystemExit(
            f"expected Emoji {EXPECTED_VERSION}, source declares {version or 'no version'}"
        )

    groups: list[dict[str, object]] = []
    group: dict[str, object] | None = None
    subgroup: dict[str, object] | None = None
    count = 0

    for line in text.splitlines():
        if line.startswith("# group: "):
            group = {"name": line.removeprefix("# group: "), "subgroups": []}
            groups.append(group)
            subgroup = None
            continue
        if line.startswith("# subgroup: "):
            if group is None:
                raise SystemExit("subgroup appeared before a group")
            subgroup = {
                "name": line.removeprefix("# subgroup: "),
                "emoji": [],
            }
            group["subgroups"].append(subgroup)  # type: ignore[union-attr]
            continue
        if not line or line.startswith("#") or ";" not in line:
            continue

        codepoints, rest = line.split(";", 1)
        status, comment = rest.split("#", 1)
        if status.strip() not in {"fully-qualified", "component"}:
            continue
        if subgroup is None:
            raise SystemExit("emoji appeared before a subgroup")

        name_match = re.match(r"\s*\S+\s+E[0-9.]+\s+(.+)$", comment)
        if name_match is None:
            raise SystemExit(f"could not parse emoji name: {line}")
        emoji = "".join(chr(int(value, 16)) for value in codepoints.split())
        subgroup["emoji"].append([emoji, name_match.group(1)])  # type: ignore[union-attr]
        count += 1

    if count != 3953:
        raise SystemExit(f"expected 3953 RGI emoji, parsed {count}")

    catalog = {
        "version": version,
        "source": SOURCE_URL,
        "groups": groups,
    }
    pathlib.Path(output).write_text(
        json.dumps(catalog, ensure_ascii=False, separators=(",", ":")) + "\n",
        encoding="utf-8",
    )


if __name__ == "__main__":
    main()
