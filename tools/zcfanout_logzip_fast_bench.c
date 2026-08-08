// SPDX-License-Identifier: MIT

#define _GNU_SOURCE

#include <errno.h>
#include <inttypes.h>
#include <limits.h>
#include <pthread.h>
#include <sched.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <time.h>
#include <unistd.h>

#ifndef RUSAGE_THREAD
#define RUSAGE_THREAD RUSAGE_SELF
#endif

#define ZC_LOGZIP_MAX_BRANCHES 64
#define ZC_LOGZIP_CACHELINE 64

enum zc_logzip_mode {
	ZC_LOGZIP_MIRROR_WRITE,
	ZC_LOGZIP_MIRROR_READ,
	ZC_LOGZIP_STRIPE_READ,
};

struct options {
	enum zc_logzip_mode mode;
	size_t lanes;
	size_t branches;
	uint64_t records_per_lane;
	size_t payload_bytes;
	size_t window;
	size_t workers;
	uint64_t skew;
	bool pin_workers;
};

struct affinity_plan {
	size_t *cpu_list;
	size_t cpu_list_len;
	size_t base_cpu;
	size_t cpu_count;
	size_t stride;
	long online_cpus;
	bool explicit_map;
	const char *source;
};

struct zc_logzip_slot {
	uint64_t sequence;
	uint64_t mask;
	uint64_t payload_token;
};

struct zc_logzip_lane {
	size_t lane_id;
	struct zc_logzip_slot *slots;
	uint64_t next_emit;
	uint64_t emitted;
	uint64_t duplicate_results;
	uint64_t checksum;
};

struct worker_stats {
	size_t worker;
	size_t lane_count;
	uint64_t result_records;
	uint64_t emitted;
	uint64_t duplicate_results;
	uint64_t logical_bytes;
	uint64_t branch_bytes;
	uint64_t checksum;
	double seconds;
	double user_cpu;
	double sys_cpu;
	long voluntary_switches;
	long involuntary_switches;
	int target_cpu;
	int start_cpu;
	int end_cpu;
	bool affinity_applied;
	int error;
	char error_msg[256];
};

struct worker_arg {
	const struct options *opt;
	const struct affinity_plan *affinity;
	pthread_barrier_t *ready;
	pthread_barrier_t *start;
	struct worker_stats *stats;
	size_t worker;
	size_t worker_count;
	bool window_power2;
	uint64_t window_mask;
	uint64_t required_mask;
	uint64_t branch_delay_span;
};

static void usage(const char *argv0)
{
	fprintf(stderr,
		"usage: %s [--mode mirror-write|mirror-read|stripe-read]\n"
		"       [--lanes N] [--branches N] [--records-per-lane N]\n"
		"       [--payload-bytes N] [--window N] [--workers N]\n"
		"       [--skew N] [--pin true|false] [--no-pin]\n\n"
		"This is a descriptor-only userspace result-log zipper microbench.\n"
		"It does not use transport sockets or block devices as mirror/stripe primitives.\n",
		argv0);
}

static void die_oom(void)
{
	fprintf(stderr, "out of memory\n");
	exit(1);
}

static double now_seconds(void)
{
	struct timespec ts;

	if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) {
		fprintf(stderr, "clock_gettime: %s\n", strerror(errno));
		exit(1);
	}
	return (double)ts.tv_sec + (double)ts.tv_nsec / 1000000000.0;
}

static double timeval_delta_sec(const struct timeval *end,
				const struct timeval *start)
{
	return (double)(end->tv_sec - start->tv_sec) +
	       (double)(end->tv_usec - start->tv_usec) / 1000000.0;
}

static uint64_t parse_u64(const char *text, const char *name)
{
	char *end = NULL;
	unsigned long long value;

	errno = 0;
	value = strtoull(text, &end, 0);
	if (errno || !end || *end) {
		fprintf(stderr, "invalid %s: %s\n", name, text);
		exit(2);
	}
	return (uint64_t)value;
}

static bool parse_bool(const char *text, const char *name)
{
	if (!strcmp(text, "1") || !strcmp(text, "true") ||
	    !strcmp(text, "yes") || !strcmp(text, "on"))
		return true;
	if (!strcmp(text, "0") || !strcmp(text, "false") ||
	    !strcmp(text, "no") || !strcmp(text, "off"))
		return false;

	fprintf(stderr, "invalid %s: %s\n", name, text);
	exit(2);
}

