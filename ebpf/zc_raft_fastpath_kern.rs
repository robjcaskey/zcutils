#![no_std]
#![no_main]

use core::ffi::c_void;
use core::mem;
use core::ptr;

const BPF_MAP_TYPE_ARRAY: u32 = 2;
const BPF_MAP_TYPE_PERCPU_ARRAY: u32 = 6;
const BPF_MAP_TYPE_CPUMAP: u32 = 16;

const LIBBPF_PIN_BY_NAME: usize = 1;
const TC_ACT_OK: i32 = 0;
const TC_ACT_SHOT: i32 = 2;
const XDP_PASS: i32 = 2;
const XDP_DROP: i32 = 1;

const ETH_HLEN: u32 = 14;
const ETH_P_IP: u16 = 0x0800;
const ETH_P_8021Q: u16 = 0x8100;
const ETH_P_8021AD: u16 = 0x88a8;
const IPPROTO_TCP: u8 = 6;
const IPPROTO_UDP: u8 = 17;

const ZC_RAFT_MAX_PORTS: usize = 8;
const ZC_RAFT_MAX_SHARDS: u32 = 256;
const ZC_RAFT_STAT_MAX: u32 = 16;

const ZC_RAFT_POLICY_F_STRICT_DROP: u32 = 1 << 0;
const ZC_RAFT_POLICY_F_SET_MARK: u32 = 1 << 1;
const ZC_RAFT_POLICY_F_SHARD_INDEX: u32 = 1 << 2;
const ZC_RAFT_POLICY_F_XDP_CPUMAP: u32 = 1 << 3;

const ZC_RAFT_STAT_TOTAL: u32 = 0;
const ZC_RAFT_STAT_TCP: u32 = 1;
const ZC_RAFT_STAT_UDP: u32 = 2;
const ZC_RAFT_STAT_RAFT_PORT: u32 = 3;
const ZC_RAFT_STAT_APPEND: u32 = 4;
const ZC_RAFT_STAT_ACK: u32 = 5;
const ZC_RAFT_STAT_UNKNOWN_MAGIC: u32 = 6;
const ZC_RAFT_STAT_DROP: u32 = 7;

