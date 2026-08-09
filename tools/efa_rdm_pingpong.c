#define _GNU_SOURCE

#include <arpa/inet.h>
#include <errno.h>
#include <netdb.h>
#include <rdma/fabric.h>
#include <rdma/fi_cm.h>
#include <rdma/fi_domain.h>
#include <rdma/fi_endpoint.h>
#include <rdma/fi_errno.h>
#include <rdma/fi_rma.h>
#include <sched.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

struct options {
	bool server;
	const char *host;
	const char *port;
	const char *provider;
	const char *domain;
	bool write_mode;
	size_t size;
	size_t iterations;
	size_t warmup;
	int cpu;
};

struct endpoint {
	struct fi_info *info;
	struct fid_fabric *fabric;
	struct fid_domain *domain;
	struct fid_av *av;
	struct fid_cq *cq;
	struct fid_ep *ep;
	struct fid_mr *mr;
	void *storage;
	char *tx;
	char *rx;
	fi_addr_t peer;
	uint64_t peer_rma_addr;
	uint64_t peer_rma_key;
	struct fi_context tx_ctx;
	struct fi_context rx_ctx;
};

static void die(const char *what)
{
	fprintf(stderr, "%s: %s\n", what, strerror(errno));
	exit(1);
}

static void die_fi(const char *what, ssize_t rc)
{
	fprintf(stderr, "%s: %s (%zd)\n", what, fi_strerror((int)-rc), rc);
	exit(1);
}

static void check_fi(const char *what, ssize_t rc)
{
	if (rc < 0)
		die_fi(what, rc);
}

static void usage(const char *argv0)
{
	fprintf(stderr,
		"usage:\n"
		"  %s server [--port 47593] [--provider efa] [--domain efa_0-rdm] [--mode send|write] [--size 64] [--iterations 100000] [--warmup 10000] [--cpu 0]\n"
		"  %s client --host PRIVATE_IP [same options]\n",
		argv0, argv0);
	exit(2);
}

static size_t parse_count(const char *raw, const char *name)
{
	char *end = NULL;
	errno = 0;
	unsigned long long value = strtoull(raw, &end, 10);
	if (errno || end == raw || *end || value == 0) {
		fprintf(stderr, "invalid %s: %s\n", name, raw);
		exit(2);
	}
	return (size_t)value;
}

static struct options parse_args(int argc, char **argv)
{
	if (argc < 2)
		usage(argv[0]);
	struct options opt = {
		.server = strcmp(argv[1], "server") == 0,
		.host = NULL,
		.port = "47593",
		.provider = "efa",
		.domain = NULL,
		.write_mode = false,
		.size = 64,
		.iterations = 100000,
		.warmup = 10000,
		.cpu = -1,
	};
	if (!opt.server && strcmp(argv[1], "client") != 0)
		usage(argv[0]);
	for (int i = 2; i < argc; i++) {
		if (!strcmp(argv[i], "--host") && i + 1 < argc)
			opt.host = argv[++i];
		else if (!strcmp(argv[i], "--port") && i + 1 < argc)
			opt.port = argv[++i];
		else if (!strcmp(argv[i], "--provider") && i + 1 < argc)
			opt.provider = argv[++i];
		else if (!strcmp(argv[i], "--domain") && i + 1 < argc)
			opt.domain = argv[++i];
		else if (!strcmp(argv[i], "--mode") && i + 1 < argc) {
			const char *mode = argv[++i];
			if (!strcmp(mode, "send")) opt.write_mode = false;
			else if (!strcmp(mode, "write")) opt.write_mode = true;
			else usage(argv[0]);
		}
		else if (!strcmp(argv[i], "--size") && i + 1 < argc)
			opt.size = parse_count(argv[++i], "size");
		else if (!strcmp(argv[i], "--iterations") && i + 1 < argc)
			opt.iterations = parse_count(argv[++i], "iterations");
		else if (!strcmp(argv[i], "--warmup") && i + 1 < argc)
			opt.warmup = parse_count(argv[++i], "warmup");
		else if (!strcmp(argv[i], "--cpu") && i + 1 < argc)
			opt.cpu = atoi(argv[++i]);
		else
			usage(argv[0]);
	}
	if (!opt.server && !opt.host)
		usage(argv[0]);
	return opt;
}

