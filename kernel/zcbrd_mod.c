// SPDX-License-Identifier: GPL-2.0

#include <linux/blk-mq.h>
#include <linux/blk_types.h>
#include <linux/blkdev.h>
#include <linux/bvec.h>
#include <linux/configfs.h>
#include <linux/errno.h>
#include <linux/highmem.h>
#include <linux/idr.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/slab.h>
#include <linux/string.h>
#include <linux/types.h>
#include <linux/vmalloc.h>

#define ZCBRD_NAME "zcbrd"
#define ZCBRD_DESC_MAGIC 0x435345445242435aULL
#define ZCBRD_DESC_VERSION 1
#define ZCBRD_DESC_F_TOPOLOGY_HINTS BIT(0)
#define ZCBRD_DESC_F_RELEASE_TOKEN BIT(1)
#define ZCBRD_DESC_F_BLOCK_EXTENTS BIT(2)

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

struct zcbrd_device {
	struct config_group group;
	struct mutex lock;
	bool powered;
	char name[DISK_NAME_LEN];
	u32 block_size;
	u64 capacity_mib;
	u32 queues;
	u32 queue_depth;
	u32 shards;
	bool descriptor_mode;
	u8 *backing;
	size_t backing_len;
	struct blk_mq_tag_set tag_set;
	struct gendisk *disk;
	int minor;
};

static struct configfs_subsystem zcbrd_subsys;
static int zcbrd_major;
static DEFINE_IDA(zcbrd_minors);

static inline struct zcbrd_device *to_zcbrd_device(struct config_item *item)
{
	return container_of(to_config_group(item), struct zcbrd_device, group);
}

static int zcbrd_validate_block_size(u32 value)
{
	if (value < 512 || value > PAGE_SIZE || !is_power_of_2(value))
		return -EINVAL;
	return 0;
}

static int zcbrd_parse_descriptor_mode(const char *page, bool *value)
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

static int zcbrd_transfer_request(struct request *rq, struct zcbrd_device *dev)
{
	struct req_iterator iter;
	struct bio_vec bvec;
	u64 pos = (u64)blk_rq_pos(rq) << SECTOR_SHIFT;
	u64 bytes = blk_rq_bytes(rq);
	u64 transferred = 0;
	enum req_op op = req_op(rq);

	if (op == REQ_OP_FLUSH)
		return 0;

	if (pos > dev->backing_len || bytes > dev->backing_len - pos)
		return -EIO;

	if (op == REQ_OP_DISCARD || op == REQ_OP_WRITE_ZEROES) {
		memset(dev->backing + pos, 0, bytes);
		return 0;
	}

	if (op != REQ_OP_READ && op != REQ_OP_WRITE)
		return -EOPNOTSUPP;

	rq_for_each_segment(bvec, rq, iter) {
		unsigned int len = bvec.bv_len;
		void *mapped;

		if (transferred + len > bytes)
			len = bytes - transferred;
		if (!len)
			break;

		mapped = bvec_kmap_local(&bvec);
		if (op == REQ_OP_WRITE) {
			flush_dcache_page(bvec.bv_page);
			memcpy(dev->backing + pos + transferred, mapped, len);
		} else {
			memcpy(mapped, dev->backing + pos + transferred, len);
			flush_dcache_page(bvec.bv_page);
		}
		kunmap_local(mapped);

		transferred += len;
		if (transferred >= bytes)
			break;
	}

	return transferred == bytes ? 0 : -EIO;
}

static blk_status_t zcbrd_queue_rq(struct blk_mq_hw_ctx *hctx,
				   const struct blk_mq_queue_data *bd)
{
	struct request *rq = bd->rq;
	struct zcbrd_device *dev = rq->q->queuedata;
	int ret;

	blk_mq_start_request(rq);
	ret = zcbrd_transfer_request(rq, dev);
	blk_mq_end_request(rq, errno_to_blk_status(ret));
	return BLK_STS_OK;
}

static const struct blk_mq_ops zcbrd_mq_ops = {
	.queue_rq = zcbrd_queue_rq,
};

static const struct block_device_operations zcbrd_fops = {
	.owner = THIS_MODULE,
};