const BPF_FUNC_MAP_LOOKUP_ELEM: usize = 1;
const BPF_FUNC_SKB_LOAD_BYTES: usize = 26;
const BPF_FUNC_REDIRECT_MAP: usize = 51;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SkBuff {
    len: u32,
    pkt_type: u32,
    mark: u32,
    queue_mapping: u32,
    protocol: u32,
    vlan_present: u32,
    vlan_tci: u32,
    vlan_proto: u32,
    priority: u32,
    ingress_ifindex: u32,
    ifindex: u32,
    tc_index: u32,
    cb: [u32; 5],
    hash: u32,
    tc_classid: u32,
    data: u32,
    data_end: u32,
    napi_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct XdpMd {
    data: u32,
    data_end: u32,
    data_meta: u32,
    ingress_ifindex: u32,
    rx_queue_index: u32,
    egress_ifindex: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ZcRaftPolicy {
    flags: u32,
    shard_count: u32,
    mark_base: u32,
    ports: [u32; ZC_RAFT_MAX_PORTS],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ZcRaftCounter {
    packets: u64,
    bytes: u64,
    appends: u64,
    acks: u64,
    unknown_magic: u64,
    drops: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ZcPkt {
    saddr: u32,
    daddr: u32,
    payload_off: u32,
    payload_len: u32,
    sport: u16,
    dport: u16,
    proto: u8,
}

#[repr(C)]
struct ZcClassifyIn {
    pkt: *const ZcPkt,
    bytes: u64,
    magic: u64,
    index: u64,
    skb_hash: u32,
    have_magic: u32,
}

#[repr(C)]
struct ZcRaftPolicyMap {
    r#type: *const [i32; BPF_MAP_TYPE_ARRAY as usize],
    max_entries: *const [i32; 1],
    key: *const u32,
    value: *const ZcRaftPolicy,
    pinning: *const [i32; LIBBPF_PIN_BY_NAME],
}

#[repr(C)]
struct ZcRaftStatsMap {
    r#type: *const [i32; BPF_MAP_TYPE_PERCPU_ARRAY as usize],
    max_entries: *const [i32; ZC_RAFT_STAT_MAX as usize],
    key: *const u32,
    value: *const ZcRaftCounter,
    pinning: *const [i32; LIBBPF_PIN_BY_NAME],
}

#[repr(C)]
struct ZcRaftShardsMap {
    r#type: *const [i32; BPF_MAP_TYPE_PERCPU_ARRAY as usize],
    max_entries: *const [i32; ZC_RAFT_MAX_SHARDS as usize],
    key: *const u32,
    value: *const ZcRaftCounter,
    pinning: *const [i32; LIBBPF_PIN_BY_NAME],
}

#[repr(C)]
struct ZcRaftCpuMap {
    r#type: *const [i32; BPF_MAP_TYPE_CPUMAP as usize],
    max_entries: *const [i32; ZC_RAFT_MAX_SHARDS as usize],
    key: *const u32,
    value: *const u32,
    pinning: *const [i32; LIBBPF_PIN_BY_NAME],
}

unsafe impl Sync for ZcRaftPolicyMap {}
unsafe impl Sync for ZcRaftStatsMap {}
unsafe impl Sync for ZcRaftShardsMap {}
unsafe impl Sync for ZcRaftCpuMap {}

#[no_mangle]
#[link_section = ".maps"]
static mut zc_raft_policy: ZcRaftPolicyMap = ZcRaftPolicyMap {
    r#type: ptr::null(),
    max_entries: ptr::null(),
    key: ptr::null(),
    value: ptr::null(),
    pinning: ptr::null(),
};

#[no_mangle]
#[link_section = ".maps"]
static mut zc_raft_stats: ZcRaftStatsMap = ZcRaftStatsMap {
    r#type: ptr::null(),
    max_entries: ptr::null(),
    key: ptr::null(),
    value: ptr::null(),
    pinning: ptr::null(),
};

#[no_mangle]
#[link_section = ".maps"]
static mut zc_raft_shards: ZcRaftShardsMap = ZcRaftShardsMap {
    r#type: ptr::null(),
    max_entries: ptr::null(),
    key: ptr::null(),
    value: ptr::null(),
    pinning: ptr::null(),
};

#[no_mangle]
#[link_section = ".maps"]
static mut zc_raft_cpu: ZcRaftCpuMap =
    ZcRaftCpuMap {
        r#type: ptr::null(),
        max_entries: ptr::null(),
        key: ptr::null(),
        value: ptr::null(),
        pinning: ptr::null(),
};

#[no_mangle]
#[link_section = "license"]
#[used]
static _license: [u8; 13] = *b"Dual MIT/GPL\0";

#[inline(always)]
unsafe fn bpf_map_lookup_elem<T>(map: *const c_void, key: *const u32) -> *mut T {
    let helper: extern "C" fn(*const c_void, *const c_void) -> *mut c_void =
        mem::transmute(BPF_FUNC_MAP_LOOKUP_ELEM);
    helper(map.cast(), key.cast()).cast()
}

#[inline(always)]
unsafe fn bpf_skb_load_bytes(skb: *const SkBuff, off: u32, to: *mut c_void, len: u32) -> i64 {
    let helper: extern "C" fn(*const SkBuff, u32, *mut c_void, u32) -> i64 =
        mem::transmute(BPF_FUNC_SKB_LOAD_BYTES);
    helper(skb, off, to, len)
}

#[inline(always)]
unsafe fn bpf_redirect_map(map: *const c_void, key: u32, flags: u64) -> i64 {
    let helper: extern "C" fn(*const c_void, u32, u64) -> i64 =
        mem::transmute(BPF_FUNC_REDIRECT_MAP);
    helper(map.cast(), key, flags)
}

#[inline(always)]
fn be16(buf: [u8; 2]) -> u16 {
    u16::from_be_bytes(buf)
}

#[inline(always)]
fn be32(buf: [u8; 4]) -> u32 {
    u32::from_be_bytes(buf)
}

#[inline(always)]
fn default_raft_port(port: u16) -> bool {
    port == 19401 || port == 9100 || port == 9200
}

#[inline(always)]
unsafe fn policy_lookup() -> *mut ZcRaftPolicy {
    let key = 0u32;
    bpf_map_lookup_elem(ptr::addr_of!(zc_raft_policy).cast(), &key)
}

#[inline(always)]
unsafe fn policy_is_configured(policy: *const ZcRaftPolicy) -> bool {
    if policy.is_null() {
        return false;
    }
    if (*policy).flags != 0 || (*policy).shard_count != 0 || (*policy).mark_base != 0 {
        return true;
    }
    let mut i = 0usize;
    while i < ZC_RAFT_MAX_PORTS {
        if (*policy).ports[i] != 0 {
            return true;
        }
        i += 1;
    }
    false
}

#[inline(always)]
unsafe fn port_matches_policy(policy: *const ZcRaftPolicy, sport: u16, dport: u16) -> bool {
    if !policy_is_configured(policy) {
        return default_raft_port(sport) || default_raft_port(dport);
    }
    let mut i = 0usize;
    while i < ZC_RAFT_MAX_PORTS {
        let port = (*policy).ports[i] as u16;
        if port != 0 && (sport == port || dport == port) {
            return true;
        }
        i += 1;
    }
    false
}

#[inline(always)]
unsafe fn policy_shards(policy: *const ZcRaftPolicy) -> u32 {
    let mut shards = if policy.is_null() || (*policy).shard_count == 0 {
        64
    } else {
        (*policy).shard_count
    };
    if shards > ZC_RAFT_MAX_SHARDS {
        shards = ZC_RAFT_MAX_SHARDS;
    }
    if shards == 0 {
        shards = 1;
    }
    shards
}

#[inline(always)]
unsafe fn bump_stat(
    key: u32,
    bytes: u64,
    is_append: u32,
    is_ack: u32,
    unknown_magic: u32,
    dropped: u32,
) {
    let counter: *mut ZcRaftCounter =
        bpf_map_lookup_elem(ptr::addr_of!(zc_raft_stats).cast(), &key);
    if counter.is_null() {
        return;
    }
    (*counter).packets = (*counter).packets.wrapping_add(1);
    (*counter).bytes = (*counter).bytes.wrapping_add(bytes);
    (*counter).appends = (*counter).appends.wrapping_add(is_append as u64);
    (*counter).acks = (*counter).acks.wrapping_add(is_ack as u64);
    (*counter).unknown_magic = (*counter)
        .unknown_magic
        .wrapping_add(unknown_magic as u64);
    (*counter).drops = (*counter).drops.wrapping_add(dropped as u64);
}

#[inline(always)]
unsafe fn bump_shard(
    shard: u32,
    bytes: u64,
    is_append: u32,
    is_ack: u32,
    unknown_magic: u32,
    dropped: u32,
) {
    if shard >= ZC_RAFT_MAX_SHARDS {
        return;
    }
    let counter: *mut ZcRaftCounter =
        bpf_map_lookup_elem(ptr::addr_of!(zc_raft_shards).cast(), &shard);
    if counter.is_null() {
        return;
    }
    (*counter).packets = (*counter).packets.wrapping_add(1);
    (*counter).bytes = (*counter).bytes.wrapping_add(bytes);
    (*counter).appends = (*counter).appends.wrapping_add(is_append as u64);
    (*counter).acks = (*counter).acks.wrapping_add(is_ack as u64);
    (*counter).unknown_magic = (*counter)
        .unknown_magic
        .wrapping_add(unknown_magic as u64);
    (*counter).drops = (*counter).drops.wrapping_add(dropped as u64);
}

#[inline(always)]
unsafe fn flow_hash(pkt: *const ZcPkt, skb_hash: u32) -> u32 {
    let mut hash = skb_hash;
    if hash == 0 {
        hash = (*pkt).saddr
            ^ (*pkt).daddr
            ^ ((*pkt).sport as u32) << 16
            ^ ((*pkt).dport as u32);
    }
    hash ^= hash >> 16;
    hash = hash.wrapping_mul(0x7feb_352d);
    hash ^= hash >> 15;
    hash = hash.wrapping_mul(0x846c_a68b);
    hash ^ (hash >> 16)
}

#[inline(always)]
fn index_hash(index: u64) -> u32 {
    let mut hash = (index as u32) ^ ((index >> 32) as u32);
    hash ^= hash >> 16;
    hash = hash.wrapping_mul(0x7feb_352d);
    hash ^ (hash >> 15)
}

#[inline(always)]
const fn magic64(bytes: &[u8; 8]) -> u64 {
    (bytes[0] as u64)
        | ((bytes[1] as u64) << 8)
        | ((bytes[2] as u64) << 16)
        | ((bytes[3] as u64) << 24)
        | ((bytes[4] as u64) << 32)
        | ((bytes[5] as u64) << 40)
        | ((bytes[6] as u64) << 48)
        | ((bytes[7] as u64) << 56)
}

#[inline(always)]
fn magic_is_append(magic: u64) -> bool {
    magic == magic64(b"URFTAE01") || magic == magic64(b"RSLTAP01")
}

#[inline(always)]
fn magic_is_ack(magic: u64) -> bool {
    magic == magic64(b"URFTACK1") || magic == magic64(b"RSLTAC01")
}

#[inline(always)]
unsafe fn parse_skb_l4(skb: *const SkBuff, pkt: *mut ZcPkt) -> bool {
    let mut eth = [0u8; 14];
    if bpf_skb_load_bytes(skb, 0, eth.as_mut_ptr().cast(), eth.len() as u32) < 0 {
        return false;
    }
    let mut off = ETH_HLEN;
    let mut eth_proto = be16([eth[12], eth[13]]);

    let mut vlan_count = 0;
    while vlan_count < 2 {
        if eth_proto != ETH_P_8021Q && eth_proto != ETH_P_8021AD {
            break;
        }
        let mut vlan = [0u8; 4];
        if bpf_skb_load_bytes(skb, off, vlan.as_mut_ptr().cast(), vlan.len() as u32) < 0 {
            return false;
        }
        off += 4;
        eth_proto = be16([vlan[2], vlan[3]]);
        vlan_count += 1;
    }

    if eth_proto != ETH_P_IP {
        return false;
    }

    let mut ip = [0u8; 20];
    if bpf_skb_load_bytes(skb, off, ip.as_mut_ptr().cast(), ip.len() as u32) < 0 {
        return false;
    }
    let ihl = ((ip[0] & 0x0f) as u32) * 4;
    if ip[0] >> 4 != 4 || ihl < 20 {
        return false;
    }

    (*pkt).saddr = be32([ip[12], ip[13], ip[14], ip[15]]);
    (*pkt).daddr = be32([ip[16], ip[17], ip[18], ip[19]]);
    (*pkt).proto = ip[9];
    off += ihl;

    if (*pkt).proto == IPPROTO_TCP {
        let mut tcp = [0u8; 20];
        if bpf_skb_load_bytes(skb, off, tcp.as_mut_ptr().cast(), tcp.len() as u32) < 0 {
            return false;
        }
        let doff = ((tcp[12] >> 4) as u32) * 4;
        if doff < 20 {
            return false;
        }
        (*pkt).sport = be16([tcp[0], tcp[1]]);
        (*pkt).dport = be16([tcp[2], tcp[3]]);
        off += doff;
    } else if (*pkt).proto == IPPROTO_UDP {
        let mut udp = [0u8; 8];
        if bpf_skb_load_bytes(skb, off, udp.as_mut_ptr().cast(), udp.len() as u32) < 0 {
            return false;
        }
        (*pkt).sport = be16([udp[0], udp[1]]);
        (*pkt).dport = be16([udp[2], udp[3]]);
        off += 8;
    } else {
        return false;
    }

    if off > (*skb).len {
        return false;
    }
    (*pkt).payload_off = off;
    (*pkt).payload_len = (*skb).len - off;
    true
}

#[inline(always)]
unsafe fn classify_common(input: *const ZcClassifyIn, policy: *const ZcRaftPolicy, shard_out: *mut u32) -> bool {
    let pkt = (*input).pkt;
    let is_append = ((*input).have_magic != 0 && magic_is_append((*input).magic)) as u32;
    let is_ack = ((*input).have_magic != 0 && magic_is_ack((*input).magic)) as u32;
    let unknown_magic =
        ((*input).have_magic != 0 && is_append == 0 && is_ack == 0) as u32;
    let mut hash = flow_hash(pkt, (*input).skb_hash);
    let shards = policy_shards(policy);
    let mut dropped = 0u32;

    if !policy.is_null()
        && ((*policy).flags & ZC_RAFT_POLICY_F_SHARD_INDEX) != 0
        && (is_append != 0 || is_ack != 0)
    {
        hash = index_hash((*input).index);
    }

    let shard = hash % shards;
    *shard_out = shard;

    bump_stat(
        ZC_RAFT_STAT_TOTAL,
        (*input).bytes,
        is_append,
        is_ack,
        unknown_magic,
        0,
    );
    if (*pkt).proto == IPPROTO_TCP {
        bump_stat(
            ZC_RAFT_STAT_TCP,
            (*input).bytes,
            is_append,
            is_ack,
            unknown_magic,
            0,
        );
    } else if (*pkt).proto == IPPROTO_UDP {
        bump_stat(
            ZC_RAFT_STAT_UDP,
            (*input).bytes,
            is_append,
            is_ack,
            unknown_magic,
            0,
        );
    }

    bump_stat(
        ZC_RAFT_STAT_RAFT_PORT,
        (*input).bytes,
        is_append,
        is_ack,
        unknown_magic,
        0,
    );
    if is_append != 0 {
        bump_stat(ZC_RAFT_STAT_APPEND, (*input).bytes, 1, 0, 0, 0);
    } else if is_ack != 0 {
        bump_stat(ZC_RAFT_STAT_ACK, (*input).bytes, 0, 1, 0, 0);
    } else if unknown_magic != 0 {
        bump_stat(
            ZC_RAFT_STAT_UNKNOWN_MAGIC,
            (*input).bytes,
            0,
            0,
            1,
            0,
        );
    }

    if unknown_magic != 0
        && !policy.is_null()
        && ((*policy).flags & ZC_RAFT_POLICY_F_STRICT_DROP) != 0
    {
        dropped = 1;
        bump_stat(
            ZC_RAFT_STAT_DROP,
            (*input).bytes,
            is_append,
            is_ack,
            unknown_magic,
            1,
        );
    }

    bump_shard(
        shard,
        (*input).bytes,
        is_append,
        is_ack,
        unknown_magic,
        dropped,
    );
    dropped != 0
}

#[no_mangle]
#[link_section = "classifier"]
pub extern "C" fn zc_raft_tc(skb: *mut SkBuff) -> i32 {
    unsafe {
        let mut pkt = ZcPkt {
            saddr: 0,
            daddr: 0,
            payload_off: 0,
            payload_len: 0,
            sport: 0,
            dport: 0,
            proto: 0,
        };
        let policy = policy_lookup();
        let mut magic = 0u64;
        let mut index = 0u64;
        let mut have_magic = 0u32;
        let mut shard = 0u32;

        if !parse_skb_l4(skb, &mut pkt) {
            return TC_ACT_OK;
        }
        if !port_matches_policy(policy, pkt.sport, pkt.dport) {
            return TC_ACT_OK;
        }

        if pkt.payload_len >= 8
            && bpf_skb_load_bytes(
                skb,
                pkt.payload_off,
                (&mut magic as *mut u64).cast(),
                8,
            ) == 0
        {
            have_magic = 1;
            if magic_is_append(magic) && pkt.payload_len >= 24 {
                let _ = bpf_skb_load_bytes(
                    skb,
                    pkt.payload_off + 16,
                    (&mut index as *mut u64).cast(),
                    8,
                );
            } else if magic == magic64(b"URFTACK1") && pkt.payload_len >= 16 {
                let _ = bpf_skb_load_bytes(
                    skb,
                    pkt.payload_off + 8,
                    (&mut index as *mut u64).cast(),
                    8,
                );
            } else if magic == magic64(b"RSLTAC01") && pkt.payload_len >= 24 {
                let _ = bpf_skb_load_bytes(
                    skb,
                    pkt.payload_off + 16,
                    (&mut index as *mut u64).cast(),
                    8,
                );
            }
            index = u64::from_be(index);
        }

        let input = ZcClassifyIn {
            pkt: &pkt,
            bytes: (*skb).len as u64,
            magic,
            index,
            skb_hash: (*skb).hash,
            have_magic,
        };
        if classify_common(&input, policy, &mut shard) {
            return TC_ACT_SHOT;
        }

        if !policy.is_null() && ((*policy).flags & ZC_RAFT_POLICY_F_SET_MARK) != 0 {
            let mark = (*policy).mark_base.wrapping_add(shard);
            (*skb).mark = mark;
            (*skb).priority = mark;
        }

        TC_ACT_OK
    }
}

#[inline(always)]
unsafe fn xdp_read_bytes<const N: usize>(base: *const u8, end: *const u8, off: u32, out: *mut [u8; N]) -> bool {
    let ptr = base.add(off as usize);
    let next = ptr.add(N);
    if next > end {
        return false;
    }
    ptr::copy_nonoverlapping(ptr, (*out).as_mut_ptr(), N);
    true
}

#[inline(always)]
unsafe fn parse_xdp_l4(ctx: *mut XdpMd, pkt: *mut ZcPkt) -> bool {
    let data = (*ctx).data as usize as *const u8;
    let data_end = (*ctx).data_end as usize as *const u8;
    let mut eth = [0u8; 14];
    let mut off = ETH_HLEN;

    if !xdp_read_bytes::<14>(data, data_end, 0, &mut eth) {
        return false;
    }
    let mut eth_proto = be16([eth[12], eth[13]]);

    let mut vlan_count = 0;
    while vlan_count < 2 {
        if eth_proto != ETH_P_8021Q && eth_proto != ETH_P_8021AD {
            break;
        }
        let mut vlan = [0u8; 4];
        if !xdp_read_bytes::<4>(data, data_end, off, &mut vlan) {
            return false;
        }
        off += 4;
        eth_proto = be16([vlan[2], vlan[3]]);
        vlan_count += 1;
    }

    if eth_proto != ETH_P_IP {
        return false;
    }

    let mut ip = [0u8; 20];
    if !xdp_read_bytes::<20>(data, data_end, off, &mut ip) {
        return false;
    }
    let ihl = ((ip[0] & 0x0f) as u32) * 4;
    if ip[0] >> 4 != 4 || ihl < 20 || data.add((off + ihl) as usize) > data_end {
        return false;
    }

    (*pkt).saddr = be32([ip[12], ip[13], ip[14], ip[15]]);
    (*pkt).daddr = be32([ip[16], ip[17], ip[18], ip[19]]);
    (*pkt).proto = ip[9];
    off += ihl;

    if (*pkt).proto == IPPROTO_TCP {
        let mut tcp = [0u8; 20];
        if !xdp_read_bytes::<20>(data, data_end, off, &mut tcp) {
            return false;
        }
        let doff = ((tcp[12] >> 4) as u32) * 4;
        if doff < 20 || data.add((off + doff) as usize) > data_end {
            return false;
        }
        (*pkt).sport = be16([tcp[0], tcp[1]]);
        (*pkt).dport = be16([tcp[2], tcp[3]]);
        off += doff;
    } else if (*pkt).proto == IPPROTO_UDP {
        let mut udp = [0u8; 8];
        if !xdp_read_bytes::<8>(data, data_end, off, &mut udp) {
            return false;
        }
        (*pkt).sport = be16([udp[0], udp[1]]);
        (*pkt).dport = be16([udp[2], udp[3]]);
        off += 8;
    } else {
        return false;
    }

    if data.add(off as usize) > data_end {
        return false;
    }
    (*pkt).payload_off = off;
    (*pkt).payload_len = data_end as usize as u32 - data as usize as u32 - off;
    true
}

#[no_mangle]
#[link_section = "xdp"]
pub extern "C" fn zc_raft_xdp(ctx: *mut XdpMd) -> i32 {
    unsafe {
        let mut pkt = ZcPkt {
            saddr: 0,
            daddr: 0,
            payload_off: 0,
            payload_len: 0,
            sport: 0,
            dport: 0,
            proto: 0,
        };
        let policy = policy_lookup();
        let mut magic = 0u64;
        let mut index = 0u64;
        let mut have_magic = 0u32;
        let mut shard = 0u32;
        let bytes = ((*ctx).data_end as usize).wrapping_sub((*ctx).data as usize) as u64;

        if !parse_xdp_l4(ctx, &mut pkt) {
            return XDP_PASS;
        }
        if !port_matches_policy(policy, pkt.sport, pkt.dport) {
            return XDP_PASS;
        }

        let data = (*ctx).data as usize as *const u8;
        let data_end = (*ctx).data_end as usize as *const u8;
        if pkt.payload_len >= 8 {
            if !xdp_read_bytes::<8>(
                data,
                data_end,
                pkt.payload_off,
                (&mut magic as *mut u64).cast(),
            ) {
                return XDP_PASS;
            }
            have_magic = 1;
            if magic_is_append(magic) && pkt.payload_len >= 24 {
                let _ = xdp_read_bytes::<8>(
                    data,
                    data_end,
                    pkt.payload_off + 16,
                    (&mut index as *mut u64).cast(),
                );
            } else if magic == magic64(b"URFTACK1") && pkt.payload_len >= 16 {
                let _ = xdp_read_bytes::<8>(
                    data,
                    data_end,
                    pkt.payload_off + 8,
                    (&mut index as *mut u64).cast(),
                );
            } else if magic == magic64(b"RSLTAC01") && pkt.payload_len >= 24 {
                let _ = xdp_read_bytes::<8>(
                    data,
                    data_end,
                    pkt.payload_off + 16,
                    (&mut index as *mut u64).cast(),
                );
            }
            index = u64::from_be(index);
        }

        let input = ZcClassifyIn {
            pkt: &pkt,
            bytes,
            magic,
            index,
            skb_hash: 0,
            have_magic,
        };
        if classify_common(&input, policy, &mut shard) {
            return XDP_DROP;
        }

        if !policy.is_null() && ((*policy).flags & ZC_RAFT_POLICY_F_XDP_CPUMAP) != 0 {
            return bpf_redirect_map(
                ptr::addr_of!(zc_raft_cpu).cast(),
                shard,
                XDP_PASS as u64,
            )
                as i32;
        }

        XDP_PASS
    }
}