static void pin_cpu(int cpu)
{
	if (cpu < 0)
		return;
	cpu_set_t set;
	CPU_ZERO(&set);
	CPU_SET(cpu, &set);
	if (sched_setaffinity(0, sizeof(set), &set) != 0)
		die("sched_setaffinity");
}

static int tcp_server(const char *port)
{
	struct addrinfo hints = {.ai_family = AF_INET, .ai_socktype = SOCK_STREAM,
		.ai_flags = AI_PASSIVE};
	struct addrinfo *ai = NULL;
	int rc = getaddrinfo(NULL, port, &hints, &ai);
	if (rc) {
		fprintf(stderr, "getaddrinfo: %s\n", gai_strerror(rc));
		exit(1);
	}
	int listener = socket(ai->ai_family, ai->ai_socktype, ai->ai_protocol);
	if (listener < 0)
		die("socket");
	int one = 1;
	setsockopt(listener, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));
	if (bind(listener, ai->ai_addr, ai->ai_addrlen) || listen(listener, 1))
		die("bind/listen");
	freeaddrinfo(ai);
	int fd = accept(listener, NULL, NULL);
	if (fd < 0)
		die("accept");
	close(listener);
	return fd;
}

static int tcp_client(const char *host, const char *port)
{
	struct addrinfo hints = {.ai_family = AF_INET, .ai_socktype = SOCK_STREAM};
	struct addrinfo *ai = NULL;
	int rc = getaddrinfo(host, port, &hints, &ai);
	if (rc) {
		fprintf(stderr, "getaddrinfo: %s\n", gai_strerror(rc));
		exit(1);
	}
	int fd = -1;
	for (int attempt = 0; attempt < 100; attempt++) {
		fd = socket(ai->ai_family, ai->ai_socktype, ai->ai_protocol);
		if (fd >= 0 && connect(fd, ai->ai_addr, ai->ai_addrlen) == 0)
			break;
		if (fd >= 0)
			close(fd);
		fd = -1;
		usleep(100000);
	}
	freeaddrinfo(ai);
	if (fd < 0)
		die("connect");
	return fd;
}

static void io_full(int fd, void *buf, size_t len, bool write_side)
{
	size_t done = 0;
	while (done < len) {
		ssize_t rc = write_side ? write(fd, (char *)buf + done, len - done)
					: read(fd, (char *)buf + done, len - done);
		if (rc < 0 && errno == EINTR)
			continue;
		if (rc <= 0)
			die(write_side ? "control write" : "control read");
		done += (size_t)rc;
	}
}

static void exchange_blob(int fd, const void *mine, size_t mine_len, void **peer,
			  size_t *peer_len, bool server)
{
	uint32_t my_len = htonl((uint32_t)mine_len), their_len;
	if (server) {
		io_full(fd, &their_len, sizeof(their_len), false);
		io_full(fd, &my_len, sizeof(my_len), true);
	} else {
		io_full(fd, &my_len, sizeof(my_len), true);
		io_full(fd, &their_len, sizeof(their_len), false);
	}
	*peer_len = ntohl(their_len);
	*peer = malloc(*peer_len);
	if (!*peer)
		die("malloc peer address");
	if (server) {
		io_full(fd, *peer, *peer_len, false);
		io_full(fd, (void *)mine, mine_len, true);
	} else {
		io_full(fd, (void *)mine, mine_len, true);
		io_full(fd, *peer, *peer_len, false);
	}
}

