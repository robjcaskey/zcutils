// SPDX-License-Identifier: GPL-2.0

#include <linux/bio.h>
#include <linux/blk-mq.h>
#include <linux/blk_types.h>
#include <linux/blkdev.h>
#include <linux/configfs.h>
#include <linux/err.h>
#include <linux/errno.h>
#include <linux/idr.h>
#include <linux/kernel.h>
#include <linux/math64.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/slab.h>
#include <linux/string.h>
#include <linux/types.h>

#define ZCSTRIPE_NAME "zcstripe"
#define ZCSTRIPE_MAX_TARGETS 32
#define ZCSTRIPE_MAX_SPEC 4096
#define ZCSTRIPE_DESC_MAGIC 0x455049525453435aULL
#define ZCSTRIPE_DESC_VERSION 1
#define ZCSTRIPE_DESC_F_TOPOLOGY_HINTS BIT(0)
#define ZCSTRIPE_DESC_F_RELEASE_TOKEN BIT(1)
#define ZCSTRIPE_DESC_F_BLOCK_EXTENTS BIT(2)
#define ZCSTRIPE_DESC_F_STRIPED_TARGET BIT(3)

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

struct zcstripe_io {
	struct request *rq;
	atomic_t pending;
	blk_status_t status;
};

struct zcstripe_device {
	struct config_group group;
	struct mutex lock;
	bool powered;
	char name[DISK_NAME_LEN];
	char targets[ZCSTRIPE_MAX_SPEC + 1];
	u32 stripe_unit;
	u32 block_size;
	u32 queues;
	u32 queue_depth;
	bool descriptor_mode;
	struct zcstripe_target *target;
	struct blk_mq_tag_set tag_set;
	struct gendisk *disk;
	int minor;
};

static struct configfs_subsystem zcstripe_subsys;
static int zcstripe_major;
static DEFINE_IDA(zcstripe_minors);

static inline struct zcstripe_device *to_zcstripe_device(struct config_item *item)
{
	return container_of(to_config_group(item), struct zcstripe_device, group);
}

static int zcstripe_parse_descriptor_mode(const char *page, bool *value)
{
	if (sysfs_streq(page, "advertise")) {
		*value = true;
		return 0;
	}
	if (sysfs_streq(page, "disabled")) {
		*value = false;
		return 0;
	}
	return kstrtobool(page, value);
}

static bool zcstripe_separator(char c)
{
	return c == ',' || c == ' ' || c == '\t' || c == '\n';
}

static char *zcstripe_next_token(char **cursor)
{
	char *start;
	char *p;

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

static void zcstripe_target_destroy(struct zcstripe_target *target)
{
	u32 i;

	if (!target)
		return;

	for (i = 0; i < target->nr_targets; i++) {
		if (target->lower[i].file)
			bdev_fput(target->lower[i].file);
	}
	kfree(target);
}

static int zcstripe_target_create(const char *targets, u32 stripe_unit,
				  u32 block_size,
				  struct zcstripe_target **target_out)
{
	struct zcstripe_target *target;
	char *spec, *cursor, *token;
	u64 min_bytes = U64_MAX;
	size_t spec_len;
	int ret = 0;

	*target_out = NULL;
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

		file = bdev_file_open_by_path(token,
					      BLK_OPEN_READ | BLK_OPEN_WRITE,
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

	*target_out = target;
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
					    struct zcstripe_io *io, u64 pos,
					    u64 bytes, enum req_op op)
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
		ret = zcstripe_submit_range(target, io, lower_idx, lower_off, op,
					    todo);
		if (ret)
			return ret;
		transferred += todo;
	}

	return 0;
}

