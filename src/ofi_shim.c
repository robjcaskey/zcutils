#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <rdma/fabric.h>
#include <rdma/fi_cm.h>
#include <rdma/fi_domain.h>
#include <rdma/fi_endpoint.h>
#include <rdma/fi_eq.h>
#include <rdma/fi_errno.h>
#include <rdma/fi_ext.h>
#if defined(__has_include)
#if __has_include(<rdma/fi_ext_efa.h>)
#include <rdma/fi_ext_efa.h>
#endif
#endif
#include <rdma/fi_rma.h>
#include <stdbool.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#ifndef FI_EPROTO
#define FI_EPROTO EPROTO
#endif

/*
 * FI_OPT_MAX_MSG_SIZE, FI_OPT_MAX_RMA_SIZE, and the standardized EFA
 * emulation query options were added to the public headers in libfabric 2.0.
 * Distribution images such as Ubuntu 24.04 still ship 1.x headers.  Keep the
 * baseline MSG providers buildable there, but leave the query return codes at
 * -FI_ENOSYS so strict EFA validation continues to fail closed instead of
 * silently assuming capabilities that the installed provider cannot prove.
 */
#if FI_MAJOR_VERSION >= 2
#define ZC_OFI_HAVE_ENDPOINT_LIMIT_OPTIONS 1
#define ZC_OFI_HAVE_EFA_EMULATION_OPTIONS 1
#else
#define ZC_OFI_HAVE_ENDPOINT_LIMIT_OPTIONS 0
#define ZC_OFI_HAVE_EFA_EMULATION_OPTIONS 0
#endif

#ifdef FI_EFA_WR_HIGH_PPS
#define ZC_OFI_HAVE_EFA_WR_HIGH_PPS 1
#else
/* Never synthesize a provider-reserved flag when the installed EFA headers
 * do not advertise it.  An older provider can accept an unknown bit without
 * executing the requested operation or producing a CQE. */
#define ZC_OFI_HAVE_EFA_WR_HIGH_PPS 0
#endif

enum zc_ofi_op_kind {
    ZC_OFI_OP_NONE = 0,
    ZC_OFI_OP_SEND = 1,
    ZC_OFI_OP_RECV = 2,
    ZC_OFI_OP_READ = 3,
    ZC_OFI_OP_WRITE = 4,
};

struct zc_ofi_mr_arena {
    const void *buf;
    size_t len;
    uint64_t access;
    struct fid_mr *mr;
    void *desc;
};

struct zc_ofi_mr_table {
    struct zc_ofi_mr_arena *arenas;
    size_t capacity;
    size_t count;
    size_t hot_index;
    uint64_t registrations;
    uint64_t closes;
    uint64_t lookups;
    uint64_t lookup_hits;
    uint64_t hot_registration_attempts;
    int posts_started;
};

struct zc_ofi_op {
    /* Must remain first: providers return this address in op_context. */
    struct fi_context2 context;
    uint64_t user_data;
    size_t len;
    fi_addr_t src_addr;
    int completion_rc;
    int prov_errno;
    uint8_t kind;
    uint8_t active;
    uint8_t completed;
    uint8_t completion_requested;
    uint8_t provider_cqe_seen;
};

struct zc_ofi_op_ring {
    struct zc_ofi_op *ops;
    size_t *completed_slots;
    size_t *posted_slots;
    size_t *completion_groups;
    size_t depth;
    size_t active;
    size_t provider_inflight;
    size_t peak_active;
    size_t next_slot;
    size_t reap_cursor;
    size_t completed_head;
    size_t completed_count;
    size_t posted_head;
    size_t posted_count;
    size_t completion_group_head;
    size_t completion_group_count;
    size_t open_completion_group_count;
    uint64_t posts;
    uint64_t completions;
    uint64_t post_eagain;
    uint64_t post_retries;
    uint64_t errors;
};

struct zc_ofi_cq_state {
    struct fi_cq_msg_entry *entries;
    fi_addr_t *sources;
    size_t batch_capacity;
    size_t configured_size;
    uint64_t polls;
    uint64_t nonempty_polls;
    uint64_t entries_read;
    uint64_t errors;
    uint64_t sleeps;
};

struct zc_ofi_endpoint {
    struct fi_info *info;
    struct fid_fabric *fabric;
    struct fid_domain *domain;
    struct fid_cq *tx_cq;
    struct fid_cq *rx_cq;
    struct fid_av *av;
    struct fid_ep *ep;
    fi_addr_t peer_addr;
    fi_addr_t last_src_addr;
    size_t av_insert_count;
    size_t max_msg_size;
    size_t max_rma_size;
    size_t inject_size;
    struct zc_ofi_mr_table send_mrs;
    struct zc_ofi_mr_table recv_mrs;
    struct zc_ofi_mr_table read_mrs;
    struct zc_ofi_mr_table write_mrs;
    /* The block read destination is one explicitly registered, stable shared
     * arena.  Keep its descriptor directly on the endpoint so every 4 KiB
     * post does not re-enter the generic MR table or dirty telemetry counters.
     * Disjoint diagnostic buffers still fall back to the table. */
    uintptr_t rma_read_arena_start;
    uintptr_t rma_read_arena_end;
    void *rma_read_arena_desc;
    struct fid_mr *rma_target_mr;
    const void *rma_target_buf;
    size_t rma_target_len;
    uint64_t rma_target_registrations;
    uint64_t rma_target_closes;
    uint64_t rma_target_hot_registration_attempts;
    struct zc_ofi_op_ring send_ring;
    struct zc_ofi_op_ring recv_ring;
    struct zc_ofi_op_ring read_ring;
    struct zc_ofi_op_ring write_ring;
    struct zc_ofi_cq_state tx_cq_state;
    struct zc_ofi_cq_state rx_cq_state;
    size_t legacy_recv_slot;
    size_t cq_headroom;
    size_t tx_cq_required;
    size_t rx_cq_required;
    size_t provider_tx_queue_requested;
    size_t provider_tx_queue_size;
    size_t provider_rx_queue_requested;
    size_t provider_rx_queue_size;
    uint32_t requested_api_version;
    uint32_t query_api_version;
    uint32_t returned_api_version;
    int strict_topology;
    int efa_provider;
    int efa_direct;
    int efa_write_high_pps_requested;
    int efa_write_high_pps_effective;
    int efa_write_high_pps_verified;
    uint64_t efa_write_high_pps_fallbacks;
    int emulated_read;
    int emulated_write;
    int emulated_read_query_rc;
    int emulated_write_query_rc;
    int max_msg_size_query_rc;
    int max_rma_size_query_rc;
    int selective_completion;
    size_t rma_read_completion_stride;
    size_t rma_read_completion_remaining;
    int rma_read_more_requested;
    int rma_read_more_enabled;
    struct fi_context2 rma_read_flush_context;
    uint8_t rma_read_flush_byte;
    void *rma_read_flush_desc;
    uint64_t rma_read_last_remote_addr;
    uint64_t rma_read_last_remote_key;
    size_t rma_read_completion_markers_inflight;
    uint64_t rma_read_periodic_markers;
    uint64_t rma_read_full_window_markers;
    uint64_t rma_read_forced_markers;
    uint64_t rma_read_marker_posts;
    uint64_t rma_read_flush_posts;
    int rma_read_flush_inflight;
    int rma_read_flush_cqe_seen;
    int rma_write_delivery_complete;
    int rma_write_more_enabled;
    int rma_write_force_flush;
    size_t rma_write_more_burst;
    size_t rma_write_more_streak;
    uint64_t rma_write_more_posts;
    uint64_t rma_write_flush_posts;
    uint64_t rma_write_forced_flush_posts;
    uint64_t rma_write_more_followup_eagain;
    int fatal_rc;
    int mr_local;
    int mr_virt_addr;
    uint64_t busy_poll_iters;
    uint64_t inject_posts;
    long cq_sleep_ns;
    char err[512];
};

static void zc_ofi_write_err(char *err, size_t err_len, const char *fmt, ...);

/* Ring callers keep both operands below depth, so their sum is below twice
 * depth.  A conditional subtract is exact and avoids a runtime integer
 * division in the per-operation post and completion paths. */
static inline size_t zc_ofi_ring_index(size_t base, size_t delta,
                                       size_t depth) {
    size_t index = base + delta;
    return index >= depth ? index - depth : index;
}

static uint64_t zc_ofi_now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000ULL + (uint64_t)ts.tv_nsec / 1000000ULL;
}

static uint64_t zc_ofi_env_u64(const char *name, uint64_t fallback) {
    const char *value = getenv(name);
    if (!value || value[0] == '\0') {
        return fallback;
    }
    char *end = NULL;
    errno = 0;
    unsigned long long parsed = strtoull(value, &end, 0);
    if (errno || end == value || *end != '\0' || value[0] == '-') {
        return fallback;
    }
    return (uint64_t)parsed;
}

static int zc_ofi_env_enabled(const char *name) {
    const char *value = getenv(name);
    if (!value || value[0] == '\0') {
        return 0;
    }
    return strcmp(value, "0") != 0 && strcmp(value, "false") != 0 &&
           strcmp(value, "no") != 0 && strcmp(value, "off") != 0;
}

static int zc_ofi_env_size(const char *name, size_t fallback, size_t min_value,
                           size_t max_value, size_t *out, char *err,
                           size_t err_len) {
    if (!out || min_value > fallback || fallback > max_value) {
        zc_ofi_write_err(err, err_len, "invalid default for %s", name);
        return -FI_EINVAL;
    }
    const char *value = getenv(name);
    if (!value || value[0] == '\0') {
        *out = fallback;
        return 0;
    }
    char *end = NULL;
    errno = 0;
    unsigned long long parsed = strtoull(value, &end, 0);
    if (errno || end == value || *end != '\0' || parsed < min_value ||
        parsed > max_value) {
        zc_ofi_write_err(err, err_len, "%s must be in %zu..=%zu, got %s",
                         name, min_value, max_value, value);
        return -FI_EINVAL;
    }
    *out = (size_t)parsed;
    return 0;
}

static void zc_ofi_cpu_relax(void) {
#if defined(__x86_64__) || defined(__i386__)
    __builtin_ia32_pause();
#elif defined(__aarch64__)
    __asm__ __volatile__("yield");
#endif
}

static void zc_ofi_wait_after_eagain(struct zc_ofi_endpoint *ep, uint64_t *spins) {
    (*spins)++;
    if (ep && *spins <= ep->busy_poll_iters) {
        zc_ofi_cpu_relax();
        return;
    }
    long sleep_ns = ep && ep->cq_sleep_ns >= 0 ? ep->cq_sleep_ns : 50000;
    if (sleep_ns == 0) {
        return;
    }
    struct timespec ts = {.tv_sec = 0, .tv_nsec = sleep_ns};
    nanosleep(&ts, NULL);
}

static int zc_ofi_poll_timed_out(const struct zc_ofi_endpoint *ep, uint64_t spins,
                                 uint64_t start_ms, int timeout_ms) {
    if (timeout_ms <= 0) {
        return 0;
    }
    /* A sleeping poll is already expensive enough to sample every pass. A
     * dedicated busy poll samples only once per 1024 misses so clock_gettime
     * does not dominate sub-20us completions. */
    if (ep && ep->cq_sleep_ns == 0 && (spins & 1023U) != 0) {
        return 0;
    }
    return zc_ofi_now_ms() - start_ms >= (uint64_t)timeout_ms;
}

static const char *zc_ofi_errstr(int rc) {
    if (rc < 0) {
        return fi_strerror(-rc);
    }
    return fi_strerror(rc);
}

static void zc_ofi_write_err(char *err, size_t err_len, const char *fmt, ...) {
    if (!err || err_len == 0) {
        return;
    }
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(err, err_len, fmt, ap);
    va_end(ap);
}

static int zc_ofi_fail(struct zc_ofi_endpoint *ep, int rc, const char *op) {
    if (ep) {
        snprintf(ep->err, sizeof(ep->err), "%s failed rc=%d (%s)", op, rc, zc_ofi_errstr(rc));
    }
    return rc ? rc : -FI_EIO;
}

static int zc_ofi_init_ring(struct zc_ofi_op_ring *ring, size_t depth,
                            enum zc_ofi_op_kind kind, char *err, size_t err_len) {
    if (!ring || depth == 0 || depth > 65536 || kind == ZC_OFI_OP_NONE) {
        zc_ofi_write_err(err, err_len, "invalid OFI %u ring depth=%zu",
                         (unsigned)kind, depth);
        return -FI_EINVAL;
    }
    struct zc_ofi_op *ops = calloc(depth, sizeof(*ops));
    size_t *completed_slots = calloc(depth, sizeof(*completed_slots));
    size_t *posted_slots = calloc(depth, sizeof(*posted_slots));
    size_t *completion_groups = calloc(depth, sizeof(*completion_groups));
    if (!ops || !completed_slots || !posted_slots || !completion_groups) {
        free(completion_groups);
        free(posted_slots);
        free(completed_slots);
        free(ops);
        zc_ofi_write_err(err, err_len, "calloc(OFI %u ring depth=%zu) failed",
                         (unsigned)kind, depth);
        return -FI_ENOMEM;
    }
    for (size_t i = 0; i < depth; i++) {
        ops[i].kind = (uint8_t)kind;
        ops[i].src_addr = FI_ADDR_UNSPEC;
    }
    free(ring->completed_slots);
    free(ring->posted_slots);
    free(ring->completion_groups);
    free(ring->ops);
    memset(ring, 0, sizeof(*ring));
    ring->ops = ops;
    ring->completed_slots = completed_slots;
    ring->posted_slots = posted_slots;
    ring->completion_groups = completion_groups;
    ring->depth = depth;
    return 0;
}

static int zc_ofi_init_mr_table(struct zc_ofi_mr_table *table, size_t capacity,
                                char *err, size_t err_len) {
    if (!table || capacity == 0 || capacity > 65536) {
        zc_ofi_write_err(err, err_len, "invalid OFI MR arena capacity=%zu", capacity);
        return -FI_EINVAL;
    }
    table->arenas = calloc(capacity, sizeof(*table->arenas));
    if (!table->arenas) {
        zc_ofi_write_err(err, err_len, "calloc(OFI MR arenas=%zu) failed", capacity);
        return -FI_ENOMEM;
    }
    table->capacity = capacity;
    table->hot_index = SIZE_MAX;
    return 0;
}

static int zc_ofi_init_cq_state(struct zc_ofi_cq_state *state, size_t cq_size,
                                size_t batch_capacity, char *err, size_t err_len) {
    if (!state || cq_size == 0 || batch_capacity == 0 ||
        batch_capacity > cq_size) {
        zc_ofi_write_err(err, err_len,
                         "invalid OFI CQ size=%zu batch_capacity=%zu",
                         cq_size, batch_capacity);
        return -FI_EINVAL;
    }
    state->entries = calloc(batch_capacity, sizeof(*state->entries));
    state->sources = calloc(batch_capacity, sizeof(*state->sources));
    if (!state->entries || !state->sources) {
        free(state->sources);
        free(state->entries);
        memset(state, 0, sizeof(*state));
        zc_ofi_write_err(err, err_len,
                         "calloc(OFI CQ batch capacity=%zu) failed", batch_capacity);
        return -FI_ENOMEM;
    }
    state->configured_size = cq_size;
    state->batch_capacity = batch_capacity;
    return 0;
}

static void zc_ofi_close_fid(struct fid *fid) {
    if (fid) {
        fi_close(fid);
    }
}

static void zc_ofi_close_mr_table(struct zc_ofi_mr_table *table) {
    if (!table) {
        return;
    }
    for (size_t i = 0; i < table->count; i++) {
        if (table->arenas[i].mr) {
            zc_ofi_close_fid(&table->arenas[i].mr->fid);
            table->closes++;
        }
    }
    free(table->arenas);
    table->arenas = NULL;
    table->capacity = 0;
    table->count = 0;
    table->hot_index = SIZE_MAX;
}

static void zc_ofi_free_ring(struct zc_ofi_op_ring *ring) {
    if (!ring) {
        return;
    }
    free(ring->completed_slots);
    free(ring->posted_slots);
    free(ring->completion_groups);
    free(ring->ops);
    memset(ring, 0, sizeof(*ring));
}

static void zc_ofi_free_cq_state(struct zc_ofi_cq_state *state) {
    if (!state) {
        return;
    }
    free(state->sources);
    free(state->entries);
    memset(state, 0, sizeof(*state));
}

void zc_ofi_close(struct zc_ofi_endpoint *ep) {
    if (!ep) {
        return;
    }
    zc_ofi_close_fid(ep->ep ? &ep->ep->fid : NULL);
    zc_ofi_close_mr_table(&ep->send_mrs);
    zc_ofi_close_mr_table(&ep->recv_mrs);
    zc_ofi_close_mr_table(&ep->read_mrs);
    zc_ofi_close_mr_table(&ep->write_mrs);
    if (ep->rma_target_mr) {
        zc_ofi_close_fid(&ep->rma_target_mr->fid);
        ep->rma_target_closes++;
    }
    zc_ofi_close_fid(ep->av ? &ep->av->fid : NULL);
    zc_ofi_close_fid(ep->rx_cq ? &ep->rx_cq->fid : NULL);
    zc_ofi_close_fid(ep->tx_cq ? &ep->tx_cq->fid : NULL);
    zc_ofi_close_fid(ep->domain ? &ep->domain->fid : NULL);
    zc_ofi_close_fid(ep->fabric ? &ep->fabric->fid : NULL);
    zc_ofi_free_cq_state(&ep->tx_cq_state);
    zc_ofi_free_cq_state(&ep->rx_cq_state);
    zc_ofi_free_ring(&ep->send_ring);
    zc_ofi_free_ring(&ep->recv_ring);
    zc_ofi_free_ring(&ep->read_ring);
    zc_ofi_free_ring(&ep->write_ring);
    if (ep->info) {
        fi_freeinfo(ep->info);
    }
    free(ep);
}

