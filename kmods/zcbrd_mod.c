// SPDX-License-Identifier: GPL-2.0

#include <linux/blk-mq.h>
#include <linux/blk_types.h>
#include <linux/blkdev.h>
#include <linux/bvec.h>
#include <linux/configfs.h>
#include <linux/errno.h>
#include <linux/highmem.h>
#include <linux/idr.h>
#include <linux/kernel.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/overflow.h>
#include <linux/slab.h>
#include <linux/string.h>
#include <linux/vmalloc.h>
#include <asm/barrier.h>

#define ZCBRD_DESC_MAGIC 0x435345445242435aULL
#define ZCBRD_DESC_VERSION 1
#define ZCBRD_DESC_F_TOPOLOGY_HINTS (1U << 0)
#define ZCBRD_DESC_F_RELEASE_TOKEN (1U << 1)
#define ZCBRD_DESC_F_BLOCK_EXTENTS (1U << 2)

struct zcbrd_slice_desc {
	u32 pool_id;
	u32 queue_id;
	u64 buffer_id;
	u32 generation;
	u32 offset;
	u32 len;
	u32 flags;
	s16 numa_node;
	s16 preferred_cpu;
};

struct zcbrd_record_desc {
	u16 desc_version;
	u16 desc_len;
	u64 record_id;
	u64 stream_id;
	u64 group_id;
	u64 sequence;
	u32 lane_id;
	u32 preferred_worker;
	u32 total_len;
	u16 slice_count;
	u16 flags;
	u64 release_token;
};

struct zcbrd_desc_batch {
	u64 magic;
	u16 version;
	u16 flags;
	u32 record_count;
	u64 device_offset;
	u64 bytes;
};

enum zcbrd_descriptor_mode {
	ZCBRD_DESC_DISABLED = 0,
	ZCBRD_DESC_ADVERTISE = 1,
};

struct zcbrd_arena {
	void *data;
	u64 bytes;
};

struct zcbrd_backing {
	struct zcbrd_arena *arenas;
	u32 arena_count;
	u32 block_size;
	u64 capacity_bytes;
	u64 arena_span_bytes;
};

enum zcbrd_data_mode {
	ZCBRD_DATA_COPY,
	ZCBRD_DATA_BYPASS,
	ZCBRD_DATA_NT_WRITE,
	ZCBRD_DATA_NT_READ,
	ZCBRD_DATA_NT_RW,
};

struct zcbrd_cfg;

struct zcbrd_disk {
	struct zcbrd_cfg *cfg;
	struct blk_mq_tag_set tag_set;
	struct gendisk *disk;
	struct zcbrd_backing backing;
	enum zcbrd_data_mode data_mode;
	int index;
};

struct zcbrd_cfg {
	struct config_group group;
	bool powered;
	char name[DISK_NAME_LEN];
	u32 block_size;
	u64 capacity_mib;
	u32 queues;
	u32 queue_depth;
	u32 shards;
	enum zcbrd_descriptor_mode descriptor_mode;
	enum zcbrd_data_mode data_mode;
	struct zcbrd_disk *runtime;
};

static DEFINE_MUTEX(zcbrd_lock);
static DEFINE_IDA(zcbrd_indexes);
static int zcbrd_major;

static inline struct zcbrd_cfg *to_zcbrd_cfg(struct config_item *item)
{
	return item ? container_of(to_config_group(item), struct zcbrd_cfg, group) : NULL;
}

