#define _GNU_SOURCE

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <infiniband/verbs.h>
#include <linux/fs.h>
#include <liburing.h>
#include <netdb.h>
#include <netinet/in.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#define TREE_MAX_REFS 128
#define TREE_MAX_LANES 64

enum ring_interleave_mode {
	RING_INTERLEAVE_SLOT,
	RING_INTERLEAVE_ROUNDROBIN,
};

struct options {
	bool server;
	bool tree_server;
	const char *host;
	const char *service;
	const char *ib_dev;
	int ib_port;
	int gid_idx;
	const char *sink;
	const char *path;
	size_t size;
	size_t count;
	size_t pipeline;
	size_t ring_entries;
	size_t submit_batch;
	size_t wc_batch;
	size_t signal_every;
	size_t uring_rings;
	size_t coalesce;
	size_t lanes;
	size_t middle_count;
	size_t leaf_coalesce;
	size_t queue_depth;
	enum ring_interleave_mode ring_interleave;
	bool direct;
	int cpu;
};

struct qp_wire {
	uint32_t qpn;
	uint32_t psn;
	uint16_t lid;
	uint8_t gid[16];
};

struct resources {
	struct ibv_context *ctx;
	struct ibv_pd *pd;
	struct ibv_cq *cq;
	struct ibv_qp *qp;
	struct ibv_mr *mr;
	struct ibv_port_attr port_attr;
	union ibv_gid gid;
	void *buf;
	size_t buf_len;
	uint32_t psn;
};

static void die_errno(const char *what)
{
	fprintf(stderr, "%s: %s\n", what, strerror(errno));
	exit(1);
}

static void die(const char *what)
{
	fprintf(stderr, "%s\n", what);
	exit(1);
}

static double now_seconds(void)
{
	struct timespec ts;
	if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0)
		die_errno("clock_gettime");
	return (double)ts.tv_sec + (double)ts.tv_nsec / 1000000000.0;
}

static size_t parse_size(const char *raw)
{
	char *end = NULL;
	errno = 0;
	unsigned long long value = strtoull(raw, &end, 10);
	if (errno || end == raw)
		die_errno("parse size");
	if (*end == 'k' || *end == 'K')
		value *= 1024ull;
	else if (*end == 'm' || *end == 'M')
		value *= 1024ull * 1024ull;
	else if (*end == 'g' || *end == 'G')
		value *= 1024ull * 1024ull * 1024ull;
	return (size_t)value;
}

static enum ring_interleave_mode parse_ring_interleave(const char *raw)
{
	if (strcmp(raw, "slot") == 0 || strcmp(raw, "slot-hash") == 0)
		return RING_INTERLEAVE_SLOT;
	if (strcmp(raw, "rr") == 0 || strcmp(raw, "round-robin") == 0 ||
	    strcmp(raw, "interleave") == 0)
		return RING_INTERLEAVE_ROUNDROBIN;
	fprintf(stderr, "unknown ring interleave mode %s; use slot or round-robin\n", raw);
	exit(2);
}

static const char *ring_interleave_name(enum ring_interleave_mode mode)
{
	switch (mode) {
	case RING_INTERLEAVE_SLOT:
		return "slot";
	case RING_INTERLEAVE_ROUNDROBIN:
		return "round-robin";
	}
	return "unknown";
}

static void usage(const char *argv0)
{
	fprintf(stderr,
		"usage:\n"
		"  %s server [--service port] [--ib-dev rxe0] [--gid-idx 1]\n"
		"     [--sink discard|block|uring-block] [--path /dev/nullb0] [--size bytes]\n"
		"     [--count n] [--pipeline n] [--ring-entries n] [--submit-batch n]\n"
		"     [--wc-batch n] [--signal-every n] [--uring-rings n] [--coalesce n]\n"
		"     [--ring-interleave slot|round-robin] [--direct] [--cpu n]\n"
		"  %s tree-server [--service base-port] [--lanes 4] [--middle 2]\n"
		"     [--leaf-coalesce n] [--queue-depth n] [same server RDMA/WAL options]\n"
		"  %s client --host addr [same service/ib-dev/gid/size/count/pipeline/cpu]\n",
		argv0, argv0, argv0);
	exit(2);
}

static struct options parse_args(int argc, char **argv)
{
	if (argc < 2)
		usage(argv[0]);

	struct options opt = {
		.server = strcmp(argv[1], "server") == 0 ||
			  strcmp(argv[1], "tree-server") == 0,
		.tree_server = strcmp(argv[1], "tree-server") == 0,
		.host = "127.0.0.1",
		.service = "48611",
		.ib_dev = "rxe0",
		.ib_port = 1,
		.gid_idx = 1,
		.sink = "discard",
		.path = "/dev/nullb0",
		.size = 4096,
		.count = 1024 * 1024,
		.pipeline = 128,
		.ring_entries = 1024,
		.submit_batch = 32,
		.wc_batch = 32,
		.signal_every = 1,
		.uring_rings = 1,
		.coalesce = 1,
		.lanes = 4,
		.middle_count = 0,
		.leaf_coalesce = 4,
		.queue_depth = 0,
		.ring_interleave = RING_INTERLEAVE_SLOT,
		.direct = false,
		.cpu = -1,
	};
	if (!opt.server && strcmp(argv[1], "client") != 0)
		usage(argv[0]);

	for (int i = 2; i < argc; i++) {
		if (strcmp(argv[i], "--host") == 0 && i + 1 < argc)
			opt.host = argv[++i];
		else if (strcmp(argv[i], "--service") == 0 && i + 1 < argc)
			opt.service = argv[++i];
		else if (strcmp(argv[i], "--ib-dev") == 0 && i + 1 < argc)
			opt.ib_dev = argv[++i];
		else if (strcmp(argv[i], "--ib-port") == 0 && i + 1 < argc)
			opt.ib_port = atoi(argv[++i]);
		else if (strcmp(argv[i], "--gid-idx") == 0 && i + 1 < argc)
			opt.gid_idx = atoi(argv[++i]);
		else if (strcmp(argv[i], "--sink") == 0 && i + 1 < argc)
			opt.sink = argv[++i];
		else if (strcmp(argv[i], "--path") == 0 && i + 1 < argc)
			opt.path = argv[++i];
		else if (strcmp(argv[i], "--size") == 0 && i + 1 < argc)
			opt.size = parse_size(argv[++i]);
		else if (strcmp(argv[i], "--count") == 0 && i + 1 < argc)
			opt.count = parse_size(argv[++i]);
		else if (strcmp(argv[i], "--pipeline") == 0 && i + 1 < argc)
			opt.pipeline = parse_size(argv[++i]);
		else if (strcmp(argv[i], "--ring-entries") == 0 && i + 1 < argc)
			opt.ring_entries = parse_size(argv[++i]);
		else if (strcmp(argv[i], "--submit-batch") == 0 && i + 1 < argc)
			opt.submit_batch = parse_size(argv[++i]);
		else if (strcmp(argv[i], "--wc-batch") == 0 && i + 1 < argc)
			opt.wc_batch = parse_size(argv[++i]);
		else if (strcmp(argv[i], "--signal-every") == 0 && i + 1 < argc)
			opt.signal_every = parse_size(argv[++i]);
		else if (strcmp(argv[i], "--uring-rings") == 0 && i + 1 < argc)
			opt.uring_rings = parse_size(argv[++i]);
		else if (strcmp(argv[i], "--coalesce") == 0 && i + 1 < argc)
			opt.coalesce = parse_size(argv[++i]);
		else if (strcmp(argv[i], "--lanes") == 0 && i + 1 < argc)
			opt.lanes = parse_size(argv[++i]);
		else if (strcmp(argv[i], "--middle") == 0 && i + 1 < argc)
			opt.middle_count = parse_size(argv[++i]);
		else if (strcmp(argv[i], "--leaf-coalesce") == 0 && i + 1 < argc)
			opt.leaf_coalesce = parse_size(argv[++i]);
		else if (strcmp(argv[i], "--queue-depth") == 0 && i + 1 < argc)
			opt.queue_depth = parse_size(argv[++i]);
		else if (strcmp(argv[i], "--ring-interleave") == 0 && i + 1 < argc)
			opt.ring_interleave = parse_ring_interleave(argv[++i]);
		else if (strcmp(argv[i], "--direct") == 0)
			opt.direct = true;
		else if (strcmp(argv[i], "--cpu") == 0 && i + 1 < argc)
			opt.cpu = atoi(argv[++i]);
		else
			usage(argv[0]);
	}
	if (opt.size == 0 || opt.count == 0 || opt.pipeline == 0 ||
	    opt.ring_entries == 0 || opt.submit_batch == 0 ||
	    opt.wc_batch == 0 || opt.signal_every == 0 ||
	    opt.uring_rings == 0 || opt.coalesce == 0 ||
	    opt.lanes == 0 || opt.leaf_coalesce == 0)
		usage(argv[0]);
	if (opt.middle_count == 0)
		opt.middle_count = (opt.lanes + 1) / 2;
	if (opt.middle_count == 0 || opt.middle_count > opt.lanes ||
	    opt.lanes > TREE_MAX_LANES || opt.coalesce > TREE_MAX_REFS ||
	    opt.leaf_coalesce > TREE_MAX_REFS)
		usage(argv[0]);
	return opt;
}