const char *zc_ofi_last_error(const struct zc_ofi_endpoint *ep) {
    if (!ep || ep->err[0] == '\0') {
        return "";
    }
    return ep->err;
}

size_t zc_ofi_max_msg_size(const struct zc_ofi_endpoint *ep) {
    return ep ? ep->max_msg_size : 0;
}

size_t zc_ofi_max_rma_size(const struct zc_ofi_endpoint *ep) {
    return ep ? ep->max_rma_size : 0;
}

size_t zc_ofi_inject_size(const struct zc_ofi_endpoint *ep) {
    return ep ? ep->inject_size : 0;
}

static int zc_ofi_finish_format(struct zc_ofi_endpoint *ep, char *buf,
                                size_t capacity, int written,
                                const char *label) {
    if (written < 0) {
        return zc_ofi_fail(ep, -FI_EIO, label);
    }
    if ((size_t)written >= capacity) {
        snprintf(ep->err, sizeof(ep->err),
                 "%s output needs %d bytes but capacity is %zu",
                 label, written + 1, capacity);
        return -FI_ETOOSMALL;
    }
    return 0;
}

int zc_ofi_format_profile(struct zc_ofi_endpoint *ep, char *buf,
                          size_t capacity) {
    if (!ep || !buf || capacity == 0 || !ep->info) {
        return -FI_EINVAL;
    }
    const char *provider = ep->info->fabric_attr &&
                                   ep->info->fabric_attr->prov_name
                               ? ep->info->fabric_attr->prov_name
                               : "unknown";
    const char *fabric = ep->info->fabric_attr && ep->info->fabric_attr->name
                             ? ep->info->fabric_attr->name
                             : "unknown";
    const char *domain = ep->info->domain_attr && ep->info->domain_attr->name
                             ? ep->info->domain_attr->name
                             : "unknown";
    const char *device = ep->info->nic && ep->info->nic->device_attr &&
                                 ep->info->nic->device_attr->name
                             ? ep->info->nic->device_attr->name
                             : "unknown";
    uint64_t mr_mode = ep->info->domain_attr ? ep->info->domain_attr->mr_mode : 0;
    int control_progress = ep->info->domain_attr
                               ? ep->info->domain_attr->control_progress
                               : FI_PROGRESS_UNSPEC;
    int data_progress = ep->info->domain_attr
                            ? ep->info->domain_attr->data_progress
                            : FI_PROGRESS_UNSPEC;
    size_t rma_iov_limit = ep->info->tx_attr ? ep->info->tx_attr->rma_iov_limit : 0;
    int written = snprintf(
        buf, capacity,
        "provider=%s fabric=%s domain=%s device=%s endpoint_type=%d "
        "api_requested=%u.%u api_query=%u.%u api_returned=%u.%u caps=%llu mode=%llu "
        "mr_mode=%llu mr_local=%d mr_virt_addr=%d control_progress=%d "
        "data_progress=%d max_msg_size=%zu max_msg_size_query_rc=%d "
        "max_rma_size=%zu max_rma_size_query_rc=%d inject_size=%zu rma_iov_limit=%zu "
        "efa=%d efa_direct=%d efa_emulated_read=%d efa_emulated_read_query_rc=%d "
        "efa_emulated_write=%d efa_emulated_write_query_rc=%d "
        "efa_write_high_pps_available=%d "
        "efa_write_high_pps_requested=%d efa_write_high_pps_effective=%d "
        "efa_write_high_pps_verified=%d "
        "provider_tx_queue_requested=%zu provider_tx_queue_size=%zu "
        "provider_rx_queue_requested=%zu provider_rx_queue_size=%zu "
        "tx_cq_size=%zu tx_cq_required=%zu "
        "rx_cq_size=%zu rx_cq_required=%zu tx_cq_batch=%zu rx_cq_batch=%zu "
        "cq_headroom=%zu cq_sleep_ns=%ld threading=%d strict_topology=%d selective_completion=%d "
        "rma_read_completion_stride=%zu rma_read_more_requested=%d rma_read_more=%d "
        "rma_write_delivery_complete=%d rma_write_more=%d rma_write_more_burst=%zu",
        provider, fabric, domain, device,
        ep->info->ep_attr ? ep->info->ep_attr->type : FI_EP_UNSPEC,
        FI_MAJOR(ep->requested_api_version), FI_MINOR(ep->requested_api_version),
        FI_MAJOR(ep->query_api_version), FI_MINOR(ep->query_api_version),
        FI_MAJOR(ep->returned_api_version), FI_MINOR(ep->returned_api_version),
        (unsigned long long)ep->info->caps,
        (unsigned long long)ep->info->mode,
        (unsigned long long)mr_mode, ep->mr_local, ep->mr_virt_addr,
        control_progress, data_progress, ep->max_msg_size,
        ep->max_msg_size_query_rc, ep->max_rma_size,
        ep->max_rma_size_query_rc, ep->inject_size, rma_iov_limit,
        ep->efa_provider, ep->efa_direct, ep->emulated_read,
        ep->emulated_read_query_rc, ep->emulated_write,
        ep->emulated_write_query_rc, ZC_OFI_HAVE_EFA_WR_HIGH_PPS,
        ep->efa_write_high_pps_requested,
        ep->efa_write_high_pps_effective,
        ep->efa_write_high_pps_verified,
        ep->provider_tx_queue_requested, ep->provider_tx_queue_size,
        ep->provider_rx_queue_requested, ep->provider_rx_queue_size,
        ep->tx_cq_state.configured_size, ep->tx_cq_required,
        ep->rx_cq_state.configured_size, ep->rx_cq_required,
        ep->tx_cq_state.batch_capacity, ep->rx_cq_state.batch_capacity,
        ep->cq_headroom, ep->cq_sleep_ns,
        ep->info->domain_attr ? ep->info->domain_attr->threading : FI_THREAD_UNSPEC,
        ep->strict_topology, ep->selective_completion,
        ep->rma_read_completion_stride,
        ep->rma_read_more_requested, ep->rma_read_more_enabled,
        ep->rma_write_delivery_complete, ep->rma_write_more_enabled,
        ep->rma_write_more_burst);
    return zc_ofi_finish_format(ep, buf, capacity, written,
                                "zc_ofi_format_profile");
}

int zc_ofi_format_stats(struct zc_ofi_endpoint *ep, char *buf,
                        size_t capacity) {
    if (!ep || !buf || capacity == 0) {
        return -FI_EINVAL;
    }
    int written = snprintf(
        buf, capacity,
        "send_depth=%zu send_active=%zu send_inflight=%zu send_peak=%zu "
        "send_posts=%llu send_completions=%llu send_eagain=%llu send_retries=%llu send_errors=%llu "
        "recv_depth=%zu recv_active=%zu recv_inflight=%zu recv_peak=%zu "
        "recv_posts=%llu recv_completions=%llu recv_eagain=%llu recv_retries=%llu recv_errors=%llu "
        "read_depth=%zu read_active=%zu read_inflight=%zu read_peak=%zu "
        "read_posts=%llu read_completions=%llu read_eagain=%llu read_retries=%llu read_errors=%llu "
        "write_depth=%zu write_active=%zu write_inflight=%zu write_peak=%zu "
        "write_posts=%llu write_completions=%llu write_eagain=%llu write_retries=%llu write_errors=%llu "
        "tx_cq_polls=%llu tx_cq_empty=%llu tx_cq_nonempty=%llu tx_cq_entries=%llu tx_cq_errors=%llu tx_cq_sleeps=%llu "
        "rx_cq_polls=%llu rx_cq_empty=%llu rx_cq_nonempty=%llu rx_cq_entries=%llu rx_cq_errors=%llu rx_cq_sleeps=%llu "
        "send_mr_reg=%llu send_mr_close=%llu send_mr_hot=%llu send_mr_hits=%llu "
        "recv_mr_reg=%llu recv_mr_close=%llu recv_mr_hot=%llu recv_mr_hits=%llu "
        "read_mr_reg=%llu read_mr_close=%llu read_mr_hot=%llu read_mr_hits=%llu "
        "write_mr_reg=%llu write_mr_close=%llu write_mr_hot=%llu write_mr_hits=%llu "
        "target_mr_reg=%llu target_mr_close=%llu target_mr_hot=%llu "
        "inject_posts=%llu fatal_rc=%d efa_write_high_pps_available=%d "
        "efa_write_high_pps_effective=%d "
        "efa_write_high_pps_verified=%d efa_write_high_pps_fallbacks=%llu "
        "rma_write_delivery_complete=%d rma_write_more=%d "
        "rma_write_more_burst=%zu rma_write_more_posts=%llu "
        "rma_write_flush_posts=%llu rma_write_forced_flush_posts=%llu "
        "rma_write_more_followup_eagain=%llu rma_write_force_flush=%d "
        "rma_read_periodic_markers=%llu rma_read_full_window_markers=%llu "
        "rma_read_forced_markers=%llu rma_read_marker_posts=%llu "
        "rma_read_unsignaled_fast_posts=%llu rma_read_more_posts=%llu "
        "rma_read_flush_posts=%llu "
        "rma_read_markers_inflight=%zu "
        "tx_cq_avg_cqes_per_nonempty=%.2f rx_cq_avg_cqes_per_nonempty=%.2f",
        ep->send_ring.depth, ep->send_ring.active,
        ep->send_ring.provider_inflight, ep->send_ring.peak_active,
        (unsigned long long)ep->send_ring.posts,
        (unsigned long long)ep->send_ring.completions,
        (unsigned long long)ep->send_ring.post_eagain,
        (unsigned long long)ep->send_ring.post_retries,
        (unsigned long long)ep->send_ring.errors,
        ep->recv_ring.depth, ep->recv_ring.active,
        ep->recv_ring.provider_inflight, ep->recv_ring.peak_active,
        (unsigned long long)ep->recv_ring.posts,
        (unsigned long long)ep->recv_ring.completions,
        (unsigned long long)ep->recv_ring.post_eagain,
        (unsigned long long)ep->recv_ring.post_retries,
        (unsigned long long)ep->recv_ring.errors,
        ep->read_ring.depth, ep->read_ring.active,
        ep->read_ring.provider_inflight, ep->read_ring.peak_active,
        (unsigned long long)ep->read_ring.posts,
        (unsigned long long)ep->read_ring.completions,
        (unsigned long long)ep->read_ring.post_eagain,
        (unsigned long long)ep->read_ring.post_retries,
        (unsigned long long)ep->read_ring.errors,
        ep->write_ring.depth, ep->write_ring.active,
        ep->write_ring.provider_inflight, ep->write_ring.peak_active,
        (unsigned long long)ep->write_ring.posts,
        (unsigned long long)ep->write_ring.completions,
        (unsigned long long)ep->write_ring.post_eagain,
        (unsigned long long)ep->write_ring.post_retries,
        (unsigned long long)ep->write_ring.errors,
        (unsigned long long)ep->tx_cq_state.polls,
        (unsigned long long)(ep->tx_cq_state.polls -
                             ep->tx_cq_state.nonempty_polls -
                             ep->tx_cq_state.errors),
        (unsigned long long)ep->tx_cq_state.nonempty_polls,
        (unsigned long long)ep->tx_cq_state.entries_read,
        (unsigned long long)ep->tx_cq_state.errors,
        (unsigned long long)ep->tx_cq_state.sleeps,
        (unsigned long long)ep->rx_cq_state.polls,
        (unsigned long long)(ep->rx_cq_state.polls -
                             ep->rx_cq_state.nonempty_polls -
                             ep->rx_cq_state.errors),
        (unsigned long long)ep->rx_cq_state.nonempty_polls,
        (unsigned long long)ep->rx_cq_state.entries_read,
        (unsigned long long)ep->rx_cq_state.errors,
        (unsigned long long)ep->rx_cq_state.sleeps,
        (unsigned long long)ep->send_mrs.registrations,
        (unsigned long long)ep->send_mrs.closes,
        (unsigned long long)ep->send_mrs.hot_registration_attempts,
        (unsigned long long)ep->send_mrs.lookup_hits,
        (unsigned long long)ep->recv_mrs.registrations,
        (unsigned long long)ep->recv_mrs.closes,
        (unsigned long long)ep->recv_mrs.hot_registration_attempts,
        (unsigned long long)ep->recv_mrs.lookup_hits,
        (unsigned long long)ep->read_mrs.registrations,
        (unsigned long long)ep->read_mrs.closes,
        (unsigned long long)ep->read_mrs.hot_registration_attempts,
        (unsigned long long)ep->read_mrs.lookup_hits,
        (unsigned long long)ep->write_mrs.registrations,
        (unsigned long long)ep->write_mrs.closes,
        (unsigned long long)ep->write_mrs.hot_registration_attempts,
        (unsigned long long)ep->write_mrs.lookup_hits,
        (unsigned long long)ep->rma_target_registrations,
        (unsigned long long)ep->rma_target_closes,
        (unsigned long long)ep->rma_target_hot_registration_attempts,
        (unsigned long long)ep->inject_posts,
        ep->fatal_rc,
        ZC_OFI_HAVE_EFA_WR_HIGH_PPS,
        ep->efa_write_high_pps_effective,
        ep->efa_write_high_pps_verified,
        (unsigned long long)ep->efa_write_high_pps_fallbacks,
        ep->rma_write_delivery_complete, ep->rma_write_more_enabled,
        ep->rma_write_more_burst,
        (unsigned long long)ep->rma_write_more_posts,
        (unsigned long long)ep->rma_write_flush_posts,
        (unsigned long long)ep->rma_write_forced_flush_posts,
        (unsigned long long)ep->rma_write_more_followup_eagain,
        ep->rma_write_force_flush,
        (unsigned long long)ep->rma_read_periodic_markers,
        (unsigned long long)ep->rma_read_full_window_markers,
        (unsigned long long)ep->rma_read_forced_markers,
        (unsigned long long)ep->rma_read_marker_posts,
        (unsigned long long)(ep->rma_read_completion_stride > 1
                                 ? ep->read_ring.posts -
                                       ep->rma_read_marker_posts
                                 : 0),
        (unsigned long long)(ep->rma_read_more_enabled
                                 ? ep->read_ring.posts -
                                       ep->rma_read_marker_posts
                                 : 0),
        (unsigned long long)ep->rma_read_flush_posts,
        ep->rma_read_completion_markers_inflight,
        ep->tx_cq_state.nonempty_polls
            ? (double)ep->tx_cq_state.entries_read /
                  (double)ep->tx_cq_state.nonempty_polls
            : 0.0,
        ep->rx_cq_state.nonempty_polls
            ? (double)ep->rx_cq_state.entries_read /
                  (double)ep->rx_cq_state.nonempty_polls
            : 0.0);
    return zc_ofi_finish_format(ep, buf, capacity, written,
                                "zc_ofi_format_stats");
}

int zc_ofi_drain_send(struct zc_ofi_endpoint *ep, int timeout_ms);

int zc_ofi_get_name(struct zc_ofi_endpoint *ep, void *buf, size_t *len) {
    if (!ep || !buf || !len) {
        return -FI_EINVAL;
    }
    int rc = fi_getname(&ep->ep->fid, buf, len);
    if (rc) {
        return zc_ofi_fail(ep, rc, "fi_getname");
    }
    return 0;
}

int zc_ofi_set_peer(struct zc_ofi_endpoint *ep, const void *addr, size_t len) {
    if (!ep || !addr || len == 0) {
        return -FI_EINVAL;
    }
    fi_addr_t peer = FI_ADDR_UNSPEC;
    int rc = fi_av_insert(ep->av, addr, 1, &peer, 0, NULL);
    if (rc < 0) {
        snprintf(ep->err, sizeof(ep->err), "fi_av_insert(peer) rc=%d (%s)", rc, zc_ofi_errstr(rc));
        return rc;
    }
    if (peer == FI_ADDR_UNSPEC) {
        peer = ep->av_insert_count;
    }
    ep->av_insert_count++;
    ep->peer_addr = peer;
    return 0;
}

static enum fi_ep_type zc_ofi_ep_type(const char *endpoint) {
    if (!endpoint || endpoint[0] == '\0' || strcmp(endpoint, "rdm") == 0 ||
        strcmp(endpoint, "FI_EP_RDM") == 0) {
        return FI_EP_RDM;
    }
    if (strcmp(endpoint, "dgram") == 0 || strcmp(endpoint, "datagram") == 0 ||
        strcmp(endpoint, "FI_EP_DGRAM") == 0) {
        return FI_EP_DGRAM;
    }
    return FI_EP_UNSPEC;
}

static int zc_ofi_is_efa_provider(const char *provider) {
    return provider &&
           (strcmp(provider, "efa") == 0 || strcmp(provider, "efa-direct") == 0);
}

static int zc_ofi_uses_verbs_rc_mr(const char *provider) {
    static const char rxm_provider[] = "verbs;ofi_rxm";
    size_t rxm_len = sizeof(rxm_provider) - 1;
    return provider &&
           (strcmp(provider, "verbs") == 0 ||
            (strncmp(provider, rxm_provider, rxm_len) == 0 &&
             (provider[rxm_len] == '\0' || provider[rxm_len] == ';')));
}

static int zc_ofi_uses_ib_ud_addr(const char *provider) {
    return provider && strstr(provider, "ofi_rxd") != NULL;
}