static int zcbrd_alloc_backing(struct zcbrd_backing *backing, u64 capacity_bytes,
			       u32 block_size, u32 requested_arenas)
{
	u64 logical_blocks;
	u64 blocks_per_arena;
	u32 arena_count;
	u32 i;
	int ret = 0;

	if (!backing || !capacity_bytes || !block_size || !requested_arenas)
		return -EINVAL;
	if (!is_power_of_2(block_size) || !IS_ALIGNED(capacity_bytes, block_size))
		return -EINVAL;

	memset(backing, 0, sizeof(*backing));
	logical_blocks = capacity_bytes / block_size;
	if (!logical_blocks)
		return -EINVAL;

	arena_count = min_t(u64, requested_arenas, logical_blocks);
	blocks_per_arena = DIV_ROUND_UP_ULL(logical_blocks, arena_count);
	backing->arena_span_bytes = blocks_per_arena * block_size;
	backing->arenas = kcalloc(arena_count, sizeof(*backing->arenas), GFP_KERNEL);
	if (!backing->arenas)
		return -ENOMEM;

	backing->arena_count = arena_count;
	backing->block_size = block_size;
	backing->capacity_bytes = capacity_bytes;

	for (i = 0; i < arena_count; i++) {
		u64 arena_base = (u64)i * backing->arena_span_bytes;
		u64 arena_bytes;

		if (arena_base >= capacity_bytes)
			break;
		arena_bytes = min_t(u64, backing->arena_span_bytes,
				    capacity_bytes - arena_base);
		backing->arenas[i].bytes = arena_bytes;
		backing->arenas[i].data = vzalloc(arena_bytes);
		if (!backing->arenas[i].data) {
			ret = -ENOMEM;
			break;
		}
	}

	if (ret) {
		for (i = 0; i < backing->arena_count; i++)
			vfree(backing->arenas[i].data);
		kfree(backing->arenas);
		memset(backing, 0, sizeof(*backing));
	}
	return ret;
}

static void zcbrd_free_backing(struct zcbrd_backing *backing)
{
	u32 i;

	if (!backing)
		return;
	for (i = 0; i < backing->arena_count; i++)
		vfree(backing->arenas[i].data);
	kfree(backing->arenas);
	memset(backing, 0, sizeof(*backing));
}

static blk_status_t zcbrd_backing_chunk(const struct zcbrd_backing *backing,
					u64 pos, u64 bytes, void **ptr,
					size_t *len)
{
	struct zcbrd_arena *arena;
	u64 arena_index;
	u64 arena_offset;
	u64 chunk;

	if (!backing || !backing->arenas || !ptr || !len || !bytes)
		return BLK_STS_IOERR;
	if (pos >= backing->capacity_bytes)
		return BLK_STS_IOERR;

	if (backing->arena_count == 1) {
		arena = &backing->arenas[0];
		chunk = min_t(u64, bytes, backing->capacity_bytes - pos);
		*ptr = (u8 *)arena->data + pos;
		*len = chunk;
		return BLK_STS_OK;
	}

	arena_index = pos / backing->arena_span_bytes;
	if (arena_index >= backing->arena_count)
		return BLK_STS_IOERR;
	arena = &backing->arenas[arena_index];
	arena_offset = pos - arena_index * backing->arena_span_bytes;
	if (arena_offset >= arena->bytes)
		return BLK_STS_IOERR;

	chunk = min_t(u64, bytes, arena->bytes - arena_offset);
	*ptr = (u8 *)arena->data + arena_offset;
	*len = chunk;
	return BLK_STS_OK;
}

static blk_status_t zcbrd_zero_range(const struct zcbrd_backing *backing,
				     u64 pos, u64 bytes)
{
	while (bytes) {
		void *dst;
		size_t len;
		blk_status_t status = zcbrd_backing_chunk(backing, pos, bytes, &dst, &len);

		if (status)
			return status;
		memset(dst, 0, len);
		pos += len;
		bytes -= len;
	}
	return BLK_STS_OK;
}

static blk_status_t zcbrd_transfer_request(struct request *rq,
					   const struct zcbrd_backing *backing,
					   enum zcbrd_data_mode data_mode)
{
	struct req_iterator iter;
	struct bio_vec bvec;
	u64 pos = (u64)blk_rq_pos(rq) << SECTOR_SHIFT;
	u64 bytes = blk_rq_bytes(rq);
	u64 transferred = 0;
	enum req_op op = req_op(rq);
	bool used_nt = false;

