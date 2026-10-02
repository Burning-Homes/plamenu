# Known issues

## Live notifications disrupt navigation and scroll position

Enabling **Live notifications** in the built-in web client can disrupt
navigation context and scroll position when returning to a previous page.
Live notifications are disabled by default while this issue remains unresolved.

## Staging 0.7.0 live-video qualification limits (2 October 2026)

The staging candidate retains the existing live player and progressive remux.
Experimental changes to live buffering recovery, native segment selection,
automatic playback-speed catchup and peer scheduling are excluded.
The original test budgets remain unchanged; the following results remain failures
or incomplete qualifications rather than passes.

- Progressive live playback with JavaScript disabled can buffer for approximately
  two seconds and exceed the 10-second decoded latency budget. A related-source
  screen trial measured 10.9 seconds; a later isolated segment-selection trial
  also measured 10.9 seconds and approximately 2.08 seconds of lost progress.
  The latter trial does not qualify the unchanged staging implementation.
- PeerTube live playback does not reliably demonstrate validated inbound peer
  segments in the original first-party-player test. HTTP playback remains a
  separate delivery path; successful playback is not proof of peer delivery.
- The original two-broadcast/eight-decoder workload has not met its decoded live
  latency gate. A prior revision with byte-identical live-player and remux source
  measured a combined p95 of 10.9 seconds against the unchanged 10-second budget.
  Later experimental changes passed live peer exchange but regressed presented
  frames and latency; those changes are excluded.

The earlier browser replica-EIO case stopped in its six-second healthy no-JS
phase, before injecting any fault, because the dropped-frame counter increased
by two. It does not demonstrate a replica-EIO storage or recovery defect. A later
original case passed all fault and repair phases on an isolated candidate with
byte-identical remux/backend code; its native JS player used experimental recovery,
so this is not an exact unchanged-source whole-case pass. Healthy no-JS smoothness
and exact revision coverage remain incomplete.

The earlier PeerTube adversarial VOD exchange startup failure subsequently passed
the original test on the same baseline plus a change restricted to the live-only
handler. VOD does not execute that handler, and all other product files are
identical. This establishes a passing unchanged VOD implementation path without
claiming an exact unchanged-source whole-suite run. There is no evidence of corrupt
or stale byte acceptance.

Full-suite validation has not been rerun for this staging version. The signed
baseline has passed static checks, Clippy, 2,815 Rust tests with no skipped tests,
and an independent 1,742-query SQLx comparison. All 70 performance measurements
had zero hard-budget failures and all three contention cases passed; the strict
performance checker still reported 10 headroom warnings and 32 lower-query-count
warnings. This is a staging candidate, not a fully green release declaration.
