#!/usr/bin/env python3
"""Validate a two-node H4D Cloud RDMA topology and emit a benchmark manifest."""

from __future__ import annotations

import argparse
import ipaddress
import json
import pathlib
import subprocess
import sys
from decimal import Decimal, InvalidOperation
from typing import Any, NoReturn, Sequence


def fatal(message: str) -> NoReturn:
    raise SystemExit(f"gce-h4d-pair-manifest: {message}")


def basename(value: str) -> str:
    return value.rstrip("/").rsplit("/", 1)[-1]


def run_json(command: Sequence[str]) -> dict[str, Any]:
    try:
        result = subprocess.run(
            command,
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
    except FileNotFoundError:
        fatal(f"required command not found: {command[0]}")
    except subprocess.CalledProcessError as exc:
        detail = exc.stderr.strip() or exc.stdout.strip() or str(exc)
        fatal(f"command failed: {detail}")
    try:
        value = json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        fatal(f"command returned invalid JSON: {exc}")
    if not isinstance(value, dict):
        fatal("command did not return a JSON object")
    return value


def describe_instance(project: str, zone: str, name: str) -> dict[str, Any]:
    return run_json(
        [
            "gcloud",
            "compute",
            "instances",
            "describe",
            name,
            "--project",
            project,
            "--zone",
            zone,
            "--format=json",
        ]
    )


def metadata_map(instance: dict[str, Any]) -> dict[str, str]:
    return {
        str(item.get("key")): str(item.get("value"))
        for item in instance.get("metadata", {}).get("items", [])
        if item.get("key") is not None and item.get("value") is not None
    }


def interface_of_type(instance: dict[str, Any], nic_type: str) -> dict[str, Any]:
    matches = [
        interface
        for interface in instance.get("networkInterfaces", [])
        if str(interface.get("nicType", "")).upper() == nic_type
    ]
    if len(matches) != 1:
        fatal(
            f"instance {instance.get('name')} must have exactly one {nic_type} "
            f"interface; found {len(matches)}"
        )
    return matches[0]


def duration_seconds(scheduling: dict[str, Any]) -> int:
    duration = scheduling.get("maxRunDuration")
    if isinstance(duration, str) and duration.endswith("s"):
        duration = duration[:-1]
    elif isinstance(duration, dict):
        duration = duration.get("seconds")
    try:
        seconds = int(duration)
    except (TypeError, ValueError):
        fatal("Spot instance has no valid maxRunDuration")
    if seconds < 1 or seconds > 2 * 60 * 60:
        fatal(f"Spot maxRunDuration is outside 1..7200 seconds: {seconds}")
    return seconds


def validate_instance(
    instance: dict[str, Any], project: str, zone: str, policy: str
) -> dict[str, Any]:
    name = str(instance.get("name", ""))
    if not name:
        fatal("instance has no name")
    if instance.get("status") != "RUNNING":
        fatal(f"instance {name} is not RUNNING: {instance.get('status')}")
    if basename(str(instance.get("zone", ""))) != zone:
        fatal(f"instance {name} is not in required zone {zone}")
    if basename(str(instance.get("machineType", ""))) != "h4d-standard-192":
        fatal(f"instance {name} is not h4d-standard-192")

    policies = [basename(str(item)) for item in instance.get("resourcePolicies", [])]
    if policies != [policy]:
        fatal(f"instance {name} does not use only compact policy {policy}: {policies}")

    scheduling = instance.get("scheduling", {})
    if scheduling.get("provisioningModel") != "SPOT":
        fatal(f"instance {name} is not Spot")
    if scheduling.get("onHostMaintenance") != "TERMINATE":
        fatal(f"instance {name} does not terminate for host maintenance")
    if scheduling.get("instanceTerminationAction") != "DELETE":
        fatal(f"instance {name} does not have DELETE as its termination action")
    max_run_seconds = duration_seconds(scheduling)

    rdma = interface_of_type(instance, "IRDMA")
    gvnic = interface_of_type(instance, "GVNIC")
    if rdma.get("accessConfigs") or rdma.get("ipv6AccessConfigs"):
        fatal(f"instance {name} exposes the IRDMA interface externally")
    if str(rdma.get("stackType", "IPV4_ONLY")) != "IPV4_ONLY":
        fatal(f"instance {name} IRDMA interface is not IPv4-only")
    try:
        rdma_ip = ipaddress.ip_address(str(rdma["networkIP"]))
    except (KeyError, ValueError):
        fatal(f"instance {name} has no valid IRDMA IPv4 address")
    if rdma_ip.version != 4 or not rdma_ip.is_private:
        fatal(f"instance {name} IRDMA address is not private IPv4: {rdma_ip}")

    control_external = [
        str(config["natIP"])
        for config in gvnic.get("accessConfigs", [])
        if config.get("natIP")
    ]
    metadata = metadata_map(instance)
    if metadata.get("bulk_traffic_policy") != "irdma-same-zone-only":
        fatal(f"instance {name} lacks the IRDMA-only bulk traffic policy metadata")
    for key in (
        "adhoc_hourly_instance_price_usd",
        "adhoc_estimated_total_cost_usd",
        "adhoc_max_total_cost_usd",
    ):
        if key not in metadata:
            fatal(f"instance {name} lacks cost guard metadata {key}")
    try:
        estimate = Decimal(metadata["adhoc_estimated_total_cost_usd"])
        ceiling = Decimal(metadata["adhoc_max_total_cost_usd"])
    except InvalidOperation:
        fatal(f"instance {name} has invalid cost guard metadata")
    if estimate > ceiling:
        fatal(f"instance {name} projected cost {estimate} exceeds ceiling {ceiling}")

    return {
        "name": name,
        "zone": zone,
        "machine_type": "h4d-standard-192",
        "placement_policy": policy,
        "max_run_seconds": max_run_seconds,
        "rdma_ipv4": str(rdma_ip),
        "rdma_network": basename(str(rdma.get("network", ""))),
        "rdma_subnet": basename(str(rdma.get("subnetwork", ""))),
        "control_ipv4": str(gvnic.get("networkIP", "")),
        "control_external_ipv4": control_external,
        "cost": {
            "hourly_instance_price_usd": metadata[
                "adhoc_hourly_instance_price_usd"
            ],
            "estimated_pair_cost_usd": str(estimate),
            "pair_cost_ceiling_usd": str(ceiling),
        },
    }


def build_manifest(args: argparse.Namespace) -> dict[str, Any]:
    region = args.zone.rsplit("-", 1)[0]
    if not region or region == args.zone:
        fatal(f"cannot derive region from zone {args.zone!r}")
    nodes = [
        validate_instance(
            describe_instance(args.project, args.zone, name),
            args.project,
            args.zone,
            args.placement_policy,
        )
        for name in args.instance
    ]
    if nodes[0]["rdma_network"] != nodes[1]["rdma_network"]:
        fatal("the two IRDMA interfaces use different Falcon networks")
    if nodes[0]["rdma_subnet"] != nodes[1]["rdma_subnet"]:
        fatal("the two IRDMA interfaces use different Falcon subnets")
    if nodes[0]["rdma_ipv4"] == nodes[1]["rdma_ipv4"]:
        fatal("the two IRDMA interfaces report the same address")
    if nodes[0]["cost"] != nodes[1]["cost"]:
        fatal("the two instances do not share identical cost guard metadata")

    network_name = nodes[0]["rdma_network"]
    subnet_name = nodes[0]["rdma_subnet"]
    network = run_json(
        [
            "gcloud",
            "compute",
            "networks",
            "describe",
            network_name,
            "--project",
            args.project,
            "--format=json",
        ]
    )
    if int(network.get("mtu", 0)) != 8896:
        fatal(f"Falcon network {network_name} MTU is not 8896")
    expected_profile = f"{args.zone}-vpc-falcon"
    if basename(str(network.get("networkProfile", ""))) != expected_profile:
        fatal(
            f"network {network_name} does not use Falcon profile {expected_profile}"
        )
    if network.get("routingConfig", {}).get("routingMode") != "REGIONAL":
        fatal(f"Falcon network {network_name} is not regional-routing-only")

    subnet = run_json(
        [
            "gcloud",
            "compute",
            "networks",
            "subnets",
            "describe",
            subnet_name,
            "--project",
            args.project,
            "--region",
            region,
            "--format=json",
        ]
    )
    if basename(str(subnet.get("region", ""))) != region:
        fatal(f"Falcon subnet {subnet_name} is not in region {region}")
    if basename(str(subnet.get("network", ""))) != network_name:
        fatal(f"Falcon subnet {subnet_name} does not belong to {network_name}")
    if subnet.get("privateIpGoogleAccess") is True:
        fatal(f"Falcon subnet {subnet_name} unexpectedly has Private Google Access")
    try:
        cidr = ipaddress.ip_network(str(subnet["ipCidrRange"]))
    except (KeyError, ValueError):
        fatal(f"Falcon subnet {subnet_name} has no valid IPv4 CIDR")
    for node in nodes:
        if ipaddress.ip_address(node["rdma_ipv4"]) not in cidr:
            fatal(f"instance {node['name']} IRDMA address is outside {cidr}")

    policy = run_json(
        [
            "gcloud",
            "beta",
            "compute",
            "resource-policies",
            "describe",
            args.placement_policy,
            "--project",
            args.project,
            "--region",
            region,
            "--format=json",
        ]
    )
    group = policy.get("groupPlacementPolicy", {})
    if group.get("collocation") != "COLLOCATED":
        fatal(f"placement policy {args.placement_policy} is not COLLOCATED")
    try:
        max_distance = int(group["maxDistance"])
        vm_count = int(group["vmCount"])
    except (KeyError, TypeError, ValueError):
        fatal(f"placement policy {args.placement_policy} lacks distance/count data")
    if max_distance > 3:
        fatal(f"placement policy maxDistance {max_distance} exceeds 3")
    if vm_count < 2:
        fatal(f"placement policy vmCount {vm_count} cannot hold the pair")
    if policy.get("status") != "READY":
        fatal(f"placement policy {args.placement_policy} is not READY")

    return {
        "schema_version": 1,
        "provider": "gce",
        "project": args.project,
        "zone": args.zone,
        "region": region,
        "machine_type": "h4d-standard-192",
        "placement": {
            "policy": args.placement_policy,
            "collocation": "COLLOCATED",
            "max_distance": max_distance,
            "vm_count": vm_count,
        },
        "rdma_network": {
            "name": network_name,
            "subnet": subnet_name,
            "cidr": str(cidr),
            "mtu": 8896,
            "profile": expected_profile,
            "routing_mode": "REGIONAL",
            "private_google_access": False,
            "external_access": False,
        },
        "bulk_traffic_policy": {
            "transport": "Cloud RDMA over same-zone Falcon VPC only",
            "allowed_peer_ipv4": [node["rdma_ipv4"] for node in nodes],
            "cross_region": False,
            "internet": False,
        },
        "nodes": nodes,
        "cost": nodes[0]["cost"],
    }


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Validate an exact two-node H4D Cloud RDMA benchmark topology"
    )
    parser.add_argument("--project", required=True)
    parser.add_argument("--zone", required=True)
    parser.add_argument("--instance", action="append", required=True)
    parser.add_argument("--placement-policy", required=True)
    parser.add_argument("--output", type=pathlib.Path)
    return parser


def main() -> None:
    args = build_parser().parse_args()
    if len(args.instance) != 2 or len(set(args.instance)) != 2:
        fatal("pass exactly two distinct --instance names")
    manifest = build_manifest(args)
    rendered = json.dumps(manifest, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(rendered, encoding="utf-8")
        print(f"wrote validated H4D pair manifest: {args.output}", file=sys.stderr)
    else:
        print(rendered, end="")


if __name__ == "__main__":
    main()
