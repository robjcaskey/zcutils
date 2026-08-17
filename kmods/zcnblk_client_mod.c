// SPDX-License-Identifier: GPL-2.0

#include <linux/blk-mq.h>
#include <linux/blk_types.h>
#include <linux/blkdev.h>
#include <linux/bvec.h>
#include <linux/atomic.h>
#include <linux/cpu.h>
#include <linux/crypto.h>
#include <linux/debugfs.h>
#include <linux/delay.h>
#include <linux/errno.h>
#include <linux/file.h>
#include <linux/fcntl.h>
#include <linux/highmem.h>
#include <linux/hash.h>
#include <linux/hugetlb.h>
#include <linux/in.h>
#include <linux/inet.h>
#include <linux/jiffies.h>
#include <linux/kernel.h>
#include <linux/kthread.h>
#include <linux/ktime.h>
#include <linux/list.h>
#include <linux/log2.h>
#include <linux/mm.h>
#include <linux/miscdevice.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/net.h>
#include <linux/overflow.h>
#include <linux/poll.h>
#include <linux/random.h>
#include <linux/sched/mm.h>
#include <linux/slab.h>
#include <linux/scatterlist.h>
#include <linux/socket.h>
#include <linux/spinlock.h>
#include <linux/string.h>
#include <linux/topology.h>
#include <linux/uaccess.h>
#include <linux/version.h>
#include <linux/vmalloc.h>
#include <linux/wait.h>
#include <linux/xarray.h>
#include <crypto/aead.h>
#include <crypto/hash.h>
#include <net/sock.h>
#include <net/tcp.h>

#include "zcnblk_shm_abi.h"

#define ZCNBLK_NAME "zcnblk"
#define ZCNBLK_DISK_NAME "zcnblk0"
#define ZCNBLK_FRAME_MAGIC "ZCNBLK01"
#define ZCNBLK_FRAME_VERSION 2
#define ZCNBLK_FRAME_HEADER_LEN 64
#define ZCNBLK_TOPOLOGY_VALID BIT(0)
#define ZCNBLK_TOPOLOGY_PORT_LANE BIT(1)
#define ZCNBLK_OP_WRITE 1
#define ZCNBLK_OP_READ 2
#define ZCNBLK_OP_READ_RESP 3
#define ZCNBLK_OP_WRITE_ACK 4
#define ZCNBLK_OP_BATCH 5
#define ZCNBLK_OP_BATCH_RESP 6
#define ZCNBLK_OP_SYNC 7
#define ZCNBLK_OP_SYNC_ACK 8
#define ZCNBLK_AES256_GCM_KEY_LEN 32
#define ZCNBLK_AES256_GCM_IV_LEN 12
#define ZCNBLK_AES256_GCM_TAG_LEN 16
#define ZCNBLK_AES256_MAGIC "ZCNBAE01"
#define ZCNBLK_AES256_MAGIC_LEN 8
#define ZCNBLK_AES256_HANDSHAKE_LEN \
	(ZCNBLK_AES256_MAGIC_LEN + ZCNBLK_AES256_GCM_IV_LEN * 2)
#define ZCNBLK_AES256_DEFAULT_FRAME_BYTES (64U * 1024U)
#define ZCNBLK_MAX_REMOTE_IPS 256

static char *remote_ip = "127.0.0.1";
module_param(remote_ip, charp, 0444);
MODULE_PARM_DESC(remote_ip, "IPv4 address of zcnblk-target");

static char *transport = "tcp";
module_param(transport, charp, 0444);
MODULE_PARM_DESC(transport, "Client transport: tcp or shm; shm exposes /dev/zcnblk-shmctl to a userspace target");

static char *remote_ips;
module_param(remote_ips, charp, 0444);
MODULE_PARM_DESC(remote_ips, "Comma-separated target IPv4 addresses distributed over contiguous lane ranges; overrides remote_ip");

static ushort remote_port_base = 19600;
module_param(remote_port_base, ushort, 0444);
MODULE_PARM_DESC(remote_port_base, "Base TCP port for zcnblk-target lanes");

static uint lanes = 1;
module_param(lanes, uint, 0444);
MODULE_PARM_DESC(lanes, "Number of target TCP ports/lanes");

static uint connections_per_lane = 1;
module_param(connections_per_lane, uint, 0444);
MODULE_PARM_DESC(connections_per_lane, "TCP connections opened to each target lane");

static uint shard_count = 1;
module_param(shard_count, uint, 0444);
MODULE_PARM_DESC(shard_count, "Must be 1; shard, mirror, stripe, and tier policy lives in userspace targets");

static ulong size_mib = 1024;
module_param(size_mib, ulong, 0444);
MODULE_PARM_DESC(size_mib, "Client block device size in MiB");

static uint logical_block_size = 4096;
module_param(logical_block_size, uint, 0444);
MODULE_PARM_DESC(logical_block_size, "Logical block size");

static uint max_frame_bytes = 393216;
module_param(max_frame_bytes, uint, 0444);
MODULE_PARM_DESC(max_frame_bytes, "Maximum ZCNBLK payload bytes per frame");

static uint queues = 0;
module_param(queues, uint, 0444);
MODULE_PARM_DESC(queues, "blk-mq hardware queues, 0 means lanes");

static uint queue_depth = 128;
module_param(queue_depth, uint, 0444);
MODULE_PARM_DESC(queue_depth, "blk-mq queue depth");

static uint pipeline_depth = 64;
module_param(pipeline_depth, uint, 0444);
MODULE_PARM_DESC(pipeline_depth, "Maximum in-flight requests per TCP connection");

static uint shm_ring_entries;
module_param(shm_ring_entries, uint, 0444);
MODULE_PARM_DESC(shm_ring_entries, "Shared transport slots per connection, 0 means pipeline_depth");

static uint shm_payload_entries;
module_param(shm_payload_entries, uint, 0444);
MODULE_PARM_DESC(shm_payload_entries, "Shared payload lease slots per connection, 0 means shm_ring_entries");

static uint shm_sector_order_slots = 65536;
module_param(shm_sector_order_slots, uint, 0444);
MODULE_PARM_DESC(shm_sector_order_slots, "Power-of-two hashed 4K sector predecessor slots for decentralized userspace ordering");

static uint shm_poll_us = 50;
module_param(shm_poll_us, uint, 0644);
MODULE_PARM_DESC(shm_poll_us, "Shared transport completion busy-poll budget and maximum idle sleep before rechecking work");

static uint shm_completion_batch = 256;
module_param(shm_completion_batch, uint, 0444);
MODULE_PARM_DESC(shm_completion_batch,
		 "Maximum contiguous shared completions consumed under one producer snapshot and consumer release");

static bool shm_ordering_epochs = true;
module_param(shm_ordering_epochs, bool, 0444);
MODULE_PARM_DESC(shm_ordering_epochs,
		 "Stamp flush epochs and publish per-lane admission vectors (required for shm)");

static bool shm_bio_arena_zero_copy;
module_param(shm_bio_arena_zero_copy, bool, 0444);
MODULE_PARM_DESC(shm_bio_arena_zero_copy,
		 "Lease an imported HugeTLB payload slot directly when an O_DIRECT bio already aliases that exact lane-local slot");

static bool shm_bio_arena_zero_copy_required;
module_param(shm_bio_arena_zero_copy_required, bool, 0444);
MODULE_PARM_DESC(shm_bio_arena_zero_copy_required,
		 "Reject malformed imported-arena bios and retry busy exact slots instead of falling back to a payload copy");

static uint fill_timeout_ms;
module_param(fill_timeout_ms, uint, 0444);
MODULE_PARM_DESC(fill_timeout_ms, "Time to wait for more queued requests before receiving a partial pipeline, 0 means reap completions immediately");

static bool worker_batch_dequeue = true;
module_param(worker_batch_dequeue, bool, 0444);
MODULE_PARM_DESC(worker_batch_dequeue,
		 "Splice each connection's pending FIFO into a worker-local batch instead of taking queue_lock per request");

static uint shm_sequence_telemetry_interval = 256;
module_param(shm_sequence_telemetry_interval, uint, 0444);
MODULE_PARM_DESC(shm_sequence_telemetry_interval,
		 "Copy the exact kernel submit sequence into the shared debug header every N requests (zero disables the copy)");

static bool write_acks;
module_param(write_acks, bool, 0444);
MODULE_PARM_DESC(write_acks, "Wait for target write acknowledgements before completing writes");

static bool null_backend;
module_param(null_backend, bool, 0444);
MODULE_PARM_DESC(null_backend, "Benchmark-only: complete reads/writes locally without TCP; flushes fail closed");

static bool null_read_zero = true;
module_param(null_read_zero, bool, 0444);
MODULE_PARM_DESC(null_read_zero, "With null_backend=1, zero-fill reads before completion");

static uint publish_delay_ms;
module_param(publish_delay_ms, uint, 0444);
MODULE_PARM_DESC(publish_delay_ms, "Delay after TCP connect before publishing /dev/zcnblk0");

static uint batch_depth = 1;
module_param(batch_depth, uint, 0444);
MODULE_PARM_DESC(batch_depth, "Maximum same-op requests to pack into one ZCNBLK batch frame");

static uint batch_fill_timeout_us;
module_param(batch_fill_timeout_us, uint, 0444);
MODULE_PARM_DESC(batch_fill_timeout_us, "Microseconds to wait for more queued same-lane requests before sending a partial ZCNBLK batch, 0 means send immediately");

static bool hctx_affinity = true;
module_param(hctx_affinity, bool, 0444);
MODULE_PARM_DESC(hctx_affinity, "Map blk-mq hardware queues directly to target connections when possible");

static int hctx_numa_node = NUMA_NO_NODE;
module_param(hctx_numa_node, int, 0444);
MODULE_PARM_DESC(hctx_numa_node,
		 "Distribute every blk-mq hardware queue across CPUs on this NUMA node; -2 splits queues evenly across NUMA nodes; -1 preserves the kernel default map");

static bool pin_threads;
module_param(pin_threads, bool, 0444);
MODULE_PARM_DESC(pin_threads, "Pin zcnblk connection kthreads to CPUs selected by pin_base_cpu/pin_cpu_count/pin_stride");

static uint pin_base_cpu;
module_param(pin_base_cpu, uint, 0444);
MODULE_PARM_DESC(pin_base_cpu, "Base CPU for pin_threads");

static uint pin_cpu_count;
module_param(pin_cpu_count, uint, 0444);
MODULE_PARM_DESC(pin_cpu_count, "CPU span for pin_threads, 0 means online CPU count");

static uint pin_stride = 1;
module_param(pin_stride, uint, 0444);
MODULE_PARM_DESC(pin_stride, "CPU stride for pin_threads");

static bool shard_affinity;
module_param(shard_affinity, bool, 0444);
MODULE_PARM_DESC(shard_affinity, "Unsupported; shard affinity belongs in the userspace fabric target");

static char *aes256_gcm_token;
module_param(aes256_gcm_token, charp, 0400);
MODULE_PARM_DESC(aes256_gcm_token, "Enable AES-256-GCM using SHA-256 over this token");

static uint aes256_gcm_frame_bytes = ZCNBLK_AES256_DEFAULT_FRAME_BYTES;
module_param(aes256_gcm_frame_bytes, uint, 0444);
MODULE_PARM_DESC(aes256_gcm_frame_bytes, "Maximum plaintext bytes per AES-256-GCM transport frame");

struct zcnblk_dev;

struct zcnblk_pdu {
	struct list_head entry;
	struct request *rq;
	u32 shard;
	u32 len;
	u16 request_id;
	u64 remote_off;
	u64 shm_sequence;
	u64 shm_submit_sequence;
	u64 shm_request_id;
	u64 shm_ordering_epoch;
	u32 shm_payload_slot;
	bool shm_bio_payload_alias;
	enum req_op op;
};

struct zcnblk_frame_header {
	u8 magic[8];
	__le16 version;
	__le16 header_len;
	__le16 op;
	__le16 flags;
	__le32 shard;
	__le32 len;
	__le64 offset;
	__le32 lane_id;
	__le32 lane_count;
	__le32 preferred_worker;
	__le32 queue_id;
	__le64 request_id;
	__le32 tier_id;
	__le32 topology_flags;
} __packed;

struct zcnblk_conn {
	struct socket *sock;
	struct mutex lock;
	spinlock_t queue_lock;
	wait_queue_head_t wait;
	struct task_struct *thread;
	struct list_head pending;
	/* Only the connection worker touches this locklessly. */
	struct list_head worker_pending;
	struct list_head inflight;
	struct zcnblk_pdu **shm_inflight;
	u32 shm_inflight_entries;
	u32 shm_payload_cursor;
	struct zcnblk_dev *dev;
	u32 inflight_count;
	u32 lane;
	u32 stream;
	u32 conn_id;
	u16 next_request_id;
	u16 port;
	u64 shm_admitted_tail;
	struct crypto_aead *tx_aead;
	struct crypto_aead *rx_aead;
	u8 tx_nonce_base[ZCNBLK_AES256_GCM_IV_LEN];
	u8 rx_nonce_base[ZCNBLK_AES256_GCM_IV_LEN];
	u64 tx_seq;
	u64 rx_seq;
	u8 *rx_plaintext;
	u32 rx_plaintext_len;
	u32 rx_offset;
	bool failed;
};

struct zcnblk_shm_state {
	void *region;
	void *fallback_region;
	size_t region_bytes;
	struct zcnblk_shm_header *header;
	struct file *arena_file;
	struct folio **arena_folios;
	unsigned long arena_nr_folios;
	bool external_hugetlb;
	struct mutex arena_lock;
	struct miscdevice misc;
	wait_queue_head_t poll_wait;
	atomic_t daemon_open;
	atomic64_t submit_sequence;
	atomic64_t ordering_epoch;
	spinlock_t ordering_flush_lock;
	atomic64_t *sector_predecessors;
	u32 sector_order_bits;
	bool transfer_payload_slots;
	bool lane_local_sequences;
	bool registered;
	struct xarray arena_page_indices;
	atomic64_t bio_alias_writes;
	atomic64_t bio_alias_reads;
	atomic64_t bio_alias_busy_fallbacks;
	atomic64_t bio_alias_required_retries;
	atomic64_t bio_alias_required_rejects;
};

struct zcnblk_dev {
	struct blk_mq_tag_set tag_set;
	struct gendisk *disk;
	struct zcnblk_conn *conns;
	u64 capacity_bytes;
	atomic64_t next_conn;
	u32 total_conns;
	u32 active_conns;
	int major;
	bool crypto_enabled;
	struct zcnblk_shm_state *shm;
};

static struct zcnblk_dev *zcnblk_dev;
static struct dentry *zcnblk_debugfs_dir;
static __be32 zcnblk_remote_addrs[ZCNBLK_MAX_REMOTE_IPS];
static u32 zcnblk_remote_addr_count;

static_assert(sizeof(struct zcnblk_shm_channel) == 320);
static_assert(sizeof(struct zcnblk_shm_request) == ZCNBLK_SHM_DESC_BYTES);
static_assert(sizeof(struct zcnblk_shm_completion) == ZCNBLK_SHM_DESC_BYTES);
static_assert(sizeof(struct zcnblk_shm_io_contract) ==
	      ZCNBLK_SHM_IO_CONTRACT_BYTES);
static_assert(sizeof(struct zcnblk_shm_arena_import) == 32);

static bool zcnblk_shm_enabled(void)
{
	return transport && !strcmp(transport, "shm");
}

static bool zcnblk_crypto_enabled(const struct zcnblk_dev *dev)
{
	return dev && dev->crypto_enabled;
}

static int zcnblk_validate_token(const char *token)
{
	size_t len;

	if (!token || !*token)
		return 0;
	len = strlen(token);
	if (len > 512)
		return -EINVAL;
	while (*token) {
		if (*token <= ' ')
			return -EINVAL;
		token++;
	}
	return 0;
}

static int zcnblk_derive_aes256_key(const char *token, u32 lane,
				    const char *direction,
				    u8 key[ZCNBLK_AES256_GCM_KEY_LEN])
{
	struct crypto_shash *sha;
	struct shash_desc *desc;
	size_t token_len = strlen(token);
	__be32 lane_be = cpu_to_be32(lane);
	static const u8 context[] = "zc aes-256-gcm lane frame v1";
	static const u8 zcnblk_context[] = "zcnblk";
	static const u8 nul = 0;
	unsigned int desc_len;
	int ret;

	sha = crypto_alloc_shash("sha256", 0, 0);
	if (IS_ERR(sha))
		return PTR_ERR(sha);
	if (crypto_shash_digestsize(sha) != ZCNBLK_AES256_GCM_KEY_LEN) {
		ret = -EINVAL;
		goto out_sha;
	}

	desc_len = sizeof(*desc) + crypto_shash_descsize(sha);
	desc = kzalloc(desc_len, GFP_KERNEL);
	if (!desc) {
		ret = -ENOMEM;
		goto out_sha;
	}
	desc->tfm = sha;

	ret = crypto_shash_init(desc);
	if (ret)
		goto out_desc;
	ret = crypto_shash_update(desc, context, sizeof(context));
	if (ret)
		goto out_desc;
	ret = crypto_shash_update(desc, zcnblk_context, sizeof(zcnblk_context));
	if (ret)
		goto out_desc;
	ret = crypto_shash_update(desc, (const u8 *)direction, strlen(direction));
	if (ret)
		goto out_desc;
	ret = crypto_shash_update(desc, &nul, sizeof(nul));
	if (ret)
		goto out_desc;
	ret = crypto_shash_update(desc, (const u8 *)token, token_len);
	if (ret)
		goto out_desc;
	ret = crypto_shash_update(desc, (const u8 *)&lane_be, sizeof(lane_be));
	if (ret)
		goto out_desc;
	ret = crypto_shash_final(desc, key);

out_desc:
	kfree_sensitive(desc);
out_sha:
	crypto_free_shash(sha);
	return ret;
}

static int zcnblk_alloc_aead_for_key(const u8 key[ZCNBLK_AES256_GCM_KEY_LEN],
				     struct crypto_aead **out)
{
	struct crypto_aead *aead;
	int ret;

	aead = crypto_alloc_aead("gcm(aes)", 0, 0);
	if (IS_ERR(aead))
		return PTR_ERR(aead);
	if (crypto_aead_ivsize(aead) != ZCNBLK_AES256_GCM_IV_LEN) {
		ret = -EINVAL;
		goto out_aead;
	}
	ret = crypto_aead_setkey(aead, key, ZCNBLK_AES256_GCM_KEY_LEN);
	if (ret)
		goto out_aead;
	ret = crypto_aead_setauthsize(aead, ZCNBLK_AES256_GCM_TAG_LEN);
	if (ret)
		goto out_aead;

	*out = aead;
	return 0;

out_aead:
	crypto_free_aead(aead);
	return ret;
}

