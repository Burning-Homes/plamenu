"""Live interoperability with Discourse's official ActivityPub plugin."""

import pytest
from plamenu_e2e import config
from plamenu_e2e.api import Api
from plamenu_e2e.discourse import DIANA_EMAIL, DiscourseApi
from plamenu_e2e.steps import step, wait_for


def _home_status_containing(api: Api, marker: str) -> dict | None:
    for entry in api.home_timeline(limit=40):
        status = entry.get("reblog") or entry
        if marker in status["content"]:
            return status
    return None


@pytest.mark.federation(direction="both")
def test_discourse_category_topic_and_reply_exchange(
    discourse_diana: DiscourseApi,
    plamenu_api: Api,
    plamenu_user,
    db,
    marker: str,
):
    """Resolve browser pages, accept a follow, then exchange a topic/reply."""
    topic = None
    group = None
    category_actor_id = discourse_diana.category_actor_id()
    db.forget_host(config.DISCOURSE_DOMAIN)

    try:
        with step("Plamenu resolves the Discourse category page as a Group"):
            category_url = discourse_diana.category_url()
            accounts = plamenu_api.search(category_url, resolve=True, type="accounts")[
                "accounts"
            ]
            assert accounts and accounts[0]["acct"] == (
                f"federation@{config.DISCOURSE_DOMAIN}"
            ), accounts
            group = accounts[0]
            assert group["group"] is True, group

        with step("Discourse accepts Plamenu's signed Follow"):
            plamenu_api.follow(group["id"])
            wait_for(
                lambda: plamenu_api.relationship(group["id"])["following"],
                desc="Discourse Accept(Follow) to reach Plamenu",
            )

        with step("Diana publishes a topic into the followed category"):
            topic = discourse_diana.create_topic(
                f"Plamenu E2E {marker}",
                f"Discourse to Plamenu live exchange {marker}",
            )
            received = wait_for(
                lambda: _home_status_containing(plamenu_api, marker),
                desc="Discourse topic to reach the Plamenu home timeline",
            )
            assert received["account"]["acct"] == (
                f"diana@{config.DISCOURSE_DOMAIN}"
            ), received

        with step("Plamenu resolves the browser topic URL to that same post"):
            topic_url = discourse_diana.topic_url(topic["topic_id"])
            statuses = plamenu_api.search(topic_url, resolve=True)["statuses"]
            assert statuses and statuses[0]["id"] == received["id"], statuses

        with step("a Plamenu reply is rendered in the Discourse topic"):
            reply_marker = f"reply{marker}"
            plamenu_api.post_status(
                f"Plamenu to Discourse live reply {reply_marker}",
                in_reply_to_id=received["id"],
            )
            reply = wait_for(
                lambda: discourse_diana.post_containing(
                    topic["topic_id"], reply_marker
                ),
                desc="Plamenu reply to appear in the Discourse topic",
            )
            assert reply["username"] == plamenu_user.username, reply
            assert reply["activity_pub_object_id"].startswith(
                f"{config.PLAMENU_URL}/ap/accounts/"
            ), reply

        with step("the successful exchange consumed no remote-fetch failures"):
            assert db.remote_fetch_failures_for(config.DISCOURSE_DOMAIN) == 0
            assert (
                discourse_diana.post_containing(topic["topic_id"], marker)["username"]
                == DIANA_EMAIL.split("@", 1)[0]
            )
    finally:
        try:
            if topic is not None:
                discourse_diana.delete_topic(topic["topic_id"])
        finally:
            if group is not None:
                plamenu_api.unfollow(group["id"])
                wait_for(
                    lambda: (
                        not any(
                            actor["handle"] == plamenu_user.acct
                            for actor in discourse_diana.category_followers(
                                category_actor_id
                            )
                        )
                    ),
                    desc="the test follower to be removed from Discourse",
                )