static void maybe_pin_cpu(int cpu)
{
	if (cpu < 0)
		return;
	cpu_set_t set;
	CPU_ZERO(&set);
	CPU_SET(cpu, &set);
	if (sched_setaffinity(0, sizeof(set), &set) != 0)
		die_errno("sched_setaffinity");
}

static void send_all(int fd, const void *buf, size_t len)
{
	const char *p = buf;
	while (len) {
		ssize_t ret = send(fd, p, len, MSG_NOSIGNAL);
		if (ret < 0)
			die_errno("send");
		if (ret == 0)
			die("send returned zero");
		p += ret;
		len -= (size_t)ret;
	}
}

static void recv_all(int fd, void *buf, size_t len)
{
	char *p = buf;
	while (len) {
		ssize_t ret = recv(fd, p, len, MSG_WAITALL);
		if (ret < 0)
			die_errno("recv");
		if (ret == 0)
			die("peer closed control socket");
		p += ret;
		len -= (size_t)ret;
	}
}

static int server_listen(const char *service, int backlog)
{
	int fd = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
	if (fd < 0)
		die_errno("socket");
	int one = 1;
	if (setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one)) != 0)
		die_errno("setsockopt SO_REUSEADDR");

	struct sockaddr_in addr = {0};
	addr.sin_family = AF_INET;
	addr.sin_port = htons((uint16_t)atoi(service));
	addr.sin_addr.s_addr = htonl(INADDR_ANY);
	if (bind(fd, (struct sockaddr *)&addr, sizeof(addr)) != 0)
		die_errno("bind");
	if (listen(fd, backlog) != 0)
		die_errno("listen");
	return fd;
}

static int server_accept_fd(int listen_fd)
{
	int conn = accept4(listen_fd, NULL, NULL, SOCK_CLOEXEC);
	if (conn < 0)
		die_errno("accept4");
	return conn;
}

static int server_accept(const char *service)
{
	int fd = server_listen(service, 1);
	int conn = server_accept_fd(fd);
	close(fd);
	return conn;
}

static int client_connect(const char *host, const char *service)
{
	struct addrinfo hints = {
		.ai_family = AF_INET,
		.ai_socktype = SOCK_STREAM,
	};
	struct addrinfo *res = NULL;
	int gai = getaddrinfo(host, service, &hints, &res);
	if (gai != 0) {
		fprintf(stderr, "getaddrinfo: %s\n", gai_strerror(gai));
		exit(1);
	}

	int fd = -1;
	for (struct addrinfo *ai = res; ai; ai = ai->ai_next) {
		fd = socket(ai->ai_family, ai->ai_socktype | SOCK_CLOEXEC, ai->ai_protocol);
		if (fd < 0)
			continue;
		if (connect(fd, ai->ai_addr, ai->ai_addrlen) == 0)
			break;
		close(fd);
		fd = -1;
	}
	freeaddrinfo(res);
	if (fd < 0)
		die_errno("connect");
	return fd;
}

static uint32_t make_psn(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (uint32_t)((ts.tv_nsec ^ (getpid() << 8)) & 0xffffff);
}

static void init_resources(const struct options *opt, struct resources *res, size_t slots)
{
	int ndev = 0;
	struct ibv_device **list = ibv_get_device_list(&ndev);
	if (!list || ndev == 0)
		die("no ibverbs devices found");

	struct ibv_device *dev = NULL;
	for (int i = 0; i < ndev; i++) {
		if (!opt->ib_dev || strcmp(ibv_get_device_name(list[i]), opt->ib_dev) == 0) {
			dev = list[i];
			break;
		}
	}
	if (!dev) {
		fprintf(stderr, "ibverbs device not found: %s\n", opt->ib_dev);
		exit(1);
	}

	res->ctx = ibv_open_device(dev);
	if (!res->ctx)
		die("ibv_open_device failed");
	ibv_free_device_list(list);

	if (ibv_query_port(res->ctx, opt->ib_port, &res->port_attr) != 0)
		die("ibv_query_port failed");
	if (ibv_query_gid(res->ctx, opt->ib_port, opt->gid_idx, &res->gid) != 0)
		die("ibv_query_gid failed");

	res->pd = ibv_alloc_pd(res->ctx);
	if (!res->pd)
		die("ibv_alloc_pd failed");
	res->cq = ibv_create_cq(res->ctx, (int)(slots + 32), NULL, NULL, 0);
	if (!res->cq)
		die("ibv_create_cq failed");

	res->buf_len = slots * opt->size;
	if (posix_memalign(&res->buf, 4096, res->buf_len) != 0)
		die_errno("posix_memalign");
	memset(res->buf, opt->server ? 0 : 0xa5, res->buf_len);
	res->mr = ibv_reg_mr(res->pd, res->buf, res->buf_len, IBV_ACCESS_LOCAL_WRITE);
	if (!res->mr)
		die("ibv_reg_mr failed");

	struct ibv_qp_init_attr qpia = {
		.send_cq = res->cq,
		.recv_cq = res->cq,
		.cap = {
			.max_send_wr = (uint32_t)(slots + 1),
			.max_recv_wr = (uint32_t)(slots + 1),
			.max_send_sge = 1,
			.max_recv_sge = 1,
		},
		.qp_type = IBV_QPT_RC,
	};
	res->qp = ibv_create_qp(res->pd, &qpia);
	if (!res->qp)
		die("ibv_create_qp failed");

	res->psn = make_psn();
}

static void local_wire(const struct resources *res, struct qp_wire *wire)
{
	memset(wire, 0, sizeof(*wire));
	wire->qpn = htonl(res->qp->qp_num);
	wire->psn = htonl(res->psn);
	wire->lid = htons(res->port_attr.lid);
	memcpy(wire->gid, res->gid.raw, sizeof(wire->gid));
}

static void to_rtr_rts(const struct options *opt, struct resources *res,
		       const struct qp_wire *remote_wire)
{
	uint32_t remote_qpn = ntohl(remote_wire->qpn);
	uint32_t remote_psn = ntohl(remote_wire->psn);
	uint16_t remote_lid = ntohs(remote_wire->lid);
	union ibv_gid remote_gid;
	memcpy(remote_gid.raw, remote_wire->gid, sizeof(remote_gid.raw));

	struct ibv_qp_attr init = {
		.qp_state = IBV_QPS_INIT,
		.pkey_index = 0,
		.port_num = (uint8_t)opt->ib_port,
		.qp_access_flags = 0,
	};
	if (ibv_modify_qp(res->qp, &init,
			  IBV_QP_STATE | IBV_QP_PKEY_INDEX | IBV_QP_PORT |
				  IBV_QP_ACCESS_FLAGS) != 0)
		die("ibv_modify_qp INIT failed");

	struct ibv_qp_attr rtr = {
		.qp_state = IBV_QPS_RTR,
		.path_mtu = IBV_MTU_4096,
		.dest_qp_num = remote_qpn,
		.rq_psn = remote_psn,
		.max_dest_rd_atomic = 1,
		.min_rnr_timer = 12,
		.ah_attr = {
			.is_global = 1,
			.dlid = remote_lid,
			.sl = 0,
			.src_path_bits = 0,
			.port_num = (uint8_t)opt->ib_port,
			.grh = {
				.dgid = remote_gid,
				.sgid_index = (uint8_t)opt->gid_idx,
				.hop_limit = 1,
			},
		},
	};
	if (ibv_modify_qp(res->qp, &rtr,
			  IBV_QP_STATE | IBV_QP_AV | IBV_QP_PATH_MTU |
				  IBV_QP_DEST_QPN | IBV_QP_RQ_PSN |
				  IBV_QP_MAX_DEST_RD_ATOMIC | IBV_QP_MIN_RNR_TIMER) != 0)
		die("ibv_modify_qp RTR failed");

	struct ibv_qp_attr rts = {
		.qp_state = IBV_QPS_RTS,
		.timeout = 14,
		.retry_cnt = 7,
		.rnr_retry = 7,
		.sq_psn = res->psn,
		.max_rd_atomic = 1,
	};
	if (ibv_modify_qp(res->qp, &rts,
			  IBV_QP_STATE | IBV_QP_TIMEOUT | IBV_QP_RETRY_CNT |
				  IBV_QP_RNR_RETRY | IBV_QP_SQ_PSN |
				  IBV_QP_MAX_QP_RD_ATOMIC) != 0)
		die("ibv_modify_qp RTS failed");
}

