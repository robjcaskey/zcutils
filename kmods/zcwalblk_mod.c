// SPDX-License-Identifier: GPL-2.0

#include <linux/blk-mq.h>
#include <linux/blk_types.h>
#include <linux/blkdev.h>
#include <linux/bvec.h>
#include <linux/configfs.h>
#include <linux/errno.h>
#include <linux/highmem.h>
#include <linux/idr.h>
#include <linux/io_uring/cmd.h>
#include <linux/kernel.h>
#include <linux/list.h>
#include <linux/math64.h>
#include <linux/miscdevice.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/overflow.h>
#include <linux/rculist.h>
#include <linux/slab.h>
#include <linux/string.h>
#include <linux/uaccess.h>

#define ZCWALBLK_DESC_MAGIC 0x4b4c424c4157435aULL
#define ZCWALBLK_DESC_VERSION 1
#define ZCWALBLK_DESC_F_TOPOLOGY_HINTS (1U << 0)
#define ZCWALBLK_DESC_F_BLOCK_EXTENTS (1U << 1)
#define ZCWALBLK_DESC_F_COMPOSITE_WAL (1U << 2)

#define ZCWALBLK_URING_MAGIC 0x31444d43574c415aULL
#define ZCWALBLK_URING_VERSION 1
#define ZCWALBLK_URING_OP_RESOLVE_BATCH 1U
#define ZCWALBLK_URING_DEFAULT_DISK U32_MAX
#define ZCWALBLK_URING_MAX_ITEMS (1U << 20)
#define ZCWALBLK_URING_MAX_RECORDS_PER_ITEM 4096U

struct zcwalblk_uring_batch_cmd {
	u64 magic;
	u32 version;
	u32 flags;
	u32 disk_index;
	u32 count;
	u32 records_per_item;
	u32 reserved;
	u64 start_record;
	u64 stride_records;
	u64 result_addr;
	u64 result_len;
};

struct zcwalblk_uring_batch_result {
	u64 checksum;
	u64 items;
	u64 records;
	u64 logical_records;
};

static inline const struct zcwalblk_uring_batch_cmd *
zcwalblk_sqe_batch_cmd(const struct io_uring_sqe *sqe)
{
#ifdef io_uring_sqe128_cmd
	return io_uring_sqe128_cmd(sqe, struct zcwalblk_uring_batch_cmd);
#elif defined(io_uring_sqe_cmd)
	return io_uring_sqe_cmd(sqe, struct zcwalblk_uring_batch_cmd);
#else
	return (const struct zcwalblk_uring_batch_cmd *)io_uring_sqe_cmd(sqe);
#endif
}

struct zcwalblk_extent {
	u32 lane_id;
	u32 records;
	u64 sequence;
	u64 offset_records;
	u64 checksum;
};

struct zcwalblk_composite {
	u32 left_index;
	u32 right_index;
	u32 records;
	u32 reserved;
	u64 base_record;
	u64 checksum;
};

struct zcwalblk_runtime {
	struct zcwalblk_extent *left_stream;
	struct zcwalblk_extent *right_stream;
	struct zcwalblk_composite *composites;
	u32 record_bytes;
	u32 extent_bytes;
	u32 records_per_extent;
	u32 records_per_composite;
	u32 lanes;
	u64 pairs;
	u64 capacity_bytes;
	u64 logical_records;
	u64 logical_bytes;
	u64 build_checksum;
};

struct zcwalblk_cfg;

struct zcwalblk_disk {
	struct zcwalblk_cfg *cfg;
	struct list_head list;
	struct blk_mq_tag_set tag_set;
	struct gendisk *disk;
	struct zcwalblk_runtime wal;
	bool write_ack;
	int index;
};

struct zcwalblk_cfg {
	struct config_group group;
	bool powered;
	char name[DISK_NAME_LEN];
	u32 block_size;
	u32 record_bytes;
	u32 extent_bytes;
	u64 capacity_mib;
	u32 queues;
	u32 queue_depth;
	u32 lanes;
	bool write_ack;
	struct zcwalblk_disk *runtime;
};

static DEFINE_MUTEX(zcwalblk_lock);
static DEFINE_IDA(zcwalblk_indexes);
static LIST_HEAD(zcwalblk_disks);
static struct zcwalblk_disk __rcu *zcwalblk_ctl_default;
static int zcwalblk_major;
static struct miscdevice zcwalblk_ctl_misc;

static inline struct zcwalblk_cfg *to_zcwalblk_cfg(struct config_item *item)
{
	return item ? container_of(to_config_group(item), struct zcwalblk_cfg, group) : NULL;
}

static inline u64 zcwalblk_hash(u64 value)
{
	value += 0x9e3779b97f4a7c15ULL;
	value = (value ^ (value >> 30)) * 0xbf58476d1ce4e5b9ULL;
	value = (value ^ (value >> 27)) * 0x94d049bb133111ebULL;
	return value ^ (value >> 31);
}

