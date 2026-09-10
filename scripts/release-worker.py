#!/usr/bin/env python3
"""Internal entry point used by the optional manually dispatched worker adapter."""

import json
import os
from pathlib import Path

from plamenu_release.worker import execute_worker

identity = json.loads(os.environ["RELEASE_IDENTITY"])
if os.environ["RELEASE_SOURCE"] != identity["source"]:
    raise SystemExit(
        "The selected worker ref moved; push the requested source and retry"
    )
execute_worker(
    Path.cwd(), Path(".ci/release-worker"), identity, os.environ["RELEASE_PHASE"]
)
