# QEMU KVM Fan Topology

- mode: `mirror`
- bytes: `64m`
- chunk: `1m`
- buffer: `1m`
- qemu_smp: `4`
- result: `0`

```text
topology=client->fan->edge1,edge2
mode=mirror
bytes=64m
chunk=1m
buffer=1m
qemu_smp=4
qemu_mem=768M
kernel=/home/rob/src/linux-7.0.8-zcslots/arch/x86/boot/bzImage
initrd=/home/rob/zcutils/qemu-zcrx/initramfs.cpio
fan_client_socket=30706
fan_edge1_socket=30707
fan_edge2_socket=30708
client_link=client:10.71.0.1/24<->fan.eth0:10.71.0.2/24
edge1_link=fan.eth1:10.71.1.1/24<->edge1:10.71.1.2/24
edge2_link=fan.eth2:10.71.2.1/24<->edge2:10.71.2.2/24
placement=zcraid-split-userspace
terminal_edges=zcsink
```

## Result Lines

```text
[fan-qemu] role=client mode=mirror bytes=64m chunk=1m buffer=1m
[fan-qemu] iface=eth0 addr=10.71.0.1/24
[fan-qemu] client: starting send
zc-tcpmux-send-result: peer=10.71.0.2:41000 lanes=1 local_data_address=10.71.0.1 bytes=67108864 encryption=none already_encrypted=false
[fan-qemu] client: ok
[fan-qemu] recent kernel log
[fan-qemu] role=fan mode=mirror bytes=64m chunk=1m buffer=1m
[fan-qemu] iface=eth0 addr=10.71.0.2/24
[fan-qemu] iface=eth1 addr=10.71.1.1/24
[fan-qemu] iface=eth2 addr=10.71.2.1/24
[fan-qemu] fan: branch1=/uring-play zc-tcpmux-send --peer-address 10.71.1.2 --port 42001 --local-data-address 10.71.1.1 --buffer-bytes 1m --encryption none --disable-authentication
[fan-qemu] fan: branch2=/uring-play zc-tcpmux-send --peer-address 10.71.2.2 --port 42002 --local-data-address 10.71.2.1 --buffer-bytes 1m --encryption none --disable-authentication
[fan-qemu] fan: starting receive/split
zc-tcpmux-receive-result: peer=10.71.0.1:60641 bytes=67108864 encryption=none already_encrypted=false output=-
zc-tcpmux-send-result: peer=10.71.1.2:42001 lanes=1 local_data_address=10.71.1.1 bytes=67113024 encryption=none already_encrypted=false
zc-tcpmux-send-result: peer=zcraid-split-branch-result: branch=cmd:/uring-play zc-tcpmux-send --peer-address 10.71.1.2 --port 42001 --local-data-address 10.71.1.1 --buffer-bytes 1m --encryption none --disable-authentication logical_bytes=67108864 wire_bytes=67113024 frames=65 cpu_seconds=0.018636 voluntary_ctxt_switches=1042 involuntary_ctxt_switches=0 migrations=252
zcraid-split-branch-result: branch=cmd:/uring-play zc-tcpmux-send --peer-address 10.71.2.2 --port 42002 --local-data-address 10.71.2.1 --buffer-bytes 1m --encryption none --disable-authentication logical_bytes=67108864 wire_bytes=67113024 frames=65 cpu_seconds=0.018048 voluntary_ctxt_switches=1056 involuntary_ctxt_switches=3 migrations=336
zcraid-split-result: mode=mirror branches=2 replicas=2 layout=- layout_writes_per_chunk=2 bytes=67108864 chunks=64 branch_logical_bytes=134217728 branch_wire_bytes=134226048 checksum=false io_buffer_bytes=1048576 seconds=2.154534 MiBps=29.70 branch_labels=cmd:/uring-play zc-tcpmux-send --peer-address 10.71.1.2 --port 42001 --local-data-address 10.71.1.1 --buffer-bytes 1m --encryption none --disable-authentication,cmd:/uring-play zc-tcpmux-send --peer-address 10.71.2.2 --port 42002 --local-data-address 10.71.2.1 --buffer-bytes 1m --encryption none --disable-authentication descriptor_mode=auto zero_copy=auto preserve_lanes=no preserve_topology=no topology=- lane_id=- lane_count=- queue_id=- preferred_worker=- lane_map=- preferred_cpu=- numa_node=- ordered=global
[fan-qemu] fan: ok
[fan-qemu] recent kernel log
[fan-qemu] role=edge1 mode=mirror bytes=64m chunk=1m buffer=1m
[fan-qemu] iface=eth0 addr=10.71.1.2/24
[fan-qemu] edge1: starting receive
zc-tcpmux-receive-result: peer=10.71.1.1:42527 bytes=67113024 encryption=none already_encrypted=false output=-
zcsink-result: consume=count bytes=67113024 checksum=0x0000000000000000 descriptor_mode=auto zero_copy=auto preserve_lanes=no preserve_topology=no topology=- lane_id=- lane_count=- queue_id=- preferred_worker=- lane_map=- preferred_cpu=- numa_node=- ordered=global
[fan-qemu] edge1: ok
[fan-qemu] recent kernel log
[fan-qemu] role=edge2 mode=mirror bytes=64m chunk=1m buffer=1m
[fan-qemu] iface=eth0 addr=10.71.2.2/24
[fan-qemu] edge2: starting receive
zc-tcpmux-receive-result: peer=10.71.2.1:58951 bytes=67113024 encryption=none already_encrypted=false output=-
zcsink-result: consume=count bytes=67113024 checksum=0x0000000000000000 descriptor_mode=auto zero_copy=auto preserve_lanes=no preserve_topology=no topology=- lane_id=- lane_count=- queue_id=- preferred_worker=- lane_map=- preferred_cpu=- numa_node=- ordered=global
[fan-qemu] edge2: ok
[fan-qemu] recent kernel log
```
