# EC2 Latency And CPU Profiling Plan

This plan targets the current `c8gn.8xlarge` 8-node deep RAID cluster from
`qemu-zcrx/ec2-c8gn8-deepraid-inventory.json` in `us-east-2a`.

Nodes:

| node | private IP | role in short runs |
| --- | --- | --- |
| n1 | 172.31.1.49 | source or root |
| n2 | 172.31.12.58 | line forwarder or tree intermediate |
| n3 | 172.31.2.55 | line sink or tree intermediate |
| n4 | 172.31.3.213 | tree leaf |
| n5 | 172.31.4.1 | tree leaf |
| n6 | 172.31.4.236 | tree leaf |
| n7 | 172.31.6.136 | tree leaf |
| n8 | 172.31.9.178 | spare for follow-up variants |

Any command that sends cluster traffic must hold `/tmp/cluster.lock`. The
tcpmux and zcnc scripts acquire it internally when `RUN_TRAFFIC=1`. The
DeepRAID tree runner now acquires and releases it directly.

## Metrics

Normalize every path to 4 KiB logical records, even when a tool reports bytes:

```text
4K logical IOPS = bytes / 4096 / seconds
4K logical MIOPS = 4K logical IOPS / 1e6
CPU seconds/GiB = (user_seconds + sys_seconds) / (bytes / 2^30)
CPU seconds/MIOP = CPU seconds / ((bytes / 4096) / 1e6)
```

Use `PROFILE_TIME=1` for aggregate role CPU. It adds one GNU `time` line per
remote role:

```text
cpu-profile-time: role=... elapsed_seconds=... user_seconds=... sys_seconds=...
```

This is lower overhead than sampling profilers and captures whole shell
pipelines such as `zc-tcpmux-receive | zcforward | zc-tcpmux-send`.

Use built-in worker CPU when available:

- `tcp-bench-uring-mux-send-worker`
- `tcp-bench-uring-mux-server-zcrx-worker`
- `tcp-wal-mux-server-zcrx-worker`
- `slot-wal-bench`

Summarize logs with:

```bash
scripts/zc-profile-summarize.py <run-dir>
```

## Short-Run Matrix

### 1. Raw Network Leg Baseline

Purpose: measure raw n1->n2 and n2->n3 transport capacity before tcpmux,
forwarding, WAL, or RAID work is added.

Default short shape:

```bash
CASE=network-zcnc scripts/ec2-latency-cpu-profile.sh
```

Run after acquiring the baton:

```bash
CASE=network-zcnc RUN_TRAFFIC=1 scripts/ec2-latency-cpu-profile.sh
```

Key knobs:

- `CONNECTIONS=8`
- `BYTES_PER_CONNECTION=256m`
- `CHUNK_BYTES=4k`
- `PIPELINE=64`
- `WORKERS=16`

Latency percentiles: not emitted by this streaming tool. Use it for throughput
and CPU efficiency only.

### 2. Encrypted Tcpmux Line

Purpose: measure the current n1 -> n2 -> n3 encrypted forwarding path, including
n2 local decrypt/consume plus forwarded ciphertext.

Dry-run command:

```bash
CASE=tcpmux-line scripts/ec2-latency-cpu-profile.sh
```

Traffic command:

```bash
CASE=tcpmux-line RUN_TRAFFIC=1 scripts/ec2-latency-cpu-profile.sh
```

Key knobs:

- `LANES=8`
- `BYTES_PER_LANE=256m`
- `CHUNK_BYTES=4k`
- `BUFFER_BYTES=4k`
- `QUEUE_DEPTH=64`
- `PROFILE_TIME=1`

Latency percentiles: not emitted today. The useful tail proxy is per-lane
completion spread plus any `zcsink` or `zcforward` straggler lines.

### 3. DeepRAID Tree

Purpose: measure zcraid split/merge fanout and fanin across the tree shapes:

- scatter 1 -> 2 -> 4
- gather 4 -> 2 -> 1
- shallow scatter 1 -> 4

Traffic command:

```bash
CASE=deepraid-tree RUN_TRAFFIC=1 scripts/ec2-latency-cpu-profile.sh
```

Key knobs:

- `DEEPRAID_BYTES=128m`
- `DEEPRAID_CHUNK_BYTES=4k`
- `DEEPRAID_PROFILE_TIME=1`

Latency percentiles: not emitted today. The tree runner gives wall time and
per-role CPU; compare gather versus scatter to identify fan-in stalls.

### 4. Local WAL And Zcbrd Device Path

Purpose: isolate the io-slot WAL path on local `zcbrd` devices without network
traffic.

Dry-run command:

```bash
CASE=zcbrd-local scripts/ec2-latency-cpu-profile.sh
```

Traffic command:

```bash
CASE=zcbrd-local RUN_TRAFFIC=1 scripts/ec2-latency-cpu-profile.sh
```

Key knobs:

- `BYTES_PER_TARGET=256m`
- `CHUNK_BYTES=4k`
- `PIPELINE=128`
- `RING=1024`

Latency percentiles: `slot-wal-bench` does not emit p99/p99.9. For SAN fabric
block-device latency percentiles, use `fio` against `/dev/zcnblk0`. For
backing-media lab checks, use `/dev/zcbrdN`; do not treat backing-media smokes
as SAN target results.

## Tail Latency Runs

Use `fio` only where there is a block-device surface. It is the available tool
that reports p99 and p99.9 without adding custom instrumentation:

```bash
sudo fio \
  --name=zc-4k-randread \
  --filename=/dev/zcnblk0 \
  --rw=randread \
  --bs=4k \
  --iodepth=64 \
  --numjobs=64 \
  --ioengine=io_uring \
  --direct=1 \
  --time_based=1 \
  --runtime=10 \
  --group_reporting=1 \
  --randrepeat=0 \
  --norandommap=1
```

Repeat with `--rw=randwrite` for acknowledged write latency. Record fio `IOPS`,
bandwidth, CPU usr/sys, `clat` p99, and p99.9.

## Run Order

1. Run `network-zcnc` first to establish the transport lower bound.
2. Run `tcpmux-line` with the same 4K shape to measure encryption, forwarding,
   and local branch overhead.
3. Run `deepraid-tree` to expose split/merge fanout and fanin costs.
4. Run `zcbrd-local` or fio to isolate WAL or block-device latency.
5. Summarize each run directory with `scripts/zc-profile-summarize.py`.

Keep each cluster run short, release the lock immediately after the command
finishes, and compare CPU efficiency only between runs with the same byte and
chunk shape.
