#!/usr/bin/env python3
"""Choose private drill subnets without consuming Docker's default pools."""

import argparse
import ipaddress
import json
from pathlib import Path


def choose_subnets(networks, routes, count=5):
    occupied = []
    for network in networks:
        for config in network.get("IPAM", {}).get("Config") or []:
            subnet = config.get("Subnet")
            if subnet:
                occupied.append(ipaddress.ip_network(subnet, strict=False))
    for route in routes:
        destination = route.get("dst", "default")
        if destination == "default":
            continue
        try:
            subnet = ipaddress.ip_network(destination, strict=False)
        except ValueError:
            continue
        if subnet.prefixlen:
            occupied.append(subnet)
    selected = []
    for private_pool in ["10.240.0.0/16", "192.168.0.0/16", "172.16.0.0/12"]:
        for subnet in ipaddress.ip_network(private_pool).subnets(new_prefix=24):
            if any(
                existing.version == 4 and subnet.overlaps(existing)
                for existing in occupied
            ):
                continue
            selected.append(str(subnet))
            if len(selected) == count:
                return selected
    raise ValueError("no non-overlapping private IPv4 subnets for the deployment drill")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("networks", type=Path)
    parser.add_argument("routes", type=Path)
    args = parser.parse_args()
    networks = json.loads(args.networks.read_text())
    routes = json.loads(args.routes.read_text())
    for subnet in choose_subnets(networks, routes):
        print(subnet)


if __name__ == "__main__":
    main()