static void zcbrd_power_off_locked(struct zcbrd_device *dev)
{
	if (!dev->powered)
		return;

	if (dev->disk) {
		del_gendisk(dev->disk);
		put_disk(dev->disk);
		dev->disk = NULL;
	}
	blk_mq_free_tag_set(&dev->tag_set);
	vfree(dev->backing);
	dev->backing = NULL;
	dev->backing_len = 0;
	if (dev->minor >= 0) {
		ida_free(&zcbrd_minors, dev->minor);
		dev->minor = -1;
	}
	dev->powered = false;
}

static int zcbrd_power_on_locked(struct zcbrd_device *dev)
{
	struct queue_limits lim = {
		.logical_block_size = dev->block_size,
		.physical_block_size = dev->block_size,
	};
	u64 capacity_bytes;
	int ret;

	if (dev->powered)
		return 0;
	if (check_mul_overflow(dev->capacity_mib, 1024ULL * 1024ULL,
			       &capacity_bytes))
		return -EOVERFLOW;
	if (capacity_bytes > SIZE_MAX)
		return -EOVERFLOW;

	dev->backing = vzalloc(capacity_bytes);
	if (!dev->backing)
		return -ENOMEM;
	dev->backing_len = capacity_bytes;

	dev->minor = ida_alloc(&zcbrd_minors, GFP_KERNEL);
	if (dev->minor < 0) {
		ret = dev->minor;
		goto out_backing;
	}

	memset(&dev->tag_set, 0, sizeof(dev->tag_set));
	dev->tag_set.ops = &zcbrd_mq_ops;
	dev->tag_set.nr_hw_queues = dev->queues;
	dev->tag_set.queue_depth = dev->queue_depth;
	dev->tag_set.numa_node = NUMA_NO_NODE;
	dev->tag_set.cmd_size = 0;
	dev->tag_set.flags = BLK_MQ_F_NO_SCHED_BY_DEFAULT;
	ret = blk_mq_alloc_tag_set(&dev->tag_set);
	if (ret)
		goto out_minor;

	dev->disk = blk_mq_alloc_disk(&dev->tag_set, &lim, dev);
	if (IS_ERR(dev->disk)) {
		ret = PTR_ERR(dev->disk);
		dev->disk = NULL;
		goto out_tagset;
	}

	dev->disk->major = zcbrd_major;
	dev->disk->first_minor = dev->minor;
	dev->disk->minors = 1;
	dev->disk->fops = &zcbrd_fops;
	dev->disk->flags |= GENHD_FL_NO_PART;
	snprintf(dev->disk->disk_name, DISK_NAME_LEN, "%s", dev->name);
	set_capacity(dev->disk, capacity_bytes >> SECTOR_SHIFT);
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
	ida_free(&zcbrd_minors, dev->minor);
	dev->minor = -1;
out_backing:
	vfree(dev->backing);
	dev->backing = NULL;
	dev->backing_len = 0;
	return ret;
}

static ssize_t zcbrd_power_show(struct config_item *item, char *page)
{
	struct zcbrd_device *dev = to_zcbrd_device(item);
	bool powered;

	mutex_lock(&dev->lock);
	powered = dev->powered;
	mutex_unlock(&dev->lock);
	return sysfs_emit(page, "%u\n", powered ? 1 : 0);
}

static ssize_t zcbrd_power_store(struct config_item *item, const char *page,
				 size_t count)
{
	struct zcbrd_device *dev = to_zcbrd_device(item);
	bool value;
	int ret;

	ret = kstrtobool(page, &value);
	if (ret)
		return ret;

	mutex_lock(&dev->lock);
	if (value)
		ret = zcbrd_power_on_locked(dev);
	else
		zcbrd_power_off_locked(dev);
	mutex_unlock(&dev->lock);

	return ret ? ret : count;
}

static ssize_t zcbrd_blocksize_show(struct config_item *item, char *page)
{
	struct zcbrd_device *dev = to_zcbrd_device(item);

	return sysfs_emit(page, "%u\n", dev->block_size);
}

static ssize_t zcbrd_blocksize_store(struct config_item *item, const char *page,
				     size_t count)
{
	struct zcbrd_device *dev = to_zcbrd_device(item);
	u32 value;
	int ret;

	ret = kstrtou32(page, 0, &value);
	if (ret)
		return ret;
	ret = zcbrd_validate_block_size(value);
	if (ret)
		return ret;

	mutex_lock(&dev->lock);
	if (dev->powered)
		ret = -EBUSY;
	else
		dev->block_size = value;
	mutex_unlock(&dev->lock);
	return ret ? ret : count;
}

