"""In-suite gate: the federation-test pairing manifest must be complete.

This runs as an ordinary (fast, stack-free) pytest test. It calls the AST
engine in tools/federation_coverage.py directly — not `request.session.items` —
so it audits the WHOLE suite even under `pytest -k` or a single-file run: a
subset invocation still enforces global pairing. It takes no peer fixtures, so
it is never skipped.

For a stack-free local check outside pytest, run:
    uv run python tools/federation_coverage.py
"""

from tools import federation_coverage


def test_federation_pairing_is_complete():
    """Every directional federation test names its opposite-direction reverse_of
    (mutually) or a one_way_reason, and no peer-fixture / *_federation.py test
    escapes the marker. See tools/federation_coverage.py for the contract."""
    violations = federation_coverage.audit()
    assert not violations, "federation pairing manifest has gaps:\n" + "\n".join(
        federation_coverage.format(v) for v in violations
    )
