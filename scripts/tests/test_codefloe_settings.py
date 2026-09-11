"""Applying repository protections must never change publication visibility."""

import contextlib
import importlib.util
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "codefloe_settings", ROOT / "scripts/codefloe-settings.py"
)
SETTINGS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SETTINGS)


class SettingsTests(unittest.TestCase):
    def run_settings(self, private, apply=True, forbidden_visibility=None):
        requests = []
        current = {
            "full_name": "plamenu/plamenu",
            "private": private,
            "default_branch": "main",
            "has_actions": True,
            "has_releases": True,
            "default_merge_style": "fast-forward-only",
            "allow_fast_forward_only_merge": True,
            "allow_merge_commits": False,
            "allow_rebase": False,
            "allow_rebase_explicit": False,
            "allow_squash_merge": False,
        }

        class Opener:
            def open(self, request, timeout):
                method = request.get_method()
                path = request.full_url.split("/api/v1", 1)[1]
                body = json.loads(request.data) if request.data else None
                requests.append((method, path, body))
                if method == "GET":
                    if path == "/repos/plamenu/plamenu":
                        result = current
                    elif path.startswith("/orgs/plamenu/teams"):
                        result = [
                            {"id": 1, "name": "maintainers", "permission": "write"}
                        ]
                    elif path.endswith("/branch_protections"):
                        result = [{"rule_name": "main"}]
                    elif path.endswith("/tag_protections"):
                        result = [{"id": 2, "name_pattern": "v*"}]
                    else:
                        result = []
                else:
                    result = None
                return io.BytesIO(json.dumps(result).encode())

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            token = root / "token"
            token.write_text("test-credential")
            (root / "ci").mkdir()
            config = json.loads((ROOT / "ci/codefloe-settings.json").read_text())
            if forbidden_visibility is not None:
                config["repository"]["private"] = forbidden_visibility
            (root / "ci/codefloe-settings.json").write_text(json.dumps(config))
            argv = ["settings", "--token-file", str(token)]
            if apply:
                argv.append("--apply")
            with (
                patch.object(sys, "argv", argv),
                patch.object(SETTINGS, "__file__", str(root / "scripts/settings.py")),
                patch.object(
                    SETTINGS.urllib.request, "build_opener", return_value=Opener()
                ),
                contextlib.redirect_stdout(io.StringIO()),
            ):
                if forbidden_visibility is not None:
                    with self.assertRaisesRegex(
                        SystemExit, "change visibility separately"
                    ):
                        SETTINGS.main()
                else:
                    SETTINGS.main()
        return requests

    def test_apply_to_private_and_public_preserves_visibility_and_protections(self):
        for private in (True, False):
            with self.subTest(private=private):
                requests = self.run_settings(private)
                patches = {
                    path: body for method, path, body in requests if method == "PATCH"
                }
                self.assertNotIn("private", patches["/repos/plamenu/plamenu"])
                repository = patches["/repos/plamenu/plamenu"]
                self.assertEqual(
                    repository["default_merge_style"], "fast-forward-only"
                )
                self.assertTrue(repository["allow_fast_forward_only_merge"])
                self.assertFalse(repository["allow_merge_commits"])
                self.assertFalse(repository["allow_rebase"])
                self.assertFalse(repository["allow_rebase_explicit"])
                self.assertFalse(repository["allow_squash_merge"])
                rule = patches["/repos/plamenu/plamenu/branch_protections/main"]
                self.assertFalse(rule["enable_push"])
                self.assertTrue(rule["apply_to_admins"])
                self.assertTrue(rule["require_signed_commits"])
                self.assertEqual(len(rule["status_check_contexts"]), 2)
                self.assertEqual(
                    patches["/repos/plamenu/plamenu/tag_protections/2"][
                        "whitelist_teams"
                    ],
                    ["maintainers"],
                )

    def test_explicit_visibility_setting_stops_before_any_mutation(self):
        for visibility in (True, False):
            requests = self.run_settings(True, forbidden_visibility=visibility)
            self.assertTrue(all(method == "GET" for method, _, _ in requests))

    def test_default_inspection_is_read_only(self):
        requests = self.run_settings(True, apply=False)
        self.assertTrue(all(method == "GET" for method, _, _ in requests))


if __name__ == "__main__":
    unittest.main()