static ssize_t zcbrd_size_mib_show(struct config_item *item, char *page)
{
	struct zcbrd_device *dev = to_zcbrd_device(item);

	return sysfs_emit(page, "%llu\n", dev->capacity_mib);
}

static ssize_t zcbrd_size_mib_store(struct config_item *item, const char *page,
				    size_t count)
{
	struct zcbrd_device *dev = to_zcbrd_device(item);
	u64 value;
	int ret = kstrtou64(page, 0, &value);

	if (ret)
		return ret;
	if (!value)
		return -EINVAL;

	mutex_lock(&dev->lock);
	if (dev->powered)
		ret = -EBUSY;
	else
		dev->capacity_mib = value;
	mutex_unlock(&dev->lock);
	return ret ? ret : count;
}

static ssize_t zcbrd_queues_show(struct config_item *item, char *page)
{
	struct zcbrd_device *dev = to_zcbrd_device(item);

	return sysfs_emit(page, "%u\n", dev->queues);
}

static ssize_t zcbrd_queues_store(struct config_item *item, const char *page,
				  size_t count)
{
	struct zcbrd_device *dev = to_zcbrd_device(item);
	u32 value;
	int ret = kstrtou32(page, 0, &value);

	if (ret)
		return ret;
	if (!value || value > 4096)
		return -EINVAL;

	mutex_lock(&dev->lock);
	if (dev->powered)
		ret = -EBUSY;
	else
		dev->queues = value;
	mutex_unlock(&dev->lock);
	return ret ? ret : count;
}

static ssize_t zcbrd_queue_depth_show(struct config_item *item, char *page)
{
	struct zcbrd_device *dev = to_zcbrd_device(item);

	return sysfs_emit(page, "%u\n", dev->queue_depth);
}

static ssize_t zcbrd_queue_depth_store(struct config_item *item,
				       const char *page, size_t count)
{
	struct zcbrd_device *dev = to_zcbrd_device(item);
	u32 value;
	int ret = kstrtou32(page, 0, &value);

	if (ret)
		return ret;
	if (value < 4 || value > 32768)
		return -EINVAL;

	mutex_lock(&dev->lock);
	if (dev->powered)
		ret = -EBUSY;
	else
		dev->queue_depth = value;
	mutex_unlock(&dev->lock);
	return ret ? ret : count;
}

static ssize_t zcbrd_shards_show(struct config_item *item, char *page)
{
	struct zcbrd_device *dev = to_zcbrd_device(item);

	return sysfs_emit(page, "%u\n", dev->shards);
}

static ssize_t zcbrd_shards_store(struct config_item *item, const char *page,
				  size_t count)
{
	struct zcbrd_device *dev = to_zcbrd_device(item);
	u32 value;
	int ret = kstrtou32(page, 0, &value);

	if (ret)
		return ret;
	if (!value || value > 65536)
		return -EINVAL;

	mutex_lock(&dev->lock);
	if (dev->powered)
		ret = -EBUSY;
	else
		dev->shards = value;
	mutex_unlock(&dev->lock);
	return ret ? ret : count;
}

static ssize_t zcbrd_descriptor_mode_show(struct config_item *item, char *page)
{
	struct zcbrd_device *dev = to_zcbrd_device(item);

	return sysfs_emit(page, "%u\n", dev->descriptor_mode ? 1 : 0);
}

