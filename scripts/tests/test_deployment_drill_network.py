"""Deployment rehearsal networking survives an exhausted default IPAM pool."""

import importlib.util
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "deployment-drill-network.py"
SPEC = importlib.util.spec_from_file_location("deployment_drill_network", SCRIPT)
network = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(network)


def docker_network(subnet):
    return {"IPAM": {"Config": [{"Subnet": subnet}]}}


class DeploymentDrillNetworks(unittest.TestCase):
    def test_default_pool_exhaustion_still_leaves_explicit_private_subnets(self):
        occupied = [docker_network("172.16.0.0/12"), docker_network("192.168.0.0/16")]
        self.assertEqual(
            network.choose_subnets(occupied, []),
            [f"10.240.{index}.0/24" for index in range(5)],
        )

    def test_docker_subnets_and_host_routes_are_both_reserved(self):
        selected = network.choose_subnets(
            [docker_network("10.240.0.0/23")], [{"dst": "10.240.2.0/24"}]
        )
        self.assertEqual(selected, [f"10.240.{index}.0/24" for index in range(3, 8)])

    def test_vpn_routes_trigger_fallback_and_ipv6_default_does_not_block_ipv4(self):
        selected = network.choose_subnets(
            [docker_network("fd00::/64")],
            [
                {"dst": "default"},
                {"dst": "0.0.0.0/0"},
                {"dst": "::/0"},
                {"dst": "10.0.0.0/8"},
                {"dst": "192.168.1.0/24"},
            ],
        )
        self.assertEqual(
            selected, [f"192.168.{index}.0/24" for index in [0, 2, 3, 4, 5]]
        )

    def test_no_private_space_fails_without_reusing_existing_networks(self):
        occupied = [
            docker_network("10.0.0.0/8"),
            docker_network("172.16.0.0/12"),
            docker_network("192.168.0.0/16"),
        ]
        with self.assertRaisesRegex(ValueError, "no non-overlapping private IPv4"):
            network.choose_subnets(occupied, [])

    def test_missing_ip_fails_before_any_docker_operation(self):
        script = SCRIPT.with_name("rehearse-deployment.sh")
        with tempfile.TemporaryDirectory() as directory:
            bin_path = Path(directory)
            for tool in ["docker", "git", "sha256sum", "diff", "python3"]:
                (bin_path / tool).symlink_to("/usr/bin/true")
            (bin_path / "dirname").symlink_to("/usr/bin/dirname")
            result = subprocess.run(
                ["/bin/bash", str(script)],
                env={"PATH": str(bin_path)},
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 1)
            self.assertIn("ip is required", result.stderr)

    def test_sudo_fallback_preserves_only_exact_compose_values(self):
        script = SCRIPT.with_name("rehearse-deployment.sh").read_text()
        start = script.index("if docker info")
        fallback = script[start : script.index("\nfi", start) + 3]
        with tempfile.TemporaryDirectory() as directory:
            bin_path = Path(directory)
            fixtures = {
                "docker": '#!/bin/sh\nif [ "$1" = info ]; then exit 1; fi\nprintf "%s\\n" "$DRILL_SUBNET" "$DRILL_ENDPOINT" "${UNRELATED_ENV-absent}" "$@"\n',
                "sg": "#!/bin/sh\nexit 1\n",
                "sudo": '#!/bin/sh\nexec /usr/bin/env -i PATH="$PATH" "$@"\n',
            }
            for name, body in fixtures.items():
                path = bin_path / name
                path.write_text(body)
                path.chmod(0o755)
            (bin_path / "env").symlink_to("/usr/bin/env")
            result = subprocess.run(
                [
                    "/bin/bash",
                    "-c",
                    fallback
                    + "\nDRILL_SUBNET=10.240.0.0/24 DRILL_ENDPOINT=source docker_run compose config",
                ],
                env=os.environ
                | {"PATH": str(bin_path), "UNRELATED_ENV": "developer-only"},
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                result.stdout.splitlines(),
                ["10.240.0.0/24", "source", "absent", "compose", "config"],
            )


if __name__ == "__main__":
    unittest.main()
