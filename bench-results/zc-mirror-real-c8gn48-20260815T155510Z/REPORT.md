# Two-host c8gn.48xlarge userspace mirror qualification

Run: `zc-mirror-real-c8gn48-20260815T155510Z`, 2026-08-15, `us-east-2c`, cluster placement group `up-zcutils-adhoc-us-east-2c`.

## Outcome

The real TCP and native EFA `FI_RMA` paths both completed three remote-persistent mirror runs. Every run produced identical branch images, had no process error, and used explicit lane/worker/CPU mappings. Native EFA strict-mode endpoint statistics reported zero hot-path MR registrations after commit `7cb32aae`.

These are durability-path measurements, not network ceilings. The receiver used two userspace RAID leaf writers backed by separate presized files on the same 100 GB gp3 root volume. The nearly equal TCP and EFA rates show that shared EBS journal sync latency is the active bottleneck.

| Transport | Logical 4K records/s, repeats | Mean | Min-max spread | Logical Gbit/s |
|---|---:|---:|---:|---:|
| Native EFA `FI_RMA`, delivery-complete | 10,144; 10,205; 10,140 | 10,163 | 65 (0.64%) | 0.332-0.334 |
| TCP mux-compatible | 10,053; 10,066; 9,860 | 9,993 | 206 (2.06%) | 0.323-0.330 |

Each repeat committed 128 MiB of logical input, 2,048 64 KiB extents, and 32,768 logical 4 KiB records to both branches. EFA branch-wire volume was 256.25 MiB and TCP branch-wire volume was 256.49 MiB. All six SHA-256 branch comparisons matched (`a6d72a...6484`).

## Topology and completion contract

- Hosts: two Spot `c8gn.48xlarge` instances, 192 Neoverse-V2 CPUs and 384 GiB RAM each; advertised 600 Gbit/s networking, one EFA device, no instance NVMe.
- Sender: lanes/workers 0-15 pinned one-to-one to CPUs 16-31.
- Receiver branch 0: lanes/workers 0-15 pinned one-to-one to CPUs 16-31.
- Receiver branch 1: lanes/workers 0-15 pinned one-to-one to CPUs 32-47.
- Userspace mirror placement occurred after the client edge. No block-device mirror, stripe, dm, md, loop, nullblk, ramdisk, or custom block primitive participated. Block-backed files were terminal leaf media only.
- Per-lane/per-worker depth: 64 per branch, 128 across two branches. Aggregate sender outstanding depth: 2,048 (16 lanes x 2 branches x 64). ACK window: 64.
- EFA data completion: initiator delivery CQ with `FI_DELIVERY_COMPLETE`, followed by an `FI_MSG` doorbell. Reported committed completion: both receiver terminal journals synced through the window high-water mark and both ACKs returned.
- TCP completion: both receiver terminal journals synced through the window high-water mark and both ACKs returned.
- Terminal mode: userspace `zcpwal` with presized journal/base files, blocking buffered terminal I/O, persistent `remote-persistent-journal-hwm` sync contract.
- Strict topology enabled. CPU pinning, lane mappings, 512-entry TX/RX CQs, 21,121 free 2 MiB hugepages, unlimited memlock, EFA device-RDMA, zero CQ sleep, IRQ balancing disabled, and NIC interrupt coalescing disabled were explicitly verified/configured.

Pinned ICMP RTT over the private network was 46/53/159 microseconds min/mean/max over 100 packets with no loss. This is not a matching theoretical denominator for the reported result: the measured completion includes two terminal journal syncs, so a network-RTT-only ceiling would be semantically invalid.

## Hardware findings

The first native-EFA attempt correctly failed strict mode before printing a benchmark because stack-backed control messages required `FI_MR_LOCAL`. The control doorbell and ACK storage are now stable preregistered arenas. A second strict preflight exposed that a 64-entry RDMA window required at least 257 TX CQ entries; qualified runs used 512. The final EFA runs show zero nonzero `send_mr_hot`, `recv_mr_hot`, `write_mr_hot`, or `target_mr_hot` counters.

Failed preflight/smoke attempts are retained as diagnostics and excluded from the table. The successful run is a two-host transport qualification, but not a three-fault-domain mirror qualification: both terminal branches are on one receiver host and one gp3 volume.

## Evidence

- `plan.json`: userspace placement and branch/lane/CPU map.
- `topology-node0.log`, `topology-node1.log`: machine/EFA topology preflight.
- `ping-rtt.log`: raw private-network RTT sample.
- `*-send.log`: full sender topology, provider profiles, endpoint counters, per-worker stats, and summaries.
- `*-recv-summary.log` and `receiver-logs/`: branch hashes, receiver summaries, full receiver profiles, and endpoint counters.
- `run-two-hosts.sh`: exact repeat harness.

