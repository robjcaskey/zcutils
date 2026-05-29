// SPDX-License-Identifier: GPL-2.0

#include <linux/blk-mq.h>
#include <linux/blk_types.h>
#include <linux/blkdev.h>
#include <linux/bvec.h>
#include <linux/atomic.h>
#include <linux/crypto.h>
#include <linux/delay.h>
#include <linux/errno.h>
#include <linux/highmem.h>
#include <linux/in.h>
#include <linux/inet.h>
#include <linux/kernel.h>
#include <linux/kthread.h>
#include <linux/list.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/net.h>
#include <linux/overflow.h>
#include <linux/random.h>
#include <linux/slab.h>
#include <linux/scatterlist.h>
#include <linux/socket.h>
#include <linux/spinlock.h>
#include <linux/string.h>
#include <linux/wait.h>
#include <crypto/aead.h>
#include <crypto/hash.h>
#include <net/sock.h>
#include <net/tcp.h>

#define ZCNBLK_NAME "zcnblk"
#define ZCNBLK_FRAME_MAGIC "ZCNBLK01"
#define ZCNBLK_FRAME_VERSION 1
#define ZCNBLK_FRAME_HEADER_LEN 32
#define ZCNBLK_OP_WRITE 1
#define ZCNBLK_OP_READ 2
#define ZCNBLK_OP_READ_RESP 3
#define ZCNBLK_OP_WRITE_ACK 4
#define ZCNBLK_OP_BATCH 5
#define ZCNBLK_OP_BATCH_RESP 6
#define ZCNBLK_AES256_GCM_KEY_LEN 32
#define ZCNBLK_AES256_GCM_IV_LEN 12
#define ZCNBLK_AES256_GCM_TAG_LEN 16
#define ZCNBLK_AES256_MAGIC "ZCNBAE01"
#define ZCNBLK_AES256_MAGIC_LEN 8
#define ZCNBLK_AES256_HANDSHAKE_LEN \
	(ZCNBLK_AES256_MAGIC_LEN + ZCNBLK_AES256_GCM_IV_LEN * 2)
#define ZCNBLK_AES256_DEFAULT_FRAME_BYTES (64U * 1024U)
#define ZCNBLK_INLINE_BOUNCE_BYTES 4096

static char *remote_ip = "127.0.0.1";
module_param(remote_ip, charp, 0444);
MODULE_PARM_DESC(remote_ip, "IPv4 address of zcnblk-target");

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
MODULE_PARM_DESC(shard_count, "Number of remote target shards");

static ulong size_mib = 1024;
module_param(size_mib, ulong, 0444);
MODULE_PARM_DESC(size_mib, "Client block device size in MiB");

static uint logical_block_size = 4096;
module_param(logical_block_size, uint, 0444);
MODULE_PARM_DESC(logical_block_size, "Logical block size");

static uint stripe_unit = 393216;
module_param(stripe_unit, uint, 0444);
MODULE_PARM_DESC(stripe_unit, "Logical stripe unit across remote shards");

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

static uint fill_timeout_ms = 5000;
module_param(fill_timeout_ms, uint, 0444);
MODULE_PARM_DESC(fill_timeout_ms, "Time to wait for more queued requests before receiving a partial pipeline");

static bool write_acks;
module_param(write_acks, bool, 0444);
MODULE_PARM_DESC(write_acks, "Wait for target write acknowledgements before completing writes");

static uint publish_delay_ms;
module_param(publish_delay_ms, uint, 0444);
MODULE_PARM_DESC(publish_delay_ms, "Delay after TCP connect before publishing /dev/zcnblk0");

static uint batch_depth = 1;
module_param(batch_depth, uint, 0444);
MODULE_PARM_DESC(batch_depth, "Maximum same-op requests to pack into one ZCNBLK batch frame");

static bool hctx_affinity = true;
module_param(hctx_affinity, bool, 0444);
MODULE_PARM_DESC(hctx_affinity, "Map blk-mq hardware queues directly to target connections when possible");

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
	void *bounce;
	u32 shard;
	u32 len;
	u16 request_id;
	u64 remote_off;
	enum req_op op;
	bool bounce_inline;
	u8 inline_bounce[ZCNBLK_INLINE_BOUNCE_BYTES];
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
} __packed;

