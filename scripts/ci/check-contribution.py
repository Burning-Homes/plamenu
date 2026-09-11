#!/usr/bin/env python3
"""Fail closed on missing diff history; check seed version and DCO trailers."""

import json
import os
import re
import subprocess


def git(*args):
    return subprocess.check_output(["git", *args], text=True).strip()


def commit(value):
    if not re.fullmatch(r"[0-9a-f]{40}", value or "") or value == "0" * 40:
        raise SystemExit("A nonzero full commit SHA is required for the diff base")
    git("cat-file", "-e", value + "^{commit}")
    return value


def main():
    with open(os.environ["GITHUB_EVENT_PATH"]) as stream:
        event = json.load(stream)
    kind = os.environ["GITHUB_EVENT_NAME"]
    if git("rev-parse", "--is-shallow-repository") != "false":
        raise SystemExit("Contribution checks require complete Git history")
    head = git("rev-parse", "HEAD")
    if kind == "pull_request":
        if head != commit(event["pull_request"]["head"]["sha"]):
            raise SystemExit("Checkout differs from the pull request head")
        base = git("merge-base", commit(event["pull_request"]["base"]["sha"]), head)
    elif kind == "push":
        if head != commit(event["after"]):
            raise SystemExit("Checkout differs from the push head")
        before = event["before"]
        if not re.fullmatch(r"[0-9a-f]{40}", before or ""):
            raise SystemExit("A nonzero full commit SHA is required for the diff base")
        parents = git("rev-list", "--parents", "-n", "1", head).split()[1:]
        # A new branch or rewritten root must check its complete history. A
        # rewritten root's previous push is not an ancestor (or in the clone).
        base = None if before == "0" * 40 or not parents else commit(before)
    elif kind == "workflow_dispatch":
        requested = os.environ.get("CHECK_BASE")
        parents = git("rev-list", "--parents", "-n", "1", head).split()[1:]
        base = commit(requested) if requested else (parents[0] if parents else None)
    else:
        raise SystemExit(f"Unsupported event: {kind}")
    if base is not None:
        subprocess.run(["git", "merge-base", "--is-ancestor", base, head], check=True)
    revisions = git(
        "rev-list", "--reverse", f"{base}..{head}" if base else head
    ).splitlines()
    if not revisions:
        raise SystemExit("Empty contribution range; provide an earlier base commit")
    # Forgejo-created merge commits do not carry DCO trailers. Exclude only
    # those commits; rev-list still follows every parent and checks their work.
    dco_revisions = git(
        "rev-list", "--reverse", "--no-merges", f"{base}..{head}" if base else head
    ).splitlines()
    for revision in dco_revisions:
        message = git("show", "-s", "--format=%B", revision)
        trailers = subprocess.check_output(
            ["git", "interpret-trailers", "--parse"], input=message, text=True
        )
        if not re.search(
            r"^Signed-off-by: .+ <[^<>\s]+@[^<>\s]+>$",
            trailers,
            re.MULTILINE | re.IGNORECASE,
        ):
            raise SystemExit(f"Missing DCO sign-off: {revision}")
    if base is not None:
        subprocess.run(["./scripts/check-seed-version.sh", base], check=True)
    else:
        # A root introduces the dataset; subsequent commits must preserve the
        # same seed-version rule as normal contribution ranges.
        for revision in revisions:
            parents = git("rev-list", "--parents", "-n", "1", revision).split()[1:]
            if parents:
                subprocess.run(
                    ["./scripts/check-seed-version.sh", parents[0], revision],
                    check=True,
                )
    print(
        f"PASS DCO and seed-version range {base or 'root'}..{head} "
        f"({len(dco_revisions)} non-merge DCO commits, {len(revisions)} total commits)"
    )


if __name__ == "__main__":
    main()
