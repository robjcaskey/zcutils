// SPDX-License-Identifier: GPL-2.0

#include <linux/bio.h>
#include <linux/blk-mq.h>
#include <linux/blk_types.h>
#include <linux/blkdev.h>
#include <linux/configfs.h>
#include <linux/errno.h>
#include <linux/idr.h>
#include <linux/kernel.h>
#include <linux/math64.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/overflow.h>
#include <linux/slab.h>
#include <linux/string.h>

#define ZCSTRIPE_DESC_MAGIC 0x455049525453435aULL
#define ZCSTRIPE_DESC_VERSION 1
#define ZCSTRIPE_DESC_F_TOPOLOGY_HINTS (1U << 0)
#define ZCSTRIPE_DESC_F_RELEASE_TOKEN (1U << 1)
#define ZCSTRIPE_DESC_F_BLOCK_EXTENTS (1U << 2)
#define ZCSTRIPE_DESC_F_STRIPED_TARGET (1U << 3)
#define ZCSTRIPE_MAX_TARGETS 32
#define ZCSTRIPE_MAX_SPEC 4096

struct zcstripe_slice_desc {
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

struct zcstripe_record_desc {
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

struct zcstripe_desc_batch {
	u64 magic;
	u16 version;
	u16 flags;
	u32 record_count;
	u64 device_offset;
	u64 bytes;
};

enum zcstripe_descriptor_mode {
	ZCSTRIPE_DESC_DISABLED = 0,
	ZCSTRIPE_DESC_ADVERTISE = 1,
};

struct zcstripe_lower {
	struct file *file;
	struct block_device *bdev;
	u64 bytes;
};

struct zcstripe_target {
	u32 nr_targets;
	u32 stripe_unit;
	u32 block_size;
	u64 capacity_bytes;
	struct zcstripe_lower lower[] __counted_by(nr_targets);
};

struct zcstripe_cfg;

struct zcstripe_disk {
	struct zcstripe_cfg *cfg;
	struct blk_mq_tag_set tag_set;
	struct gendisk *disk;
	struct zcstripe_target *target;
	int index;
};

struct zcstripe_cfg {
	struct config_group group;
	bool powered;
	char name[DISK_NAME_LEN];
	char targets[ZCSTRIPE_MAX_SPEC + 1];
	u32 stripe_unit;
	u32 block_size;
	u32 queues;
	u32 queue_depth;
	enum zcstripe_descriptor_mode descriptor_mode;
	struct zcstripe_disk *runtime;
};

struct zcstripe_io {
	struct request *rq;
	atomic_t pending;
	blk_status_t status;
};

static DEFINE_MUTEX(zcstripe_lock);
static DEFINE_IDA(zcstripe_indexes);
static int zcstripe_major;

static inline struct zcstripe_cfg *to_zcstripe_cfg(struct config_item *item)
{
	return item ? container_of(to_config_group(item), struct zcstripe_cfg, group) : NULL;
}

static void zcstripe_target_destroy(struct zcstripe_target *target)
{
	u32 i;

	if (!target)
		return;

	for (i = 0; i < target->nr_targets; i++) {
		if (target->lower[i].file) {
			bdev_fput(target->lower[i].file);
			target->lower[i].file = NULL;
			target->lower[i].bdev = NULL;
		}
	}
	kfree(target);
}

static bool zcstripe_separator(char c)
{
	return c == ',' || c == ' ' || c == '\t' || c == '\n';
}

static char *zcstripe_next_token(char **cursor)
{
	char *start, *p;

	if (!cursor || !*cursor)
		return NULL;

	start = *cursor;
	while (*start && zcstripe_separator(*start))
		start++;
	if (!*start) {
		*cursor = start;
		return NULL;
	}

	p = start;
	while (*p && !zcstripe_separator(*p))
		p++;
	if (*p) {
		*p = '\0';
		*cursor = p + 1;
	} else {
		*cursor = p;
	}

	return start;
}

static int zcstripe_target_create(const char *targets, u32 stripe_unit,
				  u32 block_size, struct zcstripe_target **out)
{
	struct zcstripe_target *target;
	char *spec, *cursor, *token;
	u64 min_bytes = U64_MAX;
	size_t spec_len;
	int ret = 0;

	*out = NULL;
	if (block_size < 512 || !is_power_of_2(block_size))
		return -EINVAL;
	if (stripe_unit < block_size || !is_power_of_2(stripe_unit))
		return -EINVAL;
	if (stripe_unit % block_size)
		return -EINVAL;

	spec_len = strnlen(targets, ZCSTRIPE_MAX_SPEC + 1);
	if (!spec_len || spec_len > ZCSTRIPE_MAX_SPEC)
		return -EINVAL;

	spec = kstrdup(targets, GFP_KERNEL);
	if (!spec)
		return -ENOMEM;

	target = kzalloc(struct_size(target, lower, ZCSTRIPE_MAX_TARGETS),
			 GFP_KERNEL);
	if (!target) {
		ret = -ENOMEM;
		goto out_spec;
	}
	target->stripe_unit = stripe_unit;
	target->block_size = block_size;

	cursor = spec;
	while ((token = zcstripe_next_token(&cursor))) {
		struct file *file;
		struct block_device *bdev;
		u64 bytes;
		u32 logical;

		if (target->nr_targets >= ZCSTRIPE_MAX_TARGETS) {
			ret = -E2BIG;
			goto out_target;
		}

		file = bdev_file_open_by_path(token, BLK_OPEN_READ | BLK_OPEN_WRITE,
					      NULL, NULL);
		if (IS_ERR(file)) {
			ret = PTR_ERR(file);
			goto out_target;
		}

		bdev = file_bdev(file);
		bytes = bdev_nr_bytes(bdev);
		logical = bdev_logical_block_size(bdev);
		if (!bytes || block_size < logical || stripe_unit % logical) {
			bdev_fput(file);
			ret = -EINVAL;
			goto out_target;
		}

		target->lower[target->nr_targets].file = file;
		target->lower[target->nr_targets].bdev = bdev;
		target->lower[target->nr_targets].bytes = bytes;
		target->nr_targets++;
		min_bytes = min(min_bytes, bytes);
	}

	if (target->nr_targets < 2) {
		ret = -EINVAL;
		goto out_target;
	}

	min_bytes = round_down(min_bytes, (u64)stripe_unit);
	if (!min_bytes || check_mul_overflow(min_bytes, (u64)target->nr_targets,
					     &target->capacity_bytes)) {
		ret = -EOVERFLOW;
		goto out_target;
	}

	*out = target;
	kfree(spec);
	return 0;

out_target:
	zcstripe_target_destroy(target);
out_spec:
	kfree(spec);
	return ret;
}

static int zcstripe_map(struct zcstripe_target *target, u64 logical,
			u32 *lower_idx, u64 *lower_off, u32 *stripe_remaining)
{
	u32 stripe_off;
	u32 dev_idx;
	u64 stripe_no;
	u64 row;

	if (logical >= target->capacity_bytes)
		return -EIO;

	stripe_no = div_u64_rem(logical, target->stripe_unit, &stripe_off);
	row = div_u64_rem(stripe_no, target->nr_targets, &dev_idx);
	*lower_idx = dev_idx;
	*lower_off = row * target->stripe_unit + stripe_off;
	*stripe_remaining = target->stripe_unit - stripe_off;
	return 0;
}

static void zcstripe_set_status(struct zcstripe_io *io, blk_status_t status)
{
	if (status != BLK_STS_OK && READ_ONCE(io->status) == BLK_STS_OK)
		WRITE_ONCE(io->status, status);
}

static void zcstripe_io_put(struct zcstripe_io *io)
{
	if (atomic_dec_and_test(&io->pending))
		blk_mq_complete_request(io->rq);
}

static void zcstripe_bio_end_io(struct bio *bio)
{
	struct zcstripe_io *io = bio->bi_private;

	if (bio->bi_status)
		zcstripe_set_status(io, bio->bi_status);

	bio_put(bio);
	zcstripe_io_put(io);
}

static void zcstripe_submit_bio(struct zcstripe_io *io, struct bio *bio)
{
	bio->bi_private = io;
	bio->bi_end_io = zcstripe_bio_end_io;
	atomic_inc(&io->pending);
	submit_bio(bio);
}

static int zcstripe_submit_page(struct zcstripe_target *target,
				struct zcstripe_io *io, u32 lower_idx,
				u64 lower_off, enum req_op op, struct page *page,
				unsigned int page_off, unsigned int len)
{
	struct zcstripe_lower *lower = &target->lower[lower_idx];
	struct bio *bio;
	int added;

	bio = bio_alloc(lower->bdev, 1, op, GFP_ATOMIC);
	if (!bio)
		return -ENOMEM;

	bio->bi_iter.bi_sector = lower_off >> SECTOR_SHIFT;
	added = bio_add_page(bio, page, len, page_off);
	if (added != len) {
		bio_put(bio);
		return -EIO;
	}

	zcstripe_submit_bio(io, bio);
	return 0;
}

static int zcstripe_submit_range(struct zcstripe_target *target,
				 struct zcstripe_io *io, u32 lower_idx,
				 u64 lower_off, enum req_op op, unsigned int len)
{
	struct zcstripe_lower *lower = &target->lower[lower_idx];
	struct bio *bio;

	bio = bio_alloc(lower->bdev, 0, op, GFP_ATOMIC);
	if (!bio)
		return -ENOMEM;

	bio->bi_iter.bi_sector = lower_off >> SECTOR_SHIFT;
	bio->bi_iter.bi_size = len;
	zcstripe_submit_bio(io, bio);
	return 0;
}

static int zcstripe_flush_all(struct zcstripe_target *target,
			      struct zcstripe_io *io)
{
	u32 i;

