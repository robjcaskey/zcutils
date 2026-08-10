#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <rdma/fabric.h>
#include <rdma/fi_cm.h>
#include <rdma/fi_domain.h>
#include <rdma/fi_endpoint.h>
#include <rdma/fi_eq.h>
#include <rdma/fi_errno.h>
#include <rdma/fi_rma.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#ifndef FI_EPROTO
#define FI_EPROTO EPROTO
#endif

struct zc_ofi_mr_cache {
    const void *buf;
    size_t len;
    uint64_t access;
    struct fid_mr *mr;
    void *desc;
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
    size_t inject_size;
    struct zc_ofi_mr_cache send_mr;
    struct zc_ofi_mr_cache recv_mr;
    struct zc_ofi_mr_cache read_mr;
    struct zc_ofi_mr_cache write_mr;
    struct fid_mr *rma_target_mr;
    struct fi_context2 async_send_context;
    struct fi_context2 async_recv_context;
    struct fi_context2 *async_send_contexts;
    size_t async_send_context_count;
    int async_send_pending;
    int async_recv_pending;
    int mr_local;
    int mr_virt_addr;
    uint64_t busy_poll_iters;
    long cq_sleep_ns;
    char err[512];
};

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
    if (errno || end == value) {
        return fallback;
    }
    return (uint64_t)parsed;
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

static void zc_ofi_close_fid(struct fid *fid) {
    if (fid) {
        fi_close(fid);
    }
}

static void zc_ofi_close_mr_cache(struct zc_ofi_mr_cache *cache) {
    if (!cache) {
        return;
    }
    zc_ofi_close_fid(cache->mr ? &cache->mr->fid : NULL);
    memset(cache, 0, sizeof(*cache));
}

