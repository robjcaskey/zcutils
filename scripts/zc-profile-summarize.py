#!/usr/bin/env python3
"""Summarize zcutils throughput and low-overhead CPU profile logs."""

from __future__ import annotations

import argparse
import math
import re
from pathlib import Path


KV_RE = re.compile(r"([A-Za-z0-9_.:-]+)=([^ \t]+)")
TEXT_SUFFIXES = {
    ".err",
    ".log",
    ".out",
    ".stderr",
    ".stdout",
    ".txt",
}


def parse_kv(line: str) -> dict[str, str]:
    return {key.rstrip(":"): value.rstrip(",") for key, value in KV_RE.findall(line)}


def number(fields: dict[str, str], *keys: str) -> float | None:
    for key in keys:
        value = fields.get(key)
        if value is None:
            continue
        value = value.strip().rstrip("%")
        try:
            parsed = float(value)
        except ValueError:
            continue
        if math.isfinite(parsed):
            return parsed
    return None


def int_number(fields: dict[str, str], *keys: str) -> int | None:
    value = number(fields, *keys)
    if value is None:
        return None
    return int(value)


def text_files(root: Path) -> list[Path]:
    if root.is_file():
        return [root]
    files: list[Path] = []
    for path in root.rglob("*"):
        if not path.is_file():
            continue
        if path.name in {"commands.txt", "metadata.txt"}:
            continue
        if path.suffix in TEXT_SUFFIXES or path.name == "results.log":
            files.append(path)
    return sorted(files)


def event_label(line: str, fields: dict[str, str]) -> str:
    prefix = line.split(None, 1)[0].rstrip(":") if line.split(None, 1) else "event"
    role = fields.get("role")
    worker = fields.get("worker") or fields.get("rxq") or fields.get("writer")
    phase = fields.get("phase")
    topology = fields.get("topology")
    parts = [prefix]
    if role:
        parts.append(role)
    if topology:
        parts.append(topology)
    if phase:
        parts.append(phase)
    if worker is not None:
        parts.append(f"worker={worker}")
    return " ".join(parts)


def bytes_from(fields: dict[str, str]) -> int | None:
    return int_number(
        fields,
        "written_bytes",
        "total_bytes",
        "bytes",
        "received_bytes",
        "submitted_bytes",
    )


def seconds_from(fields: dict[str, str]) -> float | None:
    return number(fields, "seconds", "wall_seconds", "elapsed_seconds")


def cpu_from(fields: dict[str, str]) -> float | None:
    thread_cpu = number(fields, "thread_cpu_seconds", "total_thread_cpu_seconds")
    if thread_cpu is not None:
        return thread_cpu
    user = number(fields, "user_seconds")
    sys = number(fields, "sys_seconds")
    if user is not None or sys is not None:
        return (user or 0.0) + (sys or 0.0)
    return None