static int zc_ofi_open_on_domain_caps(const char *provider, const char *endpoint,
                                       const char *node, const char *service, int server,
                                       const char *domain_name, uint64_t caps,
                                       uint64_t tx_bind_flags, uint64_t rx_bind_flags,
                                       size_t read_depth_override,
                                       size_t write_depth_override,
                                       struct zc_ofi_endpoint **out, char *err,
                                       size_t err_len) {
    if (!out) {
        zc_ofi_write_err(err, err_len, "zc_ofi_open called with null out");
        return -FI_EINVAL;
    }
    *out = NULL;
    if (!service || service[0] == '\0') {
        zc_ofi_write_err(err, err_len, "OFI service must be non-empty");
        return -FI_EINVAL;
    }
    enum fi_ep_type ep_type = zc_ofi_ep_type(endpoint);
    if (ep_type != FI_EP_RDM) {
        zc_ofi_write_err(err, err_len, "OFI WAL currently supports only FI_EP_RDM");
        return -FI_EINVAL;
    }

    struct fi_info *hints = fi_allocinfo();
    if (!hints) {
        zc_ofi_write_err(err, err_len, "fi_allocinfo failed");
        return -FI_ENOMEM;
    }
    size_t send_depth = 0;
    size_t recv_depth = 0;
    size_t read_depth = 0;
    size_t write_depth = 0;
    size_t mr_capacity = 0;
    size_t cq_headroom = 0;
    int rc = zc_ofi_env_size("URING_PLAY_OFI_TX_QUEUE_DEPTH", 64, 1, 65536,
                             &send_depth, err, err_len);
    if (!rc) {
        rc = zc_ofi_env_size("URING_PLAY_OFI_RX_QUEUE_DEPTH", 64, 1, 65536,
                             &recv_depth, err, err_len);
    }
    if (!rc && read_depth_override > 65536) {
        zc_ofi_write_err(err, err_len,
                         "OFI RMA read depth override=%zu exceeds 65536",
                         read_depth_override);
        rc = -FI_EINVAL;
    } else if (!rc && read_depth_override != 0) {
        read_depth = read_depth_override;
    } else if (!rc) {
        rc = zc_ofi_env_size("URING_PLAY_OFI_RMA_READ_QD", 1, 1, 65536,
                             &read_depth, err, err_len);
    }
    if (!rc && write_depth_override > 65536) {
        zc_ofi_write_err(err, err_len,
                         "OFI RMA write depth override=%zu exceeds 65536",
                         write_depth_override);
        rc = -FI_EINVAL;
    } else if (!rc && write_depth_override != 0) {
        write_depth = write_depth_override;
    } else if (!rc) {
        rc = zc_ofi_env_size("URING_PLAY_OFI_RMA_WRITE_QD", 1, 1, 65536,
                             &write_depth, err, err_len);
    }
    if (!rc) {
        rc = zc_ofi_env_size("URING_PLAY_OFI_MR_ARENA_COUNT", 64, 1, 65536,
                             &mr_capacity, err, err_len);
    }
    if (!rc) {
        rc = zc_ofi_env_size("URING_PLAY_OFI_CQ_HEADROOM", 64, 1, 65536,
                             &cq_headroom, err, err_len);
    }
    if (rc) {
        fi_freeinfo(hints);
        return rc;
    }
    if (send_depth > SIZE_MAX - read_depth ||
        send_depth + read_depth > SIZE_MAX - write_depth ||
        send_depth + read_depth + write_depth > SIZE_MAX - cq_headroom ||
        recv_depth > SIZE_MAX - cq_headroom) {
        fi_freeinfo(hints);
        zc_ofi_write_err(err, err_len, "OFI queue/CQ size arithmetic overflow");
        return -FI_EINVAL;
    }
    size_t provider_tx_queue_requested = send_depth + read_depth + write_depth;
    size_t provider_rx_queue_requested = recv_depth;
    /* fi_info queue sizes are provider work-request capacities, distinct from
     * CQ capacity.  Advertise the aggregate concurrently usable operation
     * rings before endpoint creation so verbs providers can allocate an SQ/RQ
     * large enough for the application contract. */
    hints->tx_attr->size = provider_tx_queue_requested;
    hints->rx_attr->size = provider_rx_queue_requested;
    int efa_provider = zc_ofi_is_efa_provider(provider);
    int verbs_rc_mr = zc_ofi_uses_verbs_rc_mr(provider);
    const char *efa_fabric = efa_provider ? getenv("URING_PLAY_OFI_EFA_FABRIC") : NULL;
    int efa_direct = (provider && strcmp(provider, "efa-direct") == 0) ||
                     (efa_fabric && strcmp(efa_fabric, "efa-direct") == 0);
    /* FI_SOURCE is a fi_getinfo input flag, not an endpoint capability.
     * Advertising it in hints->caps happens to be tolerated by the sockets
     * provider but makes verbs/RxM reject an otherwise valid RDM+RMA source
     * query with FI_ENODATA. */
    hints->caps = caps;
    hints->mode = efa_direct ? FI_CONTEXT2 : (efa_provider ? 0 : FI_CONTEXT);
    hints->addr_format = efa_provider
                             ? FI_ADDR_EFA
                             : (zc_ofi_uses_ib_ud_addr(provider)
                                    ? FI_FORMAT_UNSPEC
                                    : FI_SOCKADDR);
    hints->ep_attr->type = ep_type;
    const char *threading = getenv("URING_PLAY_OFI_THREADING");
    if (threading && strcmp(threading, "domain") == 0) {
        hints->domain_attr->threading = FI_THREAD_DOMAIN;
    } else if (threading && strcmp(threading, "endpoint") == 0) {
        hints->domain_attr->threading = FI_THREAD_ENDPOINT;
    } else if (threading && strcmp(threading, "safe") == 0) {
        hints->domain_attr->threading = FI_THREAD_SAFE;
    } else if (threading && threading[0] != '\0' &&
               strcmp(threading, "unspec") != 0) {
        zc_ofi_write_err(err, err_len,
                         "URING_PLAY_OFI_THREADING must be unspec, safe, domain, or endpoint");
        fi_freeinfo(hints);
        return -FI_EINVAL;
    }
    if (provider && provider[0] != '\0') {
        const char *query_provider = efa_direct ? "efa" : provider;
        hints->fabric_attr->prov_name = strdup(query_provider);
        if (!hints->fabric_attr->prov_name) {
            fi_freeinfo(hints);
            zc_ofi_write_err(err, err_len, "strdup(provider) failed");
            return -FI_ENOMEM;
        }
    }
    if (efa_provider) {
        const char *fabric = efa_fabric;
        if (!fabric || fabric[0] == '\0') {
            fabric = efa_direct ? "efa-direct" : "efa";
        }
        hints->fabric_attr->name = strdup(fabric);
        if (!hints->fabric_attr->name) {
            fi_freeinfo(hints);
            zc_ofi_write_err(err, err_len, "strdup(efa fabric) failed");
            return -FI_ENOMEM;
        }
    }
    if (efa_direct || verbs_rc_mr) {
        /*
         * efa-direct and the verbs core below RxM expose the device MR
         * contract verbatim.  In addition to requiring local descriptors
         * they require virtual addresses, provider-allocated keys, and
         * allocated-region registration semantics.  Supplying only
         * FI_MR_LOCAL causes both providers to reject an otherwise valid
         * RDM+RMA profile with FI_ENODATA.
         */
        hints->domain_attr->mr_mode = FI_MR_LOCAL | FI_MR_VIRT_ADDR |
                                      FI_MR_ALLOCATED | FI_MR_PROV_KEY;
    }
    const char *domain = (domain_name && domain_name[0] != '\0')
                             ? domain_name
                             : getenv("URING_PLAY_OFI_DOMAIN");
    if (domain && domain[0] != '\0') {
        hints->domain_attr->name = strdup(domain);
        if (!hints->domain_attr->name) {
            fi_freeinfo(hints);
            zc_ofi_write_err(err, err_len, "strdup(domain) failed");
            return -FI_ENOMEM;
        }
    }

    struct fi_info *info = NULL;
    const char *query_node = efa_provider ? NULL : node;
    const char *query_service = efa_provider ? NULL : service;
    uint64_t flags = (server && !efa_provider) ? FI_SOURCE : 0;
    uint32_t requested_api_version = FI_VERSION(2, 0);
    uint32_t query_api_version = requested_api_version;
    rc = fi_getinfo(query_api_version, query_node, query_service, flags, hints, &info);
    if (rc) {
        if (info) {
            fi_freeinfo(info);
            info = NULL;
        }
        query_api_version = FI_VERSION(1, 11);
        rc = fi_getinfo(query_api_version, query_node, query_service, flags, hints, &info);
    }
    fi_freeinfo(hints);
    if (rc) {
        zc_ofi_write_err(err, err_len,
                         "fi_getinfo provider=%s endpoint=%s node=%s service=%s server=%d query_node=%s query_service=%s flags=%llu rc=%d (%s)",
                         provider ? provider : "auto", endpoint ? endpoint : "rdm",
                         node ? node : "auto", service, server,
                         query_node ? query_node : "auto",
                         query_service ? query_service : "auto",
                         (unsigned long long)flags, rc, zc_ofi_errstr(rc));
        return rc;
    }

    struct zc_ofi_endpoint *ep = calloc(1, sizeof(*ep));
    if (!ep) {
        fi_freeinfo(info);
        zc_ofi_write_err(err, err_len, "calloc(endpoint) failed");
        return -FI_ENOMEM;
    }
    ep->info = info;
    ep->peer_addr = FI_ADDR_UNSPEC;
    ep->last_src_addr = FI_ADDR_UNSPEC;
    ep->legacy_recv_slot = SIZE_MAX;
    ep->max_msg_size = info->ep_attr ? info->ep_attr->max_msg_size : 0;
    ep->inject_size = info->tx_attr ? info->tx_attr->inject_size : 0;
    ep->provider_tx_queue_requested = provider_tx_queue_requested;
    ep->provider_tx_queue_size = info->tx_attr ? info->tx_attr->size : 0;
    ep->provider_rx_queue_requested = provider_rx_queue_requested;
    ep->provider_rx_queue_size = info->rx_attr ? info->rx_attr->size : 0;
    ep->requested_api_version = requested_api_version;
    ep->query_api_version = query_api_version;
    ep->returned_api_version = info->fabric_attr ? info->fabric_attr->api_version
                                                 : query_api_version;
    ep->efa_provider = efa_provider;
    ep->efa_direct = efa_direct;
    ep->efa_write_high_pps_requested =
        efa_provider &&
        zc_ofi_env_u64("URING_PLAY_OFI_EFA_WRITE_HIGH_PPS", 0) != 0;
    ep->efa_write_high_pps_effective =
        ep->efa_write_high_pps_requested && ZC_OFI_HAVE_EFA_WR_HIGH_PPS;
    ep->mr_local = info->domain_attr && (info->domain_attr->mr_mode & FI_MR_LOCAL);
    ep->mr_virt_addr =
        (info->domain_attr && (info->domain_attr->mr_mode & FI_MR_VIRT_ADDR)) ||
        efa_provider;
    uint64_t direct_mr_required = FI_MR_LOCAL | FI_MR_VIRT_ADDR |
                                  FI_MR_ALLOCATED | FI_MR_PROV_KEY;
    uint64_t returned_mr_mode =
        info->domain_attr ? info->domain_attr->mr_mode : 0;
    if (efa_direct && (!(info->mode & FI_CONTEXT2) ||
                       (returned_mr_mode & direct_mr_required) !=
                           direct_mr_required)) {
        zc_ofi_write_err(err, err_len,
                         "efa-direct requires FI_CONTEXT2 and mr_mode=0x%llx; returned mode=0x%llx mr_mode=0x%llx",
                         (unsigned long long)direct_mr_required,
                         (unsigned long long)info->mode,
                         (unsigned long long)returned_mr_mode);
        zc_ofi_close(ep);
        return -FI_EOPNOTSUPP;
    }
    ep->busy_poll_iters = zc_ofi_env_u64("URING_PLAY_OFI_BUSY_POLL_ITERS", 0);
    ep->cq_sleep_ns = (long)zc_ofi_env_u64("URING_PLAY_OFI_CQ_SLEEP_NS", 50000);
    ep->strict_topology = zc_ofi_env_enabled("URING_PLAY_TOPOLOGY_STRICT") ||
                          zc_ofi_env_enabled("URING_PLAY_TOPOLOGY_FATAL");
    ep->selective_completion =
        zc_ofi_env_enabled("URING_PLAY_OFI_SELECTIVE_COMPLETION");
    rc = zc_ofi_env_size("URING_PLAY_OFI_RMA_READ_COMPLETION_STRIDE", 1, 1,
                         65536, &ep->rma_read_completion_stride, err, err_len);
    if (rc) {
        zc_ofi_close(ep);
        return rc;
    }
    if (ep->rma_read_completion_stride > 1 && !ep->selective_completion) {
        zc_ofi_write_err(
            err, err_len,
            "RMA read completion stride=%zu requires URING_PLAY_OFI_SELECTIVE_COMPLETION=1",
            ep->rma_read_completion_stride);
        zc_ofi_close(ep);
        return -FI_EINVAL;
    }
    ep->rma_read_completion_remaining = ep->rma_read_completion_stride;
    const char *rma_read_more_env = getenv("URING_PLAY_OFI_RMA_READ_MORE");
    ep->rma_read_more_requested = rma_read_more_env
                                      ? zc_ofi_env_u64(
                                            "URING_PLAY_OFI_RMA_READ_MORE", 0) != 0
                                      : ep->efa_direct;
    ep->rma_read_more_enabled = ep->rma_read_more_requested &&
                                ep->selective_completion &&
                                ep->rma_read_completion_stride > 1;
    ep->rma_write_delivery_complete =
        zc_ofi_env_u64("URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE", 1) != 0;
    ep->rma_write_more_enabled =
        zc_ofi_env_enabled("URING_PLAY_OFI_RMA_WRITE_MORE");
    rc = zc_ofi_env_size("URING_PLAY_OFI_RMA_WRITE_MORE_BURST", 64, 1,
                         65536, &ep->rma_write_more_burst, err, err_len);
    if (rc) {
        zc_ofi_close(ep);
        return rc;
    }
    if (ep->efa_write_high_pps_requested &&
        !ZC_OFI_HAVE_EFA_WR_HIGH_PPS) {
        if (ep->strict_topology) {
            zc_ofi_write_err(
                err, err_len,
                "strict OFI topology requested FI_EFA_WR_HIGH_PPS, but the build-time EFA headers do not advertise that provider operation flag");
            zc_ofi_close(ep);
            return -FI_EOPNOTSUPP;
        }
        ep->efa_write_high_pps_fallbacks++;
    }
    if (ep->strict_topology && ep->cq_sleep_ns != 0) {
        zc_ofi_write_err(err, err_len,
                         "strict OFI topology requires URING_PLAY_OFI_CQ_SLEEP_NS=0, got %ld",
                         ep->cq_sleep_ns);
        zc_ofi_close(ep);
        return -FI_EINVAL;
    }
    if (ep->strict_topology &&
        (ep->provider_tx_queue_size < ep->provider_tx_queue_requested ||
         ep->provider_rx_queue_size < ep->provider_rx_queue_requested)) {
        zc_ofi_write_err(
            err, err_len,
            "strict OFI provider queue capacity below request tx=%zu requested=%zu rx=%zu requested=%zu",
            ep->provider_tx_queue_size, ep->provider_tx_queue_requested,
            ep->provider_rx_queue_size, ep->provider_rx_queue_requested);
        zc_ofi_close(ep);
        return -FI_ENOSPC;
    }
    ep->cq_headroom = cq_headroom;
    size_t tx_cq_required = send_depth + read_depth + write_depth + cq_headroom;
    size_t rx_cq_required = recv_depth + cq_headroom;
    ep->tx_cq_required = tx_cq_required;
    ep->rx_cq_required = rx_cq_required;
    size_t tx_cq_size = 0;
    size_t rx_cq_size = 0;
    size_t cq_batch = 0;
    size_t tx_cq_default = tx_cq_required;
    size_t rx_cq_default = rx_cq_required;
    const char *shared_cq = getenv("URING_PLAY_OFI_CQ_SIZE");
    if (shared_cq && shared_cq[0] != '\0') {
        size_t shared_cq_size = 0;
        size_t shared_default = tx_cq_required > rx_cq_required
                                    ? tx_cq_required
                                    : rx_cq_required;
        rc = zc_ofi_env_size("URING_PLAY_OFI_CQ_SIZE", shared_default, 1,
                             262144, &shared_cq_size, err, err_len);
        if (rc) {
            zc_ofi_close(ep);
            return rc;
        }
        tx_cq_default = shared_cq_size;
        rx_cq_default = shared_cq_size;
    }
    rc = zc_ofi_env_size("URING_PLAY_OFI_TX_CQ_SIZE", tx_cq_default, 1, 262144,
                         &tx_cq_size, err, err_len);
    if (!rc) {
        rc = zc_ofi_env_size("URING_PLAY_OFI_RX_CQ_SIZE", rx_cq_default, 1, 262144,
                             &rx_cq_size, err, err_len);
    }
    size_t default_batch = tx_cq_size < 64 ? tx_cq_size : 64;
    if (!rc) {
        rc = zc_ofi_env_size("URING_PLAY_OFI_CQ_BATCH", default_batch, 1,
                             65536, &cq_batch, err, err_len);
    }
    if (rc) {
        zc_ofi_close(ep);
        return rc;
    }
    if (tx_cq_size < tx_cq_required || rx_cq_size < rx_cq_required ||
        cq_batch > tx_cq_size || cq_batch > rx_cq_size) {
        zc_ofi_write_err(err, err_len,
                         "OFI CQ sizing insufficient tx=%zu required=%zu rx=%zu required=%zu batch=%zu",
                         tx_cq_size, tx_cq_required, rx_cq_size, rx_cq_required,
                         cq_batch);
        zc_ofi_close(ep);
        return -FI_EINVAL;
    }
    rc = zc_ofi_init_mr_table(&ep->send_mrs, mr_capacity, err, err_len);
    if (!rc) {
        rc = zc_ofi_init_mr_table(&ep->recv_mrs, mr_capacity, err, err_len);
    }
    if (!rc) {
        rc = zc_ofi_init_mr_table(&ep->read_mrs, mr_capacity, err, err_len);
    }
    if (!rc) {
        rc = zc_ofi_init_mr_table(&ep->write_mrs, mr_capacity, err, err_len);
    }
    if (!rc) {
        rc = zc_ofi_init_ring(&ep->send_ring, send_depth, ZC_OFI_OP_SEND,
                              err, err_len);
    }
    if (!rc) {
        rc = zc_ofi_init_ring(&ep->recv_ring, recv_depth, ZC_OFI_OP_RECV,
                              err, err_len);
    }
    if (!rc) {
        rc = zc_ofi_init_ring(&ep->read_ring, read_depth, ZC_OFI_OP_READ,
                              err, err_len);
    }
    if (!rc) {
        rc = zc_ofi_init_ring(&ep->write_ring, write_depth, ZC_OFI_OP_WRITE,
                              err, err_len);
    }
    if (!rc) {
        rc = zc_ofi_init_cq_state(&ep->tx_cq_state, tx_cq_size, cq_batch,
                                  err, err_len);
    }
    size_t rx_batch = cq_batch < rx_cq_size ? cq_batch : rx_cq_size;
    if (!rc) {
        rc = zc_ofi_init_cq_state(&ep->rx_cq_state, rx_cq_size, rx_batch,
                                  err, err_len);
    }
    if (rc) {
        zc_ofi_close(ep);
        return rc;
    }

    rc = fi_fabric(info->fabric_attr, &ep->fabric, NULL);
    if (rc) {
        zc_ofi_write_err(err, err_len, "fi_fabric rc=%d (%s)", rc, zc_ofi_errstr(rc));
        zc_ofi_close(ep);
        return rc;
    }
    rc = fi_domain(ep->fabric, info, &ep->domain, NULL);
    if (rc) {
        zc_ofi_write_err(err, err_len, "fi_domain rc=%d (%s)", rc, zc_ofi_errstr(rc));
        zc_ofi_close(ep);
        return rc;
    }

    struct fi_cq_attr cq_attr;
    memset(&cq_attr, 0, sizeof(cq_attr));
    cq_attr.format = FI_CQ_FORMAT_MSG;
    cq_attr.size = tx_cq_size;
    rc = fi_cq_open(ep->domain, &cq_attr, &ep->tx_cq, NULL);
    if (rc) {
        zc_ofi_write_err(err, err_len, "fi_cq_open(tx) rc=%d (%s)", rc, zc_ofi_errstr(rc));
        zc_ofi_close(ep);
        return rc;
    }
    cq_attr.size = rx_cq_size;
    rc = fi_cq_open(ep->domain, &cq_attr, &ep->rx_cq, NULL);
    if (rc) {
        zc_ofi_write_err(err, err_len, "fi_cq_open(rx) rc=%d (%s)", rc, zc_ofi_errstr(rc));
        zc_ofi_close(ep);
        return rc;
    }

    struct fi_av_attr av_attr;
    memset(&av_attr, 0, sizeof(av_attr));
    av_attr.type = FI_AV_TABLE;
    av_attr.count = 1;
    rc = fi_av_open(ep->domain, &av_attr, &ep->av, NULL);
    if (rc) {
        zc_ofi_write_err(err, err_len, "fi_av_open rc=%d (%s)", rc, zc_ofi_errstr(rc));
        zc_ofi_close(ep);
        return rc;
    }

    rc = fi_endpoint(ep->domain, info, &ep->ep, NULL);
    if (rc) {
        zc_ofi_write_err(err, err_len, "fi_endpoint rc=%d (%s)", rc, zc_ofi_errstr(rc));
        zc_ofi_close(ep);
        return rc;
    }
    if (ep->selective_completion) {
        tx_bind_flags |= FI_SELECTIVE_COMPLETION;
    }
    rc = fi_ep_bind(ep->ep, &ep->tx_cq->fid, tx_bind_flags);
    if (rc) {
        zc_ofi_write_err(err, err_len, "fi_ep_bind(tx_cq) rc=%d (%s)", rc, zc_ofi_errstr(rc));
        zc_ofi_close(ep);
        return rc;
    }
    rc = fi_ep_bind(ep->ep, &ep->rx_cq->fid, rx_bind_flags);
    if (rc) {
        zc_ofi_write_err(err, err_len, "fi_ep_bind(rx_cq) rc=%d (%s)", rc, zc_ofi_errstr(rc));
        zc_ofi_close(ep);
        return rc;
    }
    rc = fi_ep_bind(ep->ep, &ep->av->fid, 0);
    if (rc) {
        zc_ofi_write_err(err, err_len, "fi_ep_bind(av) rc=%d (%s)", rc, zc_ofi_errstr(rc));
        zc_ofi_close(ep);
        return rc;
    }
    rc = fi_enable(ep->ep);
    if (rc) {
        zc_ofi_write_err(err, err_len, "fi_enable rc=%d (%s)", rc, zc_ofi_errstr(rc));
        zc_ofi_close(ep);
        return rc;
    }

    ep->max_msg_size_query_rc = -FI_ENOSYS;
    ep->max_rma_size_query_rc = -FI_ENOSYS;
#if ZC_OFI_HAVE_ENDPOINT_LIMIT_OPTIONS
    size_t queried_size = 0;
    size_t queried_size_len = sizeof(queried_size);
    ep->max_msg_size_query_rc =
        fi_getopt(&ep->ep->fid, FI_OPT_ENDPOINT, FI_OPT_MAX_MSG_SIZE,
                  &queried_size, &queried_size_len);
    if (ep->max_msg_size_query_rc == 0 &&
        queried_size_len == sizeof(queried_size) && queried_size != 0) {
        ep->max_msg_size = queried_size;
    }
    if (caps & FI_RMA) {
        queried_size = 0;
        queried_size_len = sizeof(queried_size);
        ep->max_rma_size_query_rc = fi_getopt(
            &ep->ep->fid, FI_OPT_ENDPOINT, FI_OPT_MAX_RMA_SIZE,
            &queried_size, &queried_size_len);
        if (ep->max_rma_size_query_rc == 0 &&
            queried_size_len == sizeof(queried_size)) {
            ep->max_rma_size = queried_size;
        }
    }
#endif

    if (ep->strict_topology && efa_direct &&
        (ep->max_msg_size_query_rc != 0 || ep->max_msg_size == 0 ||
         ((caps & FI_RMA) &&
          (ep->max_rma_size_query_rc != 0 || ep->max_rma_size == 0)))) {
        zc_ofi_write_err(
            err, err_len,
            "strict efa-direct capability query failed max_msg_size=%zu msg_query_rc=%d max_rma_size=%zu rma_query_rc=%d caps=%llu",
            ep->max_msg_size, ep->max_msg_size_query_rc, ep->max_rma_size,
            ep->max_rma_size_query_rc, (unsigned long long)caps);
        zc_ofi_close(ep);
        return -FI_EOPNOTSUPP;
    }

    if (efa_provider) {
#if ZC_OFI_HAVE_EFA_EMULATION_OPTIONS
        bool emulated = false;
        size_t option_len = sizeof(emulated);
        ep->emulated_read_query_rc = fi_getopt(
            &ep->ep->fid, FI_OPT_ENDPOINT, FI_OPT_EFA_EMULATED_READ,
            &emulated, &option_len);
        if (ep->emulated_read_query_rc == 0) {
            ep->emulated_read = emulated;
        }
        emulated = false;
        option_len = sizeof(emulated);
        ep->emulated_write_query_rc = fi_getopt(
            &ep->ep->fid, FI_OPT_ENDPOINT, FI_OPT_EFA_EMULATED_WRITE,
            &emulated, &option_len);
        if (ep->emulated_write_query_rc == 0) {
            ep->emulated_write = emulated;
        }
#else
        ep->emulated_read_query_rc = -FI_ENOSYS;
        ep->emulated_write_query_rc = -FI_ENOSYS;
#endif
        if (ep->strict_topology && (caps & FI_RMA) &&
            (ep->emulated_read_query_rc || ep->emulated_write_query_rc ||
             ep->emulated_read || ep->emulated_write)) {
            zc_ofi_write_err(
                err, err_len,
                "strict OFI topology could not prove device EFA RMA read=%d read_query_rc=%d write=%d write_query_rc=%d; enable device RDMA or use efa-direct",
                ep->emulated_read, ep->emulated_read_query_rc,
                ep->emulated_write, ep->emulated_write_query_rc);
            zc_ofi_close(ep);
            return -FI_EOPNOTSUPP;
        }
    }

    *out = ep;
    return 0;
}

