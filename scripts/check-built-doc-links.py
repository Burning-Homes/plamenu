#!/usr/bin/env python3
"""Fail when a generated mdBook page links to a missing local target or anchor."""

from __future__ import annotations

from html.parser import HTMLParser
from pathlib import Path
import sys
from urllib.parse import unquote, urlsplit


class PageParser(HTMLParser):
    def __init__(self) -> None:
        super().__init__()
        self.ids: set[str] = set()
        self.references: list[str] = []

    def handle_starttag(
        self, tag: str, attrs: list[tuple[str, str | None]]
    ) -> None:
        attributes = dict(attrs)
        if identifier := attributes.get("id"):
            self.ids.add(identifier)
        if tag in {"a", "link"} and (reference := attributes.get("href")):
            self.references.append(reference)
        if tag in {"img", "script", "source"} and (
            reference := attributes.get("src")
        ):
            self.references.append(reference)


def parse_page(path: Path) -> PageParser:
    parser = PageParser()
    parser.feed(path.read_text(encoding="utf-8"))
    return parser


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: check-built-doc-links.py MDBOOK_OUTPUT", file=sys.stderr)
        return 2

    root = Path(sys.argv[1]).resolve()
    pages = {path.resolve(): parse_page(path) for path in root.rglob("*.html")}
    failures: list[str] = []

    for source, page in pages.items():
        for reference in page.references:
            parts = urlsplit(reference)
            if parts.scheme or parts.netloc or reference.startswith("//"):
                continue

            relative_path = unquote(parts.path)
            if relative_path.startswith("/"):
                target = root / relative_path.lstrip("/")
            elif relative_path:
                target = source.parent / relative_path
            else:
                target = source
            target = target.resolve()
            if target.is_dir():
                target /= "index.html"

            try:
                target.relative_to(root)
            except ValueError:
                failures.append(f"{source.relative_to(root)}: escapes site: {reference}")
                continue

            if not target.exists():
                failures.append(f"{source.relative_to(root)}: missing: {reference}")
                continue

            fragment = unquote(parts.fragment)
            if fragment and target.suffix == ".html":
                target_page = pages.get(target)
                if target_page is not None and fragment not in target_page.ids:
                    failures.append(
                        f"{source.relative_to(root)}: missing anchor: {reference}"
                    )

    for failure in failures:
        print(failure, file=sys.stderr)
    return int(bool(failures))


if __name__ == "__main__":
    raise SystemExit(main())