static void post_recv_slot(struct resources *res, size_t idx, size_t size)
{
	struct ibv_sge sge = {
		.addr = (uintptr_t)((char *)res->buf + idx * size),
		.length = (uint32_t)size,
		.lkey = res->mr->lkey,
	};
	struct ibv_recv_wr wr = {
		.wr_id = idx,
		.sg_list = &sge,
		.num_sge = 1,
	};
	struct ibv_recv_wr *bad = NULL;
	if (ibv_post_recv(res->qp, &wr, &bad) != 0)
		die("ibv_post_recv failed");
}

static void post_send_slot(struct resources *res, size_t idx, size_t size,
			   uint64_t wr_id, bool signaled)
{
	struct ibv_sge sge = {
		.addr = (uintptr_t)((char *)res->buf + idx * size),
		.length = (uint32_t)size,
		.lkey = res->mr->lkey,
	};
	struct ibv_send_wr wr = {
		.wr_id = wr_id,
		.sg_list = &sge,
		.num_sge = 1,
		.opcode = IBV_WR_SEND,
		.send_flags = signaled ? IBV_SEND_SIGNALED : 0,
	};
	struct ibv_send_wr *bad = NULL;
	if (ibv_post_send(res->qp, &wr, &bad) != 0)
		die("ibv_post_send failed");
}

static void poll_one(struct resources *res, struct ibv_wc *wc)
{
	for (;;) {
		int ret = ibv_poll_cq(res->cq, 1, wc);
		if (ret < 0)
			die("ibv_poll_cq failed");
		if (ret > 0)
			break;
	}
	if (wc->status != IBV_WC_SUCCESS) {
		fprintf(stderr, "work completion failed: status=%s opcode=%d wr_id=%llu\n",
			ibv_wc_status_str(wc->status), wc->opcode,
			(unsigned long long)wc->wr_id);
		exit(1);
	}
}

static int open_sink(const struct options *opt, uint64_t *capacity)
{
	*capacity = 0;
	if (strcmp(opt->sink, "discard") == 0)
		return -1;
	if (strcmp(opt->sink, "block") != 0 && strcmp(opt->sink, "uring-block") != 0) {
		fprintf(stderr, "unknown sink %s\n", opt->sink);
		exit(2);
	}

	int flags = O_WRONLY | O_CLOEXEC;
	if (opt->direct)
		flags |= O_DIRECT;
	int fd = open(opt->path, flags);
	if (fd < 0)
		die_errno(opt->path);
	if (ioctl(fd, BLKGETSIZE64, capacity) != 0)
		*capacity = 0;
	return fd;
}

struct uring_sink_ring {
	struct io_uring ring;
	size_t pending_submit;
};

struct write_group {
	size_t nr;
	size_t bytes;
	size_t *slots;
	struct iovec *iovecs;
	size_t ring;
	bool active;
};

struct uring_sink {
	bool enabled;
	int fd;
	uint64_t capacity;
	uint64_t offset;
	size_t ring_count;
	size_t next_ring;
	struct uring_sink_ring *rings;
	size_t group_capacity;
	size_t coalesce;
	struct write_group *groups;
	size_t *group_slot_storage;
	struct iovec *group_iovec_storage;
	size_t *free_groups;
	size_t free_group_count;
};

static bool sink_uses_uring(const struct options *opt)
{
	return strcmp(opt->sink, "uring-block") == 0;
}

static void uring_sink_init(struct uring_sink *sink, const struct options *opt,
			    const struct resources *res, size_t slots)
{
	memset(sink, 0, sizeof(*sink));
	if (!sink_uses_uring(opt))
		return;

	sink->enabled = true;
	sink->fd = open_sink(opt, &sink->capacity);
	if (sink->fd < 0)
		die("uring sink requires a block path");

	unsigned entries = (unsigned)opt->ring_entries;
	if (entries < slots + 32)
		entries = (unsigned)(slots + 32);
	sink->ring_count = opt->uring_rings;
	sink->coalesce = opt->coalesce;
	if (sink->coalesce > slots)
		sink->coalesce = slots;
	sink->rings = calloc(sink->ring_count, sizeof(*sink->rings));
	if (!sink->rings)
		die_errno("calloc rings");

	struct iovec reg_iovec = {
		.iov_base = res->buf,
		.iov_len = res->buf_len,
	};
	int files[1] = { sink->fd };
	for (size_t i = 0; i < sink->ring_count; i++) {
		if (io_uring_queue_init(entries, &sink->rings[i].ring, 0) != 0)
			die("io_uring_queue_init failed");
		if (io_uring_register_files(&sink->rings[i].ring, files, 1) != 0)
			die_errno("io_uring_register_files");
		if (io_uring_register_buffers(&sink->rings[i].ring, &reg_iovec, 1) != 0)
			die_errno("io_uring_register_buffers");
	}

	sink->group_capacity = slots + sink->ring_count + 8;
	sink->groups = calloc(sink->group_capacity, sizeof(*sink->groups));
	sink->group_slot_storage =
		calloc(sink->group_capacity * sink->coalesce, sizeof(*sink->group_slot_storage));
	sink->group_iovec_storage =
		calloc(sink->group_capacity * sink->coalesce, sizeof(*sink->group_iovec_storage));
	sink->free_groups = calloc(sink->group_capacity, sizeof(*sink->free_groups));
	if (!sink->groups || !sink->group_slot_storage || !sink->group_iovec_storage ||
	    !sink->free_groups)
		die_errno("calloc write groups");
	for (size_t i = 0; i < sink->group_capacity; i++) {
		sink->groups[i].slots = sink->group_slot_storage + i * sink->coalesce;
		sink->groups[i].iovecs = sink->group_iovec_storage + i * sink->coalesce;
		sink->free_groups[sink->free_group_count++] = sink->group_capacity - 1 - i;
	}
}

static void uring_sink_submit_ring(struct uring_sink_ring *ring)
{
	while (ring->pending_submit) {
		int ret = io_uring_submit(&ring->ring);
		if (ret < 0) {
			errno = -ret;
			die_errno("io_uring_submit");
		}
		if (ret == 0)
			die("io_uring_submit submitted zero SQEs");
		ring->pending_submit -= (size_t)ret;
	}
}

static void uring_sink_submit_all(struct uring_sink *sink)
{
	for (size_t i = 0; i < sink->ring_count; i++)
		uring_sink_submit_ring(&sink->rings[i]);
}

static size_t uring_sink_pending_total(const struct uring_sink *sink)
{
	size_t total = 0;
	for (size_t i = 0; i < sink->ring_count; i++)
		total += sink->rings[i].pending_submit;
	return total;
}

static size_t uring_sink_alloc_group(struct uring_sink *sink)
{
	if (!sink->free_group_count)
		die("out of write groups");
	size_t id = sink->free_groups[--sink->free_group_count];
	if (sink->groups[id].active)
		die("allocated active write group");
	return id;
}

static void uring_sink_free_group(struct uring_sink *sink, size_t id)
{
	if (id >= sink->group_capacity || !sink->groups[id].active)
		die("invalid write group completion");
	sink->groups[id].active = false;
	sink->free_groups[sink->free_group_count++] = id;
}

static size_t uring_sink_pick_ring(struct uring_sink *sink, const struct options *opt,
				   size_t first_slot)
{
	if (sink->ring_count == 1)
		return 0;
	if (opt->ring_interleave == RING_INTERLEAVE_ROUNDROBIN)
		return sink->next_ring++ % sink->ring_count;
	return first_slot % sink->ring_count;
}

static unsigned uring_sink_peek_batch(struct uring_sink *sink,
				      size_t ring_idx,
				      struct io_uring_cqe **cqes,
				      unsigned max)
{
	return io_uring_peek_batch_cqe(&sink->rings[ring_idx].ring, cqes, max);
}

static void uring_sink_wait_one(struct uring_sink *sink, size_t *group_id, int *res)
{
	struct io_uring_cqe *cqe = NULL;
	for (;;) {
		for (size_t i = 0; i < sink->ring_count; i++) {
			int ret = io_uring_peek_cqe(&sink->rings[i].ring, &cqe);
			if (ret == 0 && cqe) {
				*group_id = (size_t)cqe->user_data;
				*res = cqe->res;
				io_uring_cqe_seen(&sink->rings[i].ring, cqe);
				return;
			}
			if (ret != -EAGAIN && ret < 0) {
				errno = -ret;
				die_errno("io_uring_peek_cqe");
			}
		}
		for (size_t i = 0; i < sink->ring_count; i++) {
			if (sink->rings[i].pending_submit) {
				uring_sink_submit_ring(&sink->rings[i]);
				break;
			}
		}
	}
}

