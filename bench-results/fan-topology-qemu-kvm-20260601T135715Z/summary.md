# QEMU KVM Fan Topology

- mode: `stripe`
- bytes: `64m`
- chunk: `1m`
- buffer: `1m`
- qemu_smp: `4`
- result: `0`

```text
topology=client->fan->edge1,edge2
mode=stripe
bytes=64m
chunk=1m
buffer=1m
qemu_smp=4
qemu_mem=768M
kernel=/home/rob/src/linux-7.0.8-zcslots/arch/x86/boot/bzImage
initrd=/home/rob/zcutils/qemu-zcrx/initramfs.cpio
fan_client_socket=49565
fan_edge1_socket=49566
fan_edge2_socket=49567
client_link=client:10.71.0.1/24<->fan.eth0:10.71.0.2/24
edge1_link=fan.eth1:10.71.1.1/24<->edge1:10.71.1.2/24
edge2_link=fan.eth2:10.71.2.1/24<->edge2:10.71.2.2/24
placement=zcraid-split-userspace
terminal_edges=zcsink
```

## Result Lines

```text
[fan-qemu] role=client mode=stripe bytes=64m chunk=1m buffer=1m
[fan-qemu] iface=eth0 addr=10.71.0.1/24
[fan-qemu] client: starting send
zc-tcpmux-send-result: peer=10.71.0.2:41000 lanes=1 local_data_address=10.71.0.1 bytes=67108864 encryption=none already_encrypted=false
[fan-qemu] client: ok
[fan-qemu] recent kernel log
[fan-qemu] role=fan mode=stripe bytes=64m chunk=1m buffer=1m
[fan-qemu] iface=eth0 addr=10.71.0.2/24
[fan-qemu] iface=eth1 addr=10.71.1.1/24
[fan-qemu] iface=eth2 addr=10.71.2.1/24
[fan-qemu] fan: branch1=/uring-play zc-tcpmux-send --peer-address 10.71.1.2 --port 42001 --local-data-address 10.71.1.1 --buffer-bytes 1m --encryption none --disable-authentication
[fan-qemu] fan: branch2=/uring-play zc-tcpmux-send --peer-address 10.71.2.2 --port 42002 --local-data-address 10.71.2.1 --buffer-bytes 1m --encryption none --disable-authentication
[fan-qemu] fan: starting receive/split
zc-tcpmux-receive-result: peer=10.71.0.1:50827 bytes=67108864 encryption=none already_encrypted=false output=-
zc-tcpmux-send-result: peer=10.71.1.2:42001 lanes=1 local_data_address=10.71.1.1 bytes=33556544 encryption=none already_encrypted=false
zcraid-split-branch-result: branch=cmd:/uring-play zc-tcpmux-send --peer-address 10.71.1.2 --port 42001 --local-data-address 10.71.1.1 --buffer-bytes 1m --encryption none --disable-authentication logical_bytes=33554432 wire_bytes=33556544 frames=33 cpu_seconds=0.006496 voluntary_ctxt_switches=541 involuntary_ctxt_switches=0 migrations=183
zc-tcpmux-send-result: peer=10.71.2.2:42002 lanes=1 local_data_address=10.71.2.1 bytes=33556544 encryption=none already_encrypted=false
zcraid-split-branch-result: branch=cmd:/uring-play zc-tcpmux-send --peer-address 10.71.2.2 --port 42002 --local-data-address 10.71.2.1 --buffer-bytes 1m --encryption none --disable-authentication logical_bytes=33554432 wire_bytes=33556544 frames=33 cpu_seconds=0.011131 voluntary_ctxt_switches=526 involuntary_ctxt_switches=1 migrations=103
zcraid-split-result: mode=stripe branches=2 replicas=1 layout=- layout_writes_per_chunk=1 bytes=67108864 chunks=64 branch_logical_bytes=67108864 branch_wire_bytes=67113088 checksum=false io_buffer_bytes=1048576 seconds=2.056047 MiBps=31.13 branch_labels=cmd:/uring-play zc-tcpmux-send --peer-address 10.71.1.2 --port 42001 --local-data-address 10.71.1.1 --buffer-bytes 1m --encryption none --disable-authentication,cmd:/uring-play zc-tcpmux-send --peer-address 10.71.2.2 --port 42002 --local-data-address 10.71.2.1 --buffer-bytes 1m --encryption none --disable-authentication descriptor_mode=auto zero_copy=auto preserve_lanes=no preserve_topology=no topology=- lane_id=- lane_count=- queue_id=- preferred_worker=- lane_map=- preferred_cpu=- numa_node=- ordered=global
[fan-qemu] fan: ok
[fan-qemu] recent kernel log
[fan-qemu] role=edge1 mode=stripe bytes=64m chunk=1m buffer=1m
[fan-qemu] iface=eth0 addr=10.71.1.2/24
[fan-qemu] edge1: starting receive
zc-tcpmux-receive-result: peer=10.71.1.1:55925 bytes=33556544 encryption=none already_encrypted=false output=-
zcsink-result: consume=count bytes=33556544 checksum=0x0000000000000000 descriptor_mode=auto zero_copy=auto preserve_lanes=no preserve_topology=no topology=- lane_id=- lane_count=- queue_id=- preferred_worker=- lane_map=- preferred_cpu=- numa_node=- ordered=global
[fan-qemu] edge1: ok
[fan-qemu] recent kernel log
[fan-qemu] role=edge2 mode=stripe bytes=64m chunk=1m buffer=1m
[fan-qemu] iface=eth0 addr=10.71.2.2/24
[fan-qemu] edge2: starting receive
zc-tcpmux-receive-result: peer=10.71.2.1:53219 bytes=33556544 encryption=none already_encrypted=false output=-
zcsink-result: consume=count bytes=33556544 checksum=0x0000000000000000 descriptor_mode=auto zero_copy=auto preserve_lanes=no preserve_topology=no topology=- lane_id=- lane_count=- queue_id=- preferred_worker=- lane_map=- preferred_cpu=- numa_node=- ordered=global
[fan-qemu] edge2: ok
[fan-qemu] recent kernel log
```
