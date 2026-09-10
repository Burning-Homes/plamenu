#!/usr/bin/env python3
"""Install checksum-pinned upstream binaries without executing installer scripts."""

import argparse
import hashlib
import io
import json
import platform
import tarfile
import urllib.request
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument(
    "tools",
    nargs="*",
    help="Optional subset: cargo-nextest, mdbook, cargo-deny, cosign",
)
args = parser.parse_args()
if set(args.tools) - {"cargo-nextest", "mdbook", "cargo-deny", "cosign"}:
    parser.error("Unknown CI tool")
root = Path.home() / ".local/share/plamenu-ci/bin"
root.mkdir(parents=True, exist_ok=True)
manifest = json.loads(Path("scripts/ci/tools.json").read_text())
for item in manifest[platform.machine()]:
    if args.tools and item["binary"] not in args.tools:
        continue
    with urllib.request.urlopen(item["url"], timeout=120) as response:
        data = response.read()
    if hashlib.sha256(data).hexdigest() != item["sha256"]:
        raise SystemExit(f"Checksum mismatch: {item['binary']}")
    if item.get("format") == "binary":
        destination = root / item["binary"]
        destination.write_bytes(data)
        destination.chmod(0o755)
        print(f"Installed verified {item['binary']}")
        continue
    with tarfile.open(fileobj=io.BytesIO(data)) as archive:
        member = next(
            m for m in archive if Path(m.name).name == item["binary"] and m.isfile()
        )
        destination = root / item["binary"]
        destination.write_bytes(archive.extractfile(member).read())
        destination.chmod(0o755)
    print(f"Installed verified {item['binary']}")