def collect(paths: list[Path]) -> list[dict[str, object]]:
    rows: list[dict[str, object]] = []
    throughput_by_file: dict[Path, list[dict[str, object]]] = {}
    bytes_by_file: dict[Path, list[int]] = {}
    cpu_profile_rows: list[tuple[Path, str, dict[str, str]]] = []

    for root in paths:
        for path in text_files(root):
            try:
                lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
            except OSError:
                continue
            for line in lines:
                if "=" not in line:
                    continue
                fields = parse_kv(line)
                if not fields:
                    continue
                label = event_label(line, fields)
                if "cpu-profile-time:" in line:
                    cpu_profile_rows.append((path, label, fields))
                    continue

                bytes_value = bytes_from(fields)
                if bytes_value is not None and any(
                    marker in line
                    for marker in (
                        "tcp-bench-uring-mux",
                        "tcp-wal-mux-server",
                        "slot-wal-bench:",
                        "zcbrd-fanout-fanin-result",
                        "zcraid-split-result:",
                        "zcraid-merge-result:",
                        "zc-tcpmux-send-result:",
                        "zc-tcpmux-receive-result:",
                        "zcforward-result:",
                        "zcsink-result:",
                    )
                ):
                    bytes_by_file.setdefault(path, []).append(bytes_value)
                seconds = seconds_from(fields)
                cpu_seconds = cpu_from(fields)
                if bytes_value is None or seconds is None or seconds <= 0.0:
                    continue
                if not any(
                    marker in line
                    for marker in (
                        "tcp-bench-uring-mux",
                        "tcp-wal-mux-server",
                        "slot-wal-bench:",
                        "zcbrd-fanout-fanin-result",
                        "zcraid-split-result:",
                        "zcraid-merge-result:",
                    )
                ):
                    continue
                row = make_row(path, label, bytes_value, seconds, cpu_seconds)
                rows.append(row)
                throughput_by_file.setdefault(path, []).append(row)

    for path, label, fields in cpu_profile_rows:
        cpu_seconds = cpu_from(fields)
        seconds = seconds_from(fields)
        if cpu_seconds is None or seconds is None or seconds <= 0.0:
            continue
        candidate_rows = throughput_by_file.get(path, [])
        candidate_bytes = [int(row["bytes"]) for row in candidate_rows]
        candidate_bytes.extend(bytes_by_file.get(path, []))
        if not candidate_bytes:
            continue
        bytes_value = max(candidate_bytes)
        rows.append(make_row(path, label, bytes_value, seconds, cpu_seconds))

    return rows


def make_row(
    path: Path,
    label: str,
    bytes_value: int,
    seconds: float,
    cpu_seconds: float | None,
) -> dict[str, object]:
    gib = bytes_value / (1024.0**3)
    logical_ops = bytes_value / 4096.0
    logical_miops = logical_ops / seconds / 1_000_000.0
    gbitps = bytes_value * 8.0 / seconds / 1_000_000_000.0
    cpu_per_gib = None if cpu_seconds is None or gib <= 0.0 else cpu_seconds / gib
    cpu_per_miop = None if cpu_seconds is None or logical_ops <= 0.0 else cpu_seconds / (logical_ops / 1_000_000.0)
    return {
        "file": path,
        "label": label,
        "bytes": bytes_value,
        "gib": gib,
        "seconds": seconds,
        "gbitps": gbitps,
        "logical_miops": logical_miops,
        "cpu_seconds": cpu_seconds,
        "cpu_per_gib": cpu_per_gib,
        "cpu_per_miop": cpu_per_miop,
    }


def fmt(value: object, digits: int = 3) -> str:
    if value is None:
        return "-"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, float):
        return f"{value:.{digits}f}"
    return str(value)


def print_markdown(rows: list[dict[str, object]], limit: int) -> None:
    rows = sorted(rows, key=lambda row: (str(row["file"]), str(row["label"])))
    if limit > 0:
        rows = rows[:limit]
    print("| file | event | GiB | seconds | Gbit/s | 4K MIOPS | CPU s | CPU s/GiB | CPU s/MIOP |")
    print("| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |")
    for row in rows:
        print(
            "| {file} | {label} | {gib} | {seconds} | {gbitps} | {miops} | {cpu} | {cpu_gib} | {cpu_miop} |".format(
                file=row["file"],
                label=str(row["label"]).replace("|", "\\|"),
                gib=fmt(row["gib"]),
                seconds=fmt(row["seconds"], 6),
                gbitps=fmt(row["gbitps"]),
                miops=fmt(row["logical_miops"]),
                cpu=fmt(row["cpu_seconds"], 6),
                cpu_gib=fmt(row["cpu_per_gib"]),
                cpu_miop=fmt(row["cpu_per_miop"]),
            )
        )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("paths", nargs="+", type=Path, help="log files or directories")
    parser.add_argument("--limit", type=int, default=0, help="maximum rows to print; 0 means all")
    args = parser.parse_args()
    print_markdown(collect(args.paths), args.limit)


if __name__ == "__main__":
    main()
