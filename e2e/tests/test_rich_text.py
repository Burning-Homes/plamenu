"""Rich-text posting interop (Pleroma-extension track P4).

Plamenu accepts Pleroma's `content_type` parameter (`text/markdown` /
`text/html`), renders the post to sanitized HTML, and federates the raw
source as the AP `source` property. Outbound: the rendered HTML must display
on both Mastodon and Akkoma, and Akkoma must see the raw markdown under
`akkoma.source`. Inbound: an Akkoma markdown post's rendered HTML must
survive ingestion.
"""

import pytest
from plamenu_e2e import plamenu
from plamenu_e2e.steps import log, step, wait_for


@pytest.mark.federation(
    direction="outbound",
    peer="pleroma",
    reverse_of="test_akkoma_markdown_post_ingests_on_plamenu",
)
def test_markdown_post_federates_to_pleroma_with_source(pleroma_bob, plamenu_user):
    plamenu_api = plamenu.login(plamenu_user)

    with step("plamenu posts markdown with content_type=text/markdown"):
        status = plamenu_api.post_status(
            "**bold move** and _italics_ from plamenu",
            content_type="text/markdown",
        )
        assert "<strong>bold move</strong>" in status["content"], status["content"]
        log(f"rendered content: {status['content']}")

    with step("the raw markdown is preserved for the edit composer"):
        source = plamenu_api.get(f"/api/v1/statuses/{status['id']}/source")
        assert source["content_type"] == "text/markdown", source
        assert source["text"].startswith("**bold move**"), source

    with step("akkoma resolves the post and shows the rendered HTML"):
        resolved = wait_for(
            lambda: pleroma_bob.resolve_status(status["uri"]),
            desc="Akkoma to resolve the markdown post",
        )
        assert "<strong>bold move</strong>" in resolved["content"], resolved["content"]

    with step("akkoma received the AP source (raw markdown + mediaType)"):
        akkoma_source = (resolved.get("akkoma") or {}).get("source") or {}
        assert akkoma_source.get("mediaType") == "text/markdown", resolved.get("akkoma")
        assert akkoma_source.get("content", "").startswith("**bold move**"), (
            akkoma_source
        )


@pytest.mark.federation(
    direction="outbound",
    peer="mastodon",
    one_way_reason="outbound rendering check: Plamenu's Markdown post renders correctly on Mastodon; a display refinement of the outbound path with no inbound counterpart.",
)
def test_markdown_post_renders_on_mastodon(alice, plamenu_user):
    plamenu_api = plamenu.login(plamenu_user)

    with step("plamenu posts markdown with a mention-safe body"):
        status = plamenu_api.post_status(
            "a **strong** statement with a [link](https://example.com/p4)",
            content_type="text/markdown",
        )

    with step("mastodon resolves it and keeps the markup"):
        resolved = wait_for(
            lambda: (
                alice.search(status["uri"], resolve=True, type="statuses")["statuses"]
                or [None]
            )[0],
            desc="Mastodon to resolve the markdown post",
        )
        content = resolved["content"]
        assert "<strong>strong</strong>" in content, content
        assert 'href="https://example.com/p4"' in content, content


@pytest.mark.federation(
    direction="inbound",
    peer="pleroma",
    reverse_of="test_markdown_post_federates_to_pleroma_with_source",
)
def test_akkoma_markdown_post_ingests_on_plamenu(pleroma_bob, plamenu_user):
    plamenu_api = plamenu.login(plamenu_user)

    with step("akkoma bob posts markdown via content_type"):
        pleroma_status = pleroma_bob.post_status(
            "akkoma says **hello richly**",
            content_type="text/markdown",
        )
        assert "<strong>" in pleroma_status["content"], pleroma_status["content"]

    with step("plamenu resolves it and keeps the rendered HTML"):
        resolved = wait_for(
            lambda: plamenu_api.resolve_status(pleroma_status["uri"]),
            desc="Plamenu to resolve Akkoma's markdown post",
        )
        assert "<strong>hello richly</strong>" in resolved["content"], resolved[
            "content"
        ]


def test_post_formats_advertised(plamenu_api):
    with step("instance metadata advertises the accepted formats"):
        v1 = plamenu_api.get("/api/v1/instance")
        formats = (v1.get("pleroma") or {}).get("metadata", {}).get("post_formats")
        assert formats == ["text/plain", "text/markdown", "text/html"], formats
