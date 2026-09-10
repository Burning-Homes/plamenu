"""Static audit of the federation-test pairing manifest.

Every directional federation test carries a `@pytest.mark.federation(...)`
marker naming either its opposite-direction counterpart (`reverse_of=`) or the
reason no counterpart exists (`one_way_reason=`). This module scans the test
sources with `ast` (no live stack, selection-independent) and checks that the
pairing graph is complete and mutual, so a directional test can never silently
escape review.

Marker contract (`@pytest.mark.federation(...)`):
  - direction: REQUIRED, one of {"inbound" (peer->plamenu), "outbound"
    (plamenu->peer), "both" (one test drives both directions)}.
  - reverse_of: bare test-fn name (or "test_file.py::test_name" when the bare
    name is ambiguous) of the opposite-direction counterpart. Required when
    direction != "both" AND a counterpart exists.
  - one_way_reason: free text explaining why no reverse exists. Required when
    direction != "both" AND no counterpart exists.
  - Exactly one of {reverse_of, one_way_reason} when direction in
    {inbound, outbound}; NEITHER when direction == "both".
  - peer: optional {mastodon,pleroma,plup,sharkey,gts,mitra,onion,lemmy,peertube,
    owncast,mobilizon,discourse,plamenu2}; inferred from the filename when omitted.

Run standalone (no stack needed) for a per-peer coverage table + violations:
    uv run python tools/federation_coverage.py
or as a module:
    uv run python -m tools.federation_coverage
The in-suite gate tests/test_coverage_manifest.py calls audit() directly.
"""

from __future__ import annotations

import ast
import sys
from dataclasses import dataclass, field
from pathlib import Path

TESTS_DIR = Path(__file__).resolve().parent.parent / "tests"

DIRECTIONS = {"inbound", "outbound", "both"}
OPPOSITE = {"inbound": "outbound", "outbound": "inbound"}

# Fixtures that inject a live remote peer; a test taking any of these federates
# and must be marked (or explicitly allowlisted in LOCAL_EXCEPTIONS).
PEER_FIXTURES = {
    "alice",  # Mastodon
    "pleroma_bob",
    "plup_mari",  # upstream Pleroma (real 2.10.2)
    "sharkey_carol",
    "gts_dave",
    "mitra_erin",
    "onion_nina",  # onion-identity Mitra behind the tor-test rig
    "lemmy_frank",
    "mobilizon_grace",
    "discourse_diana",
    "doomed",  # ephemeral self-destruct peer
    "plamenu2",  # the second Plamenu instance (Plamenu against itself)
    "plamenu2_user",
    "plamenu2_api",
}

# Filename -> peer inference (checked as a prefix on the module basename).
PEER_BY_PREFIX = {
    # Checked longest-prefix-first below, so `test_plamenu2_*` never falls
    # through to another peer.
    "test_plamenu2": "plamenu2",
    "test_plup": "plup",
    "test_pleroma": "pleroma",
    "test_sharkey": "sharkey",
    "test_gotosocial": "gts",
    "test_mitra": "mitra",
    "test_tor": "onion",
    "test_lemmy": "lemmy",
    "test_peertube": "peertube",
    "test_owncast": "owncast",
    "test_mobilizon": "mobilizon",
    "test_discourse": "discourse",
}

# Tests that legitimately take a peer fixture (or live in a *_federation.py
# module) but are NOT directional federation tests — a local behavior that
# merely happens to use a remote actor as a prop. Kept small and commented so it
# cannot become a loophole. Bare function names (unique across the suite).
LOCAL_EXCEPTIONS = {
    # redraft source text is a local API concern; alice is only the audience.
    "test_delete_returns_source_text_for_redraft",
    # local full-text/hashtag search over a locally-authored post.
    "test_local_post_text_and_hashtag_search",
    # advertises the instance's supported post formats (local metadata).
    "test_post_formats_advertised",
    # the /aliases settings page is a local web view.
    "test_web_aliases_page",
    # refusing a move to an un-aliased target is local validation.
    "test_migration_to_unaliased_refused",
    # a list timeline including a followed member is a local timeline concern.
    "test_list_timeline_carries_mastodon_member",
}


@dataclass
class Record:
    name: str
    file: str  # module basename, e.g. "test_posts.py"
    lineno: int
    fixtures: frozenset[str]
    peer: str
    marked: bool = False
    direction: str | None = None
    reverse_of: str | None = None
    one_way_reason: str | None = None
    errors: list[str] = field(default_factory=list)  # parse-time marker errors

    @property
    def qual(self) -> str:
        return f"{self.file}::{self.name}"


