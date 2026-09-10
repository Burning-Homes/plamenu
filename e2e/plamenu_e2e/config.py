"""Endpoints and paths of the dev stack (see e2e/README.md for the wiring)."""

import os
from pathlib import Path

PLAMENU_URL = os.environ.get("PLAMENU_URL", "https://plamenu.local")
# The second Plamenu instance — same software, started by the e2e session
# itself (plamenu_e2e/ephemeral.py), so Plamenu's emission finally meets a
# reader that implements all of it.
PLAMENU2_URL = os.environ.get("PLAMENU2_URL", "https://plamenu2.local")
MASTODON_URL = os.environ.get("MASTODON_URL", "https://mastodon.local")
PLEROMA_URL = os.environ.get("PLEROMA_URL", "https://pleroma.local")
SHARKEY_URL = os.environ.get("SHARKEY_URL", "https://sharkey.local")
GOTOSOCIAL_URL = os.environ.get("GOTOSOCIAL_URL", "https://gotosocial.local")
MITRA_URL = os.environ.get("MITRA_URL", "https://mitra.local")
# The onion-identity Mitra (tor-test rig). Tests drive its client API
# directly over loopback; only server-to-server traffic from Plamenu rides
# Tor, which is the thing under test.
ONION_MITRA_URL = os.environ.get("ONION_MITRA_URL", "http://127.0.0.1:8381")
LEMMY_URL = os.environ.get("LEMMY_URL", "https://lemmy.local")
PEERTUBE_URL = os.environ.get("PEERTUBE_URL", "https://peertube.local")
OWNCAST_URL = os.environ.get("OWNCAST_URL", "https://owncast.local")
MOBILIZON_URL = os.environ.get("MOBILIZON_URL", "https://mobilizon.local")
DISCOURSE_URL = os.environ.get("DISCOURSE_URL", "https://discourse.local")
HUBZILLA_URL = os.environ.get("HUBZILLA_URL", "https://hubzilla.local")
FUNKWHALE_URL = os.environ.get("FUNKWHALE_URL", "https://funkwhale.local")
# Upstream Pleroma (real 2.10.2), a SEPARATE peer from the Akkoma `pleroma` one.
PLUP_URL = os.environ.get("PLUP_URL", "https://plup.local")
PLAMENU_DOMAIN = PLAMENU_URL.removeprefix("https://")
PLAMENU2_DOMAIN = PLAMENU2_URL.removeprefix("https://")
MASTODON_DOMAIN = MASTODON_URL.removeprefix("https://")
PLEROMA_DOMAIN = PLEROMA_URL.removeprefix("https://")
SHARKEY_DOMAIN = SHARKEY_URL.removeprefix("https://")
GOTOSOCIAL_DOMAIN = GOTOSOCIAL_URL.removeprefix("https://")
MITRA_DOMAIN = MITRA_URL.removeprefix("https://")
LEMMY_DOMAIN = LEMMY_URL.removeprefix("https://")
PEERTUBE_DOMAIN = PEERTUBE_URL.removeprefix("https://")
OWNCAST_DOMAIN = OWNCAST_URL.removeprefix("https://")
MOBILIZON_DOMAIN = MOBILIZON_URL.removeprefix("https://")
DISCOURSE_DOMAIN = DISCOURSE_URL.removeprefix("https://")
HUBZILLA_DOMAIN = HUBZILLA_URL.removeprefix("https://")
FUNKWHALE_DOMAIN = FUNKWHALE_URL.removeprefix("https://")
PLUP_DOMAIN = PLUP_URL.removeprefix("https://")

DB_DSN = os.environ.get(
    "DATABASE_URL", "postgres://plamenu:plamenu@127.0.0.1:5433/plamenu_dev"
)

E2E_DIR = Path(__file__).resolve().parents[1]
PLAMENU_DIR = E2E_DIR.parent

# Plamenu is TOML-config-only; `./dev up plamenu` writes this throwaway file and
# the CLI helper points every `plamenu` invocation at it (there is no default
# `plamenu.toml` in the dev tree). Override with $PLAMENU_CONFIG if needed.
PLAMENU_CONFIG = os.environ.get("PLAMENU_CONFIG", str(PLAMENU_DIR / "plamenu.dev.toml"))
PEER_ROOT = Path(os.environ.get("PLAMENU_PEER_ROOT", PLAMENU_DIR / "e2e" / "peers"))
MASTO_DIR = Path(os.environ.get("MASTODON_FIXTURE_DIR", PEER_ROOT / "mastodon-test"))
PLEROMA_DIR = Path(os.environ.get("PLEROMA_FIXTURE_DIR", PEER_ROOT / "pleroma-test"))
SHARKEY_DIR = Path(os.environ.get("SHARKEY_FIXTURE_DIR", PEER_ROOT / "sharkey-test"))
GOTOSOCIAL_DIR = Path(
    os.environ.get("GOTOSOCIAL_FIXTURE_DIR", PEER_ROOT / "gotosocial-test")
)
MITRA_DIR = Path(os.environ.get("MITRA_FIXTURE_DIR", PEER_ROOT / "mitra-test"))
TOR_DIR = Path(os.environ.get("TOR_FIXTURE_DIR", PEER_ROOT / "tor-test"))
# Mitra runs from source (only its DB is in docker); its `mitra` binary carries
# the CLI subcommands (e.g. `send-activity --rfc9421`) the 9421 tests drive.
MITRA_SOURCE_DIR = Path(os.environ.get("MITRA_SOURCE_DIR", PEER_ROOT / "mitra-source"))
LEMMY_DIR = Path(os.environ.get("LEMMY_FIXTURE_DIR", PEER_ROOT / "lemmy-test"))
PEERTUBE_DIR = Path(os.environ.get("PEERTUBE_FIXTURE_DIR", PEER_ROOT / "peertube-test"))
OWNCAST_DIR = Path(os.environ.get("OWNCAST_FIXTURE_DIR", PEER_ROOT / "owncast-test"))
MOBILIZON_DIR = Path(
    os.environ.get("MOBILIZON_FIXTURE_DIR", PEER_ROOT / "mobilizon-test")
)
DISCOURSE_DIR = Path(
    os.environ.get("DISCOURSE_FIXTURE_DIR", PEER_ROOT / "discourse-test")
)
HUBZILLA_DIR = Path(os.environ.get("HUBZILLA_FIXTURE_DIR", PEER_ROOT / "hubzilla-test"))
FUNKWHALE_DIR = Path(
    os.environ.get("FUNKWHALE_FIXTURE_DIR", PEER_ROOT / "funkwhale-test")
)
PLUP_DIR = Path(os.environ.get("PLUP_FIXTURE_DIR", PEER_ROOT / "pleroma-upstream-test"))

# Standing test account on the Mastodon side (created by the mastodon-test
# stack); all Mastodon-side actions in the tests run as alice.
ALICE = f"alice@{MASTODON_DOMAIN}"
ALICE_EMAIL = "alice@mastodon.local"
