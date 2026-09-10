"""Step reporting and polling.

Wrap each logical phase of a test in `with step("...")`. The step titles are
printed to (pytest-captured) stdout, and when a step body raises, a
`!! FAILED during step:` line is appended — so a test failure always names
the exact step that broke, right above the traceback.
"""

import time
from contextlib import contextmanager


@contextmanager
def step(title: str):
    print(f"\n>> {title}", flush=True)
    try:
        yield
    except BaseException:
        print(f"!! FAILED during step: {title}", flush=True)
        raise


def log(detail: str):
    """An indented detail line under the current step."""
    print(f"   {detail}", flush=True)


def wait_for(check, *, desc: str, timeout: float = 90.0, interval: float = 1.0):
    """Poll `check()` until it returns a truthy value, then return that value.

    Federation is asynchronous on both sides (delivery queues, Sidekiq), so
    every cross-instance assertion goes through here. On timeout, raises
    TimeoutError naming what was awaited and the last value seen.

    The timeout is deliberately generous: polling returns the instant the
    condition holds, so a large value never slows a passing run — it only
    delays the report of a genuine failure. Tight timeouts are what made
    the suite flake when Sidekiq lagged under full-suite load; do not
    "optimize" them back down, and avoid per-call values below the default.
    """
    deadline = time.monotonic() + timeout
    while True:
        value = check()
        if value:
            return value
        if time.monotonic() >= deadline:
            raise TimeoutError(
                f"gave up after {timeout:.0f}s waiting for {desc} (last value: {value!r})"
            )
        time.sleep(interval)