static size_t parse_size_arg(const char *text, const char *name)
{
	char *end = NULL;
	unsigned long long value;
	uint64_t multiplier = 1;

	errno = 0;
	value = strtoull(text, &end, 10);
	if (errno || end == text) {
		fprintf(stderr, "invalid %s: %s\n", name, text);
		exit(2);
	}
	if (!strcmp(end, "k") || !strcmp(end, "K") ||
	    !strcmp(end, "kiB") || !strcmp(end, "KiB") ||
	    !strcmp(end, "KIB")) {
		multiplier = 1024ULL;
	} else if (!strcmp(end, "m") || !strcmp(end, "M") ||
		   !strcmp(end, "miB") || !strcmp(end, "MiB") ||
		   !strcmp(end, "MIB")) {
		multiplier = 1024ULL * 1024ULL;
	} else if (!strcmp(end, "g") || !strcmp(end, "G") ||
		   !strcmp(end, "giB") || !strcmp(end, "GiB") ||
		   !strcmp(end, "GIB")) {
		multiplier = 1024ULL * 1024ULL * 1024ULL;
	} else if (*end) {
		fprintf(stderr, "invalid %s suffix: %s\n", name, text);
		exit(2);
	}
	if (value > SIZE_MAX / multiplier) {
		fprintf(stderr, "%s overflows size_t: %s\n", name, text);
		exit(2);
	}
	return (size_t)(value * multiplier);
}

static enum zc_logzip_mode parse_mode(const char *text)
{
	if (!strcmp(text, "mirror-write") || !strcmp(text, "write") ||
	    !strcmp(text, "mirror-all"))
		return ZC_LOGZIP_MIRROR_WRITE;
	if (!strcmp(text, "mirror-read") || !strcmp(text, "read-first") ||
	    !strcmp(text, "mirror-first"))
		return ZC_LOGZIP_MIRROR_READ;
	if (!strcmp(text, "stripe-read") || !strcmp(text, "stripe") ||
	    !strcmp(text, "read-all"))
		return ZC_LOGZIP_STRIPE_READ;

	fprintf(stderr,
		"unknown mode %s; use mirror-write, mirror-read, or stripe-read\n",
		text);
	exit(2);
}

static const char *mode_name(enum zc_logzip_mode mode)
{
	switch (mode) {
	case ZC_LOGZIP_MIRROR_WRITE:
		return "mirror-write";
	case ZC_LOGZIP_MIRROR_READ:
		return "mirror-read";
	case ZC_LOGZIP_STRIPE_READ:
		return "stripe-read";
	}
	return "unknown";
}

static struct options parse_args(int argc, char **argv)
{
	struct options opt = {
		.mode = ZC_LOGZIP_MIRROR_WRITE,
		.lanes = 32,
		.branches = 2,
		.records_per_lane = 1000000,
		.payload_bytes = 4096,
		.window = 8192,
		.workers = 0,
		.skew = 64,
		.pin_workers = true,
	};

	for (int i = 1; i < argc; i++) {
		if (!strcmp(argv[i], "--mode") && i + 1 < argc) {
			opt.mode = parse_mode(argv[++i]);
		} else if (!strcmp(argv[i], "--lanes") && i + 1 < argc) {
			opt.lanes = (size_t)parse_u64(argv[++i], "--lanes");
		} else if (!strcmp(argv[i], "--branches") && i + 1 < argc) {
			opt.branches = (size_t)parse_u64(argv[++i], "--branches");
		} else if ((!strcmp(argv[i], "--records-per-lane") ||
			    !strcmp(argv[i], "--records")) &&
			   i + 1 < argc) {
			opt.records_per_lane =
				parse_u64(argv[++i], "--records-per-lane");
		} else if ((!strcmp(argv[i], "--payload-bytes") ||
			    !strcmp(argv[i], "--payload")) &&
			   i + 1 < argc) {
			opt.payload_bytes =
				parse_size_arg(argv[++i], "--payload-bytes");
		} else if (!strcmp(argv[i], "--window") && i + 1 < argc) {
			opt.window = (size_t)parse_u64(argv[++i], "--window");
		} else if (!strcmp(argv[i], "--workers") && i + 1 < argc) {
			opt.workers = (size_t)parse_u64(argv[++i], "--workers");
		} else if (!strcmp(argv[i], "--skew") && i + 1 < argc) {
			opt.skew = parse_u64(argv[++i], "--skew");
		} else if (!strcmp(argv[i], "--pin")) {
			if (i + 1 < argc && argv[i + 1][0] != '-')
				opt.pin_workers =
					parse_bool(argv[++i], "--pin");
			else
				opt.pin_workers = true;
		} else if (!strcmp(argv[i], "--no-pin")) {
			opt.pin_workers = false;
		} else if (!strcmp(argv[i], "--help") || !strcmp(argv[i], "-h")) {
			usage(argv[0]);
			exit(0);
		} else {
			usage(argv[0]);
			exit(2);
		}
	}