int zc_ofi_open_on_domain(const char *provider, const char *endpoint, const char *node,
                          const char *service, int server, const char *domain_name,
                          struct zc_ofi_endpoint **out, char *err, size_t err_len) {
    return zc_ofi_open_on_domain_caps(provider, endpoint, node, service, server,
                                      domain_name, FI_MSG, FI_SEND, FI_RECV, 0, 0,
                                      out, err, err_len);
}

int zc_ofi_open(const char *provider, const char *endpoint, const char *node,
                const char *service, int server, struct zc_ofi_endpoint **out,
                char *err, size_t err_len) {
    return zc_ofi_open_on_domain(provider, endpoint, node, service, server, NULL, out, err,
                                 err_len);
}

int zc_ofi_open_rma_on_domain(const char *provider, const char *endpoint, const char *node,
                              const char *service, int server, const char *domain_name,
                              struct zc_ofi_endpoint **out, char *err, size_t err_len) {
    return zc_ofi_open_on_domain_caps(provider, endpoint, node, service, server,
                                      domain_name, FI_MSG | FI_RMA, FI_TRANSMIT,
                                      FI_RECV, 0, 0, out, err, err_len);
}

int zc_ofi_open_rma_sized_on_domain(
    const char *provider, const char *endpoint, const char *node,
    const char *service, int server, const char *domain_name,
    size_t read_depth, size_t write_depth, struct zc_ofi_endpoint **out,
    char *err, size_t err_len) {
    if (read_depth == 0 || write_depth == 0) {
        zc_ofi_write_err(err, err_len,
                         "sized OFI RMA endpoint requires nonzero read/write depths");
        return -FI_EINVAL;
    }
    return zc_ofi_open_on_domain_caps(
        provider, endpoint, node, service, server, domain_name,
        FI_MSG | FI_RMA, FI_TRANSMIT, FI_RECV,
        read_depth, write_depth, out, err, err_len);
}

int zc_ofi_open_rma(const char *provider, const char *endpoint, const char *node,
                    const char *service, int server, struct zc_ofi_endpoint **out,
                    char *err, size_t err_len) {
    return zc_ofi_open_rma_on_domain(provider, endpoint, node, service, server, NULL, out,
                                     err, err_len);
}

static struct zc_ofi_op *zc_ofi_find_context_in_ring(
    struct zc_ofi_op_ring *ring, void *context, size_t *slot) {
    if (!ring || !ring->ops || !context) {
        return NULL;
    }
    uintptr_t start = (uintptr_t)ring->ops;
    if (ring->depth > SIZE_MAX / sizeof(*ring->ops)) {
        return NULL;
    }
    uintptr_t span = ring->depth * sizeof(*ring->ops);
    uintptr_t end = start + span;
    uintptr_t value = (uintptr_t)context;
    if (end < start || value < start || value >= end ||
        (value - start) % sizeof(*ring->ops) != 0) {
        return NULL;
    }
    size_t found = (value - start) / sizeof(*ring->ops);
    struct zc_ofi_op *op = &ring->ops[found];
    if ((void *)&op->context != context) {
        return NULL;
    }
    if (slot) {
        *slot = found;
    }
    return op;
}

static struct zc_ofi_op *zc_ofi_find_context(
    struct zc_ofi_endpoint *ep, int receive_cq, void *context,
    struct zc_ofi_op_ring **out_ring, size_t *out_slot) {
    struct zc_ofi_op_ring *rings[3];
    size_t count = 0;
    if (receive_cq) {
        rings[count++] = &ep->recv_ring;
    } else {
        rings[count++] = &ep->send_ring;
        rings[count++] = &ep->read_ring;
        rings[count++] = &ep->write_ring;
    }
    for (size_t i = 0; i < count; i++) {
        size_t slot = 0;
        struct zc_ofi_op *op =
            zc_ofi_find_context_in_ring(rings[i], context, &slot);
        if (op) {
            if (out_ring) {
                *out_ring = rings[i];
            }
            if (out_slot) {
                *out_slot = slot;
            }
            return op;
        }
    }
    return NULL;
}

static int zc_ofi_complete_slot(struct zc_ofi_endpoint *ep,
                                struct zc_ofi_op_ring *ring, size_t slot,
                                size_t len, fi_addr_t source,
                                int completion_rc, int prov_errno) {
    if (!ep || !ring || !ring->ops || slot >= ring->depth) {
        return -FI_EINVAL;
    }
    struct zc_ofi_op *op = &ring->ops[slot];
    if (!op->active || op->completed || ring->provider_inflight == 0) {
        snprintf(ep->err, sizeof(ep->err),
                 "invalid OFI CQ state kind=%u slot=%zu active=%u completed=%u inflight=%zu",
                 (unsigned)op->kind, slot, (unsigned)op->active,
                 (unsigned)op->completed, ring->provider_inflight);
        ep->fatal_rc = -FI_EPROTO;
        return -FI_EPROTO;
    }
    if (!ring->completed_slots || ring->completed_count >= ring->depth) {
        snprintf(ep->err, sizeof(ep->err),
                 "OFI completed-slot FIFO overflow kind=%u slot=%zu count=%zu depth=%zu",
                 (unsigned)op->kind, slot, ring->completed_count,
                 ring->depth);
        ep->fatal_rc = -FI_EOVERFLOW;
        return -FI_EOVERFLOW;
    }
    op->len = len;
    op->src_addr = source;
    op->completion_rc = completion_rc;
    op->prov_errno = prov_errno;
    op->completed = 1;
    ring->completed_slots[zc_ofi_ring_index(
        ring->completed_head, ring->completed_count, ring->depth)] = slot;
    ring->completed_count++;
    ring->provider_inflight--;
    ring->completions++;
    if (completion_rc) {
        ring->errors++;
        ep->fatal_rc = completion_rc;
    } else if (ring == &ep->write_ring &&
               ep->efa_write_high_pps_effective) {
        /* A successful CQE proves that the provider accepted and completed
         * the FI_EFA_WR_HIGH_PPS fi_writemsg variant. */
        ep->efa_write_high_pps_verified = 1;
    }
    return 0;
}

static int zc_ofi_complete_one(struct zc_ofi_endpoint *ep, int receive_cq,
                               void *context, size_t len, fi_addr_t source,
                               int completion_rc, int prov_errno) {
    struct zc_ofi_op_ring *ring = NULL;
    size_t slot = 0;
    struct zc_ofi_op *op =
        zc_ofi_find_context(ep, receive_cq, context, &ring, &slot);
    if (!op) {
        snprintf(ep->err, sizeof(ep->err),
                 "unexpected OFI %s CQ context=%p",
                 receive_cq ? "RX" : "TX", context);
        ep->fatal_rc = -FI_EPROTO;
        return -FI_EPROTO;
    }
    return zc_ofi_complete_slot(ep, ring, slot, len, source, completion_rc,
                                prov_errno);
}

static int zc_ofi_drain_moderated_reads(struct zc_ofi_endpoint *ep) {
    struct zc_ofi_op_ring *ring = &ep->read_ring;
    for (;;) {
        size_t group_count = 0;
        int marker_group = 0;
        if (ring->completion_group_count != 0) {
            group_count = ring->completion_groups[ring->completion_group_head];
            if (group_count == 0 || group_count > ring->posted_count) {
                snprintf(ep->err, sizeof(ep->err),
                         "invalid moderated OFI read group=%zu posted=%zu groups=%zu",
                         group_count, ring->posted_count,
                         ring->completion_group_count);
                ep->fatal_rc = -FI_EPROTO;
                return -FI_EPROTO;
            }
            size_t marker_index = zc_ofi_ring_index(
                ring->posted_head, group_count - 1, ring->depth);
            size_t marker_slot = ring->posted_slots[marker_index];
            if (marker_slot >= ring->depth) {
                snprintf(ep->err, sizeof(ep->err),
                         "invalid moderated OFI marker slot=%zu depth=%zu",
                         marker_slot, ring->depth);
                ep->fatal_rc = -FI_EPROTO;
                return -FI_EPROTO;
            }
            struct zc_ofi_op *marker = &ring->ops[marker_slot];
            if (!marker->completion_requested) {
                snprintf(ep->err, sizeof(ep->err),
                         "moderated OFI group tail is not a marker slot=%zu group=%zu",
                         marker_slot, group_count);
                ep->fatal_rc = -FI_EPROTO;
                return -FI_EPROTO;
            }
            if (!marker->provider_cqe_seen) {
                return 0;
            }
            marker_group = 1;
        }
        if (group_count == 0) {
            if (!ep->rma_read_flush_cqe_seen) {
                return 0;
            }
            group_count = ring->posted_count;
            ep->rma_read_flush_cqe_seen = 0;
            ep->rma_read_flush_inflight = 0;
            if (group_count == 0) {
                return 0;
            }
        }
        if (marker_group) {
            if (ep->rma_read_completion_markers_inflight == 0) {
                return zc_ofi_fail(ep, -FI_EPROTO,
                                   "moderated-read marker count underflow");
            }
            ep->rma_read_completion_markers_inflight--;
            ring->completion_group_head = zc_ofi_ring_index(
                ring->completion_group_head, 1, ring->depth);
            ring->completion_group_count--;
        } else {
            /* A synthetic fence closes the only open partial group. Closed
             * real-marker groups keep markers_inflight nonzero and cannot
             * enter this branch. */
            ring->open_completion_group_count = 0;
        }
        const size_t depth = ring->depth;
        size_t *restrict completed_slots = ring->completed_slots;
        const size_t *restrict posted_slots = ring->posted_slots;
        struct zc_ofi_op *restrict ops = ring->ops;
        if (!completed_slots || ring->completed_count > depth ||
            group_count > ring->provider_inflight ||
            group_count > depth - ring->completed_count) {
            snprintf(ep->err, sizeof(ep->err),
                     "OFI moderated-read group accounting overflow group=%zu inflight=%zu completed=%zu depth=%zu",
                     group_count, ring->provider_inflight,
                     ring->completed_count, depth);
            ep->fatal_rc = -FI_EOVERFLOW;
            return -FI_EOVERFLOW;
        }
        size_t posted_head = ring->posted_head;
        size_t completed_tail = zc_ofi_ring_index(
            ring->completed_head, ring->completed_count, depth);
        size_t remaining = group_count;
        while (remaining) {
            size_t span = depth - posted_head;
            if (span > depth - completed_tail) {
                span = depth - completed_tail;
            }
            if (span > remaining) {
                span = remaining;
            }
            for (size_t i = 0; i < span; i++) {
                size_t slot = posted_slots[posted_head + i];
                if (slot >= depth) {
                    snprintf(ep->err, sizeof(ep->err),
                             "invalid moderated OFI read slot=%zu depth=%zu",
                             slot, depth);
                    ep->fatal_rc = -FI_EPROTO;
                    return -FI_EPROTO;
                }
                struct zc_ofi_op *op = &ops[slot];
                if (!op->active || op->completed) {
                    snprintf(ep->err, sizeof(ep->err),
                             "invalid moderated OFI read state slot=%zu active=%u completed=%u inflight=%zu",
                             slot, (unsigned)op->active,
                             (unsigned)op->completed,
                             ring->provider_inflight);
                    ep->fatal_rc = -FI_EPROTO;
                    return -FI_EPROTO;
                }
                /* The fenced marker proves the successful unsignaled prefix.
                 * Publish per-slot ownership here, then account the entire
                 * group once below rather than dirtying four ring counters per
                 * 4 KiB read. */
                op->completed = 1;
                completed_slots[completed_tail + i] = slot;
            }
            posted_head += span;
            completed_tail += span;
            remaining -= span;
            if (posted_head == depth) {
                posted_head = 0;
            }
            if (completed_tail == depth) {
                completed_tail = 0;
            }
        }
        ring->posted_head = posted_head;
        ring->posted_count -= group_count;
        ring->completed_count += group_count;
        ring->provider_inflight -= group_count;
        ring->completions += group_count;
    }
}

