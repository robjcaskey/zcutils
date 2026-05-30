// SPDX-License-Identifier: MIT

#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <liburing.h>
#include <limits.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <time.h>
#include <unistd.h>

#ifndef IORING_SETUP_SQE128
#define IORING_SETUP_SQE128 (1U << 10)
#endif

#ifndef IORING_OP_URING_CMD128
#define IORING_OP_URING_CMD128 64
#endif

#define ZCWALBLK_URING_MAGIC 0x31444d43574c415aULL
#define ZCWALBLK_URING_VERSION 1
#define ZCWALBLK_URING_OP_RESOLVE_BATCH 1U
#define ZCWALBLK_URING_DEFAULT_DISK UINT32_MAX

struct zcwalblk_uring_batch_cmd {
	uint64_t magic;
	uint32_t version;
	uint32_t flags;
	uint32_t disk_index;
	uint32_t count;
	uint32_t records_per_item;
	uint32_t reserved;
	uint64_t start_record;
	uint64_t stride_records;
	uint64_t result_addr;
	uint64_t result_len;
};

struct zcwalblk_uring_batch_result {
	uint64_t checksum;
	uint64_t items;
	uint64_t records;
	uint64_t logical_records;
};

static void usage(const char *argv0)
{
	fprintf(stderr,
		"usage: %s [--dev /dev/zcwalctl] [--entries N] [--inflight N]\n"
		"       [--commands N] [--batch N] [--records-per-item N]\n"
		"       [--stride N] [--disk-index N] [--no-result]\n",
		argv0);
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

static double now_sec(void)
{
	struct timespec ts;

	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (double)ts.tv_sec + (double)ts.tv_nsec / 1000000000.0;
}

static double timeval_delta_sec(const struct timeval *end,
				const struct timeval *start)
{
	return (double)(end->tv_sec - start->tv_sec) +
	       (double)(end->tv_usec - start->tv_usec) / 1000000.0;
}

static void prep_uring_cmd128(struct io_uring_sqe *sqe, uint32_t cmd_op, int fd)
{
	memset(sqe, 0, sizeof(*sqe));
	sqe->opcode = IORING_OP_URING_CMD128;
	sqe->fd = fd;
	sqe->cmd_op = cmd_op;
	sqe->__pad1 = 0;
}

static __attribute__((noinline)) void
copy_sqe128_cmd(uintptr_t sqe_addr, const struct zcwalblk_uring_batch_cmd *cmd)
{
	void *dst = (void *)(sqe_addr + offsetof(struct io_uring_sqe, cmd));

	memcpy(dst, cmd, sizeof(*cmd));
}

int main(int argc, char **argv)
{
	const char *dev_path = "/dev/zcwalctl";
	uint32_t entries = 256;
	uint32_t inflight = 128;
	uint64_t commands = 200000;
	uint32_t batch = 256;
	uint32_t records_per_item = 1;
	uint64_t stride = 1;
	uint32_t disk_index = ZCWALBLK_URING_DEFAULT_DISK;
	bool no_result = false;
	struct zcwalblk_uring_batch_result *results;
	struct io_uring_params params = { };
	struct io_uring ring;
	uint64_t submitted = 0;
	uint64_t completed = 0;
	uint64_t logical_records;
	uint64_t checksum = 0;
	struct rusage usage_start;
	struct rusage usage_end;
	double user_cpu;
	double sys_cpu;
	double start;
	double elapsed;
	int fd;
	int ret;

	for (int i = 1; i < argc; i++) {
		if (!strcmp(argv[i], "--dev") && i + 1 < argc) {
			dev_path = argv[++i];
		} else if (!strcmp(argv[i], "--entries") && i + 1 < argc) {
			entries = (uint32_t)parse_u64(argv[++i], "entries");
		} else if (!strcmp(argv[i], "--inflight") && i + 1 < argc) {
			inflight = (uint32_t)parse_u64(argv[++i], "inflight");
		} else if (!strcmp(argv[i], "--commands") && i + 1 < argc) {
			commands = parse_u64(argv[++i], "commands");
		} else if (!strcmp(argv[i], "--batch") && i + 1 < argc) {
			batch = (uint32_t)parse_u64(argv[++i], "batch");
		} else if (!strcmp(argv[i], "--records-per-item") && i + 1 < argc) {
			records_per_item = (uint32_t)parse_u64(argv[++i],
							       "records-per-item");
		} else if (!strcmp(argv[i], "--stride") && i + 1 < argc) {
			stride = parse_u64(argv[++i], "stride");
		} else if (!strcmp(argv[i], "--disk-index") && i + 1 < argc) {
			disk_index = (uint32_t)parse_u64(argv[++i], "disk-index");
		} else if (!strcmp(argv[i], "--no-result")) {
			no_result = true;
		} else if (!strcmp(argv[i], "--help") || !strcmp(argv[i], "-h")) {
			usage(argv[0]);
			return 0;
		} else {
			usage(argv[0]);
			return 2;
		}
	}

	if (!entries || !inflight || inflight > entries || !commands ||
	    !batch || !records_per_item) {
		usage(argv[0]);
		return 2;
	}
	if (UINT64_MAX / batch / records_per_item < commands) {
		fprintf(stderr, "logical record count overflows u64\n");
		return 2;
	}

	fd = open(dev_path, O_RDONLY | O_CLOEXEC);
	if (fd < 0) {
		perror(dev_path);
		return 1;
	}

	results = calloc(inflight, sizeof(*results));
	if (!results) {
		perror("calloc");
		close(fd);
		return 1;
	}

	params.flags = IORING_SETUP_SQE128;
	ret = io_uring_queue_init_params(entries, &ring, &params);
	if (ret < 0) {
		fprintf(stderr, "io_uring_queue_init_params: %s\n", strerror(-ret));
		free(results);
		close(fd);
		return 1;
	}

	getrusage(RUSAGE_SELF, &usage_start);
	start = now_sec();
	while (completed < commands) {
		while (submitted < commands && submitted - completed < inflight) {
			struct zcwalblk_uring_batch_cmd cmd = {
				.magic = ZCWALBLK_URING_MAGIC,
				.version = ZCWALBLK_URING_VERSION,
				.disk_index = disk_index,
				.count = batch,
				.records_per_item = records_per_item,
				.start_record = submitted *
					(uint64_t)batch * records_per_item,
				.stride_records = stride,
			};
			struct io_uring_sqe *sqe = io_uring_get_sqe(&ring);
			uint32_t slot;

			if (!sqe)
				break;

			slot = submitted % inflight;
			memset(&results[slot], 0, sizeof(results[slot]));
			if (!no_result) {
				cmd.result_addr = (uint64_t)(uintptr_t)&results[slot];
				cmd.result_len = sizeof(results[slot]);
			}

			prep_uring_cmd128(sqe, ZCWALBLK_URING_OP_RESOLVE_BATCH, fd);
			copy_sqe128_cmd((uintptr_t)sqe, &cmd);
			sqe->user_data = slot;
			submitted++;
		}

		ret = io_uring_submit(&ring);
		if (ret < 0) {
			fprintf(stderr, "io_uring_submit: %s\n", strerror(-ret));
			goto fail;
		}

		while (completed < submitted) {
			struct io_uring_cqe *cqe;
			uint32_t slot;

			ret = io_uring_wait_cqe(&ring, &cqe);
			if (ret < 0) {
				fprintf(stderr, "io_uring_wait_cqe: %s\n",
					strerror(-ret));
				goto fail;
			}
			if (cqe->res < 0) {
				fprintf(stderr, "uring_cmd failed: %s (%d)\n",
					strerror(-cqe->res), cqe->res);
				io_uring_cqe_seen(&ring, cqe);
				goto fail;
			}

			slot = (uint32_t)cqe->user_data;
			if (!no_result) {
				uint64_t expected_records =
					(uint64_t)batch * records_per_item;

				if (results[slot].items != batch ||
				    results[slot].records != expected_records) {
					fprintf(stderr,
						"bad result slot=%u items=%" PRIu64
						" records=%" PRIu64 "\n",
						slot, results[slot].items,
						results[slot].records);
					io_uring_cqe_seen(&ring, cqe);
					goto fail;
				}
				checksum ^= results[slot].checksum + completed;
			}

			io_uring_cqe_seen(&ring, cqe);
			completed++;

			if (submitted < commands &&
			    submitted - completed < inflight)
				break;
		}
	}

	elapsed = now_sec() - start;
	getrusage(RUSAGE_SELF, &usage_end);
	user_cpu = timeval_delta_sec(&usage_end.ru_utime, &usage_start.ru_utime);
	sys_cpu = timeval_delta_sec(&usage_end.ru_stime, &usage_start.ru_stime);
	logical_records = commands * (uint64_t)batch * records_per_item;
	printf("dev=%s commands=%" PRIu64 " batch=%u records_per_item=%u "
	       "inflight=%u entries=%u result=%s elapsed=%.6f "
	       "cmd/s=%.3f logical_4k_iops=%.3f gib_s=%.3f "
	       "user_cpu=%.6f sys_cpu=%.6f cpu_pct=%.1f checksum=%" PRIu64 "\n",
	       dev_path, commands, batch, records_per_item, inflight, entries,
	       no_result ? "off" : "on", elapsed, commands / elapsed,
	       logical_records / elapsed,
	       (logical_records * 4096.0) / elapsed / 1024.0 / 1024.0 / 1024.0,
	       user_cpu, sys_cpu, elapsed > 0.0 ?
		       ((user_cpu + sys_cpu) * 100.0 / elapsed) : 0.0,
	       checksum);

	io_uring_queue_exit(&ring);
	free(results);
	close(fd);
	return 0;

fail:
	io_uring_queue_exit(&ring);
	free(results);
	close(fd);
	return 1;
}