	if (!opt.lanes || !opt.branches || !opt.records_per_lane ||
	    !opt.payload_bytes || !opt.window) {
		fprintf(stderr,
			"lanes, branches, records-per-lane, payload-bytes, and window must be non-zero\n");
		exit(2);
	}
	if (opt.branches > ZC_LOGZIP_MAX_BRANCHES) {
		fprintf(stderr, "branches must be <= %d\n",
			ZC_LOGZIP_MAX_BRANCHES);
		exit(2);
	}
	if (opt.mode == ZC_LOGZIP_STRIPE_READ &&
	    opt.payload_bytes % opt.branches != 0) {
		fprintf(stderr,
			"stripe-read payload-bytes must divide evenly by branches\n");
		exit(2);
	}

	return opt;
}

static bool is_power_of_two(size_t value)
{
	return value && !(value & (value - 1));
}

static long online_cpu_count(void)
{
	long cpus = sysconf(_SC_NPROCESSORS_ONLN);

	return cpus > 0 ? cpus : 1;
}

static bool env_present(const char *name)
{
	const char *value = getenv(name);

	return value && value[0];
}

static size_t parse_env_size(const char *name, size_t fallback)
{
	const char *value = getenv(name);

	if (!value || !value[0])
		return fallback;
	return (size_t)parse_u64(value, name);
}

static void append_cpu(size_t **cpus, size_t *len, size_t *cap, size_t cpu)
{
	if (*len == *cap) {
		size_t new_cap = *cap ? *cap * 2 : 16;
		size_t *new_cpus = realloc(*cpus, new_cap * sizeof(**cpus));

		if (!new_cpus)
			die_oom();
		*cpus = new_cpus;
		*cap = new_cap;
	}
	(*cpus)[(*len)++] = cpu;
}

static char *trim_token(char *text)
{
	while (*text == ' ' || *text == '\t' || *text == '\n')
		text++;
	char *end = text + strlen(text);

	while (end > text &&
	       (end[-1] == ' ' || end[-1] == '\t' || end[-1] == '\n')) {
		end--;
		*end = '\0';
	}
	return text;
}

static void parse_cpu_list(const char *raw, size_t **cpus, size_t *cpu_count)
{
	char *copy = strdup(raw);
	char *save = NULL;
	size_t len = 0;
	size_t cap = 0;

	if (!copy)
		die_oom();

	for (char *tok = strtok_r(copy, ",", &save); tok;
	     tok = strtok_r(NULL, ",", &save)) {
		char *token = trim_token(tok);
		char *dash;

		if (!*token)
			continue;
		dash = strchr(token, '-');
		if (dash) {
			uint64_t first;
			uint64_t last;

			*dash = '\0';
			first = parse_u64(token, "URING_PLAY_PIN_CPU_LIST");
			last = parse_u64(dash + 1, "URING_PLAY_PIN_CPU_LIST");
			if (last < first || last > SIZE_MAX) {
				fprintf(stderr,
					"invalid URING_PLAY_PIN_CPU_LIST range: %s-%s\n",
					token, dash + 1);
				free(copy);
				exit(2);
			}
			for (uint64_t cpu = first; cpu <= last; cpu++)
				append_cpu(cpus, &len, &cap, (size_t)cpu);
		} else {
			uint64_t cpu = parse_u64(token,
						 "URING_PLAY_PIN_CPU_LIST");

			if (cpu > SIZE_MAX) {
				fprintf(stderr,
					"URING_PLAY_PIN_CPU_LIST CPU overflows size_t: %" PRIu64 "\n",
					cpu);
				free(copy);
				exit(2);
			}
			append_cpu(cpus, &len, &cap, (size_t)cpu);
		}
	}

	free(copy);
	if (!len) {
		fprintf(stderr, "URING_PLAY_PIN_CPU_LIST did not contain any CPUs\n");
		exit(2);
	}
	*cpu_count = len;
}