static int zc_ofi_complete_op(struct zc_ofi_endpoint *ep, int receive_cq,
                              void *context, size_t len, fi_addr_t source,
                              int completion_rc, int prov_errno) {
    if (!receive_cq && context == &ep->rma_read_flush_context) {
        if (completion_rc) {
            ep->fatal_rc = completion_rc;
            snprintf(ep->err, sizeof(ep->err),
                     "moderated OFI read flush CQ failed rc=%d prov_errno=%d",
                     completion_rc, prov_errno);
            return completion_rc;
        }
        ep->rma_read_flush_cqe_seen = 1;
        return zc_ofi_drain_moderated_reads(ep);
    }
    if (receive_cq || completion_rc || ep->rma_read_completion_stride <= 1) {
        return zc_ofi_complete_one(ep, receive_cq, context, len, source,
                                   completion_rc, prov_errno);
    }

    struct zc_ofi_op_ring *ring = &ep->read_ring;
    size_t marker_slot = 0;
    struct zc_ofi_op *marker =
        zc_ofi_find_context_in_ring(ring, context, &marker_slot);
    if (!marker) {
        /* The TX CQ is shared with SEND and WRITE. Those much less frequent
         * completions retain the fully generic dispatcher, while successful
         * moderated read markers avoid probing the SEND ring first. */
        return zc_ofi_complete_one(ep, 0, context, len, source, 0, 0);
    }
    if (!marker->completion_requested) {
        snprintf(ep->err, sizeof(ep->err),
                 "unexpected moderated OFI read CQ context=%p slot=%zu",
                 context, marker_slot);
        ep->fatal_rc = -FI_EPROTO;
        return -FI_EPROTO;
    }
    if (!ring->posted_slots || ring->posted_count == 0) {
        snprintf(ep->err, sizeof(ep->err),
                 "empty moderated OFI read posting FIFO at marker slot=%zu",
                 marker_slot);
        ep->fatal_rc = -FI_EPROTO;
        return -FI_EPROTO;
    }

    marker->provider_cqe_seen = 1;
    marker->len = len;
    return zc_ofi_drain_moderated_reads(ep);
}

static int zc_ofi_dispatch_cq(struct zc_ofi_endpoint *ep, int receive_cq) {
    if (!ep) {
        return -FI_EINVAL;
    }
    struct zc_ofi_cq_state *state =
        receive_cq ? &ep->rx_cq_state : &ep->tx_cq_state;
    struct fid_cq *cq = receive_cq ? ep->rx_cq : ep->tx_cq;
    if (!cq || !state->entries || state->batch_capacity == 0) {
        return -FI_EINVAL;
    }
    /* fi_cq_read{,from} initializes exactly the positive return count.  Do
     * not dirty the entire batch arrays before every nonblocking poll: empty
     * EFA CQ polls dominate the high-IOPS path, and unused/stale tail entries
     * are never inspected below. */
    ssize_t rc = receive_cq
                     ? fi_cq_readfrom(cq, state->entries, state->batch_capacity,
                                      state->sources)
                     : fi_cq_read(cq, state->entries, state->batch_capacity);
    if (rc > 0) {
        state->nonempty_polls++;
        state->entries_read += (uint64_t)rc;
        for (size_t i = 0; i < (size_t)rc; i++) {
            int complete_rc = zc_ofi_complete_op(
                ep, receive_cq, state->entries[i].op_context,
                state->entries[i].len,
                receive_cq ? state->sources[i] : FI_ADDR_UNSPEC, 0, 0);
            if (complete_rc) {
                return complete_rc;
            }
        }
        return (int)rc;
    }
    if (rc == -FI_EAGAIN) {
        return 0;
    }
    if (rc == -FI_EAVAIL) {
        struct fi_cq_err_entry err_entry;
        memset(&err_entry, 0, sizeof(err_entry));
        ssize_t erc = fi_cq_readerr(cq, &err_entry, 0);
        if (erc > 0) {
            int completion_rc = err_entry.err ? -err_entry.err : -FI_EIO;
            fi_addr_t source = FI_ADDR_UNSPEC;
            char provider_detail[160] = {0};
            const char *provider_error = fi_cq_strerror(
                cq, err_entry.prov_errno, err_entry.err_data,
                provider_detail, sizeof(provider_detail));
            if (!provider_error || provider_error[0] == '\0') {
                provider_error = "unavailable";
            }
#if FI_MAJOR_VERSION >= 2
            source = err_entry.src_addr;
#endif
            state->errors++;
            int complete_rc = zc_ofi_complete_op(
                ep, receive_cq, err_entry.op_context, err_entry.len, source,
                completion_rc, err_entry.prov_errno);
            char dispatch_error[160] = {0};
            if (complete_rc) {
                /* The CQE is already consumed even when its context is
                 * corrupt or stale.  Keep that protocol failure fatal, but
                 * do not let it hide the provider's original CQ diagnosis. */
                snprintf(dispatch_error, sizeof(dispatch_error), "%s", ep->err);
            }
#if FI_MAJOR_VERSION >= 2
            if (complete_rc) {
                snprintf(ep->err, sizeof(ep->err),
                         "OFI %s CQ error err=%d prov_errno=%d provider_error=%s len=%zu src=%llu context=%p dispatch_error=%s",
                         receive_cq ? "RX" : "TX", err_entry.err,
                         err_entry.prov_errno, provider_error, err_entry.len,
                         (unsigned long long)source, err_entry.op_context,
                         dispatch_error);
            } else {
                snprintf(ep->err, sizeof(ep->err),
                         "OFI %s CQ error err=%d prov_errno=%d provider_error=%s len=%zu src=%llu context=%p",
                         receive_cq ? "RX" : "TX", err_entry.err,
                         err_entry.prov_errno, provider_error, err_entry.len,
                         (unsigned long long)source, err_entry.op_context);
            }
#else
            if (complete_rc) {
                snprintf(ep->err, sizeof(ep->err),
                         "OFI %s CQ error err=%d prov_errno=%d provider_error=%s len=%zu context=%p dispatch_error=%s",
                         receive_cq ? "RX" : "TX", err_entry.err,
                         err_entry.prov_errno, provider_error, err_entry.len,
                         err_entry.op_context, dispatch_error);
            } else {
                snprintf(ep->err, sizeof(ep->err),
                         "OFI %s CQ error err=%d prov_errno=%d provider_error=%s len=%zu context=%p",
                         receive_cq ? "RX" : "TX", err_entry.err,
                         err_entry.prov_errno, provider_error, err_entry.len,
                         err_entry.op_context);
            }
#endif
            return complete_rc ? complete_rc : completion_rc;
        }
        return zc_ofi_fail(ep, (int)erc,
                           receive_cq ? "fi_cq_readerr(rx)" : "fi_cq_readerr(tx)");
    }
    return zc_ofi_fail(ep, (int)rc,
                       receive_cq ? "fi_cq_readfrom(rx)" : "fi_cq_read(tx)");
}

static int zc_ofi_dispatch_cq_counted(struct zc_ofi_endpoint *ep,
                                      int receive_cq) {
    struct zc_ofi_cq_state *state =
        receive_cq ? &ep->rx_cq_state : &ep->tx_cq_state;
    state->polls++;
    return zc_ofi_dispatch_cq(ep, receive_cq);
}

static struct zc_ofi_op *zc_ofi_prepare_slot(struct zc_ofi_endpoint *ep,
                                              struct zc_ofi_op_ring *ring,
                                              size_t slot,
                                              uint64_t user_data) {
    if (!ep || !ring || !ring->ops || slot >= ring->depth || ep->fatal_rc) {
        return NULL;
    }
    struct zc_ofi_op *op = &ring->ops[slot];
    if (op->active) {
        snprintf(ep->err, sizeof(ep->err),
                 "OFI kind=%u slot=%zu is already active",
                 (unsigned)op->kind, slot);
        return NULL;
    }
    uint8_t kind = op->kind;
    memset(op, 0, sizeof(*op));
    op->kind = kind;
    op->src_addr = FI_ADDR_UNSPEC;
    op->user_data = user_data;
    op->active = 1;
    ring->active++;
    if (ring->active > ring->peak_active) {
        ring->peak_active = ring->active;
    }
    return op;
}

static inline struct zc_ofi_op *
zc_ofi_prepare_read_slot(struct zc_ofi_endpoint *ep, size_t slot,
                         uint64_t user_data) {
    struct zc_ofi_op_ring *ring = &ep->read_ring;
    if (ep->fatal_rc) {
        return NULL;
    }
    struct zc_ofi_op *op = &ring->ops[slot];
    if (op->active) {
        snprintf(ep->err, sizeof(ep->err),
                 "OFI kind=%u slot=%zu is already active",
                 (unsigned)op->kind, slot);
        return NULL;
    }
    /* Providers own fi_context2 between post and CQE, so reset all of it.
     * The read path does not consume stale length/source diagnostics and sets
     * length plus completion policy immediately before posting; clear only
     * the remaining lifecycle fields instead of the full 104-byte record. */
    memset(&op->context, 0, sizeof(op->context));
    op->user_data = user_data;
    op->src_addr = FI_ADDR_UNSPEC;
    op->completion_rc = 0;
    op->prov_errno = 0;
    op->completed = 0;
    op->completion_requested = 0;
    op->provider_cqe_seen = 0;
    op->active = 1;
    ring->active++;
    if (ring->active > ring->peak_active) {
        ring->peak_active = ring->active;
    }
    return op;
}

static void zc_ofi_release_slot(struct zc_ofi_op_ring *ring, size_t slot) {
    struct zc_ofi_op *op = &ring->ops[slot];
    /* zc_ofi_prepare_slot() fully clears the record immediately before the
     * provider can observe it again.  Clearing the same cache line here as
     * well doubled metadata stores on every completion.  `active` is the
     * sole free-slot ownership bit; leave diagnostic fields intact until the
     * next preparation pass. */
    op->active = 0;
    if (ring->active > 0) {
        ring->active--;
    }
}

static int zc_ofi_find_free_slot(struct zc_ofi_op_ring *ring, size_t *out_slot) {
    if (!ring || !ring->ops || !out_slot || ring->active >= ring->depth) {
        return -FI_EAGAIN;
    }
    for (size_t i = 0; i < ring->depth; i++) {
        size_t slot = zc_ofi_ring_index(ring->next_slot, i, ring->depth);
        if (!ring->ops[slot].active) {
            ring->next_slot = zc_ofi_ring_index(slot, 1, ring->depth);
            *out_slot = slot;
            return 0;
        }
    }
    return -FI_EAGAIN;
}

static int zc_ofi_reap_ring(struct zc_ofi_op_ring *ring, size_t *out_slots,
                            uint64_t *out_user_data, size_t *out_lengths,
                            fi_addr_t *out_sources, size_t capacity,
                            size_t *out_count) {
    if (!ring || !out_count || capacity == 0) {
        return -FI_EINVAL;
    }
    *out_count = 0;
    while (ring->completed_count && *out_count < capacity) {
        size_t slot = ring->completed_slots[ring->completed_head];
        ring->completed_head = zc_ofi_ring_index(ring->completed_head, 1,
                                                  ring->depth);
        ring->completed_count--;
        struct zc_ofi_op *op = &ring->ops[slot];
        if (!op->active || !op->completed) {
            return -FI_EPROTO;
        }
        size_t index = *out_count;
        if (out_slots) {
            out_slots[index] = slot;
        }
        if (out_user_data) {
            out_user_data[index] = op->user_data;
        }
        if (out_lengths) {
            out_lengths[index] = op->len;
        }
        if (out_sources) {
            out_sources[index] = op->src_addr;
        }
        int completion_rc = op->completion_rc;
        zc_ofi_release_slot(ring, slot);
        ring->reap_cursor = zc_ofi_ring_index(slot, 1, ring->depth);
        (*out_count)++;
        if (completion_rc) {
            return completion_rc;
        }
    }
    return 0;
}

static int zc_ofi_remove_completed_slot(struct zc_ofi_op_ring *ring,
                                        size_t slot) {
    if (!ring || !ring->completed_slots || ring->completed_count == 0) {
        return -FI_EPROTO;
    }
    for (size_t i = 0; i < ring->completed_count; i++) {
        size_t index = zc_ofi_ring_index(ring->completed_head, i,
                                         ring->depth);
        if (ring->completed_slots[index] != slot) {
            continue;
        }
        for (size_t j = i + 1; j < ring->completed_count; j++) {
            size_t from = zc_ofi_ring_index(ring->completed_head, j,
                                            ring->depth);
            size_t to = zc_ofi_ring_index(ring->completed_head, j - 1,
                                          ring->depth);
            ring->completed_slots[to] = ring->completed_slots[from];
        }
        ring->completed_count--;
        return 0;
    }
    return -FI_EPROTO;
}

static int zc_ofi_poll_ring(struct zc_ofi_endpoint *ep,
                            struct zc_ofi_op_ring *ring, int receive_cq,
                            size_t *out_slots, uint64_t *out_user_data,
                            size_t *out_lengths, fi_addr_t *out_sources,
                            size_t capacity, size_t *out_count, int wait,
                            int timeout_ms) {
    if (!ep || !ring || !out_count || capacity == 0) {
        return -FI_EINVAL;
    }
    if (ep->fatal_rc) {
        return ep->fatal_rc;
    }
    uint64_t start = zc_ofi_now_ms();
    uint64_t spins = 0;
    uint64_t cq_polls = 0;
    struct zc_ofi_cq_state *state =
        receive_cq ? &ep->rx_cq_state : &ep->tx_cq_state;
    for (;;) {
        size_t reaped = 0;
        int rc = zc_ofi_reap_ring(ring, out_slots, out_user_data, out_lengths,
                                  out_sources, capacity, &reaped);
        *out_count = reaped;
        if (rc || reaped > 0 || ring->provider_inflight == 0 || !wait) {
            if (reaped > 0 || rc || ring->provider_inflight == 0) {
                state->polls += cq_polls;
                return rc;
            }
        }
        cq_polls++;
        int dispatched = zc_ofi_dispatch_cq(ep, receive_cq);
        if (dispatched < 0) {
            state->polls += cq_polls;
            return dispatched;
        }
        if (dispatched > 0) {
            continue;
        }
        if (!wait) {
            state->polls += cq_polls;
            return 0;
        }
        if (zc_ofi_poll_timed_out(ep, spins, start, timeout_ms)) {
            snprintf(ep->err, sizeof(ep->err),
                     "OFI %s CQ wait timed out after %d ms kind=%u inflight=%zu",
                     receive_cq ? "RX" : "TX", timeout_ms,
                     ring->ops ? (unsigned)ring->ops[0].kind : 0,
                     ring->provider_inflight);
            state->polls += cq_polls;
            return -ETIMEDOUT;
        }
        if (ep->cq_sleep_ns != 0) {
            state->sleeps++;
        }
        zc_ofi_wait_after_eagain(ep, &spins);
    }
}