static int zcnblk_crypto_init(struct zcnblk_dev *dev)
{
	int ret;

	ret = zcnblk_validate_token(aes256_gcm_token);
	if (ret)
		return ret;
	if (aes256_gcm_frame_bytes == 0 ||
	    aes256_gcm_frame_bytes > UINT_MAX - ZCNBLK_AES256_GCM_TAG_LEN)
		return -EINVAL;
	dev->crypto_enabled = aes256_gcm_token && *aes256_gcm_token;
	return 0;
}

static int zcnblk_send_all(struct socket *sock, const void *buf, size_t len);
static void zcnblk_crypto_free_conn(struct zcnblk_conn *conn);

static int zcnblk_crypto_setup_conn(struct zcnblk_conn *conn)
{
	u8 tx_key[ZCNBLK_AES256_GCM_KEY_LEN];
	u8 rx_key[ZCNBLK_AES256_GCM_KEY_LEN];
	u8 handshake[ZCNBLK_AES256_HANDSHAKE_LEN];
	int ret;

	if (!zcnblk_crypto_enabled(conn->dev))
		return 0;

	get_random_bytes(conn->tx_nonce_base, sizeof(conn->tx_nonce_base));
	get_random_bytes(conn->rx_nonce_base, sizeof(conn->rx_nonce_base));

	ret = zcnblk_derive_aes256_key(aes256_gcm_token, conn->lane,
				       "client-to-target", tx_key);
	if (ret)
		return ret;
	ret = zcnblk_derive_aes256_key(aes256_gcm_token, conn->lane,
				       "target-to-client", rx_key);
	if (ret)
		goto out_tx_key;
	ret = zcnblk_alloc_aead_for_key(tx_key, &conn->tx_aead);
	if (ret)
		goto out_keys;
	ret = zcnblk_alloc_aead_for_key(rx_key, &conn->rx_aead);
	if (ret)
		goto out_conn;

	memcpy(handshake, ZCNBLK_AES256_MAGIC, ZCNBLK_AES256_MAGIC_LEN);
	memcpy(handshake + ZCNBLK_AES256_MAGIC_LEN, conn->tx_nonce_base,
	       ZCNBLK_AES256_GCM_IV_LEN);
	memcpy(handshake + ZCNBLK_AES256_MAGIC_LEN + ZCNBLK_AES256_GCM_IV_LEN,
	       conn->rx_nonce_base, ZCNBLK_AES256_GCM_IV_LEN);
	ret = zcnblk_send_all(conn->sock, handshake, sizeof(handshake));
	if (ret)
		goto out_conn;

	memzero_explicit(handshake, sizeof(handshake));
	memzero_explicit(tx_key, sizeof(tx_key));
	memzero_explicit(rx_key, sizeof(rx_key));
	return 0;

out_conn:
	zcnblk_crypto_free_conn(conn);
out_keys:
	memzero_explicit(rx_key, sizeof(rx_key));
out_tx_key:
	memzero_explicit(tx_key, sizeof(tx_key));
	return ret;
}

static void zcnblk_crypto_free_conn(struct zcnblk_conn *conn)
{
	if (!conn)
		return;
	if (conn->tx_aead) {
		crypto_free_aead(conn->tx_aead);
		conn->tx_aead = NULL;
	}
	if (conn->rx_aead) {
		crypto_free_aead(conn->rx_aead);
		conn->rx_aead = NULL;
	}
	kfree_sensitive(conn->rx_plaintext);
	conn->rx_plaintext = NULL;
	conn->rx_plaintext_len = 0;
	conn->rx_offset = 0;
}

static void zcnblk_crypto_free(struct zcnblk_dev *dev)
{
	if (dev)
		dev->crypto_enabled = false;
}

static void zcnblk_crypto_iv(const u8 base[ZCNBLK_AES256_GCM_IV_LEN],
			     u64 seq, u8 iv[ZCNBLK_AES256_GCM_IV_LEN])
{
	__be64 seq_be = cpu_to_be64(seq);
	u8 *seq_bytes = (u8 *)&seq_be;
	size_t i;

	memcpy(iv, base, ZCNBLK_AES256_GCM_IV_LEN);
	for (i = 0; i < sizeof(seq_be); i++)
		iv[4 + i] ^= seq_bytes[i];
}

static void zcnblk_crypto_aad(u64 seq, u32 plaintext_len, u8 aad[12])
{
	__be64 seq_be = cpu_to_be64(seq);
	__be32 len_be = cpu_to_be32(plaintext_len);

	memcpy(aad, &seq_be, sizeof(seq_be));
	memcpy(aad + sizeof(seq_be), &len_be, sizeof(len_be));
}

static int zcnblk_crypto_crypt(struct crypto_aead *aead,
			       const u8 nonce_base[ZCNBLK_AES256_GCM_IV_LEN],
			       u64 seq, bool encrypt,
			       const void *src, u32 payload_len, void *dst)
{
	DECLARE_CRYPTO_WAIT(wait);
	struct scatterlist src_sg[2];
	struct scatterlist dst_sg[2];
	struct aead_request *req;
	unsigned int cryptlen;
	u8 iv[ZCNBLK_AES256_GCM_IV_LEN];
	u8 aad[12];
	int src_nents;
	int dst_nents;
	int ret;

	if (!aead)
		return -EINVAL;
	if (payload_len > UINT_MAX - ZCNBLK_AES256_GCM_TAG_LEN)
		return -EOVERFLOW;
	if ((payload_len && !src) || ((encrypt || payload_len) && !dst))
		return -EINVAL;

	zcnblk_crypto_iv(nonce_base, seq, iv);
	zcnblk_crypto_aad(seq, payload_len, aad);
	cryptlen = encrypt ? payload_len :
			     payload_len + ZCNBLK_AES256_GCM_TAG_LEN;

	src_nents = 1 + (cryptlen ? 1 : 0);
	dst_nents = 1 + ((encrypt ? payload_len + ZCNBLK_AES256_GCM_TAG_LEN :
				    payload_len) ? 1 : 0);
	sg_init_table(src_sg, src_nents);
	sg_set_buf(&src_sg[0], aad, sizeof(aad));
	if (cryptlen)
		sg_set_buf(&src_sg[1], src, cryptlen);

	sg_init_table(dst_sg, dst_nents);
	sg_set_buf(&dst_sg[0], aad, sizeof(aad));
	if (dst_nents > 1)
		sg_set_buf(&dst_sg[1], dst,
			   encrypt ? payload_len + ZCNBLK_AES256_GCM_TAG_LEN :
				     payload_len);

	req = aead_request_alloc(aead, GFP_NOIO);
	if (!req)
		return -ENOMEM;
	aead_request_set_callback(req, CRYPTO_TFM_REQ_MAY_SLEEP |
				       CRYPTO_TFM_REQ_MAY_BACKLOG,
				  crypto_req_done, &wait);
	aead_request_set_crypt(req, src_sg, dst_sg, cryptlen, iv);
	aead_request_set_ad(req, sizeof(aad));

	ret = crypto_wait_req(encrypt ? crypto_aead_encrypt(req) :
					crypto_aead_decrypt(req),
			      &wait);
	aead_request_free(req);
	return ret;
}

static int zcnblk_send_all(struct socket *sock, const void *buf, size_t len)
{
	struct msghdr msg = { .msg_flags = MSG_NOSIGNAL };
	struct kvec iov;
	size_t done = 0;

	while (done < len) {
		int ret;

		iov.iov_base = (void *)buf + done;
		iov.iov_len = len - done;
		ret = kernel_sendmsg(sock, &msg, &iov, 1, iov.iov_len);
		if (ret <= 0)
			return ret < 0 ? ret : -EPIPE;
		done += ret;
	}

	return 0;
}

static int zcnblk_send_iov_all(struct socket *sock, struct kvec *iov,
			       size_t iov_count, size_t len)
{
	struct msghdr msg = { .msg_flags = MSG_NOSIGNAL };
	size_t done = 0;
	size_t idx = 0;

	if (!iov_count && len)
		return -EINVAL;

	while (done < len) {
		size_t consumed;
		int ret;

		ret = kernel_sendmsg(sock, &msg, &iov[idx], iov_count - idx,
				     len - done);
		if (ret <= 0)
			return ret < 0 ? ret : -EPIPE;
		done += ret;
		consumed = ret;

		while (consumed && idx < iov_count) {
			if (consumed >= iov[idx].iov_len) {
				consumed -= iov[idx].iov_len;
				idx++;
			} else {
				iov[idx].iov_base =
					(char *)iov[idx].iov_base + consumed;
				iov[idx].iov_len -= consumed;
				consumed = 0;
			}
		}
	}

	return 0;
}

static int zcnblk_recv_all(struct socket *sock, void *buf, size_t len)
{
	struct msghdr msg = { };
	struct kvec iov;
	size_t done = 0;

	while (done < len) {
		int ret;

		iov.iov_base = buf + done;
		iov.iov_len = len - done;
		ret = kernel_recvmsg(sock, &msg, &iov, 1, iov.iov_len, MSG_WAITALL);
		if (ret <= 0)
			return ret < 0 ? ret : -EPIPE;
		done += ret;
	}

	return 0;
}

static int zcnblk_conn_send_all(struct zcnblk_conn *conn, const void *buf,
				size_t len)
{
	const u8 *cursor = buf;

	while (len) {
		void *wire;
		u32 chunk_len;
		u32 wire_len;
		__be32 len_be;
		int ret;

		if (!conn->tx_aead)
			return zcnblk_send_all(conn->sock, buf, len);

		chunk_len = min_t(size_t, len, aes256_gcm_frame_bytes);
		if (!chunk_len)
			return -EINVAL;
		wire_len = chunk_len + ZCNBLK_AES256_GCM_TAG_LEN;
		wire = kmalloc(wire_len, GFP_NOIO);
		if (!wire)
			return -ENOMEM;
		ret = zcnblk_crypto_crypt(conn->tx_aead, conn->tx_nonce_base,
					  conn->tx_seq, true, cursor, chunk_len,
					  wire);
		if (!ret) {
			len_be = cpu_to_be32(chunk_len);
			ret = zcnblk_send_all(conn->sock, &len_be,
					      sizeof(len_be));
		}
		if (!ret)
			ret = zcnblk_send_all(conn->sock, wire, wire_len);
		kfree_sensitive(wire);
		if (ret)
			return ret;
		conn->tx_seq++;
		cursor += chunk_len;
		len -= chunk_len;
	}

	return 0;
}

static int zcnblk_conn_recv_fill(struct zcnblk_conn *conn)
{
	__be32 len_be;
	u32 plaintext_len;
	u32 wire_len;
	void *wire;
	void *plain;
	int ret;

	if (conn->rx_offset < conn->rx_plaintext_len)
		return 0;
	kfree_sensitive(conn->rx_plaintext);
	conn->rx_plaintext = NULL;
	conn->rx_plaintext_len = 0;
	conn->rx_offset = 0;

	ret = zcnblk_recv_all(conn->sock, &len_be, sizeof(len_be));
	if (ret)
		return ret;
	plaintext_len = be32_to_cpu(len_be);
	if (!plaintext_len || plaintext_len > aes256_gcm_frame_bytes)
		return -EIO;
	if (plaintext_len > UINT_MAX - ZCNBLK_AES256_GCM_TAG_LEN)
		return -EOVERFLOW;

	wire_len = plaintext_len + ZCNBLK_AES256_GCM_TAG_LEN;
	wire = kmalloc(wire_len, GFP_NOIO);
	plain = kmalloc(plaintext_len, GFP_NOIO);
	if (!wire || !plain) {
		kfree_sensitive(wire);
		kfree_sensitive(plain);
		return -ENOMEM;
	}
	ret = zcnblk_recv_all(conn->sock, wire, wire_len);
	if (!ret)
		ret = zcnblk_crypto_crypt(conn->rx_aead, conn->rx_nonce_base,
					  conn->rx_seq, false, wire,
					  plaintext_len, plain);
	kfree_sensitive(wire);
	if (ret) {
		kfree_sensitive(plain);
		return ret;
	}
	conn->rx_seq++;
	conn->rx_plaintext = plain;
	conn->rx_plaintext_len = plaintext_len;
	conn->rx_offset = 0;
	return 0;
}

static int zcnblk_conn_recv_all(struct zcnblk_conn *conn, void *buf, size_t len)
{
	u8 *cursor = buf;

	if (!conn->rx_aead)
		return zcnblk_recv_all(conn->sock, buf, len);

	while (len) {
		size_t available;
		size_t take;
		int ret;

		ret = zcnblk_conn_recv_fill(conn);
		if (ret)
			return ret;
		available = conn->rx_plaintext_len - conn->rx_offset;
		take = min(len, available);
		memcpy(cursor, conn->rx_plaintext + conn->rx_offset, take);
		conn->rx_offset += take;
		cursor += take;
		len -= take;
		if (conn->rx_offset == conn->rx_plaintext_len) {
			kfree_sensitive(conn->rx_plaintext);
			conn->rx_plaintext = NULL;
			conn->rx_plaintext_len = 0;
			conn->rx_offset = 0;
		}
	}

	return 0;
}

static int zcnblk_conn_send_iov_all(struct zcnblk_conn *conn, struct kvec *iov,
				    size_t iov_count, size_t len)
{
	size_t i;
	int ret;

	if (!conn->tx_aead)
		return zcnblk_send_iov_all(conn->sock, iov, iov_count, len);

	for (i = 0; i < iov_count; i++) {
		if (!iov[i].iov_len)
			continue;
		ret = zcnblk_conn_send_all(conn, iov[i].iov_base, iov[i].iov_len);
		if (ret)
			return ret;
	}
	return 0;
}

static int zcnblk_send_frame_payload(struct zcnblk_conn *conn,
				     const struct zcnblk_frame_header *hdr,
				     const void *payload, u32 payload_len)
{
	int ret;

	ret = zcnblk_conn_send_all(conn, hdr, sizeof(*hdr));
	if (ret || !payload_len)
		return ret;

	return zcnblk_conn_send_all(conn, payload, payload_len);
}

static int zcnblk_recv_frame_payload(struct zcnblk_conn *conn,
				     const struct zcnblk_frame_header *hdr,
				     void *payload, u32 payload_len)
{
	if (!payload_len)
		return 0;
	return zcnblk_conn_recv_all(conn, payload, payload_len);
}

static void zcnblk_make_header(struct zcnblk_conn *conn,
			       struct zcnblk_frame_header *hdr, u16 op,
			       u16 flags, u32 shard, u32 len, u64 offset)
{
	memset(hdr, 0, sizeof(*hdr));
	memcpy(hdr->magic, ZCNBLK_FRAME_MAGIC, sizeof(hdr->magic));
	hdr->version = cpu_to_le16(ZCNBLK_FRAME_VERSION);
	hdr->header_len = cpu_to_le16(ZCNBLK_FRAME_HEADER_LEN);
	hdr->op = cpu_to_le16(op);
	hdr->flags = cpu_to_le16(flags);
	hdr->shard = cpu_to_le32(shard);
	hdr->len = cpu_to_le32(len);
	hdr->offset = cpu_to_le64(offset);
	if (conn) {
		hdr->lane_id = cpu_to_le32(conn->lane);
		hdr->lane_count = cpu_to_le32(lanes);
		hdr->preferred_worker = cpu_to_le32(conn->lane);
		hdr->queue_id = cpu_to_le32(conn->lane);
		hdr->request_id = cpu_to_le64(flags);
		hdr->tier_id = cpu_to_le32(shard);
		hdr->topology_flags =
			cpu_to_le32(ZCNBLK_TOPOLOGY_VALID |
				    ZCNBLK_TOPOLOGY_PORT_LANE);
	}
}

static int zcnblk_validate_resp(const struct zcnblk_frame_header *hdr, u16 op,
				u32 shard, u32 len, u64 offset)
{
	if (memcmp(hdr->magic, ZCNBLK_FRAME_MAGIC, sizeof(hdr->magic)))
		return -EIO;
	if (le16_to_cpu(hdr->version) != ZCNBLK_FRAME_VERSION ||
	    le16_to_cpu(hdr->header_len) != ZCNBLK_FRAME_HEADER_LEN ||
	    le16_to_cpu(hdr->op) != op ||
	    le32_to_cpu(hdr->shard) != shard ||
	    le32_to_cpu(hdr->len) != len ||
	    le64_to_cpu(hdr->offset) != offset)
		return -EIO;
	return 0;
}

static int zcnblk_map(u64 logical, u32 *shard, u64 *remote_off)
{
	if (!shard_count)
		return -EINVAL;
	*shard = 0;
	*remote_off = logical;
	return 0;
}

static int zcnblk_copy_rq_to_buf(struct request *rq, size_t rq_off,
				 void *dst, size_t len)
{
	struct req_iterator iter;
	struct bio_vec bvec;
	size_t skipped = 0;
	size_t copied = 0;

	rq_for_each_segment(bvec, rq, iter) {
		size_t seg_len = bvec.bv_len;
		size_t seg_off = 0;
		size_t take;
		void *mapped;

		if (skipped + seg_len <= rq_off) {
			skipped += seg_len;
			continue;
		}
		if (rq_off > skipped) {
			seg_off = rq_off - skipped;
			seg_len -= seg_off;
		}
		take = min(seg_len, len - copied);
		if (!take)
			break;

		mapped = bvec_kmap_local(&bvec);
		memcpy(dst + copied, mapped + seg_off, take);
		kunmap_local(mapped);
		copied += take;
		skipped += bvec.bv_len;
		if (copied == len)
			return 0;
	}

	return -EIO;
}

static int zcnblk_copy_buf_to_rq(struct request *rq, size_t rq_off,
				 const void *src, size_t len)
{
	struct req_iterator iter;
	struct bio_vec bvec;
	size_t skipped = 0;
	size_t copied = 0;

	rq_for_each_segment(bvec, rq, iter) {
		size_t seg_len = bvec.bv_len;
		size_t seg_off = 0;
		size_t take;
		void *mapped;

		if (skipped + seg_len <= rq_off) {
			skipped += seg_len;
			continue;
		}
		if (rq_off > skipped) {
			seg_off = rq_off - skipped;
			seg_len -= seg_off;
		}
		take = min(seg_len, len - copied);
		if (!take)
			break;

		mapped = bvec_kmap_local(&bvec);
		memcpy(mapped + seg_off, src + copied, take);
		flush_dcache_page(bvec.bv_page);
		kunmap_local(mapped);
		copied += take;
		skipped += bvec.bv_len;
		if (copied == len)
			return 0;
	}

	return -EIO;
}

static struct zcnblk_shm_channel *
zcnblk_shm_channel(struct zcnblk_dev *dev, u32 conn_id)
{
	struct zcnblk_shm_header *hdr = dev->shm->header;

	return dev->shm->region + hdr->channel_offset +
		conn_id * sizeof(struct zcnblk_shm_channel);
}

static struct zcnblk_shm_request *
zcnblk_shm_request(struct zcnblk_dev *dev, u32 conn_id, u64 sequence)
{
	struct zcnblk_shm_header *hdr = dev->shm->header;
	u64 index = (u64)conn_id * hdr->ring_entries +
		sequence % hdr->ring_entries;

	return dev->shm->region + hdr->request_offset +
		index * sizeof(struct zcnblk_shm_request);
}