static struct affinity_plan build_affinity_plan(void)
{
	struct affinity_plan plan = {
		.cpu_list = NULL,
		.cpu_list_len = 0,
		.base_cpu = 0,
		.cpu_count = 0,
		.stride = 1,
		.online_cpus = online_cpu_count(),
		.explicit_map = false,
		.source = "implicit-online-cpus",
	};
	const char *cpu_list = getenv("URING_PLAY_PIN_CPU_LIST");

	if (cpu_list && cpu_list[0]) {
		parse_cpu_list(cpu_list, &plan.cpu_list, &plan.cpu_list_len);
		plan.explicit_map = true;
		plan.source = "URING_PLAY_PIN_CPU_LIST";
		return plan;
	}

	plan.base_cpu = parse_env_size("URING_PLAY_PIN_BASE_CPU", 0);
	plan.cpu_count = parse_env_size("URING_PLAY_PIN_CPU_COUNT",
					(size_t)plan.online_cpus);
	plan.stride = parse_env_size("URING_PLAY_PIN_STRIDE", 1);
	if (!plan.cpu_count)
		plan.cpu_count = (size_t)plan.online_cpus;
	if (!plan.stride)
		plan.stride = 1;
	if (env_present("URING_PLAY_PIN_BASE_CPU") ||
	    env_present("URING_PLAY_PIN_CPU_COUNT") ||
	    env_present("URING_PLAY_PIN_STRIDE")) {
		plan.explicit_map = true;
		plan.source = "URING_PLAY_PIN_BASE_CPU/COUNT/STRIDE";
	}
	return plan;
}

static size_t affinity_target_cpu(const struct affinity_plan *plan, size_t index)
{
	if (plan->cpu_list_len)
		return plan->cpu_list[index % plan->cpu_list_len];
	return plan->base_cpu + (index * plan->stride) % plan->cpu_count;
}

static bool pin_current_thread(size_t cpu)
{
	cpu_set_t set;

	if (cpu >= CPU_SETSIZE)
		return false;
	CPU_ZERO(&set);
	CPU_SET(cpu, &set);
	return sched_setaffinity(0, sizeof(set), &set) == 0;
}

static uint64_t full_branch_mask(size_t branches)
{
	if (branches == 64)
		return UINT64_MAX;
	return (1ULL << branches) - 1ULL;
}

static uint64_t payload_token(size_t lane, size_t branch, uint64_t sequence,
			      size_t payload_bytes)
{
	return ((uint64_t)lane << 48) ^ ((uint64_t)branch << 40) ^
	       sequence ^ (uint64_t)payload_bytes;
}

static size_t slot_index(uint64_t sequence, size_t window, bool power2,
			 uint64_t mask)
{
	if (power2)
		return (size_t)(sequence & mask);
	return (size_t)(sequence % window);
}

static void init_lane_slots(struct zc_logzip_lane *lane, size_t window)
{
	for (size_t i = 0; i < window; i++) {
		lane->slots[i].sequence = UINT64_MAX;
		lane->slots[i].mask = 0;
		lane->slots[i].payload_token = 0;
	}
}

static void zc_logzip_advance(struct zc_logzip_lane *lane, size_t window,
			      bool power2, uint64_t window_mask,
			      uint64_t required_mask)
{
	for (;;) {
		size_t idx = slot_index(lane->next_emit, window, power2,
					window_mask);
		struct zc_logzip_slot *slot = &lane->slots[idx];

		if (slot->sequence != lane->next_emit ||
		    (slot->mask & required_mask) != required_mask)
			break;

		lane->checksum += slot->payload_token + lane->next_emit;
		slot->sequence = UINT64_MAX;
		slot->mask = 0;
		slot->payload_token = 0;
		lane->next_emit++;
		lane->emitted++;
	}
}