static void zcwalblk_wal_destroy(struct zcwalblk_runtime *wal)
{
	kvfree(wal->left_stream);
	kvfree(wal->right_stream);
	kvfree(wal->composites);
	memset(wal, 0, sizeof(*wal));
}

static int zcwalblk_wal_create(struct zcwalblk_cfg *cfg, u64 capacity_bytes,
			       struct zcwalblk_runtime *wal)
{
	u64 pair_bytes;
	u64 pairs;
	u64 logical_records;
	u64 logical_bytes;
	u32 records_per_extent;
	u32 records_per_composite;
	u32 lanes;
	u64 i;

	memset(wal, 0, sizeof(*wal));

	if (!cfg->record_bytes || !cfg->extent_bytes || !cfg->lanes)
		return -EINVAL;
	if (cfg->extent_bytes < cfg->record_bytes ||
	    cfg->extent_bytes % cfg->record_bytes)
		return -EINVAL;
	if (check_mul_overflow((u64)cfg->extent_bytes, 2ULL, &pair_bytes))
		return -EOVERFLOW;
	if (!pair_bytes)
		return -EINVAL;

	records_per_extent = cfg->extent_bytes / cfg->record_bytes;
	if (!records_per_extent ||
	    check_mul_overflow(records_per_extent, 2U,
			       &records_per_composite))
		return -EOVERFLOW;

	pairs = DIV_ROUND_UP_ULL(capacity_bytes, pair_bytes);
	if (!pairs || pairs > U32_MAX)
		return -EOVERFLOW;
	if (check_mul_overflow(pairs, (u64)records_per_composite,
			       &logical_records))
		return -EOVERFLOW;
	if (check_mul_overflow(logical_records, (u64)cfg->record_bytes,
			       &logical_bytes))
		return -EOVERFLOW;

	wal->left_stream = kvcalloc(pairs, sizeof(*wal->left_stream), GFP_KERNEL);
	wal->right_stream = kvcalloc(pairs, sizeof(*wal->right_stream), GFP_KERNEL);
	wal->composites = kvcalloc(pairs, sizeof(*wal->composites), GFP_KERNEL);
	if (!wal->left_stream || !wal->right_stream || !wal->composites) {
		zcwalblk_wal_destroy(wal);
		return -ENOMEM;
	}

	lanes = cfg->lanes;
	for (i = 0; i < pairs; i++) {
		u64 base_record = i * records_per_composite;
		u64 left_checksum;
		u64 right_checksum;
		u64 checksum;

		left_checksum = zcwalblk_hash(i ^
					      ((u64)records_per_extent << 32) ^
					      ((i % lanes) << 48));
		right_checksum = zcwalblk_hash(i ^ 0xd1b54a32d192ed03ULL ^
					       ((u64)records_per_extent << 32) ^
					       (((i + 1) % lanes) << 48));
		wal->left_stream[i].lane_id = i % lanes;
		wal->left_stream[i].records = records_per_extent;
		wal->left_stream[i].sequence = i;
		wal->left_stream[i].offset_records = base_record;
		wal->left_stream[i].checksum = left_checksum;

		wal->right_stream[i].lane_id = (i + 1) % lanes;
		wal->right_stream[i].records = records_per_extent;
		wal->right_stream[i].sequence = i;
		wal->right_stream[i].offset_records =
			base_record + records_per_extent;
		wal->right_stream[i].checksum = right_checksum;

		checksum = left_checksum + right_checksum;
		wal->composites[i].left_index = i;
		wal->composites[i].right_index = i;
		wal->composites[i].records = records_per_composite;
		wal->composites[i].base_record = base_record;
		wal->composites[i].checksum = checksum;
		wal->build_checksum += checksum;
	}

	wal->record_bytes = cfg->record_bytes;
	wal->extent_bytes = cfg->extent_bytes;
	wal->records_per_extent = records_per_extent;
	wal->records_per_composite = records_per_composite;
	wal->lanes = lanes;
	wal->pairs = pairs;
	wal->capacity_bytes = capacity_bytes;
	wal->logical_records = logical_records;
	wal->logical_bytes = logical_bytes;
	return 0;
}