static bool uring_sink_try_queue(struct uring_sink *sink, const struct options *opt,
				 const struct resources *res, const size_t *slots,
				 size_t nr_slots)
{
	size_t group_id = uring_sink_alloc_group(sink);
	struct write_group *group = &sink->groups[group_id];
	size_t ring_idx = uring_sink_pick_ring(sink, opt, slots[0]);
	struct uring_sink_ring *ring = &sink->rings[ring_idx];
	struct io_uring_sqe *sqe = io_uring_get_sqe(&ring->ring);
	if (!sqe) {
		sink->free_groups[sink->free_group_count++] = group_id;
		return false;
	}

	group->nr = nr_slots;
	group->bytes = nr_slots * opt->size;
	group->ring = ring_idx;
	group->active = true;
	for (size_t i = 0; i < nr_slots; i++) {
		group->slots[i] = slots[i];
		group->iovecs[i].iov_base = (char *)res->buf + slots[i] * opt->size;
		group->iovecs[i].iov_len = opt->size;
	}

	if (sink->capacity && sink->offset + group->bytes > sink->capacity)
		sink->offset = 0;
	if (nr_slots == 1) {
		io_uring_prep_write_fixed(sqe, 0, group->iovecs[0].iov_base,
					  (unsigned)opt->size, (off_t)sink->offset, 0);
	} else {
		io_uring_prep_writev_fixed(sqe, 0, group->iovecs, (unsigned)nr_slots,
					   (off_t)sink->offset, 0, 0);
	}
	sqe->flags |= IOSQE_FIXED_FILE;
	sqe->user_data = (uint64_t)group_id;
	sink->offset += group->bytes;
	ring->pending_submit++;
	return true;
}

static void uring_sink_close(struct uring_sink *sink)
{
	if (!sink->enabled)
		return;
	for (size_t i = 0; i < sink->ring_count; i++) {
		io_uring_unregister_buffers(&sink->rings[i].ring);
		io_uring_unregister_files(&sink->rings[i].ring);
		io_uring_queue_exit(&sink->rings[i].ring);
	}
	close(sink->fd);
	free(sink->free_groups);
	free(sink->group_iovec_storage);
	free(sink->group_slot_storage);
	free(sink->groups);
	free(sink->rings);
}

static void write_full_at(int fd, const void *buf, size_t len, uint64_t *offset,
			  uint64_t capacity)
{
	if (fd < 0)
		return;
	if (capacity && *offset + len > capacity)
		*offset = 0;
	size_t done = 0;
	while (done < len) {
		ssize_t ret = pwrite(fd, (const char *)buf + done, len - done,
				     (off_t)(*offset + done));
		if (ret < 0)
			die_errno("pwrite");
		if (ret == 0)
			die("pwrite returned zero");
		done += (size_t)ret;
	}
	*offset += len;
}

static void exchange_qp(int sock, const struct resources *res, struct qp_wire *remote)
{
	struct qp_wire local;
	local_wire(res, &local);
	send_all(sock, &local, sizeof(local));
	recv_all(sock, remote, sizeof(*remote));
}

static void complete_uring_write(const struct options *opt, struct resources *res,
				 struct uring_sink *sink, size_t group_id, int res_code,
				 size_t *write_completed,
				 size_t *write_inflight, size_t *posted)
{
	if (group_id >= sink->group_capacity || !sink->groups[group_id].active)
		die("invalid io_uring write group id");
	struct write_group *group = &sink->groups[group_id];
	if (res_code != (int)group->bytes) {
		fprintf(stderr,
			"short io_uring WAL write: group=%zu res=%d expected=%zu\n",
			group_id, res_code, group->bytes);
		exit(1);
	}
	if (*write_inflight == 0)
		die("io_uring write completion underflow");
	*write_completed += group->nr;
	(*write_inflight)--;
	for (size_t i = 0; i < group->nr && *posted < opt->count; i++) {
		post_recv_slot(res, group->slots[i], opt->size);
		(*posted)++;
	}
	uring_sink_free_group(sink, group_id);
}

struct tree_slot_ref {
	uint16_t lane;
	uint32_t slot;
};

struct tree_group_msg {
	size_t nr;
	struct tree_slot_ref refs[TREE_MAX_REFS];
};

struct tree_spsc_queue {
	struct tree_group_msg *items;
	size_t cap;
	atomic_size_t head;
	char pad0[64];
	atomic_size_t tail;
	char pad1[64];
	atomic_bool done;
};

struct tree_lane_state {
	struct ibv_cq *cq;
	struct ibv_qp *qp;
	uint32_t psn;
	int sock;
	size_t posted;
};

struct tree_rdma {
	struct ibv_context *ctx;
	struct ibv_pd *pd;
	struct ibv_mr *mr;
	struct ibv_port_attr port_attr;
	union ibv_gid gid;
	void *buf;
	size_t buf_len;
	size_t lane_count;
	size_t slots_per_lane;
	struct tree_lane_state *lanes;
};

struct tree_context {
	const struct options *opt;
	struct tree_rdma *rdma;
	struct tree_spsc_queue *leaf_queues;
	struct tree_spsc_queue *middle_queues;
	atomic_bool start;
	double start_time;
};

struct tree_leaf_arg {
	struct tree_context *ctx;
	size_t lane;
	int cpu;
};

struct tree_middle_arg {
	struct tree_context *ctx;
	size_t middle;
	size_t first_leaf;
	size_t leaf_count;
	int cpu;
};

struct tree_root_group {
	bool active;
	size_t nr;
	size_t bytes;
	struct tree_slot_ref *refs;
	struct iovec *iovecs;
};

struct tree_root_writer {
	bool enabled;
	int fd;
	uint64_t capacity;
	uint64_t offset;
	struct io_uring ring;
	size_t pending_submit;
	size_t inflight;
	size_t completed_slots;
	size_t write_count;
	size_t group_capacity;
	struct tree_root_group *groups;
	struct tree_slot_ref *group_ref_storage;
	struct iovec *group_iovec_storage;
	size_t *free_groups;
	size_t free_group_count;
};

static void tree_queue_init(struct tree_spsc_queue *q, size_t cap)
{
	if (cap < 2)
		cap = 2;
	memset(q, 0, sizeof(*q));
	q->items = calloc(cap, sizeof(*q->items));
	if (!q->items)
		die_errno("calloc tree queue");
	q->cap = cap;
	atomic_init(&q->head, 0);
	atomic_init(&q->tail, 0);
	atomic_init(&q->done, false);
}

static void tree_queue_destroy(struct tree_spsc_queue *q)
{
	free(q->items);
}

static bool tree_queue_empty(const struct tree_spsc_queue *q)
{
	size_t head = atomic_load_explicit(&q->head, memory_order_acquire);
	size_t tail = atomic_load_explicit(&q->tail, memory_order_acquire);
	return head == tail;
}

static bool tree_queue_done_empty(const struct tree_spsc_queue *q)
{
	return atomic_load_explicit(&q->done, memory_order_acquire) &&
	       tree_queue_empty(q);
}

static void tree_queue_mark_done(struct tree_spsc_queue *q)
{
	atomic_store_explicit(&q->done, true, memory_order_release);
}

static void tree_queue_push(struct tree_spsc_queue *q,
			    const struct tree_group_msg *msg)
{
	for (;;) {
		size_t head = atomic_load_explicit(&q->head, memory_order_relaxed);
		size_t tail = atomic_load_explicit(&q->tail, memory_order_acquire);
		if (head - tail < q->cap) {
			q->items[head % q->cap] = *msg;
			atomic_store_explicit(&q->head, head + 1, memory_order_release);
			return;
		}
		sched_yield();
	}
}

static bool tree_queue_pop(struct tree_spsc_queue *q, struct tree_group_msg *msg)
{
	size_t tail = atomic_load_explicit(&q->tail, memory_order_relaxed);
	size_t head = atomic_load_explicit(&q->head, memory_order_acquire);
	if (tail == head)
		return false;
	*msg = q->items[tail % q->cap];
	atomic_store_explicit(&q->tail, tail + 1, memory_order_release);
	return true;
}

static void tree_push_pending(struct tree_spsc_queue *q,
			      struct tree_group_msg *pending)
{
	if (!pending->nr)
		return;
	tree_queue_push(q, pending);
	pending->nr = 0;
}

static void tree_append_ref(struct tree_spsc_queue *q,
			    struct tree_group_msg *pending,
			    struct tree_slot_ref ref, size_t flush_at)
{
	if (pending->nr >= TREE_MAX_REFS)
		die("tree group overflow");
	pending->refs[pending->nr++] = ref;
	if (pending->nr >= flush_at)
		tree_push_pending(q, pending);
}

static void tree_local_wire(const struct tree_rdma *tree, size_t lane,
			    struct qp_wire *wire)
{
	memset(wire, 0, sizeof(*wire));
	wire->qpn = htonl(tree->lanes[lane].qp->qp_num);
	wire->psn = htonl(tree->lanes[lane].psn);
	wire->lid = htons(tree->port_attr.lid);
	memcpy(wire->gid, tree->gid.raw, sizeof(wire->gid));
}

static void tree_exchange_qp(int sock, const struct tree_rdma *tree,
			     size_t lane, struct qp_wire *remote)
{
	struct qp_wire local;
	tree_local_wire(tree, lane, &local);
	send_all(sock, &local, sizeof(local));
	recv_all(sock, remote, sizeof(*remote));
}