	if (op == REQ_OP_FLUSH)
		return BLK_STS_OK;

	if (!backing || pos > backing->capacity_bytes ||
	    bytes > backing->capacity_bytes - pos)
		return BLK_STS_IOERR;

	if (data_mode == ZCBRD_DATA_BYPASS)
		return BLK_STS_OK;

	if (op == REQ_OP_DISCARD || op == REQ_OP_WRITE_ZEROES)
		return zcbrd_zero_range(backing, pos, bytes);

	if (op != REQ_OP_READ && op != REQ_OP_WRITE)
		return BLK_STS_NOTSUPP;

	rq_for_each_segment(bvec, rq, iter) {
		unsigned int len = bvec.bv_len;
		u64 segment_done = 0;
		u8 *mapped;

		if (transferred + len > bytes)
			len = bytes - transferred;
		if (!len)
			break;

		mapped = bvec_kmap_local(&bvec);
		if (op == REQ_OP_WRITE)
			flush_dcache_page(bvec.bv_page);

		while (segment_done < len) {
			void *arena_ptr;
			size_t chunk;
			blk_status_t status = zcbrd_backing_chunk(
				backing, pos + transferred, len - segment_done,
				&arena_ptr, &chunk);

			if (status) {
				kunmap_local(mapped);
				return status;
			}
			if (op == REQ_OP_WRITE) {
				if (data_mode == ZCBRD_DATA_NT_WRITE ||
				    data_mode == ZCBRD_DATA_NT_RW) {
					memcpy_flushcache(arena_ptr,
							  mapped + segment_done,
							  chunk);
					used_nt = true;
				} else {
					memcpy(arena_ptr, mapped + segment_done,
					       chunk);
				}
			} else {
				if (data_mode == ZCBRD_DATA_NT_READ ||
				    data_mode == ZCBRD_DATA_NT_RW) {
					memcpy_flushcache(mapped + segment_done,
							  arena_ptr, chunk);
					used_nt = true;
				} else {
					memcpy(mapped + segment_done, arena_ptr,
					       chunk);
				}
			}

			segment_done += chunk;
			transferred += chunk;
		}
		if (op == REQ_OP_READ)
			flush_dcache_page(bvec.bv_page);
		kunmap_local(mapped);

		if (transferred >= bytes)
			break;
	}

	if (used_nt)
		wmb();

	return transferred == bytes ? BLK_STS_OK : BLK_STS_IOERR;
}

static blk_status_t zcbrd_queue_rq(struct blk_mq_hw_ctx *hctx,
				   const struct blk_mq_queue_data *bd)
{
	struct zcbrd_disk *dev = hctx->queue->queuedata;
	struct request *rq = bd->rq;
	blk_status_t status;

	blk_mq_start_request(rq);
	status = zcbrd_transfer_request(rq, &dev->backing, dev->data_mode);
	blk_mq_end_request(rq, status);
	return BLK_STS_OK;
}

static const struct blk_mq_ops zcbrd_mq_ops = {
	.queue_rq = zcbrd_queue_rq,
};

static const struct block_device_operations zcbrd_fops = {
	.owner = THIS_MODULE,
};

