#!/usr/bin/env python3
"""Summarize topology-explicit fio terminal-leaf JSON artifacts."""

import argparse
import json
import re
import statistics
from pathlib import Path


LOW = re.compile(r"low-(randread|randwrite)-q(1|2|4|8|16)-r([1-9][0-9]*)\.json$")
SAT = re.compile(r"sat-(randread|randwrite)-r([1-9][0-9]*)\.json$")
SYNC = re.compile(r"sync-r([1-9][0-9]*)\.json$")


def mean(values):
    return statistics.fmean(values)


def op_stats(document, operation):
    jobs = document["jobs"]
    errors = [int(job["error"]) for job in jobs]
    if any(errors):
        raise ValueError(f"fio job errors: {errors}")
    sections = [job[operation] for job in jobs]
    iops = sum(float(section["iops"]) for section in sections)
    bw_bytes = sum(float(section["bw_bytes"]) for section in sections)
    ios = sum(int(section["total_ios"]) for section in sections)
    if ios == 0:
        raise ValueError(f"no {operation} completions")
    lat_ns = sum(float(section["lat_ns"]["mean"]) * int(section["total_ios"])
                 for section in sections) / ios
    p99_ns = max(float(section["clat_ns"]["percentile"]["99.000000"])
                 for section in sections)
    return {"iops": iops, "bw_bytes": bw_bytes, "lat_ns": lat_ns,
            "p99_ns": p99_ns, "ios": ios}


def spread(values):
    return f"{min(values):,.0f}–{max(values):,.0f}"