static void tree_to_rtr_rts(const struct options *opt, struct tree_rdma *tree,
			    size_t lane_idx, const struct qp_wire *remote_wire)
{
	struct tree_lane_state *lane = &tree->lanes[lane_idx];
	uint32_t remote_qpn = ntohl(remote_wire->qpn);
	uint32_t remote_psn = ntohl(remote_wire->psn);
	uint16_t remote_lid = ntohs(remote_wire->lid);
	union ibv_gid remote_gid;
	memcpy(remote_gid.raw, remote_wire->gid, sizeof(remote_gid.raw));

	struct ibv_qp_attr init = {
		.qp_state = IBV_QPS_INIT,
		.pkey_index = 0,
		.port_num = (uint8_t)opt->ib_port,
		.qp_access_flags = 0,
	};
	if (ibv_modify_qp(lane->qp, &init,
			  IBV_QP_STATE | IBV_QP_PKEY_INDEX | IBV_QP_PORT |
				  IBV_QP_ACCESS_FLAGS) != 0)
		die("tree ibv_modify_qp INIT failed");

	struct ibv_qp_attr rtr = {
		.qp_state = IBV_QPS_RTR,
		.path_mtu = IBV_MTU_4096,
		.dest_qp_num = remote_qpn,
		.rq_psn = remote_psn,
		.max_dest_rd_atomic = 1,
		.min_rnr_timer = 12,
		.ah_attr = {
			.is_global = 1,
			.dlid = remote_lid,
			.sl = 0,
			.src_path_bits = 0,
			.port_num = (uint8_t)opt->ib_port,
			.grh = {
				.dgid = remote_gid,
				.sgid_index = (uint8_t)opt->gid_idx,
				.hop_limit = 1,
			},
		},
	};
	if (ibv_modify_qp(lane->qp, &rtr,
			  IBV_QP_STATE | IBV_QP_AV | IBV_QP_PATH_MTU |
				  IBV_QP_DEST_QPN | IBV_QP_RQ_PSN |
				  IBV_QP_MAX_DEST_RD_ATOMIC | IBV_QP_MIN_RNR_TIMER) != 0)
		die("tree ibv_modify_qp RTR failed");

	struct ibv_qp_attr rts = {
		.qp_state = IBV_QPS_RTS,
		.timeout = 14,
		.retry_cnt = 7,
		.rnr_retry = 7,
		.sq_psn = lane->psn,
		.max_rd_atomic = 1,
	};
	if (ibv_modify_qp(lane->qp, &rts,
			  IBV_QP_STATE | IBV_QP_TIMEOUT | IBV_QP_RETRY_CNT |
				  IBV_QP_RNR_RETRY | IBV_QP_SQ_PSN |
				  IBV_QP_MAX_QP_RD_ATOMIC) != 0)
		die("tree ibv_modify_qp RTS failed");
}

static void tree_init_resources(const struct options *opt, struct tree_rdma *tree,
				size_t slots_per_lane)
{
	memset(tree, 0, sizeof(*tree));
	tree->lane_count = opt->lanes;
	tree->slots_per_lane = slots_per_lane;
	tree->lanes = calloc(tree->lane_count, sizeof(*tree->lanes));
	if (!tree->lanes)
		die_errno("calloc tree lanes");

	int ndev = 0;
	struct ibv_device **list = ibv_get_device_list(&ndev);
	if (!list || ndev == 0)
		die("no ibverbs devices found");

	struct ibv_device *dev = NULL;
	for (int i = 0; i < ndev; i++) {
		if (!opt->ib_dev || strcmp(ibv_get_device_name(list[i]), opt->ib_dev) == 0) {
			dev = list[i];
			break;
		}
	}
	if (!dev) {
		fprintf(stderr, "ibverbs device not found: %s\n", opt->ib_dev);
		exit(1);
	}

	tree->ctx = ibv_open_device(dev);
	if (!tree->ctx)
		die("tree ibv_open_device failed");
	ibv_free_device_list(list);

	if (ibv_query_port(tree->ctx, opt->ib_port, &tree->port_attr) != 0)
		die("tree ibv_query_port failed");
	if (ibv_query_gid(tree->ctx, opt->ib_port, opt->gid_idx, &tree->gid) != 0)
		die("tree ibv_query_gid failed");

	tree->pd = ibv_alloc_pd(tree->ctx);
	if (!tree->pd)
		die("tree ibv_alloc_pd failed");

	tree->buf_len = tree->lane_count * slots_per_lane * opt->size;
	if (posix_memalign(&tree->buf, 4096, tree->buf_len) != 0)
		die_errno("tree posix_memalign");
	memset(tree->buf, 0, tree->buf_len);
	tree->mr = ibv_reg_mr(tree->pd, tree->buf, tree->buf_len, IBV_ACCESS_LOCAL_WRITE);
	if (!tree->mr)
		die("tree ibv_reg_mr failed");

	for (size_t i = 0; i < tree->lane_count; i++) {
		struct tree_lane_state *lane = &tree->lanes[i];
		lane->cq = ibv_create_cq(tree->ctx, (int)(slots_per_lane + 32), NULL,
					 NULL, 0);
		if (!lane->cq)
			die("tree ibv_create_cq failed");
		struct ibv_qp_init_attr qpia = {
			.send_cq = lane->cq,
			.recv_cq = lane->cq,
			.cap = {
				.max_send_wr = (uint32_t)(slots_per_lane + 1),
				.max_recv_wr = (uint32_t)(slots_per_lane + 1),
				.max_send_sge = 1,
				.max_recv_sge = 1,
			},
			.qp_type = IBV_QPT_RC,
		};
		lane->qp = ibv_create_qp(tree->pd, &qpia);
		if (!lane->qp)
			die("tree ibv_create_qp failed");
		lane->psn = make_psn();
		lane->sock = -1;
	}
}

static void tree_post_recv_slot(struct tree_rdma *tree, size_t lane_idx,
				size_t slot, size_t size)
{
	size_t global_slot = lane_idx * tree->slots_per_lane + slot;
	struct ibv_sge sge = {
		.addr = (uintptr_t)((char *)tree->buf + global_slot * size),
		.length = (uint32_t)size,
		.lkey = tree->mr->lkey,
	};
	struct ibv_recv_wr wr = {
		.wr_id = slot,
		.sg_list = &sge,
		.num_sge = 1,
	};
	struct ibv_recv_wr *bad = NULL;
	if (ibv_post_recv(tree->lanes[lane_idx].qp, &wr, &bad) != 0)
		die("tree ibv_post_recv failed");
}

static void *tree_leaf_thread(void *raw)
{
	struct tree_leaf_arg *arg = raw;
	struct tree_context *ctx = arg->ctx;
	const struct options *opt = ctx->opt;
	struct tree_rdma *tree = ctx->rdma;
	struct tree_lane_state *lane = &tree->lanes[arg->lane];

	if (arg->cpu >= 0)
		maybe_pin_cpu(arg->cpu);
	while (!atomic_load_explicit(&ctx->start, memory_order_acquire))
		sched_yield();

	size_t wc_batch = opt->wc_batch;
	if (wc_batch > tree->slots_per_lane)
		wc_batch = tree->slots_per_lane;
	struct ibv_wc *wcs = calloc(wc_batch, sizeof(*wcs));
	if (!wcs)
		die_errno("calloc tree leaf wcs");

	size_t received = 0;
	struct tree_group_msg pending = {0};
	while (received < opt->count) {
		int polled = ibv_poll_cq(lane->cq, (int)wc_batch, wcs);
		if (polled < 0)
			die("tree ibv_poll_cq failed");
		if (polled == 0) {
			sched_yield();
			continue;
		}
		for (int i = 0; i < polled; i++) {
			if (wcs[i].status != IBV_WC_SUCCESS) {
				fprintf(stderr,
					"tree leaf completion failed: lane=%zu status=%s opcode=%d wr_id=%llu\n",
					arg->lane, ibv_wc_status_str(wcs[i].status),
					wcs[i].opcode, (unsigned long long)wcs[i].wr_id);
				exit(1);
			}
			struct tree_slot_ref ref = {
				.lane = (uint16_t)arg->lane,
				.slot = (uint32_t)wcs[i].wr_id,
			};
			tree_append_ref(&ctx->leaf_queues[arg->lane], &pending, ref,
					opt->leaf_coalesce);
			received++;
		}
	}
	tree_push_pending(&ctx->leaf_queues[arg->lane], &pending);
	tree_queue_mark_done(&ctx->leaf_queues[arg->lane]);
	free(wcs);
	return NULL;
}