	for (i = 0; i < target->nr_targets; i++) {
		struct bio *bio;

		bio = bio_alloc(target->lower[i].bdev, 0,
				REQ_OP_WRITE | REQ_PREFLUSH, GFP_ATOMIC);
		if (!bio)
			return -ENOMEM;
		zcstripe_submit_bio(io, bio);
	}

	return 0;
}

static int zcstripe_transfer_discard_zeroes(struct zcstripe_target *target,
					    struct zcstripe_io *io,
					    u64 pos, u64 bytes, enum req_op op)
{
	u64 transferred = 0;

	while (transferred < bytes) {
		u64 logical = pos + transferred;
		u64 lower_off;
		u32 lower_idx;
		u32 stripe_remaining;
		unsigned int todo;
		int ret;

		ret = zcstripe_map(target, logical, &lower_idx, &lower_off,
				   &stripe_remaining);
		if (ret)
			return ret;

		todo = min_t(u64, bytes - transferred, stripe_remaining);
		ret = zcstripe_submit_range(target, io, lower_idx, lower_off, op, todo);
		if (ret)
			return ret;
		transferred += todo;
	}

	return 0;
}

static int zcstripe_submit_request(struct request *rq, struct zcstripe_target *target,
				   struct zcstripe_io *io)
{
	struct req_iterator iter;
	struct bio_vec bvec;
	u64 pos = (u64)blk_rq_pos(rq) << SECTOR_SHIFT;
	u64 bytes = blk_rq_bytes(rq);
	u64 transferred = 0;
	enum req_op op = req_op(rq);
	int ret = 0;