struct zcnblk_conn {
	struct socket *sock;
	struct mutex lock;
	spinlock_t queue_lock;
	wait_queue_head_t wait;
	struct task_struct *thread;
	struct list_head pending;
	struct list_head inflight;
	struct zcnblk_dev *dev;
	u32 inflight_count;
	u32 lane;
	u32 stream;
	u32 conn_id;
	u16 next_request_id;
	u16 port;
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
};

static struct zcnblk_dev *zcnblk_dev;

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

static void zcnblk_make_header(struct zcnblk_frame_header *hdr, u16 op,
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

static int zcnblk_map(u64 logical, u32 *shard, u64 *remote_off,
		      u32 *stripe_remaining)
{
	u64 stripe_no;
	u64 row;
	u32 stripe_off;
	u32 shard_idx;

	if (!stripe_unit || !shard_count)
		return -EINVAL;
	stripe_no = div_u64_rem(logical, stripe_unit, &stripe_off);
	row = div_u64_rem(stripe_no, shard_count, &shard_idx);
	*shard = shard_idx;
	*remote_off = row * (u64)stripe_unit + stripe_off;
	*stripe_remaining = stripe_unit - stripe_off;
	return 0;
}

static int zcnblk_copy_rq_to_buf(struct request *rq, size_t rq_off,
				 void *buf, size_t len)
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
		memcpy(buf + copied, mapped + seg_off, take);
		kunmap_local(mapped);
		copied += take;
		skipped += bvec.bv_len;
		if (copied == len)
			return 0;
	}

	return -EIO;
}

static int zcnblk_copy_buf_to_rq(struct request *rq, size_t rq_off,
				 const void *buf, size_t len)
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
		memcpy(mapped + seg_off, buf + copied, take);
		flush_dcache_page(bvec.bv_page);
		kunmap_local(mapped);
		copied += take;
		skipped += bvec.bv_len;
		if (copied == len)
			return 0;
	}

	return -EIO;
}

