"""Guard a temporary Lemmy route without redirecting unrelated peer traffic."""

import copy
import sys
import unittest
from pathlib import Path
from unittest.mock import Mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "e2e"))
from plamenu_e2e.isolated_lemmy import IsolatedLemmy, isolated_route


def configuration():
    routes = [
        {
            "match": [{"host": ["mastodon.local"]}],
            "handle": [
                {"handler": "reverse_proxy", "upstreams": [{"dial": "web:3000"}]}
            ],
        },
        {
            "match": [{"host": ["lemmy.local"]}],
            "handle": [
                {"handler": "reverse_proxy", "upstreams": [{"dial": dial}]}
                for dial in ["lemmy-web:8536"] * 3 + ["lemmy-ui-web:1234"]
            ],
        },
    ]
    return {"apps": {"http": {"servers": {"srv0": {"routes": routes}}}}}


class LemmyIsolationTests(unittest.TestCase):
    def test_cleanup_releases_only_the_empty_owned_network(self):
        for change in (None, "foreign-project", "foreign-name", "active-endpoint"):
            with self.subTest(change=change):
                peer = object.__new__(IsolatedLemmy)
                peer.project, peer.nonce = "owned", "nonce"
                peer.route_attempted, peer.original = False, None
                peer.command = Mock(side_effect=["", "network-id", None])
                info = {
                    "Name": "owned_private",
                    "Labels": {"com.docker.compose.project": "owned"},
                    "Containers": {},
                }
                if change == "foreign-project":
                    info["Labels"]["com.docker.compose.project"] = "foreign"
                elif change == "foreign-name":
                    info["Name"] = "shared"
                elif change == "active-endpoint":
                    info["Containers"] = {"foreign": {}}
                peer.inspect, peer.save = Mock(return_value=info), Mock()
                if change:
                    with self.assertRaises(RuntimeError):
                        peer.close()
                    self.assertEqual(peer.command.call_count, 2)
                else:
                    peer.close()
                    peer.command.assert_called_with(
                        ["docker", "network", "rm", "network-id"]
                    )

    def test_only_exact_lemmy_route_changes_and_input_is_untouched(self):
        original = configuration()
        before = copy.deepcopy(original)
        result = isolated_route(original, "172.19.0.9:8536", "172.19.0.10:1234")
        self.assertEqual(original, before)
        routes = result["apps"]["http"]["servers"]["srv0"]["routes"]
        self.assertEqual(
            routes[0], original["apps"]["http"]["servers"]["srv0"]["routes"][0]
        )
        self.assertEqual(
            [h["upstreams"][0]["dial"] for h in routes[1]["handle"]],
            ["172.19.0.9:8536"] * 3 + ["172.19.0.10:1234"],
        )

    def test_ambiguous_missing_or_modified_routes_are_refused(self):
        for change in ("missing", "duplicate", "foreign", "extra"):
            with self.subTest(change=change):
                value = configuration()
                routes = value["apps"]["http"]["servers"]["srv0"]["routes"]
                if change == "missing":
                    routes.pop()
                elif change == "duplicate":
                    routes.append(copy.deepcopy(routes[1]))
                elif change == "foreign":
                    routes[1]["handle"][0]["upstreams"][0]["dial"] = "other:8536"
                else:
                    routes[1]["handle"].append(copy.deepcopy(routes[1]["handle"][0]))
                with self.assertRaises(RuntimeError):
                    isolated_route(value, "172.19.0.9:8536", "172.19.0.10:1234")