static int zcstripe_submit_request(struct request *rq, struct zcstripe_target *target)
{
	struct req_iterator iter;
	struct bio_vec bvec;
	struct zcstripe_io *io;
	u64 pos = (u64)blk_rq_pos(rq) << SECTOR_SHIFT;
	u64 bytes = blk_rq_bytes(rq);
	u64 transferred = 0;
	enum req_op op = req_op(rq);
	int ret = 0;

	io = kzalloc(sizeof(*io), GFP_ATOMIC);
	if (!io)
		return -ENOMEM;
	io->rq = rq;
	atomic_set(&io->pending, 1);
	io->status = BLK_STS_OK;
	rq->end_io_data = io;

	if (op == REQ_OP_FLUSH) {
		ret = zcstripe_flush_all(target, io);
		goto out;
	}

	if (pos > target->capacity_bytes || bytes > target->capacity_bytes - pos) {
		ret = -EIO;
		goto out;
	}

	if (op == REQ_OP_DISCARD || op == REQ_OP_WRITE_ZEROES) {
		ret = zcstripe_transfer_discard_zeroes(target, io, pos, bytes, op);
		goto out;
	}

	if (op != REQ_OP_READ && op != REQ_OP_WRITE) {
		ret = -EOPNOTSUPP;
		goto out;
	}

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
				goto out;

			todo = min3(seg_len - seg_done, stripe_remaining,
				    UINT_MAX - bvec.bv_offset - seg_done);
			if (!todo) {
				ret = -EIO;
				goto out;
			}

			ret = zcstripe_submit_page(target, io, lower_idx,
						   lower_off, op, bvec.bv_page,
						   bvec.bv_offset + seg_done,
						   todo);
			if (ret)
				goto out;

			seg_done += todo;
			transferred += todo;
		}

		if (transferred >= bytes)
			break;
	}

	if (transferred != bytes)
		ret = -EIO;

out:
	if (ret)
		zcstripe_set_status(io, errno_to_blk_status(ret));
	zcstripe_io_put(io);
	return 0;
}

static blk_status_t zcstripe_queue_rq(struct blk_mq_hw_ctx *hctx,
				      const struct blk_mq_queue_data *bd)
{
	struct request *rq = bd->rq;
	struct zcstripe_device *dev = rq->q->queuedata;
	int ret;

	blk_mq_start_request(rq);
	ret = zcstripe_submit_request(rq, dev->target);
	if (ret)
		blk_mq_end_request(rq, errno_to_blk_status(ret));
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

static void zcstripe_power_off_locked(struct zcstripe_device *dev)
{
	if (!dev->powered)
		return;

	if (dev->disk) {
		del_gendisk(dev->disk);
		put_disk(dev->disk);
		dev->disk = NULL;
	}
	blk_mq_free_tag_set(&dev->tag_set);
	zcstripe_target_destroy(dev->target);
	dev->target = NULL;
	if (dev->minor >= 0) {
		ida_free(&zcstripe_minors, dev->minor);
		dev->minor = -1;
	}
	dev->powered = false;
}

static int zcstripe_power_on_locked(struct zcstripe_device *dev)
{
	struct queue_limits lim = {
		.logical_block_size = dev->block_size,
		.physical_block_size = dev->block_size,
	};
	int ret;

	if (dev->powered)
		return 0;

	ret = zcstripe_target_create(dev->targets, dev->stripe_unit,
				     dev->block_size, &dev->target);
	if (ret)
		return ret;

	dev->minor = ida_alloc(&zcstripe_minors, GFP_KERNEL);
	if (dev->minor < 0) {
		ret = dev->minor;
		goto out_target;
	}

	memset(&dev->tag_set, 0, sizeof(dev->tag_set));
	dev->tag_set.ops = &zcstripe_mq_ops;
	dev->tag_set.nr_hw_queues = dev->queues;
	dev->tag_set.queue_depth = dev->queue_depth;
	dev->tag_set.numa_node = NUMA_NO_NODE;
	dev->tag_set.flags = BLK_MQ_F_STACKING | BLK_MQ_F_NO_SCHED_BY_DEFAULT;
	ret = blk_mq_alloc_tag_set(&dev->tag_set);
	if (ret)
		goto out_minor;

	dev->disk = blk_mq_alloc_disk(&dev->tag_set, &lim, dev);
	if (IS_ERR(dev->disk)) {
		ret = PTR_ERR(dev->disk);
		dev->disk = NULL;
		goto out_tagset;
	}

	dev->disk->major = zcstripe_major;
	dev->disk->first_minor = dev->minor;
	dev->disk->minors = 1;
	dev->disk->fops = &zcstripe_fops;
	dev->disk->flags |= GENHD_FL_NO_PART;
	snprintf(dev->disk->disk_name, DISK_NAME_LEN, "%s", dev->name);
	set_capacity(dev->disk, dev->target->capacity_bytes >> SECTOR_SHIFT);
	dev->disk->queue->queuedata = dev;

	ret = add_disk(dev->disk);
	if (ret)
		goto out_disk;

	dev->powered = true;
	return 0;

out_disk:
	put_disk(dev->disk);
	dev->disk = NULL;
out_tagset:
	blk_mq_free_tag_set(&dev->tag_set);
out_minor:
	ida_free(&zcstripe_minors, dev->minor);
	dev->minor = -1;
out_target:
	zcstripe_target_destroy(dev->target);
	dev->target = NULL;
	return ret;
}

static ssize_t zcstripe_power_show(struct config_item *item, char *page)
{
	struct zcstripe_device *dev = to_zcstripe_device(item);

	return sysfs_emit(page, "%u\n", dev->powered ? 1 : 0);
}

static ssize_t zcstripe_power_store(struct config_item *item, const char *page,
				    size_t count)
{
	struct zcstripe_device *dev = to_zcstripe_device(item);
	bool value;
	int ret = kstrtobool(page, &value);

	if (ret)
		return ret;

	mutex_lock(&dev->lock);
	if (value)
		ret = zcstripe_power_on_locked(dev);
	else
		zcstripe_power_off_locked(dev);
	mutex_unlock(&dev->lock);
	return ret ? ret : count;
}

static ssize_t zcstripe_targets_show(struct config_item *item, char *page)
{
	struct zcstripe_device *dev = to_zcstripe_device(item);

	return sysfs_emit(page, "%s\n", dev->targets);
}

static ssize_t zcstripe_targets_store(struct config_item *item, const char *page,
				      size_t count)
{
	struct zcstripe_device *dev = to_zcstripe_device(item);
	size_t len = strnlen(page, ZCSTRIPE_MAX_SPEC + 1);
	int ret = 0;

	if (!len || len > ZCSTRIPE_MAX_SPEC)
		return -EINVAL;

	mutex_lock(&dev->lock);
	if (dev->powered) {
		ret = -EBUSY;
	} else {
		strscpy(dev->targets, page, sizeof(dev->targets));
		strim(dev->targets);
		if (!dev->targets[0])
			ret = -EINVAL;
	}
	mutex_unlock(&dev->lock);
	return ret ? ret : count;
}

#define ZCSTRIPE_U32_ATTR(_name, _field, _min, _max, _pow2)		\
static ssize_t zcstripe_##_name##_show(struct config_item *item, char *page) \
{									\
	struct zcstripe_device *dev = to_zcstripe_device(item);		\
	return sysfs_emit(page, "%u\n", dev->_field);			\
}									\
static ssize_t zcstripe_##_name##_store(struct config_item *item,	\
					const char *page, size_t count)	\
{									\
	struct zcstripe_device *dev = to_zcstripe_device(item);		\
	u32 value;							\
	int ret = kstrtou32(page, 0, &value);				\
	if (ret)							\
		return ret;						\
	if (value < (_min) || value > (_max))				\
		return -EINVAL;						\
	if ((_pow2) && !is_power_of_2(value))				\
		return -EINVAL;						\
	mutex_lock(&dev->lock);						\
	if (dev->powered)						\
		ret = -EBUSY;						\
	else								\
		dev->_field = value;					\
	mutex_unlock(&dev->lock);					\
	return ret ? ret : count;					\
}

