"""Fixture-only private-network allowance, restricted to exact TLS peer names.

Songhive's explicit lookup forbids private DNS results. Our disposable peers
live behind a private Docker TLS proxy. Keep URL checks and redirect checking;
permit only these known HTTPS origins. This module is never in the application
or upstream image, and does not change signature verification or TLS trust.
"""

from urllib.parse import urlparse

from songhive.federation import fetch

_original = fetch.validate_fetch_url
_PEERS = frozenset(
    {
        "plamenu.local",
        "plamenu2.local",
        "mastodon.local",
        "songhive.local",
        "funkwhale.local",
    }
)


def _validate(url):
    parsed = urlparse(url)
    if (
        parsed.scheme == "https"
        and parsed.hostname in _PEERS
        and parsed.port in (None, 443)
        and parsed.username is None
        and parsed.password is None
    ):
        return
    _original(url)


fetch.validate_fetch_url = _validate