static int zcbrd_create_disk(struct zcbrd_cfg *cfg)
{
	struct queue_limits lim = { };
	struct zcbrd_disk *dev;
	u64 capacity_bytes;
	int ret;

	if (cfg->powered)
		return 0;
	if (cfg->capacity_mib == 0 || cfg->queues == 0 || cfg->queue_depth < 4)
		return -EINVAL;
	if (blk_validate_block_size(cfg->block_size))
		return -EINVAL;
	if (check_mul_overflow(cfg->capacity_mib, (u64)SZ_1M, &capacity_bytes))
		return -EOVERFLOW;
	if (capacity_bytes > SIZE_MAX)
		return -EOVERFLOW;

	dev = kzalloc(sizeof(*dev), GFP_KERNEL);
	if (!dev)
		return -ENOMEM;
	dev->cfg = cfg;
	ret = zcbrd_alloc_backing(&dev->backing, capacity_bytes, cfg->block_size,
				  cfg->shards);
	if (ret)
		goto out_free_dev;
	dev->data_mode = cfg->data_mode;

	dev->tag_set.ops = &zcbrd_mq_ops;
	dev->tag_set.nr_hw_queues = cfg->queues;
	dev->tag_set.queue_depth = cfg->queue_depth;
	dev->tag_set.numa_node = NUMA_NO_NODE;
	dev->tag_set.cmd_size = 0;
	dev->tag_set.flags = BLK_MQ_F_NO_SCHED_BY_DEFAULT;
	dev->tag_set.driver_data = dev;

	ret = blk_mq_alloc_tag_set(&dev->tag_set);
	if (ret)
		goto out_free_backing;

	lim.logical_block_size = cfg->block_size;
	lim.physical_block_size = cfg->block_size;
	lim.io_min = cfg->block_size;
	lim.io_opt = cfg->block_size;
	lim.dma_alignment = cfg->block_size - 1;
	lim.features = BLK_FEAT_SYNCHRONOUS | BLK_FEAT_NOWAIT;
	lim.max_segments = USHRT_MAX;
	lim.max_segment_size = UINT_MAX;
	lim.max_hw_sectors = UINT_MAX >> SECTOR_SHIFT;

	dev->disk = blk_mq_alloc_disk(&dev->tag_set, &lim, dev);
	if (IS_ERR(dev->disk)) {
		ret = PTR_ERR(dev->disk);
		dev->disk = NULL;
		goto out_free_tags;
	}
	dev->disk->flags |= GENHD_FL_NO_PART;
	dev->disk->major = zcbrd_major;
	dev->index = ida_alloc(&zcbrd_indexes, GFP_KERNEL);
	if (dev->index < 0) {
		ret = dev->index;
		goto out_put_disk;
	}
	dev->disk->first_minor = dev->index;
	dev->disk->minors = 1;
	dev->disk->fops = &zcbrd_fops;
	dev->disk->private_data = dev;
	strscpy(dev->disk->disk_name, cfg->name, DISK_NAME_LEN);
	set_capacity(dev->disk, capacity_bytes >> SECTOR_SHIFT);

	ret = add_disk(dev->disk);
	if (ret)
		goto out_free_ida;

	cfg->runtime = dev;
	cfg->powered = true;
	pr_info("zcbrd: disk %s created bytes=%llu queues=%u depth=%u arenas=%u arena_span_bytes=%llu layout=contiguous data_mode=%s\n",
		cfg->name, capacity_bytes, cfg->queues, cfg->queue_depth,
		dev->backing.arena_count, dev->backing.arena_span_bytes,
		dev->data_mode == ZCBRD_DATA_BYPASS ? "bypass" :
		dev->data_mode == ZCBRD_DATA_NT_WRITE ? "nt-write" :
		dev->data_mode == ZCBRD_DATA_NT_READ ? "nt-read" :
		dev->data_mode == ZCBRD_DATA_NT_RW ? "nt-rw" : "copy");
	return 0;

out_free_ida:
	ida_free(&zcbrd_indexes, dev->index);
out_put_disk:
	put_disk(dev->disk);
out_free_tags:
	blk_mq_free_tag_set(&dev->tag_set);
out_free_backing:
	zcbrd_free_backing(&dev->backing);
out_free_dev:
	kfree(dev);
	return ret;
}