static int zc_ofi_wait_slot(struct zc_ofi_endpoint *ep,
                            struct zc_ofi_op_ring *ring, int receive_cq,
                            size_t slot, size_t *out_len,
                            fi_addr_t *out_source, int timeout_ms) {
    if (!ep || !ring || slot >= ring->depth || !ring->ops[slot].active) {
        return -FI_EINVAL;
    }
    if (ep->fatal_rc) {
        return ep->fatal_rc;
    }
    uint64_t start = zc_ofi_now_ms();
    uint64_t spins = 0;
    for (;;) {
        struct zc_ofi_op *op = &ring->ops[slot];
        if (op->completed) {
            int rc = op->completion_rc;
            if (out_len) {
                *out_len = op->len;
            }
            if (out_source) {
                *out_source = op->src_addr;
            }
            int remove_rc = zc_ofi_remove_completed_slot(ring, slot);
            if (remove_rc) {
                snprintf(ep->err, sizeof(ep->err),
                         "OFI completed slot=%zu missing from completion FIFO",
                         slot);
                ep->fatal_rc = remove_rc;
                return remove_rc;
            }
            zc_ofi_release_slot(ring, slot);
            return rc;
        }
        int dispatched = zc_ofi_dispatch_cq_counted(ep, receive_cq);
        if (dispatched < 0) {
            return dispatched;
        }
        if (dispatched > 0) {
            continue;
        }
        if (zc_ofi_poll_timed_out(ep, spins, start, timeout_ms)) {
            snprintf(ep->err, sizeof(ep->err),
                     "OFI %s CQ wait timed out after %d ms kind=%u slot=%zu",
                     receive_cq ? "RX" : "TX", timeout_ms,
                     (unsigned)op->kind, slot);
            return -ETIMEDOUT;
        }
        struct zc_ofi_cq_state *state =
            receive_cq ? &ep->rx_cq_state : &ep->tx_cq_state;
        if (ep->cq_sleep_ns != 0) {
            state->sleeps++;
        }
        zc_ofi_wait_after_eagain(ep, &spins);
    }
}

static int zc_ofi_register_cached(struct zc_ofi_endpoint *ep, const void *buf,
                                  size_t len, uint64_t access,
                                  struct zc_ofi_mr_table *table, void **desc) {
    if (!ep || !buf || len == 0 || !table || !desc) {
        return -FI_EINVAL;
    }
    *desc = NULL;
    if (!ep->mr_local) {
        return 0;
    }
    table->lookups++;
    uintptr_t requested_start = (uintptr_t)buf;
    uintptr_t requested_end = requested_start + len;
    if (requested_end < requested_start) {
        snprintf(ep->err, sizeof(ep->err), "OFI MR lookup range overflow");
        return -FI_EINVAL;
    }
    /* The registered shared arena supplies essentially every data-plane post.
     * Check its last successful descriptor first instead of re-scanning the
     * startup/control registrations on every 4 KiB operation. */
    if (table->hot_index < table->count) {
        struct zc_ofi_mr_arena *arena = &table->arenas[table->hot_index];
        uintptr_t arena_start = (uintptr_t)arena->buf;
        uintptr_t arena_end = arena_start + arena->len;
        if (arena_end >= arena_start && arena->access == access &&
            requested_start >= arena_start && requested_end <= arena_end) {
            table->lookup_hits++;
            *desc = arena->desc;
            return 0;
        }
    }
    for (size_t i = 0; i < table->count; i++) {
        if (i == table->hot_index) {
            continue;
        }
        struct zc_ofi_mr_arena *arena = &table->arenas[i];
        uintptr_t arena_start = (uintptr_t)arena->buf;
        uintptr_t arena_end = arena_start + arena->len;
        if (arena_end >= arena_start && arena->access == access &&
            requested_start >= arena_start && requested_end <= arena_end) {
            table->lookup_hits++;
            table->hot_index = i;
            *desc = arena->desc;
            return 0;
        }
    }
    if (table->posts_started) {
        table->hot_registration_attempts++;
        if (ep->strict_topology) {
            snprintf(ep->err, sizeof(ep->err),
                     "strict OFI topology rejected hot-path MR registration access=%llu len=%zu registrations=%zu capacity=%zu",
                     (unsigned long long)access, len, table->count,
                     table->capacity);
            return -FI_EBUSY;
        }
    }
    if (table->count >= table->capacity) {
        snprintf(ep->err, sizeof(ep->err),
                 "OFI MR arena table full access=%llu count=%zu capacity=%zu",
                 (unsigned long long)access, table->count, table->capacity);
        return -FI_ENOSPC;
    }
    struct fid_mr *mr = NULL;
    int rc = fi_mr_reg(ep->domain, buf, len, access, 0, 0, 0, &mr, NULL);
    if (rc) {
        return zc_ofi_fail(ep, rc, "fi_mr_reg");
    }
    size_t arena_index = table->count++;
    struct zc_ofi_mr_arena *arena = &table->arenas[arena_index];
    arena->buf = buf;
    arena->len = len;
    arena->access = access;
    arena->mr = mr;
    arena->desc = fi_mr_desc(mr);
    table->hot_index = arena_index;
    table->registrations++;
    *desc = arena->desc;
    return 0;
}

int zc_ofi_register_send_buffer(struct zc_ofi_endpoint *ep, const void *buf,
                                size_t len) {
    if (!ep) {
        return -FI_EINVAL;
    }
    void *desc = NULL;
    return zc_ofi_register_cached(ep, buf, len, FI_SEND, &ep->send_mrs, &desc);
}

int zc_ofi_register_recv_buffer(struct zc_ofi_endpoint *ep, void *buf,
                                size_t len) {
    if (!ep) {
        return -FI_EINVAL;
    }
    void *desc = NULL;
    return zc_ofi_register_cached(ep, buf, len, FI_RECV, &ep->recv_mrs, &desc);
}

int zc_ofi_rma_register_write_buffer(struct zc_ofi_endpoint *ep,
                                     const void *buf, size_t len) {
    if (!ep) {
        return -FI_EINVAL;
    }
    void *desc = NULL;
    return zc_ofi_register_cached(ep, buf, len, FI_WRITE, &ep->write_mrs, &desc);
}

int zc_ofi_rma_register_read_buffer(struct zc_ofi_endpoint *ep, void *buf, size_t len) {
    if (!ep || !buf || len == 0) {
        return -FI_EINVAL;
    }
    void *desc = NULL;
    int rc = zc_ofi_register_cached(ep, buf, len, FI_READ,
                                    &ep->read_mrs, &desc);
    if (!rc) {
        ep->rma_read_arena_start = (uintptr_t)buf;
        ep->rma_read_arena_end = (uintptr_t)buf + len;
        ep->rma_read_arena_desc = desc;
    }
    return rc;
}

static inline int zc_ofi_rma_read_desc(struct zc_ofi_endpoint *ep,
                                       const void *buf, size_t len,
                                       void **desc) {
    *desc = NULL;
    if (!ep->mr_local) {
        return 0;
    }
    uintptr_t requested_start = (uintptr_t)buf;
    uintptr_t requested_end = requested_start + len;
    if (requested_end >= requested_start &&
        requested_start >= ep->rma_read_arena_start &&
        requested_end <= ep->rma_read_arena_end) {
        *desc = ep->rma_read_arena_desc;
        return 0;
    }
    return zc_ofi_register_cached(ep, buf, len, FI_READ,
                                  &ep->read_mrs, desc);
}

int zc_ofi_rma_read_queue_init(struct zc_ofi_endpoint *ep, size_t depth) {
    if (!ep || depth == 0 || depth > 65536) {
        return -FI_EINVAL;
    }
    if (ep->read_ring.active != 0) {
        snprintf(ep->err, sizeof(ep->err),
                 "cannot resize OFI RMA read queue with active=%zu inflight=%zu",
                 ep->read_ring.active, ep->read_ring.provider_inflight);
        return -FI_EBUSY;
    }
    if (ep->send_ring.depth > SIZE_MAX - depth ||
        ep->send_ring.depth + depth > SIZE_MAX - ep->write_ring.depth ||
        ep->send_ring.depth + depth + ep->write_ring.depth >
            SIZE_MAX - ep->cq_headroom) {
        snprintf(ep->err, sizeof(ep->err),
                 "OFI RMA read queue/CQ size arithmetic overflow");
        return -FI_EINVAL;
    }
    size_t queue_entries = ep->send_ring.depth + depth + ep->write_ring.depth;
    size_t required = queue_entries + ep->cq_headroom;
    if (queue_entries > ep->tx_cq_state.configured_size ||
        (ep->strict_topology && required > ep->tx_cq_state.configured_size)) {
        snprintf(ep->err, sizeof(ep->err),
                 "OFI TX CQ size=%zu is below required=%zu for RMA read depth=%zu; set URING_PLAY_OFI_TX_CQ_SIZE before open",
                 ep->tx_cq_state.configured_size, required, depth);
        return -FI_ENOSPC;
    }
    if (ep->read_ring.depth == depth) {
        ep->tx_cq_required = required;
        if (ep->rma_read_completion_stride > 1 &&
            !ep->rma_read_flush_desc) {
            int register_rc = zc_ofi_register_cached(
                ep, &ep->rma_read_flush_byte, 1, FI_READ,
                &ep->read_mrs, &ep->rma_read_flush_desc);
            if (register_rc) {
                return register_rc;
            }
        }
        return 0;
    }
    int rc = zc_ofi_init_ring(&ep->read_ring, depth, ZC_OFI_OP_READ,
                              ep->err, sizeof(ep->err));
    if (!rc) {
        ep->tx_cq_required = required;
        if (ep->rma_read_completion_stride > 1) {
            rc = zc_ofi_register_cached(
                ep, &ep->rma_read_flush_byte, 1, FI_READ,
                &ep->read_mrs, &ep->rma_read_flush_desc);
        }
    }
    return rc;
}

static int zc_ofi_read_post_call(struct zc_ofi_endpoint *ep, void *buf,
                                 size_t len, void *desc,
                                 uint64_t remote_addr, uint64_t remote_key,
                                 void *context, int completion_requested) {
    /* FI_SELECTIVE_COMPLETION makes the flag-less fi_read() entry point
     * unsignaled. Use that provider fast path for the ordinary prefix and
     * pay to construct fi_msg_rma only for the fenced FI_COMPLETION marker.
     * Asynchronous failures still generate CQ errors by libfabric contract. */
    if (!ep->selective_completion ||
        (!completion_requested && !ep->rma_read_more_enabled)) {
        return (int)fi_read(ep->ep, buf, len, desc, ep->peer_addr,
                            remote_addr, remote_key, context);
    }
    struct iovec local_iov = {
        .iov_base = buf,
        .iov_len = len,
    };
    struct fi_rma_iov remote_iov = {
        .addr = remote_addr,
        .len = len,
        .key = remote_key,
    };
    void *descriptors[1] = {desc};
    struct fi_msg_rma message = {
        .msg_iov = &local_iov,
        .desc = descriptors,
        .iov_count = 1,
        .addr = ep->peer_addr,
        .rma_iov = &remote_iov,
        .rma_iov_count = 1,
        .context = context,
        .data = 0,
    };
    uint64_t flags = completion_requested ? FI_COMPLETION : FI_MORE;
    if (ep->rma_read_completion_stride > 1 && completion_requested) {
        flags |= FI_FENCE;
    }
    return (int)fi_readmsg(ep->ep, &message, flags);
}

int zc_ofi_rma_read_post(struct zc_ofi_endpoint *ep, void *buf, size_t len,
                         uint64_t remote_addr, uint64_t remote_key, size_t slot,
                         uint64_t user_data, int force_completion) {
    if (!ep || !buf || len == 0 || !ep->read_ring.ops ||
        slot >= ep->read_ring.depth) {
        return -FI_EINVAL;
    }
    if (ep->peer_addr == FI_ADDR_UNSPEC) {
        snprintf(ep->err, sizeof(ep->err), "OFI peer address is not set");
        return -FI_EINVAL;
    }
    if (ep->max_rma_size && len > ep->max_rma_size) {
        snprintf(ep->err, sizeof(ep->err),
                 "OFI RMA read len=%zu exceeds max_rma_size=%zu",
                 len, ep->max_rma_size);
        return -EMSGSIZE;
    }
    void *desc = NULL;
    int rc = zc_ofi_rma_read_desc(ep, buf, len, &desc);
    if (rc) {
        return rc;
    }
    struct zc_ofi_op *op = zc_ofi_prepare_read_slot(ep, slot, user_data);
    if (!op) {
        return ep->fatal_rc ? ep->fatal_rc : -FI_EBUSY;
    }
    /* A full posting window cannot admit the next periodic marker until some
     * reads retire.  Make the last real read in that window the fenced marker
     * instead of forcing the polling side to issue a synthetic one-byte RMA
     * read merely to close a partial stride.  Besides removing that provider
     * operation, this guarantees progress when the configured stride exceeds
     * the queue depth. */
    int moderated = ep->rma_read_completion_stride > 1;
    int periodic_marker = moderated && ep->rma_read_completion_remaining == 1;
    int full_window_marker = moderated &&
                             ep->read_ring.posted_count + 1 ==
                                 ep->read_ring.depth;
    int forced_marker = moderated && force_completion;
    int completion_requested = !moderated ||
                               periodic_marker || full_window_marker ||
                               forced_marker;
    if (moderated &&
        (!ep->read_ring.posted_slots || !ep->read_ring.completion_groups ||
         ep->read_ring.posted_count >= ep->read_ring.depth ||
         (completion_requested &&
          ep->read_ring.completion_group_count >= ep->read_ring.depth))) {
        snprintf(ep->err, sizeof(ep->err),
                 "OFI moderated read FIFO overflow slot=%zu posted=%zu groups=%zu depth=%zu",
                 slot, ep->read_ring.posted_count,
                 ep->read_ring.completion_group_count,
                 ep->read_ring.depth);
        ep->fatal_rc = -FI_EOVERFLOW;
        zc_ofi_release_slot(&ep->read_ring, slot);
        return -FI_EOVERFLOW;
    }
    op->len = len;
    op->completion_requested = (uint8_t)completion_requested;
    rc = zc_ofi_read_post_call(ep, buf, len, desc, remote_addr, remote_key,
                               &op->context, completion_requested);
    if (rc) {
        zc_ofi_release_slot(&ep->read_ring, slot);
        if (rc == -FI_EAGAIN) {
            ep->read_ring.post_eagain++;
            ep->read_ring.post_retries++;
            return rc;
        }
        return zc_ofi_fail(ep, rc, "fi_read(async)");
    }
    if (!ep->read_mrs.posts_started) {
        ep->read_mrs.posts_started = 1;
    }
    if (moderated &&
        ep->rma_read_completion_remaining ==
            ep->rma_read_completion_stride) {
        /* A synthetic fence, if a partial group ever needs the liveness
         * fallback, may read any valid byte from that group. Preserve its
         * address once at group open instead of dirtying two endpoint words
         * for every 4 KiB operation. */
        ep->rma_read_last_remote_addr = remote_addr;
        ep->rma_read_last_remote_key = remote_key;
    }
    if (moderated) {
        ep->read_ring.posted_slots[zc_ofi_ring_index(
            ep->read_ring.posted_head, ep->read_ring.posted_count,
            ep->read_ring.depth)] = slot;
        ep->read_ring.posted_count++;
        ep->read_ring.open_completion_group_count++;
        if (completion_requested) {
            size_t group_tail = zc_ofi_ring_index(
                ep->read_ring.completion_group_head,
                ep->read_ring.completion_group_count,
                ep->read_ring.depth);
            ep->read_ring.completion_groups[group_tail] =
                ep->read_ring.open_completion_group_count;
            ep->read_ring.completion_group_count++;
            ep->read_ring.open_completion_group_count = 0;
        }
    }
    ep->read_ring.provider_inflight++;
    ep->read_ring.posts++;
    if (moderated && completion_requested) {
        /* Every fenced real read closes the outstanding unsignaled prefix.
         * Restart the periodic budget at that boundary.  Besides matching
         * the actual completion group, this avoids a runtime division on
         * every RMA read. */
        ep->rma_read_completion_remaining = ep->rma_read_completion_stride;
        ep->rma_read_completion_markers_inflight++;
        ep->rma_read_marker_posts++;
        ep->rma_read_periodic_markers += periodic_marker ? 1 : 0;
        ep->rma_read_full_window_markers += full_window_marker ? 1 : 0;
        ep->rma_read_forced_markers += forced_marker ? 1 : 0;
    } else if (moderated) {
        ep->rma_read_completion_remaining--;
    }
    return 0;
}

int zc_ofi_rma_read_poll(struct zc_ofi_endpoint *ep, size_t *out_slots,
                         uint64_t *out_user_data, size_t capacity,
                         size_t *out_count, int wait, int timeout_ms) {
    if (!ep || !out_slots || !out_user_data || !out_count || capacity == 0 ||
        !ep->read_ring.ops || capacity > ep->read_ring.depth) {
        return -FI_EINVAL;
    }
    if (ep->rma_read_completion_stride > 1 &&
        ep->rma_read_completion_remaining != ep->rma_read_completion_stride && wait &&
        ep->rma_read_completion_markers_inflight == 0 &&
        !ep->rma_read_flush_inflight) {
        struct iovec local_iov = {
            .iov_base = &ep->rma_read_flush_byte,
            .iov_len = 1,
        };
        struct fi_rma_iov remote_iov = {
            .addr = ep->rma_read_last_remote_addr,
            .len = 1,
            .key = ep->rma_read_last_remote_key,
        };
        void *descriptors[1] = {ep->rma_read_flush_desc};
        struct fi_msg_rma message = {
            .msg_iov = &local_iov,
            .desc = descriptors,
            .iov_count = 1,
            .addr = ep->peer_addr,
            .rma_iov = &remote_iov,
            .rma_iov_count = 1,
            .context = &ep->rma_read_flush_context,
            .data = 0,
        };
        int flush_rc =
            (int)fi_readmsg(ep->ep, &message, FI_FENCE | FI_COMPLETION);
        if (flush_rc) {
            return zc_ofi_fail(ep, flush_rc, "fi_readmsg(partial-read-fence)");
        }
        ep->rma_read_flush_inflight = 1;
        ep->rma_read_flush_posts++;
    }
    if (ep->rma_read_completion_stride > 1 &&
        ep->rma_read_completion_remaining != ep->rma_read_completion_stride &&
        ep->rma_read_completion_markers_inflight == 0 &&
        !ep->rma_read_flush_inflight) {
        /* The next fenced marker has not been posted yet.  A blocking poll
         * here would prevent the caller from admitting enough descriptors to
         * close the group. */
        wait = 0;
    }
    return zc_ofi_poll_ring(ep, &ep->read_ring, 0, out_slots,
                            out_user_data, NULL, NULL, capacity, out_count,
                            wait, timeout_ms);
}