static void *tree_middle_thread(void *raw)
{
	struct tree_middle_arg *arg = raw;
	struct tree_context *ctx = arg->ctx;
	const struct options *opt = ctx->opt;

	if (arg->cpu >= 0)
		maybe_pin_cpu(arg->cpu);
	while (!atomic_load_explicit(&ctx->start, memory_order_acquire))
		sched_yield();

	struct tree_group_msg pending = {0};
	for (;;) {
		bool any = false;
		bool all_done = true;
		for (size_t i = 0; i < arg->leaf_count; i++) {
			size_t leaf = arg->first_leaf + i;
			struct tree_spsc_queue *q = &ctx->leaf_queues[leaf];
			struct tree_group_msg msg;
			while (tree_queue_pop(q, &msg)) {
				any = true;
				for (size_t r = 0; r < msg.nr; r++)
					tree_append_ref(&ctx->middle_queues[arg->middle],
							&pending, msg.refs[r], opt->coalesce);
			}
			if (!tree_queue_done_empty(q))
				all_done = false;
		}
		if (all_done)
			break;
		if (!any)
			sched_yield();
	}
	tree_push_pending(&ctx->middle_queues[arg->middle], &pending);
	tree_queue_mark_done(&ctx->middle_queues[arg->middle]);
	return NULL;
}

static void tree_root_writer_init(struct tree_root_writer *writer,
				  const struct options *opt,
				  const struct tree_rdma *tree,
				  size_t total_slots)
{
	memset(writer, 0, sizeof(*writer));
	writer->fd = -1;
	if (strcmp(opt->sink, "discard") == 0)
		return;
	if (strcmp(opt->sink, "uring-block") != 0)
		die("tree-server supports --sink discard or --sink uring-block");

	writer->enabled = true;
	writer->fd = open_sink(opt, &writer->capacity);
	if (writer->fd < 0)
		die("tree uring sink requires a block path");

	unsigned entries = (unsigned)opt->ring_entries;
	if (entries < total_slots + 32)
		entries = (unsigned)(total_slots + 32);
	if (io_uring_queue_init(entries, &writer->ring, 0) != 0)
		die("tree io_uring_queue_init failed");

	int files[1] = { writer->fd };
	struct iovec reg_iovec = {
		.iov_base = tree->buf,
		.iov_len = tree->buf_len,
	};
	if (io_uring_register_files(&writer->ring, files, 1) != 0)
		die_errno("tree io_uring_register_files");
	if (io_uring_register_buffers(&writer->ring, &reg_iovec, 1) != 0)
		die_errno("tree io_uring_register_buffers");

	writer->group_capacity = total_slots + entries + 64;
	writer->groups = calloc(writer->group_capacity, sizeof(*writer->groups));
	writer->group_ref_storage =
		calloc(writer->group_capacity * opt->coalesce,
		       sizeof(*writer->group_ref_storage));
	writer->group_iovec_storage =
		calloc(writer->group_capacity * opt->coalesce,
		       sizeof(*writer->group_iovec_storage));
	writer->free_groups = calloc(writer->group_capacity, sizeof(*writer->free_groups));
	if (!writer->groups || !writer->group_ref_storage ||
	    !writer->group_iovec_storage || !writer->free_groups)
		die_errno("calloc tree root writer groups");
	for (size_t i = 0; i < writer->group_capacity; i++) {
		writer->groups[i].refs = writer->group_ref_storage + i * opt->coalesce;
		writer->groups[i].iovecs = writer->group_iovec_storage + i * opt->coalesce;
		writer->free_groups[writer->free_group_count++] =
			writer->group_capacity - 1 - i;
	}
}

static void tree_root_writer_close(struct tree_root_writer *writer)
{
	if (!writer->enabled)
		return;
	io_uring_unregister_buffers(&writer->ring);
	io_uring_unregister_files(&writer->ring);
	io_uring_queue_exit(&writer->ring);
	close(writer->fd);
	free(writer->free_groups);
	free(writer->group_iovec_storage);
	free(writer->group_ref_storage);
	free(writer->groups);
}

static size_t tree_root_alloc_group(struct tree_root_writer *writer)
{
	if (!writer->free_group_count)
		die("tree root out of write groups");
	size_t id = writer->free_groups[--writer->free_group_count];
	if (writer->groups[id].active)
		die("tree root allocated active group");
	return id;
}

static void tree_root_free_group(struct tree_root_writer *writer, size_t id)
{
	if (id >= writer->group_capacity || !writer->groups[id].active)
		die("tree root invalid group free");
	writer->groups[id].active = false;
	writer->free_groups[writer->free_group_count++] = id;
}

static void tree_root_submit(struct tree_root_writer *writer)
{
	while (writer->pending_submit) {
		int ret = io_uring_submit(&writer->ring);
		if (ret < 0) {
			errno = -ret;
			die_errno("tree io_uring_submit");
		}
		if (ret == 0)
			die("tree io_uring_submit submitted zero SQEs");
		writer->pending_submit -= (size_t)ret;
	}
}

static void tree_root_complete_group(struct tree_context *ctx,
				     struct tree_root_writer *writer,
				     size_t group_id, int res_code)
{
	const struct options *opt = ctx->opt;
	struct tree_rdma *tree = ctx->rdma;
	if (group_id >= writer->group_capacity || !writer->groups[group_id].active)
		die("tree root invalid completion group");
	struct tree_root_group *group = &writer->groups[group_id];
	if (res_code != (int)group->bytes) {
		fprintf(stderr,
			"tree short io_uring WAL write: group=%zu res=%d expected=%zu\n",
			group_id, res_code, group->bytes);
		exit(1);
	}
	if (!writer->inflight)
		die("tree root completion underflow");
	writer->inflight--;
	writer->completed_slots += group->nr;
	for (size_t i = 0; i < group->nr; i++) {
		struct tree_slot_ref ref = group->refs[i];
		struct tree_lane_state *lane = &tree->lanes[ref.lane];
		if (lane->posted < opt->count) {
			tree_post_recv_slot(tree, ref.lane, ref.slot, opt->size);
			lane->posted++;
		}
	}
	tree_root_free_group(writer, group_id);
}

static void tree_root_complete_discard(struct tree_context *ctx,
				       struct tree_root_writer *writer,
				       const struct tree_group_msg *msg)
{
	const struct options *opt = ctx->opt;
	struct tree_rdma *tree = ctx->rdma;
	writer->completed_slots += msg->nr;
	for (size_t i = 0; i < msg->nr; i++) {
		struct tree_slot_ref ref = msg->refs[i];
		struct tree_lane_state *lane = &tree->lanes[ref.lane];
		if (lane->posted < opt->count) {
			tree_post_recv_slot(tree, ref.lane, ref.slot, opt->size);
			lane->posted++;
		}
	}
}

static unsigned tree_root_drain_cqes(struct tree_context *ctx,
				     struct tree_root_writer *writer,
				     struct io_uring_cqe **cqes, unsigned max)
{
	unsigned ready = io_uring_peek_batch_cqe(&writer->ring, cqes, max);
	for (unsigned i = 0; i < ready; i++) {
		tree_root_complete_group(ctx, writer, (size_t)cqes[i]->user_data,
					 cqes[i]->res);
	}
	if (ready)
		io_uring_cq_advance(&writer->ring, ready);
	return ready;
}

static void tree_root_wait_one(struct tree_context *ctx,
			       struct tree_root_writer *writer)
{
	struct io_uring_cqe *cqe = NULL;
	int ret = io_uring_wait_cqe(&writer->ring, &cqe);
	if (ret < 0) {
		errno = -ret;
		die_errno("tree io_uring_wait_cqe");
	}
	tree_root_complete_group(ctx, writer, (size_t)cqe->user_data, cqe->res);
	io_uring_cqe_seen(&writer->ring, cqe);
}

static bool tree_root_try_queue(struct tree_context *ctx,
				struct tree_root_writer *writer,
				const struct tree_group_msg *msg)
{
	const struct options *opt = ctx->opt;
	struct tree_rdma *tree = ctx->rdma;
	size_t group_id = tree_root_alloc_group(writer);
	struct tree_root_group *group = &writer->groups[group_id];
	struct io_uring_sqe *sqe = io_uring_get_sqe(&writer->ring);
	if (!sqe) {
		writer->free_groups[writer->free_group_count++] = group_id;
		return false;
	}

	group->nr = msg->nr;
	group->bytes = msg->nr * opt->size;
	group->active = true;
	for (size_t i = 0; i < msg->nr; i++) {
		struct tree_slot_ref ref = msg->refs[i];
		size_t global_slot = ref.lane * tree->slots_per_lane + ref.slot;
		group->refs[i] = ref;
		group->iovecs[i].iov_base = (char *)tree->buf + global_slot * opt->size;
		group->iovecs[i].iov_len = opt->size;
	}

	if (writer->capacity && writer->offset + group->bytes > writer->capacity)
		writer->offset = 0;
	if (msg->nr == 1) {
		io_uring_prep_write_fixed(sqe, 0, group->iovecs[0].iov_base,
					  (unsigned)opt->size, (off_t)writer->offset, 0);
	} else {
		io_uring_prep_writev_fixed(sqe, 0, group->iovecs, (unsigned)msg->nr,
					   (off_t)writer->offset, 0, 0);
	}
	sqe->flags |= IOSQE_FIXED_FILE;
	sqe->user_data = (uint64_t)group_id;
	writer->offset += group->bytes;
	writer->pending_submit++;
	writer->write_count++;
	return true;
}