static struct zcnblk_shm_completion *
zcnblk_shm_completion(struct zcnblk_dev *dev, u32 conn_id, u64 sequence)
{
	struct zcnblk_shm_header *hdr = dev->shm->header;
	u64 index = (u64)conn_id * hdr->ring_entries +
		sequence % hdr->ring_entries;

	return dev->shm->region + hdr->completion_offset +
		index * sizeof(struct zcnblk_shm_completion);
}

static struct zcnblk_shm_io_contract *
zcnblk_shm_io_contract(struct zcnblk_dev *dev, u32 conn_id, u64 sequence)
{
	struct zcnblk_shm_header *hdr = dev->shm->header;
	u64 index = (u64)conn_id * hdr->ring_entries +
		sequence % hdr->ring_entries;
	u64 offset = hdr->reserved[ZCNBLK_SHM_HEADER_IO_CONTRACT_OFFSET];

	return dev->shm->region + offset +
		index * sizeof(struct zcnblk_shm_io_contract);
}

static void *zcnblk_shm_payload_slot(struct zcnblk_dev *dev, u32 conn_id,
				    u32 payload_slot)
{
	struct zcnblk_shm_header *hdr = dev->shm->header;
	u64 index = (u64)conn_id * hdr->payload_entries +
		payload_slot;

	return dev->shm->region + hdr->payload_offset + index * hdr->slot_bytes;
}

static u64 *zcnblk_shm_payload_owner(struct zcnblk_dev *dev, u32 conn_id,
				     u32 payload_slot)
{
	struct zcnblk_shm_header *hdr = dev->shm->header;
	u64 index = (u64)conn_id * hdr->payload_entries + payload_slot;
	u64 offset = hdr->reserved[ZCNBLK_SHM_HEADER_PAYLOAD_OWNER_OFFSET];

	return dev->shm->region + offset + index * sizeof(u64);
}

static void zcnblk_shm_release_payload_slot(struct zcnblk_conn *conn,
					    u32 payload_slot, u64 owner,
					    bool return_to_app)
{
	u64 *token;
	u64 released_owner = return_to_app ?
		ZCNBLK_SHM_PAYLOAD_OWNER_APP_RESERVED : 0;
	struct zcnblk_shm_channel *channel;

	if (!conn->dev->shm->transfer_payload_slots)
		return;
	token = zcnblk_shm_payload_owner(conn->dev, conn->conn_id, payload_slot);
	if (cmpxchg(token, owner, released_owner) != owner) {
		pr_err_ratelimited("zcnblk: payload owner release mismatch channel=%u slot=%u owner=%llu actual=%llu\n",
				   conn->conn_id, payload_slot, owner,
				   READ_ONCE(*token));
		return;
	}
	channel = zcnblk_shm_channel(conn->dev, conn->conn_id);
	if (!return_to_app)
		atomic64_inc((atomic64_t *)&channel->payload_free_slots);
}

static int zcnblk_shm_claim_payload_slot(struct zcnblk_conn *conn, u32 *slot)
{
	struct zcnblk_shm_header *hdr = conn->dev->shm->header;
	struct zcnblk_shm_channel *channel =
		zcnblk_shm_channel(conn->dev, conn->conn_id);
	u32 start = conn->shm_payload_cursor;
	u32 i;

	for (i = 0; i < hdr->payload_entries; i++) {
		u32 candidate = (start + i) % hdr->payload_entries;
		u64 *owner = zcnblk_shm_payload_owner(conn->dev, conn->conn_id,
						      candidate);

		if (cmpxchg(owner, 0, ZCNBLK_SHM_PAYLOAD_OWNER_RESERVED))
			continue;
		conn->shm_payload_cursor = (candidate + 1) % hdr->payload_entries;
		atomic64_dec((atomic64_t *)&channel->payload_free_slots);
		*slot = candidate;
		return 0;
	}
	return -EAGAIN;
}

static int zcnblk_shm_claim_specific_payload_slot(struct zcnblk_conn *conn,
						   u32 slot)
{
	struct zcnblk_shm_channel *channel =
		zcnblk_shm_channel(conn->dev, conn->conn_id);
	u64 *owner;

	if (slot >= conn->dev->shm->header->payload_entries)
		return -EINVAL;
	owner = zcnblk_shm_payload_owner(conn->dev, conn->conn_id, slot);
	if (cmpxchg(owner, ZCNBLK_SHM_PAYLOAD_OWNER_APP_RESERVED,
		    ZCNBLK_SHM_PAYLOAD_OWNER_RESERVED) ==
	    ZCNBLK_SHM_PAYLOAD_OWNER_APP_RESERVED)
		return 0;
	if (cmpxchg(owner, 0, ZCNBLK_SHM_PAYLOAD_OWNER_RESERVED))
		return -EAGAIN;
	atomic64_dec((atomic64_t *)&channel->payload_free_slots);
	return 0;
}

static bool zcnblk_shm_rq_matches_region(struct zcnblk_shm_state *shm,
					 struct request *rq, u64 region_offset,
					 size_t length)
{
	struct req_iterator iter;
	struct bio_vec bvec;
	size_t checked = 0;

	rq_for_each_segment(bvec, rq, iter) {
		size_t segment = 0;

		while (segment < bvec.bv_len && checked < length) {
			size_t bvec_offset = bvec.bv_offset + segment;
			struct page *bio_page = bvec.bv_page +
				(bvec_offset >> PAGE_SHIFT);
			void *entry = xa_load(&shm->arena_page_indices,
					      page_to_pfn(bio_page));
			u64 want_page = (region_offset + checked) >> PAGE_SHIFT;
			size_t bio_in_page = bvec_offset & ~PAGE_MASK;
			size_t want_in_page = (region_offset + checked) & ~PAGE_MASK;
			size_t take;

			if (!entry || xa_to_value(entry) != want_page ||
			    bio_in_page != want_in_page)
				return false;
			take = min3(bvec.bv_len - segment, length - checked,
				    PAGE_SIZE - bio_in_page);
			segment += take;
			checked += take;
		}
		if (checked == length)
			return true;
	}
	return false;
}

enum zcnblk_shm_alias_match {
	ZCNBLK_SHM_ALIAS_NONE,
	ZCNBLK_SHM_ALIAS_EXACT,
	ZCNBLK_SHM_ALIAS_MISMATCH,
};

static enum zcnblk_shm_alias_match
zcnblk_shm_rq_payload_alias(struct zcnblk_conn *conn, struct request *rq,
			    u32 length, u32 *payload_slot)
{
	struct zcnblk_shm_state *shm = conn->dev->shm;
	struct zcnblk_shm_header *hdr = shm->header;
	struct req_iterator iter;
	struct bio_vec first;
	u64 region_offset = 0;
	u64 payload_relative;
	u64 global_slot;
	u32 channel;
	bool found = false;

	if ((!shm_bio_arena_zero_copy && !shm_bio_arena_zero_copy_required) ||
	    !shm->external_hugetlb || !shm->transfer_payload_slots)
		return ZCNBLK_SHM_ALIAS_NONE;
	rq_for_each_segment(first, rq, iter) {
		struct page *page = first.bv_page +
			(first.bv_offset >> PAGE_SHIFT);
		void *entry = xa_load(&shm->arena_page_indices,
					      page_to_pfn(page));

		if (!entry)
			return ZCNBLK_SHM_ALIAS_NONE;
		region_offset = (u64)xa_to_value(entry) * PAGE_SIZE +
			(first.bv_offset & ~PAGE_MASK);
		found = true;
		break;
	}
	if (!found)
		return ZCNBLK_SHM_ALIAS_NONE;
	if (!length || length > hdr->slot_bytes ||
	    region_offset < hdr->payload_offset)
		return ZCNBLK_SHM_ALIAS_MISMATCH;
	payload_relative = region_offset - hdr->payload_offset;
	global_slot = payload_relative;
	if (do_div(global_slot, hdr->slot_bytes))
		return ZCNBLK_SHM_ALIAS_MISMATCH;
	channel = div_u64(global_slot, hdr->payload_entries);
	if (channel != conn->conn_id)
		return ZCNBLK_SHM_ALIAS_MISMATCH;
	*payload_slot = global_slot % hdr->payload_entries;
	return zcnblk_shm_rq_matches_region(shm, rq, region_offset, length) ?
		ZCNBLK_SHM_ALIAS_EXACT : ZCNBLK_SHM_ALIAS_MISMATCH;
}

static bool zcnblk_shm_daemon_online(struct zcnblk_dev *dev)
{
	return dev->shm && smp_load_acquire(&dev->shm->header->daemon_online);
}

static bool zcnblk_shm_has_capacity(struct zcnblk_conn *conn)
{
	struct zcnblk_shm_channel *channel =
		zcnblk_shm_channel(conn->dev, conn->conn_id);
	struct zcnblk_shm_header *hdr = conn->dev->shm->header;
	u64 prod = READ_ONCE(channel->req_prod);
	u64 comp = smp_load_acquire(&channel->comp_cons);
	u64 lease = smp_load_acquire(&channel->payload_lease_hwm);
	u64 payload_safe = min(comp, lease);
	u64 req;
	u32 inflight_slot;

	if (conn->dev->shm->transfer_payload_slots) {
		req = smp_load_acquire(&channel->req_cons);
		inflight_slot = prod % conn->shm_inflight_entries;
		return prod - req < hdr->ring_entries &&
			!READ_ONCE(conn->shm_inflight[inflight_slot]);
	}

	if (hdr->payload_entries == hdr->ring_entries)
		return prod - payload_safe < hdr->ring_entries;
	req = smp_load_acquire(&channel->req_cons);

	return prod - req < hdr->ring_entries &&
		prod - payload_safe < hdr->payload_entries;
}

static bool zcnblk_shm_completion_ready(struct zcnblk_conn *conn)
{
	struct zcnblk_shm_channel *channel =
		zcnblk_shm_channel(conn->dev, conn->conn_id);

	return smp_load_acquire(&channel->comp_prod) !=
		READ_ONCE(channel->comp_cons);
}

static int zcnblk_send_rq_payload(struct zcnblk_conn *conn, struct request *rq,
				  size_t rq_off, size_t len)
{
	struct req_iterator iter;
	struct bio_vec bvec;
	size_t skipped = 0;
	size_t sent = 0;

	rq_for_each_segment(bvec, rq, iter) {
		size_t seg_len = bvec.bv_len;
		size_t seg_off = 0;
		size_t take;
		void *mapped;
		int ret;

		if (skipped + seg_len <= rq_off) {
			skipped += seg_len;
			continue;
		}
		if (rq_off > skipped) {
			seg_off = rq_off - skipped;
			seg_len -= seg_off;
		}
		take = min(seg_len, len - sent);
		if (!take)
			break;

		mapped = bvec_kmap_local(&bvec);
		ret = zcnblk_conn_send_all(conn, mapped + seg_off, take);
		kunmap_local(mapped);
		if (ret)
			return ret;
		sent += take;
		skipped += bvec.bv_len;
		if (sent == len)
			return 0;
	}

	return -EIO;
}

static int zcnblk_recv_rq_payload(struct zcnblk_conn *conn, struct request *rq,
				  size_t rq_off, size_t len)
{
	struct req_iterator iter;
	struct bio_vec bvec;
	size_t skipped = 0;
	size_t received = 0;

	rq_for_each_segment(bvec, rq, iter) {
		size_t seg_len = bvec.bv_len;
		size_t seg_off = 0;
		size_t take;
		void *mapped;
		int ret;

		if (skipped + seg_len <= rq_off) {
			skipped += seg_len;
			continue;
		}
		if (rq_off > skipped) {
			seg_off = rq_off - skipped;
			seg_len -= seg_off;
		}
		take = min(seg_len, len - received);
		if (!take)
			break;

		mapped = bvec_kmap_local(&bvec);
		ret = zcnblk_conn_recv_all(conn, mapped + seg_off, take);
		flush_dcache_page(bvec.bv_page);
		kunmap_local(mapped);
		if (ret)
			return ret;
		received += take;
		skipped += bvec.bv_len;
		if (received == len)
			return 0;
	}

	return -EIO;
}

static int zcnblk_do_frame(struct zcnblk_conn *conn, struct request *rq,
			   enum req_op op, size_t rq_off, u64 logical,
			   u32 len)
{
	struct zcnblk_frame_header hdr;
	u32 shard;
	u64 remote_off;
	int ret;

	ret = zcnblk_map(logical, &shard, &remote_off);
	if (ret)
		return ret;

	if (op == REQ_OP_WRITE) {
		zcnblk_make_header(conn, &hdr, ZCNBLK_OP_WRITE, 0, shard, len,
				   remote_off);
		ret = zcnblk_conn_send_all(conn, &hdr, sizeof(hdr));
		if (!ret)
			ret = zcnblk_send_rq_payload(conn, rq, rq_off, len);
		if (!ret && write_acks) {
			ret = zcnblk_conn_recv_all(conn, &hdr, sizeof(hdr));
			if (!ret)
				ret = zcnblk_recv_frame_payload(conn, &hdr, NULL, 0);
			if (!ret)
				ret = zcnblk_validate_resp(&hdr, ZCNBLK_OP_WRITE_ACK,
							   shard, len, remote_off);
		}
		return ret;
	}

	zcnblk_make_header(conn, &hdr, ZCNBLK_OP_READ, 0, shard, len,
			   remote_off);
	ret = zcnblk_send_frame_payload(conn, &hdr, NULL, 0);
	if (ret)
		return ret;
	ret = zcnblk_conn_recv_all(conn, &hdr, sizeof(hdr));
	if (ret)
		return ret;
	ret = zcnblk_validate_resp(&hdr, ZCNBLK_OP_READ_RESP, shard, len,
				   remote_off);
	if (ret)
		return ret;
	return zcnblk_recv_rq_payload(conn, rq, rq_off, len);
}

static int zcnblk_do_sync(struct zcnblk_conn *conn, u16 request_id)
{
	struct zcnblk_frame_header hdr;
	int ret;

	zcnblk_make_header(conn, &hdr, ZCNBLK_OP_SYNC, request_id, 0, 0, 0);
	ret = zcnblk_send_frame_payload(conn, &hdr, NULL, 0);
	if (ret)
		return ret;
	ret = zcnblk_conn_recv_all(conn, &hdr, sizeof(hdr));
	if (ret)
		return ret;
	ret = zcnblk_validate_resp(&hdr, ZCNBLK_OP_SYNC_ACK, 0, 0, 0);
	if (ret)
		return ret;
	return zcnblk_recv_frame_payload(conn, &hdr, NULL, 0);
}

static blk_status_t zcnblk_transfer_request_on_conn(struct zcnblk_dev *dev,
						    struct zcnblk_conn *conn,
						    struct request *rq)
{
	enum req_op op = req_op(rq);
	u64 logical = (u64)blk_rq_pos(rq) << SECTOR_SHIFT;
	u64 bytes = blk_rq_bytes(rq);
	u64 done = 0;
	int ret = 0;

	if (op == REQ_OP_FLUSH)
		return zcnblk_do_sync(conn, 0) ? BLK_STS_IOERR : BLK_STS_OK;
	if (op != REQ_OP_READ && op != REQ_OP_WRITE)
		return BLK_STS_NOTSUPP;
	if (logical > dev->capacity_bytes || bytes > dev->capacity_bytes - logical)
		return BLK_STS_IOERR;

	while (done < bytes) {
		u32 frame_len;

		frame_len = min_t(u64, bytes - done, max_frame_bytes);
		ret = zcnblk_do_frame(conn, rq, op, done, logical + done,
				      frame_len);
		if (ret)
			break;
		done += frame_len;
	}

	return ret ? BLK_STS_IOERR : BLK_STS_OK;
}

static bool zcnblk_request_is_single_frame(struct zcnblk_dev *dev,
					   struct request *rq, u32 *shard,
					   u64 *remote_off, u32 *len)
{
	u64 logical = (u64)blk_rq_pos(rq) << SECTOR_SHIFT;
	u64 bytes = blk_rq_bytes(rq);

	if (req_op(rq) != REQ_OP_READ && req_op(rq) != REQ_OP_WRITE)
		return false;
	if (logical > dev->capacity_bytes || bytes > dev->capacity_bytes - logical)
		return false;
	if (!bytes || bytes > max_frame_bytes || bytes > U32_MAX)
		return false;
	if (zcnblk_map(logical, shard, remote_off))
		return false;
	*len = bytes;
	return true;
}

static struct zcnblk_pdu *zcnblk_pop_pending(struct zcnblk_conn *conn)
{
	struct zcnblk_pdu *pdu = NULL;

	if (worker_batch_dequeue && list_empty(&conn->worker_pending)) {
		/*
		 * Transfer a whole producer batch with one queue-lock round trip.
		 * queue_rq() remains a multi-producer FIFO, while the sole connection
		 * worker drains its private list without bouncing queue_lock for every
		 * 4 KiB request.
		 */
		spin_lock(&conn->queue_lock);
		list_splice_tail_init(&conn->pending, &conn->worker_pending);
		spin_unlock(&conn->queue_lock);
	}
	if (!list_empty(&conn->worker_pending)) {
		pdu = list_first_entry(&conn->worker_pending,
				       struct zcnblk_pdu, entry);
		list_del_init(&pdu->entry);
	} else if (!worker_batch_dequeue) {
		spin_lock(&conn->queue_lock);
		if (!list_empty(&conn->pending)) {
			pdu = list_first_entry(&conn->pending,
					       struct zcnblk_pdu, entry);
			list_del_init(&pdu->entry);
		}
		spin_unlock(&conn->queue_lock);
	}
	return pdu;
}

static bool zcnblk_worker_has_pending(struct zcnblk_conn *conn)
{
	return !list_empty(&conn->worker_pending) ||
		!list_empty_careful(&conn->pending);
}

static u32 zcnblk_pending_count(struct zcnblk_conn *conn, u32 limit)
{
	struct zcnblk_pdu *pdu;
	u32 count = 0;

	spin_lock(&conn->queue_lock);
	list_for_each_entry(pdu, &conn->pending, entry) {
		count++;
		if (count >= limit)
			break;
	}
	spin_unlock(&conn->queue_lock);
	return count;
}

