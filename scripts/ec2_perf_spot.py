#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "boto3>=1.34",
# ]
# ///

from __future__ import annotations

import argparse
import dataclasses
import datetime as dt
import json
import os
import pathlib
import re
import shlex
import subprocess
import sys
import time
import uuid
from typing import Any

import boto3
from botocore.exceptions import ClientError


REQUIRED_ACCOUNT = "968134102381"
DEFAULT_PROFILE = "tf"
DEFAULT_NODES = 3
DEFAULT_KEY_NAME = "adhocMasterKeypair"
DEFAULT_SSH_KEY_PATH = "/home/rob/robsSecretStore/aws/adhocMasterKeypair-20260523-ed25519"
DEFAULT_SSH_PUBLIC_KEY_PATH = f"{DEFAULT_SSH_KEY_PATH}.pub"
DEFAULT_KEY_FINGERPRINT_AWS = "XO82hTI96j+kXyMZYaMxFAxxfZX7nnd1brvPJvdRH9I="
ADHOC_SUPPORT_PREFIX = "up-adhoc"
ADHOC_SUPPORT_PURPOSE = "adhoc-performance-compute-support"
DEFAULT_SSH_CMD = (
    "ssh -o StrictHostKeyChecking=accept-new "
    "-o ServerAliveInterval=30 "
    f"-i {DEFAULT_SSH_KEY_PATH}"
)
DEFAULT_INSTANCE_TYPES = (
    "m6idn.32xlarge",
    "m6idn.metal",
    "m8idn.48xlarge",
    "m8idn.96xlarge",
    "c8id.96xlarge",
    "c8id.metal-96xl",
    "i8g.48xlarge",
    "i8g.metal-48xl",
)
DEFAULT_REGIONS = (
    "us-east-1",
    "us-east-2",
    "us-west-2",
    "sa-east-1",
    "eu-north-1",
    "eu-central-1",
    "eu-west-1",
    "ap-northeast-1",
    "ap-southeast-1",
    "ap-southeast-2",
)
DEFAULT_CACHE_PATH = pathlib.Path(".cache/ec2_perf_spot_cache.json")
DEFAULT_CACHE_TTL_SECONDS = 15 * 60
DEFAULT_MAX_DAILY_COST_USD = 50.0
PUBLIC_IPV4_HOURLY_USD = 0.005
# Deliberately conservative enough for a short-lived budget guard. Real EBS
# prices vary by region, but this root volume is not meant to dominate spend.
ROOT_EBS_GB_MONTH_USD = 0.12
UBUNTU_2404_AMI_PARAM = {
    "x86_64": "/aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id",
    "arm64": "/aws/service/canonical/ubuntu/server/24.04/stable/current/arm64/hvm/ebs-gp3/ami-id",
}


@dataclasses.dataclass(frozen=True)
class Candidate:
    region: str
    az: str
    subnet_id: str
    instance_type: str
    arch: str
    vcpus: int
    mem_gib: float
    local_nvme_gb: int
    network_gbps: float
    network_label: str
    efa: bool
    spot_price: float
    hourly_total: float
    score: float
    placement_score: int | None = None


@dataclasses.dataclass(frozen=True)
class SpotPriceRow:
    region: str
    az: str
    instance_type: str
    arch: str
    vcpus: int
    mem_gib: float
    local_nvme_gb: int
    network_label: str
    network_gbps: float
    efa: bool
    spot_price: float
    hourly_total: float
    timestamp: str


def utcnow() -> dt.datetime:
    return dt.datetime.now(dt.UTC).replace(microsecond=0)


def parse_csv(raw: str | None, default: tuple[str, ...]) -> list[str]:
    if raw is None or raw.strip() == "":
        return list(default)
    return [part.strip() for part in raw.split(",") if part.strip()]


def split_values(raw: str | list[str] | None) -> list[str]:
    if raw is None:
        return []
    if isinstance(raw, str):
        return [part.strip() for part in raw.split(",") if part.strip()]
    out: list[str] = []
    for item in raw:
        out.extend(part.strip() for part in item.split(",") if part.strip())
    return out


def parse_utc_timestamp(raw: str) -> dt.datetime:
    normalized = raw.strip()
    if normalized.endswith("Z"):
        normalized = normalized[:-1] + "+00:00"
    parsed = dt.datetime.fromisoformat(normalized)
    if parsed.tzinfo is None:
        raise ValueError("timestamp must include timezone or trailing Z")
    return parsed.astimezone(dt.UTC).replace(microsecond=0)


def parse_network_gbps(label: str | None) -> float:
    if not label:
        return 0.0
    lowered = label.lower()
    m = re.search(r"(\d+(?:\.\d+)?)\s*gigabit", lowered)
    if m:
        return float(m.group(1))
    m = re.search(r"(\d+(?:\.\d+)?)\s*gbps", lowered)
    if m:
        return float(m.group(1))
    return 0.0


def boto_session(profile: str) -> boto3.Session:
    return boto3.Session(profile_name=profile)