static inline u64 zcwalblk_resolve_record(const struct zcwalblk_runtime *wal,
					  u64 record)
{
	const struct zcwalblk_composite *composite;
	const struct zcwalblk_extent *extent;
	u32 within_composite;
	u32 local_record;
	u64 composite_index;
	u64 absolute_record;
	u64 stream_bit;

	composite_index = div_u64_rem(record, wal->records_per_composite,
				      &within_composite);
	if (unlikely(composite_index >= wal->pairs))
		composite_index %= wal->pairs;

	composite = &wal->composites[composite_index];
	if (within_composite < wal->records_per_extent) {
		extent = &wal->left_stream[composite->left_index];
		local_record = within_composite;
		stream_bit = 0;
	} else {
		extent = &wal->right_stream[composite->right_index];
		local_record = within_composite - wal->records_per_extent;
		stream_bit = 1;
	}

	absolute_record = extent->offset_records + local_record;
	return composite->checksum ^
	       rol64(extent->checksum, local_record & 31) ^
	       zcwalblk_hash(absolute_record ^
			     rol64(composite->base_record, 7) ^
			     rol64((u64)wal->record_bytes, 13) ^
			     rol64((u64)extent->lane_id, 19) ^
			     rol64(extent->sequence, 23) ^
			     stream_bit);
}

static struct zcwalblk_disk *zcwalblk_find_disk_rcu(u32 disk_index)
{
	struct zcwalblk_disk *dev;

	if (disk_index == ZCWALBLK_URING_DEFAULT_DISK)
		return rcu_dereference(zcwalblk_ctl_default);

	list_for_each_entry_rcu(dev, &zcwalblk_disks, list) {
		if ((u32)dev->index == disk_index)
			return dev;
	}

	return NULL;
}

static int zcwalblk_ctl_uring_cmd(struct io_uring_cmd *ioucmd,
				  unsigned int issue_flags)
{
	const struct zcwalblk_uring_batch_cmd *cmd;
	struct zcwalblk_uring_batch_result result = { };
	struct zcwalblk_disk *dev;
	u64 checksum = 0;
	u64 records;
	u64 record;
	u32 i, j;
	int ret = 0;

	(void)issue_flags;

	if (ioucmd->cmd_op != ZCWALBLK_URING_OP_RESOLVE_BATCH)
		return -EOPNOTSUPP;

	cmd = zcwalblk_sqe_batch_cmd(ioucmd->sqe);
	if (cmd->magic != ZCWALBLK_URING_MAGIC ||
	    cmd->version != ZCWALBLK_URING_VERSION)
		return -EINVAL;
	if (!cmd->count || cmd->count > ZCWALBLK_URING_MAX_ITEMS ||
	    !cmd->records_per_item ||
	    cmd->records_per_item > ZCWALBLK_URING_MAX_RECORDS_PER_ITEM)
		return -EINVAL;
	if (check_mul_overflow((u64)cmd->count,
			       (u64)cmd->records_per_item, &records))
		return -EOVERFLOW;
	if (cmd->result_addr &&
	    cmd->result_len < sizeof(struct zcwalblk_uring_batch_result))
		return -EINVAL;

	rcu_read_lock();
	dev = zcwalblk_find_disk_rcu(cmd->disk_index);
	if (!dev) {
		ret = -ENODEV;
		goto out_rcu;
	}

	for (i = 0; i < cmd->count; i++) {
		u64 base;

		if (check_mul_overflow((u64)i, cmd->stride_records, &base) ||
		    check_add_overflow(cmd->start_record, base, &base)) {
			ret = -EOVERFLOW;
			goto out_rcu;
		}

		for (j = 0; j < cmd->records_per_item; j++) {
			if (check_add_overflow(base, (u64)j, &record)) {
				ret = -EOVERFLOW;
				goto out_rcu;
			}

			checksum ^= rol64(zcwalblk_resolve_record(&dev->wal, record),
					  (record + i + j) & 63);
		}
	}

	result.checksum = checksum;
	result.items = cmd->count;
	result.records = records;
	result.logical_records = dev->wal.logical_records;

out_rcu:
	rcu_read_unlock();
	if (ret)
		return ret;

	if (cmd->result_addr &&
	    copy_to_user(u64_to_user_ptr(cmd->result_addr), &result,
			 sizeof(result)))
		return -EFAULT;

	return records > INT_MAX ? INT_MAX : (int)records;
}

static const struct file_operations zcwalblk_ctl_fops = {
	.owner = THIS_MODULE,
	.uring_cmd = zcwalblk_ctl_uring_cmd,
};

static void zcwalblk_fill_seed(u8 *dst, unsigned int len, u64 seed,
			       u32 record_offset)
{
	while (len >= sizeof(u64)) {
		memcpy(dst, &seed, sizeof(seed));
		dst += sizeof(u64);
		record_offset += sizeof(u64);
		len -= sizeof(u64);
	}

	while (len) {
		*dst++ = (u8)(seed >> ((record_offset & 7) * 8));
		record_offset++;
		len--;
	}
}