static void zcbrd_destroy_disk(struct zcbrd_cfg *cfg)
{
	struct zcbrd_disk *dev = cfg->runtime;

	if (!dev)
		return;
	cfg->runtime = NULL;
	cfg->powered = false;

	del_gendisk(dev->disk);
	ida_free(&zcbrd_indexes, dev->index);
	put_disk(dev->disk);
	blk_mq_free_tag_set(&dev->tag_set);
	zcbrd_free_backing(&dev->backing);
	kfree(dev);
	pr_info("zcbrd: disk %s removed\n", cfg->name);
}

static ssize_t zcbrd_features_show(struct config_item *item, char *page)
{
	return sysfs_emit(page,
			  "power,blocksize,size_mib,queues,queue_depth,shards,descriptor_mode,data_mode,descriptor_abi\n");
}

CONFIGFS_ATTR_RO(zcbrd_, features);

static ssize_t zcbrd_power_show(struct config_item *item, char *page)
{
	struct zcbrd_cfg *cfg = to_zcbrd_cfg(item);
	bool powered;

	mutex_lock(&zcbrd_lock);
	powered = cfg->powered;
	mutex_unlock(&zcbrd_lock);
	return sysfs_emit(page, "%u\n", powered ? 1 : 0);
}

static ssize_t zcbrd_power_store(struct config_item *item, const char *page,
				 size_t count)
{
	struct zcbrd_cfg *cfg = to_zcbrd_cfg(item);
	bool power;
	int ret;

	ret = kstrtobool(page, &power);
	if (ret)
		return ret;

	mutex_lock(&zcbrd_lock);
	if (power)
		ret = zcbrd_create_disk(cfg);
	else
		zcbrd_destroy_disk(cfg);
	mutex_unlock(&zcbrd_lock);

	return ret ? ret : count;
}

CONFIGFS_ATTR(zcbrd_, power);

static ssize_t zcbrd_blocksize_show(struct config_item *item, char *page)
{
	struct zcbrd_cfg *cfg = to_zcbrd_cfg(item);

	return sysfs_emit(page, "%u\n", cfg->block_size);
}

static ssize_t zcbrd_blocksize_store(struct config_item *item, const char *page,
				     size_t count)
{
	struct zcbrd_cfg *cfg = to_zcbrd_cfg(item);
	u32 value;
	int ret;

	ret = kstrtou32(page, 0, &value);
	if (ret)
		return ret;
	if (blk_validate_block_size(value))
		return -EINVAL;

	mutex_lock(&zcbrd_lock);
	if (cfg->powered)
		ret = -EBUSY;
	else
		cfg->block_size = value;
	mutex_unlock(&zcbrd_lock);
	return ret ? ret : count;
}

CONFIGFS_ATTR(zcbrd_, blocksize);

static ssize_t zcbrd_size_mib_show(struct config_item *item, char *page)
{
	struct zcbrd_cfg *cfg = to_zcbrd_cfg(item);

	return sysfs_emit(page, "%llu\n", cfg->capacity_mib);
}

static ssize_t zcbrd_size_mib_store(struct config_item *item, const char *page,
				    size_t count)
{
	struct zcbrd_cfg *cfg = to_zcbrd_cfg(item);
	u64 value;
	int ret;

	ret = kstrtou64(page, 0, &value);
	if (ret)
		return ret;
	if (!value)
		return -EINVAL;

	mutex_lock(&zcbrd_lock);
	if (cfg->powered)
		ret = -EBUSY;
	else
		cfg->capacity_mib = value;
	mutex_unlock(&zcbrd_lock);
	return ret ? ret : count;
}

CONFIGFS_ATTR(zcbrd_, size_mib);

static ssize_t zcbrd_queues_show(struct config_item *item, char *page)
{
	struct zcbrd_cfg *cfg = to_zcbrd_cfg(item);

	return sysfs_emit(page, "%u\n", cfg->queues);
}

