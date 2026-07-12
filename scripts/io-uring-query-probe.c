#define _GNU_SOURCE

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#define IORING_REGISTER_QUERY 35U
#define IO_URING_QUERY_OPCODES 0U
#define IO_URING_QUERY_ZCRX 1U
#define IORING_OP_SEND_ZC 47U
#define IORING_REGISTER_ZCRX_IFQ 32U

struct io_uring_query_hdr {
	uint64_t next_entry;
	uint64_t query_data;
	uint32_t query_op;
	uint32_t size;
	int32_t result;
	uint32_t resv[3];
};

struct io_uring_query_opcode {
	uint32_t nr_request_opcodes;
	uint32_t nr_register_opcodes;
	uint64_t feature_flags;
	uint64_t ring_setup_flags;
	uint64_t enter_flags;
	uint64_t sqe_flags;
	uint32_t nr_query_opcodes;
	uint32_t pad;
};

struct io_uring_query_zcrx {
	uint64_t register_flags;
	uint64_t area_flags;
	uint32_t nr_ctrl_opcodes;
	uint32_t features;
	uint32_t rq_hdr_size;
	uint32_t rq_hdr_alignment;
	uint64_t resv2;
};

static int query(uint32_t op, void *data, uint32_t size, int32_t *result)
{
	struct io_uring_query_hdr hdr = {
		.query_data = (uintptr_t)data,
		.query_op = op,
		.size = size,
	};
	long ret;

	ret = syscall(SYS_io_uring_register, -1, IORING_REGISTER_QUERY, &hdr, 0);
	if (ret < 0) {
		fprintf(stderr, "io_uring query %u failed: %s\n", op,
			strerror(errno));
		return -1;
	}
	*result = hdr.result;
	return 0;
}

int main(void)
{
	struct io_uring_query_opcode opcodes = {0};
	struct io_uring_query_zcrx zcrx = {0};
	int32_t opcode_result;
	int32_t zcrx_result;
	int ok;

	if (query(IO_URING_QUERY_OPCODES, &opcodes, sizeof(opcodes),
		  &opcode_result) != 0)
		return 1;

	ok = opcodes.nr_request_opcodes > IORING_OP_SEND_ZC &&
		opcodes.nr_register_opcodes > IORING_REGISTER_ZCRX_IFQ &&
		opcodes.nr_query_opcodes > IO_URING_QUERY_ZCRX;
	printf("opcode_query_result=%d request_opcodes=%u register_opcodes=%u "
	       "query_opcodes=%u send_zc=%s zcrx_ifq=%s\n",
	       opcode_result, opcodes.nr_request_opcodes,
	       opcodes.nr_register_opcodes, opcodes.nr_query_opcodes,
	       opcodes.nr_request_opcodes > IORING_OP_SEND_ZC ? "yes" : "no",
	       opcodes.nr_register_opcodes > IORING_REGISTER_ZCRX_IFQ ? "yes" : "no");

	if (query(IO_URING_QUERY_ZCRX, &zcrx, sizeof(zcrx), &zcrx_result) != 0)
		return 1;
	printf("zcrx_query_result=%d register_flags=0x%llx area_flags=0x%llx "
	       "ctrl_opcodes=%u features=0x%x rq_hdr_size=%u rq_hdr_alignment=%u\n",
	       zcrx_result, (unsigned long long)zcrx.register_flags,
	       (unsigned long long)zcrx.area_flags, zcrx.nr_ctrl_opcodes,
	       zcrx.features, zcrx.rq_hdr_size, zcrx.rq_hdr_alignment);

	return ok && opcode_result == 0 && zcrx_result == 0 ? 0 : 1;
}
