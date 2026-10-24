"""Source-bound inventories shared by the gate writers and evidence validators."""

import hashlib
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def source_files(root, suite):
    root = Path(root)
    if suite == "browser":
        paths = list((root / "accessibility/tests").rglob("*.mjs"))
        paths += [
            p
            for p in (root / "accessibility").glob("*.mjs")
            if not p.name.endswith(".test.mjs")
        ]
        paths += list((root / "accessibility/fixtures").rglob("*.sql"))
        paths += [root / name for name in (
            "scripts/run-accessibility-tests.py", "scripts/browser_fixture.py",
            "scripts/cold_rendition_fixture.py",
            "scripts/accessibility-container.sh", "e2e/plamenu_e2e/lifecycle.py",
        )]
        paths += [
            root / "accessibility" / name
            for name in (
                "package.json",
                "package-lock.json",
                "coverage-policy.json",
            )
        ]
    elif suite == "e2e":
        paths = list((root / "e2e/tests").rglob("*.py"))
        paths += list((root / "e2e/plamenu_e2e").glob("*.py"))
        paths += [root / "e2e" / name for name in ("conftest.py", "pyproject.toml")]
    else:
        raise ValueError(f"Unknown inventory: {suite}")
    return {
        str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest()
        for p in sorted(paths)
    }


def inventory_path(root, suite):
    return (
        Path(root)
        / ("accessibility" if suite == "browser" else "e2e")
        / "inventory.json"
    )


def load_inventory(root, suite):
    path = inventory_path(root, suite)
    record = json.loads(path.read_text())
    if record.get("schema") != 1 or record.get("suite") != suite:
        raise ValueError(f"Unsupported {suite} inventory")
    if record.get("files") != source_files(root, suite):
        raise ValueError(
            f"Stale {suite} inventory; run ./dev gate-inventory --write and review the diff"
        )
    cases = record.get("tests", [])
    ids = [case["id"] for case in cases]
    if not ids or len(ids) != len(set(ids)):
        raise ValueError(f"Empty or duplicate {suite} inventory")
    return record


def inventory_digest(record):
    return hashlib.sha256(
        json.dumps(
            record, sort_keys=True, separators=(",", ":"), ensure_ascii=False
        ).encode()
    ).hexdigest()
