//! RDMA payload transport for the serial userspace custody stage. TCP carries
//! descriptors and durability receipts; no application payload traverses TCP.
//! One registered pool survives for the lane lifetime. Slot reuse is gated by
//! BOTH remote durable frontiers, never by an early client acknowledgement.

use crate::*;
use serde::Deserialize;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Config {
    pub provider: String,
    pub domain: Option<String>,
}

impl Config {
    /// Comma-separated domains are assigned round-robin at lane admission;
    /// this selection is never performed while submitting an I/O.
    pub fn domain_for_lane(&self, lane: usize) -> Option<&str> {
        let value = self.domain.as_deref()?;
        value
            .split(',')
            .nth(lane % value.split(',').count())
            .map(str::trim)
    }
    pub fn from_env() -> io::Result<Option<Self>> {
        env::var_os("URING_PLAY_RAID_MIRROR_RDMA_CONFIG")
            .map(|path| serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other))
            .transpose()
    }
}

pub(super) struct Pool {
    storage: FixedSendBuffers,
    pub slots: usize,
    pub window: usize,
    pub extent_bytes: usize,
    pub bytes: usize,
}
// SAFETY: the pool exposes no safe mutation. Receive/write permissions belong
// to protocol slots. A peer cannot reuse a slot before both consumers release
// it. As with all RDMA transports, memory keys are only granted to trusted peers.
unsafe impl Send for Pool {}
unsafe impl Sync for Pool {}

impl Pool {
    pub fn new(slots: usize, window: usize, extent_bytes: usize) -> io::Result<Arc<Self>> {
        let bytes = slots
            .checked_mul(window)
            .and_then(|v| v.checked_mul(extent_bytes))
            .filter(|&v| v != 0 && v <= 1024 * 1024 * 1024)
            .ok_or_else(|| io::Error::other("invalid RDMA custody pool geometry"))?;
        let storage = if env_enabled_or("URING_PLAY_HUGETLB", false) {
            FixedSendBuffers::new_hugetlb(1, bytes)?
        } else {
            FixedSendBuffers::new(1, bytes)?
        };
        Ok(Arc::new(Self {
            storage,
            slots,
            window,
            extent_bytes,
            bytes,
        }))
    }
    pub fn ptr(&self) -> *mut u8 {
        self.storage.ptr(0)
    }
    pub fn slot(&self, sequence: usize) -> usize {
        sequence / self.window % self.slots
    }
    /// Caller has received a delivery-complete doorbell for this slot and
    /// retains its reuse credit until every local/NIC consumer has finished.
    pub unsafe fn received(&self, sequence: usize, count: usize) -> &[u8] {
        assert!(count <= self.window);
        let offset = self.slot(sequence) * self.window * self.extent_bytes;
        unsafe { slice::from_raw_parts(self.ptr().add(offset), count * self.extent_bytes) }
    }
}

fn exchange(
    stream: &mut TcpStream,
    ep: &mut ZcOfiEndpoint,
    contract: &str,
    server: bool,
) -> io::Result<()> {
    let local = ep.local_name()?;
    let profile = ep.profile()?;
    let (remote, remote_profile, remote_contract) = if server {
        zcofi_write_addr(stream, &local)?;
        zcofi_write_control_text(stream, "peer profile", &profile)?;
        zcofi_write_control_text(stream, "wire contract", contract)?;
        (
            zcofi_read_addr(stream)?,
            zcofi_read_control_text(stream, "peer profile")?,
            zcofi_read_control_text(stream, "wire contract")?,
        )
    } else {
        let fields = (
            zcofi_read_addr(stream)?,
            zcofi_read_control_text(stream, "peer profile")?,
            zcofi_read_control_text(stream, "wire contract")?,
        );
        zcofi_write_addr(stream, &local)?;
        zcofi_write_control_text(stream, "peer profile", &profile)?;
        zcofi_write_control_text(stream, "wire contract", contract)?;
        fields
    };
    zcofi_validate_peer_profile(&profile, &remote_profile, contract, &remote_contract)?;
    ep.set_peer(&remote)?;
    eprintln!(
        "rdma-custody-profile: local={profile} remote={remote_profile} payload_transport=fi-rma-write control_transport=tcp payload_tcp_bytes=0"
    );
    Ok(())
}

fn contract(lane: usize, window: usize, extent: usize) -> String {
    format!(
        "zc-custody-rma-v1;lane={lane};window={window};extent={extent};payload=fi-rma-write;control=tcp;reuse=both-consumers"
    )
}

pub(super) struct Receiver {
    // Drop the MR/endpoint before the backing mapping.
    _endpoint: ZcOfiEndpoint,
    pub pool: Arc<Pool>,
}

impl Receiver {
    pub fn accept(
        stream: &mut TcpStream,
        config: &Config,
        lane: usize,
        pool: Arc<Pool>,
    ) -> io::Result<Self> {
        let addr = stream.local_addr()?;
        eprintln!(
            "rdma-custody-lane: role=receiver lane={lane} cpu={} provider={} domain={} pool_bytes={} slots={} per_lane_qd={}",
            current_cpu(),
            config.provider,
            config.domain_for_lane(lane).unwrap_or("implicit"),
            pool.bytes,
            pool.slots,
            pool.window
        );
        let mut ep = ZcOfiEndpoint::open_rma_on_domain(
            &config.provider,
            "rdm",
            &addr.ip().to_string(),
            "0",
            true,
            config.domain_for_lane(lane),
        )?;
        exchange(
            stream,
            &mut ep,
            &contract(lane, pool.window, pool.extent_bytes),
            true,
        )?;
        zcofi_rma_server_connection_warmup(&mut ep, lane, &config.provider)?;
        let (remote_addr, remote_key) =
            unsafe { ep.rma_register_target_raw(pool.ptr(), pool.bytes)? };
        let meta = ZcOfiRmaMeta {
            lane_id: lane as u32,
            lane_count: 1,
            bytes_per_lane: pool.bytes as u64,
            extent_bytes: pool.extent_bytes as u64,
            remote_addr,
            remote_key,
        };
        stream.write_all(&meta.encode())?;
        Ok(Self {
            _endpoint: ep,
            pool,
        })
    }
}