static void zcwalblk_fill_bytes(const struct zcwalblk_runtime *wal, u8 *dst,
				u64 byte_pos, unsigned int len)
{
	while (len) {
		u32 record_offset;
		u64 record = div_u64_rem(byte_pos, wal->record_bytes,
					 &record_offset);
		unsigned int todo = min_t(unsigned int, len,
					  wal->record_bytes - record_offset);
		u64 seed = zcwalblk_resolve_record(wal, record);

		zcwalblk_fill_seed(dst, todo, seed, record_offset);
		dst += todo;
		byte_pos += todo;
		len -= todo;
	}
}

static blk_status_t zcwalblk_transfer_request(struct request *rq,
					      struct zcwalblk_disk *dev)
{
	struct req_iterator iter;
	struct bio_vec bvec;
	u64 pos = (u64)blk_rq_pos(rq) << SECTOR_SHIFT;
	u64 bytes = blk_rq_bytes(rq);
	u64 transferred = 0;
	enum req_op op = req_op(rq);

	if (op == REQ_OP_FLUSH)
		return BLK_STS_OK;

	if (pos > dev->wal.capacity_bytes || bytes > dev->wal.capacity_bytes - pos)
		return BLK_STS_IOERR;

	if (op == REQ_OP_DISCARD || op == REQ_OP_WRITE_ZEROES)
		return dev->write_ack ? BLK_STS_OK : BLK_STS_NOTSUPP;

	if (op == REQ_OP_WRITE)
		return dev->write_ack ? BLK_STS_OK : BLK_STS_NOTSUPP;
	if (op != REQ_OP_READ)
		return BLK_STS_NOTSUPP;

	rq_for_each_segment(bvec, rq, iter) {
		unsigned int len = bvec.bv_len;
		void *mapped;

		if (transferred + len > bytes)
			len = bytes - transferred;
		if (!len)
			break;

		mapped = bvec_kmap_local(&bvec);
		zcwalblk_fill_bytes(&dev->wal, mapped, pos + transferred, len);
		flush_dcache_page(bvec.bv_page);
		kunmap_local(mapped);

		transferred += len;
		if (transferred >= bytes)
			break;
	}

	return transferred == bytes ? BLK_STS_OK : BLK_STS_IOERR;
}

static blk_status_t zcwalblk_queue_rq(struct blk_mq_hw_ctx *hctx,
				      const struct blk_mq_queue_data *bd)
{
	struct zcwalblk_disk *dev = hctx->queue->queuedata;
	struct request *rq = bd->rq;
	blk_status_t status;

	blk_mq_start_request(rq);
	status = zcwalblk_transfer_request(rq, dev);
	blk_mq_end_request(rq, status);
	return BLK_STS_OK;
}

static const struct blk_mq_ops zcwalblk_mq_ops = {
	.queue_rq = zcwalblk_queue_rq,
};

static const struct block_device_operations zcwalblk_fops = {
	.owner = THIS_MODULE,
};

