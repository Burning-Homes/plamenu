"""Collection-only reporter; no environment probes or test execution."""

import json
from pathlib import Path


def pytest_addoption(parser):
    parser.addoption("--inventory-output", required=True)


def pytest_collection_finish(session):
    cases = []
    for item in session.items:
        cases.append(
            {
                "id": item.nodeid,
                "nested_capabilities": sorted(
                    {m.args[0] for m in item.iter_markers("nested_capability")}
                ),
                "capabilities": sorted(
                    {m.args[0] for m in item.iter_markers("capability")}
                ),
            }
        )
    Path(session.config.getoption("inventory_output")).write_text(json.dumps(cases))
