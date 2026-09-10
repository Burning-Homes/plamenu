"""FEP-f228 / FEP-171b conversation collections: a local thread advertises
its `context` (collection of posts) on every Note and serves the collection, so
a peer can backfill the whole thread in one fetch instead of crawling `replies`
node by node. Cross-peer threading/DM interop with the new fields is covered by
the regression suites (test_threads, test_direct, test_replies, …)."""

from plamenu_e2e import config
from plamenu_e2e.steps import step


def test_thread_advertises_and_serves_context_collection(
    plamenu_api, plamenu_user, marker
):
    """Covers local Notes carrying `context` (and the Mastodon `conversation`
    alias), and `GET /contexts/{id}` returns an OrderedCollection of the
    thread's distributable posts, attributed to the conversation owner."""
    with step("a local user builds a public thread"):
        root = plamenu_api.post_status(f"ctx root {marker}root", visibility="public")
        reply = plamenu_api.post_status(
            f"ctx reply {marker}reply",
            visibility="public",
            in_reply_to_id=root["id"],
        )

    with step("every Note advertises the conversation context"):
        root_note = plamenu_api.ap_get(
            f"/users/{plamenu_user.username}/statuses/{root['id']}"
        )
        reply_note = plamenu_api.ap_get(
            f"/users/{plamenu_user.username}/statuses/{reply['id']}"
        )
        context_url = root_note.get("context")
        assert context_url, "the root Note carries a context"
        assert root_note.get("conversation") == context_url, (
            "the Mastodon `conversation` alias mirrors `context`"
        )
        assert reply_note.get("context") == context_url, (
            "the reply shares the root's conversation context"
        )

    with step("the context collection lists the thread's posts"):
        path = context_url.removeprefix(config.PLAMENU_URL)
        collection = plamenu_api.ap_get(path)
        assert collection["type"] == "OrderedCollection"
        assert collection["attributedTo"] == root_note["attributedTo"], collection[
            "attributedTo"
        ]
        items = collection["first"]["orderedItems"]
        assert root_note["id"] in items, "the root is in its own context"
        assert reply_note["id"] in items, "the reply is in the context"