static ssize_t zcbrd_queues_store(struct config_item *item, const char *page,
				  size_t count)
{
	struct zcbrd_cfg *cfg = to_zcbrd_cfg(item);
	u32 value;
	int ret;

	ret = kstrtou32(page, 0, &value);
	if (ret)
		return ret;
	if (!value || value > 4096)
		return -EINVAL;

	mutex_lock(&zcbrd_lock);
	if (cfg->powered)
		ret = -EBUSY;
	else
		cfg->queues = value;
	mutex_unlock(&zcbrd_lock);
	return ret ? ret : count;
}

CONFIGFS_ATTR(zcbrd_, queues);

static ssize_t zcbrd_queue_depth_show(struct config_item *item, char *page)
{
	struct zcbrd_cfg *cfg = to_zcbrd_cfg(item);

	return sysfs_emit(page, "%u\n", cfg->queue_depth);
}

static ssize_t zcbrd_queue_depth_store(struct config_item *item, const char *page,
				       size_t count)
{
	struct zcbrd_cfg *cfg = to_zcbrd_cfg(item);
	u32 value;
	int ret;

	ret = kstrtou32(page, 0, &value);
	if (ret)
		return ret;
	if (value < 4 || value > 32768)
		return -EINVAL;

	mutex_lock(&zcbrd_lock);
	if (cfg->powered)
		ret = -EBUSY;
	else
		cfg->queue_depth = value;
	mutex_unlock(&zcbrd_lock);
	return ret ? ret : count;
}

CONFIGFS_ATTR(zcbrd_, queue_depth);

static ssize_t zcbrd_shards_show(struct config_item *item, char *page)
{
	struct zcbrd_cfg *cfg = to_zcbrd_cfg(item);

	return sysfs_emit(page, "%u\n", cfg->shards);
}

static ssize_t zcbrd_shards_store(struct config_item *item, const char *page,
				  size_t count)
{
	struct zcbrd_cfg *cfg = to_zcbrd_cfg(item);
	u32 value;
	int ret;

	ret = kstrtou32(page, 0, &value);
	if (ret)
		return ret;
	if (!value || value > 65536)
		return -EINVAL;

	mutex_lock(&zcbrd_lock);
	if (cfg->powered)
		ret = -EBUSY;
	else
		cfg->shards = value;
	mutex_unlock(&zcbrd_lock);
	return ret ? ret : count;
}

CONFIGFS_ATTR(zcbrd_, shards);

static int zcbrd_parse_descriptor_mode(const char *page,
				       enum zcbrd_descriptor_mode *mode)
{
	char *buf, *text;
	int ret = 0;

	buf = kstrndup(page, PAGE_SIZE, GFP_KERNEL);
	if (!buf)
		return -ENOMEM;
	text = strim(buf);
	if (!strcmp(text, "0") || !strcmp(text, "off") ||
	    !strcmp(text, "false") || !strcmp(text, "disabled"))
		*mode = ZCBRD_DESC_DISABLED;
	else if (!strcmp(text, "1") || !strcmp(text, "on") ||
		 !strcmp(text, "true") || !strcmp(text, "advertise"))
		*mode = ZCBRD_DESC_ADVERTISE;
	else
		ret = -EINVAL;
	kfree(buf);
	return ret;
}

static ssize_t zcbrd_descriptor_mode_show(struct config_item *item, char *page)
{
	struct zcbrd_cfg *cfg = to_zcbrd_cfg(item);

	return sysfs_emit(page, "%u\n", cfg->descriptor_mode == ZCBRD_DESC_ADVERTISE);
}

static ssize_t zcbrd_descriptor_mode_store(struct config_item *item,
					   const char *page, size_t count)
{
	struct zcbrd_cfg *cfg = to_zcbrd_cfg(item);
	enum zcbrd_descriptor_mode mode;
	int ret;

	ret = zcbrd_parse_descriptor_mode(page, &mode);
	if (ret)
		return ret;

	mutex_lock(&zcbrd_lock);
	if (cfg->powered)
		ret = -EBUSY;
	else
		cfg->descriptor_mode = mode;
	mutex_unlock(&zcbrd_lock);
	return ret ? ret : count;
}