@dataclass
class Violation:
    kind: str
    test: str
    message: str


def _peer_from_file(basename: str) -> str:
    for prefix in sorted(PEER_BY_PREFIX, key=len, reverse=True):
        if basename.startswith(prefix):
            return PEER_BY_PREFIX[prefix]
    return "mastodon"


def _is_federation_marker(call: ast.Call) -> bool:
    """True when `call.func` is the attribute chain `pytest.mark.federation`."""
    f = call.func
    return (
        isinstance(f, ast.Attribute)
        and f.attr == "federation"
        and isinstance(f.value, ast.Attribute)
        and f.value.attr == "mark"
        and isinstance(f.value.value, ast.Name)
        and f.value.value.id == "pytest"
    )


def _parse_marker(call: ast.Call, rec: Record) -> None:
    rec.marked = True
    if call.args:
        rec.errors.append("federation marker takes keyword arguments only")
    allowed = {"direction", "reverse_of", "one_way_reason", "peer"}
    for kw in call.keywords:
        if kw.arg is None:
            rec.errors.append("**kwargs splat in federation marker is not allowed")
            continue
        if kw.arg not in allowed:
            rec.errors.append(f"unknown federation kwarg {kw.arg!r}")
            continue
        try:
            value = ast.literal_eval(kw.value)
        except (ValueError, SyntaxError):
            rec.errors.append(f"federation kwarg {kw.arg!r} must be a literal")
            continue
        if kw.arg == "peer" and value:
            rec.peer = value
        elif kw.arg == "direction":
            rec.direction = value
        elif kw.arg == "reverse_of":
            rec.reverse_of = value
        elif kw.arg == "one_way_reason":
            rec.one_way_reason = value


def collect() -> list[Record]:
    """AST-scan tests/*.py; one Record per `test_*` function."""
    records: list[Record] = []
    for path in sorted(TESTS_DIR.glob("test_*.py")):
        tree = ast.parse(path.read_text(), filename=str(path))
        basename = path.name
        for node in tree.body:
            if not isinstance(node, ast.FunctionDef) or not node.name.startswith(
                "test_"
            ):
                continue
            fixtures = frozenset(a.arg for a in node.args.args)
            rec = Record(
                name=node.name,
                file=basename,
                lineno=node.lineno,
                fixtures=fixtures,
                peer=_peer_from_file(basename),
            )
            for deco in node.decorator_list:
                if isinstance(deco, ast.Call) and _is_federation_marker(deco):
                    _parse_marker(deco, rec)
            records.append(rec)
    return records


def _index(records: list[Record]) -> dict[str, list[Record]]:
    idx: dict[str, list[Record]] = {}
    for rec in records:
        idx.setdefault(rec.name, []).append(rec)
    return idx


def _resolve(
    reverse_of: str, index: dict[str, list[Record]]
) -> tuple[Record | None, str]:
    """Resolve a `reverse_of` value to a Record. Returns (record, error): error is
    "" on success, else "dangling"/"ambiguous"."""
    if "::" in reverse_of:
        want_file, _, want_name = reverse_of.partition("::")
        matches = [r for r in index.get(want_name, []) if r.file == want_file]
        if not matches:
            return None, "dangling"
        return matches[0], ""
    matches = index.get(reverse_of, [])
    if not matches:
        return None, "dangling"
    if len(matches) > 1:
        return None, "ambiguous"
    return matches[0], ""


