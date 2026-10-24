"""Observe async state sooner without shortening deadlines or flooding workers."""

import sys
import unittest
from itertools import pairwise
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "e2e"))
from plamenu_e2e import steps


class Clock:
    def __init__(self):
        self.now = 0.0
        self.sleeps = []

    def monotonic(self):
        return self.now

    def sleep(self, seconds):
        assert seconds > 0
        self.sleeps.append(seconds)
        self.now += seconds


class PollingTests(unittest.TestCase):
    def test_fast_readiness_returns_original_value_before_the_old_one_second_wait(self):
        for ready_at in (0.05, 0.2, 0.4, 0.8):
            with self.subTest(ready_at=ready_at):
                clock = Clock()
                value = object()
                with (
                    patch.object(steps.time, "monotonic", side_effect=clock.monotonic),
                    patch.object(steps.time, "sleep", side_effect=clock.sleep),
                ):
                    result = steps.wait_for(
                        lambda value=value, clock=clock, ready_at=ready_at: (
                            value if clock.now >= ready_at else None
                        ),
                        desc="ready",
                    )
                self.assertIs(result, value)
                self.assertLessEqual(clock.now, 1.0)
                self.assertGreaterEqual(clock.now, ready_at)
                if ready_at <= 0.4:
                    self.assertLess(clock.now, 1.0)

    def test_absence_observes_the_full_timeout_with_at_most_three_extra_checks(self):
        clock = Clock()
        checks = []

        def absent():
            checks.append(clock.now)

        with (
            patch.object(steps.time, "monotonic", side_effect=clock.monotonic),
            patch.object(steps.time, "sleep", side_effect=clock.sleep),
            self.assertRaisesRegex(TimeoutError, "after 90s waiting for absent.*None"),
        ):
            steps.wait_for(absent, desc="absent")
        self.assertEqual(clock.now, 90.0)
        self.assertLessEqual(len(checks), 94)  # Old cadence: 91 observations.
        late = [now for now in checks if now >= 1]
        self.assertTrue(all(b - a >= 1 for a, b in pairwise(late)))

    def test_explicit_media_interval_keeps_its_cadence_and_full_deadline(self):
        clock = Clock()
        checks = []
        with (
            patch.object(steps.time, "monotonic", side_effect=clock.monotonic),
            patch.object(steps.time, "sleep", side_effect=clock.sleep),
            self.assertRaises(TimeoutError),
        ):
            steps.wait_for(
                lambda: checks.append(clock.now), desc="media", timeout=7, interval=3
            )
        self.assertEqual(checks, [0, 3, 6, 9])
        self.assertEqual(clock.now, 9)

    def test_slow_checks_skip_missed_slots_instead_of_bursting(self):
        clock = Clock()
        checks = []

        def slow():
            checks.append(clock.now)
            clock.now += 0.35
            return len(checks) == 4

        with (
            patch.object(steps.time, "monotonic", side_effect=clock.monotonic),
            patch.object(steps.time, "sleep", side_effect=clock.sleep),
        ):
            self.assertTrue(steps.wait_for(slow, desc="slow"))
        self.assertTrue(all(delay > 0 for delay in clock.sleeps))
        self.assertTrue(all(b - a >= 0.35 for a, b in pairwise(checks)))

    def test_success_and_errors_are_not_retried_or_masked(self):
        value = object()
        with patch.object(steps.time, "sleep") as sleep:
            self.assertIs(steps.wait_for(lambda: value, desc="immediate"), value)
            with self.assertRaisesRegex(RuntimeError, "check broke"):
                steps.wait_for(
                    lambda: (_ for _ in ()).throw(RuntimeError("check broke")),
                    desc="error",
                )
        sleep.assert_not_called()

    def test_zero_timeout_still_checks_once_and_invalid_interval_cannot_busy_poll(self):
        with self.assertRaises(TimeoutError):
            steps.wait_for(lambda: False, desc="one check", timeout=0)
        for interval in (0, -1):
            with self.subTest(interval=interval), self.assertRaises(ValueError):
                steps.wait_for(lambda: False, desc="invalid", interval=interval)