static int zc_logzip_process(struct zc_logzip_lane *lane, size_t branch,
			     uint64_t sequence, uint64_t required_mask,
			     enum zc_logzip_mode mode, uint64_t token,
			     size_t window, bool power2, uint64_t window_mask,
			     char *error_msg, size_t error_len)
{
	struct zc_logzip_slot *slot;
	size_t idx;
	uint64_t bit = 1ULL << branch;

	if (sequence < lane->next_emit) {
		lane->duplicate_results++;
		lane->checksum += token + branch + sequence;
		return 0;
	}

	idx = slot_index(sequence, window, power2, window_mask);
	slot = &lane->slots[idx];
	if (slot->sequence != UINT64_MAX && slot->sequence != sequence) {
		snprintf(error_msg, error_len,
			 "lane=%zu window overflow next_emit=%" PRIu64
			 " incoming_sequence=%" PRIu64
			 " slot_sequence=%" PRIu64 " window=%zu",
			 lane->lane_id, lane->next_emit, sequence,
			 slot->sequence, window);
		return -1;
	}
	if (slot->sequence == UINT64_MAX) {
		slot->sequence = sequence;
		slot->mask = 0;
		slot->payload_token = token;
	}
	if (slot->mask & bit)
		lane->duplicate_results++;
	slot->mask |= bit;
	if (mode == ZC_LOGZIP_MIRROR_READ)
		slot->mask = required_mask;

	lane->checksum += slot->payload_token + slot->mask + sequence + branch;
	zc_logzip_advance(lane, window, power2, window_mask, required_mask);
	return 0;
}

static int alloc_worker_lanes(const struct worker_arg *arg,
			      struct zc_logzip_lane **lanes_out,
			      size_t *lane_count_out,
			      char *error_msg, size_t error_len)
{
	size_t lane_count = 0;
	struct zc_logzip_lane *lanes;
	size_t lane_index = 0;

	for (size_t lane = arg->worker; lane < arg->opt->lanes;
	     lane += arg->worker_count)
		lane_count++;

	lanes = calloc(lane_count ? lane_count : 1, sizeof(*lanes));
	if (!lanes) {
		snprintf(error_msg, error_len, "calloc lane state failed");
		return ENOMEM;
	}

	for (size_t lane = arg->worker; lane < arg->opt->lanes;
	     lane += arg->worker_count) {
		void *slots = NULL;
		size_t bytes;
		int ret;

		if (arg->opt->window > SIZE_MAX / sizeof(*lanes[lane_index].slots)) {
			snprintf(error_msg, error_len,
				 "window byte count overflows size_t");
			return EOVERFLOW;
		}
		bytes = arg->opt->window * sizeof(*lanes[lane_index].slots);
		ret = posix_memalign(&slots, ZC_LOGZIP_CACHELINE, bytes);
		if (ret) {
			snprintf(error_msg, error_len,
				 "posix_memalign slots failed: %s",
				 strerror(ret));
			return ret;
		}
		lanes[lane_index].lane_id = lane;
		lanes[lane_index].slots = slots;
		init_lane_slots(&lanes[lane_index], arg->opt->window);
		lane_index++;
	}

	*lanes_out = lanes;
	*lane_count_out = lane_count;
	return 0;
}

static void free_worker_lanes(struct zc_logzip_lane *lanes, size_t lane_count)
{
	if (!lanes)
		return;
	for (size_t i = 0; i < lane_count; i++)
		free(lanes[i].slots);
	free(lanes);
}

static int run_lane(struct zc_logzip_lane *lane, const struct worker_arg *arg,
		    uint64_t *result_records, char *error_msg,
		    size_t error_len)
{
	const struct options *opt = arg->opt;
	uint64_t end_tick;

	if (UINT64_MAX - opt->records_per_lane < arg->branch_delay_span) {
		snprintf(error_msg, error_len,
			 "records-per-lane plus branch skew overflows u64");
		return -1;
	}
	end_tick = opt->records_per_lane + arg->branch_delay_span;

	for (uint64_t tick = 0; tick < end_tick; tick++) {
		for (size_t branch = 0; branch < opt->branches; branch++) {
			uint64_t delay = opt->skew * branch;
			uint64_t sequence;
			uint64_t token;

			if (tick < delay)
				continue;
			sequence = tick - delay;
			if (sequence >= opt->records_per_lane)
				continue;

			token = payload_token(lane->lane_id, branch, sequence,
					      opt->payload_bytes);
			if (zc_logzip_process(lane, branch, sequence,
					      arg->required_mask, opt->mode,
					      token, opt->window,
					      arg->window_power2,
					      arg->window_mask, error_msg,
					      error_len) != 0)
				return -1;
			(*result_records)++;
		}
	}

	zc_logzip_advance(lane, opt->window, arg->window_power2,
			  arg->window_mask, arg->required_mask);
	if (lane->emitted != opt->records_per_lane) {
		snprintf(error_msg, error_len,
			 "lane=%zu emitted %" PRIu64 " of %" PRIu64
			 " records",
			 lane->lane_id, lane->emitted, opt->records_per_lane);
		return -1;
	}
	return 0;
}

