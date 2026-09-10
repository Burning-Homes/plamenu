"""FEP-044f quote posts federate in both directions.

Quoting a remote post is a handshake: the quoter sends a QuoteRequest, the
quoted side answers with an Accept carrying a QuoteAuthorization stamp, and
the quoter records the approval. Both roles are exercised here (alice's
default quote policy on Mastodon is "public", so her Accepts are automatic).
"""

import pytest
from plamenu_e2e import config
from plamenu_e2e.api import ApiError
from plamenu_e2e.steps import log, step, wait_for


@pytest.mark.federation(
    direction="inbound", reverse_of="test_plamenu_quote_of_mastodon_post"
)
def test_mastodon_quote_of_plamenu_post(alice, plamenu_user, cli, db, marker):
    """Covers: inbound QuoteRequest -> our Accept with a QuoteAuthorization
    stamp, Mastodon's re-fetch/stamp-verification to state=accepted, the
    quotes row on our side, and the 'quote' notification for the quoted user."""
    with step(f"alice follows @{plamenu_user.acct}"):
        account = alice.resolve_account(plamenu_user.acct)
        assert account, "Mastodon could not resolve the account"
        alice.follow(account["id"])
        wait_for(
            lambda: alice.relationship(account["id"])["following"],
            desc="the follow to be accepted before posting",
        )

    with step("post from Plamenu; find it on alice's home timeline"):
        cli.post(plamenu_user.username, f"Quote me if you dare! {marker}")
        quoted = wait_for(
            lambda: alice.home_status_containing(marker),
            desc="the Plamenu post to appear on alice's home timeline",
        )

    with step("alice quotes it (QuoteRequest -> our Accept + stamp)"):
        quote = alice.post_status(
            f"Look at this! {marker}", quoted_status_id=quoted["id"]
        )
        wait_for(
            lambda: alice.quote_state(quote["id"]) == "accepted",
            timeout=120,
            desc="the Mastodon-side quote to reach state=accepted (stamp verified)",
        )

    with step("plamenu recorded the quote and notified the quoted user"):
        # plamenu accepts the quote row on its own schedule (it verifies the
        # stamp on the inbound quote status), independent of Mastodon's state
        wait_for(
            lambda: db.quote_state_for_quoted(plamenu_user.username) == "accepted",
            desc="plamenu's quote row to become accepted",
        )
        wait_for(
            lambda: db.notification_count(plamenu_user.username, "quote") >= 1,
            desc="a quote notification for the quoted Plamenu user",
        )


@pytest.mark.federation(
    direction="outbound", reverse_of="test_mastodon_quote_of_plamenu_post"
)
def test_plamenu_quote_of_mastodon_post(
    alice, plamenu_user, plamenu_api, cli, db, marker
):
    """Covers: outbound QuoteRequest from a client-API quote post, the fresh
    quote starting as 'pending', inbound Accept -> state=accepted with the
    approval URI recorded, and the accepted state visible via the client API."""
    with step(f"@{plamenu_user.username} follows {config.ALICE}"):
        cli.follow(plamenu_user.username, config.ALICE)
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the outbound follow to be accepted (pending=false)",
        )

    with step("alice posts; wait for the status to reach Plamenu"):
        alice.post_status(f"Original thought {marker}")
        target_id = wait_for(
            lambda: db.status_id_containing(marker),
            desc="the Mastodon post to arrive in plamenu's statuses table",
        )

    with step("quote alice's post from Plamenu (QuoteRequest -> Mastodon Accept)"):
        quote = plamenu_api.post_status(
            f"Replying with a quote {marker}", quoted_status_id=target_id
        )
        state = (quote.get("quote") or {}).get("state", "")
        assert state == "pending", (
            f"fresh remote quote should be pending (state={state})"
        )

        def accepted_with_approval():
            row = db.quote_for_status(int(quote["id"]))
            if row and row[0] == "accepted" and (row[1] or "").startswith("http"):
                return row
            return None

        state, approval_uri = wait_for(
            accepted_with_approval,
            timeout=120,
            desc="the quote row to become accepted with an http(s) approval URI",
        )
        log(f"approval recorded: {approval_uri}")

    with step("the client API shows the accepted quote"):
        assert plamenu_api.quote_state(quote["id"]) == "accepted"