static int zcnblk_debugfs_state_show(struct seq_file *m, void *unused)
{
	struct zcnblk_dev *dev = READ_ONCE(zcnblk_dev);
	u32 conn_id;

	if (!dev) {
		seq_puts(m, "online=0\n");
		return 0;
	}
	seq_printf(m, "online=1 transport=%s total_conns=%u active_conns=%u\n",
		   transport, dev->total_conns, READ_ONCE(dev->active_conns));
	if (!dev->shm) {
		seq_puts(m, "shm=0\n");
		return 0;
	}
	seq_printf(m,
		   "shm=1 daemon_online=%llu daemon_generation=%llu global_submit_sequence=%llu published_submit_sequence=%llu sequence_telemetry_interval=%u completion_batch=%u ring_entries=%u payload_entries=%u transfer_payload_slots=%u arena_backing=%s region_bytes=%zu bio_arena_zero_copy=%u bio_arena_zero_copy_required=%u bio_alias_writes=%lld bio_alias_reads=%lld bio_alias_busy_fallbacks=%lld bio_alias_required_retries=%lld bio_alias_required_rejects=%lld\n",
		   smp_load_acquire(&dev->shm->header->daemon_online),
		   READ_ONCE(dev->shm->header->daemon_generation),
		   atomic64_read(&dev->shm->submit_sequence),
		   READ_ONCE(dev->shm->header->global_submit_sequence),
		   shm_sequence_telemetry_interval,
		   shm_completion_batch,
		   dev->shm->header->ring_entries,
		   dev->shm->header->payload_entries,
		   dev->shm->transfer_payload_slots,
		   dev->shm->external_hugetlb ? "external-hugetlb" : "vmalloc-user",
		   dev->shm->region_bytes, shm_bio_arena_zero_copy,
		   shm_bio_arena_zero_copy_required,
		   atomic64_read(&dev->shm->bio_alias_writes),
		   atomic64_read(&dev->shm->bio_alias_reads),
		   atomic64_read(&dev->shm->bio_alias_busy_fallbacks),
		   atomic64_read(&dev->shm->bio_alias_required_retries),
		   atomic64_read(&dev->shm->bio_alias_required_rejects));

	for (conn_id = 0; conn_id < dev->total_conns; conn_id++) {
		struct zcnblk_conn *conn = &dev->conns[conn_id];
		struct zcnblk_shm_channel *channel =
			zcnblk_shm_channel(dev, conn_id);
		u64 req_prod = smp_load_acquire(&channel->req_prod);
		u64 req_cons = smp_load_acquire(&channel->req_cons);
		u64 comp_prod = smp_load_acquire(&channel->comp_prod);
		u64 comp_cons = smp_load_acquire(&channel->comp_cons);
		struct zcnblk_shm_completion *next_completion =
			zcnblk_shm_completion(dev, conn_id, comp_cons);
		u64 next_completion_sequence =
			smp_load_acquire(&next_completion->sequence);
		u32 next_slot = req_prod % conn->shm_inflight_entries;
		u32 pending = zcnblk_pending_count(conn, UINT_MAX);
		u32 inflight_slots = 0;
		u32 slot;

		for (slot = 0; slot < conn->shm_inflight_entries; slot++)
			inflight_slots += !!READ_ONCE(conn->shm_inflight[slot]);
		seq_printf(m,
			   "conn=%u lane=%u stream=%u failed=%u pending=%u inflight_count=%u inflight_slots=%u req_prod=%llu req_cons=%llu req_used=%llu admitted_tail=%llu flush_tail=%llu flush_epoch=%llu comp_prod=%llu comp_cons=%llu comp_ready=%llu next_comp_sequence=%llu next_req_slot=%u next_req_slot_busy=%u payload_free_slots=%llu payload_lease_hwm=%llu request_wake_armed=%llu completion_wake_armed=%llu request_publishes=%llu request_kicks=%llu completion_kicks=%llu has_capacity=%u\n",
			   conn_id, conn->lane, conn->stream,
			   READ_ONCE(conn->failed), pending,
			   READ_ONCE(conn->inflight_count), inflight_slots,
			   req_prod, req_cons, req_prod - req_cons,
			   READ_ONCE(conn->shm_admitted_tail),
			   READ_ONCE(channel->request_producer_reserved
				     [ZCNBLK_SHM_CHANNEL_FLUSH_TAIL]),
			   smp_load_acquire(&channel->request_producer_reserved
					    [ZCNBLK_SHM_CHANNEL_FLUSH_EPOCH]),
			   comp_prod, comp_cons, comp_prod - comp_cons,
			   next_completion_sequence,
			   next_slot,
			   !!READ_ONCE(conn->shm_inflight[next_slot]),
			   atomic64_read((atomic64_t *)&channel->payload_free_slots),
			   smp_load_acquire(&channel->payload_lease_hwm),
			   smp_load_acquire(&channel->request_wake_armed),
			   smp_load_acquire(&channel->completion_wake_armed),
			   READ_ONCE(channel->request_publishes),
			   READ_ONCE(channel->request_kicks),
			   READ_ONCE(channel->completion_kicks),
			   zcnblk_shm_has_capacity(conn));
	}
	return 0;
}

DEFINE_SHOW_ATTRIBUTE(zcnblk_debugfs_state);

static void zcnblk_wait_for_batch_fill(struct zcnblk_conn *conn, u32 depth)
{
	unsigned int max_us;
	u32 want;

	if (!batch_fill_timeout_us || depth < 2)
		return;

	want = depth - 1;
	if (!list_empty(&conn->worker_pending))
		return;
	if (zcnblk_pending_count(conn, want) >= want)
		return;

	max_us = batch_fill_timeout_us > UINT_MAX / 2 ? UINT_MAX :
						 batch_fill_timeout_us * 2;
	usleep_range(batch_fill_timeout_us, max_us);
}

static void zcnblk_complete_pdu(struct zcnblk_pdu *pdu, blk_status_t status)
{
	struct request *rq = pdu->rq;

	pdu->rq = NULL;
	blk_mq_end_request(rq, status);
}

static blk_status_t zcnblk_null_complete_request(struct zcnblk_dev *dev,
						 struct request *rq)
{
	enum req_op op = req_op(rq);
	u64 logical = (u64)blk_rq_pos(rq) << SECTOR_SHIFT;
	u64 bytes = blk_rq_bytes(rq);

	if (op == REQ_OP_FLUSH)
		return BLK_STS_OK;
	if (op != REQ_OP_READ && op != REQ_OP_WRITE)
		return BLK_STS_NOTSUPP;
	if (logical > dev->capacity_bytes || bytes > dev->capacity_bytes - logical)
		return BLK_STS_IOERR;
	if (op == REQ_OP_READ && null_read_zero) {
		struct req_iterator iter;
		struct bio_vec bvec;

		rq_for_each_segment(bvec, rq, iter) {
			void *mapped = bvec_kmap_local(&bvec);

			memset(mapped, 0, bvec.bv_len);
			flush_dcache_page(bvec.bv_page);
			kunmap_local(mapped);
		}
	}
	return BLK_STS_OK;
}

static int zcnblk_prepare_pdu(struct zcnblk_conn *conn, struct zcnblk_pdu *pdu)
{
	pdu->op = req_op(pdu->rq);
	if (pdu->op == REQ_OP_FLUSH) {
		pdu->shard = 0;
		pdu->remote_off = 0;
		pdu->len = 0;
		pdu->request_id = conn->next_request_id++;
		return 0;
	}
	if (!zcnblk_request_is_single_frame(conn->dev, pdu->rq, &pdu->shard,
					    &pdu->remote_off, &pdu->len))
		return -EOPNOTSUPP;

	pdu->request_id = conn->next_request_id++;
	return 0;
}

static int zcnblk_send_prepared_pdu(struct zcnblk_conn *conn,
				    struct zcnblk_pdu *pdu)
{
	struct zcnblk_frame_header hdr;
	int ret;

	if (pdu->op == REQ_OP_WRITE) {
		zcnblk_make_header(conn, &hdr, ZCNBLK_OP_WRITE,
				   pdu->request_id, pdu->shard, pdu->len,
				   pdu->remote_off);
		ret = zcnblk_conn_send_all(conn, &hdr, sizeof(hdr));
		if (!ret)
			ret = zcnblk_send_rq_payload(conn, pdu->rq, 0, pdu->len);
		return ret;
	}
	if (pdu->op == REQ_OP_FLUSH) {
		zcnblk_make_header(conn, &hdr, ZCNBLK_OP_SYNC,
				   pdu->request_id, 0, 0, 0);
		return zcnblk_send_frame_payload(conn, &hdr, NULL, 0);
	}

	zcnblk_make_header(conn, &hdr, ZCNBLK_OP_READ, pdu->request_id,
			   pdu->shard, pdu->len, pdu->remote_off);
	return zcnblk_send_frame_payload(conn, &hdr, NULL, 0);
}

static int zcnblk_send_pdu(struct zcnblk_conn *conn, struct zcnblk_pdu *pdu)
{
	int ret;

	ret = zcnblk_prepare_pdu(conn, pdu);
	if (ret)
		return ret;
	return zcnblk_send_prepared_pdu(conn, pdu);
}

static void zcnblk_add_inflight_or_complete(struct zcnblk_conn *conn,
					    struct zcnblk_pdu *pdu)
{
	if (pdu->op == REQ_OP_READ || pdu->op == REQ_OP_FLUSH || write_acks) {
		list_add_tail(&pdu->entry, &conn->inflight);
		conn->inflight_count++;
	} else {
		zcnblk_complete_pdu(pdu, BLK_STS_OK);
	}
}

static void zcnblk_push_pending_front(struct zcnblk_conn *conn,
				      struct zcnblk_pdu *pdu)
{
	/* All callers are the sole connection worker. */
	list_add(&pdu->entry, &conn->worker_pending);
}

static int zcnblk_send_batch(struct zcnblk_conn *conn, struct zcnblk_pdu *first)
{
	struct zcnblk_frame_header outer;
	struct zcnblk_frame_header *hdrs;
	struct zcnblk_pdu **pdus;
	struct kvec *iov;
	u32 available;
	u32 depth;
	u32 count = 0;
	enum req_op batch_op;
	size_t iov_count;
	size_t total_len;
	int ret;
	u32 i;

	available = pipeline_depth - conn->inflight_count;
	depth = min(batch_depth, available);
	if (depth < 2) {
		ret = zcnblk_prepare_pdu(conn, first);
		if (ret == -EOPNOTSUPP)
			return ret;
		if (ret) {
			zcnblk_complete_pdu(first, BLK_STS_IOERR);
			return ret;
		}
		ret = zcnblk_send_prepared_pdu(conn, first);
		if (ret) {
			zcnblk_complete_pdu(first, BLK_STS_IOERR);
			return ret;
		}
		zcnblk_add_inflight_or_complete(conn, first);
		return 0;
	}
	batch_op = req_op(first->rq);
	if (batch_op == REQ_OP_FLUSH) {
		ret = zcnblk_prepare_pdu(conn, first);
		if (ret) {
			zcnblk_complete_pdu(first, BLK_STS_IOERR);
			return ret;
		}
		ret = zcnblk_send_prepared_pdu(conn, first);
		if (ret) {
			zcnblk_complete_pdu(first, BLK_STS_IOERR);
			return ret;
		}
		zcnblk_add_inflight_or_complete(conn, first);
		return 0;
	}
	zcnblk_wait_for_batch_fill(conn, depth);

	pdus = kcalloc(depth, sizeof(*pdus), GFP_NOIO);
	hdrs = kcalloc(depth, sizeof(*hdrs), GFP_NOIO);
	iov = kcalloc(2, sizeof(*iov), GFP_NOIO);
	if (!pdus || !hdrs || !iov) {
		kfree(iov);
		kfree(hdrs);
		kfree(pdus);
		zcnblk_complete_pdu(first, BLK_STS_IOERR);
		return -ENOMEM;
	}

	ret = zcnblk_prepare_pdu(conn, first);
	if (ret) {
		kfree(iov);
		kfree(hdrs);
		kfree(pdus);
		if (ret == -EOPNOTSUPP)
			return ret;
		zcnblk_complete_pdu(first, BLK_STS_IOERR);
		return ret;
	}
	pdus[count++] = first;

	while (count < depth) {
		struct zcnblk_pdu *pdu = zcnblk_pop_pending(conn);

		if (!pdu)
			break;
		if (req_op(pdu->rq) != batch_op) {
			zcnblk_push_pending_front(conn, pdu);
			break;
		}
		ret = zcnblk_prepare_pdu(conn, pdu);
		if (ret) {
			if (ret == -EOPNOTSUPP || ret == -ENOMEM) {
				zcnblk_push_pending_front(conn, pdu);
			} else {
				zcnblk_complete_pdu(pdu, BLK_STS_IOERR);
			}
			break;
		}
		pdus[count++] = pdu;
	}

	if (count == 1) {
		ret = zcnblk_send_prepared_pdu(conn, first);
		if (ret) {
			zcnblk_complete_pdu(first, BLK_STS_IOERR);
			goto out;
		}
		zcnblk_add_inflight_or_complete(conn, first);
		goto out;
	}

	zcnblk_make_header(conn, &outer, ZCNBLK_OP_BATCH, 0, 0, count, 0);
	for (i = 0; i < count; i++) {
		u16 wire_op = pdus[i]->op == REQ_OP_WRITE ? ZCNBLK_OP_WRITE :
							   ZCNBLK_OP_READ;

		zcnblk_make_header(conn, &hdrs[i], wire_op,
				   pdus[i]->request_id, pdus[i]->shard,
				   pdus[i]->len, pdus[i]->remote_off);
	}

	iov_count = 0;
	total_len = sizeof(outer) + count * sizeof(*hdrs);
	iov[iov_count].iov_base = &outer;
	iov[iov_count++].iov_len = sizeof(outer);
	iov[iov_count].iov_base = hdrs;
	iov[iov_count++].iov_len = count * sizeof(*hdrs);

	ret = zcnblk_conn_send_iov_all(conn, iov, iov_count, total_len);
	if (!ret && batch_op == REQ_OP_WRITE) {
		for (i = 0; i < count; i++) {
			ret = zcnblk_send_rq_payload(conn, pdus[i]->rq, 0,
						     pdus[i]->len);
			if (ret)
				break;
		}
	}
	if (ret) {
		for (i = 0; i < count; i++)
			zcnblk_complete_pdu(pdus[i], BLK_STS_IOERR);
		goto out;
	}

	for (i = 0; i < count; i++)
		zcnblk_add_inflight_or_complete(conn, pdus[i]);

out:
	kfree(iov);
	kfree(hdrs);
	kfree(pdus);
	return ret;
}

static struct zcnblk_pdu *zcnblk_find_inflight(struct zcnblk_conn *conn,
					       u16 response_op,
					       const struct zcnblk_frame_header *hdr)
{
	struct zcnblk_pdu *pdu;
	enum req_op want_op;
	u16 request_id = le16_to_cpu(hdr->flags);

	if (response_op == ZCNBLK_OP_READ_RESP)
		want_op = REQ_OP_READ;
	else if (response_op == ZCNBLK_OP_SYNC_ACK)
		want_op = REQ_OP_FLUSH;
	else
		want_op = REQ_OP_WRITE;

	list_for_each_entry(pdu, &conn->inflight, entry) {
		if (pdu->op == want_op && pdu->request_id == request_id)
			return pdu;
	}
	return NULL;
}

static int zcnblk_validate_response_header(struct zcnblk_conn *conn,
					   const struct zcnblk_frame_header *hdr,
					   u16 *response_op)
{
	if (memcmp(hdr->magic, ZCNBLK_FRAME_MAGIC, sizeof(hdr->magic)) ||
	    le16_to_cpu(hdr->version) != ZCNBLK_FRAME_VERSION ||
	    le16_to_cpu(hdr->header_len) != ZCNBLK_FRAME_HEADER_LEN)
		return -EIO;

	*response_op = le16_to_cpu(hdr->op);
	if (*response_op != ZCNBLK_OP_READ_RESP &&
	    *response_op != ZCNBLK_OP_WRITE_ACK &&
	    *response_op != ZCNBLK_OP_SYNC_ACK) {
		pr_err_ratelimited("zcnblk: lane=%u stream=%u invalid response op=%u flags=%u shard=%u len=%u offset=%llu\n",
				   conn->lane, conn->stream, *response_op,
				   le16_to_cpu(hdr->flags), le32_to_cpu(hdr->shard),
				   le32_to_cpu(hdr->len), le64_to_cpu(hdr->offset));
		return -EIO;
	}
	return 0;
}

static int zcnblk_complete_response_header(struct zcnblk_conn *conn,
					   const struct zcnblk_frame_header *hdr)
{
	struct zcnblk_pdu *pdu;
	u16 response_op;
	int ret;

	ret = zcnblk_validate_response_header(conn, hdr, &response_op);
	if (ret)
		return ret;

	pdu = zcnblk_find_inflight(conn, response_op, hdr);
	if (!pdu) {
		pr_err_ratelimited("zcnblk: lane=%u stream=%u no inflight match op=%u flags=%u shard=%u len=%u offset=%llu inflight=%u\n",
				   conn->lane, conn->stream, response_op,
				   le16_to_cpu(hdr->flags), le32_to_cpu(hdr->shard),
				   le32_to_cpu(hdr->len), le64_to_cpu(hdr->offset),
				   conn->inflight_count);
		return -EIO;
	}
	if (pdu->shard != le32_to_cpu(hdr->shard) ||
	    pdu->len != le32_to_cpu(hdr->len) ||
	    pdu->remote_off != le64_to_cpu(hdr->offset)) {
		pr_err_ratelimited("zcnblk: lane=%u stream=%u response tag matched but location differed flags=%u got=%u/%u/%llu want=%u/%u/%llu\n",
				   conn->lane, conn->stream, le16_to_cpu(hdr->flags),
				   le32_to_cpu(hdr->shard), le32_to_cpu(hdr->len),
				   le64_to_cpu(hdr->offset), pdu->shard, pdu->len,
				   pdu->remote_off);
		return -EIO;
	}

	list_del_init(&pdu->entry);
	conn->inflight_count--;

	if (response_op == ZCNBLK_OP_READ_RESP) {
		ret = zcnblk_recv_rq_payload(conn, pdu->rq, 0, pdu->len);
		zcnblk_complete_pdu(pdu, ret ? BLK_STS_IOERR : BLK_STS_OK);
	} else {
		ret = zcnblk_recv_frame_payload(conn, hdr, NULL, 0);
		if (ret) {
			zcnblk_complete_pdu(pdu, BLK_STS_IOERR);
			return ret;
		}
		zcnblk_complete_pdu(pdu, BLK_STS_OK);
	}

	return 0;
}

static int zcnblk_recv_batch_completion(struct zcnblk_conn *conn,
					const struct zcnblk_frame_header *outer)
{
	struct zcnblk_frame_header *hdrs;
	u32 count = le32_to_cpu(outer->len);
	u64 want_payload = le64_to_cpu(outer->offset);
	u64 payload = 0;
	u32 i;
	int ret;

	if (!count || count > pipeline_depth || count > U16_MAX)
		return -EIO;
	hdrs = kcalloc(count, sizeof(*hdrs), GFP_NOIO);
	if (!hdrs)
		return -ENOMEM;

	ret = zcnblk_conn_recv_all(conn, hdrs, count * sizeof(*hdrs));
	if (ret)
		goto out;

	for (i = 0; i < count; i++) {
		u16 response_op;

		ret = zcnblk_validate_response_header(conn, &hdrs[i], &response_op);
		if (ret)
			goto out;
		if (response_op == ZCNBLK_OP_READ_RESP)
			payload += le32_to_cpu(hdrs[i].len);
	}
	if (want_payload && want_payload != payload) {
		ret = -EIO;
		goto out;
	}
	for (i = 0; i < count; i++) {
		ret = zcnblk_complete_response_header(conn, &hdrs[i]);
		if (ret)
			goto out;
	}

out:
	kfree(hdrs);
	return ret;
}