	if (!target)
		return -EINVAL;

	if (op == REQ_OP_FLUSH)
		return zcstripe_flush_all(target, io);

	if (pos > target->capacity_bytes || bytes > target->capacity_bytes - pos)
		return -EIO;

	if (op == REQ_OP_DISCARD || op == REQ_OP_WRITE_ZEROES)
		return zcstripe_transfer_discard_zeroes(target, io, pos, bytes, op);

	if (op != REQ_OP_READ && op != REQ_OP_WRITE)
		return -EOPNOTSUPP;

	rq_for_each_segment(bvec, rq, iter) {
		unsigned int seg_done = 0;
		unsigned int seg_len = bvec.bv_len;

		if (transferred + seg_len > bytes)
			seg_len = bytes - transferred;
		if (!seg_len)
			break;

		while (seg_done < seg_len) {
			u64 logical = pos + transferred;
			u64 lower_off;
			u32 lower_idx;
			u32 stripe_remaining;
			unsigned int todo;

			ret = zcstripe_map(target, logical, &lower_idx, &lower_off,
					   &stripe_remaining);
			if (ret)
				return ret;

			todo = min3(seg_len - seg_done, stripe_remaining,
				    UINT_MAX - bvec.bv_offset - seg_done);
			if (!todo)
				return -EIO;

			ret = zcstripe_submit_page(target, io, lower_idx, lower_off,
						   op, bvec.bv_page,
						   bvec.bv_offset + seg_done,
						   todo);
			if (ret)
				return ret;

			seg_done += todo;
			transferred += todo;
		}

		if (transferred >= bytes)
			break;
	}

