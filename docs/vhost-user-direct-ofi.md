# Direct OFI storage for QEMU and libvirt

`zcvhost-user-blk --direct-ofi` is the virtual-machine frontend for the
userspace storage path. It does not open `/dev/zcnblk0` or any other host block
device:

```text
guest virtio-blk
  -> QEMU vhost-user shared memory
  -> lane-local registered buffers in zcvhost-user-blk
  -> libfabric RMA
  -> downstream userspace volume stage
```

The frontend translates virtqueue requests but does not make placement,
mirror, stripe, spill, locality, or tiering decisions. Those remain downstream
userspace responsibilities. `zcvhost-ofi-volume` is a volatile registered-memory
terminal used to prove and benchmark the transport; its flush acknowledges an
ordered remote-memory barrier, not persistent media.

## Local QEMU correctness gate

The sockets provider exercises the same framing, RMA queues, registered
buffers, lane HWM barrier, and stock QEMU vhost-user-blk attachment. It uses the
host network stack and is not a zero-syscall or representative performance run:

```sh
QUEUES=1 QUEUE_SIZE=32 GUEST_MODE=smoke \
  scripts/zcvhost-direct-ofi-qemu-smoke.sh
```

The script rejects a backend log containing a zcnblk kernel edge and requires
the guest write/read/flush test to finish with zero backend I/O errors. The
local sockets provider limits the combined OFI queue depth; EFA runs can use
the larger queue sizes for which the endpoint is sized at creation.

## EFA data path

Start the downstream userspace volume stage first, then the vhost-user server:

```sh
zcvhost-ofi-volume \
  --bind 10.0.0.20 --provider efa --endpoint rdm \
  --base-service 37000 --lanes 8 --capacity-bytes 68719476736 \
  --lane-cpus 8,9,10,11,12,13,14,15 --require-hugetlb

URING_PLAY_OFI_EFA_FABRIC=efa-direct \
URING_PLAY_OFI_CQ_SLEEP_NS=0 \
zcvhost-user-blk \
  --socket /run/zcutils/vm0-vhost-blk.sock \
  --direct-ofi 10.0.0.20 --direct-provider efa --direct-endpoint rdm \
  --direct-base-service 37000 --direct-capacity-bytes 68719476736 \
  --direct-slot-bytes 4096 --direct-require-hugetlb \
  --queues 8 --queue-size 512 --queue-cpus 0,1,2,3,4,5,6,7
```

EFA/verbs data operations post to registered userspace NIC queues without a
per-I/O system call. Control-plane connection setup, vhost-user negotiation,
idle waits, and shutdown can still enter the kernel. Sustained I/O stays in the
bounded-poll lane loop. A representative run must also declare NIC/domain,
NUMA placement, vCPU-to-queue mapping, lane-to-worker mapping, HugeTLB and
memlock capacity, and raw transport RTT.

## libvirt attachment

Libvirt 7.1 or newer supports [`vhostuser` disks](https://libvirt.org/formatdomain.html#hard-drives-floppy-disks-cdroms). The VM needs shared memory;
the disk queue count and queue size must match the backend. With the backend
listening at the path above, add the following elements to the domain:

```xml
<memoryBacking>
  <hugepages/>
  <locked/>
  <source type='memfd'/>
  <access mode='shared'/>
</memoryBacking>

<devices>
  <disk type='vhostuser' device='disk' snapshot='no'>
    <driver name='qemu' type='raw' queues='8' queue_size='512'/>
    <source type='unix' path='/run/zcutils/vm0-vhost-blk.sock' mode='client'>
      <reconnect enabled='yes' timeout='10'/>
    </source>
    <target dev='vdb' bus='virtio'/>
  </disk>
</devices>
```

`snapshot='no'` is intentional. Libvirt block jobs, libvirt incremental backup,
and libvirt snapshots do not operate on a vhost-user disk. Snapshot and
migration coordination must be performed by the downstream userspace volume
service, which owns the data and durability policy.

The libvirt/QEMU process must be allowed to access the Unix socket and lock the
configured shared memory. Apply SELinux/AppArmor and service-manager policy to
the exact socket and HugeTLB resources rather than granting general block-device
access.

## Completion semantics

- A remote read completes after its RMA read completion and guest copy.
- A normal write completes after the remotely acknowledged RMA operation.
- A virtio flush snapshots every lane's admitted write HWM, drains each lane to
  that HWM, and waits for the downstream ordered-barrier acknowledgement.
- The volatile test volume acknowledges remote registered memory only. A
  persistent userspace RAID/WAL stage must define the stronger barrier it
  advertises before it can be used for durability results.

Do not compare normal write acknowledgement, flush/FUA drain, and remote-read
latency using one RTT ceiling; they have different completion contracts.
