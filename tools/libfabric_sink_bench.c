#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <linux/fs.h>
#include <rdma/fabric.h>
#include <rdma/fi_cm.h>
#include <rdma/fi_domain.h>
#include <rdma/fi_endpoint.h>
#include <rdma/fi_errno.h>
#include <rdma/fi_eq.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sched.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

struct options {
	bool server;
	const char *host;
	const char *service;
	const char *provider;
	const char *sink;
	const char *path;
	size_t size;
	size_t count;
	size_t pipeline;
	bool direct;
};

struct op_ctx {
	struct fi_context ctx;
	size_t index;
};

struct fabric_ep {
	struct fi_info *info;
	struct fid_fabric *fabric;
	struct fid_domain *domain;
	struct fid_eq *eq;
	struct fid_cq *cq;
	struct fid_ep *ep;
	struct fid_pep *pep;
};

static void die_errno(const char *what)
{
	fprintf(stderr, "%s: %s\n", what, strerror(errno));
	exit(1);
}

static void die_fi(const char *what, ssize_t ret)
{
	fprintf(stderr, "%s: %s (%zd)\n", what, fi_strerror((int)-ret), ret);
	exit(1);
}

static void check_fi(const char *what, ssize_t ret)
{
	if (ret < 0)
		die_fi(what, ret);
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

static void usage(const char *argv0)
{
	fprintf(stderr,
		"usage:\n"
		"  %s server [--service port] [--provider tcp] [--sink discard|block] [--path /dev/nullb0]\n"
		"     [--size bytes] [--count n] [--pipeline n] [--direct]\n"
		"  %s client --host addr [same size/count/pipeline/provider/service]\n",
		argv0, argv0);
	exit(2);
}

static struct options parse_args(int argc, char **argv)
{
	if (argc < 2)
		usage(argv[0]);

	struct options opt = {
		.server = strcmp(argv[1], "server") == 0,
		.host = "127.0.0.1",
		.service = "47592",
		.provider = "tcp",
		.sink = "discard",
		.path = "/dev/nullb0",
		.size = 1024 * 1024,
		.count = 256,
		.pipeline = 64,
		.direct = false,
	};
	if (!opt.server && strcmp(argv[1], "client") != 0)
		usage(argv[0]);

	for (int i = 2; i < argc; i++) {
		if (strcmp(argv[i], "--host") == 0 && i + 1 < argc)
			opt.host = argv[++i];
		else if (strcmp(argv[i], "--service") == 0 && i + 1 < argc)
			opt.service = argv[++i];
		else if (strcmp(argv[i], "--provider") == 0 && i + 1 < argc)
			opt.provider = argv[++i];
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
		else if (strcmp(argv[i], "--direct") == 0)
			opt.direct = true;
		else
			usage(argv[0]);
	}
	if (opt.size == 0 || opt.count == 0 || opt.pipeline == 0)
		usage(argv[0]);
	return opt;
}

static struct fi_info *make_hints(const struct options *opt)
{
	struct fi_info *hints = fi_allocinfo();
	if (!hints) {
		fprintf(stderr, "fi_allocinfo failed\n");
		exit(1);
	}
	hints->caps = FI_MSG;
	hints->mode = FI_CONTEXT;
	hints->ep_attr->type = FI_EP_MSG;
	hints->fabric_attr->prov_name = strdup(opt->provider);
	return hints;
}

static void open_common(struct fabric_ep *fep, struct fi_info *info)
{
	struct fi_eq_attr eq_attr = {
		.wait_obj = FI_WAIT_UNSPEC,
	};
	struct fi_cq_attr cq_attr = {
		.format = FI_CQ_FORMAT_CONTEXT,
		.wait_obj = FI_WAIT_UNSPEC,
	};

	check_fi("fi_domain", fi_domain(fep->fabric, info, &fep->domain, NULL));
	check_fi("fi_eq_open", fi_eq_open(fep->fabric, &eq_attr, &fep->eq, NULL));
	check_fi("fi_cq_open", fi_cq_open(fep->domain, &cq_attr, &fep->cq, NULL));
	check_fi("fi_endpoint", fi_endpoint(fep->domain, info, &fep->ep, NULL));
	check_fi("fi_ep_bind cq", fi_ep_bind(fep->ep, &fep->cq->fid, FI_SEND | FI_RECV));
	check_fi("fi_ep_bind eq", fi_ep_bind(fep->ep, &fep->eq->fid, 0));
	check_fi("fi_enable", fi_enable(fep->ep));
}

static void wait_connected(struct fid_eq *eq, const char *label)
{
	struct fi_eq_cm_entry entry;
	uint32_t event = 0;
	ssize_t ret = fi_eq_sread(eq, &event, &entry, sizeof(entry), -1, 0);
	check_fi(label, ret);
	if (event != FI_CONNECTED) {
		fprintf(stderr, "%s: expected FI_CONNECTED, got event=%u\n", label, event);
		exit(1);
	}
}

static void setup_client(const struct options *opt, struct fabric_ep *fep)
{
	struct fi_info *hints = make_hints(opt);
	check_fi("fi_getinfo client",
		 fi_getinfo(FI_VERSION(1, 11), opt->host, opt->service, 0, hints, &fep->info));
	fi_freeinfo(hints);
	check_fi("fi_fabric", fi_fabric(fep->info->fabric_attr, &fep->fabric, NULL));
	open_common(fep, fep->info);
	check_fi("fi_connect", fi_connect(fep->ep, fep->info->dest_addr, NULL, 0));
	wait_connected(fep->eq, "client wait connected");
}

static void setup_server(const struct options *opt, struct fabric_ep *fep)
{
	struct fi_info *hints = make_hints(opt);
	struct fi_info *listen_info = NULL;
	check_fi("fi_getinfo server",
		 fi_getinfo(FI_VERSION(1, 11), NULL, opt->service, FI_SOURCE, hints, &listen_info));
	fi_freeinfo(hints);

	check_fi("fi_fabric", fi_fabric(listen_info->fabric_attr, &fep->fabric, NULL));
	struct fi_eq_attr eq_attr = {
		.wait_obj = FI_WAIT_UNSPEC,
	};
	check_fi("fi_eq_open listen", fi_eq_open(fep->fabric, &eq_attr, &fep->eq, NULL));
	check_fi("fi_passive_ep", fi_passive_ep(fep->fabric, listen_info, &fep->pep, NULL));
	check_fi("fi_pep_bind", fi_pep_bind(fep->pep, &fep->eq->fid, 0));
	check_fi("fi_listen", fi_listen(fep->pep));

	struct fi_eq_cm_entry entry;
	uint32_t event = 0;
	ssize_t ret = fi_eq_sread(fep->eq, &event, &entry, sizeof(entry), -1, 0);
	check_fi("server wait connreq", ret);
	if (event != FI_CONNREQ) {
		fprintf(stderr, "server: expected FI_CONNREQ, got event=%u\n", event);
		exit(1);
	}

	fep->info = entry.info;
	check_fi("fi_domain", fi_domain(fep->fabric, fep->info, &fep->domain, NULL));
	struct fi_cq_attr cq_attr = {
		.format = FI_CQ_FORMAT_CONTEXT,
		.wait_obj = FI_WAIT_UNSPEC,
	};
	check_fi("fi_cq_open", fi_cq_open(fep->domain, &cq_attr, &fep->cq, NULL));
	check_fi("fi_endpoint", fi_endpoint(fep->domain, fep->info, &fep->ep, NULL));
	check_fi("fi_ep_bind cq", fi_ep_bind(fep->ep, &fep->cq->fid, FI_SEND | FI_RECV));
	check_fi("fi_ep_bind eq", fi_ep_bind(fep->ep, &fep->eq->fid, 0));
	check_fi("fi_enable", fi_enable(fep->ep));
	check_fi("fi_accept", fi_accept(fep->ep, NULL, 0));
	wait_connected(fep->eq, "server wait connected");
	fi_freeinfo(listen_info);
}

static void close_fabric(struct fabric_ep *fep)
{
	if (fep->ep)
		fi_close(&fep->ep->fid);
	if (fep->pep)
		fi_close(&fep->pep->fid);
	if (fep->cq)
		fi_close(&fep->cq->fid);
	if (fep->eq)
		fi_close(&fep->eq->fid);
	if (fep->domain)
		fi_close(&fep->domain->fid);
	if (fep->fabric)
		fi_close(&fep->fabric->fid);
	if (fep->info)
		fi_freeinfo(fep->info);
}

static void *aligned_alloc_or_die(size_t alignment, size_t len)
{
	void *ptr = NULL;
	if (posix_memalign(&ptr, alignment, len) != 0)
		die_errno("posix_memalign");
	memset(ptr, 0xa5, len);
	return ptr;
}

static ssize_t read_cq(struct fid_cq *cq, struct fi_cq_entry *comp)
{
	for (;;) {
		ssize_t ret = fi_cq_sread(cq, comp, 1, NULL, -1);
		if (ret == -FI_EAVAIL) {
			struct fi_cq_err_entry err;
			memset(&err, 0, sizeof(err));
			fi_cq_readerr(cq, &err, 0);
			fprintf(stderr, "cq error: prov_errno=%d err=%d %s\n",
				err.prov_errno, err.err, fi_strerror(err.err));
			exit(1);
		}
		if (ret >= 0)
			return ret;
		if (ret != -FI_EAGAIN)
			die_fi("fi_cq_sread", ret);
	}
}

static void post_recv(struct fabric_ep *fep, char *buf, size_t size, struct op_ctx *ctx)
{
	for (;;) {
		ssize_t ret = fi_recv(fep->ep, buf, size, NULL, 0, &ctx->ctx);
		if (ret == 0)
			return;
		if (ret != -FI_EAGAIN)
			die_fi("fi_recv", ret);
		sched_yield();
	}
}

static int open_sink(const struct options *opt, uint64_t *capacity)
{
	*capacity = 0;
	if (strcmp(opt->sink, "discard") == 0)
		return -1;
	if (strcmp(opt->sink, "block") != 0) {
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

static void write_full_at(int fd, const void *buf, size_t len, uint64_t *offset, uint64_t capacity)
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
		if (ret == 0) {
			fprintf(stderr, "pwrite returned zero\n");
			exit(1);
		}
		done += (size_t)ret;
	}
	*offset += len;
}

static const char *actual_provider(const struct fabric_ep *fep)
{
	if (!fep->info || !fep->info->fabric_attr || !fep->info->fabric_attr->prov_name)
		return "unknown";
	return fep->info->fabric_attr->prov_name;
}

static void run_server(const struct options *opt)
{
	struct fabric_ep fep = {0};
	setup_server(opt, &fep);

	size_t slots = opt->pipeline < opt->count ? opt->pipeline : opt->count;
	char *buf = aligned_alloc_or_die(4096, slots * opt->size);
	struct op_ctx *ctx = calloc(slots, sizeof(*ctx));
	if (!ctx)
		die_errno("calloc ctx");
	for (size_t i = 0; i < slots; i++)
		ctx[i].index = i;

	uint64_t capacity = 0, offset = 0;
	int sink_fd = open_sink(opt, &capacity);

	size_t posted = 0, completed = 0;
	for (; posted < slots; posted++)
		post_recv(&fep, buf + posted * opt->size, opt->size, &ctx[posted]);

	double start = now_seconds();
	while (completed < opt->count) {
		struct fi_cq_entry comp;
		read_cq(fep.cq, &comp);
		struct op_ctx *done = (struct op_ctx *)comp.op_context;
		write_full_at(sink_fd, buf + done->index * opt->size, opt->size, &offset, capacity);
		completed++;
		if (posted < opt->count) {
			post_recv(&fep, buf + done->index * opt->size, opt->size, done);
			posted++;
		}
	}
	double elapsed = now_seconds() - start;
	uint64_t bytes = (uint64_t)opt->count * (uint64_t)opt->size;
	printf("libfabric-sink-server: requested_provider=%s actual_provider=%s sink=%s path=%s direct=%s count=%zu size=%zu pipeline=%zu bytes=%llu seconds=%.6f MBps=%.2f Gbitps=%.3f\n",
	       opt->provider, actual_provider(&fep), opt->sink, sink_fd >= 0 ? opt->path : "none",
	       opt->direct ? "yes" : "no", opt->count, opt->size, opt->pipeline,
	       (unsigned long long)bytes, elapsed,
	       (double)bytes / (1000.0 * 1000.0) / elapsed,
	       (double)bytes * 8.0 / 1000000000.0 / elapsed);

	if (sink_fd >= 0)
		close(sink_fd);
	free(ctx);
	free(buf);
	close_fabric(&fep);
}

static void run_client(const struct options *opt)
{
	struct fabric_ep fep = {0};
	setup_client(opt, &fep);

	size_t slots = opt->pipeline < opt->count ? opt->pipeline : opt->count;
	char *buf = aligned_alloc_or_die(4096, slots * opt->size);
	struct op_ctx *ctx = calloc(slots, sizeof(*ctx));
	size_t *free_slots = calloc(slots, sizeof(*free_slots));
	if (!ctx || !free_slots)
		die_errno("calloc client");
	for (size_t i = 0; i < slots; i++) {
		ctx[i].index = i;
		free_slots[i] = slots - 1 - i;
	}
	size_t free_count = slots, posted = 0, completed = 0, inflight = 0;

	double start = now_seconds();
	while (completed < opt->count) {
		while (posted < opt->count && free_count > 0) {
			size_t slot = free_slots[--free_count];
			for (;;) {
				ssize_t ret = fi_send(fep.ep, buf + slot * opt->size, opt->size,
						      NULL, 0, &ctx[slot].ctx);
				if (ret == 0)
					break;
				if (ret != -FI_EAGAIN)
					die_fi("fi_send", ret);
				struct fi_cq_entry comp;
				read_cq(fep.cq, &comp);
				struct op_ctx *done = (struct op_ctx *)comp.op_context;
				free_slots[free_count++] = done->index;
				completed++;
				inflight--;
			}
			posted++;
			inflight++;
		}
		if (inflight > 0) {
			struct fi_cq_entry comp;
			read_cq(fep.cq, &comp);
			struct op_ctx *done = (struct op_ctx *)comp.op_context;
			free_slots[free_count++] = done->index;
			completed++;
			inflight--;
		}
	}
	double elapsed = now_seconds() - start;
	uint64_t bytes = (uint64_t)opt->count * (uint64_t)opt->size;
	printf("libfabric-sink-client: requested_provider=%s actual_provider=%s count=%zu size=%zu pipeline=%zu bytes=%llu seconds=%.6f MBps=%.2f Gbitps=%.3f\n",
	       opt->provider, actual_provider(&fep), opt->count, opt->size, opt->pipeline,
	       (unsigned long long)bytes, elapsed,
	       (double)bytes / (1000.0 * 1000.0) / elapsed,
	       (double)bytes * 8.0 / 1000000000.0 / elapsed);

	free(free_slots);
	free(ctx);
	free(buf);
	close_fabric(&fep);
}

int main(int argc, char **argv)
{
	struct options opt = parse_args(argc, argv);
	if (opt.server)
		run_server(&opt);
	else
		run_client(&opt);
	return 0;
}