	return transferred == bytes ? 0 : -EIO;
}

static blk_status_t zcstripe_queue_rq(struct blk_mq_hw_ctx *hctx,
				      const struct blk_mq_queue_data *bd)
{
	struct zcstripe_disk *dev = hctx->queue->queuedata;
	struct request *rq = bd->rq;
	struct zcstripe_io *io;
	int ret;

	blk_mq_start_request(rq);
	io = kzalloc(sizeof(*io), GFP_ATOMIC);
	if (!io) {
		blk_mq_end_request(rq, BLK_STS_RESOURCE);
		return BLK_STS_OK;
	}

	io->rq = rq;
	atomic_set(&io->pending, 1);
	io->status = BLK_STS_OK;
	rq->end_io_data = io;

	ret = zcstripe_submit_request(rq, dev->target, io);
	if (ret)
		zcstripe_set_status(io, errno_to_blk_status(ret));
	zcstripe_io_put(io);
	return BLK_STS_OK;
}

static void zcstripe_complete(struct request *rq)
{
	struct zcstripe_io *io = rq->end_io_data;
	blk_status_t status;

	if (!io) {
		blk_mq_end_request(rq, BLK_STS_IOERR);
		return;
	}

	status = READ_ONCE(io->status);
	rq->end_io_data = NULL;
	kfree(io);
	blk_mq_end_request(rq, status);
}

static const struct blk_mq_ops zcstripe_mq_ops = {
	.queue_rq = zcstripe_queue_rq,
	.complete = zcstripe_complete,
};

static const struct block_device_operations zcstripe_fops = {
	.owner = THIS_MODULE,
};

static int zcstripe_create_disk(struct zcstripe_cfg *cfg)
{
	struct queue_limits lim = { };
	struct zcstripe_disk *dev;
	int ret;

	if (cfg->powered)
		return 0;
	if (!cfg->targets[0] || cfg->queues == 0 || cfg->queue_depth < 4)
		return -EINVAL;
	if (blk_validate_block_size(cfg->block_size))
		return -EINVAL;

	dev = kzalloc(sizeof(*dev), GFP_KERNEL);
	if (!dev)
		return -ENOMEM;
	dev->cfg = cfg;

	ret = zcstripe_target_create(cfg->targets, cfg->stripe_unit,
				     cfg->block_size, &dev->target);
	if (ret)
		goto out_free_dev;

	dev->tag_set.ops = &zcstripe_mq_ops;
	dev->tag_set.nr_hw_queues = cfg->queues;
	dev->tag_set.queue_depth = cfg->queue_depth;
	dev->tag_set.numa_node = NUMA_NO_NODE;
	dev->tag_set.cmd_size = 0;
	dev->tag_set.flags = BLK_MQ_F_NO_SCHED_BY_DEFAULT | BLK_MQ_F_STACKING;
	dev->tag_set.driver_data = dev;

	ret = blk_mq_alloc_tag_set(&dev->tag_set);
	if (ret)
		goto out_destroy_target;

	lim.logical_block_size = cfg->block_size;
	lim.physical_block_size = cfg->block_size;
	lim.io_min = cfg->block_size;
	lim.io_opt = cfg->stripe_unit;
	lim.dma_alignment = cfg->block_size - 1;
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
	dev->disk->major = zcstripe_major;
	dev->index = ida_alloc(&zcstripe_indexes, GFP_KERNEL);
	if (dev->index < 0) {
		ret = dev->index;
		goto out_put_disk;
	}
	dev->disk->first_minor = dev->index;
	dev->disk->minors = 1;
	dev->disk->fops = &zcstripe_fops;
	dev->disk->private_data = dev;
	strscpy(dev->disk->disk_name, cfg->name, DISK_NAME_LEN);
	set_capacity(dev->disk, dev->target->capacity_bytes >> SECTOR_SHIFT);

	ret = add_disk(dev->disk);
	if (ret)
		goto out_free_ida;

	cfg->runtime = dev;
	cfg->powered = true;
	pr_info("zcstripe: disk %s created bytes=%llu targets=%u stripe_unit=%u queues=%u\n",
		cfg->name, dev->target->capacity_bytes, dev->target->nr_targets,
		cfg->stripe_unit, cfg->queues);
	return 0;

out_free_ida:
	ida_free(&zcstripe_indexes, dev->index);
out_put_disk:
	put_disk(dev->disk);
out_free_tags:
	blk_mq_free_tag_set(&dev->tag_set);
out_destroy_target:
	zcstripe_target_destroy(dev->target);
out_free_dev:
	kfree(dev);
	return ret;
}

static void zcstripe_destroy_disk(struct zcstripe_cfg *cfg)
{
	struct zcstripe_disk *dev = cfg->runtime;

	if (!dev)
		return;
	cfg->runtime = NULL;
	cfg->powered = false;

	del_gendisk(dev->disk);
	ida_free(&zcstripe_indexes, dev->index);
	put_disk(dev->disk);
	blk_mq_free_tag_set(&dev->tag_set);
	zcstripe_target_destroy(dev->target);
	kfree(dev);
	pr_info("zcstripe: disk %s removed\n", cfg->name);
}

static ssize_t zcstripe_features_show(struct config_item *item, char *page)
{
	return sysfs_emit(page,
			  "power,targets,stripe_unit,blocksize,queues,queue_depth,descriptor_mode,descriptor_abi\n");
}

CONFIGFS_ATTR_RO(zcstripe_, features);

static ssize_t zcstripe_power_show(struct config_item *item, char *page)
{
	struct zcstripe_cfg *cfg = to_zcstripe_cfg(item);
	bool powered;

	mutex_lock(&zcstripe_lock);
	powered = cfg->powered;
	mutex_unlock(&zcstripe_lock);
	return sysfs_emit(page, "%u\n", powered ? 1 : 0);
}

static ssize_t zcstripe_power_store(struct config_item *item, const char *page,
				    size_t count)
{
	struct zcstripe_cfg *cfg = to_zcstripe_cfg(item);
	bool power;
	int ret;

	ret = kstrtobool(page, &power);
	if (ret)
		return ret;

	mutex_lock(&zcstripe_lock);
	if (power)
		ret = zcstripe_create_disk(cfg);
	else
		zcstripe_destroy_disk(cfg);
	mutex_unlock(&zcstripe_lock);

	return ret ? ret : count;
}

CONFIGFS_ATTR(zcstripe_, power);

static ssize_t zcstripe_targets_show(struct config_item *item, char *page)
{
	struct zcstripe_cfg *cfg = to_zcstripe_cfg(item);

	return sysfs_emit(page, "%s\n", cfg->targets);
}

static ssize_t zcstripe_targets_store(struct config_item *item, const char *page,
				      size_t count)
{
	struct zcstripe_cfg *cfg = to_zcstripe_cfg(item);
	char *buf;
	char *text;
	int ret = 0;

	if (count > ZCSTRIPE_MAX_SPEC)
		return -EINVAL;
	buf = kstrndup(page, count, GFP_KERNEL);
	if (!buf)
		return -ENOMEM;
	text = strim(buf);
	if (!*text || strlen(text) > ZCSTRIPE_MAX_SPEC) {
		ret = -EINVAL;
		goto out;
	}

	mutex_lock(&zcstripe_lock);
	if (cfg->powered)
		ret = -EBUSY;
	else
		strscpy(cfg->targets, text, sizeof(cfg->targets));
	mutex_unlock(&zcstripe_lock);

out:
	kfree(buf);
	return ret ? ret : count;
}

CONFIGFS_ATTR(zcstripe_, targets);

static ssize_t zcstripe_stripe_unit_show(struct config_item *item, char *page)
{
	struct zcstripe_cfg *cfg = to_zcstripe_cfg(item);

	return sysfs_emit(page, "%u\n", cfg->stripe_unit);
}

static ssize_t zcstripe_stripe_unit_store(struct config_item *item,
					  const char *page, size_t count)
{
	struct zcstripe_cfg *cfg = to_zcstripe_cfg(item);
	u32 value;
	int ret;

	ret = kstrtou32(page, 0, &value);
	if (ret)
		return ret;
	if (value < 512 || !is_power_of_2(value))
		return -EINVAL;

	mutex_lock(&zcstripe_lock);
	if (cfg->powered)
		ret = -EBUSY;
	else
		cfg->stripe_unit = value;
	mutex_unlock(&zcstripe_lock);
	return ret ? ret : count;
}

CONFIGFS_ATTR(zcstripe_, stripe_unit);

static ssize_t zcstripe_blocksize_show(struct config_item *item, char *page)
{
	struct zcstripe_cfg *cfg = to_zcstripe_cfg(item);

	return sysfs_emit(page, "%u\n", cfg->block_size);
}

static ssize_t zcstripe_blocksize_store(struct config_item *item,
					const char *page, size_t count)
{
	struct zcstripe_cfg *cfg = to_zcstripe_cfg(item);
	u32 value;
	int ret;

	ret = kstrtou32(page, 0, &value);
	if (ret)
		return ret;
	if (blk_validate_block_size(value))
		return -EINVAL;

	mutex_lock(&zcstripe_lock);
	if (cfg->powered)
		ret = -EBUSY;
	else
		cfg->block_size = value;
	mutex_unlock(&zcstripe_lock);
	return ret ? ret : count;
}

CONFIGFS_ATTR(zcstripe_, blocksize);

static ssize_t zcstripe_queues_show(struct config_item *item, char *page)
{
	struct zcstripe_cfg *cfg = to_zcstripe_cfg(item);

	return sysfs_emit(page, "%u\n", cfg->queues);
}

static ssize_t zcstripe_queues_store(struct config_item *item, const char *page,
				     size_t count)
{
	struct zcstripe_cfg *cfg = to_zcstripe_cfg(item);
	u32 value;
	int ret;

	ret = kstrtou32(page, 0, &value);
	if (ret)
		return ret;
	if (!value || value > 4096)
		return -EINVAL;

	mutex_lock(&zcstripe_lock);
	if (cfg->powered)
		ret = -EBUSY;
	else
		cfg->queues = value;
	mutex_unlock(&zcstripe_lock);
	return ret ? ret : count;
}

CONFIGFS_ATTR(zcstripe_, queues);

static ssize_t zcstripe_queue_depth_show(struct config_item *item, char *page)
{
	struct zcstripe_cfg *cfg = to_zcstripe_cfg(item);

	return sysfs_emit(page, "%u\n", cfg->queue_depth);
}

static ssize_t zcstripe_queue_depth_store(struct config_item *item,
					  const char *page, size_t count)
{
	struct zcstripe_cfg *cfg = to_zcstripe_cfg(item);
	u32 value;
	int ret;

	ret = kstrtou32(page, 0, &value);
	if (ret)
		return ret;
	if (value < 4 || value > 32768)
		return -EINVAL;

	mutex_lock(&zcstripe_lock);
	if (cfg->powered)
		ret = -EBUSY;
	else
		cfg->queue_depth = value;
	mutex_unlock(&zcstripe_lock);
	return ret ? ret : count;
}

CONFIGFS_ATTR(zcstripe_, queue_depth);

static int zcstripe_parse_descriptor_mode(const char *page,
					  enum zcstripe_descriptor_mode *mode)
{
	char *buf, *text;
	int ret = 0;