class JsonTtlCache:
    def __init__(self, path: str | pathlib.Path, ttl_seconds: int, disabled: bool = False):
        self.path = pathlib.Path(path)
        self.ttl_seconds = ttl_seconds
        self.disabled = disabled
        self.data: dict[str, Any] = {}
        if not disabled:
            self._load()

    def _load(self) -> None:
        try:
            self.data = json.loads(self.path.read_text(encoding="utf-8"))
        except FileNotFoundError:
            self.data = {}
        except json.JSONDecodeError:
            self.data = {}

    def get(self, key: str) -> Any | None:
        if self.disabled:
            return None
        item = self.data.get(key)
        if not isinstance(item, dict) or "saved_at" not in item or "value" not in item:
            return None
        try:
            saved_at = dt.datetime.fromisoformat(item["saved_at"])
        except ValueError:
            return None
        if saved_at.tzinfo is None:
            saved_at = saved_at.replace(tzinfo=dt.UTC)
        if utcnow() - saved_at.astimezone(dt.UTC) > dt.timedelta(seconds=self.ttl_seconds):
            return None
        return item["value"]

    def set(self, key: str, value: Any) -> None:
        if self.disabled:
            return
        self.data[key] = {"saved_at": utcnow().isoformat(), "value": json_safe(value)}

    def delete(self, key: str) -> None:
        if self.disabled:
            return
        self.data.pop(key, None)

    def delete_prefix(self, prefix: str) -> None:
        if self.disabled:
            return
        for key in list(self.data):
            if key.startswith(prefix):
                del self.data[key]

    def save(self) -> None:
        if self.disabled:
            return
        self.path.parent.mkdir(parents=True, exist_ok=True)
        tmp = self.path.with_suffix(self.path.suffix + ".tmp")
        tmp.write_text(json.dumps(self.data, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        tmp.replace(self.path)


def json_safe(value: Any) -> Any:
    if isinstance(value, dt.datetime):
        return value.astimezone(dt.UTC).isoformat() if value.tzinfo else value.isoformat()
    if isinstance(value, dt.date):
        return value.isoformat()
    if isinstance(value, dict):
        return {str(k): json_safe(v) for k, v in value.items()}
    if isinstance(value, (list, tuple, set)):
        return [json_safe(item) for item in value]
    return value


def verify_account(session: boto3.Session) -> None:
    account = session.client("sts").get_caller_identity()["Account"]
    if account != REQUIRED_ACCOUNT:
        raise SystemExit(
            f"refusing to operate in AWS account {account}; expected {REQUIRED_ACCOUNT}"
        )


def enabled_regions(session: boto3.Session, cache: JsonTtlCache) -> list[str]:
    cache_key = "enabled-regions"
    cached = cache.get(cache_key)
    if cached is not None:
        return list(cached)
    ec2 = session.client("ec2", region_name="us-east-1")
    regions = ec2.describe_regions(AllRegions=True)["Regions"]
    out: list[str] = []
    for region in regions:
        status = region.get("OptInStatus", "opt-in-not-required")
        if status in ("opt-in-not-required", "opted-in"):
            out.append(region["RegionName"])
    out = sorted(out)
    cache.set(cache_key, out)
    return out


def describe_types(
    ec2: Any, region: str, instance_types: list[str], cache: JsonTtlCache
) -> dict[str, dict[str, Any]]:
    cache_key = f"{region}:describe-types:{','.join(sorted(instance_types))}"
    cached = cache.get(cache_key)
    if cached is not None:
        return dict(cached)
    out: dict[str, dict[str, Any]] = {}
    for i in range(0, len(instance_types), 100):
        chunk = instance_types[i : i + 100]
        try:
            resp = ec2.describe_instance_types(InstanceTypes=chunk)
        except ClientError as exc:
            code = exc.response.get("Error", {}).get("Code")
            if code not in ("InvalidInstanceType", "InvalidInstanceType.Malformed"):
                raise
            for instance_type in chunk:
                try:
                    resp = ec2.describe_instance_types(InstanceTypes=[instance_type])
                except ClientError:
                    continue
                for item in resp["InstanceTypes"]:
                    out[item["InstanceType"]] = item
            continue
        for item in resp["InstanceTypes"]:
            out[item["InstanceType"]] = item
    cache.set(cache_key, out)
    return out


def offerings_by_type(
    ec2: Any, region: str, instance_types: list[str], cache: JsonTtlCache
) -> dict[str, set[str]]:
    cache_key = f"{region}:offerings:{','.join(sorted(instance_types))}"
    cached = cache.get(cache_key)
    if cached is not None:
        return {key: set(value) for key, value in cached.items()}
    out: dict[str, set[str]] = {item: set() for item in instance_types}
    paginator = ec2.get_paginator("describe_instance_type_offerings")
    for instance_type in instance_types:
        try:
            pages = paginator.paginate(
                LocationType="availability-zone",
                Filters=[{"Name": "instance-type", "Values": [instance_type]}],
            )
            for page in pages:
                for offering in page["InstanceTypeOfferings"]:
                    out.setdefault(offering["InstanceType"], set()).add(offering["Location"])
        except ClientError:
            continue
    cache.set(cache_key, {key: sorted(value) for key, value in out.items()})
    return out


def default_public_subnets_by_az(ec2: Any, region: str, cache: JsonTtlCache) -> dict[str, dict[str, str]]:
    cache_key = f"{region}:default-public-subnets"
    cached = cache.get(cache_key)
    if cached is not None:
        return dict(cached)
    resp = ec2.describe_subnets(
        Filters=[
            {"Name": "default-for-az", "Values": ["true"]},
            {"Name": "map-public-ip-on-launch", "Values": ["true"]},
            {"Name": "state", "Values": ["available"]},
        ]
    )
    out: dict[str, dict[str, str]] = {}
    for subnet in resp["Subnets"]:
        out[subnet["AvailabilityZone"]] = {
            "subnet_id": subnet["SubnetId"],
            "vpc_id": subnet["VpcId"],
        }
    cache.set(cache_key, out)
    return out


def latest_spot_by_az(
    ec2: Any, region: str, instance_type: str, cache: JsonTtlCache
) -> dict[str, float]:
    cache_key = f"{region}:spot:{instance_type}:linux"
    cached = cache.get(cache_key)
    if cached is not None:
        return {key: float(value) for key, value in cached.items()}
    cutoff = utcnow() - dt.timedelta(hours=8)
    paginator = ec2.get_paginator("describe_spot_price_history")
    prices: dict[str, tuple[dt.datetime, float]] = {}
    try:
        pages = paginator.paginate(
            InstanceTypes=[instance_type],
            ProductDescriptions=["Linux/UNIX"],
            StartTime=cutoff,
            PaginationConfig={"PageSize": 1000},
        )
        for page in pages:
            for item in page["SpotPriceHistory"]:
                az = item["AvailabilityZone"]
                ts = item["Timestamp"]
                price = float(item["SpotPrice"])
                if az not in prices or ts > prices[az][0]:
                    prices[az] = (ts, price)
    except ClientError:
        return {}
    out = {az: price for az, (_ts, price) in prices.items()}
    cache.set(cache_key, out)
    return out


def latest_spot_details_by_az(
    ec2: Any,
    region: str,
    instance_type: str,
    cache: JsonTtlCache,
    *,
    hours: int = 8,
) -> dict[str, dict[str, Any]]:
    cache_key = f"{region}:spot-detail:{instance_type}:linux:hours={hours}"
    cached = cache.get(cache_key)
    if cached is not None:
        return {
            az: {
                "price": float(item["price"]),
                "timestamp": str(item["timestamp"]),
            }
            for az, item in dict(cached).items()
        }

    cutoff = utcnow() - dt.timedelta(hours=hours)
    paginator = ec2.get_paginator("describe_spot_price_history")
    prices: dict[str, tuple[dt.datetime, float]] = {}
    try:
        pages = paginator.paginate(
            InstanceTypes=[instance_type],
            ProductDescriptions=["Linux/UNIX"],
            StartTime=cutoff,
            PaginationConfig={"PageSize": 1000},
        )
        for page in pages:
            for item in page["SpotPriceHistory"]:
                az = item["AvailabilityZone"]
                ts = item["Timestamp"]
                price = float(item["SpotPrice"])
                if az not in prices or ts > prices[az][0]:
                    prices[az] = (ts, price)
    except ClientError:
        return {}

    out = {
        az: {
            "price": price,
            "timestamp": ts.astimezone(dt.UTC).isoformat(),
        }
        for az, (ts, price) in prices.items()
    }
    cache.set(cache_key, out)
    return out


def placement_scores(
    ec2: Any,
    region: str,
    instance_type: str,
    nodes: int,
    cache: JsonTtlCache,
) -> dict[str, int]:
    cache_key = f"{region}:placement-score:{instance_type}:nodes={nodes}"
    cached = cache.get(cache_key)
    if cached is not None:
        return {key: int(value) for key, value in cached.items()}
    try:
        resp = ec2.get_spot_placement_scores(
            InstanceTypes=[instance_type],
            TargetCapacity=nodes,
            SingleAvailabilityZone=True,
            RegionNames=[region],
        )
    except ClientError:
        return {}

    scores: dict[str, int] = {}
    for item in resp.get("SpotPlacementScores", []):
        az = item.get("AvailabilityZone")
        score = item.get("Score")
        if az and score is not None:
            scores[az] = int(score)
    cache.set(cache_key, scores)
    return scores


def type_caps(item: dict[str, Any]) -> dict[str, Any]:
    storage = item.get("InstanceStorageInfo") or {}
    network = item.get("NetworkInfo") or {}
    processor = item.get("ProcessorInfo") or {}
    return {
        "arch": (processor.get("SupportedArchitectures") or ["unknown"])[0],
        "vcpus": int(item.get("VCpuInfo", {}).get("DefaultVCpus", 0)),
        "mem_gib": float(item.get("MemoryInfo", {}).get("SizeInMiB", 0)) / 1024.0,
        "local_nvme_gb": int(storage.get("TotalSizeInGB", 0) or 0),
        "network_label": network.get("NetworkPerformance", ""),
        "network_gbps": parse_network_gbps(network.get("NetworkPerformance", "")),
        "efa": bool(network.get("EfaSupported", False)),
    }


def rank_candidates(args: argparse.Namespace) -> list[Candidate]:
    session = boto_session(args.profile)
    verify_account(session)
    cache = JsonTtlCache(args.cache, args.cache_ttl_seconds, args.no_cache)

    regions = parse_csv(args.regions, tuple(enabled_regions(session, cache) if args.all_regions else DEFAULT_REGIONS))
    instance_types = parse_csv(args.instance_types, DEFAULT_INSTANCE_TYPES)
    out: list[Candidate] = []

    for region in regions:
        ec2 = session.client("ec2", region_name=region)
        subnets = default_public_subnets_by_az(ec2, region, cache)
        if not subnets:
            continue
        types = describe_types(ec2, region, instance_types, cache)
        offerings = offerings_by_type(ec2, region, list(types), cache)

        for instance_type, desc in types.items():
            caps = type_caps(desc)
            if args.local_nvme and caps["local_nvme_gb"] <= 0:
                continue
            if args.x86_only and caps["arch"] != "x86_64":
                continue
            if args.metal_only and ".metal" not in instance_type:
                continue
            if args.efa and not caps["efa"]:
                continue
            if caps["network_gbps"] < args.min_network_gbps:
                continue

            scores = (
                placement_scores(ec2, region, instance_type, args.nodes, cache)
                if args.with_placement_score
                else {}
            )
            prices = latest_spot_by_az(ec2, region, instance_type, cache)
            for az, price in prices.items():
                if az not in offerings.get(instance_type, set()):
                    continue
                if az not in subnets:
                    continue
                hourly_total = price * args.nodes
                if args.max_hourly_total is not None and hourly_total > args.max_hourly_total:
                    continue
                local_tb = max(caps["local_nvme_gb"] / 1024.0, 0.01)
                net = max(caps["network_gbps"], 1.0)
                if args.score_mode == "bandwidth":
                    score = net / max(hourly_total, 0.01)
                elif args.score_mode == "memory-bandwidth":
                    score = (net * max(caps["mem_gib"], 1.0)) / max(hourly_total, 0.01)
                else:
                    # Favor cheap high-network local-NVMe boxes, with placement score as a tiebreaker.
                    score = (net * local_tb) / max(hourly_total, 0.01)
                if az in scores:
                    score *= 1.0 + (scores[az] / 20.0)
                out.append(
                    Candidate(
                        region=region,
                        az=az,
                        subnet_id=subnets[az]["subnet_id"],
                        instance_type=instance_type,
                        arch=caps["arch"],
                        vcpus=caps["vcpus"],
                        mem_gib=caps["mem_gib"],
                        local_nvme_gb=caps["local_nvme_gb"],
                        network_gbps=caps["network_gbps"],
                        network_label=caps["network_label"],
                        efa=caps["efa"],
                        spot_price=price,
                        hourly_total=hourly_total,
                        score=score,
                        placement_score=scores.get(az),
                    )
                )

    out.sort(key=lambda item: (-item.score, item.hourly_total, -item.network_gbps))
    cache.save()
    return out


def print_candidates(candidates: list[Candidate], limit: int, json_output: bool) -> None:
    selected = candidates[:limit]
    if json_output:
        print(json.dumps([dataclasses.asdict(item) for item in selected], indent=2, sort_keys=True))
        return

    if not selected:
        print("no candidates matched filters", file=sys.stderr)
        return

    headers = (
        "rank",
        "region",
        "az",
        "subnet",
        "type",
        "arch",
        "net",
        "nvmeGB",
        "efa",
        "$/inst",
        "$/3hr",
        "place",
    )
    rows: list[tuple[str, ...]] = []
    for idx, item in enumerate(selected, 1):
        rows.append(
            (
                str(idx),
                item.region,
                item.az,
                item.subnet_id,
                item.instance_type,
                item.arch,
                item.network_label.replace("Gigabit", "G"),
                str(item.local_nvme_gb),
                "yes" if item.efa else "no",
                f"{item.spot_price:.4f}",
                f"{item.hourly_total * 3.0:.2f}",
                "" if item.placement_score is None else str(item.placement_score),
            )
        )

    widths = [len(item) for item in headers]
    for row in rows:
        for i, value in enumerate(row):
            widths[i] = max(widths[i], len(value))
    fmt = "  ".join("{:<" + str(width) + "}" for width in widths)
    print(fmt.format(*headers))
    print(fmt.format(*("-" * width for width in widths)))
    for row in rows:
        print(fmt.format(*row))


def collect_spot_prices(args: argparse.Namespace) -> list[SpotPriceRow]:
    session = boto_session(args.profile)
    verify_account(session)
    cache = JsonTtlCache(args.cache, args.cache_ttl_seconds, args.no_cache)
    regions = parse_csv(args.regions, tuple(enabled_regions(session, cache) if args.all_regions else DEFAULT_REGIONS))
    instance_types = parse_csv(args.instance_types, DEFAULT_INSTANCE_TYPES)
    out: list[SpotPriceRow] = []

    for region in regions:
        ec2 = session.client("ec2", region_name=region)
        types = describe_types(ec2, region, instance_types, cache)
        offerings = offerings_by_type(ec2, region, list(types), cache)
        for instance_type, desc in types.items():
            caps = type_caps(desc)
            prices = latest_spot_details_by_az(
                ec2,
                region,
                instance_type,
                cache,
                hours=args.history_hours,
            )
            for az, item in prices.items():
                if args.offered_only and az not in offerings.get(instance_type, set()):
                    continue
                price = float(item["price"])
                out.append(
                    SpotPriceRow(
                        region=region,
                        az=az,
                        instance_type=instance_type,
                        arch=caps["arch"],
                        vcpus=caps["vcpus"],
                        mem_gib=caps["mem_gib"],
                        local_nvme_gb=caps["local_nvme_gb"],
                        network_label=caps["network_label"],
                        network_gbps=caps["network_gbps"],
                        efa=caps["efa"],
                        spot_price=price,
                        hourly_total=price * args.nodes,
                        timestamp=str(item["timestamp"]),
                    )
                )

    sort_keys = {
        "price": lambda item: (item.spot_price, item.region, item.az, item.instance_type),
        "region": lambda item: (item.region, item.az, item.instance_type, item.spot_price),
        "network-value": lambda item: (
            item.spot_price / max(item.network_gbps, 1.0),
            item.region,
            item.az,
            item.instance_type,
        ),
    }
    out.sort(key=sort_keys[args.sort])
    cache.save()
    return out


def print_spot_prices(rows: list[SpotPriceRow], limit: int, json_output: bool) -> None:
    selected = rows[:limit]
    if json_output:
        print(json.dumps([dataclasses.asdict(item) for item in selected], indent=2, sort_keys=True))
        return
    if not selected:
        print("no spot prices matched filters", file=sys.stderr)
        return

    headers = (
        "region",
        "az",
        "type",
        "arch",
        "vcpus",
        "memGiB",
        "net",
        "nvmeGB",
        "efa",
        "$/inst-hr",
        "$/cluster-hr",
        "$/Gbit-hr",
        "timestamp",
    )
    rows_out: list[tuple[str, ...]] = []
    for item in selected:
        rows_out.append(
            (
                item.region,
                item.az,
                item.instance_type,
                item.arch,
                str(item.vcpus),
                f"{item.mem_gib:.0f}",
                item.network_label.replace("Gigabit", "G"),
                str(item.local_nvme_gb),
                "yes" if item.efa else "no",
                f"{item.spot_price:.4f}",
                f"{item.hourly_total:.4f}",
                f"{item.spot_price / max(item.network_gbps, 1.0):.5f}",
                item.timestamp,
            )
        )

    widths = [len(item) for item in headers]
    for row in rows_out:
        for i, value in enumerate(row):
            widths[i] = max(widths[i], len(value))
    fmt = "  ".join("{:<" + str(width) + "}" for width in widths)
    print(fmt.format(*headers))
    print(fmt.format(*("-" * width for width in widths)))
    for row in rows_out:
        print(fmt.format(*row))


def resolve_ami(session: boto3.Session, region: str, arch: str) -> str:
    param = UBUNTU_2404_AMI_PARAM.get(arch)
    if not param:
        raise SystemExit(f"no default AMI parameter for architecture {arch}; pass --ami-id")
    ssm = session.client("ssm", region_name=region)
    return ssm.get_parameter(Name=param)["Parameter"]["Value"]


def infer_arch(session: boto3.Session, region: str, instance_type: str) -> str:
    ec2 = session.client("ec2", region_name=region)
    desc = ec2.describe_instance_types(InstanceTypes=[instance_type])["InstanceTypes"][0]
    return type_caps(desc)["arch"]


def validate_drop_dead(raw: str, allow_over_3h: bool) -> dt.datetime:
    drop_dead = parse_utc_timestamp(raw)
    now = utcnow()
    if drop_dead <= now + dt.timedelta(minutes=15):
        raise SystemExit("drop-dead must be more than 15 minutes in the future")
    if drop_dead > now + dt.timedelta(hours=3) and not allow_over_3h:
        raise SystemExit("drop-dead is over 3 hours out; pass --approve-over-3h")
    if drop_dead > now + dt.timedelta(hours=4, minutes=5):
        raise SystemExit("refusing leases beyond about 4 hours for this adhoc launcher")
    return drop_dead


def estimate_launch_ceiling(
    nodes: int,
    drop_dead: dt.datetime,
    max_spot_price: str | None,
    associate_public_ip: bool,
    root_gb: int,
) -> dict[str, float | None]:
    hours = max((drop_dead - utcnow()).total_seconds() / 3600.0, 0.0)
    public_ipv4 = nodes * hours * PUBLIC_IPV4_HOURLY_USD if associate_public_ip else 0.0
    root_ebs = nodes * root_gb * (ROOT_EBS_GB_MONTH_USD / 730.0) * hours
    spot = None
    total = None
    if max_spot_price is not None:
        spot = nodes * float(max_spot_price) * hours
        total = spot + public_ipv4 + root_ebs
    return {
        "duration_hours": hours,
        "spot_ceiling_usd": spot,
        "public_ipv4_ceiling_usd": public_ipv4,
        "root_ebs_ceiling_usd": root_ebs,
        "total_ceiling_usd": total,
    }


def current_public_ipv4() -> str:
    try:
        result = subprocess.run(
            ["curl", "-fsS4", "--max-time", "5", "https://checkip.amazonaws.com"],
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
    except subprocess.CalledProcessError as exc:
        raise SystemExit(f"failed to discover public IPv4: {exc.stderr.strip()}") from exc
    ip = result.stdout.strip()
    if not re.fullmatch(r"\d+\.\d+\.\d+\.\d+", ip):
        raise SystemExit(f"unexpected public IPv4 response: {ip!r}")
    return ip


def subnet_vpc_id(ec2: Any, subnet_id: str) -> str:
    resp = ec2.describe_subnets(SubnetIds=[subnet_id])
    return resp["Subnets"][0]["VpcId"]


def adhoc_support_tags(
    name: str,
    region: str,
    *,
    az: str | None = None,
    kind: str,
) -> list[dict[str, str]]:
    tags = [
        {"Key": "Name", "Value": name},
        {"Key": "Project", "Value": "uring-play"},
        {"Key": "Purpose", "Value": ADHOC_SUPPORT_PURPOSE},
        {"Key": "AdhocSupport", "Value": "true"},
        {"Key": "AdhocSupportLifetime", "Value": "shared-regional"},
        {"Key": "AdhocSupportKind", "Value": kind},
        {"Key": "AdhocSupportRegion", "Value": region},
        {"Key": "AdhocCleanupScope", "Value": f"region:{region}"},
    ]
    if az:
        tags.extend(
            [
                {"Key": "AdhocSupportAz", "Value": az},
                {"Key": "AdhocCleanupScopeAz", "Value": f"az:{az}"},
            ]
        )
    return tags


def ensure_control_sg_result(
    session: boto3.Session,
    *,
    region: str,
    subnet_id: str,
    vpc_id: str | None,
    name: str,
    ssh_cidr: str | None,
    yes: bool,
) -> dict[str, Any]:
    ec2 = session.client("ec2", region_name=region)
    vpc_id = vpc_id or subnet_vpc_id(ec2, subnet_id)
    ssh_cidr = ssh_cidr or f"{current_public_ipv4()}/32"

    resp = ec2.describe_security_groups(
        Filters=[
            {"Name": "group-name", "Values": [name]},
            {"Name": "vpc-id", "Values": [vpc_id]},
        ]
    )
    created = False
    if resp["SecurityGroups"]:
        sg = resp["SecurityGroups"][0]
        group_id = sg["GroupId"]
        if yes:
            ec2.create_tags(
                Resources=[group_id],
                Tags=adhoc_support_tags(name, region, kind="control-security-group"),
            )
    else:
        if not yes:
            return {
                "region": region,
                "vpc_id": vpc_id,
                "name": name,
                "ssh_cidr": ssh_cidr,
                "status": "would-create",
                "tags": adhoc_support_tags(name, region, kind="control-security-group"),
            }
        create = ec2.create_security_group(
            GroupName=name,
            Description="uring-play adhoc support: public SSH control and private self traffic",
            VpcId=vpc_id,
            TagSpecifications=[
                {
                    "ResourceType": "security-group",
                    "Tags": adhoc_support_tags(name, region, kind="control-security-group"),
                }
            ],
        )
        group_id = create["GroupId"]
        created = True

    ingress_rules = [
        {
            "IpProtocol": "tcp",
            "FromPort": 22,
            "ToPort": 22,
            "IpRanges": [{"CidrIp": ssh_cidr, "Description": "adhoc SSH control"}],
        },
        {
            "IpProtocol": "-1",
            "UserIdGroupPairs": [
                {
                    "GroupId": group_id,
                    "Description": "adhoc private worker traffic inside this SG",
                }
            ],
        },
    ]
    if yes:
        for rule in ingress_rules:
            try:
                ec2.authorize_security_group_ingress(GroupId=group_id, IpPermissions=[rule])
            except ClientError as exc:
                code = exc.response.get("Error", {}).get("Code")
                if code != "InvalidPermission.Duplicate":
                    raise

    return {
        "region": region,
        "vpc_id": vpc_id,
        "security_group_id": group_id,
        "name": name,
        "ssh_cidr": ssh_cidr,
        "created": created,
        "status": "ready" if yes else "exists-dry-run",
    }


def ensure_control_sg(args: argparse.Namespace) -> None:
    session = boto_session(args.profile)
    verify_account(session)
    result = ensure_control_sg_result(
        session,
        region=args.region,
        subnet_id=args.subnet_id,
        vpc_id=args.vpc_id,
        name=args.name,
        ssh_cidr=args.ssh_cidr,
        yes=args.yes,
    )
    print(json.dumps(result, indent=2, sort_keys=True))


def launch_instances(args: argparse.Namespace) -> None:
    session = boto_session(args.profile)
    verify_account(session)
    drop_dead = validate_drop_dead(args.drop_dead_utc, args.approve_over_3h)
    arch = infer_arch(session, args.region, args.instance_type)
    ami_id = args.ami_id or resolve_ami(session, args.region, arch)
    run_id = args.run_id or f"uring-perf-{utcnow().strftime('%Y%m%dT%H%M%SZ')}-{uuid.uuid4().hex[:8]}"
    ec2 = session.client("ec2", region_name=args.region)
    security_group_ids = split_values(args.security_group_ids)
    if args.security_group_id:
        security_group_ids.append(args.security_group_id)
    security_group_ids = list(dict.fromkeys(security_group_ids))
    if not security_group_ids:
        raise SystemExit("pass at least one existing security group with --security-group-ids")
    if args.yes and args.max_spot_price is None:
        raise SystemExit("launch requires --max-spot-price so the spend ceiling is bounded")
    cost_ceiling = estimate_launch_ceiling(
        args.nodes,
        drop_dead,
        args.max_spot_price,
        bool(args.associate_public_ip),
        args.root_gb,
    )
    if cost_ceiling["total_ceiling_usd"] is not None:
        total = float(cost_ceiling["total_ceiling_usd"])
        if total > args.max_total_cost:
            raise SystemExit(
                f"refusing launch: estimated ceiling ${total:.2f} exceeds "
                f"--max-total-cost ${args.max_total_cost:.2f}"
            )

    tags = [
        {"Key": "Name", "Value": run_id},
        {"Key": "Project", "Value": "uring-play"},
        {"Key": "Purpose", "Value": "adhoc-performance-compute"},
        {"Key": "uringPlayRunId", "Value": run_id},
        {"Key": "adhocKeepaliveModeAction", "Value": "terminate"},
        {"Key": "adhocKeepalive", "Value": drop_dead.strftime("%Y-%m-%dT%H:%M:%SZ")},
    ]
    network_interface: dict[str, Any] = {
        "DeviceIndex": 0,
        "SubnetId": args.subnet_id,
        "Groups": security_group_ids,
        "AssociatePublicIpAddress": bool(args.associate_public_ip),
    }
    if args.enable_efa:
        network_interface["InterfaceType"] = "efa"
    if args.ena_express:
        network_interface["EnaSrdSpecification"] = {
            "EnaSrdEnabled": True,
            "EnaSrdUdpSpecification": {"EnaSrdUdpEnabled": True},
        }

    spot_options: dict[str, Any] = {
        "SpotInstanceType": "one-time",
        "InstanceInterruptionBehavior": "terminate",
    }
    if args.max_spot_price:
        spot_options["MaxPrice"] = str(args.max_spot_price)

    request = {
        "ImageId": ami_id,
        "InstanceType": args.instance_type,
        "MinCount": args.nodes,
        "MaxCount": args.nodes,
        "KeyName": args.key_name,
        "InstanceMarketOptions": {
            "MarketType": "spot",
            "SpotOptions": spot_options,
        },
        "NetworkInterfaces": [network_interface],
        "Placement": {"AvailabilityZone": args.availability_zone},
        "BlockDeviceMappings": [
            {
                "DeviceName": "/dev/sda1",
                "Ebs": {
                    "DeleteOnTermination": True,
                    "VolumeSize": args.root_gb,
                    "VolumeType": "gp3",
                },
            }
        ],
        "TagSpecifications": [
            {"ResourceType": "instance", "Tags": tags},
            {"ResourceType": "volume", "Tags": tags},
        ],
        "MetadataOptions": {"HttpTokens": "required", "HttpEndpoint": "enabled"},
    }
    if args.placement_group:
        request["Placement"]["GroupName"] = args.placement_group

    print(
        json.dumps(
            {
                "run_id": run_id,
                "region": args.region,
                "az": args.availability_zone,
                "subnet_id": args.subnet_id,
                "instance_type": args.instance_type,
                "nodes": args.nodes,
                "ami_id": ami_id,
                "drop_dead_utc": drop_dead.strftime("%Y-%m-%dT%H:%M:%SZ"),
                "enable_efa": args.enable_efa,
                "ena_express": args.ena_express,
                "cost_ceiling": cost_ceiling,
                "request": request,
            },
            indent=2,
            sort_keys=True,
        )
    )
    if not args.yes:
        raise SystemExit("dry run only; pass --yes to launch")

    resp = ec2.run_instances(**request)
    instance_ids = [item["InstanceId"] for item in resp["Instances"]]
    waiter = ec2.get_waiter("instance_running")
    waiter.wait(InstanceIds=instance_ids)
    desc = ec2.describe_instances(InstanceIds=instance_ids)
    instances: list[dict[str, Any]] = []
    for reservation in desc["Reservations"]:
        for inst in reservation["Instances"]:
            instances.append(
                {
                    "instance_id": inst["InstanceId"],
                    "private_ip": inst.get("PrivateIpAddress"),
                    "public_ip": inst.get("PublicIpAddress"),
                    "az": inst["Placement"]["AvailabilityZone"],
                    "instance_type": inst["InstanceType"],
                }
            )
    inventory = {
        "run_id": run_id,
        "region": args.region,
        "drop_dead_utc": drop_dead.strftime("%Y-%m-%dT%H:%M:%SZ"),
        "instances": sorted(instances, key=lambda item: item["private_ip"] or ""),
    }
    path = pathlib.Path(args.inventory)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(inventory, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(json.dumps(inventory, indent=2, sort_keys=True))
    print(f"wrote inventory: {path}")


def load_inventory(path: str) -> dict[str, Any]:
    return json.loads(pathlib.Path(path).read_text(encoding="utf-8"))


def ssh_target(user: str, host: str) -> str:
    return f"{user}@{host}"


def run_checked(cmd: list[str], dry_run: bool) -> None:
    print("+", " ".join(shlex.quote(part) for part in cmd))
    if not dry_run:
        subprocess.run(cmd, check=True)


def load_public_key_material(public_key_path: str | None, private_key_path: str | None) -> str:
    if public_key_path:
        expanded_public = os.path.expanduser(public_key_path)
        try:
            public_key = pathlib.Path(expanded_public).read_text(encoding="utf-8").strip()
        except OSError as exc:
            raise SystemExit(f"failed to read public key {expanded_public}: {exc}") from exc
        if not public_key.startswith("ssh-ed25519 "):
            raise SystemExit(f"refusing non-ed25519 public key from {expanded_public}")
        return public_key

    if private_key_path is None:
        raise SystemExit("pass --public-key or --private-key")
    expanded = os.path.expanduser(private_key_path)
    try:
        result = subprocess.run(
            ["ssh-keygen", "-y", "-f", expanded],
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
    except subprocess.CalledProcessError as exc:
        raise SystemExit(f"failed to derive public key from {expanded}: {exc.stderr.strip()}") from exc
    public_key = result.stdout.strip()
    if not public_key.startswith("ssh-ed25519 "):
        raise SystemExit(f"refusing non-ed25519 public key from {expanded}")
    return public_key


def ensure_key_pair_result(
    session: boto3.Session,
    *,
    region: str,
    key_name: str,
    public_key: str,
    expected_fingerprint: str | None,
    yes: bool,
) -> dict[str, Any]:
    ec2 = session.client("ec2", region_name=region)
    try:
        resp = ec2.describe_key_pairs(KeyNames=[key_name])
        kp = resp["KeyPairs"][0]
        fingerprint = kp.get("KeyFingerprint")
        if expected_fingerprint and fingerprint != expected_fingerprint:
            raise SystemExit(
                f"{region}: key {key_name} exists with fingerprint "
                f"{fingerprint}, expected {expected_fingerprint}"
            )
        if yes and kp.get("KeyPairId"):
            ec2.create_tags(
                Resources=[kp["KeyPairId"]],
                Tags=adhoc_support_tags(key_name, region, kind="ssh-key-pair"),
            )
        return {
            "region": region,
            "status": "exists-tagged" if yes else "exists",
            "key_pair_id": kp.get("KeyPairId"),
            "fingerprint": fingerprint,
            "key_name": key_name,
        }
    except ClientError as exc:
        code = exc.response.get("Error", {}).get("Code")
        if code != "InvalidKeyPair.NotFound":
            raise

    if not yes:
        return {
            "region": region,
            "status": "would-import",
            "key_name": key_name,
            "tags": adhoc_support_tags(key_name, region, kind="ssh-key-pair"),
        }
    try:
        resp = ec2.import_key_pair(
            KeyName=key_name,
            PublicKeyMaterial=public_key,
            TagSpecifications=[
                {
                    "ResourceType": "key-pair",
                    "Tags": adhoc_support_tags(key_name, region, kind="ssh-key-pair"),
                }
            ],
        )
    except ClientError as exc:
        code = exc.response.get("Error", {}).get("Code")
        if code == "InvalidParameterValue":
            resp = ec2.import_key_pair(
                KeyName=key_name,
                PublicKeyMaterial=public_key,
            )
            key_pair_id = resp.get("KeyPairId")
            if key_pair_id:
                ec2.create_tags(
                    Resources=[key_pair_id],
                    Tags=adhoc_support_tags(key_name, region, kind="ssh-key-pair"),
                )
        else:
            raise
    return {
        "region": region,
        "status": "imported",
        "key_pair_id": resp.get("KeyPairId"),
        "fingerprint": resp.get("KeyFingerprint"),
        "key_name": key_name,
    }


def register_key(args: argparse.Namespace) -> None:
    session = boto_session(args.profile)
    verify_account(session)
    cache = JsonTtlCache(args.cache, args.cache_ttl_seconds, args.no_cache)
    regions = parse_csv(args.regions, tuple(enabled_regions(session, cache) if args.all_regions else DEFAULT_REGIONS))
    public_key = load_public_key_material(args.public_key, args.private_key)

    results = [
        ensure_key_pair_result(
            session,
            region=region,
            key_name=args.key_name,
            public_key=public_key,
            expected_fingerprint=args.expected_fingerprint,
            yes=args.yes,
        )
        for region in regions
    ]

    cache.save()
    print(json.dumps(results, indent=2, sort_keys=True))


def ensure_placement_group_result(
    session: boto3.Session,
    *,
    region: str,
    name: str,
    yes: bool,
) -> dict[str, Any]:
    ec2 = session.client("ec2", region_name=region)
    try:
        resp = ec2.describe_placement_groups(GroupNames=[name])
        group = resp["PlacementGroups"][0]
        return {
            "region": region,
            "name": name,
            "status": "exists",
            "strategy": group.get("Strategy"),
            "state": group.get("State"),
        }
    except ClientError as exc:
        code = exc.response.get("Error", {}).get("Code")
        if code != "InvalidPlacementGroup.Unknown":
            raise
    if not yes:
        return {
            "region": region,
            "name": name,
            "status": "would-create",
            "strategy": "cluster",
            "tags": adhoc_support_tags(name, region, kind="placement-group"),
        }
    ec2.create_placement_group(
        GroupName=name,
        Strategy="cluster",
        TagSpecifications=[
            {
                "ResourceType": "placement-group",
                "Tags": adhoc_support_tags(name, region, kind="placement-group"),
            }
        ],
    )
    return {
        "region": region,
        "name": name,
        "status": "created",
        "strategy": "cluster",
    }


def prep_region_az(args: argparse.Namespace) -> None:
    session = boto_session(args.profile)
    verify_account(session)
    cache = JsonTtlCache(args.cache, args.cache_ttl_seconds, args.no_cache)
    ec2 = session.client("ec2", region_name=args.region)
    subnets = default_public_subnets_by_az(ec2, args.region, cache)
    subnet_id = args.subnet_id
    if not subnet_id:
        if args.availability_zone not in subnets:
            raise SystemExit(f"no default public subnet found for {args.availability_zone}")
        subnet_id = subnets[args.availability_zone]["subnet_id"]
    vpc_id = args.vpc_id or subnet_vpc_id(ec2, subnet_id)
    public_key = load_public_key_material(args.public_key, args.private_key)
    key = ensure_key_pair_result(
        session,
        region=args.region,
        key_name=args.key_name,
        public_key=public_key,
        expected_fingerprint=args.expected_fingerprint,
        yes=args.yes,
    )
    sg = ensure_control_sg_result(
        session,
        region=args.region,
        subnet_id=subnet_id,
        vpc_id=vpc_id,
        name=args.security_group_name,
        ssh_cidr=args.ssh_cidr,
        yes=args.yes,
    )
    placement = None
    if args.placement_group:
        placement = ensure_placement_group_result(
            session,
            region=args.region,
            name=args.placement_group,
            yes=args.yes,
        )
    cache.save()
    launch_args = [
        "scripts/ec2_perf_spot.py",
        "launch",
        "--region",
        args.region,
        "--availability-zone",
        args.availability_zone,
        "--subnet-id",
        subnet_id,
        "--security-group-ids",
        sg.get("security_group_id", "<CONTROL_SG_ID>"),
        "--key-name",
        args.key_name,
    ]
    if placement is not None:
        launch_args.extend(["--placement-group", args.placement_group])
    print(
        json.dumps(
            {
                "status": "ready" if args.yes else "dry-run",
                "region": args.region,
                "availability_zone": args.availability_zone,
                "subnet_id": subnet_id,
                "vpc_id": vpc_id,
                "no_hourly_cost_components": [
                    "ec2 key pair import",
                    "security group",
                    *([] if placement is None else ["cluster placement group"]),
                ],
                "key_pair": key,
                "control_security_group": sg,
                "placement_group": placement,
                "launch_args_prefix": launch_args,
            },
            indent=2,
            sort_keys=True,
        )
    )


def list_adhoc_support(args: argparse.Namespace) -> None:
    session = boto_session(args.profile)
    verify_account(session)
    regions = parse_csv(args.regions, tuple(DEFAULT_REGIONS if not args.all_regions else enabled_regions(session, JsonTtlCache(args.cache, args.cache_ttl_seconds, args.no_cache))))
    out: list[dict[str, Any]] = []
    for region in regions:
        ec2 = session.client("ec2", region_name=region)
        sgs = ec2.describe_security_groups(
            Filters=[
                {"Name": "tag:Project", "Values": ["uring-play"]},
                {"Name": "tag:AdhocSupport", "Values": ["true"]},
            ]
        )["SecurityGroups"]
        for sg in sgs:
            out.append(
                {
                    "region": region,
                    "kind": "security-group",
                    "id": sg["GroupId"],
                    "name": sg["GroupName"],
                    "vpc_id": sg["VpcId"],
                    "tags": sg.get("Tags", []),
                }
            )
        try:
            pgs = ec2.describe_placement_groups(
                Filters=[
                    {"Name": "tag:Project", "Values": ["uring-play"]},
                    {"Name": "tag:AdhocSupport", "Values": ["true"]},
                ]
            )["PlacementGroups"]
        except ClientError:
            pgs = []
        for pg in pgs:
            out.append(
                {
                    "region": region,
                    "kind": "placement-group",
                    "name": pg["GroupName"],
                    "strategy": pg.get("Strategy"),
                    "state": pg.get("State"),
                    "tags": pg.get("Tags", []),
                }
            )
        try:
            keys = ec2.describe_key_pairs(
                Filters=[
                    {"Name": "tag:Project", "Values": ["uring-play"]},
                    {"Name": "tag:AdhocSupport", "Values": ["true"]},
                ]
            )["KeyPairs"]
        except ClientError:
            keys = []
        for key in keys:
            out.append(
                {
                    "region": region,
                    "kind": "key-pair",
                    "id": key.get("KeyPairId"),
                    "name": key.get("KeyName"),
                    "fingerprint": key.get("KeyFingerprint"),
                    "tags": key.get("Tags", []),
                }
            )
    print(json.dumps(out, indent=2, sort_keys=True))


def sync_repo(args: argparse.Namespace) -> None:
    inv = load_inventory(args.inventory)
    root = pathlib.Path(args.repo).resolve()
    excludes = [
        ".git/",
        "target/",
        "qemu-zcrx/*.log",
        "qemu-zcrx/host-*",
        "qemu-zcrx/raft-lab-*",
        "qemu-zcrx/local-bench-*",
    ]
    for inst in inv["instances"]:
        host = inst["public_ip"] if args.public_ip else inst["private_ip"]
        if not host:
            raise SystemExit(f"instance {inst['instance_id']} has no selected IP")
        remote = f"{ssh_target(args.user, host)}:{args.remote_dir.rstrip('/')}/"
        cmd = [
            "rsync",
            "-az",
            "--info=progress2",
            "-e",
            args.ssh_cmd,
        ]
        for exclude in excludes:
            cmd.extend(["--exclude", exclude])
        cmd.extend([str(root) + "/", remote])
        run_checked(cmd, args.dry_run)


def remote_exec(args: argparse.Namespace) -> None:
    inv = load_inventory(args.inventory)
    for idx, inst in enumerate(inv["instances"], 1):
        host = inst["public_ip"] if args.public_ip else inst["private_ip"]
        if not host:
            raise SystemExit(f"instance {inst['instance_id']} has no selected IP")
        env = {
            "URING_NODE_INDEX": str(idx),
            "URING_RUN_ID": inv["run_id"],
            "URING_PRIVATE_IPS": ",".join(item["private_ip"] for item in inv["instances"]),
        }
        remote_cmd = " ".join(
            [*(f"{key}={shlex.quote(value)}" for key, value in env.items()), args.command]
        )
        cmd = [*shlex.split(args.ssh_cmd), ssh_target(args.user, host), remote_cmd]
        run_checked(cmd, args.dry_run)


def ssh_commands(args: argparse.Namespace) -> None:
    inv = load_inventory(args.inventory)
    for idx, inst in enumerate(inv["instances"], 1):
        host = inst["public_ip"] if args.public_ip else inst["private_ip"]
        if not host:
            raise SystemExit(f"instance {inst['instance_id']} has no selected IP")
        parts = shlex.split(args.ssh_cmd)
        if args.identity_file:
            parts.extend(["-i", os.path.expanduser(args.identity_file)])
        parts.append(ssh_target(args.user, host))
        print(f"node{idx} {inst['instance_id']} {host}:")
        print("  " + " ".join(shlex.quote(part) for part in parts))


def terminate_run(args: argparse.Namespace) -> None:
    session = boto_session(args.profile)
    verify_account(session)
    ec2 = session.client("ec2", region_name=args.region)
    resp = ec2.describe_instances(
        Filters=[
            {"Name": "tag:uringPlayRunId", "Values": [args.run_id]},
            {
                "Name": "instance-state-name",
                "Values": ["pending", "running", "stopping", "stopped"],
            },
        ]
    )
    ids: list[str] = []
    for reservation in resp["Reservations"]:
        for inst in reservation["Instances"]:
            ids.append(inst["InstanceId"])
    if not ids:
        print("no matching instances")
        return
    print("terminating", " ".join(ids))
    if args.yes:
        ec2.terminate_instances(InstanceIds=ids)
    else:
        raise SystemExit("dry run only; pass --yes to terminate")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Select and manage bounded adhoc EC2 Spot clusters for uring-play speed tests."
    )
    sub = parser.add_subparsers(dest="cmd", required=True)

    recommend = sub.add_parser("recommend")
    recommend.add_argument("--profile", default=DEFAULT_PROFILE)
    recommend.add_argument("--regions", help="comma-separated regions; defaults to a curated set")
    recommend.add_argument("--all-regions", action="store_true")
    recommend.add_argument("--instance-types", help="comma-separated instance types")
    recommend.add_argument("--nodes", type=int, default=DEFAULT_NODES)
    recommend.add_argument("--min-network-gbps", type=float, default=100.0)
    recommend.add_argument("--max-hourly-total", type=float, default=12.50)
    recommend.add_argument("--local-nvme", action=argparse.BooleanOptionalAction, default=True)
    recommend.add_argument("--x86-only", action=argparse.BooleanOptionalAction, default=True)
    recommend.add_argument("--metal-only", action=argparse.BooleanOptionalAction, default=False)
    recommend.add_argument("--efa", action=argparse.BooleanOptionalAction, default=True)
    recommend.add_argument(
        "--score-mode",
        choices=("balanced", "bandwidth", "memory-bandwidth"),
        default="balanced",
    )
    recommend.add_argument("--with-placement-score", action="store_true")
    recommend.add_argument("--cache", default=str(DEFAULT_CACHE_PATH))
    recommend.add_argument("--cache-ttl-seconds", type=int, default=DEFAULT_CACHE_TTL_SECONDS)
    recommend.add_argument("--no-cache", action="store_true")
    recommend.add_argument("--limit", type=int, default=20)
    recommend.add_argument("--json", action="store_true")
    recommend.set_defaults(func=lambda args: print_candidates(rank_candidates(args), args.limit, args.json))

    prices = sub.add_parser("spot-prices")
    prices.add_argument("--profile", default=DEFAULT_PROFILE)
    prices.add_argument("--regions", help="comma-separated regions; defaults to the recommendation set")
    prices.add_argument("--all-regions", action="store_true")
    prices.add_argument("--instance-types", required=True, help="comma-separated instance types")
    prices.add_argument("--nodes", type=int, default=DEFAULT_NODES)
    prices.add_argument("--history-hours", type=int, default=8)
    prices.add_argument("--offered-only", action=argparse.BooleanOptionalAction, default=True)
    prices.add_argument("--sort", choices=("price", "region", "network-value"), default="price")
    prices.add_argument("--cache", default=str(DEFAULT_CACHE_PATH))
    prices.add_argument("--cache-ttl-seconds", type=int, default=DEFAULT_CACHE_TTL_SECONDS)
    prices.add_argument("--no-cache", action="store_true")
    prices.add_argument("--limit", type=int, default=20)
    prices.add_argument("--json", action="store_true")
    prices.set_defaults(func=lambda args: print_spot_prices(collect_spot_prices(args), args.limit, args.json))

    keyp = sub.add_parser("register-key")
    keyp.add_argument("--profile", default=DEFAULT_PROFILE)
    keyp.add_argument("--regions", help="comma-separated regions; defaults to the recommendation set")
    keyp.add_argument("--all-regions", action="store_true")
    keyp.add_argument("--key-name", default=DEFAULT_KEY_NAME)
    keyp.add_argument("--public-key", default=DEFAULT_SSH_PUBLIC_KEY_PATH)
    keyp.add_argument("--private-key")
    keyp.add_argument("--expected-fingerprint", default=DEFAULT_KEY_FINGERPRINT_AWS)
    keyp.add_argument("--cache", default=str(DEFAULT_CACHE_PATH))
    keyp.add_argument("--cache-ttl-seconds", type=int, default=DEFAULT_CACHE_TTL_SECONDS)
    keyp.add_argument("--no-cache", action="store_true")
    keyp.add_argument("--yes", action="store_true")
    keyp.set_defaults(func=register_key)

    sgp = sub.add_parser("ensure-control-sg")
    sgp.add_argument("--profile", default=DEFAULT_PROFILE)
    sgp.add_argument("--region", required=True)
    sgp.add_argument("--subnet-id", required=True)
    sgp.add_argument("--vpc-id")
    sgp.add_argument("--name", default=f"{ADHOC_SUPPORT_PREFIX}-ctl")
    sgp.add_argument("--ssh-cidr", help="defaults to current public IPv4 /32")
    sgp.add_argument("--yes", action="store_true")
    sgp.set_defaults(func=ensure_control_sg)

    prep = sub.add_parser("prep-region-az")
    prep.add_argument("--profile", default=DEFAULT_PROFILE)
    prep.add_argument("--region", required=True)
    prep.add_argument("--availability-zone", required=True)
    prep.add_argument("--subnet-id")
    prep.add_argument("--vpc-id")
    prep.add_argument("--security-group-name", default=f"{ADHOC_SUPPORT_PREFIX}-ctl")
    prep.add_argument("--ssh-cidr", help="defaults to current public IPv4 /32")
    prep.add_argument("--placement-group", help="optional no-hourly-cost cluster placement group name")
    prep.add_argument("--key-name", default=DEFAULT_KEY_NAME)
    prep.add_argument("--public-key", default=DEFAULT_SSH_PUBLIC_KEY_PATH)
    prep.add_argument("--private-key")
    prep.add_argument("--expected-fingerprint", default=DEFAULT_KEY_FINGERPRINT_AWS)
    prep.add_argument("--cache", default=str(DEFAULT_CACHE_PATH))
    prep.add_argument("--cache-ttl-seconds", type=int, default=DEFAULT_CACHE_TTL_SECONDS)
    prep.add_argument("--no-cache", action="store_true")
    prep.add_argument("--yes", action="store_true")
    prep.set_defaults(func=prep_region_az)

    listp = sub.add_parser("list-adhoc-support")
    listp.add_argument("--profile", default=DEFAULT_PROFILE)
    listp.add_argument("--regions", help="comma-separated regions; defaults to the recommendation set")
    listp.add_argument("--all-regions", action="store_true")
    listp.add_argument("--cache", default=str(DEFAULT_CACHE_PATH))
    listp.add_argument("--cache-ttl-seconds", type=int, default=DEFAULT_CACHE_TTL_SECONDS)
    listp.add_argument("--no-cache", action="store_true")
    listp.set_defaults(func=list_adhoc_support)

    launch = sub.add_parser("launch")
    launch.add_argument("--profile", default=DEFAULT_PROFILE)
    launch.add_argument("--region", required=True)
    launch.add_argument("--availability-zone", required=True)
    launch.add_argument("--subnet-id", required=True)
    launch.add_argument("--security-group-id")
    launch.add_argument("--security-group-ids", nargs="*", help="existing SG ids; accepts spaces or commas")
    launch.add_argument("--key-name", default=DEFAULT_KEY_NAME)
    launch.add_argument("--instance-type", required=True)
    launch.add_argument("--nodes", type=int, default=DEFAULT_NODES)
    launch.add_argument("--drop-dead-utc", required=True)
    launch.add_argument("--approve-over-3h", action="store_true")
    launch.add_argument("--ami-id")
    launch.add_argument("--max-spot-price")
    launch.add_argument("--max-total-cost", type=float, default=DEFAULT_MAX_DAILY_COST_USD)
    launch.add_argument("--root-gb", type=int, default=64)
    launch.add_argument("--enable-efa", action=argparse.BooleanOptionalAction, default=True)
    launch.add_argument("--ena-express", action=argparse.BooleanOptionalAction, default=False)
    launch.add_argument("--associate-public-ip", action=argparse.BooleanOptionalAction, default=True)
    launch.add_argument("--placement-group", help="use an existing placement group; this script will not create one")
    launch.add_argument("--run-id")
    launch.add_argument("--inventory", default="qemu-zcrx/ec2-adhoc-inventory.json")
    launch.add_argument("--yes", action="store_true")
    launch.set_defaults(func=launch_instances)

    sync = sub.add_parser("sync")
    sync.add_argument("--inventory", required=True)
    sync.add_argument("--repo", default=".")
    sync.add_argument("--remote-dir", default="~/uring-play")
    sync.add_argument("--user", default="ubuntu")
    sync.add_argument("--ssh-cmd", default=DEFAULT_SSH_CMD)
    sync.add_argument("--public-ip", action=argparse.BooleanOptionalAction, default=True)
    sync.add_argument("--dry-run", action="store_true")
    sync.set_defaults(func=sync_repo)

    rexec = sub.add_parser("exec")
    rexec.add_argument("--inventory", required=True)
    rexec.add_argument("--user", default="ubuntu")
    rexec.add_argument("--ssh-cmd", default=DEFAULT_SSH_CMD)
    rexec.add_argument("--public-ip", action=argparse.BooleanOptionalAction, default=True)
    rexec.add_argument("--dry-run", action="store_true")
    rexec.add_argument("command")
    rexec.set_defaults(func=remote_exec)

    sshp = sub.add_parser("ssh-commands")
    sshp.add_argument("--inventory", required=True)
    sshp.add_argument("--user", default="ubuntu")
    sshp.add_argument("--ssh-cmd", default="ssh -o StrictHostKeyChecking=accept-new -o ServerAliveInterval=30")
    sshp.add_argument("--identity-file", default=DEFAULT_SSH_KEY_PATH)
    sshp.add_argument("--public-ip", action=argparse.BooleanOptionalAction, default=True)
    sshp.set_defaults(func=ssh_commands)

    terminate = sub.add_parser("terminate")
    terminate.add_argument("--profile", default=DEFAULT_PROFILE)
    terminate.add_argument("--region", required=True)
    terminate.add_argument("--run-id", required=True)
    terminate.add_argument("--yes", action="store_true")
    terminate.set_defaults(func=terminate_run)

    return parser


def main() -> None:
    parser = build_parser()
    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
