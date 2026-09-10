"""Differential response-shape checking against a live Mastodon.

3rd-party clients are coded to Mastodon's *exact* response shapes: the root
key an `adapter: :json` render wraps a payload in, whether a list comes back
bare or enveloped, which fields exist and their JSON types. Plamenu drifting
from any of these silently breaks real clients while shape-agnostic tests stay
green (the account-collections regression: a bare array where Mastodon sends
`{"collections": [...]}`).

`skeleton()` reduces a JSON value to keys + types (values erased). `null` is a
wildcard because nullable fields vary by fixture. `assert_same_shape()` runs
the same operation on Mastodon and Plamenu and diffs the two skeletons, so the
check is a live differential against the pinned peer, not a hand-copied schema
that rots.
"""

WILDCARD = "*"  # a null on either side — nullable field, matches any type


def skeleton(value):
    """Keys + types of a JSON value; scalar values erased, null -> wildcard."""
    if isinstance(value, dict):
        return {k: skeleton(v) for k, v in value.items()}
    if isinstance(value, list):
        # Merge every element's skeleton so an optional key present on only
        # some elements is still represented (list order/length don't matter).
        merged: dict = {}
        element = None
        for item in value:
            element = skeleton(item)
            if isinstance(element, dict):
                for k, v in element.items():
                    merged[k] = v if k not in merged else _merge(merged[k], v)
        if merged:
            return ["<list>", merged]
        return ["<list>", element] if value else ["<list>"]
    if value is None:
        return WILDCARD
    if isinstance(value, bool):
        return "bool"
    if isinstance(value, str):
        return "str"
    if isinstance(value, int):
        return "int"
    if isinstance(value, float):
        return "float"
    return type(value).__name__


def _merge(a, b):
    return a if a != WILDCARD else b


def diff(expected, actual, path="", allow_extra=frozenset(), reference="Mastodon"):
    """Return a list of human-readable divergences of `actual` from `expected`
    (Mastodon is `expected`). Wildcards match anything; ints match floats.

    `allow_extra` is a set of key names Plamenu may add that Mastodon lacks
    (intentional additive extensions, e.g. the `pleroma` object) — an EXTRA
    key by one of those names, at any depth, is not reported.

    `reference` names whatever `expected` came from, for the messages: the
    Plamenu-against-Plamenu tests diff a post against the author's own
    rendering of it rather than against a peer."""
    problems: list[str] = []
    if expected == WILDCARD or actual == WILDCARD:
        return problems
    if isinstance(expected, dict) and isinstance(actual, dict):
        for key in expected.keys() | actual.keys():
            here = f"{path}.{key}" if path else key
            if key not in actual:
                problems.append(f"{here}: MISSING ({reference} has {expected[key]!r})")
            elif key not in expected:
                if key not in allow_extra:
                    problems.append(f"{here}: EXTRA ({reference} has no such key)")
            else:
                problems += diff(
                    expected[key], actual[key], here, allow_extra, reference
                )
        return problems
    if isinstance(expected, list) and isinstance(actual, list):
        if len(expected) > 1 and len(actual) > 1:
            problems += diff(
                expected[1], actual[1], f"{path}[]", allow_extra, reference
            )
        return problems
    # One side is a container and the other isn't (e.g. a wrapped object vs a
    # bare array) — a structural mismatch, and `expected`/`actual` may be
    # unhashable, so don't fall through to the scalar comparison below.
    if isinstance(expected, (dict, list)) or isinstance(actual, (dict, list)):
        problems.append(
            f"{path or '<root>'}: {_kind(actual)}, {reference} has {_kind(expected)}"
        )
        return problems
    if {expected, actual} == {"int", "float"}:
        return problems
    if expected != actual:
        problems.append(f"{path}: type {actual!r}, {reference} has {expected!r}")
    return problems


def _kind(value):
    if isinstance(value, dict):
        return "object{" + ",".join(sorted(value)) + "}"
    if isinstance(value, list):
        return "array"
    return repr(value)


def assert_same_shape(label, mastodon_body, plamenu_body, *, ignore=(), allow_extra=()):
    """Assert Plamenu's response skeleton matches Mastodon's.

    `ignore` is a set of top-level keys to skip (e.g. deeply nested embedded
    entities whose parity is a separate concern, like `accounts` in the
    collection `show` response — account-serializer parity is its own test).
    `allow_extra` is a set of key names (any depth) that Plamenu may add as
    intentional additive extensions Mastodon lacks (e.g. `pleroma`)."""
    exp = skeleton(mastodon_body)
    act = skeleton(plamenu_body)
    for key in ignore:
        if isinstance(exp, dict):
            exp.pop(key, None)
        if isinstance(act, dict):
            act.pop(key, None)
    problems = diff(exp, act, allow_extra=frozenset(allow_extra))
    assert not problems, (
        f"{label}: Plamenu response shape diverges from Mastodon:\n  - "
        + "\n  - ".join(problems)
        + f"\n\nMastodon: {mastodon_body!r}\nPlamenu:  {plamenu_body!r}"
    )