ZCSTRIPE_U32_ATTR(stripe_unit, stripe_unit, 512, UINT_MAX, true);
ZCSTRIPE_U32_ATTR(blocksize, block_size, 512, PAGE_SIZE, true);
ZCSTRIPE_U32_ATTR(queues, queues, 1, 4096, false);
ZCSTRIPE_U32_ATTR(queue_depth, queue_depth, 4, 32768, false);

static ssize_t zcstripe_descriptor_mode_show(struct config_item *item,
					     char *page)
{
	struct zcstripe_device *dev = to_zcstripe_device(item);

	return sysfs_emit(page, "%u\n", dev->descriptor_mode ? 1 : 0);
}

static ssize_t zcstripe_descriptor_mode_store(struct config_item *item,
					      const char *page, size_t count)
{
	struct zcstripe_device *dev = to_zcstripe_device(item);
	bool value;
	int ret = zcstripe_parse_descriptor_mode(page, &value);

	if (ret)
		return ret;

	mutex_lock(&dev->lock);
	if (dev->powered)
		ret = -EBUSY;
	else
		dev->descriptor_mode = value;
	mutex_unlock(&dev->lock);
	return ret ? ret : count;
}

static ssize_t zcstripe_descriptor_abi_show(struct config_item *item, char *page)
{
	struct zcstripe_device *dev = to_zcstripe_device(item);
	u32 features = ZCSTRIPE_DESC_F_TOPOLOGY_HINTS |
		       ZCSTRIPE_DESC_F_RELEASE_TOKEN |
		       ZCSTRIPE_DESC_F_BLOCK_EXTENTS |
		       ZCSTRIPE_DESC_F_STRIPED_TARGET;

	return sysfs_emit(page,
			  "magic=0x%016llx\nversion=%u\nmode=%u\nfeatures=0x%08x\nslice_desc=%zu\nrecord_desc=%zu\nbatch=%zu\nstripe_unit=%u\ntargets=%s\nqueues=%u\n",
			  ZCSTRIPE_DESC_MAGIC, ZCSTRIPE_DESC_VERSION,
			  dev->descriptor_mode ? 1 : 0, features,
			  sizeof(struct zcstripe_slice_desc),
			  sizeof(struct zcstripe_record_desc),
			  sizeof(struct zcstripe_desc_batch), dev->stripe_unit,
			  dev->targets, dev->queues);
}