@pytest.mark.federation(
    direction="outbound", reverse_of="test_mastodon_revokes_a_quote_it_granted"
)
def test_plamenu_revokes_an_accepted_quote(
    alice, plamenu_user, plamenu_api, cli, db, marker
):
    """Covers: the quoted author revoking a previously-granted quote. Alice
    (Mastodon) quotes a Plamenu post; the Plamenu author revokes it via
    `quotes/{id}/revoke`, and Mastodon drops the accepted state when it receives
    our `Delete(QuoteAuthorization)`."""
    with step(f"alice follows @{plamenu_user.acct} and quotes a Plamenu post"):
        account = alice.resolve_account(plamenu_user.acct)
        assert account, "Mastodon could not resolve the account"
        alice.follow(account["id"])
        wait_for(
            lambda: alice.relationship(account["id"])["following"],
            desc="the follow to be accepted before posting",
        )
        cli.post(plamenu_user.username, f"Quote then revoke {marker}")
        quoted = wait_for(
            lambda: alice.home_status_containing(marker),
            desc="the Plamenu post to appear on alice's home timeline",
        )
        quote = alice.post_status(
            f"Look at this! {marker}", quoted_status_id=quoted["id"]
        )
        wait_for(
            lambda: alice.quote_state(quote["id"]) == "accepted",
            timeout=120,
            desc="the Mastodon-side quote to reach state=accepted",
        )

    with step("the Plamenu author revokes the quote"):
        quoted_local_id = wait_for(
            lambda: db.status_id_containing(f"Quote then revoke {marker}"),
            desc="the local quoted status id",
        )
        quoting_id = wait_for(
            lambda: (
                listing[0]["id"]
                if (
                    listing := plamenu_api.get(
                        f"/api/v1/statuses/{quoted_local_id}/quotes"
                    )
                )
                else None
            ),
            desc="the quoting status to appear under the quotes listing",
        )
        plamenu_api.post(
            f"/api/v1/statuses/{quoted_local_id}/quotes/{quoting_id}/revoke"
        )
        log(f"revoked quote of local status {quoted_local_id} by {quoting_id}")

    with step("Mastodon drops the accepted quote on receiving our Delete"):
        wait_for(
            lambda: alice.quote_state(quote["id"]) != "accepted",
            timeout=120,
            desc="the Mastodon-side quote to leave state=accepted after revocation",
        )