static int zcnblk_recv_completion(struct zcnblk_conn *conn)
{
	struct zcnblk_frame_header hdr;
	u16 response_op;
	int ret;

	ret = zcnblk_conn_recv_all(conn, &hdr, sizeof(hdr));
	if (ret)
		return ret;
	if (memcmp(hdr.magic, ZCNBLK_FRAME_MAGIC, sizeof(hdr.magic)) ||
	    le16_to_cpu(hdr.version) != ZCNBLK_FRAME_VERSION ||
	    le16_to_cpu(hdr.header_len) != ZCNBLK_FRAME_HEADER_LEN)
		return -EIO;

	response_op = le16_to_cpu(hdr.op);
	if (response_op == ZCNBLK_OP_BATCH_RESP) {
		return zcnblk_recv_batch_completion(conn, &hdr);
	}
	return zcnblk_complete_response_header(conn, &hdr);
}

static void zcnblk_fail_list(struct list_head *head)
{
	struct zcnblk_pdu *pdu;
	struct zcnblk_pdu *tmp;

	list_for_each_entry_safe(pdu, tmp, head, entry) {
		list_del_init(&pdu->entry);
		zcnblk_complete_pdu(pdu, BLK_STS_IOERR);
	}
}

static int zcnblk_shm_submit_pdu(struct zcnblk_conn *conn,
				 struct zcnblk_pdu *pdu)
{
	struct zcnblk_dev *dev = conn->dev;
	struct zcnblk_shm_channel *channel;
	struct zcnblk_shm_request *desc;
	struct zcnblk_shm_io_contract *io_contract;
	void *payload;
	u64 sequence;
	u32 inflight_slot;
	u32 payload_slot;
	u32 alias_slot = 0;
	u64 submit_sequence;
	u16 wire_op;
	enum zcnblk_shm_alias_match alias_match;
	bool payload_alias;
	int ret;

	if (!zcnblk_shm_daemon_online(dev) || !zcnblk_shm_has_capacity(conn))
		return -EAGAIN;

	ret = zcnblk_prepare_pdu(conn, pdu);
	if (ret)
		return ret;

	channel = zcnblk_shm_channel(dev, conn->conn_id);
	sequence = READ_ONCE(channel->req_prod);
	inflight_slot = sequence % conn->shm_inflight_entries;
	if (WARN_ON_ONCE(conn->shm_inflight[inflight_slot]))
		return -EOVERFLOW;
	desc = zcnblk_shm_request(dev, conn->conn_id, sequence);
	alias_match = zcnblk_shm_rq_payload_alias(conn, pdu->rq, pdu->len,
						 &alias_slot);
	payload_alias = alias_match == ZCNBLK_SHM_ALIAS_EXACT;
	if (alias_match == ZCNBLK_SHM_ALIAS_MISMATCH &&
	    shm_bio_arena_zero_copy_required) {
		atomic64_inc(&dev->shm->bio_alias_required_rejects);
		return -EOPNOTSUPP;
	}
	if (dev->shm->transfer_payload_slots) {
		ret = payload_alias ?
			zcnblk_shm_claim_specific_payload_slot(conn, alias_slot) :
			zcnblk_shm_claim_payload_slot(conn, &payload_slot);
		if (payload_alias && !ret) {
			payload_slot = alias_slot;
		} else if (payload_alias && ret == -EAGAIN) {
			if (shm_bio_arena_zero_copy_required) {
				atomic64_inc(&dev->shm->bio_alias_required_retries);
				return -EAGAIN;
			}
			atomic64_inc(&dev->shm->bio_alias_busy_fallbacks);
			payload_alias = false;
			ret = zcnblk_shm_claim_payload_slot(conn, &payload_slot);
		}
		if (ret)
			return ret;
	} else {
		payload_slot = sequence % dev->shm->header->payload_entries;
	}
	payload = zcnblk_shm_payload_slot(dev, conn->conn_id, payload_slot);
	if (pdu->op == REQ_OP_WRITE) {
		wire_op = ZCNBLK_SHM_OP_WRITE;
		ret = payload_alias ? 0 :
			zcnblk_copy_rq_to_buf(pdu->rq, 0, payload, pdu->len);
		if (ret)
			goto out_release_slot;
	} else if (pdu->op == REQ_OP_READ) {
		wire_op = ZCNBLK_SHM_OP_READ;
	} else if (pdu->op == REQ_OP_FLUSH) {
		wire_op = ZCNBLK_SHM_OP_SYNC;
	} else {
		ret = -EOPNOTSUPP;
		goto out_release_slot;
	}
	/*
	 * Allocate the global sequence only when this descriptor can be
	 * published. Admission-time allocation leaves invisible holes behind
	 * busy connection queues and can indefinitely block a userspace HWM.
	 */
	if (dev->shm->lane_local_sequences) {
		if (sequence > div_u64(U64_MAX - conn->conn_id - 1,
				       dev->total_conns)) {
			ret = -EOVERFLOW;
			goto out_release_slot;
		}
		submit_sequence = sequence * dev->total_conns + conn->conn_id + 1;
	} else {
		submit_sequence = atomic64_inc_return(&dev->shm->submit_sequence);
	}
	if (dev->shm->transfer_payload_slots)
		smp_store_release(zcnblk_shm_payload_owner(dev, conn->conn_id,
							     payload_slot),
				  submit_sequence);

	memset(desc, 0, sizeof(*desc));
	io_contract = zcnblk_shm_io_contract(dev, conn->conn_id, sequence);
	memset(io_contract, 0, sizeof(*io_contract));
	if (wire_op != ZCNBLK_SHM_OP_SYNC) {
		if (wire_op == ZCNBLK_SHM_OP_WRITE &&
		    pdu->rq->cmd_flags & REQ_FUA)
			io_contract->flags |= ZCNBLK_SHM_IO_F_FUA;
		if (pdu->rq->cmd_flags & REQ_POLLED)
			io_contract->flags |=
				ZCNBLK_SHM_IO_F_POLLED_COMPLETION;
		if (wire_op == ZCNBLK_SHM_OP_WRITE &&
		    pdu->rq->cmd_flags & REQ_ATOMIC)
			io_contract->flags |= ZCNBLK_SHM_IO_F_ATOMIC_WRITE;
		io_contract->ioprio = req_get_ioprio(pdu->rq);
		if (wire_op == ZCNBLK_SHM_OP_WRITE && pdu->rq->bio)
			io_contract->write_lifetime =
				pdu->rq->bio->bi_write_hint;
		if (dev->shm->transfer_payload_slots) {
			io_contract->flags |=
				ZCNBLK_SHM_IO_F_REGISTERED_LEASE;
			io_contract->lease_id = submit_sequence;
		}
	}
	pdu->shm_request_id =
		(pdu->shm_ordering_epoch << ZCNBLK_SHM_REQUEST_ID_BITS) |
		((u64)pdu->request_id & ZCNBLK_SHM_REQUEST_ID_MASK);
	desc->request_id = pdu->shm_request_id;
	desc->offset = pdu->remote_off;
	desc->len = pdu->len;
	desc->op = wire_op;
	desc->flags = ZCNBLK_SHM_F_TOPOLOGY_VALID |
		ZCNBLK_SHM_F_PORT_LANE;
	if (payload_alias)
		desc->flags |= ZCNBLK_SHM_F_APP_PAYLOAD_ALIAS;
	desc->lane = conn->lane;
	desc->stream = conn->stream;
	desc->queue_id = conn->conn_id;
	desc->payload_slot = payload_slot;
	desc->submit_sequence = submit_sequence;
	if (wire_op != ZCNBLK_SHM_OP_SYNC) {
		u64 sector = pdu->remote_off >> 12;
		u32 order_slot = hash_64(sector, dev->shm->sector_order_bits);

		desc->sector_predecessor = atomic64_xchg(
			&dev->shm->sector_predecessors[order_slot],
			desc->submit_sequence);
	}
	pdu->shm_sequence = sequence;
	pdu->shm_submit_sequence = submit_sequence;
	pdu->shm_payload_slot = payload_slot;
	pdu->shm_bio_payload_alias = payload_alias;
	if (payload_alias && pdu->op == REQ_OP_WRITE)
		atomic64_inc(&dev->shm->bio_alias_writes);
	conn->shm_inflight[inflight_slot] = pdu;

	/* Publish descriptor bytes before making the sequence visible. */
	smp_store_release(&desc->sequence, sequence + 1);
	smp_store_release(&channel->req_prod, sequence + 1);
	if (!dev->shm->lane_local_sequences && shm_sequence_telemetry_interval &&
	    !(desc->submit_sequence & (shm_sequence_telemetry_interval - 1)))
		WRITE_ONCE(dev->shm->header->global_submit_sequence,
			   desc->submit_sequence);
	WRITE_ONCE(channel->request_publishes,
		   READ_ONCE(channel->request_publishes) + 1);
	list_add_tail(&pdu->entry, &conn->inflight);
	conn->inflight_count++;
	if (xchg(&channel->request_wake_armed, 0)) {
		WRITE_ONCE(channel->request_kicks,
			   READ_ONCE(channel->request_kicks) + 1);
		wake_up_interruptible_poll(&dev->shm->poll_wait,
					   EPOLLIN | EPOLLRDNORM);
	}
	return 0;

out_release_slot:
	if (dev->shm->transfer_payload_slots)
		zcnblk_shm_release_payload_slot(conn, payload_slot,
						ZCNBLK_SHM_PAYLOAD_OWNER_RESERVED,
						false);
	return ret;
}

static int zcnblk_shm_consume_completion_at(struct zcnblk_conn *conn,
					    u64 sequence)
{
	struct zcnblk_dev *dev = conn->dev;
	struct zcnblk_shm_completion *desc;
	struct zcnblk_pdu *pdu;
	u16 want_op;
	u32 inflight_slot;
	u32 payload_channel;
	bool payload_ref;
	blk_status_t status = BLK_STS_OK;
	int ret = 0;

	desc = zcnblk_shm_completion(dev, conn->conn_id, sequence);
	if (smp_load_acquire(&desc->sequence) != sequence + 1)
		return 0;
	if (list_empty(&conn->inflight))
		return -EIO;
	inflight_slot = desc->request_sequence % conn->shm_inflight_entries;
	pdu = conn->shm_inflight[inflight_slot];
	if (!pdu) {
		pr_err_ratelimited("zcnblk: shm completion has no inflight match channel=%u comp=%llu request_seq=%llu slot=%u\n",
				   conn->conn_id, sequence,
				   desc->request_sequence, inflight_slot);
		return -EIO;
	}
	want_op = pdu->op == REQ_OP_WRITE ? ZCNBLK_SHM_OP_WRITE :
		  pdu->op == REQ_OP_READ ? ZCNBLK_SHM_OP_READ :
		  ZCNBLK_SHM_OP_SYNC;
	payload_ref = desc->flags & ZCNBLK_SHM_CQE_F_READ_PAYLOAD_REF;
	payload_channel = conn->conn_id;
	if (desc->request_sequence != pdu->shm_sequence ||
	    desc->request_id != pdu->shm_request_id || desc->op != want_op ||
	    desc->offset != pdu->remote_off || desc->len != pdu->len ||
	    desc->lane != conn->lane || desc->stream != conn->stream) {
		pr_err_ratelimited("zcnblk: shm completion mismatch channel=%u comp=%llu request_seq=%llu expected=%llu op=%u expected_op=%u request_id=%llu expected_id=%llu\n",
				   conn->conn_id, sequence,
				   desc->request_sequence, pdu->shm_sequence,
				   desc->op, want_op, desc->request_id,
				   pdu->shm_request_id);
		return -EIO;
	}
	if (payload_ref) {
		payload_channel =
			(desc->flags & ZCNBLK_SHM_CQE_REF_CHANNEL_MASK) >>
			ZCNBLK_SHM_CQE_REF_CHANNEL_SHIFT;
		if (pdu->op != REQ_OP_READ || !dev->shm->transfer_payload_slots ||
		    !(dev->shm->header->reserved[ZCNBLK_SHM_HEADER_CAPABILITIES] &
		      ZCNBLK_SHM_CAP_READ_PAYLOAD_REF) ||
		    desc->flags & ~(ZCNBLK_SHM_CQE_F_READ_PAYLOAD_REF |
				    ZCNBLK_SHM_CQE_REF_CHANNEL_MASK) ||
		    payload_channel >= dev->total_conns ||
		    desc->payload_slot >= dev->shm->header->payload_entries ||
		    !atomic64_read((atomic64_t *)zcnblk_shm_payload_owner(
				dev, payload_channel, desc->payload_slot))) {
			pr_err_ratelimited("zcnblk: invalid shm read payload ref channel=%u source_channel=%u slot=%u flags=%#x\n",
				   conn->conn_id, payload_channel,
				   desc->payload_slot, desc->flags);
			return -EIO;
		}
	} else if (desc->flags || desc->payload_slot != pdu->shm_payload_slot) {
		pr_err_ratelimited("zcnblk: invalid shm completion payload channel=%u slot=%u expected=%u flags=%#x\n",
				   conn->conn_id, desc->payload_slot,
				   pdu->shm_payload_slot, desc->flags);
		return -EIO;
	}
	if (desc->status) {
		status = BLK_STS_IOERR;
	} else if (pdu->op == REQ_OP_READ &&
		   !(pdu->shm_bio_payload_alias &&
		     payload_channel == conn->conn_id &&
		     desc->payload_slot == pdu->shm_payload_slot)) {
		ret = zcnblk_copy_buf_to_rq(
			pdu->rq, 0,
			zcnblk_shm_payload_slot(dev, payload_channel,
						desc->payload_slot),
			pdu->len);
		if (ret)
			status = BLK_STS_IOERR;
	} else if (pdu->op == REQ_OP_READ) {
		atomic64_inc(&dev->shm->bio_alias_reads);
	}

	list_del_init(&pdu->entry);
	conn->shm_inflight[inflight_slot] = NULL;
	conn->inflight_count--;
	if (dev->shm->transfer_payload_slots && pdu->op != REQ_OP_WRITE)
		zcnblk_shm_release_payload_slot(conn, pdu->shm_payload_slot,
						pdu->shm_submit_sequence,
						pdu->shm_bio_payload_alias);
	zcnblk_complete_pdu(pdu, status);
	return ret ? ret : 1;
}

static int zcnblk_shm_consume_completions(struct zcnblk_conn *conn)
{
	struct zcnblk_shm_channel *channel =
		zcnblk_shm_channel(conn->dev, conn->conn_id);
	u64 sequence = READ_ONCE(channel->comp_cons);
	u64 produced = smp_load_acquire(&channel->comp_prod);
	u32 completed = 0;
	int ret = 0;

	while (sequence != produced && completed < shm_completion_batch) {
		ret = zcnblk_shm_consume_completion_at(conn, sequence);
		if (ret <= 0)
			break;
		sequence++;
		completed++;
	}
	/* Read payloads and non-transfer slots become reusable at this release. */
	if (completed)
		smp_store_release(&channel->comp_cons, sequence);
	return ret < 0 ? ret : completed;
}

static bool zcnblk_shm_spin_for_work(struct zcnblk_conn *conn)
{
	u64 deadline;

	if (!shm_poll_us)
		return false;
	deadline = ktime_get_ns() + (u64)shm_poll_us * NSEC_PER_USEC;
	do {
		if (zcnblk_shm_completion_ready(conn) ||
		    (zcnblk_shm_daemon_online(conn->dev) &&
		     zcnblk_worker_has_pending(conn) &&
		     zcnblk_shm_has_capacity(conn)))
			return true;
		cpu_relax();
	} while (ktime_get_ns() < deadline && !kthread_should_stop());
	return false;
}

static int zcnblk_shm_conn_thread(void *data)
{
	struct zcnblk_conn *conn = data;

	while (!kthread_should_stop()) {
		bool progressed = false;
		int ret;

		while (conn->inflight_count < pipeline_depth &&
		       zcnblk_shm_daemon_online(conn->dev) &&
		       zcnblk_shm_has_capacity(conn)) {
			struct zcnblk_pdu *pdu = zcnblk_pop_pending(conn);

			if (!pdu)
				break;
			ret = zcnblk_shm_submit_pdu(conn, pdu);
			if (ret == -EAGAIN) {
				zcnblk_push_pending_front(conn, pdu);
				break;
			}
			if (ret) {
				zcnblk_complete_pdu(pdu, BLK_STS_IOERR);
				if (ret != -EOPNOTSUPP)
					conn->failed = true;
				break;
			}
			progressed = true;
		}

		while ((ret = zcnblk_shm_consume_completions(conn)) > 0)
			progressed = true;
		if (ret < 0) {
			conn->failed = true;
			break;
		}
		if (conn->failed)
			break;
		if (progressed)
			continue;
		if (zcnblk_shm_spin_for_work(conn))
			continue;

		smp_store_release(&zcnblk_shm_channel(conn->dev, conn->conn_id)
					->completion_wake_armed, 1);
		if (kthread_should_stop() || conn->failed ||
		    zcnblk_shm_completion_ready(conn) ||
		    (zcnblk_shm_daemon_online(conn->dev) &&
		     zcnblk_worker_has_pending(conn) &&
		     zcnblk_shm_has_capacity(conn))) {
			WRITE_ONCE(zcnblk_shm_channel(conn->dev, conn->conn_id)
					->completion_wake_armed, 0);
			continue;
		}
		/*
		 * A producer wake is the fast path, but it cannot be the only
		 * liveness mechanism.  blk-mq can refill an empty connection at the
		 * same boundary where the consumer arms this wait.  If that wake is
		 * missed, there may be neither an in-flight completion nor another
		 * submission to wake the connection again, leaving an otherwise empty
		 * shared ring with requests stranded on conn->pending.  Bound the idle
		 * sleep by the configured SHM polling interval so the condition is
		 * rechecked without adding a timer to the active hot path.
		 */
		if (shm_poll_us) {
			wait_event_interruptible_timeout(conn->wait,
				kthread_should_stop() || conn->failed ||
				zcnblk_shm_completion_ready(conn) ||
				(zcnblk_shm_daemon_online(conn->dev) &&
				 zcnblk_worker_has_pending(conn) &&
				 zcnblk_shm_has_capacity(conn)),
				max_t(unsigned long, 1,
				      usecs_to_jiffies(shm_poll_us)));
		} else {
			wait_event_interruptible(conn->wait,
				kthread_should_stop() || conn->failed ||
				zcnblk_shm_completion_ready(conn) ||
				(zcnblk_shm_daemon_online(conn->dev) &&
				 zcnblk_worker_has_pending(conn) &&
				 zcnblk_shm_has_capacity(conn)));
		}
		WRITE_ONCE(zcnblk_shm_channel(conn->dev, conn->conn_id)
				->completion_wake_armed, 0);
	}

	{
		LIST_HEAD(pending);

		spin_lock(&conn->queue_lock);
		list_splice_init(&conn->pending, &pending);
		spin_unlock(&conn->queue_lock);
		zcnblk_fail_list(&pending);
	}
	zcnblk_fail_list(&conn->worker_pending);
	zcnblk_fail_list(&conn->inflight);
	if (conn->failed)
		wait_event_interruptible(conn->wait, kthread_should_stop());
	return 0;
}