static void *worker_main(void *data)
{
	struct worker_arg *arg = data;
	const struct options *opt = arg->opt;
	struct worker_stats *stats = arg->stats;
	struct zc_logzip_lane *lanes = NULL;
	struct rusage usage_start;
	struct rusage usage_end;
	size_t lane_count = 0;
	double start;
	int alloc_ret;

	stats->worker = arg->worker;
	stats->target_cpu = opt->pin_workers ?
		(int)affinity_target_cpu(arg->affinity, arg->worker) : -1;
	stats->start_cpu = sched_getcpu();
	stats->end_cpu = stats->start_cpu;

	if (opt->pin_workers && stats->target_cpu >= 0)
		stats->affinity_applied =
			pin_current_thread((size_t)stats->target_cpu);

	alloc_ret = alloc_worker_lanes(arg, &lanes, &lane_count,
				       stats->error_msg,
				       sizeof(stats->error_msg));
	stats->lane_count = lane_count;
	if (alloc_ret)
		stats->error = alloc_ret;

	pthread_barrier_wait(arg->ready);
	pthread_barrier_wait(arg->start);

	if (stats->error)
		goto out;

	getrusage(RUSAGE_THREAD, &usage_start);
	start = now_seconds();
	for (size_t i = 0; i < lane_count; i++) {
		if (run_lane(&lanes[i], arg, &stats->result_records,
			     stats->error_msg, sizeof(stats->error_msg)) != 0) {
			stats->error = EINVAL;
			break;
		}
		stats->emitted += lanes[i].emitted;
		stats->duplicate_results += lanes[i].duplicate_results;
		stats->checksum += lanes[i].checksum;
	}
	stats->seconds = now_seconds() - start;
	getrusage(RUSAGE_THREAD, &usage_end);
	stats->user_cpu = timeval_delta_sec(&usage_end.ru_utime,
					    &usage_start.ru_utime);
	stats->sys_cpu = timeval_delta_sec(&usage_end.ru_stime,
					   &usage_start.ru_stime);
	stats->voluntary_switches = usage_end.ru_nvcsw - usage_start.ru_nvcsw;
	stats->involuntary_switches = usage_end.ru_nivcsw -
				      usage_start.ru_nivcsw;
	stats->end_cpu = sched_getcpu();

out:
	free_worker_lanes(lanes, lane_count);
	return NULL;
}

static size_t auto_workers(size_t requested, size_t lanes, long online_cpus)
{
	size_t online = online_cpus > 0 ? (size_t)online_cpus : 1;

	if (requested)
		return requested > lanes ? lanes : requested;
	return lanes < online ? lanes : online;
}

static void validate_options(const struct options *opt, size_t worker_count,
			     uint64_t *branch_delay_span)
{
	if (opt->branches > 1 &&
	    opt->skew > UINT64_MAX / (opt->branches - 1)) {
		fprintf(stderr, "skew * (branches - 1) overflows u64\n");
		exit(2);
	}
	*branch_delay_span = opt->skew * (opt->branches - 1);
	if (*branch_delay_span >= opt->window) {
		fprintf(stderr,
			"window=%zu must exceed max branch skew span %" PRIu64 "\n",
			opt->window, *branch_delay_span);
		exit(2);
	}
	if (!worker_count) {
		fprintf(stderr, "worker count must be non-zero\n");
		exit(2);
	}
}

static void print_lanes_for_worker(size_t worker, size_t worker_count,
				   size_t lanes)
{
	size_t printed = 0;

	printf(" lanes=");
	for (size_t lane = worker; lane < lanes; lane += worker_count) {
		if (printed)
			printf(",");
		if (printed >= 16) {
			printf("...");
			return;
		}
		printf("%zu", lane);
		printed++;
	}
	if (!printed)
		printf("-");
}

