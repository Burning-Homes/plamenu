"""Proxy preservation and ownership guards for fresh GoToSocial E2E fixtures."""

import copy
import hashlib
import io
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "e2e"))
from plamenu_e2e.isolated_gotosocial import IsolatedGoToSocial, isolated_route


def proxy_config():
    return {
        "apps": {
            "http": {
                "servers": {
                    "srv0": {
                        "routes": [
                            {
                                "match": [{"host": ["gotosocial.local"]}],
                                "handle": [
                                    {
                                        "handler": "subroute",
                                        "routes": [
                                            {
                                                "handle": [
                                                    {
                                                        "handler": "reverse_proxy",
                                                        "upstreams": [
                                                            {
                                                                "dial": "gotosocial-web:8080"
                                                            }
                                                        ],
                                                    }
                                                ]
                                            }
                                        ],
                                    }
                                ],
                            },
                            {"match": [{"host": ["mastodon.local"]}], "handle": []},
                        ]
                    }
                }
            },
            "tls": {
                "automation": {"policies": [{"issuers": [{"module": "internal"}]}]}
            },
        }
    }


class IsolationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def peer(self):
        return IsolatedGoToSocial(
            self.root,
            {},
            io.StringIO(),
            peer_root=self.root,
            proxy="local-test-proxy",
            standing="standing-gotosocial",
        )

    def test_route_changes_only_exact_peer_and_keeps_original_for_restore(self):
        original = proxy_config()
        preserved = copy.deepcopy(original)
        replacement = isolated_route(original, "172.20.0.99:8080")
        routes = replacement["apps"]["http"]["servers"]["srv0"]["routes"]
        upstream = routes[0]["handle"][0]["routes"][0]["handle"][0]["upstreams"]
        self.assertEqual(upstream, [{"dial": "172.20.0.99:8080"}])
        upstream[0]["dial"] = "gotosocial-web:8080"
        self.assertEqual(replacement, preserved)
        self.assertEqual(original, preserved)

    def test_ambiguous_or_already_changed_route_is_rejected(self):
        original = proxy_config()
        routes = original["apps"]["http"]["servers"]["srv0"]["routes"]
        routes.append(copy.deepcopy(routes[0]))
        with self.assertRaisesRegex(RuntimeError, "one exact"):
            isolated_route(original, "172.20.0.99:8080")
        routes.pop()
        routes[0]["handle"][0]["routes"][0]["handle"][0]["upstreams"][0]["dial"] = (
            "another-fixture:8080"
        )
        with self.assertRaisesRegex(RuntimeError, "upstream differs"):
            isolated_route(original, "172.20.0.99:8080")

    def test_external_proxy_change_blocks_overwrite_and_container_stop(self):
        peer = self.peer()
        peer.route_attempted = True
        peer.original = proxy_config()
        peer.replacement = isolated_route(peer.original, "172.20.0.99:8080")
        changed = copy.deepcopy(peer.replacement)
        changed["unrelated-new-setting"] = True
        peer.active = Mock(return_value=changed)
        peer.reload = Mock()
        peer.command = Mock()
        with self.assertRaisesRegex(RuntimeError, "refuse overwrite"):
            peer.close()
        peer.reload.assert_not_called()
        peer.command.assert_not_called()

    def test_cleanup_does_not_signal_container_with_changed_ownership(self):
        peer = self.peer()
        peer.peer_id = "new-peer-id"
        peer.inspect = Mock(return_value={"Config": {"Labels": {}}})
        peer.command = Mock()
        with self.assertRaisesRegex(RuntimeError, "refuse stop"):
            peer.close()
        peer.command.assert_not_called()

    def test_failed_reload_can_restore_original_without_touching_other_peers(self):
        peer = self.peer()
        peer.original = proxy_config()
        peer.replacement = isolated_route(peer.original, "172.20.0.99:8080")
        peer.route_attempted = True
        peer.source = self.root / "Caddyfile"
        peer.source.write_text("fixture-source")
        peer.before_source = hashlib.sha256(peer.source.read_bytes()).hexdigest()
        peer.active = Mock(return_value=peer.original)
        peer.reload = Mock()
        peer.command = Mock()
        peer.close()
        peer.reload.assert_called_once_with(peer.original)
        peer.command.assert_not_called()
        self.assertEqual(peer.source.read_text(), "fixture-source")


if __name__ == "__main__":
    unittest.main()