	buf = kstrndup(page, PAGE_SIZE, GFP_KERNEL);
	if (!buf)
		return -ENOMEM;
	text = strim(buf);
	if (!strcmp(text, "0") || !strcmp(text, "off") ||
	    !strcmp(text, "false") || !strcmp(text, "disabled"))
		*mode = ZCSTRIPE_DESC_DISABLED;
	else if (!strcmp(text, "1") || !strcmp(text, "on") ||
		 !strcmp(text, "true") || !strcmp(text, "advertise"))
		*mode = ZCSTRIPE_DESC_ADVERTISE;
	else
		ret = -EINVAL;
	kfree(buf);
	return ret;
}

static ssize_t zcstripe_descriptor_mode_show(struct config_item *item, char *page)
{
	struct zcstripe_cfg *cfg = to_zcstripe_cfg(item);

	return sysfs_emit(page, "%u\n", cfg->descriptor_mode == ZCSTRIPE_DESC_ADVERTISE);
}

static ssize_t zcstripe_descriptor_mode_store(struct config_item *item,
					      const char *page, size_t count)
{
	struct zcstripe_cfg *cfg = to_zcstripe_cfg(item);
	enum zcstripe_descriptor_mode mode;
	int ret;

	ret = zcstripe_parse_descriptor_mode(page, &mode);
	if (ret)
		return ret;

	mutex_lock(&zcstripe_lock);
	if (cfg->powered)
		ret = -EBUSY;
	else
		cfg->descriptor_mode = mode;
	mutex_unlock(&zcstripe_lock);
	return ret ? ret : count;
}

CONFIGFS_ATTR(zcstripe_, descriptor_mode);

static ssize_t zcstripe_descriptor_abi_show(struct config_item *item, char *page)
{
	struct zcstripe_cfg *cfg = to_zcstripe_cfg(item);
	u32 features = ZCSTRIPE_DESC_F_TOPOLOGY_HINTS |
		       ZCSTRIPE_DESC_F_RELEASE_TOKEN |
		       ZCSTRIPE_DESC_F_BLOCK_EXTENTS |
		       ZCSTRIPE_DESC_F_STRIPED_TARGET;

	return sysfs_emit(page,
			  "magic=0x%016llx\nversion=%u\nmode=%u\nfeatures=0x%08x\nslice_desc=%zu\nrecord_desc=%zu\nbatch=%zu\nstripe_unit=%u\ntargets=%s\nqueues=%u\n",
			  ZCSTRIPE_DESC_MAGIC, ZCSTRIPE_DESC_VERSION,
			  cfg->descriptor_mode == ZCSTRIPE_DESC_ADVERTISE,
			  features, sizeof(struct zcstripe_slice_desc),
			  sizeof(struct zcstripe_record_desc),
			  sizeof(struct zcstripe_desc_batch),
			  cfg->stripe_unit, cfg->targets, cfg->queues);
}

CONFIGFS_ATTR_RO(zcstripe_, descriptor_abi);

static struct configfs_attribute *zcstripe_device_attrs[] = {
	&zcstripe_attr_power,
	&zcstripe_attr_targets,
	&zcstripe_attr_stripe_unit,
	&zcstripe_attr_blocksize,
	&zcstripe_attr_queues,
	&zcstripe_attr_queue_depth,
	&zcstripe_attr_descriptor_mode,
	&zcstripe_attr_descriptor_abi,
	NULL,
};

static void zcstripe_device_release(struct config_item *item)
{
	kfree(to_zcstripe_cfg(item));
}

static const struct configfs_item_operations zcstripe_device_ops = {
	.release = zcstripe_device_release,
};

static const struct config_item_type zcstripe_device_type = {
	.ct_item_ops = &zcstripe_device_ops,
	.ct_attrs = zcstripe_device_attrs,
	.ct_owner = THIS_MODULE,
};

static struct config_group *zcstripe_make_group(struct config_group *group,
						const char *name)
{
	struct zcstripe_cfg *cfg;

	if (!name || !*name || strlen(name) >= DISK_NAME_LEN)
		return ERR_PTR(-EINVAL);

	cfg = kzalloc(sizeof(*cfg), GFP_KERNEL);
	if (!cfg)
		return ERR_PTR(-ENOMEM);

	strscpy(cfg->name, name, sizeof(cfg->name));
	cfg->stripe_unit = 4096;
	cfg->block_size = 4096;
	cfg->queues = 4;
	cfg->queue_depth = 256;
	cfg->descriptor_mode = ZCSTRIPE_DESC_DISABLED;

	config_group_init_type_name(&cfg->group, name, &zcstripe_device_type);
	return &cfg->group;
}

static void zcstripe_drop_item(struct config_group *group, struct config_item *item)
{
	struct zcstripe_cfg *cfg = to_zcstripe_cfg(item);

	mutex_lock(&zcstripe_lock);
	zcstripe_destroy_disk(cfg);
	mutex_unlock(&zcstripe_lock);
	config_item_put(item);
}

static struct configfs_attribute *zcstripe_group_attrs[] = {
	&zcstripe_attr_features,
	NULL,
};

static const struct configfs_group_operations zcstripe_group_ops = {
	.make_group = zcstripe_make_group,
	.drop_item = zcstripe_drop_item,
};

static const struct config_item_type zcstripe_group_type = {
	.ct_group_ops = &zcstripe_group_ops,
	.ct_attrs = zcstripe_group_attrs,
	.ct_owner = THIS_MODULE,
};

static struct configfs_subsystem zcstripe_subsys = {
	.su_group = {
		.cg_item = {
			.ci_namebuf = "zcstripe",
			.ci_type = &zcstripe_group_type,
		},
	},
};

static int __init zcstripe_init(void)
{
	int ret;

	zcstripe_major = register_blkdev(0, "zcstripe");
	if (zcstripe_major < 0)
		return zcstripe_major;

	config_group_init(&zcstripe_subsys.su_group);
	mutex_init(&zcstripe_subsys.su_mutex);
	ret = configfs_register_subsystem(&zcstripe_subsys);
	if (ret) {
		unregister_blkdev(zcstripe_major, "zcstripe");
		return ret;
	}

	pr_info("zcstripe: C module loaded\n");
	return 0;
}

static void __exit zcstripe_exit(void)
{
	configfs_unregister_subsystem(&zcstripe_subsys);
	unregister_blkdev(zcstripe_major, "zcstripe");
	ida_destroy(&zcstripe_indexes);
	pr_info("zcstripe: C module unloaded\n");
}

module_init(zcstripe_init);
module_exit(zcstripe_exit);

MODULE_AUTHOR("Rob Caskey, OpenAI");
MODULE_DESCRIPTION("Zero-copy friendly striped block target");
MODULE_LICENSE("GPL");
