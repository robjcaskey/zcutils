/* SPDX-License-Identifier: GPL-2.0 WITH Linux-syscall-note */
#ifndef ZCNBLK_SHM_ABI_H
#define ZCNBLK_SHM_ABI_H

#include <linux/ioctl.h>
#include <linux/types.h>

#define ZCNBLK_SHM_MAGIC 0x31304d48534e435aULL /* "ZCNSHM01" */
#define ZCNBLK_SHM_VERSION 3U
#define ZCNBLK_SHM_DESC_BYTES 64U

#define ZCNBLK_SHM_OP_WRITE 1U
#define ZCNBLK_SHM_OP_READ 2U
#define ZCNBLK_SHM_OP_SYNC 7U

#define ZCNBLK_SHM_F_TOPOLOGY_VALID (1U << 0)
#define ZCNBLK_SHM_F_PORT_LANE (1U << 1)

#define ZCNBLK_SHM_CAP_SECTOR_PREDECESSOR (1ULL << 0)
#define ZCNBLK_SHM_CAP_TRANSFER_PAYLOAD_SLOTS (1ULL << 1)
#define ZCNBLK_SHM_CAP_READ_PAYLOAD_REF (1ULL << 2)
#define ZCNBLK_SHM_CAP_REQUEST_WAKE_ARMED (1ULL << 3)
#define ZCNBLK_SHM_CAP_COMPLETION_WAKE_ARMED (1ULL << 4)

#define ZCNBLK_SHM_CQE_F_READ_PAYLOAD_REF (1U << 0)
#define ZCNBLK_SHM_CQE_REF_CHANNEL_SHIFT 8U
#define ZCNBLK_SHM_CQE_REF_CHANNEL_MASK 0xffffff00U

#define ZCNBLK_SHM_ATTACH_F_TRANSFER_PAYLOAD_SLOTS (1U << 0)

/* header.reserved[] assignments for capability extensions. */
#define ZCNBLK_SHM_HEADER_CAPABILITIES 0U
#define ZCNBLK_SHM_HEADER_PAYLOAD_OWNER_OFFSET 1U

/* Reserved while the kernel fills a slot but before it publishes a request. */
#define ZCNBLK_SHM_PAYLOAD_OWNER_RESERVED (~0ULL)

/*
 * One control block has one kernel producer and one userspace consumer.
 * In legacy mode payload_lease_hwm is userspace's exclusive upper bound: every
 * request below it has no remaining WAL/leaf reference. With transferred
 * payload slots, payload_free_slots is the atomic count of free payload pages
 * and each slot has an owner token at header.reserved[PAYLOAD_OWNER_OFFSET]. A
 * nonzero token is the request submit_sequence that owns the page.
 */
struct zcnblk_shm_channel {
	/* Kernel producer, userspace reader. */
	__u64 req_prod;
	__u64 request_publishes;
	__u64 request_kicks;
	__u64 request_producer_reserved[5];

	/* Userspace consumer; armed is exchanged only at the sleep boundary. */
	__u64 req_cons;
	__u64 request_wake_armed;
	__u64 request_consumer_reserved[6];

	/* Userspace producer, kernel reader. */
	__u64 comp_prod;
	__u64 payload_lease_hwm;
	__u64 completion_producer_reserved[6];

	/* Kernel consumer. */
	__u64 comp_cons;
	__u64 completion_kicks;
	__u64 completion_wake_armed;
	__u64 completion_consumer_reserved[5];

	/* Cross-owner atomic used only by transferred-payload mode. */
	__u64 payload_free_slots;
	__u64 payload_reserved[7];
};

struct zcnblk_shm_request {
	__u64 sequence;
	__u64 request_id;
	__u64 offset;
	__u32 len;
	__u16 op;
	__u16 flags;
	__u32 lane;
	__u32 stream;
	__u32 queue_id;
	__u32 payload_slot;
	__u64 submit_sequence;
	/* Previous request touching this hashed 4K sector, or zero. */
	__u64 sector_predecessor;
};

struct zcnblk_shm_completion {
	__u64 sequence;
	__u64 request_id;
	__u64 offset;
	__u64 committed_hwm;
	__u32 len;
	__u32 lane;
	__u32 stream;
	__u32 payload_slot;
	__u16 op;
	__s16 status;
	__u32 flags;
	__u64 request_sequence;
};

struct zcnblk_shm_header {
	__u64 magic;
	__u32 version;
	__u32 header_bytes;
	__u32 channels;
	__u32 ring_entries;
	__u32 payload_entries;
	__u32 slot_bytes;
	__u32 descriptor_bytes;
	__u64 channel_offset;
	__u64 request_offset;
	__u64 completion_offset;
	__u64 payload_offset;
	__u64 region_bytes;
	__u64 capacity_bytes;
	__u64 daemon_generation;
	__u64 daemon_online;
	__u64 global_submit_sequence;
	__u64 reserved[4];
};

struct zcnblk_shm_attach {
	__u64 magic;
	__u32 version;
	__u32 flags;
};

#define ZCNBLK_SHM_IOC_MAGIC 0xbc
#define ZCNBLK_SHM_IOC_ATTACH \
	_IOW(ZCNBLK_SHM_IOC_MAGIC, 1, struct zcnblk_shm_attach)
#define ZCNBLK_SHM_IOC_KICK _IOW(ZCNBLK_SHM_IOC_MAGIC, 2, __u32)
#define ZCNBLK_SHM_IOC_GET_INFO \
	_IOR(ZCNBLK_SHM_IOC_MAGIC, 3, struct zcnblk_shm_header)

#endif /* ZCNBLK_SHM_ABI_H */
