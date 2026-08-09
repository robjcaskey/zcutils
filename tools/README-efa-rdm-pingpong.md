# EFA RDM ping-pong latency probe

`efa_rdm_pingpong.c` is a QD1, one-lane libfabric `FI_EP_RDM` ping-pong probe.
The client measures from posting an EFA send until both its send completion and
the peer's reply are complete. The result therefore has
`remote-receive-and-reply` semantics; half the RTT is only a lower-bound
estimate, not a measured one-way latency.

`--mode write` measures a separate device-RDMA path: `fi_writedata`, remote CQ
notification, and an explicit reply. This is a remote-write acknowledgement,
not an early local write completion, so compare it only with workloads having
matching acknowledgement semantics. The default is `--mode send`.

Build on each EFA host:

```sh
cc -O3 -Wall -Wextra -Werror -o efa-rdm-pingpong tools/efa_rdm_pingpong.c -lfabric
```

Run the server first, pinning both peers to an explicitly selected CPU. Select
the EFA domain explicitly when a host has multiple network cards:

```sh
taskset -c 0 ./efa-rdm-pingpong server --domain efa_0-rdm --cpu 0
taskset -c 0 ./efa-rdm-pingpong client --host SERVER_PRIVATE_IP --domain efa_0-rdm --cpu 0
```

The JSON output records provider/domain, CPU, lane count, per-worker and
aggregate QD, payload, warmup/sample counts, memlock, RTT percentiles, and the
matching sequential QD1 ceiling. Run multiple repetitions on otherwise quiet
hosts and report spread. Validate the selected domain's PCI/NUMA locality with
`ibdev2netdev`, `lspci -vv`, and `/sys/class/infiniband/efa_N/device/numa_node`.
