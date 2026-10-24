"""Unmodified BookWyrm: real discovery, signed follows, posts and replies.

BookWyrm intentionally ignores unrelated microblog Notes. Public mentions and
replies are its supported microblog inputs; book statuses are its own output.
Only local fixture accounts participate in these tests.
"""

import pytest
from plamenu_e2e import ephemeral, unique
from plamenu_e2e.bookwyrm import BookWyrm, reachable
from plamenu_e2e.steps import step, wait_for


@pytest.fixture(scope="module")
def bookwyrm():
    if not reachable():
        pytest.skip("BookWyrm fixture is down; run ./dev up bookwyrm")
    return BookWyrm()


@pytest.fixture(scope="module")
def bookwyrm_plamenu(bookwyrm):
    # Never reset or occupy the standing Plamenu-against-Plamenu peer.
    instance = ephemeral.Instance()
    try:
        instance.start(suite_settings=True)
        yield instance
    finally:
        instance.stop()


@pytest.mark.federation(direction="both", peer="bookwyrm")
def test_bookwyrm_discovery_and_follow_lifecycle(bookwyrm, bookwyrm_plamenu):
    instance = bookwyrm_plamenu
    user, api = instance.user_api("bwfollow")
    local = api.get("/api/v1/accounts/verify_credentials")
    with step("BookWyrm resolves Plamenu with WebFinger and follows it"):
        resolved = bookwyrm.resolve(f"{user.username}@{instance.domain}")
        assert resolved and resolved["remote_id"] == local["uri"]
        assert not resolved["bookwyrm_user"]
        bookwyrm.follow(resolved["username"])
        wait_for(
            lambda: bookwyrm.following(local["uri"]), desc="Plamenu Accept in BookWyrm"
        )
        wait_for(
            lambda: instance.db.follower_count(user.username) == 1,
            desc="BookWyrm follower in Plamenu",
        )
    with step("Plamenu resolves BookWyrm and follows it"):
        remote = api.resolve_account(bookwyrm.acct)
        assert remote and remote["uri"] == bookwyrm.actor
        api.follow(remote["id"])
        wait_for(
            lambda: api.relationship(remote["id"])["following"],
            desc="BookWyrm Accept in Plamenu",
        )
        wait_for(
            lambda: bookwyrm.followed_by(local["uri"]),
            desc="Plamenu follower in BookWyrm",
        )
        assert not api.relationship(remote["id"])["requested"]
    with step("both sides process Undo(Follow)"):
        bookwyrm.unfollow(resolved["username"])
        api.unfollow(remote["id"])
        wait_for(
            lambda: instance.db.follower_count(user.username) == 0,
            desc="BookWyrm Undo in Plamenu",
        )
        wait_for(
            lambda: not bookwyrm.followed_by(local["uri"]),
            desc="Plamenu Undo in BookWyrm",
        )


@pytest.mark.federation(direction="both", peer="bookwyrm")
def test_bookwyrm_posts_and_replies_both_ways(bookwyrm, bookwyrm_plamenu):
    instance = bookwyrm_plamenu
    user, api = instance.user_api("bwposts")
    local = api.get("/api/v1/accounts/verify_credentials")
    remote = api.resolve_account(bookwyrm.acct)
    api.follow(remote["id"])
    wait_for(
        lambda: api.relationship(remote["id"])["following"],
        desc="BookWyrm author followed",
    )
    bookwyrm.resolve(f"{user.username}@{instance.domain}")
    bookwyrm.follow(f"{user.username}@{instance.domain}")
    wait_for(lambda: bookwyrm.following(local["uri"]), desc="Plamenu author followed")
    marker = unique("bwpost")
    with step("BookWyrm publishes a review and pushes it to Plamenu"):
        review = bookwyrm.post(
            "review", f"Review body {marker}", name=f"Review {marker}", rating="4.5"
        )
        # Observe pushed delivery before any search can fetch the object.
        received_id = wait_for(
            lambda: instance.db_value(
                "SELECT id FROM statuses WHERE uri = %s", review["remote_id"]
            ),
            desc="BookWyrm Create(Review) delivery",
        )
        received = api.get_status(str(received_id))
        assert f"Review body {marker}" in received["content"]
        assert "Plamenu federation fixture book" in received["title"]
        assert "4.5" in received["title"]
    with step("Plamenu replies to that review and BookWyrm replies back"):
        reply = api.post_status(
            f"Plamenu reply {marker}", in_reply_to_id=received["id"]
        )
        incoming = wait_for(
            lambda: bookwyrm.status(reply["uri"]), desc="Plamenu reply in BookWyrm"
        )
        assert incoming["reply_parent__remote_id"] == review["remote_id"]
        back = bookwyrm.post("reply", f"BookWyrm reply {marker}", reply_to=reply["uri"])
        back_id = wait_for(
            lambda: instance.db_value(
                "SELECT id FROM statuses WHERE uri = %s", back["remote_id"]
            ),
            desc="BookWyrm reply in Plamenu",
        )
        assert api.get_status(str(back_id))["in_reply_to_id"] == reply["id"]
    with step("Plamenu publishes a mentioned Note, then receives a BookWyrm reply"):
        note = api.post_status(f"@{bookwyrm.acct} Standalone post {marker}")
        wait_for(
            lambda: bookwyrm.status(note["uri"]),
            desc="Plamenu standalone mentioned Note in BookWyrm",
        )
        back = bookwyrm.post(
            "reply", f"Reply to standalone {marker}", reply_to=note["uri"]
        )
        back_id = wait_for(
            lambda: instance.db_value(
                "SELECT id FROM statuses WHERE uri = %s", back["remote_id"]
            ),
            desc="BookWyrm reply to standalone Note",
        )
        assert api.get_status(str(back_id))["in_reply_to_id"] == note["id"]


