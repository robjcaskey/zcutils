// SPDX-License-Identifier: MIT
// Versioned, pointer-free ABI between the placement-free miniport and its broker.
#pragma once
#include <stdint.h>

#define ZCNBLK_SERVICE_MAGIC UINT64_C(0x5a434e4257494e31)
#define ZCNBLK_SERVICE_VERSION 1u
#define ZCNBLK_SERVICE_HEADER 80u
#define ZCNBLK_SERVICE_MAX_IO 65536u
#define ZCNBLK_SERVICE_QUERY 1u
#define ZCNBLK_SERVICE_CONNECT 2u
#define ZCNBLK_SERVICE_EXCHANGE 3u
#define ZCNBLK_SERVICE_DISCONNECT 4u
#define ZCNBLK_IO_WRITE 1u
#define ZCNBLK_IO_READ 2u
#define ZCNBLK_IO_SYNC 7u
#define ZCNBLK_IO_FUA 1u

typedef struct ZCNBLK_SERVICE_MESSAGE {
    uint64_t Magic;
    uint32_t Version;
    uint32_t Command;
    uint64_t Session;
    uint64_t RequestId;
    uint64_t Offset;
    uint64_t Capacity;
    uint32_t Op;
    uint32_t Length;
    uint32_t Flags;
    uint32_t Status;
    uint8_t UnitId[16];
    uint8_t Data[ZCNBLK_SERVICE_MAX_IO];
} ZCNBLK_SERVICE_MESSAGE;
