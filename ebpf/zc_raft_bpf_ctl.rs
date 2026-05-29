use std::env;
use std::ffi::CString;
use std::mem;
use std::os::fd::RawFd;

const BPF_MAP_LOOKUP_ELEM: i32 = 1;
const BPF_MAP_UPDATE_ELEM: i32 = 2;
const BPF_OBJ_GET: i32 = 7;
const BPF_ANY: u64 = 0;

#[cfg(target_arch = "x86_64")]
const SYS_BPF: isize = 321;
#[cfg(target_arch = "aarch64")]
const SYS_BPF: isize = 280;

const BPF_FS_ROOT: &str = "/sys/fs/bpf/tc/globals";
const ZC_RAFT_MAX_PORTS: usize = 8;
const ZC_RAFT_MAX_SHARDS: u32 = 256;

const ZC_RAFT_POLICY_F_STRICT_DROP: u32 = 1 << 0;
const ZC_RAFT_POLICY_F_SET_MARK: u32 = 1 << 1;
const ZC_RAFT_POLICY_F_SHARD_INDEX: u32 = 1 << 2;
const ZC_RAFT_POLICY_F_XDP_CPUMAP: u32 = 1 << 3;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct BpfObjGetAttr {
    pathname: u64,
    bpf_fd: u32,
    file_flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct BpfMapElemAttr {
    map_fd: u32,
    _pad: u32,
    key: u64,
    value: u64,
    flags: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ZcRaftPolicy {
    flags: u32,
    shard_count: u32,
    mark_base: u32,
    ports: [u32; ZC_RAFT_MAX_PORTS],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ZcRaftCounter {
    packets: u64,
    bytes: u64,
    appends: u64,
    acks: u64,
    unknown_magic: u64,
    drops: u64,
}

unsafe extern "C" {
    fn syscall(num: isize, ...) -> isize;
    fn close(fd: i32) -> i32;
}

fn errno_message() -> String {
    std::io::Error::last_os_error().to_string()
}

fn bpf_sys<T>(cmd: i32, attr: &mut T) -> Result<i32, String> {
    let ret = unsafe { syscall(SYS_BPF, cmd, attr as *mut T, mem::size_of::<T>() as u32) };
    if ret < 0 {
        Err(errno_message())
    } else {
        Ok(ret as i32)
    }
}

fn bpf_obj_get(path: &str) -> Result<RawFd, String> {
    let c_path = CString::new(path).map_err(|err| err.to_string())?;
    let mut attr = BpfObjGetAttr {
        pathname: c_path.as_ptr() as u64,
        bpf_fd: 0,
        file_flags: 0,
    };
    bpf_sys(BPF_OBJ_GET, &mut attr)
}

fn bpf_lookup(fd: RawFd, key: &u32, value: *mut u8) -> Result<(), String> {
    let mut attr = BpfMapElemAttr {
        map_fd: fd as u32,
        _pad: 0,
        key: key as *const u32 as u64,
        value: value as u64,
        flags: 0,
    };
    bpf_sys(BPF_MAP_LOOKUP_ELEM, &mut attr).map(|_| ())
}

fn bpf_update<T>(fd: RawFd, key: &u32, value: &T) -> Result<(), String> {
    let mut attr = BpfMapElemAttr {
        map_fd: fd as u32,
        _pad: 0,
        key: key as *const u32 as u64,
        value: value as *const T as u64,
        flags: BPF_ANY,
    };
    bpf_sys(BPF_MAP_UPDATE_ELEM, &mut attr).map(|_| ())
}

fn close_fd(fd: RawFd) {
    unsafe {
        let _ = close(fd);
    }
}

fn per_cpu_stride(value_size: usize) -> usize {
    (value_size + 7) & !7
}

fn possible_cpu_count() -> usize {
    std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
}

fn aggregate_counter(raw: &[u8], cpus: usize) -> ZcRaftCounter {
    let stride = per_cpu_stride(mem::size_of::<ZcRaftCounter>());
    let mut out = ZcRaftCounter::default();
    for cpu in 0..cpus {
        let offset = cpu * stride;
        if offset + mem::size_of::<ZcRaftCounter>() > raw.len() {
            break;
        }
        let counter = unsafe { (raw.as_ptr().add(offset) as *const ZcRaftCounter).read_unaligned() };
        out.packets += counter.packets;
        out.bytes += counter.bytes;
        out.appends += counter.appends;
        out.acks += counter.acks;
        out.unknown_magic += counter.unknown_magic;
        out.drops += counter.drops;
    }
    out
}

fn stat_name(key: u32) -> &'static str {
    match key {
        0 => "total",
        1 => "tcp",
        2 => "udp",
        3 => "raft_port",
        4 => "append",
        5 => "ack",
        6 => "unknown_magic",
        7 => "drop",
        _ => "reserved",
    }
}

fn dump_counter_map(label: &str, fd: RawFd, max_key: u32, skip_zero: bool) -> Result<(), String> {
    let cpus = possible_cpu_count();
    let stride = per_cpu_stride(mem::size_of::<ZcRaftCounter>());
    let mut raw = vec![0u8; stride * cpus];

    for key in 0..max_key {
        raw.fill(0);
        bpf_lookup(fd, &key, raw.as_mut_ptr())
            .map_err(|err| format!("lookup {label}[{key}]: {err}"))?;
        let counter = aggregate_counter(&raw, cpus);
        if skip_zero
            && counter.packets == 0
            && counter.bytes == 0
            && counter.appends == 0
            && counter.acks == 0
            && counter.unknown_magic == 0
            && counter.drops == 0
        {
            continue;
        }
        let name = if label == "stats" { stat_name(key) } else { "shard" };
        println!(
            "{label}[{key}:{name}] packets={} bytes={} appends={} acks={} unknown_magic={} drops={}",
            counter.packets,
            counter.bytes,
            counter.appends,
            counter.acks,
            counter.unknown_magic,
            counter.drops
        );
    }
    Ok(())
}

fn dump_stats(root: &str) -> Result<(), String> {
    let stats_path = format!("{root}/zc_raft_stats");
    let shards_path = format!("{root}/zc_raft_shards");
    let stats_fd = bpf_obj_get(&stats_path).map_err(|err| format!("open {stats_path}: {err}"))?;
    let shards_fd = match bpf_obj_get(&shards_path) {
        Ok(fd) => fd,
        Err(err) => {
            close_fd(stats_fd);
            return Err(format!("open {shards_path}: {err}"));
        }
    };

    let result = dump_counter_map("stats", stats_fd, 16, false)
        .and_then(|_| dump_counter_map("shards", shards_fd, ZC_RAFT_MAX_SHARDS, true));
    close_fd(stats_fd);
    close_fd(shards_fd);
    result
}

fn parse_ports(value: &str, policy: &mut ZcRaftPolicy) -> Result<(), String> {
    policy.ports = [0; ZC_RAFT_MAX_PORTS];
    for (idx, port) in value.split(',').filter(|port| !port.is_empty()).enumerate() {
        if idx >= ZC_RAFT_MAX_PORTS {
            return Err(format!("too many ports; max is {ZC_RAFT_MAX_PORTS}"));
        }
        let parsed = port
            .parse::<u32>()
            .map_err(|err| format!("invalid port {port:?}: {err}"))?;
        if parsed > u16::MAX as u32 {
            return Err(format!("invalid port {port:?}: outside u16 range"));
        }
        policy.ports[idx] = parsed;
    }
    Ok(())
}

fn parse_u32(name: &str, value: &str) -> Result<u32, String> {
    if let Some(hex) = value.strip_prefix("0x") {
        u32::from_str_radix(hex, 16).map_err(|err| format!("invalid {name}: {err}"))
    } else {
        value
            .parse::<u32>()
            .map_err(|err| format!("invalid {name}: {err}"))
    }
}

fn update_policy(root: &str, args: &[String]) -> Result<(), String> {
    let mut policy = ZcRaftPolicy {
        flags: 0,
        shard_count: 64,
        mark_base: 0,
        ports: [0; ZC_RAFT_MAX_PORTS],
    };
    policy.ports[0] = 19401;
    policy.ports[1] = 9100;
    policy.ports[2] = 9200;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--ports" => {
                i += 1;
                let value = args.get(i).ok_or("--ports requires CSV value")?;
                parse_ports(value, &mut policy)?;
            }
            "--shards" => {
                i += 1;
                let value = args.get(i).ok_or("--shards requires value")?;
                policy.shard_count = parse_u32("--shards", value)?;
                if policy.shard_count == 0 || policy.shard_count > ZC_RAFT_MAX_SHARDS {
                    return Err(format!("--shards must be 1..{ZC_RAFT_MAX_SHARDS}"));
                }
            }
            "--mark-base" => {
                i += 1;
                let value = args.get(i).ok_or("--mark-base requires value")?;
                policy.mark_base = parse_u32("--mark-base", value)?;
            }
            "--mark" => policy.flags |= ZC_RAFT_POLICY_F_SET_MARK,
            "--strict-drop" => policy.flags |= ZC_RAFT_POLICY_F_STRICT_DROP,
            "--shard-by-index" => policy.flags |= ZC_RAFT_POLICY_F_SHARD_INDEX,
            "--xdp-cpumap" => policy.flags |= ZC_RAFT_POLICY_F_XDP_CPUMAP,
            other => return Err(format!("unknown policy option: {other}")),
        }
        i += 1;
    }

    let path = format!("{root}/zc_raft_policy");
    let fd = bpf_obj_get(&path).map_err(|err| format!("open {path}: {err}"))?;
    let key = 0u32;
    let result = bpf_update(fd, &key, &policy);
    close_fd(fd);
    result?;

    print!(
        "policy flags=0x{:x} shards={} mark_base={} ports=",
        policy.flags, policy.shard_count, policy.mark_base
    );
    let mut first = true;
    for port in policy.ports.iter().copied().filter(|port| *port != 0) {
        if !first {
            print!(",");
        }
        first = false;
        print!("{port}");
    }
    println!();
    Ok(())
}

fn usage(program: &str) {
    eprintln!(
        "usage:\n  {program} stats [bpf-fs-root]\n  {program} policy [bpf-fs-root] [--ports CSV] [--shards N] [--mark --mark-base N] [--strict-drop] [--shard-by-index] [--xdp-cpumap]"
    );
}

fn main() {
    let args = env::args().collect::<Vec<_>>();
    if args.len() < 2 {
        usage(&args[0]);
        std::process::exit(2);
    }

    let result = match args[1].as_str() {
        "stats" => {
            let root = args.get(2).map(String::as_str).unwrap_or(BPF_FS_ROOT);
            dump_stats(root)
        }
        "policy" => {
            let mut root = BPF_FS_ROOT;
            let mut first_opt = 2usize;
            if args.get(2).is_some_and(|arg| !arg.starts_with("--")) {
                root = &args[2];
                first_opt = 3;
            }
            update_policy(root, &args[first_opt..])
        }
        _ => {
            usage(&args[0]);
            std::process::exit(2);
        }
    };

    if let Err(err) = result {
        eprintln!("{err}");
        std::process::exit(1);
    }
}