CONFIGFS_ATTR(zcbrd_, descriptor_mode);

static ssize_t zcbrd_data_mode_show(struct config_item *item, char *page)
{
	struct zcbrd_cfg *cfg = to_zcbrd_cfg(item);

	return sysfs_emit(page, "%s\n",
			  cfg->data_mode == ZCBRD_DATA_BYPASS ? "bypass" :
			  cfg->data_mode == ZCBRD_DATA_NT_WRITE ? "nt-write" :
			  cfg->data_mode == ZCBRD_DATA_NT_READ ? "nt-read" :
			  cfg->data_mode == ZCBRD_DATA_NT_RW ? "nt-rw" :
			  "copy");
}

static ssize_t zcbrd_data_mode_store(struct config_item *item,
				     const char *page, size_t count)
{
	struct zcbrd_cfg *cfg = to_zcbrd_cfg(item);
	enum zcbrd_data_mode mode;
	int ret = 0;

	if (sysfs_streq(page, "copy") || sysfs_streq(page, "normal"))
		mode = ZCBRD_DATA_COPY;
	else if (sysfs_streq(page, "bypass") || sysfs_streq(page, "nocopy") ||
		 sysfs_streq(page, "no-copy") ||
		 sysfs_streq(page, "zero-copy-bench"))
		mode = ZCBRD_DATA_BYPASS;
	else if (sysfs_streq(page, "nt-write") ||
		 sysfs_streq(page, "nontemporal-write") ||
		 sysfs_streq(page, "non-temporal-write") ||
		 sysfs_streq(page, "movnti-write"))
		mode = ZCBRD_DATA_NT_WRITE;
	else if (sysfs_streq(page, "nt-read") ||
		 sysfs_streq(page, "nontemporal-read") ||
		 sysfs_streq(page, "non-temporal-read") ||
		 sysfs_streq(page, "movnti-read"))
		mode = ZCBRD_DATA_NT_READ;
	else if (sysfs_streq(page, "nt-rw") ||
		 sysfs_streq(page, "nontemporal") ||
		 sysfs_streq(page, "non-temporal") ||
		 sysfs_streq(page, "movnti"))
		mode = ZCBRD_DATA_NT_RW;
	else
		return -EINVAL;

	mutex_lock(&zcbrd_lock);
	if (cfg->powered)
		ret = -EBUSY;
	else
		cfg->data_mode = mode;
	mutex_unlock(&zcbrd_lock);
	return ret ? ret : count;
}

CONFIGFS_ATTR(zcbrd_, data_mode);

static ssize_t zcbrd_descriptor_abi_show(struct config_item *item, char *page)
{
	struct zcbrd_cfg *cfg = to_zcbrd_cfg(item);
	u32 features = ZCBRD_DESC_F_TOPOLOGY_HINTS |
		       ZCBRD_DESC_F_RELEASE_TOKEN |
		       ZCBRD_DESC_F_BLOCK_EXTENTS;

	return sysfs_emit(page,
			  "magic=0x%016llx\nversion=%u\nmode=%u\nfeatures=0x%08x\nslice_desc=%zu\nrecord_desc=%zu\nbatch=%zu\nqueues=%u\nshards=%u\n",
			  ZCBRD_DESC_MAGIC, ZCBRD_DESC_VERSION,
			  cfg->descriptor_mode == ZCBRD_DESC_ADVERTISE,
			  features, sizeof(struct zcbrd_slice_desc),
			  sizeof(struct zcbrd_record_desc),
			  sizeof(struct zcbrd_desc_batch),
			  cfg->queues, cfg->shards);
}

CONFIGFS_ATTR_RO(zcbrd_, descriptor_abi);