static bool tree_middle_done(struct tree_context *ctx)
{
	for (size_t i = 0; i < ctx->opt->middle_count; i++) {
		if (!tree_queue_done_empty(&ctx->middle_queues[i]))
			return false;
	}
	return true;
}

static void tree_run_root(struct tree_context *ctx, struct tree_root_writer *writer,
			  int cpu)
{
	const struct options *opt = ctx->opt;
	if (cpu >= 0)
		maybe_pin_cpu(cpu);
	while (!atomic_load_explicit(&ctx->start, memory_order_acquire))
		sched_yield();

	size_t total_slots = opt->lanes * opt->count;
	size_t cqe_batch = opt->wc_batch;
	if (cqe_batch < 1)
		cqe_batch = 1;
	struct io_uring_cqe **cqes = calloc(cqe_batch, sizeof(*cqes));
	if (!cqes)
		die_errno("calloc tree root cqes");

	size_t next_middle = 0;
	while (writer->completed_slots < total_slots) {
		if (writer->enabled)
			tree_root_drain_cqes(ctx, writer, cqes, (unsigned)cqe_batch);

		bool any = false;
		for (size_t scan = 0; scan < opt->middle_count; scan++) {
			size_t middle = (next_middle + scan) % opt->middle_count;
			struct tree_group_msg msg;
			while (tree_queue_pop(&ctx->middle_queues[middle], &msg)) {
				any = true;
				if (!writer->enabled) {
					tree_root_complete_discard(ctx, writer, &msg);
					continue;
				}
				while (!tree_root_try_queue(ctx, writer, &msg)) {
					tree_root_submit(writer);
					tree_root_wait_one(ctx, writer);
				}
				writer->inflight++;
				if (writer->pending_submit >= opt->submit_batch)
					tree_root_submit(writer);
			}
		}
		next_middle = (next_middle + 1) % opt->middle_count;

		if (writer->enabled && writer->pending_submit &&
		    (!any || writer->pending_submit >= opt->submit_batch))
			tree_root_submit(writer);

		if (writer->completed_slots >= total_slots)
			break;
		if (!any) {
			bool done = tree_middle_done(ctx);
			if (writer->enabled && writer->inflight) {
				tree_root_wait_one(ctx, writer);
			} else if (done) {
				break;
			} else {
				sched_yield();
			}
		}
	}
	if (writer->enabled) {
		if (writer->pending_submit)
			tree_root_submit(writer);
		while (writer->completed_slots < total_slots && writer->inflight)
			tree_root_wait_one(ctx, writer);
	}

	double elapsed = now_seconds() - ctx->start_time;
	uint64_t bytes = (uint64_t)total_slots * (uint64_t)opt->size;
	printf("ibv-tree-server: dev=%s sink=%s path=%s direct=%s lanes=%zu middle=%zu count_per_lane=%zu size=%zu pipeline=%zu leaf_coalesce=%zu coalesce=%zu queue_depth=%zu ring_entries=%zu submit_batch=%zu wc_batch=%zu writes=%zu bytes=%llu seconds=%.6f MBps=%.2f Gbitps=%.3f\n",
	       opt->ib_dev, opt->sink, writer->enabled ? opt->path : "none",
	       opt->direct ? "yes" : "no", opt->lanes, opt->middle_count,
	       opt->count, opt->size, opt->pipeline, opt->leaf_coalesce,
	       opt->coalesce, opt->queue_depth, opt->ring_entries, opt->submit_batch,
	       opt->wc_batch, writer->write_count, (unsigned long long)bytes, elapsed,
	       (double)bytes / (1000.0 * 1000.0) / elapsed,
	       (double)bytes * 8.0 / 1000000000.0 / elapsed);

	free(cqes);
}

static void run_tree_server(const struct options *opt)
{
	size_t slots_per_lane = opt->pipeline < opt->count ? opt->pipeline : opt->count;
	struct tree_rdma tree;
	tree_init_resources(opt, &tree, slots_per_lane);

	int base_port = atoi(opt->service);
	int *listen_fds = calloc(opt->lanes, sizeof(*listen_fds));
	if (!listen_fds)
		die_errno("calloc tree listen fds");
	for (size_t i = 0; i < opt->lanes; i++) {
		char service[32];
		snprintf(service, sizeof(service), "%d", base_port + (int)i);
		listen_fds[i] = server_listen(service, 1);
	}

	for (size_t i = 0; i < opt->lanes; i++) {
		tree.lanes[i].sock = server_accept_fd(listen_fds[i]);
		close(listen_fds[i]);
		struct qp_wire remote;
		tree_exchange_qp(tree.lanes[i].sock, &tree, i, &remote);
		tree_to_rtr_rts(opt, &tree, i, &remote);
	}
	free(listen_fds);

	for (size_t lane = 0; lane < opt->lanes; lane++) {
		for (size_t slot = 0; slot < slots_per_lane; slot++) {
			tree_post_recv_slot(&tree, lane, slot, opt->size);
			tree.lanes[lane].posted++;
		}
	}

	size_t queue_depth = opt->queue_depth;
	if (!queue_depth)
		queue_depth = slots_per_lane * 4 + 1024;
	if (queue_depth < opt->coalesce * 2)
		queue_depth = opt->coalesce * 2;

	struct tree_spsc_queue *leaf_queues = calloc(opt->lanes, sizeof(*leaf_queues));
	struct tree_spsc_queue *middle_queues =
		calloc(opt->middle_count, sizeof(*middle_queues));
	if (!leaf_queues || !middle_queues)
		die_errno("calloc tree queues");
	for (size_t i = 0; i < opt->lanes; i++)
		tree_queue_init(&leaf_queues[i], queue_depth);
	for (size_t i = 0; i < opt->middle_count; i++)
		tree_queue_init(&middle_queues[i], queue_depth);

	struct tree_context ctx = {
		.opt = opt,
		.rdma = &tree,
		.leaf_queues = leaf_queues,
		.middle_queues = middle_queues,
	};
	atomic_init(&ctx.start, false);

	int root_cpu = opt->cpu >= 0 ? opt->cpu + (int)opt->lanes +
					       (int)opt->middle_count : -1;
	struct tree_root_writer writer;
	if (root_cpu >= 0)
		maybe_pin_cpu(root_cpu);
	tree_root_writer_init(&writer, opt, &tree, opt->lanes * slots_per_lane);
	if (opt->cpu >= 0)
		maybe_pin_cpu(opt->cpu);

	pthread_t *leaf_threads = calloc(opt->lanes, sizeof(*leaf_threads));
	pthread_t *middle_threads = calloc(opt->middle_count, sizeof(*middle_threads));
	struct tree_leaf_arg *leaf_args = calloc(opt->lanes, sizeof(*leaf_args));
	struct tree_middle_arg *middle_args = calloc(opt->middle_count, sizeof(*middle_args));
	if (!leaf_threads || !middle_threads || !leaf_args || !middle_args)
		die_errno("calloc tree thread args");

	for (size_t i = 0; i < opt->lanes; i++) {
		leaf_args[i] = (struct tree_leaf_arg){
			.ctx = &ctx,
			.lane = i,
			.cpu = opt->cpu >= 0 ? opt->cpu + (int)i : -1,
		};
		if (pthread_create(&leaf_threads[i], NULL, tree_leaf_thread,
				   &leaf_args[i]) != 0)
			die_errno("pthread_create leaf");
	}
	for (size_t i = 0; i < opt->middle_count; i++) {
		size_t first = i * opt->lanes / opt->middle_count;
		size_t end = (i + 1) * opt->lanes / opt->middle_count;
		middle_args[i] = (struct tree_middle_arg){
			.ctx = &ctx,
			.middle = i,
			.first_leaf = first,
			.leaf_count = end - first,
			.cpu = opt->cpu >= 0 ? opt->cpu + (int)opt->lanes + (int)i : -1,
		};
		if (pthread_create(&middle_threads[i], NULL, tree_middle_thread,
				   &middle_args[i]) != 0)
			die_errno("pthread_create middle");
	}

	ctx.start_time = now_seconds();
	atomic_store_explicit(&ctx.start, true, memory_order_release);
	char ready = 1;
	for (size_t i = 0; i < opt->lanes; i++)
		send_all(tree.lanes[i].sock, &ready, sizeof(ready));

	tree_run_root(&ctx, &writer, root_cpu);

	for (size_t i = 0; i < opt->lanes; i++)
		pthread_join(leaf_threads[i], NULL);
	for (size_t i = 0; i < opt->middle_count; i++)
		pthread_join(middle_threads[i], NULL);

	tree_root_writer_close(&writer);

	for (size_t i = 0; i < opt->lanes; i++) {
		if (tree.lanes[i].sock >= 0)
			close(tree.lanes[i].sock);
		tree_queue_destroy(&leaf_queues[i]);
	}
	for (size_t i = 0; i < opt->middle_count; i++)
		tree_queue_destroy(&middle_queues[i]);
	free(leaf_threads);
	free(middle_threads);
	free(leaf_args);
	free(middle_args);
	free(leaf_queues);
	free(middle_queues);
}

