"""Helper library for Plamenu's end-to-end federation tests.

Modules:
  config   — endpoints, paths and credentials of the dev stack
  shell    — subprocess + docker compose plumbing (sg-docker fallback)
  api      — minimal Mastodon-API client (works against Plamenu and Mastodon)
  mastodon — Mastodon-side helpers (token minting, account creation)
  plamenu  — Plamenu-side helpers (CLI wrapper, OAuth login)
  db       — read-only queries against Plamenu's Postgres
  steps    — step/wait_for primitives that make failures point at a step
"""

import secrets


def unique(prefix: str) -> str:
    """A collision-free lowercase-alphanumeric name (valid username/hashtag)."""
    return f"{prefix}{secrets.token_hex(4)}"
