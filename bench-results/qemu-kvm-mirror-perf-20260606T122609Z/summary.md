# QEMU KVM Fan Topology

- mode: `mirror`
- bytes: `512m`
- chunk: `1m`
- buffer: `4m`
- qemu_smp: `4`
- result: `0`

```text
topology=client->fan->edge1,edge2
mode=mirror
bytes=512m
chunk=1m
buffer=4m
qemu_smp=4
qemu_mem=1024M
kernel=/home/rob/src/linux-7.0.8-zcslots/arch/x86/boot/bzImage
initrd=/home/rob/zcutils/qemu-zcrx/initramfs.cpio
fan_client_socket=48439
fan_edge1_socket=48440
fan_edge2_socket=48441
client_link=client:10.71.0.1/24<->fan.eth0:10.71.0.2/24
edge1_link=fan.eth1:10.71.1.1/24<->edge1:10.71.1.2/24
edge2_link=fan.eth2:10.71.2.1/24<->edge2:10.71.2.2/24
placement=zcraid-split-userspace
terminal_edges=zcsink
```

## Result Lines

```text
[fan-qemu] role=client mode=mirror bytes=512m chunk=1m buffer=4m
[fan-qemu] iface=eth0 addr=10.71.0.1/24
[fan-qemu] client: starting send
zc-tcpmux-send-result: peer=10.71.0.2:41000 lanes=1 local_data_address=10.71.0.1 bytes=536870912 encryption=none already_encrypted=false
[fan-qemu] client: ok
[fan-qemu] recent kernel log
[fan-qemu] role=fan mode=mirror bytes=512m chunk=1m buffer=4m
[fan-qemu] iface=eth0 addr=10.71.0.2/24
[fan-qemu] iface=eth1 addr=10.71.1.1/24
[fan-qemu] iface=eth2 addr=10.71.2.1/24
[fan-qemu] fan: branch1=/uring-play zc-tcpmux-send --peer-address 10.71.1.2 --port 42001 --local-data-address 10.71.1.1 --buffer-bytes 4m --encryption none --disable-authentication
[fan-qemu] fan: branch2=/uring-play zc-tcpmux-send --peer-address 10.71.2.2 --port 42002 --local-data-address 10.71.2.1 --buffer-bytes 4m --encryption none --disable-authentication
[fan-qemu] fan: starting receive/split
zc-tcpmux-receive-result: peer=10.71.0.1:59405 bytes=536870912 encryption=none already_encrypted=false output=-
zc-tcpmux-send-result: peer=10.71.1.2:42001 lanes=1 local_data_address=10.71.1.1 bytes=536903744 encryption=none already_encrypted=false
zcraid-split-branch-result: branch=cmd:/uring-play zc-tcpmux-send --peer-address 10.71.1.2 --port 42001 --local-data-address 10.71.1.1 --buffer-bytes 4m --encryption none --disable-authentication logical_bytes=536870912 wire_bytes=536903744 frames=513 zero_copy_payload_bytes=0 copy_payload_bytes=536870912 zc_notifications=0 zc_copied_notifications=0 cpu_seconds=0.189367 voluntary_ctxt_switches=8355 involuntary_ctxt_switches=20 migrations=1886
zc-tcpmux-send-result: peer=10.71.2.2:42002 lanes=1 local_data_address=10.71.2.1 bytes=536903744 encryption=none already_encrypted=false
zcraid-split-branch-result: branch=cmd:/uring-play zc-tcpmux-send --peer-address 10.71.2.2 --port 42002 --local-data-address 10.71.2.1 --buffer-bytes 4m --encryption none --disable-authentication logical_bytes=536870912 wire_bytes=536903744 frames=513 zero_copy_payload_bytes=0 copy_payload_bytes=536870912 zc_notifications=0 zc_copied_notifications=0 cpu_seconds=0.198712 voluntary_ctxt_switches=8292 involuntary_ctxt_switches=13 migrations=1822
zcraid-split-result: mode=mirror branches=2 replicas=2 layout=- layout_writes_per_chunk=2 bytes=536870912 chunks=512 branch_logical_bytes=1073741824 branch_wire_bytes=1073807488 branch_zero_copy_payload_bytes=0 branch_copy_payload_bytes=1073741824 branch_zc_notifications=0 branch_zc_copied_notifications=0 checksum=false io_buffer_bytes=4194304 zero_copy_send=auto seconds=2.875190 active_seconds=0.888503 first_chunk_wait_seconds=1.986687 input_seconds=0.876232 eof_enqueue_seconds=0.000004 branch_drain_seconds=0.012268 MiBps=178.08 active_MiBps=576.25 branch_labels=cmd:/uring-play zc-tcpmux-send --peer-address 10.71.1.2 --port 42001 --local-data-address 10.71.1.1 --buffer-bytes 4m --encryption none --disable-authentication,cmd:/uring-play zc-tcpmux-send --peer-address 10.71.2.2 --port 42002 --local-data-address 10.71.2.1 --buffer-bytes 4m --encryption none --disable-authentication descriptor_mode=auto zero_copy=auto preserve_lanes=no preserve_topology=no topology=- lane_id=- lane_count=- queue_id=- preferred_worker=- lane_map=- preferred_cpu=- numa_node=- ordered=global
[fan-qemu] fan: ok
[fan-qemu] recent kernel log
[fan-qemu] role=edge1 mode=mirror bytes=512m chunk=1m buffer=4m
[fan-qemu] iface=eth0 addr=10.71.1.2/24
[fan-qemu] edge1: starting receive
zc-tcpmux-receive-result: peer=10.71.1.1:51793 bytes=536903744 encryption=none already_encrypted=false output=-
zcsink-result: consume=count bytes=536903744 checksum=0x0000000000000000 descriptor_mode=auto zero_copy=auto preserve_lanes=no preserve_topology=no topology=- lane_id=- lane_count=- queue_id=- preferred_worker=- lane_map=- preferred_cpu=- numa_node=- ordered=global
[fan-qemu] edge1: ok
[fan-qemu] recent kernel log
[fan-qemu] role=edge2 mode=mirror bytes=512m chunk=1m buffer=4m
[fan-qemu] iface=eth0 addr=10.71.2.2/24
[fan-qemu] edge2: starting receive
zc-tcpmux-receive-result: peer=10.71.2.1:58803 bytes=536903744 encryption=none already_encrypted=false output=-
zcsink-result: consume=count bytes=536903744 checksum=0x0000000000000000 descriptor_mode=auto zero_copy=auto preserve_lanes=no preserve_topology=no topology=- lane_id=- lane_count=- queue_id=- preferred_worker=- lane_map=- preferred_cpu=- numa_node=- ordered=global
[fan-qemu] edge2: ok
[fan-qemu] recent kernel log
```