static void open_endpoint(const struct options *opt, struct endpoint *ep)
{
	struct fi_info *hints = fi_allocinfo();
	if (!hints)
		die("fi_allocinfo");
	hints->caps = FI_MSG | (opt->write_mode ? FI_RMA | FI_WRITE | FI_REMOTE_WRITE : 0);
	hints->mode = FI_CONTEXT;
	hints->ep_attr->type = FI_EP_RDM;
	hints->fabric_attr->prov_name = strdup(opt->provider);
	if (opt->domain)
		hints->domain_attr->name = strdup(opt->domain);
	check_fi("fi_getinfo", fi_getinfo(FI_VERSION(1, 11), NULL, NULL, 0, hints,
					   &ep->info));
	fi_freeinfo(hints);
	check_fi("fi_fabric", fi_fabric(ep->info->fabric_attr, &ep->fabric, NULL));
	check_fi("fi_domain", fi_domain(ep->fabric, ep->info, &ep->domain, NULL));
	struct fi_cq_attr cq_attr = {.format = FI_CQ_FORMAT_DATA, .size = 16};
	check_fi("fi_cq_open", fi_cq_open(ep->domain, &cq_attr, &ep->cq, NULL));
	struct fi_av_attr av_attr = {.type = FI_AV_UNSPEC, .count = 2};
	check_fi("fi_av_open", fi_av_open(ep->domain, &av_attr, &ep->av, NULL));
	check_fi("fi_endpoint", fi_endpoint(ep->domain, ep->info, &ep->ep, NULL));
	check_fi("fi_ep_bind cq", fi_ep_bind(ep->ep, &ep->cq->fid, FI_SEND | FI_RECV));
	check_fi("fi_ep_bind av", fi_ep_bind(ep->ep, &ep->av->fid, 0));
	check_fi("fi_enable", fi_enable(ep->ep));
	if (posix_memalign(&ep->storage, 4096, opt->size * 2))
		die("posix_memalign");
	memset(ep->storage, 0x5a, opt->size * 2);
	ep->tx = ep->storage;
	ep->rx = (char *)ep->storage + opt->size;
	uint64_t access = FI_SEND | FI_RECV;
	if (opt->write_mode)
		access |= FI_WRITE | FI_REMOTE_WRITE;
	check_fi("fi_mr_reg", fi_mr_reg(ep->domain, ep->storage, opt->size * 2,
					 access, 0, 0, 0, &ep->mr, NULL));
}

static void connect_fabric(struct endpoint *ep, int control_fd, bool server)
{
	size_t my_len = 0;
	ssize_t rc = fi_getname(&ep->ep->fid, NULL, &my_len);
	if (rc != -FI_ETOOSMALL)
		die_fi("fi_getname size", rc);
	void *mine = malloc(my_len), *peer = NULL;
	if (!mine)
		die("malloc local address");
	check_fi("fi_getname", fi_getname(&ep->ep->fid, mine, &my_len));
	size_t peer_len = 0;
	exchange_blob(control_fd, mine, my_len, &peer, &peer_len, server);
	ssize_t inserted = fi_av_insert(ep->av, peer, 1, &ep->peer, 0, NULL);
	if (inserted != 1)
		die_fi("fi_av_insert", inserted);
	free(peer);
	free(mine);
}

static uint64_t host_to_be64(uint64_t value)
{
#if __BYTE_ORDER__ == __ORDER_LITTLE_ENDIAN__
	return __builtin_bswap64(value);
#else
	return value;
#endif
}

static uint64_t be64_to_host(uint64_t value)
{
	return host_to_be64(value);
}

static void exchange_rma(struct endpoint *ep, int fd, bool server)
{
	uint64_t mine[2] = {
		host_to_be64((uint64_t)(uintptr_t)ep->rx),
		host_to_be64(fi_mr_key(ep->mr)),
	};
	uint64_t peer[2];
	if (server) {
		io_full(fd, peer, sizeof(peer), false);
		io_full(fd, mine, sizeof(mine), true);
	} else {
		io_full(fd, mine, sizeof(mine), true);
		io_full(fd, peer, sizeof(peer), false);
	}
	ep->peer_rma_addr = be64_to_host(peer[0]);
	ep->peer_rma_key = be64_to_host(peer[1]);
}