static int zcwalblk_create_disk(struct zcwalblk_cfg *cfg)
{
	struct queue_limits lim = { };
	struct zcwalblk_disk *dev;
	u64 capacity_bytes;
	int ret;

	if (cfg->powered)
		return 0;
	if (cfg->capacity_mib == 0 || cfg->queues == 0 ||
	    cfg->queue_depth < 4 || cfg->lanes == 0)
		return -EINVAL;
	if (blk_validate_block_size(cfg->block_size) ||
	    blk_validate_block_size(cfg->record_bytes))
		return -EINVAL;
	if (cfg->record_bytes < cfg->block_size ||
	    cfg->extent_bytes < cfg->record_bytes ||
	    cfg->extent_bytes % cfg->record_bytes)
		return -EINVAL;
	if (check_mul_overflow(cfg->capacity_mib, (u64)SZ_1M, &capacity_bytes))
		return -EOVERFLOW;
	capacity_bytes = round_down(capacity_bytes, (u64)cfg->block_size);
	if (!capacity_bytes)
		return -EINVAL;

	dev = kzalloc(sizeof(*dev), GFP_KERNEL);
	if (!dev)
		return -ENOMEM;
	INIT_LIST_HEAD(&dev->list);
	dev->cfg = cfg;
	dev->write_ack = cfg->write_ack;

	ret = zcwalblk_wal_create(cfg, capacity_bytes, &dev->wal);
	if (ret)
		goto out_free_dev;

	dev->tag_set.ops = &zcwalblk_mq_ops;
	dev->tag_set.nr_hw_queues = cfg->queues;
	dev->tag_set.queue_depth = cfg->queue_depth;
	dev->tag_set.numa_node = NUMA_NO_NODE;
	dev->tag_set.cmd_size = 0;
	dev->tag_set.flags = BLK_MQ_F_NO_SCHED_BY_DEFAULT;
	dev->tag_set.driver_data = dev;

	ret = blk_mq_alloc_tag_set(&dev->tag_set);
	if (ret)
		goto out_destroy_wal;

	lim.logical_block_size = cfg->block_size;
	lim.physical_block_size = cfg->block_size;
	lim.io_min = cfg->block_size;
	lim.io_opt = cfg->extent_bytes;
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
	dev->disk->major = zcwalblk_major;
	dev->index = ida_alloc(&zcwalblk_indexes, GFP_KERNEL);
	if (dev->index < 0) {
		ret = dev->index;
		goto out_put_disk;
	}
	dev->disk->first_minor = dev->index;
	dev->disk->minors = 1;
	dev->disk->fops = &zcwalblk_fops;
	dev->disk->private_data = dev;
	strscpy(dev->disk->disk_name, cfg->name, DISK_NAME_LEN);
	set_capacity(dev->disk, capacity_bytes >> SECTOR_SHIFT);
	set_disk_ro(dev->disk, !dev->write_ack);

	ret = add_disk(dev->disk);
	if (ret)
		goto out_free_ida;

	list_add_tail_rcu(&dev->list, &zcwalblk_disks);
	if (!rcu_access_pointer(zcwalblk_ctl_default))
		rcu_assign_pointer(zcwalblk_ctl_default, dev);
	cfg->runtime = dev;
	cfg->powered = true;
	pr_info("zcwalblk: disk %s created bytes=%llu pairs=%llu extent=%u record=%u lanes=%u queues=%u depth=%u write_mode=%s checksum=%llu\n",
		cfg->name, capacity_bytes, dev->wal.pairs, cfg->extent_bytes,
		cfg->record_bytes, cfg->lanes, cfg->queues, cfg->queue_depth,
		dev->write_ack ? "ack" : "reject", dev->wal.build_checksum);
	return 0;

out_free_ida:
	ida_free(&zcwalblk_indexes, dev->index);
out_put_disk:
	put_disk(dev->disk);
out_free_tags:
	blk_mq_free_tag_set(&dev->tag_set);
out_destroy_wal:
	zcwalblk_wal_destroy(&dev->wal);
out_free_dev:
	kfree(dev);
	return ret;
}

static void zcwalblk_destroy_disk(struct zcwalblk_cfg *cfg)
{
	struct zcwalblk_disk *dev = cfg->runtime;
	struct zcwalblk_disk *next_default = NULL;

	if (!dev)
		return;
	cfg->runtime = NULL;
	cfg->powered = false;
	list_del_rcu(&dev->list);
	if (rcu_access_pointer(zcwalblk_ctl_default) == dev) {
		next_default = list_first_entry_or_null(&zcwalblk_disks,
							struct zcwalblk_disk,
							list);
		rcu_assign_pointer(zcwalblk_ctl_default, next_default);
	}
	synchronize_rcu();

	del_gendisk(dev->disk);
	ida_free(&zcwalblk_indexes, dev->index);
	put_disk(dev->disk);
	blk_mq_free_tag_set(&dev->tag_set);
	zcwalblk_wal_destroy(&dev->wal);
	kfree(dev);
	pr_info("zcwalblk: disk %s removed\n", cfg->name);
}

static ssize_t zcwalblk_features_show(struct config_item *item, char *page)
{
	return sysfs_emit(page,
			  "power,blocksize,size_mib,queues,queue_depth,lanes,record_bytes,extent_bytes,write_mode,descriptor_abi,uring_cmd_batch_resolve=/dev/zcwalctl\n");
}

CONFIGFS_ATTR_RO(zcwalblk_, features);

static ssize_t zcwalblk_power_show(struct config_item *item, char *page)
{
	struct zcwalblk_cfg *cfg = to_zcwalblk_cfg(item);
	bool powered;

	mutex_lock(&zcwalblk_lock);
	powered = cfg->powered;
	mutex_unlock(&zcwalblk_lock);
	return sysfs_emit(page, "%u\n", powered ? 1 : 0);
}

static ssize_t zcwalblk_power_store(struct config_item *item, const char *page,
				    size_t count)
{
	struct zcwalblk_cfg *cfg = to_zcwalblk_cfg(item);
	bool power;
	int ret;

	ret = kstrtobool(page, &power);
	if (ret)
		return ret;

	mutex_lock(&zcwalblk_lock);
	if (power)
		ret = zcwalblk_create_disk(cfg);
	else
		zcwalblk_destroy_disk(cfg);
	mutex_unlock(&zcwalblk_lock);

	return ret ? ret : count;
}

CONFIGFS_ATTR(zcwalblk_, power);

static ssize_t zcwalblk_blocksize_show(struct config_item *item, char *page)
{
	struct zcwalblk_cfg *cfg = to_zcwalblk_cfg(item);

	return sysfs_emit(page, "%u\n", cfg->block_size);
}

