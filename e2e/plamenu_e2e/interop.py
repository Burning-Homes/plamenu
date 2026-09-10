"""Cross-instance choreography shared by the Plamenu-against-Plamenu tests.

Both sides of these tests speak the same client API, so the setup steps
(resolve a handle, follow it, wait for the Accept, find the delivered post)
are symmetric — one helper serves either direction. Keeping them here rather
than in a test module means a peer whose behaviour changes is fixed in one
place, and it keeps the tests themselves about the thing under test.
"""

import re

from .api import Api
from .steps import wait_for

# A receiver sanitizes inbound HTML the way Mastodon does: every anchor comes
# out with `rel="nofollow noopener noreferrer"` and no `target`, whatever the
# author wrote there (a hashtag's `rel="tag"` and a profile link's `rel="me"`
# included). Everything else about the markup — structure, hrefs, classes —
# must survive verbatim, so comparisons of an author's rendering against a
# reader's drop only the attributes the reader is entitled to rewrite.
_READER_REWRITTEN = re.compile(r'\s(?:rel|target)="[^"]*"')


def as_a_reader_rewrites_it(html: str) -> str:
    """`html` with the link attributes an ingesting server rewrites removed."""
    return _READER_REWRITTEN.sub("", html)


def resolve(api: Api, acct: str) -> dict:
    """Resolve `user@domain` from `api`'s instance (webfinger + actor fetch).

    Polled: the first resolve of a peer runs a live fetch, and a peer that
    was unreachable a moment ago is retried on its own schedule."""
    account = wait_for(
        lambda: api.resolve_account(acct),
        desc=f"{api.base_url} to resolve {acct}",
    )
    assert account["acct"] == acct, account
    return account


def resolve_group(api: Api, acct: str) -> dict:
    """Resolve `name@domain` as a *Group* actor.

    The sigil is the namespace: `@handle` asks for a Person and `!handle` for a
    Group, because a peer may serve both under one name (Lemmy does)."""
    account = wait_for(
        lambda: next(
            iter(api.search(f"!{acct}", resolve=True, type="accounts")["accounts"]),
            None,
        ),
        desc=f"{api.base_url} to resolve the group !{acct}",
    )
    assert account["acct"] == acct, account
    return account


def follow(api: Api, acct: str) -> dict:
    """Resolve and follow `acct`, waiting for the Accept. Returns the account."""
    account = resolve(api, acct)
    api.follow(account["id"])
    wait_for(
        lambda: api.relationship(account["id"])["following"],
        desc=f"{acct} to accept the follow from {api.base_url}",
    )
    return account


def mutual_follow(
    api_a: Api, acct_a: str, api_b: Api, acct_b: str
) -> tuple[dict, dict]:
    """A follows B and B follows A. Returns (B as A sees it, A as B sees it)."""
    return follow(api_a, acct_b), follow(api_b, acct_a)


def delivered(api: Api, marker: str) -> dict:
    """The home-timeline status carrying `marker`, once it has been delivered."""
    return wait_for(
        lambda: api.home_status_containing(marker),
        desc=f"the post carrying {marker} to reach {api.base_url}",
    )


def ingested(api: Api, uri: str) -> dict:
    """The status with `uri` as `api`'s instance stored it, fetching it if the
    instance has not seen it yet (search-by-URL with resolve)."""
    return wait_for(
        lambda: api.resolve_status(uri),
        desc=f"{api.base_url} to ingest {uri}",
    )
