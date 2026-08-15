#!/usr/bin/env python3
"""Measure a one-byte TCP_NODELAY echo RTT without throughput batching."""

import argparse
import json
import socket
import statistics
import time


def configured_socket() -> socket.socket:
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    return sock


def run_server(address: str, port: int) -> None:
    listener = configured_socket()
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind((address, port))
    listener.listen(1)
    print(f"TCP_RTT_READY address={address} port={port}", flush=True)
    conn, peer = listener.accept()
    with conn:
        conn.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        while True:
            data = conn.recv(1)
            if not data:
                break
            conn.sendall(data)
    print(f"TCP_RTT_SERVER_PASS peer={peer[0]}:{peer[1]}")


def percentile(values: list[int], fraction: float) -> int:
    return sorted(values)[round((len(values) - 1) * fraction)]


def run_client(address: str, port: int, source: str | None, samples: int, warmup: int) -> None:
    sock = configured_socket()
    if source:
        sock.bind((source, 0))
    sock.connect((address, port))
    for _ in range(warmup):
        sock.sendall(b"x")
        if sock.recv(1) != b"x":
            raise RuntimeError("bad echo during warmup")
    latencies = []
    for _ in range(samples):
        start = time.perf_counter_ns()
        sock.sendall(b"x")
        if sock.recv(1) != b"x":
            raise RuntimeError("bad echo")
        latencies.append(time.perf_counter_ns() - start)
    sock.close()
    result = {
        "samples": samples,
        "warmup": warmup,
        "minimum_ns": min(latencies),
        "median_ns": int(statistics.median(latencies)),
        "p99_ns": percentile(latencies, 0.99),
        "maximum_ns": max(latencies),
        "mean_ns": int(statistics.fmean(latencies)),
        "semantics": "one-byte-tcp-nodelay-echo-rtt",
    }
    print(json.dumps(result, sort_keys=True))


def main() -> None:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="role", required=True)
    server = sub.add_parser("server")
    server.add_argument("address")
    server.add_argument("port", type=int)
    client = sub.add_parser("client")
    client.add_argument("address")
    client.add_argument("port", type=int)
    client.add_argument("--source")
    client.add_argument("--samples", type=int, default=20_000)
    client.add_argument("--warmup", type=int, default=1_000)
    args = parser.parse_args()
    if args.role == "server":
        run_server(args.address, args.port)
    else:
        run_client(args.address, args.port, args.source, args.samples, args.warmup)


if __name__ == "__main__":
    main()
