"""IRI-valued ActivityPub Create delivery against a real peer origin."""

import pytest
from plamenu_e2e import apsign, config, mastodon, redeliver, unique
from plamenu_e2e.steps import step, wait_for


@pytest.mark.federation(
    direction="inbound",
    one_way_reason=(
        "IRI-valued Create receive support: the object is dereferenced from its "
        "origin before ordinary delivery ingestion; this has no outbound counterpart"
    ),
)
def test_iri_valued_create_dereferences_and_ingests(plamenu_api, db, marker):
    """A genuine Mastodon actor signs a Create that references, rather than
    embeds, its public Note. Plamenu must fetch that Note from Mastodon, ingest
    it once, and keep a replay idempotent.

    Mastodon normally embeds the Note in its outbound Create, so the harness
    deliberately builds this standards-valid envelope while retaining the
    real actor key and real dereferenceable Mastodon object.
    """
    username = unique("refcreate")
    acct = f"{username}@{config.MASTODON_DOMAIN}"
    with step(f"create a fresh Mastodon author @{acct} and an uncached Note"):
        mastodon.create_account(username)
        author = mastodon.api_as(f"{username}@mastodon.local")
        remote = author.post_status(f"IRI-valued Create {marker}")
        note_uri = remote["uri"]
        assert db.status_id_containing(marker) is None

    with step("deliver a real actor-signed Create carrying the Note as an IRI"):
        key_id, private_key = apsign.mastodon_signer(username)
        actor_uri = key_id.split("#", 1)[0]
        activity = {
            "@context": redeliver.AS2_CONTEXT,
            "id": f"{note_uri}#reference-create",
            "type": "Create",
            "actor": actor_uri,
            "to": [redeliver.PUBLIC],
            "object": note_uri,
        }
        assert (
            redeliver.deliver(
                activity,
                actor_uri=actor_uri,
                private_key_pem=private_key,
                key_id=key_id,
            )
            == 202
        )
        stored_id = wait_for(
            lambda: db.status_id_containing(marker),
            desc="the referenced Mastodon Note to be fetched and stored",
        )
        hits = plamenu_api.search(marker, type="statuses")["statuses"]
        assert hits and hits[0]["uri"] == note_uri, hits

    with step("replay the same reference Create without duplicating the status"):
        assert (
            redeliver.deliver(
                activity,
                actor_uri=actor_uri,
                private_key_pem=private_key,
                key_id=key_id,
            )
            == 202
        )
        assert db.status_id_containing(marker) == stored_id