pub(super) struct Sender {
    endpoint: ZcOfiEndpoint,
    // Some for a relay forwarding its NIC receive mapping. The source arena
    // caller instead owns its memory for longer than this Sender.
    _pool_lease: Option<Arc<Pool>>,
    remote: ZcOfiRmaMeta,
    pub remote_slots: usize,
    window: usize,
    extent_bytes: usize,
    slots: Vec<usize>,
    tokens: Vec<u64>,
    completed: Vec<bool>,
}

impl Sender {
    /// Source memory must remain live until this endpoint has been dropped.
    /// Submitted ranges must be immutable until their local TX CQ completion.
    pub unsafe fn connect(
        stream: &mut TcpStream,
        config: &Config,
        lane: usize,
        window: usize,
        extent_bytes: usize,
        source: *const u8,
        source_bytes: usize,
        pool_lease: Option<Arc<Pool>>,
    ) -> io::Result<Self> {
        if !env_enabled_or("URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE", true) {
            return Err(io::Error::other(
                "RDMA custody requires remote delivery completion before a doorbell",
            ));
        }
        let peer = stream.peer_addr()?;
        eprintln!(
            "rdma-custody-lane: role=sender lane={lane} cpu={} provider={} domain={} registered_source_bytes={source_bytes} per_lane_qd={window}",
            current_cpu(),
            config.provider,
            config.domain_for_lane(lane).unwrap_or("implicit")
        );
        let mut ep = ZcOfiEndpoint::open_rma_on_domain(
            &config.provider,
            "rdm",
            &peer.ip().to_string(),
            "0",
            false,
            config.domain_for_lane(lane),
        )?;
        exchange(
            stream,
            &mut ep,
            &contract(lane, window, extent_bytes),
            false,
        )?;
        zcofi_rma_client_connection_warmup(&mut ep, lane, &config.provider)?;
        let mut bytes = [0; ZCOFI_RMA_CONTROL_LEN];
        stream.read_exact(&mut bytes)?;
        let remote = ZcOfiRmaMeta::decode(&bytes)?;
        let window_bytes = window
            .checked_mul(extent_bytes)
            .filter(|&n| n != 0)
            .ok_or_else(|| io::Error::other("invalid RDMA window"))?;
        if remote.lane_id as usize != lane
            || remote.extent_bytes != extent_bytes as u64
            || remote.bytes_per_lane < window_bytes as u64
            || remote.bytes_per_lane % window_bytes as u64 != 0
            || remote.bytes_per_lane > 1024 * 1024 * 1024
        {
            return Err(io::Error::other("RDMA pool metadata mismatch"));
        }
        unsafe {
            ep.rma_register_write_buffer_raw(source, source_bytes)?;
        }
        ep.rma_write_queue_init(window)?;
        let remote_slots = remote.bytes_per_lane as usize / window_bytes;
        Ok(Self {
            endpoint: ep,
            _pool_lease: pool_lease,
            remote,
            remote_slots,
            window,
            extent_bytes,
            slots: vec![0; window],
            tokens: vec![0; window],
            completed: vec![false; window],
        })
    }

    /// All ranges borrow already registered storage. `repeated` is the benchmark
    /// source's constant payload; a relay uses contiguous distinct RX records.
    pub unsafe fn write_window(
        &mut self,
        sequence: usize,
        count: usize,
        source: *const u8,
        repeated: bool,
    ) -> io::Result<()> {
        if count == 0 || count > self.window || sequence % self.window != 0 {
            return Err(io::Error::other("invalid RDMA custody window"));
        }
        let base = (sequence / self.window % self.remote_slots) * self.window * self.extent_bytes;
        self.completed[..count].fill(false);
        for slot in 0..count {
            let ptr = if repeated {
                source
            } else {
                unsafe { source.add(slot * self.extent_bytes) }
            };
            let remote = self
                .remote
                .remote_addr
                .checked_add((base + slot * self.extent_bytes) as u64)
                .ok_or_else(|| io::Error::other("RDMA address overflow"))?;
            let posted = unsafe {
                self.endpoint.rma_write_post_more_raw(
                    ptr,
                    self.extent_bytes,
                    remote,
                    self.remote.remote_key,
                    slot,
                    (sequence + slot) as u64,
                    slot + 1 != count,
                )?
            };
            if !posted {
                return Err(io::Error::other("RDMA custody queue exhausted"));
            }
        }
        let mut pending = count;
        while pending != 0 {
            let completed = self.endpoint.rma_write_poll(
                &mut self.slots[..count],
                &mut self.tokens[..count],
                true,
            )?;
            for index in 0..completed {
                let slot = self.slots[index];
                if slot >= count
                    || self.tokens[index] != (sequence + slot) as u64
                    || self.completed[slot]
                {
                    return Err(io::Error::other("stale RDMA custody completion"));
                }
                self.completed[slot] = true;
            }
            pending = pending
                .checked_sub(completed)
                .ok_or_else(|| io::Error::other("excess RDMA completions"))?;
        }
        Ok(())
    }
}