int main(int argc, char **argv)
{
	struct options opt = parse_args(argc, argv);
	struct affinity_plan affinity = build_affinity_plan();
	size_t worker_count = auto_workers(opt.workers, opt.lanes,
					   affinity.online_cpus);
	uint64_t branch_delay_span = 0;
	bool window_power2 = is_power_of_two(opt.window);
	uint64_t window_mask = window_power2 ? opt.window - 1 : 0;
	uint64_t full_mask = full_branch_mask(opt.branches);
	uint64_t required_mask =
		opt.mode == ZC_LOGZIP_MIRROR_READ ? 1ULL : full_mask;
	pthread_barrier_t ready;
	pthread_barrier_t start_barrier;
	pthread_t *threads;
	struct worker_arg *args;
	struct worker_stats *stats;
	uint64_t branch_bytes_per_record;
	uint64_t total_result_records = 0;
	uint64_t total_emitted = 0;
	uint64_t total_duplicate_results = 0;
	uint64_t total_logical_bytes = 0;
	uint64_t total_branch_bytes = 0;
	uint64_t total_checksum = 0;
	double max_seconds = 0.0;
	double wall_start;
	double wall_seconds;
	int failed = 0;

	validate_options(&opt, worker_count, &branch_delay_span);

	if (!opt.pin_workers) {
		fprintf(stderr,
			"PERF WARNING: zcfanout-logzip-fast-bench running without worker pinning; set --pin true and URING_PLAY_PIN_CPU_LIST before treating numbers as representative\n");
	} else if (!affinity.explicit_map) {
		fprintf(stderr,
			"PERF WARNING: zcfanout-logzip-fast-bench pinning uses implicit CPU mapping; state lane-to-worker and lane-to-CPU mapping before treating numbers as representative\n");
	}
	if (worker_count < opt.lanes) {
		fprintf(stderr,
			"PERF WARNING: zcfanout-logzip-fast-bench workers=%zu lanes=%zu; at least one worker owns multiple lanes, so report the printed lane map with results\n",
			worker_count, opt.lanes);
	}
	if ((long)worker_count > affinity.online_cpus) {
		fprintf(stderr,
			"PERF WARNING: zcfanout-logzip-fast-bench workers=%zu exceeds online_cpus=%ld; this topology is oversubscribed\n",
			worker_count, affinity.online_cpus);
	}
	if (!window_power2) {
		fprintf(stderr,
			"PERF WARNING: zcfanout-logzip-fast-bench window=%zu is not a power of two; modulo slot lookup is slower than the mask fast path\n",
			opt.window);
	}

	printf("zcfanout-logzip-fast-bench: mode=%s lanes=%zu branches=%zu "
	       "records_per_lane=%" PRIu64 " payload_bytes=%zu window=%zu "
	       "workers=%zu skew=%" PRIu64 " pin_workers=%s "
	       "descriptor_only=yes result_record_bytes=24 window_power2=%s "
	       "lane_worker_map=round-robin cpu_map_source=%s sort=no "
	       "global_queue=no payload_copy=no deep_payload_inspection=no "
	       "transport=no block_devices=no placement_owner=userspace\n",
	       mode_name(opt.mode), opt.lanes, opt.branches,
	       opt.records_per_lane, opt.payload_bytes, opt.window,
	       worker_count, opt.skew, opt.pin_workers ? "true" : "false",
	       window_power2 ? "true" : "false", affinity.source);

	branch_bytes_per_record =
		opt.mode == ZC_LOGZIP_STRIPE_READ ?
			opt.payload_bytes :
			opt.payload_bytes * opt.branches;

	threads = calloc(worker_count, sizeof(*threads));
	args = calloc(worker_count, sizeof(*args));
	stats = calloc(worker_count, sizeof(*stats));
	if (!threads || !args || !stats)
		die_oom();

	if (pthread_barrier_init(&ready, NULL, (unsigned int)worker_count + 1) ||
	    pthread_barrier_init(&start_barrier, NULL,
				 (unsigned int)worker_count + 1)) {
		fprintf(stderr, "pthread_barrier_init failed\n");
		return 1;
	}

	for (size_t worker = 0; worker < worker_count; worker++) {
		args[worker] = (struct worker_arg) {
			.opt = &opt,
			.affinity = &affinity,
			.ready = &ready,
			.start = &start_barrier,
			.stats = &stats[worker],
			.worker = worker,
			.worker_count = worker_count,
			.window_power2 = window_power2,
			.window_mask = window_mask,
			.required_mask = required_mask,
			.branch_delay_span = branch_delay_span,
		};
		if (pthread_create(&threads[worker], NULL, worker_main,
				   &args[worker])) {
			fprintf(stderr, "pthread_create worker=%zu failed: %s\n",
				worker, strerror(errno));
			return 1;
		}
	}

	pthread_barrier_wait(&ready);
	wall_start = now_seconds();
	pthread_barrier_wait(&start_barrier);

	for (size_t worker = 0; worker < worker_count; worker++)
		pthread_join(threads[worker], NULL);
	wall_seconds = now_seconds() - wall_start;

	for (size_t worker = 0; worker < worker_count; worker++) {
		struct worker_stats *s = &stats[worker];
		double secs = s->seconds > 0.0 ? s->seconds : 1e-12;

		if (s->error) {
			fprintf(stderr,
				"zcfanout-logzip-fast-worker-error: worker=%zu error=%s detail=%s\n",
				worker, strerror(s->error), s->error_msg);
			failed = 1;
		}

		s->logical_bytes = s->emitted * opt.payload_bytes;
		s->branch_bytes = s->emitted * branch_bytes_per_record;
		printf("zcfanout-logzip-fast-worker-map: worker=%zu "
		       "target_cpu=%d affinity_applied=%s start_cpu=%d "
		       "end_cpu=%d lane_count=%zu lane_first=%zu "
		       "lane_step=%zu",
		       worker, s->target_cpu,
		       s->affinity_applied ? "true" : "false",
		       s->start_cpu, s->end_cpu, s->lane_count, worker,
		       worker_count);
		print_lanes_for_worker(worker, worker_count, opt.lanes);
		printf("\n");
		printf("zcfanout-logzip-fast-worker: worker=%zu lanes=%zu "
		       "result_records=%" PRIu64 " emitted=%" PRIu64
		       " duplicate_results=%" PRIu64 " logical_bytes=%" PRIu64
		       " branch_bytes=%" PRIu64 " checksum=0x%016" PRIx64
		       " seconds=%.6f result_records_per_sec=%.0f "
		       "emitted_per_sec=%.0f logical_4k_iops=%.0f "
		       "branch_Gibitps=%.3f user_cpu=%.6f sys_cpu=%.6f "
		       "voluntary_switches=%ld involuntary_switches=%ld\n",
		       worker, s->lane_count, s->result_records, s->emitted,
		       s->duplicate_results, s->logical_bytes, s->branch_bytes,
		       s->checksum, s->seconds, s->result_records / secs,
		       s->emitted / secs, s->logical_bytes / 4096.0 / secs,
		       s->branch_bytes * 8.0 / 1000000000.0 / secs,
		       s->user_cpu, s->sys_cpu, s->voluntary_switches,
		       s->involuntary_switches);

		total_result_records += s->result_records;
		total_emitted += s->emitted;
		total_duplicate_results += s->duplicate_results;
		total_logical_bytes += s->logical_bytes;
		total_branch_bytes += s->branch_bytes;
		total_checksum += s->checksum;
		if (s->seconds > max_seconds)
			max_seconds = s->seconds;
	}

	if (failed)
		return 1;

	if (max_seconds <= 0.0)
		max_seconds = wall_seconds > 0.0 ? wall_seconds : 1e-12;

	printf("zcfanout-logzip-fast-summary: mode=%s lanes=%zu branches=%zu "
	       "result_records=%" PRIu64 " emitted=%" PRIu64
	       " duplicate_results=%" PRIu64 " logical_bytes=%" PRIu64
	       " branch_bytes=%" PRIu64 " checksum=0x%016" PRIx64
	       " seconds=%.6f wall_seconds=%.6f result_records_per_sec=%.0f "
	       "emitted_per_sec=%.0f logical_4k_iops=%.0f logical_GiBps=%.3f "
	       "branch_Gibitps=%.3f descriptor_only=yes lane_worker_map=round-robin "
	       "cpu_map_source=%s sort=no global_queue=no payload_copy=no "
	       "deep_payload_inspection=no transport=no block_devices=no\n",
	       mode_name(opt.mode), opt.lanes, opt.branches,
	       total_result_records, total_emitted, total_duplicate_results,
	       total_logical_bytes, total_branch_bytes, total_checksum,
	       max_seconds, wall_seconds, total_result_records / max_seconds,
	       total_emitted / max_seconds,
	       total_logical_bytes / 4096.0 / max_seconds,
	       total_logical_bytes / (1024.0 * 1024.0 * 1024.0) / max_seconds,
	       total_branch_bytes * 8.0 / 1000000000.0 / max_seconds,
	       affinity.source);

	pthread_barrier_destroy(&ready);
	pthread_barrier_destroy(&start_barrier);
	free(threads);
	free(args);
	free(stats);
	free(affinity.cpu_list);
	return 0;
}