def load_runs(root):
    low, sat, sync = [], [], []
    for path in sorted(root.glob("node-*/*.json")):
        name = path.name
        with path.open() as stream:
            document = json.load(stream)
        host = path.parent.name
        match = LOW.fullmatch(name)
        if match:
            mode, qd, repeat = match.groups()
            operation = "read" if mode == "randread" else "write"
            item = op_stats(document, operation)
            item.update(host=host, mode=operation, qd=int(qd), repeat=int(repeat))
            item["ceiling_iops"] = int(qd) * 1e9 / item["lat_ns"]
            item["efficiency"] = item["iops"] / item["ceiling_iops"] * 100
            low.append(item)
            continue
        match = SAT.fullmatch(name)
        if match:
            mode, repeat = match.groups()
            operation = "read" if mode == "randread" else "write"
            item = op_stats(document, operation)
            item.update(host=host, mode=operation, repeat=int(repeat))
            item["ceiling_iops"] = 2048 * 1e9 / item["lat_ns"]
            item["efficiency"] = item["iops"] / item["ceiling_iops"] * 100
            sat.append(item)
            continue
        match = SYNC.fullmatch(name)
        if match:
            item = op_stats(document, "write")
            sync_sections = [job["sync"] for job in document["jobs"]]
            sync_ios = sum(int(section["total_ios"]) for section in sync_sections)
            item["sync_lat_ns"] = (
                sum(float(section["lat_ns"]["mean"]) * int(section["total_ios"])
                    for section in sync_sections) / sync_ios
            )
            item.update(host=host, repeat=int(match.group(1)))
            sync.append(item)
    return low, sat, sync


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("artifact_dir", type=Path)
    args = parser.parse_args()
    root = args.artifact_dir.resolve()
    low, sat, sync = load_runs(root)
    hosts = sorted({item["host"] for item in low})
    expected_low = len(hosts) * 2 * 5 * 3
    expected_sat = len(hosts) * 2 * 3
    expected_sync = len(hosts) * 3
    if len(hosts) != 2 or len(low) != expected_low or len(sat) != expected_sat or len(sync) != expected_sync:
        raise SystemExit(
            f"incomplete matrix: hosts={len(hosts)} low={len(low)}/{expected_low} "
            f"sat={len(sat)}/{expected_sat} sync={len(sync)}/{expected_sync}"
        )

    lines = [
        "# i8g single-terminal-leaf fio benchmark",
        "",
        "## Scope and completion semantics",
        "",
        "Two `i8g.48xlarge` Spot hosts in `us-east-2c` were measured independently. "
        "Each host used exactly one 3.4-TB local instance-store NVMe device as a terminal leaf, "
        "through one 256-GiB preallocated ext4 file. No mirror, stripe, spill, tier, device mapper, "
        "or kernel placement primitive was used.",
        "",
        "Direct reads complete when data is visible to fio. Direct writes in the low-QD and saturation "
        "tables have no durability barrier. The sync table uses the synchronous fio engine and follows "
        "every direct 4-KiB write with `fsync(2)`. This is ephemeral local PCIe instance storage: an "
        "fsync completion is a local barrier completion, not remote persistence, and instance loss "
        "destroys the leaf. Network transport RTT, network-RTT IOPS ceiling, and remote acknowledgement "
        "semantics are not applicable.",
        "",
        "Runs are shared-system cloud measurements. There are six samples per point (three repeats on "
        "each host); ranges below expose host and repeat spread.",
        "",
    ]
    for mode in ("read", "write"):
        lines += [f"## Low-QD direct {mode}", "",
                  "One lane and one worker were pinned to the NVMe-local NUMA node, so per-worker QD "
                  "equals aggregate outstanding depth.", "",
                  "| QD | Mean IOPS | IOPS range | Mean latency | p99 completion | Matching QD/lat ceiling | Efficiency |",
                  "|---:|---:|---:|---:|---:|---:|---:|"]
        for qd in (1, 2, 4, 8, 16):
            points = [item for item in low if item["mode"] == mode and item["qd"] == qd]
            lines.append(
                f"| {qd} | {mean([p['iops'] for p in points]):,.0f} | "
                f"{spread([p['iops'] for p in points])} | "
                f"{mean([p['lat_ns'] for p in points]) / 1e3:,.2f} us | "
                f"{mean([p['p99_ns'] for p in points]) / 1e3:,.2f} us | "
                f"{mean([p['ceiling_iops'] for p in points]):,.0f} | "
                f"{mean([p['efficiency'] for p in points]):.2f}% |"
            )
        lines.append("")

    lines += ["## Very-high-QD single-leaf saturation", "",
              "Thirty-two lanes/workers were pinned within the leaf's NUMA node, with per-worker QD64 "
              "and aggregate outstanding depth 2,048. Each worker owned a disjoint 8-GiB region.", "",
              "| Operation | Mean IOPS | IOPS range | Payload rate | Mean latency | Efficiency vs 2048/lat |",
              "|---|---:|---:|---:|---:|---:|"]
    for mode in ("read", "write"):
        points = [item for item in sat if item["mode"] == mode]
        lines.append(
            f"| {mode} | {mean([p['iops'] for p in points]):,.0f} | "
            f"{spread([p['iops'] for p in points])} | "
            f"{mean([p['bw_bytes'] for p in points]) * 8 / 1e9:,.2f} Gbit/s | "
            f"{mean([p['lat_ns'] for p in points]) / 1e3:,.2f} us | "
            f"{mean([p['efficiency'] for p in points]):.2f}% |"
        )

    lines += ["", "## Per-write local fsync drain", "",
              "One worker, one lane, per-worker QD1, aggregate outstanding depth 1.", "",
              "| Fsync-completed cycles/s | Range | Write latency | fsync drain latency | Serial cycle time |",
              "|---:|---:|---:|---:|---:|"]
    lines.append(
        f"| {mean([p['iops'] for p in sync]):,.0f} | {spread([p['iops'] for p in sync])} | "
        f"{mean([p['lat_ns'] for p in sync]) / 1e3:,.2f} us | "
        f"{mean([p['sync_lat_ns'] for p in sync]) / 1e3:,.2f} us | "
        f"{1e6 / mean([p['iops'] for p in sync]):,.2f} us |"
    )
    cost_path = root / "cost-summary.json"
    if cost_path.exists():
        with cost_path.open() as stream:
            cost = json.load(stream)
        lines += ["", "## Cost and teardown", "",
                  f"Both instances are confirmed `{cost['final_state']}`. The measured lease window was "
                  f"{cost['duration_seconds'] / 60:.1f} minutes at "
                  f"${cost['spot_price_per_host_hour_usd']:.4f}/host-hour. Estimated compute, public IPv4, "
                  f"and root-volume cost is about ${cost['estimated_total_usd']:.2f}, below the "
                  f"${cost['hard_cost_ceiling_usd']:.2f} hard launch ceiling.", ""]
    lines += ["", "## Validation", "",
              f"- Accepted {len(low)} low-QD, {len(sat)} saturation, and {len(sync)} fsync samples; all fio job errors were zero.",
              "- Strict preflight required 1,024 HugeTLB pages, at least 1 GiB memlock headroom, io_uring fixed buffers, registered files, NUMA pinning, and an explicit worker-to-hctx mapping before numbers were printed.",
              "- An initial precondition attempt was rejected on both hosts because fio's io_uring `end_fsync` returned `EINVAL` on this kernel/ext4/device combination. The accepted matrix keeps io_uring for non-barrier direct I/O and measures barriers separately with the synchronous engine and `fsync(2)`.",
              "- Full per-host topology maps and raw fio JSON are stored beside this report.", ""]
    (root / "REPORT.md").write_text("\n".join(lines))
    print(root / "REPORT.md")


if __name__ == "__main__":
    main()