CONFIGFS_ATTR(zcstripe_, power);
CONFIGFS_ATTR(zcstripe_, targets);
CONFIGFS_ATTR(zcstripe_, stripe_unit);
CONFIGFS_ATTR(zcstripe_, blocksize);
CONFIGFS_ATTR(zcstripe_, queues);
CONFIGFS_ATTR(zcstripe_, queue_depth);
CONFIGFS_ATTR(zcstripe_, descriptor_mode);
CONFIGFS_ATTR_RO(zcstripe_, descriptor_abi);

static struct configfs_attribute *zcstripe_attrs[] = {
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

static void zcstripe_release(struct config_item *item)
{
	struct zcstripe_device *dev = to_zcstripe_device(item);

	kfree(dev);
}

static struct configfs_item_operations zcstripe_item_ops = {
	.release = zcstripe_release,
};

static const struct config_item_type zcstripe_device_type = {
	.ct_item_ops = &zcstripe_item_ops,
	.ct_attrs = zcstripe_attrs,
	.ct_owner = THIS_MODULE,
};

static struct config_group *zcstripe_make_group(struct config_group *group,
						const char *name)
{
	struct zcstripe_device *dev;

	dev = kzalloc(sizeof(*dev), GFP_KERNEL);
	if (!dev)
		return ERR_PTR(-ENOMEM);

	mutex_init(&dev->lock);
	strscpy(dev->name, name, sizeof(dev->name));
	dev->stripe_unit = 4096;
	dev->block_size = 4096;
	dev->queues = 4;
	dev->queue_depth = 256;
	dev->minor = -1;
	config_group_init_type_name(&dev->group, name, &zcstripe_device_type);
	return &dev->group;
}

static void zcstripe_drop_item(struct config_group *group,
			       struct config_item *item)
{
	struct zcstripe_device *dev = to_zcstripe_device(item);

	mutex_lock(&dev->lock);
	zcstripe_power_off_locked(dev);
	mutex_unlock(&dev->lock);
	config_item_put(item);
}

static ssize_t zcstripe_features_show(struct config_item *item, char *page)
{
	return sysfs_emit(page,
			  "power,targets,stripe_unit,blocksize,queues,queue_depth,descriptor_mode,descriptor_abi\n");
}

CONFIGFS_ATTR_RO(zcstripe_, features);

static struct configfs_attribute *zcstripe_root_attrs[] = {
	&zcstripe_attr_features,
	NULL,
};

static struct configfs_group_operations zcstripe_group_ops = {
	.make_group = zcstripe_make_group,
	.drop_item = zcstripe_drop_item,
};

static const struct config_item_type zcstripe_root_type = {
	.ct_group_ops = &zcstripe_group_ops,
	.ct_attrs = zcstripe_root_attrs,
	.ct_owner = THIS_MODULE,
};

static int __init zcstripe_init(void)
{
	int ret;

	zcstripe_major = register_blkdev(0, ZCSTRIPE_NAME);
	if (zcstripe_major < 0)
		return zcstripe_major;

	mutex_init(&zcstripe_subsys.su_mutex);
	config_group_init_type_name(&zcstripe_subsys.su_group, ZCSTRIPE_NAME,
				    &zcstripe_root_type);
	ret = configfs_register_subsystem(&zcstripe_subsys);
	if (ret) {
		unregister_blkdev(zcstripe_major, ZCSTRIPE_NAME);
		zcstripe_major = 0;
	}
	return ret;
}

static void __exit zcstripe_exit(void)
{
	configfs_unregister_subsystem(&zcstripe_subsys);
	unregister_blkdev(zcstripe_major, ZCSTRIPE_NAME);
	ida_destroy(&zcstripe_minors);
}

module_init(zcstripe_init);
module_exit(zcstripe_exit);

MODULE_AUTHOR("Rob Caskey, OpenAI");
MODULE_DESCRIPTION("Zero-copy friendly striped block target");
MODULE_LICENSE("GPL");