static void post_recv(struct endpoint *ep, size_t size)
{
	for (;;) {
		ssize_t rc = fi_recv(ep->ep, ep->rx, size, fi_mr_desc(ep->mr),
				     FI_ADDR_UNSPEC, &ep->rx_ctx);
		if (!rc)
			return;
		if (rc != -FI_EAGAIN)
			die_fi("fi_recv", rc);
	}
}

static void post_send(struct endpoint *ep, size_t size)
{
	for (;;) {
		ssize_t rc = fi_send(ep->ep, ep->tx, size, fi_mr_desc(ep->mr), ep->peer,
				     &ep->tx_ctx);
		if (!rc)
			return;
		if (rc != -FI_EAGAIN)
			die_fi("fi_send", rc);
	}
}

static void post_write(struct endpoint *ep, size_t size)
{
	for (;;) {
		ssize_t rc = fi_writedata(ep->ep, ep->tx, size, fi_mr_desc(ep->mr),
					 0x454641ull, ep->peer, ep->peer_rma_addr,
					 ep->peer_rma_key, &ep->tx_ctx);
		if (!rc)
			return;
		if (rc != -FI_EAGAIN)
			die_fi("fi_writedata", rc);
	}
}

static void wait_completions(struct endpoint *ep, unsigned needed)
{
	unsigned got = 0;
	while (got < needed) {
		struct fi_cq_data_entry entries[2];
		ssize_t rc = fi_cq_read(ep->cq, entries, needed - got);
		if (rc > 0) {
			got += (unsigned)rc;
			continue;
		}
		if (rc == -FI_EAGAIN)
			continue;
		if (rc == -FI_EAVAIL) {
			struct fi_cq_err_entry err = {0};
			fi_cq_readerr(ep->cq, &err, 0);
			fprintf(stderr, "CQ error err=%d prov_errno=%d: %s\n", err.err,
				err.prov_errno, fi_strerror(err.err));
			exit(1);
		}
		die_fi("fi_cq_read", rc);
	}
}