static int zcnblk_conn_thread(void *data)
{
	struct zcnblk_conn *conn = data;

	while (!kthread_should_stop()) {
		bool sent = false;

		while (conn->inflight_count < pipeline_depth) {
			struct zcnblk_pdu *pdu = zcnblk_pop_pending(conn);
			blk_status_t status;
			int ret;

			if (!pdu)
				break;

			if (batch_depth > 1) {
				ret = zcnblk_send_batch(conn, pdu);
				if (ret == -EOPNOTSUPP) {
					if (shard_affinity) {
						zcnblk_complete_pdu(pdu, BLK_STS_IOERR);
						sent = true;
						continue;
					}
					if (conn->inflight_count) {
						zcnblk_push_pending_front(conn, pdu);
						break;
					}
					status = zcnblk_transfer_request_on_conn(conn->dev, conn, pdu->rq);
					zcnblk_complete_pdu(pdu, status);
					sent = true;
					continue;
				}
				if (ret) {
					pr_err_ratelimited("zcnblk: lane=%u stream=%u batch send failed ret=%d inflight=%u pending=%d batch_depth=%u\n",
							   conn->lane, conn->stream, ret,
							   conn->inflight_count,
							   !list_empty_careful(&conn->pending),
							   batch_depth);
					conn->failed = true;
					break;
				}
				sent = true;
				continue;
			}

			ret = zcnblk_send_pdu(conn, pdu);
			if (ret == -EOPNOTSUPP) {
				if (shard_affinity) {
					zcnblk_complete_pdu(pdu, BLK_STS_IOERR);
					sent = true;
					continue;
				}
				if (conn->inflight_count) {
					zcnblk_push_pending_front(conn, pdu);
					break;
				}
				status = zcnblk_transfer_request_on_conn(conn->dev, conn, pdu->rq);
				zcnblk_complete_pdu(pdu, status);
				sent = true;
				continue;
			}
			if (ret) {
				pr_err_ratelimited("zcnblk: lane=%u stream=%u send failed ret=%d inflight=%u pending=%d\n",
						   conn->lane, conn->stream, ret,
						   conn->inflight_count,
						   !list_empty_careful(&conn->pending));
				zcnblk_complete_pdu(pdu, BLK_STS_IOERR);
				conn->failed = true;
				break;
			}

			zcnblk_add_inflight_or_complete(conn, pdu);
			sent = true;
		}

		if (conn->inflight_count && conn->inflight_count < pipeline_depth &&
		    fill_timeout_ms) {
			if (zcnblk_worker_has_pending(conn)) {
				continue;
			}
			wait_event_interruptible_timeout(
				conn->wait,
				kthread_should_stop() ||
					zcnblk_worker_has_pending(conn),
				msecs_to_jiffies(fill_timeout_ms));
			if (kthread_should_stop())
				break;
			if (zcnblk_worker_has_pending(conn))
				continue;
		}

		if (conn->inflight_count) {
			int ret = zcnblk_recv_completion(conn);

			if (ret) {
				pr_err_ratelimited("zcnblk: lane=%u stream=%u recv completion failed inflight=%u pending=%d\n",
						   conn->lane, conn->stream,
						   conn->inflight_count,
						   !list_empty_careful(&conn->pending));
				pr_err_ratelimited("zcnblk: lane=%u stream=%u recv completion ret=%d\n",
						   conn->lane, conn->stream, ret);
				conn->failed = true;
				break;
			}
			continue;
		}

		if (sent)
			continue;

		if (kthread_should_stop() || zcnblk_worker_has_pending(conn)) {
			continue;
		}
		wait_event_interruptible(conn->wait,
					 kthread_should_stop() ||
					 zcnblk_worker_has_pending(conn));
	}

	{
		LIST_HEAD(pending);

		spin_lock(&conn->queue_lock);
		list_splice_init(&conn->pending, &pending);
		spin_unlock(&conn->queue_lock);
		zcnblk_fail_list(&pending);
	}
	zcnblk_fail_list(&conn->worker_pending);
	zcnblk_fail_list(&conn->inflight);
	if (conn->failed)
		wait_event_interruptible(conn->wait, kthread_should_stop());
	return 0;
}

static void zcnblk_shm_snapshot_flush_vector(struct zcnblk_dev *dev,
					      u64 ordering_epoch)
{
	u32 channel_id;

	for (channel_id = 0; channel_id < dev->total_conns; channel_id++) {
		struct zcnblk_conn *conn = &dev->conns[channel_id];
		struct zcnblk_shm_channel *channel =
			zcnblk_shm_channel(dev, channel_id);

		/*
		 * queue_lock closes the race with a request that observed the old
		 * epoch but has not joined this lane yet. Requests admitted after
		 * the snapshot may be over-fenced, but cannot be omitted from an
		 * earlier flush.
		 */
		spin_lock(&conn->queue_lock);
		WRITE_ONCE(channel->request_producer_reserved
			   [ZCNBLK_SHM_CHANNEL_FLUSH_TAIL],
			   conn->shm_admitted_tail);
		smp_store_release(&channel->request_producer_reserved
				  [ZCNBLK_SHM_CHANNEL_FLUSH_EPOCH],
				  ordering_epoch);
		spin_unlock(&conn->queue_lock);
	}
}

static blk_status_t zcnblk_queue_rq(struct blk_mq_hw_ctx *hctx,
				    const struct blk_mq_queue_data *bd)
{
	struct zcnblk_dev *dev = hctx->queue->queuedata;
	struct request *rq = bd->rq;
	struct zcnblk_pdu *pdu = blk_mq_rq_to_pdu(rq);
	struct zcnblk_conn *conn;
	u64 seq;
	u32 conn_idx;

	blk_mq_start_request(rq);
	if (req_op(rq) != REQ_OP_READ &&
	    req_op(rq) != REQ_OP_WRITE &&
	    req_op(rq) != REQ_OP_FLUSH) {
		blk_mq_end_request(rq, BLK_STS_NOTSUPP);
		return BLK_STS_OK;
	}
	if (null_backend) {
		if (req_op(rq) == REQ_OP_FLUSH) {
			pr_err_ratelimited("zcnblk: null_backend refuses to acknowledge block flush without durable media\n");
			blk_mq_end_request(rq, BLK_STS_NOTSUPP);
			return BLK_STS_OK;
		}
		blk_mq_end_request(rq, zcnblk_null_complete_request(dev, rq));
		return BLK_STS_OK;
	}
	if (!dev->shm ||
	    !smp_load_acquire(&dev->shm->header->daemon_online)) {
		blk_mq_end_request(rq, BLK_STS_IOERR);
		return BLK_STS_OK;
	}

	INIT_LIST_HEAD(&pdu->entry);
	pdu->rq = rq;

	if (shard_affinity && req_op(rq) != REQ_OP_FLUSH) {
		u32 shard;
		u64 remote_off;
		u32 len;

		if (!zcnblk_request_is_single_frame(dev, rq, &shard,
						    &remote_off, &len)) {
			blk_mq_end_request(rq, BLK_STS_IOERR);
			return BLK_STS_OK;
		}
		seq = atomic64_inc_return(&dev->next_conn);
		conn_idx = (shard % lanes) * connections_per_lane;
		conn_idx += (u32)((seq - 1) % connections_per_lane);
	} else if (hctx_affinity) {
		conn_idx = hctx->queue_num % dev->total_conns;
	} else {
		seq = atomic64_inc_return(&dev->next_conn);
		conn_idx = (u32)((seq - 1) % dev->total_conns);
	}
	conn = &dev->conns[conn_idx];
	if (READ_ONCE(conn->failed)) {
		blk_mq_end_request(rq, BLK_STS_IOERR);
		return BLK_STS_OK;
	}

	if (shm_ordering_epochs && req_op(rq) == REQ_OP_FLUSH) {
		/* Flushes are rare; serialize only their vector cuts. */
		spin_lock(&dev->shm->ordering_flush_lock);
		pdu->shm_ordering_epoch =
			atomic64_fetch_inc(&dev->shm->ordering_epoch);
		zcnblk_shm_snapshot_flush_vector(dev,
						 pdu->shm_ordering_epoch);
		spin_lock(&conn->queue_lock);
		conn->shm_admitted_tail++;
		list_add_tail(&pdu->entry, &conn->pending);
		spin_unlock(&conn->queue_lock);
		spin_unlock(&dev->shm->ordering_flush_lock);
	} else {
		spin_lock(&conn->queue_lock);
		pdu->shm_ordering_epoch = shm_ordering_epochs ?
			atomic64_read(&dev->shm->ordering_epoch) : 1;
		conn->shm_admitted_tail++;
		list_add_tail(&pdu->entry, &conn->pending);
		spin_unlock(&conn->queue_lock);
	}
	wake_up(&conn->wait);
	return BLK_STS_OK;
}

static void zcnblk_map_queues(struct blk_mq_tag_set *set)
{
	struct blk_mq_queue_map *qmap = &set->map[HCTX_TYPE_DEFAULT];
	unsigned int local_cpus;
	unsigned int local_index = 0;
	unsigned int cpu;

	if (hctx_numa_node == NUMA_NO_NODE) {
		blk_mq_map_queues(qmap);
		return;
	}
	if (hctx_numa_node == -2) {
		unsigned int node_count = 0;
		int node;

		for (node = 0; node < nr_node_ids; node++)
			if (!cpumask_empty(cpumask_of_node(node)))
				node_count++;
		for_each_possible_cpu(cpu) {
			unsigned int node_rank = 0;
			unsigned int node_cpus;
			unsigned int node_index = 0;
			unsigned int queue_begin;
			unsigned int queue_end;
			unsigned int queue;
			unsigned int peer;
			int cpu_node = cpu_to_node(cpu);

			for (node = 0; node < cpu_node; node++)
				if (!cpumask_empty(cpumask_of_node(node)))
					node_rank++;
			node_cpus = cpumask_weight(cpumask_of_node(cpu_node));
			for_each_cpu(peer, cpumask_of_node(cpu_node)) {
				if (peer == cpu)
					break;
				node_index++;
			}
			queue_begin = div_u64((u64)node_rank * qmap->nr_queues,
					      node_count);
			queue_end = div_u64((u64)(node_rank + 1) *
					    qmap->nr_queues, node_count);
			if (queue_end <= queue_begin) {
				queue = min(queue_begin, qmap->nr_queues - 1);
			} else {
				queue = queue_begin +
					div_u64((u64)node_index *
						(queue_end - queue_begin), node_cpus);
				if (queue >= queue_end)
					queue = queue_end - 1;
			}
			qmap->mq_map[cpu] = qmap->queue_offset + queue;
		}
		return;
	}

	local_cpus = cpumask_weight(cpumask_of_node(hctx_numa_node));
	for_each_possible_cpu(cpu) {
		unsigned int queue = 0;

		if (cpu_to_node(cpu) == hctx_numa_node) {
			queue = div_u64((u64)local_index * qmap->nr_queues,
					local_cpus);
			if (queue >= qmap->nr_queues)
				queue = qmap->nr_queues - 1;
			local_index++;
		}
		qmap->mq_map[cpu] = qmap->queue_offset + queue;
	}
}

static const struct blk_mq_ops zcnblk_mq_ops = {
	.queue_rq = zcnblk_queue_rq,
	.map_queues = zcnblk_map_queues,
};

static const struct block_device_operations zcnblk_fops = {
	.owner = THIS_MODULE,
};

static int zcnblk_shm_import_hugetlb_arena(
	struct zcnblk_shm_state *shm,
	const struct zcnblk_shm_arena_import *import)
{
	struct zcnblk_shm_header *new_header;
	struct folio **folios = NULL;
	struct page **pages = NULL;
	struct file *file = NULL;
	void *new_region = NULL;
	unsigned long page_count;
	unsigned long page_index = 0;
	unsigned long i;
	pgoff_t first_offset = 0;
	long nr_folios = 0;
	long seals;
	u64 old_region_bytes;
	int ret = 0;

	if (import->magic != ZCNBLK_SHM_MAGIC ||
	    import->version != ZCNBLK_SHM_VERSION ||
	    import->flags != ZCNBLK_SHM_ARENA_IMPORT_F_HUGETLB ||
	    import->fd < 0 || import->reserved ||
	    !PAGE_ALIGNED(import->region_bytes) ||
	    import->region_bytes > SIZE_MAX ||
	    import->region_bytes < shm->region_bytes)
		return -EINVAL;
	if (import->region_bytes >> PAGE_SHIFT > UINT_MAX)
		return -E2BIG;

	mutex_lock(&shm->arena_lock);
	if (shm->external_hugetlb ||
	    smp_load_acquire(&shm->header->daemon_online) ||
	    atomic64_read(&shm->submit_sequence)) {
		ret = -EBUSY;
		goto out_unlock;
	}

	file = fget(import->fd);
	if (!file) {
		ret = -EBADF;
		goto out_unlock;
	}
	if (!is_file_hugepages(file) ||
	    !IS_ALIGNED(import->region_bytes,
			 huge_page_size(hstate_file(file))) ||
	    i_size_read(file_inode(file)) != import->region_bytes) {
		ret = -EINVAL;
		goto out_file;
	}
	seals = READ_ONCE(HUGETLBFS_I(file_inode(file))->seals);
	if ((seals & (F_SEAL_SHRINK | F_SEAL_GROW)) !=
	    (F_SEAL_SHRINK | F_SEAL_GROW) ||
	    seals & (F_SEAL_WRITE | F_SEAL_FUTURE_WRITE)) {
		ret = -EINVAL;
		goto out_file;
	}

	page_count = import->region_bytes >> PAGE_SHIFT;
	folios = kvmalloc_array(page_count, sizeof(*folios), GFP_KERNEL);
	if (!folios) {
		ret = -ENOMEM;
		goto out_file;
	}
	nr_folios = memfd_pin_folios(file, 0, import->region_bytes - 1,
				      folios, page_count, &first_offset);
	if (nr_folios <= 0) {
		ret = nr_folios ? nr_folios : -EINVAL;
		nr_folios = 0;
		goto out_folios;
	}
	if (first_offset) {
		ret = -EINVAL;
		goto out_unpin;
	}

	pages = kvmalloc_array(page_count, sizeof(*pages), GFP_KERNEL);
	if (!pages) {
		ret = -ENOMEM;
		goto out_unpin;
	}
	for (i = 0; i < nr_folios && page_index < page_count; i++) {
		unsigned long subpage;
		unsigned long folio_pages;

		if (!folio_test_hugetlb(folios[i])) {
			ret = -EINVAL;
			goto out_pages;
		}
		folio_pages = folio_nr_pages(folios[i]);
		for (subpage = 0;
		     subpage < folio_pages && page_index < page_count;
		     subpage++)
			pages[page_index++] = folio_page(folios[i], subpage);
	}
	if (page_index != page_count) {
		ret = -EINVAL;
		goto out_pages;
	}
	for (i = 0; i < page_count; i++) {
		ret = xa_err(xa_store(&shm->arena_page_indices,
				      page_to_pfn(pages[i]), xa_mk_value(i),
				      GFP_KERNEL));
		if (ret)
			goto out_page_index;
	}
	new_region = vmap(pages, page_count, VM_MAP, PAGE_KERNEL);
	if (!new_region) {
		ret = -ENOMEM;
		goto out_page_index;
	}

	old_region_bytes = shm->region_bytes;
	memcpy(new_region, shm->region, old_region_bytes);
	new_header = new_region;
	new_header->region_bytes = import->region_bytes;
	new_header->reserved[ZCNBLK_SHM_HEADER_CAPABILITIES] |=
		ZCNBLK_SHM_CAP_EXTERNAL_HUGETLB_IMPORT |
		ZCNBLK_SHM_CAP_EXTERNAL_HUGETLB_ACTIVE;

	/*
	 * Connection kthreads exist before the userspace daemon imports its
	 * arena.  Keep the original vmalloc mapping alive until those kthreads
	 * have been stopped at module teardown: a reader may have loaded the old
	 * region pointer immediately before this one-time backing swap.
	 */
	shm->fallback_region = shm->region;
	shm->region = new_region;
	shm->region_bytes = import->region_bytes;
	shm->header = new_header;
	shm->arena_file = file;
	shm->arena_folios = folios;
	shm->arena_nr_folios = nr_folios;
	shm->external_hugetlb = true;
	new_region = NULL;
	file = NULL;
	folios = NULL;
	ret = 0;

out_page_index:
	if (ret) {
		xa_destroy(&shm->arena_page_indices);
		xa_init(&shm->arena_page_indices);
	}
out_pages:
	kvfree(pages);
out_unpin:
	if (ret && nr_folios > 0)
		unpin_folios(folios, nr_folios);
out_folios:
	kvfree(folios);
out_file:
	if (file)
		fput(file);
out_unlock:
	mutex_unlock(&shm->arena_lock);
	return ret;
}

static int zcnblk_shm_ctl_open(struct inode *inode, struct file *file)
{
	struct zcnblk_shm_state *shm;

	if (!zcnblk_dev || !zcnblk_dev->shm)
		return -ENODEV;
	shm = zcnblk_dev->shm;
	if (atomic_cmpxchg(&shm->daemon_open, 0, 1))
		return -EBUSY;
	file->private_data = shm;
	return nonseekable_open(inode, file);
}

static int zcnblk_shm_ctl_release(struct inode *inode, struct file *file)
{
	struct zcnblk_shm_state *shm = file->private_data;
	u32 i;

	(void)inode;
	if (!shm)
		return 0;
	smp_store_release(&shm->header->daemon_online, 0);
	if (zcnblk_dev && zcnblk_dev->conns) {
		for (i = 0; i < zcnblk_dev->active_conns; i++) {
			WRITE_ONCE(zcnblk_dev->conns[i].failed, true);
			wake_up(&zcnblk_dev->conns[i].wait);
		}
	}
	atomic_set(&shm->daemon_open, 0);
	wake_up_interruptible_poll(&shm->poll_wait, EPOLLHUP | EPOLLERR);
	return 0;
}