static ssize_t zcwalblk_blocksize_store(struct config_item *item,
					const char *page, size_t count)
{
	struct zcwalblk_cfg *cfg = to_zcwalblk_cfg(item);
	u32 value;
	int ret;

	ret = kstrtou32(page, 0, &value);
	if (ret)
		return ret;
	if (blk_validate_block_size(value))
		return -EINVAL;

	mutex_lock(&zcwalblk_lock);
	if (cfg->powered)
		ret = -EBUSY;
	else
		cfg->block_size = value;
	mutex_unlock(&zcwalblk_lock);
	return ret ? ret : count;
}

CONFIGFS_ATTR(zcwalblk_, blocksize);

static ssize_t zcwalblk_size_mib_show(struct config_item *item, char *page)
{
	struct zcwalblk_cfg *cfg = to_zcwalblk_cfg(item);

	return sysfs_emit(page, "%llu\n", cfg->capacity_mib);
}

static ssize_t zcwalblk_size_mib_store(struct config_item *item,
				       const char *page, size_t count)
{
	struct zcwalblk_cfg *cfg = to_zcwalblk_cfg(item);
	u64 value;
	int ret;

	ret = kstrtou64(page, 0, &value);
	if (ret)
		return ret;
	if (!value)
		return -EINVAL;

	mutex_lock(&zcwalblk_lock);
	if (cfg->powered)
		ret = -EBUSY;
	else
		cfg->capacity_mib = value;
	mutex_unlock(&zcwalblk_lock);
	return ret ? ret : count;
}

CONFIGFS_ATTR(zcwalblk_, size_mib);

static ssize_t zcwalblk_queues_show(struct config_item *item, char *page)
{
	struct zcwalblk_cfg *cfg = to_zcwalblk_cfg(item);

	return sysfs_emit(page, "%u\n", cfg->queues);
}

static ssize_t zcwalblk_queues_store(struct config_item *item, const char *page,
				     size_t count)
{
	struct zcwalblk_cfg *cfg = to_zcwalblk_cfg(item);
	u32 value;
	int ret;

	ret = kstrtou32(page, 0, &value);
	if (ret)
		return ret;
	if (!value || value > 4096)
		return -EINVAL;

	mutex_lock(&zcwalblk_lock);
	if (cfg->powered)
		ret = -EBUSY;
	else
		cfg->queues = value;
	mutex_unlock(&zcwalblk_lock);
	return ret ? ret : count;
}

CONFIGFS_ATTR(zcwalblk_, queues);

static ssize_t zcwalblk_queue_depth_show(struct config_item *item, char *page)
{
	struct zcwalblk_cfg *cfg = to_zcwalblk_cfg(item);

	return sysfs_emit(page, "%u\n", cfg->queue_depth);
}

static ssize_t zcwalblk_queue_depth_store(struct config_item *item,
					  const char *page, size_t count)
{
	struct zcwalblk_cfg *cfg = to_zcwalblk_cfg(item);
	u32 value;
	int ret;

	ret = kstrtou32(page, 0, &value);
	if (ret)
		return ret;
	if (value < 4 || value > 32768)
		return -EINVAL;

	mutex_lock(&zcwalblk_lock);
	if (cfg->powered)
		ret = -EBUSY;
	else
		cfg->queue_depth = value;
	mutex_unlock(&zcwalblk_lock);
	return ret ? ret : count;
}

CONFIGFS_ATTR(zcwalblk_, queue_depth);

static ssize_t zcwalblk_lanes_show(struct config_item *item, char *page)
{
	struct zcwalblk_cfg *cfg = to_zcwalblk_cfg(item);

	return sysfs_emit(page, "%u\n", cfg->lanes);
}

static ssize_t zcwalblk_lanes_store(struct config_item *item, const char *page,
				    size_t count)
{
	struct zcwalblk_cfg *cfg = to_zcwalblk_cfg(item);
	u32 value;
	int ret;

	ret = kstrtou32(page, 0, &value);
	if (ret)
		return ret;
	if (!value || value > 65536)
		return -EINVAL;

	mutex_lock(&zcwalblk_lock);
	if (cfg->powered)
		ret = -EBUSY;
	else
		cfg->lanes = value;
	mutex_unlock(&zcwalblk_lock);
	return ret ? ret : count;
}

CONFIGFS_ATTR(zcwalblk_, lanes);

static ssize_t zcwalblk_record_bytes_show(struct config_item *item, char *page)
{
	struct zcwalblk_cfg *cfg = to_zcwalblk_cfg(item);

	return sysfs_emit(page, "%u\n", cfg->record_bytes);
}

