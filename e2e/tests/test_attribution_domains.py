"""`attributionDomains` federation (Mastodon's `fediverse:creator`
authorization list): the account property rides the actor document in both
directions via `Update(Actor)`.

- Plamenu → Mastodon: setting the list through `update_credentials` reaches
  Mastodon's stored remote account.
- Mastodon → Plamenu: a Mastodon user's list lands in Plamenu's accounts row.
"""

import pytest
from plamenu_e2e import config, mastodon
from plamenu_e2e.steps import step, wait_for


@pytest.mark.federation(
    direction="outbound", reverse_of="test_attribution_domains_federate_from_mastodon"
)
def test_attribution_domains_federate_to_mastodon(alice, plamenu_user, plamenu_api):
    with step(f"alice follows @{plamenu_user.acct} (audience for the Update)"):
        account = alice.resolve_account(plamenu_user.acct)
        assert account, "Mastodon could not resolve the Plamenu user"
        alice.follow(account["id"])
        wait_for(
            lambda: alice.relationship(account["id"])["following"],
            desc="the follow to be accepted",
        )

    with step("the Plamenu user authorizes a domain; Mastodon stores it"):
        body = plamenu_api.patch(
            "/api/v1/accounts/update_credentials",
            **{"attribution_domains[]": "press.example"},
        )
        assert body["source"]["attribution_domains"] == ["press.example"]
        wait_for(
            lambda: (
                mastodon.remote_attribution_domains(
                    plamenu_user.username, config.PLAMENU_DOMAIN
                )
                == "press.example"
            ),
            desc="Mastodon to store the attribution domain",
        )


@pytest.mark.federation(
    direction="inbound", reverse_of="test_attribution_domains_federate_to_mastodon"
)
def test_attribution_domains_federate_from_mastodon(
    alice, cli, db, plamenu_user, marker
):
    victim = f"attrib{marker[-8:]}"
    with step(f"a fresh Mastodon account @{victim} is followed from Plamenu"):
        mastodon.create_account(victim)
        cli.follow(plamenu_user.username, f"{victim}@mastodon.local")
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the follow to be accepted",
        )

    with step("they authorize a domain; Plamenu stores it off the Update"):
        victim_api = mastodon.api_as(f"{victim}@mastodon.local")
        victim_api.patch(
            "/api/v1/accounts/update_credentials",
            **{"attribution_domains[]": "papers.example"},
        )
        wait_for(
            lambda: (
                db.attribution_domains(victim, "mastodon.local") == ["papers.example"]
            ),
            desc="the inbound Update to carry attributionDomains",
        )