void zc_ofi_close(struct zc_ofi_endpoint *ep) {
    if (!ep) {
        return;
    }
    zc_ofi_close_mr_cache(&ep->send_mr);
    zc_ofi_close_mr_cache(&ep->recv_mr);
    zc_ofi_close_mr_cache(&ep->read_mr);
    zc_ofi_close_mr_cache(&ep->write_mr);
    zc_ofi_close_fid(ep->rma_target_mr ? &ep->rma_target_mr->fid : NULL);
    zc_ofi_close_fid(ep->ep ? &ep->ep->fid : NULL);
    zc_ofi_close_fid(ep->av ? &ep->av->fid : NULL);
    zc_ofi_close_fid(ep->rx_cq ? &ep->rx_cq->fid : NULL);
    zc_ofi_close_fid(ep->tx_cq ? &ep->tx_cq->fid : NULL);
    zc_ofi_close_fid(ep->domain ? &ep->domain->fid : NULL);
    zc_ofi_close_fid(ep->fabric ? &ep->fabric->fid : NULL);
    free(ep->async_send_contexts);
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

size_t zc_ofi_inject_size(const struct zc_ofi_endpoint *ep) {
    return ep ? ep->inject_size : 0;
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
    return provider && strcmp(provider, "efa") == 0;
}

static int zc_ofi_open_on_domain_caps(const char *provider, const char *endpoint,
                                       const char *node, const char *service, int server,
                                       const char *domain_name, uint64_t caps,
                                       uint64_t tx_bind_flags, uint64_t rx_bind_flags,
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
    int efa_provider = zc_ofi_is_efa_provider(provider);
    hints->caps = caps | (server && !efa_provider ? FI_SOURCE : 0);
    hints->mode = efa_provider ? 0 : FI_CONTEXT;
    hints->addr_format = efa_provider ? FI_ADDR_EFA : FI_SOCKADDR;
    hints->ep_attr->type = ep_type;
    if (provider && provider[0] != '\0') {
        hints->fabric_attr->prov_name = strdup(provider);
        if (!hints->fabric_attr->prov_name) {
            fi_freeinfo(hints);
            zc_ofi_write_err(err, err_len, "strdup(provider) failed");
            return -FI_ENOMEM;
        }
    }
    if (efa_provider) {
        const char *fabric = getenv("URING_PLAY_OFI_EFA_FABRIC");
        if (!fabric || fabric[0] == '\0') {
            fabric = "efa";
        }
        hints->fabric_attr->name = strdup(fabric);
        if (!hints->fabric_attr->name) {
            fi_freeinfo(hints);
            zc_ofi_write_err(err, err_len, "strdup(efa fabric) failed");
            return -FI_ENOMEM;
        }
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
    int rc = fi_getinfo(FI_VERSION(1, 11), query_node, query_service, flags, hints, &info);
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
    ep->max_msg_size = info->ep_attr ? info->ep_attr->max_msg_size : 0;
    ep->inject_size = info->tx_attr ? info->tx_attr->inject_size : 0;
    ep->mr_local = info->domain_attr && (info->domain_attr->mr_mode & FI_MR_LOCAL);
    ep->mr_virt_addr =
        (info->domain_attr && (info->domain_attr->mr_mode & FI_MR_VIRT_ADDR)) ||
        efa_provider;
    ep->busy_poll_iters = zc_ofi_env_u64("URING_PLAY_OFI_BUSY_POLL_ITERS", 0);
    ep->cq_sleep_ns = (long)zc_ofi_env_u64("URING_PLAY_OFI_CQ_SLEEP_NS", 50000);

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
    cq_attr.size = 1024;
    rc = fi_cq_open(ep->domain, &cq_attr, &ep->tx_cq, NULL);
    if (rc) {
        zc_ofi_write_err(err, err_len, "fi_cq_open(tx) rc=%d (%s)", rc, zc_ofi_errstr(rc));
        zc_ofi_close(ep);
        return rc;
    }
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

    *out = ep;
    return 0;
}

int zc_ofi_open_on_domain(const char *provider, const char *endpoint, const char *node,
                          const char *service, int server, const char *domain_name,
                          struct zc_ofi_endpoint **out, char *err, size_t err_len) {
    return zc_ofi_open_on_domain_caps(provider, endpoint, node, service, server,
                                      domain_name, FI_MSG, FI_SEND, FI_RECV, out, err,
                                      err_len);
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
                                      FI_RECV, out, err, err_len);
}

int zc_ofi_open_rma(const char *provider, const char *endpoint, const char *node,
                    const char *service, int server, struct zc_ofi_endpoint **out,
                    char *err, size_t err_len) {
    return zc_ofi_open_rma_on_domain(provider, endpoint, node, service, server, NULL, out,
                                     err, err_len);
}

static int zc_ofi_wait_cq(struct zc_ofi_endpoint *ep, struct fid_cq *cq, const char *label,
                          void *context, int with_src, fi_addr_t *src_addr,
                          size_t *len, int timeout_ms) {
    uint64_t start = zc_ofi_now_ms();
    uint64_t spins = 0;
    for (;;) {
        struct fi_cq_msg_entry entry;
        memset(&entry, 0, sizeof(entry));
        ssize_t rc;
        if (with_src) {
            fi_addr_t src = FI_ADDR_UNSPEC;
            rc = fi_cq_readfrom(cq, &entry, 1, &src);
            if (rc > 0) {
                if (src_addr) {
                    *src_addr = src;
                }
            }
        } else {
            rc = fi_cq_read(cq, &entry, 1);
        }
        if (rc > 0) {
            if (entry.op_context != context) {
                snprintf(ep->err, sizeof(ep->err),
                         "unexpected OFI CQ context got=%p expected=%p",
                         entry.op_context, context);
                return -FI_EPROTO;
            }
            if (len) {
                *len = entry.len;
            }
            return 0;
        }
        if (rc == -FI_EAGAIN) {
            if (zc_ofi_poll_timed_out(ep, spins, start, timeout_ms)) {
                snprintf(ep->err, sizeof(ep->err), "OFI CQ wait timed out after %d ms", timeout_ms);
                return -ETIMEDOUT;
            }
            zc_ofi_wait_after_eagain(ep, &spins);
            continue;
        }
        if (rc == -FI_EAVAIL) {
            struct fi_cq_err_entry err_entry;
            memset(&err_entry, 0, sizeof(err_entry));
            ssize_t erc = fi_cq_readerr(cq, &err_entry, 0);
            if (erc >= 0) {
#if FI_MAJOR_VERSION >= 2
                snprintf(ep->err, sizeof(ep->err),
                         "OFI %s CQ error err=%d prov_errno=%d len=%zu src=%llu",
                         label, err_entry.err, err_entry.prov_errno, err_entry.len,
                         (unsigned long long)err_entry.src_addr);
#else
                snprintf(ep->err, sizeof(ep->err),
                         "OFI %s CQ error err=%d prov_errno=%d len=%zu",
                         label, err_entry.err, err_entry.prov_errno, err_entry.len);
#endif
                return err_entry.err ? -err_entry.err : -FI_EIO;
            }
        }
        return zc_ofi_fail(ep, (int)rc, label);
    }
}

static int zc_ofi_register_cached(struct zc_ofi_endpoint *ep, const void *buf, size_t len,
                                  uint64_t access, struct zc_ofi_mr_cache *cache,
                                  void **desc) {
    *desc = NULL;
    if (!ep->mr_local) {
        return 0;
    }
    if (cache->mr && cache->access == access) {
        uintptr_t cached_start = (uintptr_t)cache->buf;
        uintptr_t cached_end = cached_start + cache->len;
        uintptr_t requested_start = (uintptr_t)buf;
        uintptr_t requested_end = requested_start + len;
        if (cached_end >= cached_start && requested_end >= requested_start &&
            requested_start >= cached_start && requested_end <= cached_end) {
            *desc = cache->desc;
            return 0;
        }
    }
    zc_ofi_close_mr_cache(cache);
    struct fid_mr *mr = NULL;
    int rc = fi_mr_reg(ep->domain, buf, len, access, 0, 0, 0, &mr, NULL);
    if (rc) {
        return zc_ofi_fail(ep, rc, "fi_mr_reg");
    }
    cache->buf = buf;
    cache->len = len;
    cache->access = access;
    cache->mr = mr;
    cache->desc = fi_mr_desc(mr);
    *desc = cache->desc;
    return 0;
}

int zc_ofi_rma_register_read_buffer(struct zc_ofi_endpoint *ep, void *buf, size_t len) {
    if (!ep || !buf || len == 0) {
        return -FI_EINVAL;
    }
    void *desc = NULL;
    return zc_ofi_register_cached(ep, buf, len, FI_READ, &ep->read_mr, &desc);
}

int zc_ofi_rma_register_target(struct zc_ofi_endpoint *ep, void *buf, size_t len,
                               uint64_t *addr, uint64_t *key) {
    if (!ep || !buf || len == 0 || !addr || !key) {
        return -FI_EINVAL;
    }
    zc_ofi_close_fid(ep->rma_target_mr ? &ep->rma_target_mr->fid : NULL);
    ep->rma_target_mr = NULL;
    struct fid_mr *mr = NULL;
    int rc = fi_mr_reg(ep->domain, buf, len, FI_REMOTE_READ | FI_REMOTE_WRITE,
                       0, 0, 0, &mr, NULL);
    if (rc) {
        return zc_ofi_fail(ep, rc, "fi_mr_reg(rma_target)");
    }
    ep->rma_target_mr = mr;
    *addr = ep->mr_virt_addr ? (uint64_t)(uintptr_t)buf : 0;
    *key = fi_mr_key(mr);
    return 0;
}

int zc_ofi_rma_write(struct zc_ofi_endpoint *ep, const void *buf, size_t len,
                     uint64_t remote_addr, uint64_t remote_key, int timeout_ms) {
    if (!ep || !buf || len == 0) {
        return -FI_EINVAL;
    }
    if (ep->peer_addr == FI_ADDR_UNSPEC) {
        snprintf(ep->err, sizeof(ep->err), "OFI peer address is not set");
        return -FI_EINVAL;
    }
    void *desc = NULL;
    int rc = zc_ofi_register_cached(ep, buf, len, FI_WRITE, &ep->write_mr, &desc);
    if (rc) {
        return rc;
    }
    struct fi_context2 context;
    memset(&context, 0, sizeof(context));
    uint64_t post_start = zc_ofi_now_ms();
    uint64_t post_spins = 0;
    do {
        rc = (int)fi_write(ep->ep, buf, len, desc, ep->peer_addr, remote_addr,
                           remote_key, &context);
        if (rc == -FI_EAGAIN) {
            if (timeout_ms > 0 && zc_ofi_now_ms() - post_start >= (uint64_t)timeout_ms) {
                snprintf(ep->err, sizeof(ep->err),
                         "OFI fi_write post timed out after %d ms", timeout_ms);
                return -ETIMEDOUT;
            }
            zc_ofi_wait_after_eagain(ep, &post_spins);
        }
    } while (rc == -FI_EAGAIN);
    if (rc) {
        return zc_ofi_fail(ep, rc, "fi_write");
    }
    return zc_ofi_wait_cq(ep, ep->tx_cq, "tx fi_write cq", &context, 0, NULL, NULL,
                          timeout_ms);
}

int zc_ofi_rma_read(struct zc_ofi_endpoint *ep, void *buf, size_t len,
                    uint64_t remote_addr, uint64_t remote_key, int timeout_ms) {
    if (!ep || !buf || len == 0) {
        return -FI_EINVAL;
    }
    if (ep->peer_addr == FI_ADDR_UNSPEC) {
        snprintf(ep->err, sizeof(ep->err), "OFI peer address is not set");
        return -FI_EINVAL;
    }
    void *desc = NULL;
    int rc = zc_ofi_register_cached(ep, buf, len, FI_READ, &ep->read_mr, &desc);
    if (rc) {
        return rc;
    }
    struct fi_context2 context;
    memset(&context, 0, sizeof(context));
    uint64_t post_start = zc_ofi_now_ms();
    uint64_t post_spins = 0;
    do {
        rc = (int)fi_read(ep->ep, buf, len, desc, ep->peer_addr, remote_addr,
                          remote_key, &context);
        if (rc == -FI_EAGAIN) {
            if (zc_ofi_poll_timed_out(ep, post_spins, post_start, timeout_ms)) {
                snprintf(ep->err, sizeof(ep->err),
                         "OFI fi_read post timed out after %d ms", timeout_ms);
                return -ETIMEDOUT;
            }
            zc_ofi_wait_after_eagain(ep, &post_spins);
        }
    } while (rc == -FI_EAGAIN);
    if (rc) {
        return zc_ofi_fail(ep, rc, "fi_read");
    }
    return zc_ofi_wait_cq(ep, ep->tx_cq, "tx fi_read cq", &context, 0, NULL, NULL,
                          timeout_ms);
}

int zc_ofi_send(struct zc_ofi_endpoint *ep, const void *buf, size_t len, int timeout_ms) {
    if (!ep || !buf) {
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
    int rc = zc_ofi_register_cached(ep, buf, len, FI_SEND, &ep->send_mr, &desc);
    if (rc) {
        return rc;
    }
    struct fi_context2 context;
    memset(&context, 0, sizeof(context));
    uint64_t post_start = zc_ofi_now_ms();
    uint64_t post_spins = 0;
    do {
        rc = (int)fi_send(ep->ep, buf, len, desc, ep->peer_addr, &context);
        if (rc == -FI_EAGAIN) {
            if (timeout_ms > 0 && zc_ofi_now_ms() - post_start >= (uint64_t)timeout_ms) {
                snprintf(ep->err, sizeof(ep->err), "OFI fi_send post timed out after %d ms", timeout_ms);
                return -ETIMEDOUT;
            }
            zc_ofi_wait_after_eagain(ep, &post_spins);
        }
    } while (rc == -FI_EAGAIN);
    if (rc) {
        return zc_ofi_fail(ep, rc, "fi_send");
    }
    rc = zc_ofi_wait_cq(ep, ep->tx_cq, "tx fi_cq_read", &context, 0, NULL, NULL, timeout_ms);
    return rc;
}

int zc_ofi_send_many(struct zc_ofi_endpoint *ep, const void *base, size_t stride,
                     size_t len, size_t count, int timeout_ms) {
    if (!ep || !base || len == 0) {
        return -FI_EINVAL;
    }
    if (count == 0) {
        return 0;
    }
    int rc = zc_ofi_drain_send(ep, timeout_ms);
    if (rc) {
        return rc;
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
    rc = zc_ofi_register_cached(ep, base, span, FI_SEND, &ep->send_mr, &desc);
    if (rc) {
        return rc;
    }
    struct fi_context2 *contexts = calloc(count, sizeof(*contexts));
    if (!contexts) {
        snprintf(ep->err, sizeof(ep->err), "calloc(send_many contexts) failed");
        return -FI_ENOMEM;
    }
    const char *bytes = (const char *)base;
    size_t posted = 0;
    uint64_t post_start = zc_ofi_now_ms();
    uint64_t post_spins = 0;
    for (; posted < count; posted++) {
        const void *buf = bytes + posted * stride;
        do {
            rc = (int)fi_send(ep->ep, buf, len, desc, ep->peer_addr, &contexts[posted]);
            if (rc == -FI_EAGAIN) {
                if (timeout_ms > 0 && zc_ofi_now_ms() - post_start >= (uint64_t)timeout_ms) {
                    snprintf(ep->err, sizeof(ep->err),
                             "OFI fi_send_many post timed out after %d ms posted=%zu/%zu",
                             timeout_ms, posted, count);
                    free(contexts);
                    return -ETIMEDOUT;
                }
                zc_ofi_wait_after_eagain(ep, &post_spins);
            }
        } while (rc == -FI_EAGAIN);
        if (rc) {
            free(contexts);
            return zc_ofi_fail(ep, rc, "fi_send_many");
        }
    }
    for (size_t i = 0; i < posted; i++) {
        rc = zc_ofi_wait_cq(ep, ep->tx_cq, "tx fi_send_many cq", &contexts[i],
                            0, NULL, NULL, timeout_ms);
        if (rc) {
            free(contexts);
            return rc;
        }
    }
    free(contexts);
    return 0;
}

int zc_ofi_send_many_nowait(struct zc_ofi_endpoint *ep, const void *base, size_t stride,
                            size_t len, size_t count, int timeout_ms) {
    if (!ep || !base || len == 0) {
        return -FI_EINVAL;
    }
    if (count == 0) {
        return 0;
    }
    int rc = zc_ofi_drain_send(ep, timeout_ms);
    if (rc) {
        return rc;
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
    rc = zc_ofi_register_cached(ep, base, span, FI_SEND, &ep->send_mr, &desc);
    if (rc) {
        return rc;
    }
    struct fi_context2 *contexts = calloc(count, sizeof(*contexts));
    if (!contexts) {
        snprintf(ep->err, sizeof(ep->err), "calloc(send_many_nowait contexts) failed");
        return -FI_ENOMEM;
    }
    const char *bytes = (const char *)base;
    size_t posted = 0;
    uint64_t post_start = zc_ofi_now_ms();
    uint64_t post_spins = 0;
    for (; posted < count; posted++) {
        const void *buf = bytes + posted * stride;
        do {
            rc = (int)fi_send(ep->ep, buf, len, desc, ep->peer_addr, &contexts[posted]);
            if (rc == -FI_EAGAIN) {
                if (timeout_ms > 0 && zc_ofi_now_ms() - post_start >= (uint64_t)timeout_ms) {
                    snprintf(ep->err, sizeof(ep->err),
                             "OFI fi_send_many_nowait post timed out after %d ms posted=%zu/%zu",
                             timeout_ms, posted, count);
                    free(contexts);
                    return -ETIMEDOUT;
                }
                zc_ofi_wait_after_eagain(ep, &post_spins);
            }
        } while (rc == -FI_EAGAIN);
        if (rc) {
            free(contexts);
            return zc_ofi_fail(ep, rc, "fi_send_many_nowait");
        }
    }
    ep->async_send_contexts = contexts;
    ep->async_send_context_count = posted;
    return 0;
}

int zc_ofi_drain_send(struct zc_ofi_endpoint *ep, int timeout_ms) {
    if (!ep) {
        return -FI_EINVAL;
    }
    if (ep->async_send_context_count != 0) {
        for (size_t i = 0; i < ep->async_send_context_count; i++) {
            int rc = zc_ofi_wait_cq(ep, ep->tx_cq, "tx fi_send_many_nowait cq",
                                    &ep->async_send_contexts[i], 0, NULL, NULL,
                                    timeout_ms);
            if (rc) {
                return rc;
            }
        }
        free(ep->async_send_contexts);
        ep->async_send_contexts = NULL;
        ep->async_send_context_count = 0;
    }
    if (!ep->async_send_pending) {
        return 0;
    }
    int rc = zc_ofi_wait_cq(ep, ep->tx_cq, "tx fi_cq_read", &ep->async_send_context,
                            0, NULL, NULL, timeout_ms);
    if (!rc) {
        ep->async_send_pending = 0;
    }
    return rc;
}

int zc_ofi_send_nowait(struct zc_ofi_endpoint *ep, const void *buf, size_t len, int timeout_ms) {
    if (!ep || !buf) {
        return -FI_EINVAL;
    }
    int rc = zc_ofi_drain_send(ep, timeout_ms);
    if (rc) {
        return rc;
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
    rc = zc_ofi_register_cached(ep, buf, len, FI_SEND, &ep->send_mr, &desc);
    if (rc) {
        return rc;
    }
    memset(&ep->async_send_context, 0, sizeof(ep->async_send_context));
    uint64_t post_start = zc_ofi_now_ms();
    uint64_t post_spins = 0;
    do {
        rc = (int)fi_send(ep->ep, buf, len, desc, ep->peer_addr, &ep->async_send_context);
        if (rc == -FI_EAGAIN) {
            if (timeout_ms > 0 && zc_ofi_now_ms() - post_start >= (uint64_t)timeout_ms) {
                snprintf(ep->err, sizeof(ep->err), "OFI fi_send post timed out after %d ms", timeout_ms);
                return -ETIMEDOUT;
            }
            zc_ofi_wait_after_eagain(ep, &post_spins);
        }
    } while (rc == -FI_EAGAIN);
    if (rc) {
        return zc_ofi_fail(ep, rc, "fi_send_nowait");
    }
    ep->async_send_pending = 1;
    return 0;
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

int zc_ofi_recv(struct zc_ofi_endpoint *ep, void *buf, size_t cap, size_t *out_len,
                int timeout_ms) {
    if (!ep || !buf || !out_len) {
        return -FI_EINVAL;
    }
    void *desc = NULL;
    int rc = zc_ofi_register_cached(ep, buf, cap, FI_RECV, &ep->recv_mr, &desc);
    if (rc) {
        return rc;
    }
    struct fi_context2 context;
    memset(&context, 0, sizeof(context));
    uint64_t post_start = zc_ofi_now_ms();
    uint64_t post_spins = 0;
    do {
        rc = (int)fi_recv(ep->ep, buf, cap, desc, FI_ADDR_UNSPEC, &context);
        if (rc == -FI_EAGAIN) {
            if (timeout_ms > 0 && zc_ofi_now_ms() - post_start >= (uint64_t)timeout_ms) {
                snprintf(ep->err, sizeof(ep->err), "OFI fi_recv post timed out after %d ms", timeout_ms);
                return -ETIMEDOUT;
            }
            zc_ofi_wait_after_eagain(ep, &post_spins);
        }
    } while (rc == -FI_EAGAIN);
    if (rc) {
        return zc_ofi_fail(ep, rc, "fi_recv");
    }
    size_t len = 0;
    fi_addr_t src = FI_ADDR_UNSPEC;
    rc = zc_ofi_wait_cq(ep, ep->rx_cq, "rx fi_cq_read", &context, 1, &src, &len, timeout_ms);
    if (rc) {
        return rc;
    }
    ep->last_src_addr = src;
    *out_len = len;
    return 0;
}

int zc_ofi_recv_start(struct zc_ofi_endpoint *ep, void *buf, size_t cap, int timeout_ms) {
    if (!ep || !buf) {
        return -FI_EINVAL;
    }
    if (ep->async_recv_pending) {
        snprintf(ep->err, sizeof(ep->err), "OFI async recv is already pending");
        return -FI_EBUSY;
    }
    void *desc = NULL;
    int rc = zc_ofi_register_cached(ep, buf, cap, FI_RECV, &ep->recv_mr, &desc);
    if (rc) {
        return rc;
    }
    memset(&ep->async_recv_context, 0, sizeof(ep->async_recv_context));
    uint64_t post_start = zc_ofi_now_ms();
    uint64_t post_spins = 0;
    do {
        rc = (int)fi_recv(ep->ep, buf, cap, desc, FI_ADDR_UNSPEC, &ep->async_recv_context);
        if (rc == -FI_EAGAIN) {
            if (timeout_ms > 0 && zc_ofi_now_ms() - post_start >= (uint64_t)timeout_ms) {
                snprintf(ep->err, sizeof(ep->err), "OFI fi_recv post timed out after %d ms", timeout_ms);
                return -ETIMEDOUT;
            }
            zc_ofi_wait_after_eagain(ep, &post_spins);
        }
    } while (rc == -FI_EAGAIN);
    if (rc) {
        return zc_ofi_fail(ep, rc, "fi_recv_start");
    }
    ep->async_recv_pending = 1;
    return 0;
}

int zc_ofi_recv_finish(struct zc_ofi_endpoint *ep, size_t *out_len, int timeout_ms) {
    if (!ep || !out_len) {
        return -FI_EINVAL;
    }
    if (!ep->async_recv_pending) {
        snprintf(ep->err, sizeof(ep->err), "OFI async recv is not pending");
        return -FI_EINVAL;
    }
    size_t len = 0;
    fi_addr_t src = FI_ADDR_UNSPEC;
    int rc = zc_ofi_wait_cq(ep, ep->rx_cq, "rx fi_cq_read", &ep->async_recv_context,
                            1, &src, &len, timeout_ms);
    if (rc) {
        return rc;
    }
    ep->async_recv_pending = 0;
    ep->last_src_addr = src;
    *out_len = len;
    return 0;
}