static struct configfs_attribute *zcbrd_device_attrs[] = {
	&zcbrd_attr_power,
	&zcbrd_attr_blocksize,
	&zcbrd_attr_size_mib,
	&zcbrd_attr_queues,
	&zcbrd_attr_queue_depth,
	&zcbrd_attr_shards,
	&zcbrd_attr_descriptor_mode,
	&zcbrd_attr_data_mode,
	&zcbrd_attr_descriptor_abi,
	NULL,
};

static void zcbrd_device_release(struct config_item *item)
{
	kfree(to_zcbrd_cfg(item));
}

static const struct configfs_item_operations zcbrd_device_ops = {
	.release = zcbrd_device_release,
};

static const struct config_item_type zcbrd_device_type = {
	.ct_item_ops = &zcbrd_device_ops,
	.ct_attrs = zcbrd_device_attrs,
	.ct_owner = THIS_MODULE,
};

static struct config_group *zcbrd_make_group(struct config_group *group,
					     const char *name)
{
	struct zcbrd_cfg *cfg;

	if (!name || !*name || strlen(name) >= DISK_NAME_LEN)
		return ERR_PTR(-EINVAL);

	cfg = kzalloc(sizeof(*cfg), GFP_KERNEL);
	if (!cfg)
		return ERR_PTR(-ENOMEM);

	strscpy(cfg->name, name, sizeof(cfg->name));
	cfg->block_size = 4096;
	cfg->capacity_mib = 64;
	cfg->queues = 4;
	cfg->queue_depth = 256;
	cfg->shards = 4;
	cfg->descriptor_mode = ZCBRD_DESC_DISABLED;
	cfg->data_mode = ZCBRD_DATA_COPY;

	config_group_init_type_name(&cfg->group, name, &zcbrd_device_type);
	return &cfg->group;
}

static void zcbrd_drop_item(struct config_group *group, struct config_item *item)
{
	struct zcbrd_cfg *cfg = to_zcbrd_cfg(item);

	mutex_lock(&zcbrd_lock);
	zcbrd_destroy_disk(cfg);
	mutex_unlock(&zcbrd_lock);
	config_item_put(item);
}

static struct configfs_attribute *zcbrd_group_attrs[] = {
	&zcbrd_attr_features,
	NULL,
};

static const struct configfs_group_operations zcbrd_group_ops = {
	.make_group = zcbrd_make_group,
	.drop_item = zcbrd_drop_item,
};

static const struct config_item_type zcbrd_group_type = {
	.ct_group_ops = &zcbrd_group_ops,
	.ct_attrs = zcbrd_group_attrs,
	.ct_owner = THIS_MODULE,
};

static struct configfs_subsystem zcbrd_subsys = {
	.su_group = {
		.cg_item = {
			.ci_namebuf = "zcbrd",
			.ci_type = &zcbrd_group_type,
		},
	},
};

static int __init zcbrd_init(void)
{
	int ret;

	zcbrd_major = register_blkdev(0, "zcbrd");
	if (zcbrd_major < 0)
		return zcbrd_major;

	config_group_init(&zcbrd_subsys.su_group);
	mutex_init(&zcbrd_subsys.su_mutex);
	ret = configfs_register_subsystem(&zcbrd_subsys);
	if (ret) {
		unregister_blkdev(zcbrd_major, "zcbrd");
		return ret;
	}

	pr_info("zcbrd: C module loaded\n");
	return 0;
}

static void __exit zcbrd_exit(void)
{
	configfs_unregister_subsystem(&zcbrd_subsys);
	unregister_blkdev(zcbrd_major, "zcbrd");
	ida_destroy(&zcbrd_indexes);
	pr_info("zcbrd: C module unloaded\n");
}

module_init(zcbrd_init);
module_exit(zcbrd_exit);

MODULE_AUTHOR("Rob Caskey, OpenAI");
MODULE_DESCRIPTION("Zero-copy friendly RAM block device");
MODULE_LICENSE("GPL");