static uint64_t now_ns(void)
{
	struct timespec ts;
	if (clock_gettime(CLOCK_MONOTONIC_RAW, &ts))
		die("clock_gettime");
	return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

static int cmp_u64(const void *a, const void *b)
{
	uint64_t aa = *(const uint64_t *)a, bb = *(const uint64_t *)b;
	return aa > bb ? 1 : aa < bb ? -1 : 0;
}

static uint64_t percentile(const uint64_t *v, size_t n, double p)
{
	size_t i = (size_t)(p * (double)(n - 1) + 0.5);
	return v[i];
}

static void close_endpoint(struct endpoint *ep)
{
	if (ep->mr) fi_close(&ep->mr->fid);
	if (ep->ep) fi_close(&ep->ep->fid);
	if (ep->cq) fi_close(&ep->cq->fid);
	if (ep->av) fi_close(&ep->av->fid);
	if (ep->domain) fi_close(&ep->domain->fid);
	if (ep->fabric) fi_close(&ep->fabric->fid);
	if (ep->info) fi_freeinfo(ep->info);
	free(ep->storage);
}

int main(int argc, char **argv)
{
	struct options opt = parse_args(argc, argv);
	pin_cpu(opt.cpu);
	int control_fd = opt.server ? tcp_server(opt.port) : tcp_client(opt.host, opt.port);
	struct endpoint ep = {0};
	open_endpoint(&opt, &ep);
	connect_fabric(&ep, control_fd, opt.server);
	if (opt.write_mode)
		exchange_rma(&ep, control_fd, opt.server);
	char ready = 'R';
	if (opt.server) {
		io_full(control_fd, &ready, 1, true);
		io_full(control_fd, &ready, 1, false);
	} else {
		io_full(control_fd, &ready, 1, false);
		io_full(control_fd, &ready, 1, true);
	}

	size_t total = opt.warmup + opt.iterations;
	uint64_t *samples = opt.server ? NULL : calloc(opt.iterations, sizeof(*samples));
	if (!opt.server && !samples)
		die("calloc samples");
	for (size_t i = 0; i < total; i++) {
		if (!opt.server || !opt.write_mode)
			post_recv(&ep, opt.size);
		if (opt.server) {
			wait_completions(&ep, 1);
			post_send(&ep, opt.size);
			wait_completions(&ep, 1);
		} else {
			uint64_t start = now_ns();
			if (opt.write_mode)
				post_write(&ep, opt.size);
			else
				post_send(&ep, opt.size);
			wait_completions(&ep, 2);
			uint64_t end = now_ns();
			if (i >= opt.warmup)
				samples[i - opt.warmup] = end - start;
		}
	}

	struct rlimit memlock = {0};
	getrlimit(RLIMIT_MEMLOCK, &memlock);
	const char *provider = ep.info->fabric_attr->prov_name ?: "unknown";
	const char *domain = ep.info->domain_attr->name ?: "unknown";
	const char *mode = opt.write_mode ? "efa-rdm-write-remote-cq-ack" : "efa-rdm-send-recv";
	if (opt.server) {
		printf("{\"role\":\"server\",\"mode\":\"%s\","
		       "\"provider\":\"%s\",\"domain\":\"%s\",\"cpu\":%d,"
		       "\"lane_count\":1,\"per_worker_qd\":1,\"aggregate_qd\":1,"
		       "\"payload_bytes\":%zu,\"warmup\":%zu,\"iterations\":%zu,"
		       "\"memlock_soft_bytes\":%llu}\n", mode, provider, domain, sched_getcpu(),
		       opt.size, opt.warmup, opt.iterations,
		       (unsigned long long)memlock.rlim_cur);
	} else {
		qsort(samples, opt.iterations, sizeof(*samples), cmp_u64);
		long double sum = 0;
		for (size_t i = 0; i < opt.iterations; i++) sum += samples[i];
		uint64_t min = samples[0], p50 = percentile(samples, opt.iterations, .50),
			 p95 = percentile(samples, opt.iterations, .95),
			 p99 = percentile(samples, opt.iterations, .99), max = samples[opt.iterations - 1];
		printf("{\"role\":\"client\",\"mode\":\"%s\","
		       "\"completion_semantics\":\"%s\","
		       "\"provider\":\"%s\",\"domain\":\"%s\",\"cpu\":%d,"
		       "\"lane_count\":1,\"per_worker_qd\":1,\"aggregate_qd\":1,"
		       "\"payload_bytes\":%zu,\"warmup\":%zu,\"iterations\":%zu,"
		       "\"rtt_min_us\":%.3f,\"rtt_p50_us\":%.3f,\"rtt_p95_us\":%.3f,"
		       "\"rtt_p99_us\":%.3f,\"rtt_max_us\":%.3f,\"rtt_mean_us\":%.3Lf,"
		       "\"one_way_lower_bound_min_us\":%.3f,"
		       "\"theoretical_sequential_iops_from_p50\":%.1f,"
		       "\"memlock_soft_bytes\":%llu}\n",
		       mode, opt.write_mode ? "remote-write-cq-and-explicit-reply" : "remote-receive-and-reply",
		       provider, domain, sched_getcpu(), opt.size, opt.warmup, opt.iterations,
		       min / 1000.0, p50 / 1000.0, p95 / 1000.0, p99 / 1000.0,
		       max / 1000.0, sum / (long double)opt.iterations / 1000.0L,
		       min / 2000.0, 1e9 / (double)p50,
		       (unsigned long long)memlock.rlim_cur);
	}
	free(samples);
	close(control_fd);
	close_endpoint(&ep);
	return 0;
}