static ssize_t zcwalblk_record_bytes_store(struct config_item *item,
					   const char *page, size_t count)
{
	struct zcwalblk_cfg *cfg = to_zcwalblk_cfg(item);
	u32 value;
	int ret;

	ret = kstrtou32(page, 0, &value);
	if (ret)
		return ret;
	if (blk_validate_block_size(value))
		return -EINVAL;

	mutex_lock(&zcwalblk_lock);
	if (cfg->powered)
		ret = -EBUSY;
	else
		cfg->record_bytes = value;
	mutex_unlock(&zcwalblk_lock);
	return ret ? ret : count;
}

CONFIGFS_ATTR(zcwalblk_, record_bytes);

static ssize_t zcwalblk_extent_bytes_show(struct config_item *item, char *page)
{
	struct zcwalblk_cfg *cfg = to_zcwalblk_cfg(item);

	return sysfs_emit(page, "%u\n", cfg->extent_bytes);
}

static ssize_t zcwalblk_extent_bytes_store(struct config_item *item,
					   const char *page, size_t count)
{
	struct zcwalblk_cfg *cfg = to_zcwalblk_cfg(item);
	u32 value;
	int ret;

	ret = kstrtou32(page, 0, &value);
	if (ret)
		return ret;
	if (!value)
		return -EINVAL;

	mutex_lock(&zcwalblk_lock);
	if (cfg->powered)
		ret = -EBUSY;
	else
		cfg->extent_bytes = value;
	mutex_unlock(&zcwalblk_lock);
	return ret ? ret : count;
}

CONFIGFS_ATTR(zcwalblk_, extent_bytes);

static int zcwalblk_parse_write_mode(const char *page, bool *write_ack)
{
	char *buf, *text;
	int ret = 0;

	buf = kstrndup(page, PAGE_SIZE, GFP_KERNEL);
	if (!buf)
		return -ENOMEM;
	text = strim(buf);
	if (!strcmp(text, "reject") || !strcmp(text, "readonly") ||
	    !strcmp(text, "read-only") || !strcmp(text, "0") ||
	    !strcmp(text, "false"))
		*write_ack = false;
	else if (!strcmp(text, "ack") || !strcmp(text, "synthetic-ack") ||
		 !strcmp(text, "1") || !strcmp(text, "true"))
		*write_ack = true;
	else
		ret = -EINVAL;
	kfree(buf);
	return ret;
}

static ssize_t zcwalblk_write_mode_show(struct config_item *item, char *page)
{
	struct zcwalblk_cfg *cfg = to_zcwalblk_cfg(item);

	return sysfs_emit(page, "%s\n", cfg->write_ack ? "ack" : "reject");
}

static ssize_t zcwalblk_write_mode_store(struct config_item *item,
					 const char *page, size_t count)
{
	struct zcwalblk_cfg *cfg = to_zcwalblk_cfg(item);
	bool write_ack;
	int ret;

	ret = zcwalblk_parse_write_mode(page, &write_ack);
	if (ret)
		return ret;

	mutex_lock(&zcwalblk_lock);
	if (cfg->powered)
		ret = -EBUSY;
	else
		cfg->write_ack = write_ack;
	mutex_unlock(&zcwalblk_lock);
	return ret ? ret : count;
}

CONFIGFS_ATTR(zcwalblk_, write_mode);

static ssize_t zcwalblk_descriptor_abi_show(struct config_item *item, char *page)
{
	struct zcwalblk_cfg *cfg = to_zcwalblk_cfg(item);
	u32 features = ZCWALBLK_DESC_F_TOPOLOGY_HINTS |
		       ZCWALBLK_DESC_F_BLOCK_EXTENTS |
		       ZCWALBLK_DESC_F_COMPOSITE_WAL;
	struct zcwalblk_disk *runtime;
	ssize_t ret;

	mutex_lock(&zcwalblk_lock);
	runtime = cfg->runtime;
	ret = sysfs_emit(page,
			 "magic=0x%016llx\nversion=%u\nfeatures=0x%08x\nextent_desc=%zu\ncomposite_desc=%zu\nblocksize=%u\nrecord_bytes=%u\nextent_bytes=%u\nlanes=%u\npowered=%u\npairs=%llu\nlogical_records=%llu\nlogical_bytes=%llu\nbuild_checksum=%llu\nwrite_mode=%s\n",
			 ZCWALBLK_DESC_MAGIC, ZCWALBLK_DESC_VERSION, features,
			 sizeof(struct zcwalblk_extent),
			 sizeof(struct zcwalblk_composite), cfg->block_size,
			 cfg->record_bytes, cfg->extent_bytes, cfg->lanes,
			 cfg->powered ? 1 : 0, runtime ? runtime->wal.pairs : 0,
			 runtime ? runtime->wal.logical_records : 0,
			 runtime ? runtime->wal.logical_bytes : 0,
			 runtime ? runtime->wal.build_checksum : 0,
			 cfg->write_ack ? "ack" : "reject");
	mutex_unlock(&zcwalblk_lock);
	return ret;
}