def audit() -> list[Violation]:
    """Return the list of pairing violations (empty when the manifest is sound)."""
    records = collect()
    index = _index(records)
    violations: list[Violation] = []

    def is_federation(rec: Record) -> bool:
        return bool(rec.fixtures & PEER_FIXTURES) or rec.file.endswith("_federation.py")

    for rec in records:
        # Anti-loophole: any peer-fixture / *_federation.py test must be marked.
        if not rec.marked:
            if is_federation(rec) and rec.name not in LOCAL_EXCEPTIONS:
                violations.append(
                    Violation(
                        "unmarked-federation-test",
                        rec.qual,
                        "takes a peer fixture or lives in a *_federation.py module "
                        "but has no @pytest.mark.federation marker (add one, or "
                        "allowlist it in LOCAL_EXCEPTIONS if it is a local test).",
                    )
                )
            continue

        # Allowlisted-but-marked is a contradiction worth surfacing.
        if rec.name in LOCAL_EXCEPTIONS:
            violations.append(
                Violation(
                    "allowlisted-but-marked",
                    rec.qual,
                    "is in LOCAL_EXCEPTIONS yet carries a federation marker; "
                    "remove it from one place or the other.",
                )
            )

        for err in rec.errors:
            violations.append(Violation("marker-syntax", rec.qual, err))

        if rec.direction not in DIRECTIONS:
            violations.append(
                Violation(
                    "invalid-direction",
                    rec.qual,
                    f"direction must be one of {sorted(DIRECTIONS)}, got "
                    f"{rec.direction!r}.",
                )
            )
            continue

        if rec.direction == "both":
            if rec.reverse_of or rec.one_way_reason:
                violations.append(
                    Violation(
                        "both-with-extra",
                        rec.qual,
                        'direction="both" must set neither reverse_of nor '
                        "one_way_reason.",
                    )
                )
            continue

        # inbound / outbound: exactly one of reverse_of / one_way_reason.
        has_rev = bool(rec.reverse_of)
        has_reason = bool(rec.one_way_reason)
        if has_rev == has_reason:
            violations.append(
                Violation(
                    "needs-exactly-one",
                    rec.qual,
                    "a directional test needs exactly one of reverse_of / "
                    f"one_way_reason (reverse_of={rec.reverse_of!r}, "
                    f"one_way_reason set={has_reason}).",
                )
            )
            continue

        if has_reason:
            continue  # a documented one-way test; nothing to pair.

        target, err = _resolve(rec.reverse_of, index)
        if err == "dangling":
            violations.append(
                Violation(
                    "dangling-reverse",
                    rec.qual,
                    f"reverse_of={rec.reverse_of!r} names no collected test.",
                )
            )
            continue
        if err == "ambiguous":
            violations.append(
                Violation(
                    "ambiguous-reverse",
                    rec.qual,
                    f"reverse_of={rec.reverse_of!r} matches more than one test; "
                    "use the file::name form.",
                )
            )
            continue
        assert target is not None
        # The target must be the opposite direction...
        if target.direction != OPPOSITE.get(rec.direction):
            violations.append(
                Violation(
                    "wrong-direction",
                    rec.qual,
                    f"reverse_of points at {target.qual} whose direction is "
                    f"{target.direction!r}, not the opposite of {rec.direction!r}.",
                )
            )
        # ...and must point back at us (mutual).
        if target.reverse_of:
            back, back_err = _resolve(target.reverse_of, index)
            mutual = (
                back_err == ""
                and back is not None
                and back.name == rec.name
                and back.file == rec.file
            )
        else:
            mutual = False
        if not mutual:
            violations.append(
                Violation(
                    "non-mutual",
                    rec.qual,
                    f"reverse_of points at {target.qual}, but that test's "
                    f"reverse_of ({target.reverse_of!r}) does not point back here.",
                )
            )

    return violations


def format(violation: Violation) -> str:
    return f"[{violation.kind}] {violation.test}: {violation.message}"


def _coverage_table(records: list[Record]) -> str:
    by_peer: dict[str, list[Record]] = {}
    for rec in records:
        if rec.marked and rec.direction in DIRECTIONS:
            by_peer.setdefault(rec.peer, []).append(rec)
    lines = []
    for peer in sorted(by_peer):
        rows = sorted(by_peer[peer], key=lambda r: (r.direction or "", r.name))
        counts = {
            d: sum(1 for r in rows if r.direction == d) for d in sorted(DIRECTIONS)
        }
        lines.append(
            f"── {peer}  (inbound={counts['inbound']} "
            f"outbound={counts['outbound']} both={counts['both']})"
        )
        for rec in rows:
            if rec.direction == "both":
                pair = "both-ways"
            elif rec.reverse_of:
                pair = f"reverse_of {rec.reverse_of}"
            else:
                pair = f"one-way: {(rec.one_way_reason or '')[:60]}"
            lines.append(f"   {rec.direction:8} {rec.name}  [{pair}]")
    return "\n".join(lines)


def main() -> int:
    records = collect()
    marked = [r for r in records if r.marked]
    print(_coverage_table(records))
    print()
    total_fed = sum(
        1
        for r in records
        if (r.fixtures & PEER_FIXTURES) or r.file.endswith("_federation.py")
    )
    print(
        f"{len(marked)} federation-marked tests; "
        f"{total_fed} tests take a peer fixture or live in a *_federation.py module."
    )
    violations = audit()
    if not violations:
        print("\nVIOLATIONS: none — the pairing manifest is complete and mutual.")
        return 0
    print(f"\nVIOLATIONS ({len(violations)}):")
    for v in violations:
        print(f"  {format(v)}")
    return 1


if __name__ == "__main__":
    sys.exit(main())