static void run_server(const struct options *opt)
{
	size_t slots = opt->pipeline < opt->count ? opt->pipeline : opt->count;
	struct resources res = {0};
	init_resources(opt, &res, slots);

	int sock = server_accept(opt->service);
	struct qp_wire remote;
	exchange_qp(sock, &res, &remote);
	to_rtr_rts(opt, &res, &remote);

	for (size_t i = 0; i < slots; i++)
		post_recv_slot(&res, i, opt->size);

	uint64_t capacity = 0, offset = 0;
	struct uring_sink uring = {0};
	uring_sink_init(&uring, opt, &res, slots);
	int sink_fd = uring.enabled ? -1 : open_sink(opt, &capacity);

	char ready = 1;
	send_all(sock, &ready, sizeof(ready));

	size_t posted = slots;
	size_t recv_completed = 0;
	size_t write_completed = 0;
	size_t write_inflight = 0;
	size_t pending_nr = 0;
	size_t *pending_slots = calloc(opt->coalesce, sizeof(*pending_slots));
	if (!pending_slots)
		die_errno("calloc pending coalesce slots");
	size_t wc_batch = opt->wc_batch;
	if (wc_batch > slots)
		wc_batch = slots;
	struct ibv_wc *wcs = calloc(wc_batch, sizeof(*wcs));
	struct io_uring_cqe **cqes = calloc(wc_batch, sizeof(*cqes));
	if (!wcs || !cqes)
		die_errno("calloc completion batches");
	double start = now_seconds();
	while (recv_completed < opt->count ||
	       (uring.enabled && pending_nr > 0) ||
	       (uring.enabled && write_completed < opt->count)) {
		if (uring.enabled) {
			for (size_t r = 0; r < uring.ring_count; r++) {
				unsigned cq_ready = uring_sink_peek_batch(&uring, r, cqes,
									  (unsigned)wc_batch);
				for (unsigned i = 0; i < cq_ready; i++) {
					complete_uring_write(opt, &res, &uring,
							     (size_t)cqes[i]->user_data,
							     cqes[i]->res, &write_completed,
							     &write_inflight, &posted);
				}
				if (cq_ready)
					io_uring_cq_advance(&uring.rings[r].ring, cq_ready);
			}

			if (pending_nr &&
			    (pending_nr >= opt->coalesce || recv_completed == opt->count ||
			     posted == recv_completed)) {
				while (!uring_sink_try_queue(&uring, opt, &res,
							     pending_slots, pending_nr)) {
					uring_sink_submit_all(&uring);
					size_t group_id = 0;
					int res_code = 0;
					uring_sink_wait_one(&uring, &group_id, &res_code);
					complete_uring_write(opt, &res, &uring, group_id, res_code,
							     &write_completed, &write_inflight,
							     &posted);
				}
				write_inflight++;
				pending_nr = 0;
			}

			if (uring_sink_pending_total(&uring) &&
			    (uring_sink_pending_total(&uring) >= opt->submit_batch ||
			     posted == recv_completed || recv_completed == opt->count))
				uring_sink_submit_all(&uring);

			if (posted == recv_completed && pending_nr == 0 && write_inflight) {
				size_t group_id = 0;
				int res_code = 0;
				uring_sink_wait_one(&uring, &group_id, &res_code);
				complete_uring_write(opt, &res, &uring, group_id, res_code,
						     &write_completed, &write_inflight, &posted);
				continue;
			}
		}

		if (recv_completed < opt->count && posted > recv_completed) {
			int polled = ibv_poll_cq(res.cq, (int)wc_batch, wcs);
			if (polled < 0)
				die("ibv_poll_cq failed");
			if (polled == 0) {
				if (uring.enabled && uring_sink_pending_total(&uring))
					uring_sink_submit_all(&uring);
				continue;
			}
			for (int i = 0; i < polled; i++) {
				if (wcs[i].status != IBV_WC_SUCCESS) {
					fprintf(stderr,
						"work completion failed: status=%s opcode=%d wr_id=%llu\n",
						ibv_wc_status_str(wcs[i].status),
						wcs[i].opcode,
						(unsigned long long)wcs[i].wr_id);
					exit(1);
				}
				size_t idx = (size_t)wcs[i].wr_id;
				recv_completed++;
				if (uring.enabled) {
					pending_slots[pending_nr++] = idx;
					if (pending_nr >= opt->coalesce ||
					    recv_completed == opt->count ||
					    posted == recv_completed) {
						while (!uring_sink_try_queue(&uring, opt, &res,
									     pending_slots,
									     pending_nr)) {
							uring_sink_submit_all(&uring);
							size_t group_id = 0;
							int res_code = 0;
							uring_sink_wait_one(&uring, &group_id, &res_code);
							complete_uring_write(opt, &res, &uring,
									     group_id, res_code,
									     &write_completed,
									     &write_inflight,
									     &posted);
						}
						write_inflight++;
						pending_nr = 0;
						if (uring_sink_pending_total(&uring) >= opt->submit_batch)
							uring_sink_submit_all(&uring);
					}
				} else {
					write_full_at(sink_fd,
						      (char *)res.buf + idx * opt->size,
						      opt->size, &offset, capacity);
					write_completed++;
					if (posted < opt->count) {
						post_recv_slot(&res, idx, opt->size);
						posted++;
					}
				}
			}
		}
	}
	double elapsed = now_seconds() - start;
	uint64_t bytes = (uint64_t)opt->count * (uint64_t)opt->size;
	printf("ibv-nullblk-server: dev=%s sink=%s path=%s direct=%s count=%zu size=%zu pipeline=%zu ring_entries=%zu submit_batch=%zu wc_batch=%zu uring_rings=%zu ring_interleave=%s coalesce=%zu bytes=%llu seconds=%.6f MBps=%.2f Gbitps=%.3f\n",
	       opt->ib_dev, opt->sink, (sink_fd >= 0 || uring.enabled) ? opt->path : "none",
	       opt->direct ? "yes" : "no", opt->count, opt->size, opt->pipeline,
	       opt->ring_entries, opt->submit_batch, opt->wc_batch, opt->uring_rings,
	       ring_interleave_name(opt->ring_interleave), opt->coalesce,
	       (unsigned long long)bytes, elapsed,
	       (double)bytes / (1000.0 * 1000.0) / elapsed,
	       (double)bytes * 8.0 / 1000000000.0 / elapsed);

	if (sink_fd >= 0)
		close(sink_fd);
	uring_sink_close(&uring);
	close(sock);
	free(cqes);
	free(wcs);
	free(pending_slots);
}

static void run_client(const struct options *opt)
{
	size_t slots = opt->pipeline < opt->count ? opt->pipeline : opt->count;
	struct resources res = {0};
	init_resources(opt, &res, slots);

	int sock = client_connect(opt->host, opt->service);
	struct qp_wire remote;
	exchange_qp(sock, &res, &remote);
	to_rtr_rts(opt, &res, &remote);

	char ready = 0;
	recv_all(sock, &ready, sizeof(ready));

	size_t posted = 0;
	size_t completed = 0;
	size_t signal_every = opt->signal_every;
	if (signal_every > slots)
		signal_every = slots;
	double start = now_seconds();
	while (completed < opt->count) {
		while (posted < opt->count && posted - completed < slots) {
			size_t next = posted + 1;
			size_t idx = posted % slots;
			bool signaled = signal_every == 1 ||
					next == opt->count ||
					(next % signal_every) == 0 ||
					next - completed == slots;
			post_send_slot(&res, idx, opt->size, next, signaled);
			posted++;
		}
		if (completed < posted) {
			struct ibv_wc wc;
			poll_one(&res, &wc);
			if ((size_t)wc.wr_id <= completed || (size_t)wc.wr_id > posted) {
				fprintf(stderr,
					"unexpected send completion wr_id=%llu completed=%zu posted=%zu\n",
					(unsigned long long)wc.wr_id, completed, posted);
				exit(1);
			}
			completed = (size_t)wc.wr_id;
		}
	}
	double elapsed = now_seconds() - start;
	uint64_t bytes = (uint64_t)opt->count * (uint64_t)opt->size;
	printf("ibv-nullblk-client: dev=%s count=%zu size=%zu pipeline=%zu signal_every=%zu bytes=%llu seconds=%.6f MBps=%.2f Gbitps=%.3f\n",
	       opt->ib_dev, opt->count, opt->size, opt->pipeline, signal_every,
	       (unsigned long long)bytes, elapsed,
	       (double)bytes / (1000.0 * 1000.0) / elapsed,
	       (double)bytes * 8.0 / 1000000000.0 / elapsed);

	close(sock);
}

int main(int argc, char **argv)
{
	struct options opt = parse_args(argc, argv);
	maybe_pin_cpu(opt.cpu);
	if (opt.tree_server)
		run_tree_server(&opt);
	else if (opt.server)
		run_server(&opt);
	else
		run_client(&opt);
	return 0;
}