@pytest.mark.federation(
    direction="inbound",
    peer="bookwyrm",
    one_way_reason="BookWyrm-specific book post kinds; Plamenu sends standard Notes, covered by the roundtrip test",
)
@pytest.mark.parametrize(
    "kind,fields",
    [
        ("review", {"name": "Review title", "rating": "3.5"}),
        ("rating", {"rating": "2.5"}),
        ("comment", {}),
        ("quotation", {"quote": "A quoted passage from a fixture book."}),
    ],
)
def test_bookwyrm_book_status_resolution(bookwyrm, bookwyrm_plamenu, kind, fields):
    _, api = bookwyrm_plamenu.user_api("bwresolve")
    marker = unique("bwbook")
    published = bookwyrm.post(kind, f"Book discussion {marker}", **fields)
    native = bookwyrm.ap_document(published["remote_id"], native=True)
    assert native["type"] == kind.capitalize()
    assert native["inReplyToBook"]
    pure = bookwyrm.ap_document(published["remote_id"])
    assert pure["type"] == ("Article" if kind == "review" else "Note")
    received = api.resolve_status(published["remote_id"])
    assert received
    if kind != "rating":
        assert f"Book discussion {marker}" in received["content"]
    presentation = received["content"] + (received.get("title") or "")
    assert "Plamenu federation fixture book" in presentation
    if kind in {"review", "rating"}:
        assert fields["rating"] in presentation
    if kind == "quotation":
        assert fields["quote"] in received["content"]


@pytest.mark.federation(
    direction="inbound",
    peer="bookwyrm",
    reverse_of="test_plamenu_status_edit_delete_in_bookwyrm",
)
def test_bookwyrm_review_edit_delete_in_plamenu(bookwyrm, bookwyrm_plamenu):
    instance = bookwyrm_plamenu
    _, api = instance.user_api("bwedit")
    remote = api.resolve_account(bookwyrm.acct)
    api.follow(remote["id"])
    wait_for(
        lambda: api.relationship(remote["id"])["following"],
        desc="BookWyrm review author followed",
    )
    marker = unique("bwedit")
    review = bookwyrm.post(
        "review", f"Original {marker}", name="Original review", rating="3.5"
    )
    status_id = wait_for(
        lambda: instance.db_value(
            "SELECT id FROM statuses WHERE uri = %s", review["remote_id"]
        ),
        desc="initial review delivery",
    )
    with step("BookWyrm edits the review, sending a native Review Update"):
        for revision, rating in [(1, "4.5"), (2, "2.5"), (3, "5")]:
            text = f"Edited {revision} {marker}"
            title = f"Revised review {revision}"
            bookwyrm.post("review", text, existing=review, name=title, rating=rating)
            wait_for(
                lambda text=text: text in api.get_status(str(status_id))["content"],
                desc=f"native Review Update revision {revision} in Plamenu",
            )
            edited = api.get_status(str(status_id))
            assert title in edited["title"]
            assert rating in edited["title"]
            assert "Plamenu federation fixture book" in edited["title"]
    with step("BookWyrm deletes the review"):
        bookwyrm.delete(review)
        wait_for(
            lambda: api.get_status_or_none(str(status_id)) is None,
            desc="BookWyrm Delete in Plamenu",
        )


@pytest.mark.federation(
    direction="outbound",
    peer="bookwyrm",
    reverse_of="test_bookwyrm_review_edit_delete_in_plamenu",
)
def test_plamenu_status_edit_delete_in_bookwyrm(bookwyrm, bookwyrm_plamenu):
    _, api = bookwyrm_plamenu.user_api("bweditout")
    marker = unique("bweditout")
    post = api.post_status(f"@{bookwyrm.acct} Original {marker}")
    wait_for(
        lambda: bookwyrm.status(post["uri"]), desc="Plamenu mentioned Note delivery"
    )
    with step("Plamenu edits its Note"):
        api.edit_status(post["id"], f"@{bookwyrm.acct} Edited {marker}")
        wait_for(
            lambda: f"Edited {marker}" in bookwyrm.status(post["uri"])["content"],
            desc="Plamenu Update in BookWyrm",
        )
    with step("Plamenu deletes its Note"):
        api.delete_status(post["id"])
        wait_for(
            lambda: bookwyrm.status(post["uri"])["deleted"],
            desc="Plamenu Delete in BookWyrm",
        )