static int zcnblk_do_frame(struct zcnblk_conn *conn, struct request *rq,
			   enum req_op op, size_t rq_off, u64 logical,
			   u32 len, void *bounce)
{
	struct zcnblk_frame_header hdr;
	u32 shard;
	u32 stripe_rem;
	u64 remote_off;
	int ret;

	ret = zcnblk_map(logical, &shard, &remote_off, &stripe_rem);
	if (ret)
		return ret;

	if (op == REQ_OP_WRITE) {
		ret = zcnblk_copy_rq_to_buf(rq, rq_off, bounce, len);
		if (ret)
			return ret;
		zcnblk_make_header(&hdr, ZCNBLK_OP_WRITE, 0, shard, len, remote_off);
		ret = zcnblk_send_frame_payload(conn, &hdr, bounce, len);
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

	zcnblk_make_header(&hdr, ZCNBLK_OP_READ, 0, shard, len, remote_off);
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
	ret = zcnblk_recv_frame_payload(conn, &hdr, bounce, len);
	if (ret)
		return ret;
	return zcnblk_copy_buf_to_rq(rq, rq_off, bounce, len);
}

static blk_status_t zcnblk_transfer_request_on_conn(struct zcnblk_dev *dev,
						    struct zcnblk_conn *conn,
						    struct request *rq)
{
	enum req_op op = req_op(rq);
	u64 logical = (u64)blk_rq_pos(rq) << SECTOR_SHIFT;
	u64 bytes = blk_rq_bytes(rq);
	u64 done = 0;
	void *bounce;
	u32 bounce_bytes;
	int ret = 0;

	if (op == REQ_OP_FLUSH)
		return BLK_STS_OK;
	if (op != REQ_OP_READ && op != REQ_OP_WRITE)
		return BLK_STS_NOTSUPP;
	if (logical > dev->capacity_bytes || bytes > dev->capacity_bytes - logical)
		return BLK_STS_IOERR;

	bounce_bytes = min_t(u64, bytes, max_frame_bytes);
	bounce = kmalloc(bounce_bytes, GFP_NOIO);
	if (!bounce)
		return BLK_STS_RESOURCE;

	while (done < bytes) {
		u32 shard;
		u32 stripe_rem;
		u64 remote_off;
		u32 frame_len;

		ret = zcnblk_map(logical + done, &shard, &remote_off, &stripe_rem);
		if (ret)
			break;
		frame_len = min_t(u64, bytes - done, max_frame_bytes);
		frame_len = min(frame_len, stripe_rem);
		ret = zcnblk_do_frame(conn, rq, op, done, logical + done,
				      frame_len, bounce);
		if (ret)
			break;
		done += frame_len;
	}

	kfree(bounce);
	return ret ? BLK_STS_IOERR : BLK_STS_OK;
}

static bool zcnblk_request_is_single_frame(struct zcnblk_dev *dev,
					   struct request *rq, u32 *shard,
					   u64 *remote_off, u32 *len)
{
	u64 logical = (u64)blk_rq_pos(rq) << SECTOR_SHIFT;
	u64 bytes = blk_rq_bytes(rq);
	u32 stripe_rem;

	if (req_op(rq) != REQ_OP_READ && req_op(rq) != REQ_OP_WRITE)
		return false;
	if (logical > dev->capacity_bytes || bytes > dev->capacity_bytes - logical)
		return false;
	if (!bytes || bytes > max_frame_bytes || bytes > U32_MAX)
		return false;
	if (zcnblk_map(logical, shard, remote_off, &stripe_rem))
		return false;
	if (bytes > stripe_rem)
		return false;
	*len = bytes;
	return true;
}

static struct zcnblk_pdu *zcnblk_pop_pending(struct zcnblk_conn *conn)
{
	struct zcnblk_pdu *pdu = NULL;

	spin_lock(&conn->queue_lock);
	if (!list_empty(&conn->pending)) {
		pdu = list_first_entry(&conn->pending, struct zcnblk_pdu, entry);
		list_del_init(&pdu->entry);
	}
	spin_unlock(&conn->queue_lock);
	return pdu;
}

static void zcnblk_complete_pdu(struct zcnblk_pdu *pdu, blk_status_t status)
{
	struct request *rq = pdu->rq;

	if (pdu->bounce && !pdu->bounce_inline)
		kfree(pdu->bounce);
	pdu->bounce = NULL;
	pdu->bounce_inline = false;
	pdu->rq = NULL;
	blk_mq_end_request(rq, status);
}

static void zcnblk_free_pdu_bounce(struct zcnblk_pdu *pdu)
{
	if (pdu->bounce && !pdu->bounce_inline)
		kfree(pdu->bounce);
	pdu->bounce = NULL;
	pdu->bounce_inline = false;
}

static int zcnblk_prepare_pdu(struct zcnblk_conn *conn, struct zcnblk_pdu *pdu)
{
	int ret;

	pdu->op = req_op(pdu->rq);
	if (!zcnblk_request_is_single_frame(conn->dev, pdu->rq, &pdu->shard,
					    &pdu->remote_off, &pdu->len))
		return -EOPNOTSUPP;

	if (pdu->len <= ZCNBLK_INLINE_BOUNCE_BYTES) {
		pdu->bounce = pdu->inline_bounce;
		pdu->bounce_inline = true;
	} else {
		pdu->bounce = kmalloc(pdu->len, GFP_NOIO);
		pdu->bounce_inline = false;
		if (!pdu->bounce)
			return -ENOMEM;
	}

	if (pdu->op == REQ_OP_WRITE) {
		ret = zcnblk_copy_rq_to_buf(pdu->rq, 0, pdu->bounce, pdu->len);
		if (ret)
			return ret;
	}

	pdu->request_id = conn->next_request_id++;
	return 0;
}

static int zcnblk_send_prepared_pdu(struct zcnblk_conn *conn,
				    struct zcnblk_pdu *pdu)
{
	struct zcnblk_frame_header hdr;
	int ret;

	if (pdu->op == REQ_OP_WRITE) {
		zcnblk_make_header(&hdr, ZCNBLK_OP_WRITE, pdu->request_id,
				   pdu->shard, pdu->len, pdu->remote_off);
		ret = zcnblk_send_frame_payload(conn, &hdr, pdu->bounce,
						pdu->len);
		if (!write_acks)
			zcnblk_free_pdu_bounce(pdu);
		return ret;
	}

	zcnblk_make_header(&hdr, ZCNBLK_OP_READ, pdu->request_id, pdu->shard,
			   pdu->len, pdu->remote_off);
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
	if (pdu->op == REQ_OP_READ || write_acks) {
		list_add_tail(&pdu->entry, &conn->inflight);
		conn->inflight_count++;
	} else {
		zcnblk_complete_pdu(pdu, BLK_STS_OK);
	}
}

static void zcnblk_push_pending_front(struct zcnblk_conn *conn,
				      struct zcnblk_pdu *pdu)
{
	spin_lock(&conn->queue_lock);
	list_add(&pdu->entry, &conn->pending);
	spin_unlock(&conn->queue_lock);
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

	pdus = kcalloc(depth, sizeof(*pdus), GFP_NOIO);
	hdrs = kcalloc(depth, sizeof(*hdrs), GFP_NOIO);
	iov = kcalloc(depth + 2, sizeof(*iov), GFP_NOIO);
	if (!pdus || !hdrs || !iov) {
		kfree(iov);
		kfree(hdrs);
		kfree(pdus);
		zcnblk_complete_pdu(first, BLK_STS_IOERR);
		return -ENOMEM;
	}

	batch_op = req_op(first->rq);
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

	zcnblk_make_header(&outer, ZCNBLK_OP_BATCH, 0, 0, count, 0);
	for (i = 0; i < count; i++) {
		u16 wire_op = pdus[i]->op == REQ_OP_WRITE ? ZCNBLK_OP_WRITE :
							   ZCNBLK_OP_READ;

		zcnblk_make_header(&hdrs[i], wire_op, pdus[i]->request_id,
				   pdus[i]->shard, pdus[i]->len,
				   pdus[i]->remote_off);
	}

	iov_count = 0;
	total_len = sizeof(outer) + count * sizeof(*hdrs);
	iov[iov_count].iov_base = &outer;
	iov[iov_count++].iov_len = sizeof(outer);
	iov[iov_count].iov_base = hdrs;
	iov[iov_count++].iov_len = count * sizeof(*hdrs);
	if (batch_op == REQ_OP_WRITE) {
		for (i = 0; i < count; i++) {
			iov[iov_count].iov_base = pdus[i]->bounce;
			iov[iov_count++].iov_len = pdus[i]->len;
			total_len += pdus[i]->len;
		}
	}

	ret = zcnblk_conn_send_iov_all(conn, iov, iov_count, total_len);
	if (batch_op == REQ_OP_WRITE && !write_acks) {
		for (i = 0; i < count; i++)
			zcnblk_free_pdu_bounce(pdus[i]);
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
	u16 want_op = response_op == ZCNBLK_OP_READ_RESP ? REQ_OP_READ : REQ_OP_WRITE;
	u16 request_id = le16_to_cpu(hdr->flags);

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
	    *response_op != ZCNBLK_OP_WRITE_ACK) {
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
		ret = zcnblk_recv_frame_payload(conn, hdr, pdu->bounce,
						pdu->len);
		if (ret) {
			zcnblk_complete_pdu(pdu, BLK_STS_IOERR);
			return ret;
		}
		ret = zcnblk_copy_buf_to_rq(pdu->rq, 0, pdu->bounce, pdu->len);
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
			wait_event_interruptible_timeout(
				conn->wait,
				kthread_should_stop() ||
					!list_empty_careful(&conn->pending),
				msecs_to_jiffies(fill_timeout_ms));
			if (kthread_should_stop())
				break;
			if (!list_empty_careful(&conn->pending))
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

		wait_event_interruptible(conn->wait,
					 kthread_should_stop() ||
					 !list_empty_careful(&conn->pending));
	}

	{
		LIST_HEAD(pending);

		spin_lock(&conn->queue_lock);
		list_splice_init(&conn->pending, &pending);
		spin_unlock(&conn->queue_lock);
		zcnblk_fail_list(&pending);
	}
	zcnblk_fail_list(&conn->inflight);
	if (conn->failed)
		wait_event_interruptible(conn->wait, kthread_should_stop());
	return 0;
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
	if (req_op(rq) == REQ_OP_FLUSH) {
		blk_mq_end_request(rq, BLK_STS_OK);
		return BLK_STS_OK;
	}
	if (req_op(rq) != REQ_OP_READ && req_op(rq) != REQ_OP_WRITE) {
		blk_mq_end_request(rq, BLK_STS_NOTSUPP);
		return BLK_STS_OK;
	}

	INIT_LIST_HEAD(&pdu->entry);
	pdu->rq = rq;
	pdu->bounce = NULL;
	pdu->bounce_inline = false;

	if (hctx_affinity) {
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

	spin_lock(&conn->queue_lock);
	list_add_tail(&pdu->entry, &conn->pending);
	spin_unlock(&conn->queue_lock);
	wake_up(&conn->wait);
	return BLK_STS_OK;
}

static const struct blk_mq_ops zcnblk_mq_ops = {
	.queue_rq = zcnblk_queue_rq,
};

static const struct block_device_operations zcnblk_fops = {
	.owner = THIS_MODULE,
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
	}
	dev->active_conns = 0;
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
	conn->thread = kthread_run(zcnblk_conn_thread, conn, "zcnblk-%u-%u",
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
	return 0;
}

static int zcnblk_connect_all(struct zcnblk_dev *dev)
{
	__be32 addr = in_aton(remote_ip);
	u32 lane;
	u32 stream;
	u32 idx = 0;
	int ret;

	if (!addr)
		return -EINVAL;
	dev->conns = kcalloc(dev->total_conns, sizeof(*dev->conns), GFP_KERNEL);
	if (!dev->conns)
		return -ENOMEM;

	for (lane = 0; lane < lanes; lane++) {
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

	if (!lanes || !connections_per_lane || !shard_count || !size_mib || !stripe_unit ||
	    !max_frame_bytes || !queue_depth || !batch_depth)
		return -EINVAL;
	if (remote_port_base > U16_MAX - lanes)
		return -EINVAL;
	if (blk_validate_block_size(logical_block_size))
		return -EINVAL;
	if (stripe_unit % logical_block_size || max_frame_bytes % logical_block_size)
		return -EINVAL;
	if (check_mul_overflow((u64)size_mib, (u64)SZ_1M, &capacity_bytes))
		return -EOVERFLOW;
	if (check_mul_overflow(lanes, connections_per_lane, &total_conns))
		return -EOVERFLOW;
	zcnblk_dev = kzalloc(sizeof(*zcnblk_dev), GFP_KERNEL);
	if (!zcnblk_dev)
		return -ENOMEM;
	zcnblk_dev->capacity_bytes = capacity_bytes;
	zcnblk_dev->total_conns = total_conns;
	atomic64_set(&zcnblk_dev->next_conn, 0);

	ret = zcnblk_crypto_init(zcnblk_dev);
	if (ret)
		goto out_free_dev;
	ret = zcnblk_connect_all(zcnblk_dev);
	if (ret)
		goto out_crypto;
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
	zcnblk_dev->tag_set.flags = BLK_MQ_F_NO_SCHED_BY_DEFAULT | BLK_MQ_F_BLOCKING;
	zcnblk_dev->tag_set.driver_data = zcnblk_dev;

	ret = blk_mq_alloc_tag_set(&zcnblk_dev->tag_set);
	if (ret)
		goto out_unregister;

	lim.logical_block_size = logical_block_size;
	lim.physical_block_size = logical_block_size;
	lim.io_min = logical_block_size;
	lim.io_opt = stripe_unit;
	lim.max_segments = USHRT_MAX;
	lim.max_segment_size = UINT_MAX;
	lim.max_hw_sectors = max_frame_bytes >> SECTOR_SHIFT;

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
	strscpy(zcnblk_dev->disk->disk_name, "zcnblk0", DISK_NAME_LEN);
	set_capacity(zcnblk_dev->disk, capacity_bytes >> SECTOR_SHIFT);

	ret = add_disk(zcnblk_dev->disk);
	if (ret)
		goto out_put_disk;

	pr_info("zcnblk: /dev/zcnblk0 remote=%s:%u lanes=%u connections_per_lane=%u total_conns=%u shards=%u bytes=%llu stripe=%u frame=%u queues=%u depth=%u batch_depth=%u write_acks=%d hctx_affinity=%d encryption=%s aes_frame=%u publish_delay_ms=%u\n",
		remote_ip, remote_port_base, lanes, connections_per_lane,
		total_conns, shard_count, capacity_bytes, stripe_unit,
		max_frame_bytes, nr_queues, queue_depth, batch_depth,
		write_acks, hctx_affinity,
		zcnblk_crypto_enabled(zcnblk_dev) ? "aes-256-gcm" : "none",
		aes256_gcm_frame_bytes, publish_delay_ms);
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
	del_gendisk(zcnblk_dev->disk);
	zcnblk_disconnect_all(zcnblk_dev);
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
MODULE_DESCRIPTION("zcnblk client-side TCP block device");
MODULE_LICENSE("GPL");