int zc_ofi_rma_register_target(struct zc_ofi_endpoint *ep, void *buf, size_t len,
                               uint64_t *addr, uint64_t *key) {
    if (!ep || !buf || len == 0 || !addr || !key) {
        return -FI_EINVAL;
    }
    if (ep->rma_target_mr && ep->rma_target_buf == buf &&
        ep->rma_target_len == len) {
        *addr = ep->mr_virt_addr ? (uint64_t)(uintptr_t)buf : 0;
        *key = fi_mr_key(ep->rma_target_mr);
        return 0;
    }
    if (ep->rma_target_mr) {
        ep->rma_target_hot_registration_attempts++;
        if (ep->strict_topology) {
            snprintf(ep->err, sizeof(ep->err),
                     "strict OFI topology rejected RMA target MR replacement old_len=%zu new_len=%zu registrations=%llu",
                     ep->rma_target_len, len,
                     (unsigned long long)ep->rma_target_registrations);
            return -FI_EBUSY;
        }
        zc_ofi_close_fid(&ep->rma_target_mr->fid);
        ep->rma_target_closes++;
        ep->rma_target_mr = NULL;
        ep->rma_target_buf = NULL;
        ep->rma_target_len = 0;
    }
    struct fid_mr *mr = NULL;
    int rc = fi_mr_reg(ep->domain, buf, len, FI_REMOTE_READ | FI_REMOTE_WRITE,
                       0, 0, 0, &mr, NULL);
    if (rc) {
        return zc_ofi_fail(ep, rc, "fi_mr_reg(rma_target)");
    }
    ep->rma_target_mr = mr;
    ep->rma_target_buf = buf;
    ep->rma_target_len = len;
    ep->rma_target_registrations++;
    *addr = ep->mr_virt_addr ? (uint64_t)(uintptr_t)buf : 0;
    *key = fi_mr_key(mr);
    return 0;
}

static int zc_ofi_send_post_call(struct zc_ofi_endpoint *ep, const void *buf,
                                 size_t len, void *desc,
                                 fi_addr_t destination, void *context) {
    if (!ep->selective_completion) {
        return (int)fi_send(ep->ep, buf, len, desc, destination, context);
    }
    struct iovec iov = {
        .iov_base = (void *)buf,
        .iov_len = len,
    };
    void *descriptors[1] = {desc};
    struct fi_msg message = {
        .msg_iov = &iov,
        .desc = descriptors,
        .iov_count = 1,
        .addr = destination,
        .context = context,
        .data = 0,
    };
    return (int)fi_sendmsg(ep->ep, &message, FI_COMPLETION);
}

static int zc_ofi_post_send(struct zc_ofi_endpoint *ep, const void *buf,
                            size_t len, void *desc, fi_addr_t destination,
                            int timeout_ms, size_t *out_slot) {
    uint64_t start = zc_ofi_now_ms();
    uint64_t spins = 0;
    size_t slot = 0;
    int rc = zc_ofi_find_free_slot(&ep->send_ring, &slot);
    while (rc == -FI_EAGAIN) {
        size_t reaped = 0;
        rc = zc_ofi_poll_ring(ep, &ep->send_ring, 0, NULL, NULL, NULL,
                              NULL, ep->send_ring.depth, &reaped, 1,
                              timeout_ms);
        if (rc) {
            return rc;
        }
        rc = zc_ofi_find_free_slot(&ep->send_ring, &slot);
    }
    if (rc) {
        return rc;
    }
    struct zc_ofi_op *op =
        zc_ofi_prepare_slot(ep, &ep->send_ring, slot, 0);
    if (!op) {
        return ep->fatal_rc ? ep->fatal_rc : -FI_EBUSY;
    }
    for (;;) {
        rc = zc_ofi_send_post_call(ep, buf, len, desc, destination,
                                   &op->context);
        if (rc != -FI_EAGAIN) {
            break;
        }
        ep->send_ring.post_eagain++;
        ep->send_ring.post_retries++;
        int dispatched = zc_ofi_dispatch_cq_counted(ep, 0);
        if (dispatched < 0) {
            zc_ofi_release_slot(&ep->send_ring, slot);
            return dispatched;
        }
        size_t reaped = 0;
        int progress_rc = zc_ofi_reap_ring(
            &ep->send_ring, NULL, NULL, NULL, NULL, ep->send_ring.depth,
            &reaped);
        if (progress_rc) {
            zc_ofi_release_slot(&ep->send_ring, slot);
            return progress_rc;
        }
        if (zc_ofi_poll_timed_out(ep, spins, start, timeout_ms)) {
            zc_ofi_release_slot(&ep->send_ring, slot);
            snprintf(ep->err, sizeof(ep->err),
                     "OFI fi_send post timed out after %d ms", timeout_ms);
            return -ETIMEDOUT;
        }
        if (dispatched == 0) {
            zc_ofi_wait_after_eagain(ep, &spins);
        }
    }
    if (rc) {
        zc_ofi_release_slot(&ep->send_ring, slot);
        return zc_ofi_fail(ep, rc, "fi_send");
    }
    ep->send_mrs.posts_started = 1;
    ep->send_ring.provider_inflight++;
    ep->send_ring.posts++;
    *out_slot = slot;
    return 0;
}

int zc_ofi_rma_write_queue_init(struct zc_ofi_endpoint *ep, size_t depth) {
    if (!ep || depth == 0 || depth > 65536) {
        return -FI_EINVAL;
    }
    if (ep->write_ring.active != 0) {
        snprintf(ep->err, sizeof(ep->err),
                 "cannot resize OFI RMA write queue with active=%zu inflight=%zu",
                 ep->write_ring.active, ep->write_ring.provider_inflight);
        return -FI_EBUSY;
    }
    if (ep->send_ring.depth > SIZE_MAX - ep->read_ring.depth ||
        ep->send_ring.depth + ep->read_ring.depth > SIZE_MAX - depth ||
        ep->send_ring.depth + ep->read_ring.depth + depth >
            SIZE_MAX - ep->cq_headroom) {
        snprintf(ep->err, sizeof(ep->err),
                 "OFI RMA write queue/CQ size arithmetic overflow");
        return -FI_EINVAL;
    }
    size_t queue_entries = ep->send_ring.depth + ep->read_ring.depth + depth;
    size_t required = queue_entries + ep->cq_headroom;
    if (queue_entries > ep->tx_cq_state.configured_size ||
        (ep->strict_topology && required > ep->tx_cq_state.configured_size)) {
        snprintf(ep->err, sizeof(ep->err),
                 "OFI TX CQ size=%zu is below required=%zu for RMA write depth=%zu; set URING_PLAY_OFI_TX_CQ_SIZE before open",
                 ep->tx_cq_state.configured_size, required, depth);
        return -FI_ENOSPC;
    }
    if (ep->write_ring.depth == depth) {
        ep->tx_cq_required = required;
        return 0;
    }
    int rc = zc_ofi_init_ring(&ep->write_ring, depth, ZC_OFI_OP_WRITE,
                              ep->err, sizeof(ep->err));
    if (!rc) {
        ep->tx_cq_required = required;
    }
    return rc;
}

static int zc_ofi_write_post_call(struct zc_ofi_endpoint *ep,
                                  const void *buf, size_t len, void *desc,
                                  uint64_t remote_addr, uint64_t remote_key,
                                  void *context, int use_more) {
    if (!ep->efa_write_high_pps_effective && !ep->selective_completion &&
        !ep->rma_write_delivery_complete && !use_more) {
        return (int)fi_write(ep->ep, buf, len, desc, ep->peer_addr,
                             remote_addr, remote_key, context);
    }
    struct iovec local_iov = {
        .iov_base = (void *)buf,
        .iov_len = len,
    };
    struct fi_rma_iov remote_iov = {
        .addr = remote_addr,
        .len = len,
        .key = remote_key,
    };
    void *descriptors[1] = {desc};
    struct fi_msg_rma message = {
        .msg_iov = &local_iov,
        .desc = descriptors,
        .iov_count = 1,
        .addr = ep->peer_addr,
        .rma_iov = &remote_iov,
        .rma_iov_count = 1,
        .context = context,
        .data = 0,
    };
    uint64_t flags = 0;
    if (use_more) {
        flags |= FI_MORE;
    }
#if ZC_OFI_HAVE_EFA_WR_HIGH_PPS
    if (ep->efa_write_high_pps_effective) {
        flags |= FI_EFA_WR_HIGH_PPS;
    }
#endif
    if (ep->selective_completion || ep->efa_write_high_pps_effective) {
        /* Provider-specific flags make the completion request explicit.  In
         * particular, EFA accepts a high-PPS write without FI_COMPLETION but
         * does not then produce the CQE required to recycle this slot. */
        flags |= FI_COMPLETION;
    }
    if (ep->rma_write_delivery_complete) {
        /* The WAL metadata doorbell may only be sent after the remote payload
         * is visible at the leaf. A transmit-completion-only CQE is not a
         * remote-delivery guarantee. */
        flags |= FI_COMPLETION | FI_DELIVERY_COMPLETE;
    }
    int rc = (int)fi_writemsg(ep->ep, &message, flags);
    if (!rc && ep->efa_write_high_pps_effective) {
        return 0;
    }
    if (!ep->efa_write_high_pps_effective ||
        (rc != -FI_EINVAL && rc != -FI_EOPNOTSUPP && rc != -FI_ENOSYS)) {
        return rc;
    }
    if (ep->strict_topology) {
        snprintf(ep->err, sizeof(ep->err),
                 "strict OFI topology requested FI_EFA_WR_HIGH_PPS but fi_writemsg rejected it rc=%d (%s)",
                 rc, zc_ofi_errstr(rc));
        return rc;
    }
    ep->efa_write_high_pps_effective = 0;
    ep->efa_write_high_pps_fallbacks++;
    if (!ep->selective_completion && !ep->rma_write_delivery_complete) {
        return (int)fi_write(ep->ep, buf, len, desc, ep->peer_addr,
                             remote_addr, remote_key, context);
    }
    uint64_t fallback_flags = FI_COMPLETION | (use_more ? FI_MORE : 0);
    if (ep->rma_write_delivery_complete) {
        fallback_flags |= FI_DELIVERY_COMPLETE;
    }
    return (int)fi_writemsg(ep->ep, &message, fallback_flags);
}

int zc_ofi_rma_write_post_more(struct zc_ofi_endpoint *ep, const void *buf,
                               size_t len, uint64_t remote_addr,
                               uint64_t remote_key, size_t slot,
                               uint64_t user_data, int more) {
    if (!ep || !buf || len == 0 || !ep->write_ring.ops ||
        slot >= ep->write_ring.depth) {
        return -FI_EINVAL;
    }
    if (ep->peer_addr == FI_ADDR_UNSPEC) {
        snprintf(ep->err, sizeof(ep->err), "OFI peer address is not set");
        return -FI_EINVAL;
    }
    if (ep->max_rma_size && len > ep->max_rma_size) {
        snprintf(ep->err, sizeof(ep->err),
                 "OFI RMA write len=%zu exceeds max_rma_size=%zu",
                 len, ep->max_rma_size);
        return -EMSGSIZE;
    }
    void *desc = NULL;
    int rc = zc_ofi_register_cached(ep, buf, len, FI_WRITE,
                                    &ep->write_mrs, &desc);
    if (rc) {
        return rc;
    }
    struct zc_ofi_op *op =
        zc_ofi_prepare_slot(ep, &ep->write_ring, slot, user_data);
    if (!op) {
        return ep->fatal_rc ? ep->fatal_rc : -FI_EBUSY;
    }
    int use_more = more && ep->rma_write_more_enabled &&
                   !ep->rma_write_force_flush &&
                   ep->rma_write_more_streak + 1 < ep->rma_write_more_burst;
    rc = zc_ofi_write_post_call(ep, buf, len, desc, remote_addr, remote_key,
                                &op->context, use_more);
    if (rc) {
        zc_ofi_release_slot(&ep->write_ring, slot);
        if (rc == -FI_EAGAIN) {
            if (ep->rma_write_more_streak != 0) {
                /* A successful FI_MORE post promises a prompt follow-up. If
                 * provider backpressure rejects that follow-up, make the next
                 * accepted post a non-FI_MORE boundary. This prevents an
                 * arbitrarily long deferred-doorbell streak across retries. */
                ep->rma_write_force_flush = 1;
                ep->rma_write_more_followup_eagain++;
            }
            ep->write_ring.post_eagain++;
            ep->write_ring.post_retries++;
            return rc;
        }
        return zc_ofi_fail(ep, rc, "fi_write(async)");
    }
    ep->write_mrs.posts_started = 1;
    ep->write_ring.provider_inflight++;
    ep->write_ring.posts++;
    if (use_more) {
        ep->rma_write_more_posts++;
        ep->rma_write_more_streak++;
    } else if (ep->rma_write_more_enabled) {
        ep->rma_write_flush_posts++;
        ep->rma_write_forced_flush_posts += more ? 1 : 0;
        ep->rma_write_more_streak = 0;
        ep->rma_write_force_flush = 0;
    }
    return 0;
}

int zc_ofi_rma_write_post(struct zc_ofi_endpoint *ep, const void *buf,
                          size_t len, uint64_t remote_addr,
                          uint64_t remote_key, size_t slot,
                          uint64_t user_data) {
    return zc_ofi_rma_write_post_more(ep, buf, len, remote_addr, remote_key,
                                      slot, user_data, 0);
}

int zc_ofi_rma_write_poll(struct zc_ofi_endpoint *ep, size_t *out_slots,
                          uint64_t *out_user_data, size_t capacity,
                          size_t *out_count, int wait, int timeout_ms) {
    if (!ep || !out_slots || !out_user_data || !out_count || capacity == 0 ||
        !ep->write_ring.ops || capacity > ep->write_ring.depth) {
        return -FI_EINVAL;
    }
    return zc_ofi_poll_ring(ep, &ep->write_ring, 0, out_slots,
                            out_user_data, NULL, NULL, capacity, out_count,
                            wait, timeout_ms);
}

int zc_ofi_rma_write(struct zc_ofi_endpoint *ep, const void *buf, size_t len,
                     uint64_t remote_addr, uint64_t remote_key, int timeout_ms) {
    if (!ep || !buf || len == 0) {
        return -FI_EINVAL;
    }
    size_t slot = 0;
    int rc = zc_ofi_find_free_slot(&ep->write_ring, &slot);
    if (rc) {
        snprintf(ep->err, sizeof(ep->err),
                 "no free synchronous OFI RMA write slot depth=%zu active=%zu",
                 ep->write_ring.depth, ep->write_ring.active);
        return rc;
    }
    uint64_t start = zc_ofi_now_ms();
    uint64_t spins = 0;
    for (;;) {
        rc = zc_ofi_rma_write_post(ep, buf, len, remote_addr, remote_key,
                                   slot, 0);
        if (rc != -FI_EAGAIN) {
            break;
        }
        int dispatched = zc_ofi_dispatch_cq_counted(ep, 0);
        if (dispatched < 0) {
            return dispatched;
        }
        if (zc_ofi_poll_timed_out(ep, spins, start, timeout_ms)) {
            snprintf(ep->err, sizeof(ep->err),
                     "OFI fi_write post timed out after %d ms", timeout_ms);
            return -ETIMEDOUT;
        }
        if (dispatched == 0) {
            zc_ofi_wait_after_eagain(ep, &spins);
        }
    }
    if (rc) {
        return rc;
    }
    return zc_ofi_wait_slot(ep, &ep->write_ring, 0, slot, NULL, NULL,
                            timeout_ms);
}

int zc_ofi_rma_read(struct zc_ofi_endpoint *ep, void *buf, size_t len,
                    uint64_t remote_addr, uint64_t remote_key, int timeout_ms) {
    if (!ep || !buf || len == 0) {
        return -FI_EINVAL;
    }
    size_t slot = 0;
    int rc = zc_ofi_find_free_slot(&ep->read_ring, &slot);
    if (rc) {
        snprintf(ep->err, sizeof(ep->err),
                 "no free synchronous OFI RMA read slot depth=%zu active=%zu",
                 ep->read_ring.depth, ep->read_ring.active);
        return rc;
    }
    uint64_t start = zc_ofi_now_ms();
    uint64_t spins = 0;
    for (;;) {
        rc = zc_ofi_rma_read_post(ep, buf, len, remote_addr, remote_key,
                                  slot, 0, 0);
        if (rc != -FI_EAGAIN) {
            break;
        }
        int dispatched = zc_ofi_dispatch_cq_counted(ep, 0);
        if (dispatched < 0) {
            return dispatched;
        }
        if (zc_ofi_poll_timed_out(ep, spins, start, timeout_ms)) {
            snprintf(ep->err, sizeof(ep->err),
                     "OFI fi_read post timed out after %d ms", timeout_ms);
            return -ETIMEDOUT;
        }
        if (dispatched == 0) {
            zc_ofi_wait_after_eagain(ep, &spins);
        }
    }
    if (rc) {
        return rc;
    }
    return zc_ofi_wait_slot(ep, &ep->read_ring, 0, slot, NULL, NULL,
                            timeout_ms);
}