@pytest.mark.federation(
    direction="inbound", reverse_of="test_plamenu_revokes_an_accepted_quote"
)
def test_mastodon_revokes_a_quote_it_granted(
    alice, plamenu_user, plamenu_api, cli, db, marker
):
    """Covers M31's inbound `Delete(QuoteAuthorization)`: a Plamenu quote of a
    Mastodon post is accepted, then the Mastodon author revokes it via
    `quotes/{id}/revoke`; Mastodon federates a Delete of the authorization
    stamp and the Plamenu quote row leaves state=accepted (revoked, stamp
    cleared)."""
    with step(f"@{plamenu_user.username} follows {config.ALICE}"):
        cli.follow(plamenu_user.username, config.ALICE)
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the outbound follow to be accepted (pending=false)",
        )

    with step("alice posts; Plamenu quotes it; the quote is accepted"):
        alice_status = alice.post_status(f"Revocable wisdom {marker}")
        target_id = wait_for(
            lambda: db.status_id_containing(marker),
            desc="the Mastodon post to arrive in plamenu's statuses table",
        )
        quote = plamenu_api.post_status(
            f"Quoting while it lasts {marker}", quoted_status_id=target_id
        )
        wait_for(
            lambda: (
                (row := db.quote_for_status(int(quote["id"]))) and row[0] == "accepted"
            ),
            timeout=120,
            desc="the quote row to become accepted",
        )

    with step("alice revokes the authorization she granted"):
        quoting_mastodon_id = wait_for(
            lambda: next(
                (
                    s["id"]
                    for s in alice.get(f"/api/v1/statuses/{alice_status['id']}/quotes")
                    if marker in s["content"]
                ),
                None,
            ),
            timeout=120,
            desc="the quoting post to appear in alice's quotes listing",
        )
        alice.post(
            f"/api/v1/statuses/{alice_status['id']}/quotes/{quoting_mastodon_id}/revoke"
        )
        log(f"alice revoked quote {quoting_mastodon_id}")

    with step("plamenu's quote is revoked by the inbound Delete"):
        state, approval = wait_for(
            lambda: (
                row
                if (row := db.quote_for_status(int(quote["id"])))
                and row[0] != "accepted"
                else None
            ),
            timeout=120,
            desc="the plamenu quote row to leave state=accepted",
        )
        assert state == "revoked", f"expected revoked, got {state}"
        assert not approval, f"the stamp should be cleared, got {approval}"


@pytest.mark.federation(
    direction="inbound",
    one_way_reason="same-direction refinement of test_mastodon_quote_of_plamenu_post: a followers-only interaction policy gates a non-follower's inbound QuoteRequest; no reverse.",
)
def test_followers_only_quote_policy_gates_a_stranger(
    alice, plamenu_user, plamenu_api, cli, db, marker
):
    """Covers: a followers-only quote policy set through `interaction_policy`,
    federated as the Note's `interactionPolicy`. A Mastodon account that does
    not follow the Plamenu author is refused; once it follows, the quote is
    accepted (its QuoteRequest reaches our policy check and is granted)."""
    with step("plamenu_user posts and restricts quoting to followers"):
        cli.post(plamenu_user.username, f"followers only {marker}")
        local_id = wait_for(
            lambda: db.status_id_containing(f"followers only {marker}"),
            desc="the local status id",
        )
        plamenu_api.patch(
            f"/api/v1/statuses/{local_id}/interaction_policy",
            quote_approval_policy="followers",
        )
        uri = plamenu_api.get_status(str(local_id))["uri"]

    with step("a non-following alice resolves the post but may not quote it"):
        statuses = wait_for(
            lambda: alice.search(uri, resolve=True)["statuses"] or None,
            desc="Mastodon to resolve the Plamenu post",
        )

        # Mastodon reads our federated interactionPolicy: a non-follower is
        # refused before any QuoteRequest is even sent.
        def quote_is_refused():
            try:
                alice.post_status(
                    f"sneaky quote {marker}", quoted_status_id=statuses[0]["id"]
                )
                return False
            except ApiError as err:
                return "quote" in str(err).lower()

        wait_for(
            quote_is_refused,
            timeout=120,
            desc="Mastodon to refuse a non-follower's quote per our policy",
        )

    with step("after following, the same author may quote"):
        account = alice.resolve_account(plamenu_user.acct)
        alice.follow(account["id"])
        wait_for(
            lambda: alice.relationship(account["id"])["following"],
            desc="the follow to be accepted",
        )

        quote_holder = {"quote": None}

        def quote_now_accepted():
            if quote_holder["quote"] is None:
                try:
                    quote_holder["quote"] = alice.post_status(
                        f"now allowed {marker}", quoted_status_id=statuses[0]["id"]
                    )
                except ApiError:
                    return None
            try:
                state = alice.quote_state(quote_holder["quote"]["id"])
            except ApiError:
                return None
            return quote_holder["quote"] if state == "accepted" else None

        wait_for(
            quote_now_accepted,
            timeout=120,
            desc="a follower's quote to be accepted",
        )
