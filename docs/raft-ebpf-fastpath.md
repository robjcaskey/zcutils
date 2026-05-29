# Raft eBPF Fast Path

`ebpf/zc_raft_fastpath_kern.rs` contains the first kernel-side hot-path piece for
the Raft experiments. It is deliberately a classifier/steering module, not a
Raft implementation:

- TC `classifier` program: parses IPv4 TCP/UDP packets, recognizes the zcutils
  Raft WAL magic (`URFTAE01`, `URFTACK1`) and the raft-zero-copy slotbench magic
  (`RSLTAP01`, `RSLTAC01`), counts traffic, and can write a stable flow shard to
  `skb->mark` and `skb->priority`.
- XDP `xdp` program: performs the same early classification and counters before
  skb allocation. It can optionally redirect by shard through a CPU map when the
  map has been populated.
- `ebpf/zc_raft_bpf_ctl.rs`: minimal syscall-only Rust control tool for policy and
  counter reads. It avoids a hard dependency on `bpftool`.

Build:

```sh
scripts/zc-raft-ebpf build
```

Attach TC to a real interface or a lab `raft0`:

```sh
sudo IFACE=raft0 scripts/zc-raft-ebpf attach-tc
sudo scripts/zc-raft-ebpf policy --ports 19401,9100,9200 --shards 16 --mark --mark-base 5000
sudo scripts/zc-raft-ebpf stats
```

Attach inside the netdevsim lab:

```sh
sudo NETNS=zraft_30055_1 IFACE=raft0 scripts/zc-raft-ebpf attach-tc
```

Attach XDP when the driver supports it:

```sh
sudo IFACE=eth0 scripts/zc-raft-ebpf attach-xdp
```

The default policy watches ports `19401`, `9100`, and `9200`, uses 64 logical
shards, and does not mutate packet marks. Use `--mark` to write `mark_base +
shard` into `skb->mark` and `skb->priority`.

Do not enable `--strict-drop` for normal TCP Raft tests. TCP segmentation means
many valid packets are middle chunks of a larger Raft write and do not start
with a frame magic. Strict drop is useful only for controlled UDP or tiny-frame
smokes where every packet starts with a protocol header.

This does not move WAL commit, quorum state, leader election, or ack generation
into eBPF. Those remain in userspace with io_uring because BPF does not own
stable TCP stream reassembly, block-device writes, or recoverable consensus
state. The useful kernel work here is queue-local classification, steering,
early invalid-packet drops in controlled tests, and cheap counters.