CONFIGFS_ATTR_RO(zcwalblk_, descriptor_abi);

static struct configfs_attribute *zcwalblk_device_attrs[] = {
	&zcwalblk_attr_power,
	&zcwalblk_attr_blocksize,
	&zcwalblk_attr_size_mib,
	&zcwalblk_attr_queues,
	&zcwalblk_attr_queue_depth,
	&zcwalblk_attr_lanes,
	&zcwalblk_attr_record_bytes,
	&zcwalblk_attr_extent_bytes,
	&zcwalblk_attr_write_mode,
	&zcwalblk_attr_descriptor_abi,
	NULL,
};

static void zcwalblk_device_release(struct config_item *item)
{
	kfree(to_zcwalblk_cfg(item));
}

static const struct configfs_item_operations zcwalblk_device_ops = {
	.release = zcwalblk_device_release,
};

static const struct config_item_type zcwalblk_device_type = {
	.ct_item_ops = &zcwalblk_device_ops,
	.ct_attrs = zcwalblk_device_attrs,
	.ct_owner = THIS_MODULE,
};

static struct config_group *zcwalblk_make_group(struct config_group *group,
						const char *name)
{
	struct zcwalblk_cfg *cfg;

	if (!name || !*name || strlen(name) >= DISK_NAME_LEN)
		return ERR_PTR(-EINVAL);

	cfg = kzalloc(sizeof(*cfg), GFP_KERNEL);
	if (!cfg)
		return ERR_PTR(-ENOMEM);

	strscpy(cfg->name, name, sizeof(cfg->name));
	cfg->block_size = 4096;
	cfg->record_bytes = 4096;
	cfg->extent_bytes = 384 * 1024;
	cfg->capacity_mib = 1024;
	cfg->queues = 4;
	cfg->queue_depth = 256;
	cfg->lanes = 4;
	cfg->write_ack = false;

	config_group_init_type_name(&cfg->group, name, &zcwalblk_device_type);
	return &cfg->group;
}

static void zcwalblk_drop_item(struct config_group *group, struct config_item *item)
{
	struct zcwalblk_cfg *cfg = to_zcwalblk_cfg(item);

	mutex_lock(&zcwalblk_lock);
	zcwalblk_destroy_disk(cfg);
	mutex_unlock(&zcwalblk_lock);
	config_item_put(item);
}

static struct configfs_attribute *zcwalblk_group_attrs[] = {
	&zcwalblk_attr_features,
	NULL,
};

static const struct configfs_group_operations zcwalblk_group_ops = {
	.make_group = zcwalblk_make_group,
	.drop_item = zcwalblk_drop_item,
};

static const struct config_item_type zcwalblk_group_type = {
	.ct_group_ops = &zcwalblk_group_ops,
	.ct_attrs = zcwalblk_group_attrs,
	.ct_owner = THIS_MODULE,
};

static struct configfs_subsystem zcwalblk_subsys = {
	.su_group = {
		.cg_item = {
			.ci_namebuf = "zcwalblk",
			.ci_type = &zcwalblk_group_type,
		},
	},
};

static int __init zcwalblk_init(void)
{
	int ret;

	zcwalblk_major = register_blkdev(0, "zcwalblk");
	if (zcwalblk_major < 0)
		return zcwalblk_major;

	zcwalblk_ctl_misc.minor = MISC_DYNAMIC_MINOR;
	zcwalblk_ctl_misc.name = "zcwalctl";
	zcwalblk_ctl_misc.fops = &zcwalblk_ctl_fops;
	ret = misc_register(&zcwalblk_ctl_misc);
	if (ret) {
		unregister_blkdev(zcwalblk_major, "zcwalblk");
		return ret;
	}

	config_group_init(&zcwalblk_subsys.su_group);
	mutex_init(&zcwalblk_subsys.su_mutex);
	ret = configfs_register_subsystem(&zcwalblk_subsys);
	if (ret) {
		misc_deregister(&zcwalblk_ctl_misc);
		unregister_blkdev(zcwalblk_major, "zcwalblk");
		return ret;
	}

	pr_info("zcwalblk: C module loaded\n");
	return 0;
}

static void __exit zcwalblk_exit(void)
{
	configfs_unregister_subsystem(&zcwalblk_subsys);
	misc_deregister(&zcwalblk_ctl_misc);
	unregister_blkdev(zcwalblk_major, "zcwalblk");
	ida_destroy(&zcwalblk_indexes);
	pr_info("zcwalblk: C module unloaded\n");
}

module_init(zcwalblk_init);
module_exit(zcwalblk_exit);

MODULE_AUTHOR("Rob Caskey, OpenAI");
MODULE_DESCRIPTION("Zero-copy WAL/composite descriptor block facade");
MODULE_LICENSE("GPL");