int zc_ofi_send(struct zc_ofi_endpoint *ep, const void *buf, size_t len, int timeout_ms) {
    if (!ep || !buf || len == 0) {
        return -FI_EINVAL;
    }
    if (ep->peer_addr == FI_ADDR_UNSPEC) {
        snprintf(ep->err, sizeof(ep->err), "OFI peer address is not set");
        return -FI_EINVAL;
    }
    if (ep->max_msg_size && len > ep->max_msg_size) {
        snprintf(ep->err, sizeof(ep->err), "OFI message len=%zu exceeds max_msg_size=%zu",
                 len, ep->max_msg_size);
        return -EMSGSIZE;
    }
    void *desc = NULL;
    int rc = zc_ofi_register_cached(ep, buf, len, FI_SEND, &ep->send_mrs, &desc);
    if (rc) {
        return rc;
    }
    size_t slot = 0;
    rc = zc_ofi_post_send(ep, buf, len, desc, ep->peer_addr, timeout_ms,
                          &slot);
    if (rc) {
        return rc;
    }
    return zc_ofi_wait_slot(ep, &ep->send_ring, 0, slot, NULL, NULL,
                            timeout_ms);
}

int zc_ofi_send_many(struct zc_ofi_endpoint *ep, const void *base, size_t stride,
                     size_t len, size_t count, int timeout_ms) {
    if (!ep || !base || len == 0) {
        return -FI_EINVAL;
    }
    if (count == 0) {
        return 0;
    }
    if (ep->peer_addr == FI_ADDR_UNSPEC) {
        snprintf(ep->err, sizeof(ep->err), "OFI peer address is not set");
        return -FI_EINVAL;
    }
    if (ep->max_msg_size && len > ep->max_msg_size) {
        snprintf(ep->err, sizeof(ep->err), "OFI message len=%zu exceeds max_msg_size=%zu",
                 len, ep->max_msg_size);
        return -EMSGSIZE;
    }
    if (stride < len) {
        snprintf(ep->err, sizeof(ep->err), "OFI send_many stride=%zu below len=%zu",
                 stride, len);
        return -FI_EINVAL;
    }
    if (count > 1 && stride > (SIZE_MAX - len) / (count - 1)) {
        snprintf(ep->err, sizeof(ep->err), "OFI send_many byte span overflow");
        return -FI_EINVAL;
    }
    size_t span = len + stride * (count - 1);
    void *desc = NULL;
    int rc = zc_ofi_register_cached(ep, base, span, FI_SEND,
                                    &ep->send_mrs, &desc);
    if (rc) {
        return rc;
    }
    const char *bytes = (const char *)base;
    for (size_t posted = 0; posted < count; posted++) {
        size_t slot = 0;
        rc = zc_ofi_post_send(ep, bytes + posted * stride, len, desc,
                              ep->peer_addr, timeout_ms, &slot);
        if (rc) {
            return rc;
        }
    }
    return zc_ofi_drain_send(ep, timeout_ms);
}

int zc_ofi_send_many_nowait(struct zc_ofi_endpoint *ep, const void *base, size_t stride,
                            size_t len, size_t count, int timeout_ms) {
    if (!ep || !base || len == 0) {
        return -FI_EINVAL;
    }
    if (count == 0) {
        return 0;
    }
    if (ep->peer_addr == FI_ADDR_UNSPEC) {
        snprintf(ep->err, sizeof(ep->err), "OFI peer address is not set");
        return -FI_EINVAL;
    }
    if (ep->max_msg_size && len > ep->max_msg_size) {
        snprintf(ep->err, sizeof(ep->err), "OFI message len=%zu exceeds max_msg_size=%zu",
                 len, ep->max_msg_size);
        return -EMSGSIZE;
    }
    if (stride < len) {
        snprintf(ep->err, sizeof(ep->err), "OFI send_many_nowait stride=%zu below len=%zu",
                 stride, len);
        return -FI_EINVAL;
    }
    if (count > 1 && stride > (SIZE_MAX - len) / (count - 1)) {
        snprintf(ep->err, sizeof(ep->err), "OFI send_many_nowait byte span overflow");
        return -FI_EINVAL;
    }
    size_t span = len + stride * (count - 1);
    void *desc = NULL;
    int rc = zc_ofi_register_cached(ep, base, span, FI_SEND,
                                    &ep->send_mrs, &desc);
    if (rc) {
        return rc;
    }
    const char *bytes = (const char *)base;
    for (size_t posted = 0; posted < count; posted++) {
        size_t slot = 0;
        rc = zc_ofi_post_send(ep, bytes + posted * stride, len, desc,
                              ep->peer_addr, timeout_ms, &slot);
        if (rc) {
            return rc;
        }
    }
    return 0;
}

int zc_ofi_drain_send(struct zc_ofi_endpoint *ep, int timeout_ms) {
    if (!ep) {
        return -FI_EINVAL;
    }
    while (ep->send_ring.active > 0) {
        size_t reaped = 0;
        int rc = zc_ofi_poll_ring(ep, &ep->send_ring, 0, NULL, NULL, NULL,
                                  NULL, ep->send_ring.depth, &reaped, 1,
                                  timeout_ms);
        if (rc) {
            return rc;
        }
    }
    return 0;
}

size_t zc_ofi_send_pending(const struct zc_ofi_endpoint *ep) {
    return ep ? ep->send_ring.active : 0;
}

int zc_ofi_send_poll(struct zc_ofi_endpoint *ep, size_t *out_count,
                     int wait, int timeout_ms) {
    if (!ep || !out_count || !ep->send_ring.ops) {
        return -FI_EINVAL;
    }
    return zc_ofi_poll_ring(ep, &ep->send_ring, 0, NULL, NULL, NULL,
                            NULL, ep->send_ring.depth, out_count, wait,
                            timeout_ms);
}

int zc_ofi_send_nowait(struct zc_ofi_endpoint *ep, const void *buf, size_t len, int timeout_ms) {
    if (!ep || !buf || len == 0) {
        return -FI_EINVAL;
    }
    if (ep->peer_addr == FI_ADDR_UNSPEC) {
        snprintf(ep->err, sizeof(ep->err), "OFI peer address is not set");
        return -FI_EINVAL;
    }
    if (ep->max_msg_size && len > ep->max_msg_size) {
        snprintf(ep->err, sizeof(ep->err), "OFI message len=%zu exceeds max_msg_size=%zu",
                 len, ep->max_msg_size);
        return -EMSGSIZE;
    }
    void *desc = NULL;
    int rc = zc_ofi_register_cached(ep, buf, len, FI_SEND,
                                    &ep->send_mrs, &desc);
    if (rc) {
        return rc;
    }
    size_t slot = 0;
    return zc_ofi_post_send(ep, buf, len, desc, ep->peer_addr, timeout_ms,
                            &slot);
}

int zc_ofi_send_to_last(struct zc_ofi_endpoint *ep, const void *buf, size_t len, int timeout_ms) {
    if (!ep) {
        return -FI_EINVAL;
    }
    if (ep->last_src_addr == FI_ADDR_UNSPEC && ep->peer_addr == FI_ADDR_UNSPEC) {
        snprintf(ep->err, sizeof(ep->err), "OFI last source and peer addresses are not set");
        return -FI_EINVAL;
    }
    fi_addr_t old = ep->peer_addr;
    if (ep->last_src_addr != FI_ADDR_UNSPEC) {
        ep->peer_addr = ep->last_src_addr;
    }
    int rc = zc_ofi_send(ep, buf, len, timeout_ms);
    ep->peer_addr = old;
    return rc;
}

int zc_ofi_inject(struct zc_ofi_endpoint *ep, const void *buf, size_t len) {
    if (!ep || !buf) {
        return -FI_EINVAL;
    }
    if (ep->fatal_rc) {
        return ep->fatal_rc;
    }
    if (ep->peer_addr == FI_ADDR_UNSPEC) {
        snprintf(ep->err, sizeof(ep->err), "OFI peer address is not set");
        return -FI_EINVAL;
    }
    if (ep->inject_size && len > ep->inject_size) {
        snprintf(ep->err, sizeof(ep->err), "OFI inject len=%zu exceeds inject_size=%zu",
                 len, ep->inject_size);
        return -FI_EINVAL;
    }
    int rc = (int)fi_inject(ep->ep, buf, len, ep->peer_addr);
    if (rc) {
        return zc_ofi_fail(ep, rc, "fi_inject");
    }
    ep->inject_posts++;
    return 0;
}

int zc_ofi_inject_to_last(struct zc_ofi_endpoint *ep, const void *buf, size_t len) {
    if (!ep) {
        return -FI_EINVAL;
    }
    if (ep->last_src_addr == FI_ADDR_UNSPEC && ep->peer_addr == FI_ADDR_UNSPEC) {
        snprintf(ep->err, sizeof(ep->err), "OFI last source and peer addresses are not set");
        return -FI_EINVAL;
    }
    fi_addr_t old = ep->peer_addr;
    if (ep->last_src_addr != FI_ADDR_UNSPEC) {
        ep->peer_addr = ep->last_src_addr;
    }
    int rc = zc_ofi_inject(ep, buf, len);
    ep->peer_addr = old;
    return rc;
}

int zc_ofi_recv_queue_init(struct zc_ofi_endpoint *ep, size_t depth) {
    if (!ep || depth == 0 || depth > 65536) {
        return -FI_EINVAL;
    }
    if (ep->recv_ring.active != 0) {
        snprintf(ep->err, sizeof(ep->err),
                 "cannot resize OFI receive queue with active=%zu inflight=%zu",
                 ep->recv_ring.active, ep->recv_ring.provider_inflight);
        return -FI_EBUSY;
    }
    if (depth > SIZE_MAX - ep->cq_headroom) {
        snprintf(ep->err, sizeof(ep->err),
                 "OFI receive queue/CQ size arithmetic overflow");
        return -FI_EINVAL;
    }
    size_t required = depth + ep->cq_headroom;
    if (depth > ep->rx_cq_state.configured_size ||
        (ep->strict_topology && required > ep->rx_cq_state.configured_size)) {
        snprintf(ep->err, sizeof(ep->err),
                 "OFI RX CQ size=%zu is below required=%zu for receive depth=%zu; set URING_PLAY_OFI_RX_CQ_SIZE before open",
                 ep->rx_cq_state.configured_size, required, depth);
        return -FI_ENOSPC;
    }
    if (ep->recv_ring.depth == depth) {
        ep->rx_cq_required = required;
        return 0;
    }
    int rc = zc_ofi_init_ring(&ep->recv_ring, depth, ZC_OFI_OP_RECV,
                              ep->err, sizeof(ep->err));
    if (!rc) {
        ep->rx_cq_required = required;
    }
    return rc;
}

int zc_ofi_recv_post(struct zc_ofi_endpoint *ep, void *buf, size_t cap,
                     size_t slot, uint64_t user_data) {
    if (!ep || !buf || cap == 0 || !ep->recv_ring.ops ||
        slot >= ep->recv_ring.depth) {
        return -FI_EINVAL;
    }
    void *desc = NULL;
    int rc = zc_ofi_register_cached(ep, buf, cap, FI_RECV,
                                    &ep->recv_mrs, &desc);
    if (rc) {
        return rc;
    }
    struct zc_ofi_op *op =
        zc_ofi_prepare_slot(ep, &ep->recv_ring, slot, user_data);
    if (!op) {
        return ep->fatal_rc ? ep->fatal_rc : -FI_EBUSY;
    }
    rc = (int)fi_recv(ep->ep, buf, cap, desc, FI_ADDR_UNSPEC,
                      &op->context);
    if (rc) {
        zc_ofi_release_slot(&ep->recv_ring, slot);
        if (rc == -FI_EAGAIN) {
            ep->recv_ring.post_eagain++;
            ep->recv_ring.post_retries++;
            return rc;
        }
        return zc_ofi_fail(ep, rc, "fi_recv(async)");
    }
    ep->recv_mrs.posts_started = 1;
    ep->recv_ring.provider_inflight++;
    ep->recv_ring.posts++;
    return 0;
}

int zc_ofi_recv_poll(struct zc_ofi_endpoint *ep, size_t *out_slots,
                     uint64_t *out_user_data, size_t *out_lengths,
                     fi_addr_t *out_sources, size_t capacity,
                     size_t *out_count, int wait, int timeout_ms) {
    if (!ep || !out_slots || !out_user_data || !out_lengths || !out_sources ||
        !out_count || capacity == 0 || !ep->recv_ring.ops ||
        capacity > ep->recv_ring.depth) {
        return -FI_EINVAL;
    }
    int rc = zc_ofi_poll_ring(ep, &ep->recv_ring, 1, out_slots,
                              out_user_data, out_lengths, out_sources, capacity,
                              out_count, wait, timeout_ms);
    if (!rc && *out_count > 0) {
        ep->last_src_addr = out_sources[*out_count - 1];
    }
    return rc;
}

int zc_ofi_recv(struct zc_ofi_endpoint *ep, void *buf, size_t cap, size_t *out_len,
                int timeout_ms) {
    if (!ep || !buf || cap == 0 || !out_len) {
        return -FI_EINVAL;
    }
    size_t slot = 0;
    int rc = zc_ofi_find_free_slot(&ep->recv_ring, &slot);
    if (rc) {
        snprintf(ep->err, sizeof(ep->err),
                 "no free synchronous OFI receive slot depth=%zu active=%zu",
                 ep->recv_ring.depth, ep->recv_ring.active);
        return rc;
    }
    uint64_t start = zc_ofi_now_ms();
    uint64_t spins = 0;
    for (;;) {
        rc = zc_ofi_recv_post(ep, buf, cap, slot, 0);
        if (rc != -FI_EAGAIN) {
            break;
        }
        int dispatched = zc_ofi_dispatch_cq_counted(ep, 1);
        if (dispatched < 0) {
            return dispatched;
        }
        if (zc_ofi_poll_timed_out(ep, spins, start, timeout_ms)) {
            snprintf(ep->err, sizeof(ep->err),
                     "OFI fi_recv post timed out after %d ms", timeout_ms);
            return -ETIMEDOUT;
        }
        if (dispatched == 0) {
            zc_ofi_wait_after_eagain(ep, &spins);
        }
    }
    if (rc) {
        return rc;
    }
    fi_addr_t source = FI_ADDR_UNSPEC;
    rc = zc_ofi_wait_slot(ep, &ep->recv_ring, 1, slot, out_len, &source,
                          timeout_ms);
    if (!rc) {
        ep->last_src_addr = source;
    }
    return rc;
}

int zc_ofi_recv_start(struct zc_ofi_endpoint *ep, void *buf, size_t cap, int timeout_ms) {
    if (!ep || !buf || cap == 0) {
        return -FI_EINVAL;
    }
    if (ep->legacy_recv_slot != SIZE_MAX) {
        snprintf(ep->err, sizeof(ep->err), "OFI async recv is already pending");
        return -FI_EBUSY;
    }
    size_t slot = 0;
    int rc = zc_ofi_find_free_slot(&ep->recv_ring, &slot);
    if (rc) {
        snprintf(ep->err, sizeof(ep->err),
                 "no free OFI receive slot depth=%zu active=%zu",
                 ep->recv_ring.depth, ep->recv_ring.active);
        return rc;
    }
    uint64_t start = zc_ofi_now_ms();
    uint64_t spins = 0;
    for (;;) {
        rc = zc_ofi_recv_post(ep, buf, cap, slot, 0);
        if (rc != -FI_EAGAIN) {
            break;
        }
        int dispatched = zc_ofi_dispatch_cq_counted(ep, 1);
        if (dispatched < 0) {
            return dispatched;
        }
        if (zc_ofi_poll_timed_out(ep, spins, start, timeout_ms)) {
            snprintf(ep->err, sizeof(ep->err),
                     "OFI fi_recv_start post timed out after %d ms", timeout_ms);
            return -ETIMEDOUT;
        }
        if (dispatched == 0) {
            zc_ofi_wait_after_eagain(ep, &spins);
        }
    }
    if (rc) {
        return rc;
    }
    ep->legacy_recv_slot = slot;
    return 0;
}

int zc_ofi_recv_finish(struct zc_ofi_endpoint *ep, size_t *out_len, int timeout_ms) {
    if (!ep || !out_len) {
        return -FI_EINVAL;
    }
    if (ep->legacy_recv_slot == SIZE_MAX) {
        snprintf(ep->err, sizeof(ep->err), "OFI async recv is not pending");
        return -FI_EINVAL;
    }
    size_t len = 0;
    fi_addr_t src = FI_ADDR_UNSPEC;
    size_t slot = ep->legacy_recv_slot;
    int rc = zc_ofi_wait_slot(ep, &ep->recv_ring, 1, slot, &len, &src,
                              timeout_ms);
    if (rc) {
        if (!ep->recv_ring.ops[slot].active) {
            ep->legacy_recv_slot = SIZE_MAX;
        }
        return rc;
    }
    ep->legacy_recv_slot = SIZE_MAX;
    ep->last_src_addr = src;
    *out_len = len;
    return 0;
}

int zc_ofi_recv_try_finish(struct zc_ofi_endpoint *ep, size_t *out_len,
                           int *out_ready) {
    if (!ep || !out_len || !out_ready) {
        return -FI_EINVAL;
    }
    *out_ready = 0;
    *out_len = 0;
    if (ep->legacy_recv_slot == SIZE_MAX) {
        snprintf(ep->err, sizeof(ep->err), "OFI async recv is not pending");
        return -FI_EINVAL;
    }
    size_t slot = ep->legacy_recv_slot;
    struct zc_ofi_op *op = &ep->recv_ring.ops[slot];
    if (!op->completed) {
        int dispatched = zc_ofi_dispatch_cq_counted(ep, 1);
        if (dispatched < 0) {
            return dispatched;
        }
        if (!op->completed) {
            return 0;
        }
    }
    size_t len = 0;
    fi_addr_t src = FI_ADDR_UNSPEC;
    int rc = zc_ofi_wait_slot(ep, &ep->recv_ring, 1, slot, &len, &src, 1);
    if (rc) {
        if (!ep->recv_ring.ops[slot].active) {
            ep->legacy_recv_slot = SIZE_MAX;
        }
        return rc;
    }
    ep->legacy_recv_slot = SIZE_MAX;
    ep->last_src_addr = src;
    *out_len = len;
    *out_ready = 1;
    return 0;
}