static ssize_t zcbrd_descriptor_mode_store(struct config_item *item,
					   const char *page, size_t count)
{
	struct zcbrd_device *dev = to_zcbrd_device(item);
	bool value;
	int ret = zcbrd_parse_descriptor_mode(page, &value);

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

static ssize_t zcbrd_descriptor_abi_show(struct config_item *item, char *page)
{
	struct zcbrd_device *dev = to_zcbrd_device(item);
	u32 features = ZCBRD_DESC_F_TOPOLOGY_HINTS | ZCBRD_DESC_F_RELEASE_TOKEN |
		       ZCBRD_DESC_F_BLOCK_EXTENTS;

	return sysfs_emit(page,
			  "magic=0x%016llx\nversion=%u\nmode=%u\nfeatures=0x%08x\nslice_desc=%zu\nrecord_desc=%zu\nbatch=%zu\nqueues=%u\nshards=%u\n",
			  ZCBRD_DESC_MAGIC, ZCBRD_DESC_VERSION,
			  dev->descriptor_mode ? 1 : 0, features,
			  sizeof(struct zcbrd_slice_desc),
			  sizeof(struct zcbrd_record_desc),
			  sizeof(struct zcbrd_desc_batch), dev->queues,
			  dev->shards);
}

CONFIGFS_ATTR(zcbrd_, power);
CONFIGFS_ATTR(zcbrd_, blocksize);
CONFIGFS_ATTR(zcbrd_, size_mib);
CONFIGFS_ATTR(zcbrd_, queues);
CONFIGFS_ATTR(zcbrd_, queue_depth);
CONFIGFS_ATTR(zcbrd_, shards);
CONFIGFS_ATTR(zcbrd_, descriptor_mode);
CONFIGFS_ATTR_RO(zcbrd_, descriptor_abi);

static struct configfs_attribute *zcbrd_attrs[] = {
	&zcbrd_attr_power,
	&zcbrd_attr_blocksize,
	&zcbrd_attr_size_mib,
	&zcbrd_attr_queues,
	&zcbrd_attr_queue_depth,
	&zcbrd_attr_shards,
	&zcbrd_attr_descriptor_mode,
	&zcbrd_attr_descriptor_abi,
	NULL,
};

static void zcbrd_release(struct config_item *item)
{
	struct zcbrd_device *dev = to_zcbrd_device(item);

	kfree(dev);
}

static struct configfs_item_operations zcbrd_item_ops = {
	.release = zcbrd_release,
};

static const struct config_item_type zcbrd_device_type = {
	.ct_item_ops = &zcbrd_item_ops,
	.ct_attrs = zcbrd_attrs,
	.ct_owner = THIS_MODULE,
};

static struct config_group *zcbrd_make_group(struct config_group *group,
					     const char *name)
{
	struct zcbrd_device *dev;

	dev = kzalloc(sizeof(*dev), GFP_KERNEL);
	if (!dev)
		return ERR_PTR(-ENOMEM);

	mutex_init(&dev->lock);
	strscpy(dev->name, name, sizeof(dev->name));
	dev->block_size = 4096;
	dev->capacity_mib = 64;
	dev->queues = 4;
	dev->queue_depth = 256;
	dev->shards = 4;
	dev->minor = -1;
	config_group_init_type_name(&dev->group, name, &zcbrd_device_type);
	return &dev->group;
}

static void zcbrd_drop_item(struct config_group *group, struct config_item *item)
{
	struct zcbrd_device *dev = to_zcbrd_device(item);

	mutex_lock(&dev->lock);
	zcbrd_power_off_locked(dev);
	mutex_unlock(&dev->lock);
	config_item_put(item);
}

static ssize_t zcbrd_features_show(struct config_item *item, char *page)
{
	return sysfs_emit(page,
			  "power,blocksize,size_mib,queues,queue_depth,shards,descriptor_mode,descriptor_abi\n");
}

CONFIGFS_ATTR_RO(zcbrd_, features);

static struct configfs_attribute *zcbrd_root_attrs[] = {
	&zcbrd_attr_features,
	NULL,
};

static struct configfs_group_operations zcbrd_group_ops = {
	.make_group = zcbrd_make_group,
	.drop_item = zcbrd_drop_item,
};

static const struct config_item_type zcbrd_root_type = {
	.ct_group_ops = &zcbrd_group_ops,
	.ct_attrs = zcbrd_root_attrs,
	.ct_owner = THIS_MODULE,
};

static int __init zcbrd_init(void)
{
	int ret;

	zcbrd_major = register_blkdev(0, ZCBRD_NAME);
	if (zcbrd_major < 0)
		return zcbrd_major;

	mutex_init(&zcbrd_subsys.su_mutex);
	config_group_init_type_name(&zcbrd_subsys.su_group, ZCBRD_NAME,
				    &zcbrd_root_type);
	ret = configfs_register_subsystem(&zcbrd_subsys);
	if (ret) {
		unregister_blkdev(zcbrd_major, ZCBRD_NAME);
		zcbrd_major = 0;
	}
	return ret;
}

static void __exit zcbrd_exit(void)
{
	configfs_unregister_subsystem(&zcbrd_subsys);
	unregister_blkdev(zcbrd_major, ZCBRD_NAME);
	ida_destroy(&zcbrd_minors);
}

module_init(zcbrd_init);
module_exit(zcbrd_exit);

MODULE_AUTHOR("Rob Caskey, OpenAI");
MODULE_DESCRIPTION("Zero-copy friendly RAM block device");
MODULE_LICENSE("GPL");