static long zcnblk_shm_ctl_ioctl(struct file *file, unsigned int cmd,
				 unsigned long arg)
{
	struct zcnblk_shm_state *shm = file->private_data;
	void __user *argp = (void __user *)arg;
	u32 channel;
	u32 i;

	if (!shm)
		return -ENODEV;
	switch (cmd) {
	case ZCNBLK_SHM_IOC_GET_INFO:
		if (copy_to_user(argp, shm->header, sizeof(*shm->header)))
			return -EFAULT;
		return 0;
	case ZCNBLK_SHM_IOC_IMPORT_ARENA: {
		struct zcnblk_shm_arena_import import;

		if (copy_from_user(&import, argp, sizeof(import)))
			return -EFAULT;
		return zcnblk_shm_import_hugetlb_arena(shm, &import);
	}
	case ZCNBLK_SHM_IOC_ATTACH: {
		struct zcnblk_shm_attach attach;

		if (copy_from_user(&attach, argp, sizeof(attach)))
			return -EFAULT;
		if (attach.magic != ZCNBLK_SHM_MAGIC ||
		    attach.version != ZCNBLK_SHM_VERSION ||
		    attach.flags & ~(ZCNBLK_SHM_ATTACH_F_TRANSFER_PAYLOAD_SLOTS |
				     ZCNBLK_SHM_ATTACH_F_LANE_LOCAL_SEQUENCE))
			return -EINVAL;
		if (attach.flags & ZCNBLK_SHM_ATTACH_F_TRANSFER_PAYLOAD_SLOTS) {
			if (!(shm->header->reserved[ZCNBLK_SHM_HEADER_CAPABILITIES] &
			      ZCNBLK_SHM_CAP_TRANSFER_PAYLOAD_SLOTS))
				return -EOPNOTSUPP;
			shm->transfer_payload_slots = true;
		}
		if (attach.flags & ZCNBLK_SHM_ATTACH_F_LANE_LOCAL_SEQUENCE) {
			if (!(shm->header->reserved[ZCNBLK_SHM_HEADER_CAPABILITIES] &
			      ZCNBLK_SHM_CAP_LANE_LOCAL_SEQUENCE))
				return -EOPNOTSUPP;
			shm->lane_local_sequences = true;
		}
		WRITE_ONCE(shm->header->daemon_generation,
			   READ_ONCE(shm->header->daemon_generation) + 1);
		smp_store_release(&shm->header->daemon_online, 1);
		for (i = 0; i < zcnblk_dev->active_conns; i++)
			wake_up(&zcnblk_dev->conns[i].wait);
		return 0;
	}
	case ZCNBLK_SHM_IOC_KICK:
		if (copy_from_user(&channel, argp, sizeof(channel)))
			return -EFAULT;
		if (channel == U32_MAX) {
			for (i = 0; i < zcnblk_dev->active_conns; i++)
				wake_up(&zcnblk_dev->conns[i].wait);
			return 0;
		}
		if (channel >= zcnblk_dev->active_conns)
			return -EINVAL;
		WRITE_ONCE(zcnblk_shm_channel(zcnblk_dev, channel)->completion_kicks,
			   READ_ONCE(zcnblk_shm_channel(zcnblk_dev, channel)->completion_kicks) + 1);
		wake_up(&zcnblk_dev->conns[channel].wait);
		return 0;
	default:
		return -ENOTTY;
	}
}

static int zcnblk_shm_ctl_mmap(struct file *file, struct vm_area_struct *vma)
{
	struct zcnblk_shm_state *shm = file->private_data;
	struct file *arena_file = NULL;
	struct file *control_file;
	unsigned long len = vma->vm_end - vma->vm_start;
	int ret;

	if (!shm)
		return -ENODEV;
	if (vma->vm_pgoff || len != shm->region_bytes)
		return -EINVAL;
	mutex_lock(&shm->arena_lock);
	if (shm->external_hugetlb)
		arena_file = get_file(shm->arena_file);
	mutex_unlock(&shm->arena_lock);
	if (!arena_file)
		return remap_vmalloc_range(vma, shm->region, 0);

	/* Replace the control-file VMA backing with the retained HugeTLB memfd. */
	control_file = vma->vm_file;
	vma->vm_file = arena_file;
	ret = vfs_mmap(arena_file, vma);
	if (ret) {
		vma->vm_file = control_file;
		fput(arena_file);
		return ret;
	}
	fput(control_file);
	return 0;
}

static unsigned long zcnblk_shm_ctl_get_unmapped_area(
	struct file *file, unsigned long addr, unsigned long len,
	unsigned long pgoff, unsigned long flags)
{
	struct zcnblk_shm_state *shm = file->private_data;
	struct file *arena_file = NULL;
	unsigned long area;

	if (!shm)
		return -ENODEV;
	mutex_lock(&shm->arena_lock);
	if (shm->external_hugetlb)
		arena_file = get_file(shm->arena_file);
	mutex_unlock(&shm->arena_lock);
	if (!arena_file) {
#if LINUX_VERSION_CODE >= KERNEL_VERSION(6, 19, 0)
		return mm_get_unmapped_area(file, addr, len, pgoff, flags);
#else
		return mm_get_unmapped_area(current->mm, file, addr, len,
					    pgoff, flags);
#endif
	}

	/*
	 * The control file redirects mmap to the imported hugetlbfs file.  Its
	 * address selection must be redirected too: the generic character-device
	 * allocator may return a base-page-aligned hole whose PMD already contains
	 * regular PTEs, which cannot host a HugeTLB mapping.
	 */
	if (arena_file->f_op->get_unmapped_area)
		area = arena_file->f_op->get_unmapped_area(
			arena_file, addr, len, pgoff, flags);
	else {
#if LINUX_VERSION_CODE >= KERNEL_VERSION(6, 19, 0)
		area = mm_get_unmapped_area(
			arena_file, addr, len, pgoff, flags);
#else
		area = mm_get_unmapped_area(
			current->mm, arena_file, addr, len, pgoff, flags);
#endif
	}
	fput(arena_file);
	return area;
}

static __poll_t zcnblk_shm_ctl_poll(struct file *file, poll_table *wait)
{
	struct zcnblk_shm_state *shm = file->private_data;
	__poll_t mask = 0;
	u32 i;

	if (!shm)
		return EPOLLERR;
	poll_wait(file, &shm->poll_wait, wait);
	if (!smp_load_acquire(&shm->header->daemon_online))
		mask |= EPOLLHUP;
	for (i = 0; i < shm->header->channels; i++) {
		struct zcnblk_shm_channel *channel =
			zcnblk_shm_channel(zcnblk_dev, i);

		if (smp_load_acquire(&channel->req_prod) !=
		    READ_ONCE(channel->req_cons)) {
			mask |= EPOLLIN | EPOLLRDNORM;
			break;
		}
	}
	return mask;
}

static const struct file_operations zcnblk_shm_ctl_fops = {
	.owner = THIS_MODULE,
	.open = zcnblk_shm_ctl_open,
	.release = zcnblk_shm_ctl_release,
	.unlocked_ioctl = zcnblk_shm_ctl_ioctl,
	.compat_ioctl = compat_ptr_ioctl,
	.get_unmapped_area = zcnblk_shm_ctl_get_unmapped_area,
	.mmap = zcnblk_shm_ctl_mmap,
	.poll = zcnblk_shm_ctl_poll,
	.llseek = noop_llseek,
};

static void zcnblk_disconnect_all(struct zcnblk_dev *dev)
{
	u32 i;
	u32 active;

	if (!dev || !dev->conns)
		return;
	active = min(dev->active_conns, dev->total_conns);
	for (i = 0; i < active; i++) {
		if (dev->conns[i].sock)
			kernel_sock_shutdown(dev->conns[i].sock, SHUT_RDWR);
		wake_up(&dev->conns[i].wait);
	}
	for (i = 0; i < active; i++) {
		if (dev->conns[i].thread) {
			kthread_stop(dev->conns[i].thread);
			dev->conns[i].thread = NULL;
		}
	}
	for (i = 0; i < active; i++) {
		if (dev->conns[i].sock) {
			sock_release(dev->conns[i].sock);
			dev->conns[i].sock = NULL;
		}
		zcnblk_crypto_free_conn(&dev->conns[i]);
		kfree(dev->conns[i].shm_inflight);
		dev->conns[i].shm_inflight = NULL;
		dev->conns[i].shm_inflight_entries = 0;
	}
	dev->active_conns = 0;
}

static int zcnblk_thread_pin_cpu(u32 conn_id, unsigned int *cpu)
{
	unsigned int count = pin_cpu_count ? pin_cpu_count : num_online_cpus();
	unsigned int stride = pin_stride ? pin_stride : 1;
	unsigned int target;
	u64 offset;

	if (!count)
		return -EINVAL;
	offset = (u64)conn_id * stride;
	target = pin_base_cpu + (unsigned int)(offset % count);
	if (target >= nr_cpu_ids || !cpu_online(target))
		return -EINVAL;
	*cpu = target;
	return 0;
}

static int zcnblk_shm_page_align(u64 value, u64 *aligned)
{
	u64 rounded;

	if (check_add_overflow(value, (u64)PAGE_SIZE - 1, &rounded))
		return -EOVERFLOW;
	*aligned = rounded & PAGE_MASK;
	return 0;
}

static int zcnblk_shm_layout_init(struct zcnblk_dev *dev)
{
	struct zcnblk_shm_state *shm;
	struct zcnblk_shm_header *hdr;
	u64 entries = shm_ring_entries ? shm_ring_entries : pipeline_depth;
	u64 payload_entries = shm_payload_entries ? shm_payload_entries : entries;
	u64 descriptor_slots;
	u64 payload_slots;
	u64 bytes;
	u64 offset = PAGE_SIZE;
	int ret;

	if (!entries || entries > U32_MAX || entries < pipeline_depth ||
	    !payload_entries || payload_entries > U32_MAX ||
	    payload_entries < pipeline_depth || !shm_sector_order_slots ||
	    !is_power_of_2(shm_sector_order_slots))
		return -EINVAL;
	if (check_mul_overflow((u64)dev->total_conns, entries,
			       &descriptor_slots) ||
	    check_mul_overflow((u64)dev->total_conns, payload_entries,
			       &payload_slots))
		return -EOVERFLOW;

	shm = kzalloc(sizeof(*shm), GFP_KERNEL);
	if (!shm)
		return -ENOMEM;
	dev->shm = shm;
	mutex_init(&shm->arena_lock);
	xa_init(&shm->arena_page_indices);
	atomic64_set(&shm->bio_alias_writes, 0);
	atomic64_set(&shm->bio_alias_reads, 0);
	atomic64_set(&shm->bio_alias_busy_fallbacks, 0);
	atomic64_set(&shm->bio_alias_required_retries, 0);
	atomic64_set(&shm->bio_alias_required_rejects, 0);
	shm->sector_predecessors = kvcalloc(shm_sector_order_slots,
					    sizeof(*shm->sector_predecessors),
					    GFP_KERNEL);
	if (!shm->sector_predecessors) {
		ret = -ENOMEM;
		goto out_free;
	}
	shm->sector_order_bits = ilog2(shm_sector_order_slots);

	if (check_mul_overflow((u64)dev->total_conns,
			       (u64)sizeof(struct zcnblk_shm_channel), &bytes) ||
	    check_add_overflow(offset, bytes, &offset) ||
	    zcnblk_shm_page_align(offset, &offset)) {
		ret = -EOVERFLOW;
		goto out_free;
	}
	/* Save offsets after each prior section has been aligned. */
	shm->region_bytes = offset;
	if (check_mul_overflow(descriptor_slots,
			       (u64)sizeof(struct zcnblk_shm_request), &bytes) ||
	    check_add_overflow(offset, bytes, &offset) ||
	    zcnblk_shm_page_align(offset, &offset)) {
		ret = -EOVERFLOW;
		goto out_free;
	}
	if (check_mul_overflow(descriptor_slots,
			       (u64)sizeof(struct zcnblk_shm_completion), &bytes) ||
	    check_add_overflow(offset, bytes, &offset) ||
	    zcnblk_shm_page_align(offset, &offset)) {
		ret = -EOVERFLOW;
		goto out_free;
	}
	if (check_mul_overflow(descriptor_slots,
			       (u64)sizeof(struct zcnblk_shm_io_contract), &bytes) ||
	    check_add_overflow(offset, bytes, &offset) ||
	    zcnblk_shm_page_align(offset, &offset)) {
		ret = -EOVERFLOW;
		goto out_free;
	}
	if (check_mul_overflow(payload_slots, (u64)sizeof(u64), &bytes) ||
	    check_add_overflow(offset, bytes, &offset) ||
	    zcnblk_shm_page_align(offset, &offset)) {
		ret = -EOVERFLOW;
		goto out_free;
	}
	if (check_mul_overflow(payload_slots, (u64)max_frame_bytes, &bytes) ||
	    check_add_overflow(offset, bytes, &offset) ||
	    zcnblk_shm_page_align(offset, &offset) || offset > SIZE_MAX) {
		ret = -EOVERFLOW;
		goto out_free;
	}

	shm->region_bytes = offset;
	shm->region = vmalloc_user(shm->region_bytes);
	if (!shm->region) {
		ret = -ENOMEM;
		goto out_free;
	}
	shm->header = shm->region;
	hdr = shm->header;
	hdr->magic = ZCNBLK_SHM_MAGIC;
	hdr->version = ZCNBLK_SHM_VERSION;
	hdr->header_bytes = PAGE_SIZE;
	hdr->channels = dev->total_conns;
	hdr->ring_entries = entries;
	hdr->payload_entries = payload_entries;
	hdr->slot_bytes = max_frame_bytes;
	hdr->descriptor_bytes = ZCNBLK_SHM_DESC_BYTES;
	hdr->channel_offset = PAGE_SIZE;
	ret = zcnblk_shm_page_align(
		hdr->channel_offset +
			(u64)dev->total_conns * sizeof(struct zcnblk_shm_channel),
		&hdr->request_offset);
	if (ret)
		goto out_region;
	ret = zcnblk_shm_page_align(
		hdr->request_offset +
			descriptor_slots * sizeof(struct zcnblk_shm_request),
		&hdr->completion_offset);
	if (ret)
		goto out_region;
	ret = zcnblk_shm_page_align(
		hdr->completion_offset +
			descriptor_slots * sizeof(struct zcnblk_shm_completion),
		&hdr->reserved[ZCNBLK_SHM_HEADER_IO_CONTRACT_OFFSET]);
	if (ret)
		goto out_region;
	ret = zcnblk_shm_page_align(
		hdr->reserved[ZCNBLK_SHM_HEADER_IO_CONTRACT_OFFSET] +
			descriptor_slots * sizeof(struct zcnblk_shm_io_contract),
		&hdr->reserved[ZCNBLK_SHM_HEADER_PAYLOAD_OWNER_OFFSET]);
	if (ret)
		goto out_region;
	ret = zcnblk_shm_page_align(
		hdr->reserved[ZCNBLK_SHM_HEADER_PAYLOAD_OWNER_OFFSET] +
			payload_slots * sizeof(u64),
		&hdr->payload_offset);
	if (ret)
		goto out_region;
	hdr->region_bytes = shm->region_bytes;
	hdr->capacity_bytes = dev->capacity_bytes;
	hdr->reserved[ZCNBLK_SHM_HEADER_CAPABILITIES] =
		ZCNBLK_SHM_CAP_SECTOR_PREDECESSOR |
		ZCNBLK_SHM_CAP_TRANSFER_PAYLOAD_SLOTS |
		ZCNBLK_SHM_CAP_READ_PAYLOAD_REF |
		ZCNBLK_SHM_CAP_REQUEST_WAKE_ARMED |
		ZCNBLK_SHM_CAP_COMPLETION_WAKE_ARMED |
		ZCNBLK_SHM_CAP_IO_CONTRACT_SIDECAR |
		ZCNBLK_SHM_CAP_EXTERNAL_HUGETLB_IMPORT |
		ZCNBLK_SHM_CAP_BIO_ARENA_ALIAS |
		ZCNBLK_SHM_CAP_LANE_LOCAL_SEQUENCE;
	if (shm_sequence_telemetry_interval != 1)
		hdr->reserved[ZCNBLK_SHM_HEADER_CAPABILITIES] |=
			ZCNBLK_SHM_CAP_SAMPLED_SEQUENCE_TELEMETRY;
	hdr->reserved[ZCNBLK_SHM_HEADER_IO_FEATURES] =
		ZCNBLK_SHM_IO_FEATURE_ALL;
	if (shm_ordering_epochs)
		hdr->reserved[ZCNBLK_SHM_HEADER_CAPABILITIES] |=
			ZCNBLK_SHM_CAP_ORDERING_EPOCH |
			ZCNBLK_SHM_CAP_ORDERING_VECTOR;
	for (ret = 0; ret < dev->total_conns; ret++)
		atomic64_set((atomic64_t *)&zcnblk_shm_channel(dev, ret)->payload_free_slots,
			     payload_entries);
	init_waitqueue_head(&shm->poll_wait);
	atomic_set(&shm->daemon_open, 0);
	atomic64_set(&shm->submit_sequence, 0);
	atomic64_set(&shm->ordering_epoch, 1);
	spin_lock_init(&shm->ordering_flush_lock);

	shm->misc.minor = MISC_DYNAMIC_MINOR;
	shm->misc.name = "zcnblk-shmctl";
	shm->misc.fops = &zcnblk_shm_ctl_fops;
	shm->misc.mode = 0600;
	ret = misc_register(&shm->misc);
	if (ret)
		goto out_region;
	shm->registered = true;
	return 0;

out_region:
	vfree(shm->region);
out_free:
	xa_destroy(&shm->arena_page_indices);
	kvfree(shm->sector_predecessors);
	kfree(shm);
	dev->shm = NULL;
	return ret;
}

static void zcnblk_shm_layout_destroy(struct zcnblk_dev *dev)
{
	struct zcnblk_shm_state *shm;

	if (!dev || !dev->shm)
		return;
	shm = dev->shm;
	if (shm->registered)
		misc_deregister(&shm->misc);
	if (shm->external_hugetlb) {
		vunmap(shm->region);
		unpin_folios(shm->arena_folios, shm->arena_nr_folios);
		kvfree(shm->arena_folios);
		fput(shm->arena_file);
		vfree(shm->fallback_region);
	} else {
		vfree(shm->region);
	}
	xa_destroy(&shm->arena_page_indices);
	kvfree(shm->sector_predecessors);
	kfree(shm);
	dev->shm = NULL;
}

static int zcnblk_shm_connect_one(struct zcnblk_dev *dev,
				  struct zcnblk_conn *conn, u32 lane,
				  u32 stream, u32 conn_id)
{
	int ret;

	mutex_init(&conn->lock);
	spin_lock_init(&conn->queue_lock);
	init_waitqueue_head(&conn->wait);
	INIT_LIST_HEAD(&conn->pending);
	INIT_LIST_HEAD(&conn->worker_pending);
	INIT_LIST_HEAD(&conn->inflight);
	conn->dev = dev;
	conn->lane = lane;
	conn->stream = stream;
	conn->conn_id = conn_id;
	conn->shm_inflight_entries = dev->shm->header->ring_entries;
	conn->shm_inflight = kcalloc(conn->shm_inflight_entries,
					    sizeof(*conn->shm_inflight), GFP_KERNEL);
	if (!conn->shm_inflight)
		return -ENOMEM;
	conn->thread = kthread_create(zcnblk_shm_conn_thread, conn,
				      "zcnblk-shm-%u-%u", lane, stream);
	if (IS_ERR(conn->thread)) {
		ret = PTR_ERR(conn->thread);
		conn->thread = NULL;
		kfree(conn->shm_inflight);
		conn->shm_inflight = NULL;
		conn->shm_inflight_entries = 0;
		return ret;
	}
	if (pin_threads) {
		unsigned int cpu;

		ret = zcnblk_thread_pin_cpu(conn_id, &cpu);
		if (ret) {
			pr_warn("zcnblk: PERF WARNING: shm conn_id=%u lane=%u stream=%u maps to invalid/offline CPU base=%u count=%u stride=%u; leaving thread unpinned\n",
				conn_id, lane, stream, pin_base_cpu, pin_cpu_count,
				pin_stride);
		} else {
			kthread_bind(conn->thread, cpu);
		}
	}
	wake_up_process(conn->thread);
	return 0;
}

