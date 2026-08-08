# QEMU KVM Fan Topology

- mode: `mirror`
- bytes: `512m`
- chunk: `1m`
- buffer: `4m`
- qemu_smp: `4`
- result: `1`

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
fan_client_socket=36892
fan_edge1_socket=36893
fan_edge2_socket=36894
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
[fan-qemu] client: failed
[fan-qemu] recent kernel log
[fan-qemu] role=fan mode=mirror bytes=512m chunk=1m buffer=4m
[fan-qemu] iface=eth0 addr=10.71.0.2/24
[fan-qemu] iface=eth1 addr=10.71.1.1/24
[fan-qemu] iface=eth2 addr=10.71.2.1/24
[fan-qemu] fan: branch1=/uring-play zc-tcpmux-send --peer-address 10.71.1.2 --port 42001 --local-data-address 10.71.1.1 --buffer-bytes 4m --encryption none --disable-authentication
[fan-qemu] fan: branch2=/uring-play zc-tcpmux-send --peer-address 10.71.2.2 --port 42002 --local-data-address 10.71.2.1 --buffer-bytes 4m --encryption none --disable-authentication
[fan-qemu] fan: starting receive/split
[fan-qemu] fan: failed
[fan-qemu] recent kernel log
[fan-qemu] role=edge1 mode=mirror bytes=512m chunk=1m buffer=4m
[fan-qemu] iface=eth0 addr=10.71.1.2/24
[fan-qemu] edge1: starting receive
[fan-qemu] edge1: failed
[fan-qemu] recent kernel log
[fan-qemu] role=edge2 mode=mirror bytes=512m chunk=1m buffer=4m
[fan-qemu] iface=eth0 addr=10.71.2.2/24
[fan-qemu] edge2: starting receive
[fan-qemu] edge2: failed
[fan-qemu] recent kernel log
```