static int zcnblk_shm_connect_all(struct zcnblk_dev *dev)
{
	u32 lane;
	u32 stream;
	u32 idx = 0;
	int ret;

	ret = zcnblk_shm_layout_init(dev);
	if (ret)
		return ret;
	dev->conns = kcalloc(dev->total_conns, sizeof(*dev->conns), GFP_KERNEL);
	if (!dev->conns) {
		zcnblk_shm_layout_destroy(dev);
		return -ENOMEM;
	}
	for (lane = 0; lane < lanes; lane++) {
		for (stream = 0; stream < connections_per_lane; stream++) {
			ret = zcnblk_shm_connect_one(dev, &dev->conns[idx], lane,
						 stream, idx);
			if (ret) {
				zcnblk_disconnect_all(dev);
				kfree(dev->conns);
				dev->conns = NULL;
				zcnblk_shm_layout_destroy(dev);
				return ret;
			}
			idx++;
			dev->active_conns = idx;
		}
	}
	return 0;
}

static int zcnblk_connect_one(struct zcnblk_dev *dev, struct zcnblk_conn *conn,
			      u32 lane, u32 stream, u32 conn_id, __be32 addr)
{
	struct sockaddr_in sin = {
		.sin_family = AF_INET,
		.sin_addr.s_addr = addr,
		.sin_port = htons(remote_port_base + lane),
	};
	int ret;

	mutex_init(&conn->lock);
	spin_lock_init(&conn->queue_lock);
	init_waitqueue_head(&conn->wait);
	INIT_LIST_HEAD(&conn->pending);
	INIT_LIST_HEAD(&conn->worker_pending);
	INIT_LIST_HEAD(&conn->inflight);
	conn->dev = dev;
	conn->lane = lane;
	conn->stream = stream;
	conn->conn_id = conn_id;
	conn->port = remote_port_base + lane;
	ret = sock_create_kern(&init_net, AF_INET, SOCK_STREAM, IPPROTO_TCP,
			       &conn->sock);
	if (ret)
		return ret;
	ret = kernel_connect(conn->sock, (void *)&sin, sizeof(sin), 0);
	if (ret) {
		sock_release(conn->sock);
		conn->sock = NULL;
		return ret;
	}
	tcp_sock_set_nodelay(conn->sock->sk);
	ret = zcnblk_crypto_setup_conn(conn);
	if (ret) {
		kernel_sock_shutdown(conn->sock, SHUT_RDWR);
		sock_release(conn->sock);
		conn->sock = NULL;
		return ret;
	}
	conn->thread = kthread_create(zcnblk_conn_thread, conn, "zcnblk-%u-%u",
				      lane, stream);
	if (IS_ERR(conn->thread)) {
		ret = PTR_ERR(conn->thread);
		conn->thread = NULL;
		kernel_sock_shutdown(conn->sock, SHUT_RDWR);
		sock_release(conn->sock);
		conn->sock = NULL;
		zcnblk_crypto_free_conn(conn);
		return ret;
	}
	if (pin_threads) {
		unsigned int cpu;

		ret = zcnblk_thread_pin_cpu(conn_id, &cpu);
		if (ret) {
			pr_warn("zcnblk: PERF WARNING: pin_threads requested but conn_id=%u lane=%u stream=%u maps to invalid/offline CPU base=%u count=%u stride=%u; leaving thread unpinned\n",
				conn_id, lane, stream, pin_base_cpu, pin_cpu_count,
				pin_stride);
		} else {
			kthread_bind(conn->thread, cpu);
		}
	}
	wake_up_process(conn->thread);
	return 0;
}

static int zcnblk_parse_remote_addrs(void)
{
	char *spec, *cursor, *token;
	__be32 addr;
	int ret = 0;

	zcnblk_remote_addr_count = 0;
	if (!remote_ips || !*remote_ips) {
		addr = in_aton(remote_ip);
		if (!addr)
			return -EINVAL;
		zcnblk_remote_addrs[0] = addr;
		zcnblk_remote_addr_count = 1;
		return 0;
	}

	spec = kstrdup(remote_ips, GFP_KERNEL);
	if (!spec)
		return -ENOMEM;
	cursor = spec;
	while ((token = strsep(&cursor, ","))) {
		token = strim(token);
		if (!*token)
			continue;
		if (zcnblk_remote_addr_count >= ZCNBLK_MAX_REMOTE_IPS) {
			ret = -E2BIG;
			goto out;
		}
		addr = in_aton(token);
		if (!addr) {
			ret = -EINVAL;
			goto out;
		}
		zcnblk_remote_addrs[zcnblk_remote_addr_count++] = addr;
	}
	if (!zcnblk_remote_addr_count)
		ret = -EINVAL;
	else if (zcnblk_remote_addr_count > lanes)
		ret = -EINVAL;

out:
	kfree(spec);
	return ret;
}

static __be32 zcnblk_remote_addr_for_lane(u32 lane)
{
	u32 idx;

	if (zcnblk_remote_addr_count <= 1)
		return zcnblk_remote_addrs[0];
	idx = div_u64((u64)lane * zcnblk_remote_addr_count, lanes);
	if (idx >= zcnblk_remote_addr_count)
		idx = zcnblk_remote_addr_count - 1;
	return zcnblk_remote_addrs[idx];
}

static int zcnblk_connect_all(struct zcnblk_dev *dev)
{
	u32 lane;
	u32 stream;
	u32 idx = 0;
	int ret;

	ret = zcnblk_parse_remote_addrs();
	if (ret)
		return ret;
	dev->conns = kcalloc(dev->total_conns, sizeof(*dev->conns), GFP_KERNEL);
	if (!dev->conns)
		return -ENOMEM;

	for (lane = 0; lane < lanes; lane++) {
		__be32 addr = zcnblk_remote_addr_for_lane(lane);

		for (stream = 0; stream < connections_per_lane; stream++) {
			ret = zcnblk_connect_one(dev, &dev->conns[idx], lane,
						 stream, idx, addr);
			if (ret) {
				pr_err("zcnblk: connect lane=%u stream=%u %pI4:%u failed ret=%d\n",
				       lane, stream, &addr, remote_port_base + lane,
				       ret);
				zcnblk_disconnect_all(dev);
				kfree(dev->conns);
				dev->conns = NULL;
				return ret;
			}
			idx++;
			dev->active_conns = idx;
		}
	}
	return 0;
}

static int __init zcnblk_init(void)
{
	struct queue_limits lim = { };
	u64 capacity_bytes;
	u32 total_conns;
	u32 nr_queues;
	int ret;

	if (!lanes || !connections_per_lane || !shard_count || !size_mib ||
	    !max_frame_bytes || !queue_depth || !pipeline_depth || !batch_depth ||
	    !shm_completion_batch)
		return -EINVAL;
	if (shm_sequence_telemetry_interval &&
	    !is_power_of_2(shm_sequence_telemetry_interval)) {
		pr_err("zcnblk: shm_sequence_telemetry_interval must be zero or a power of two\n");
		return -EINVAL;
	}
	if (!transport || (strcmp(transport, "tcp") && strcmp(transport, "shm"))) {
		pr_err("zcnblk: unknown transport=%s; use tcp or shm\n",
		       transport ? transport : "(null)");
		return -EINVAL;
	}
	if (zcnblk_shm_enabled() && null_backend) {
		pr_err("zcnblk: transport=shm and null_backend=1 are mutually exclusive\n");
		return -EINVAL;
	}
	if (zcnblk_shm_enabled() && !shm_ordering_epochs) {
		pr_err("zcnblk: transport=shm requires shm_ordering_epochs=1; refusing a block edge that cannot preserve global sync cuts\n");
		return -EINVAL;
	}
	if (zcnblk_shm_enabled() && aes256_gcm_token && *aes256_gcm_token) {
		pr_err("zcnblk: transport=shm does not encrypt same-host shared memory; encryption belongs on the userspace remote transport\n");
		return -EINVAL;
	}
	if (shard_count != 1) {
		pr_err("zcnblk: shard_count=%u rejected; kernel client is only the fabric block edge, userspace owns striping and mirroring\n",
		       shard_count);
		return -EINVAL;
	}
	if (shard_affinity) {
		pr_err("zcnblk: shard_affinity rejected; userspace target/gateway owns shard placement\n");
		return -EINVAL;
	}
	if (hctx_numa_node != NUMA_NO_NODE && hctx_numa_node != -2 &&
	    (hctx_numa_node < 0 || hctx_numa_node >= nr_node_ids ||
	     cpumask_empty(cpumask_of_node(hctx_numa_node)))) {
		pr_err("zcnblk: hctx_numa_node=%d has no possible CPUs\n",
		       hctx_numa_node);
		return -EINVAL;
	}
	if (!zcnblk_shm_enabled() && remote_port_base > U16_MAX - lanes)
		return -EINVAL;
	if (blk_validate_block_size(logical_block_size))
		return -EINVAL;
	if (max_frame_bytes % logical_block_size)
		return -EINVAL;
	if (check_mul_overflow((u64)size_mib, (u64)SZ_1M, &capacity_bytes))
		return -EOVERFLOW;
	if (check_mul_overflow(lanes, connections_per_lane, &total_conns))
		return -EOVERFLOW;
	if (!pin_threads)
		pr_warn("zcnblk: PERF WARNING: connection kthreads are not module-pinned; set pin_threads=1 pin_base_cpu=<cpu> pin_cpu_count=<n> pin_stride=<n>, or bind [zcnblk-L-S] kthreads with taskset before benchmarking\n");
	if (!hctx_affinity)
		pr_warn("zcnblk: PERF WARNING: hctx_affinity=0; blk-mq queues will not map directly to target connections\n");
	if (!zcnblk_shm_enabled() && batch_depth <= 1 && pipeline_depth > 1)
		pr_warn("zcnblk: PERF WARNING: batch_depth=1 with pipeline_depth=%u; 4K IOPS runs will pay more per-request wakeup/header overhead\n",
			pipeline_depth);
	if (!zcnblk_shm_enabled() && batch_depth > 1 && !batch_fill_timeout_us)
		pr_warn("zcnblk: PERF WARNING: batch_depth=%u but batch_fill_timeout_us=0; connection kthreads may send underfilled batches before fio has filled the lane queue\n",
			batch_depth);
	zcnblk_dev = kzalloc(sizeof(*zcnblk_dev), GFP_KERNEL);
	if (!zcnblk_dev)
		return -ENOMEM;
	zcnblk_dev->capacity_bytes = capacity_bytes;
	zcnblk_dev->total_conns = total_conns;
	atomic64_set(&zcnblk_dev->next_conn, 0);

	ret = zcnblk_crypto_init(zcnblk_dev);
	if (ret)
		goto out_free_dev;
	if (null_backend) {
		pr_warn("zcnblk: PERF WARNING: null_backend=1 completes requests locally; this is a device-edge ceiling test, not fabric or RAID performance\n");
	} else if (zcnblk_shm_enabled()) {
		ret = zcnblk_shm_connect_all(zcnblk_dev);
		if (ret)
			goto out_crypto;
	} else {
		ret = zcnblk_connect_all(zcnblk_dev);
		if (ret)
			goto out_crypto;
	}
	if (publish_delay_ms)
		msleep(publish_delay_ms);

	zcnblk_dev->major = register_blkdev(0, ZCNBLK_NAME);
	if (zcnblk_dev->major <= 0) {
		ret = zcnblk_dev->major ? zcnblk_dev->major : -EBUSY;
		goto out_disconnect;
	}

	nr_queues = queues ? queues : total_conns;
	zcnblk_dev->tag_set.ops = &zcnblk_mq_ops;
	zcnblk_dev->tag_set.nr_hw_queues = nr_queues;
	zcnblk_dev->tag_set.queue_depth = queue_depth;
	zcnblk_dev->tag_set.numa_node = NUMA_NO_NODE;
	zcnblk_dev->tag_set.cmd_size = sizeof(struct zcnblk_pdu);
	zcnblk_dev->tag_set.flags = BLK_MQ_F_NO_SCHED_BY_DEFAULT;
	if (!null_backend && !zcnblk_shm_enabled())
		zcnblk_dev->tag_set.flags |= BLK_MQ_F_BLOCKING;
	zcnblk_dev->tag_set.driver_data = zcnblk_dev;

	ret = blk_mq_alloc_tag_set(&zcnblk_dev->tag_set);
	if (ret)
		goto out_unregister;

	lim.logical_block_size = logical_block_size;
	lim.physical_block_size = logical_block_size;
	lim.io_min = logical_block_size;
	lim.max_segments = USHRT_MAX;
	lim.max_segment_size = UINT_MAX;
	lim.max_hw_sectors = max_frame_bytes >> SECTOR_SHIFT;
	/* Normal writes are cached; REQ_FUA is carried to the userspace WAL leaf. */
	lim.features = BLK_FEAT_WRITE_CACHE | BLK_FEAT_FUA;

	zcnblk_dev->disk = blk_mq_alloc_disk(&zcnblk_dev->tag_set, &lim, zcnblk_dev);
	if (IS_ERR(zcnblk_dev->disk)) {
		ret = PTR_ERR(zcnblk_dev->disk);
		zcnblk_dev->disk = NULL;
		goto out_tags;
	}
	zcnblk_dev->disk->flags |= GENHD_FL_NO_PART;
	zcnblk_dev->disk->major = zcnblk_dev->major;
	zcnblk_dev->disk->first_minor = 0;
	zcnblk_dev->disk->minors = 1;
	zcnblk_dev->disk->fops = &zcnblk_fops;
	zcnblk_dev->disk->private_data = zcnblk_dev;
	strscpy(zcnblk_dev->disk->disk_name, ZCNBLK_DISK_NAME, DISK_NAME_LEN);
	set_capacity(zcnblk_dev->disk, capacity_bytes >> SECTOR_SHIFT);

	ret = add_disk(zcnblk_dev->disk);
	if (ret)
		goto out_put_disk;
	zcnblk_debugfs_dir = debugfs_create_dir(ZCNBLK_NAME, NULL);
	if (IS_ERR_OR_NULL(zcnblk_debugfs_dir)) {
		if (IS_ERR(zcnblk_debugfs_dir))
			pr_warn("zcnblk: debugfs state unavailable: %ld\n",
				PTR_ERR(zcnblk_debugfs_dir));
		zcnblk_debugfs_dir = NULL;
	} else {
		debugfs_create_file("state", 0444, zcnblk_debugfs_dir, NULL,
				    &zcnblk_debugfs_state_fops);
	}

	pr_info("zcnblk: /dev/%s transport=%s remote=%s remote_ips=%s remote_count=%u port_base=%u lanes=%u connections_per_lane=%u total_conns=%u shards=%u bytes=%llu frame=%u queues=%u depth=%u pipeline_depth=%u batch_depth=%u batch_fill_timeout_us=%u fill_timeout_ms=%u write_acks=%d null_backend=%d null_read_zero=%d hctx_affinity=%d hctx_numa_node=%d pin_threads=%d pin_base_cpu=%u pin_cpu_count=%u pin_stride=%u shard_affinity=%d encryption=%s aes_frame=%u publish_delay_ms=%u shm_ring_entries=%u shm_payload_entries=%u shm_region_bytes=%zu shm_poll_us=%u shm_bio_arena_zero_copy=%d shm_bio_arena_zero_copy_required=%d placement_owner=userspace block_client_placement=no\n",
		ZCNBLK_DISK_NAME, transport, remote_ip, remote_ips ? remote_ips : "-", zcnblk_remote_addr_count,
		remote_port_base, lanes, connections_per_lane,
		total_conns, shard_count, capacity_bytes, max_frame_bytes,
		nr_queues, queue_depth, pipeline_depth,
		batch_depth, batch_fill_timeout_us, fill_timeout_ms, write_acks,
		null_backend, null_read_zero,
		hctx_affinity, hctx_numa_node, pin_threads, pin_base_cpu, pin_cpu_count, pin_stride,
		shard_affinity,
		zcnblk_crypto_enabled(zcnblk_dev) ? "aes-256-gcm" : "none",
		aes256_gcm_frame_bytes, publish_delay_ms,
		zcnblk_dev->shm ? zcnblk_dev->shm->header->ring_entries : 0,
		zcnblk_dev->shm ? zcnblk_dev->shm->header->payload_entries : 0,
		zcnblk_dev->shm ? zcnblk_dev->shm->region_bytes : 0,
		shm_poll_us, shm_bio_arena_zero_copy,
		shm_bio_arena_zero_copy_required);
	return 0;

out_put_disk:
	put_disk(zcnblk_dev->disk);
out_tags:
	blk_mq_free_tag_set(&zcnblk_dev->tag_set);
out_unregister:
	unregister_blkdev(zcnblk_dev->major, ZCNBLK_NAME);
out_disconnect:
	zcnblk_disconnect_all(zcnblk_dev);
	kfree(zcnblk_dev->conns);
	zcnblk_shm_layout_destroy(zcnblk_dev);
out_crypto:
	zcnblk_crypto_free(zcnblk_dev);
out_free_dev:
	kfree(zcnblk_dev);
	zcnblk_dev = NULL;
	return ret;
}

static void __exit zcnblk_exit(void)
{
	if (!zcnblk_dev)
		return;
	debugfs_remove(zcnblk_debugfs_dir);
	zcnblk_debugfs_dir = NULL;
	del_gendisk(zcnblk_dev->disk);
	zcnblk_disconnect_all(zcnblk_dev);
	zcnblk_shm_layout_destroy(zcnblk_dev);
	zcnblk_crypto_free(zcnblk_dev);
	put_disk(zcnblk_dev->disk);
	blk_mq_free_tag_set(&zcnblk_dev->tag_set);
	unregister_blkdev(zcnblk_dev->major, ZCNBLK_NAME);
	kfree(zcnblk_dev->conns);
	kfree(zcnblk_dev);
	zcnblk_dev = NULL;
}

module_init(zcnblk_init);
module_exit(zcnblk_exit);

MODULE_AUTHOR("zcutils");
MODULE_DESCRIPTION("zcnblk SAN fabric client block device");
MODULE_LICENSE("GPL");
