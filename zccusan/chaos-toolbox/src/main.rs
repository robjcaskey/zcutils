//! Generic, cloud-neutral fault-injection primitives for the standalone
//! zccusan chaos toolbox image.
//!
//! The binary deliberately knows nothing about ZcVolume placement or the
//! zccusan control plane. An external test chooses an exact PID, executable,
//! peer/port, or node and grades the workload while this process performs one
//! bounded Linux fault.

use serde_json::json;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::net::IpAddr;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
const MAX_NETWORK_FAULT_SECONDS: u64 = 3600;

extern "C" fn mark_interrupted(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Release);
}

fn install_signal_handlers() {
    // SAFETY: the handler only performs a lock-free atomic store. SIGKILL
    // cannot be recovered; callers can always run `network-restore` by ID.
    unsafe {
        libc::signal(
            libc::SIGINT,
            mark_interrupted as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            mark_interrupted as *const () as libc::sighandler_t,
        );
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn usage() -> io::Error {
    invalid(
        "usage: zccusan-chaos-toolbox COMMAND [OPTIONS]\n\
         commands:\n\
           agent\n\
           preflight\n\
           process-kill (--pid PID | --exe NAME | --cgroup-contains CONTAINER_ID [--all]) \\
             [--signal TERM|KILL] [--dry-run]\n\
           network-blackhole --experiment ID --port PORT [--peer IP] [--protocol tcp|udp] \\
             [--direction ingress|egress|both] --duration-seconds N [--dry-run]\n\
           network-restore --experiment ID [--dry-run]\n\
           node-poweroff --confirm-node NODE [--dry-run]\n\
         mutating commands require ZCCUSAN_CHAOS_ALLOWED=1; node-poweroff also \\
         requires ZCCUSAN_CHAOS_ALLOW_NODE_POWEROFF=1",
    )
}

fn allow_enabled(name: &str) -> bool {
    env::var(name).is_ok_and(|value| matches!(value.as_str(), "1" | "true" | "yes"))
}

fn require_allow() -> io::Result<()> {
    if allow_enabled("ZCCUSAN_CHAOS_ALLOWED") {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "fault injection is disabled; set ZCCUSAN_CHAOS_ALLOWED=1 in an explicitly authorized toolbox Pod",
        ))
    }
}

fn parse_u64(value: &str, name: &str) -> io::Result<u64> {
    value
        .parse::<u64>()
        .map_err(|error| invalid(format!("invalid {name} {value:?}: {error}")))
}

fn parse_u16(value: &str, name: &str) -> io::Result<u16> {
    value
        .parse::<u16>()
        .map_err(|error| invalid(format!("invalid {name} {value:?}: {error}")))
        .and_then(|value| {
            if value == 0 {
                Err(invalid(format!("{name} must be nonzero")))
            } else {
                Ok(value)
            }
        })
}

fn executable_name(pid: u32) -> io::Result<Option<String>> {
    let path = PathBuf::from(format!("/proc/{pid}/exe"));
    match fs::read_link(path) {
        Ok(target) => Ok(target
            .file_name()
            .and_then(OsStr::to_str)
            .map(str::to_string)),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn matching_pids(name: &str) -> io::Result<Vec<u32>> {
    if name.is_empty() || name.contains('/') {
        return Err(invalid("--exe must be a non-empty executable basename"));
    }
    let own_pid = std::process::id();
    let mut matches = Vec::new();
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        if pid <= 1 || pid == own_pid {
            continue;
        }
        if executable_name(pid)?.as_deref() == Some(name) {
            matches.push(pid);
        }
    }
    matches.sort_unstable();
    Ok(matches)
}

fn validate_cgroup_token(value: &str) -> io::Result<&str> {
    if !(32..=64).contains(&value.len()) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid(
            "--cgroup-contains must be a 32-64 character hexadecimal container ID",
        ));
    }
    Ok(value)
}

fn matching_cgroup_pids(token: &str) -> io::Result<Vec<u32>> {
    let token = validate_cgroup_token(token)?;
    let own_pid = std::process::id();
    let mut matches = Vec::new();
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        if pid <= 1 || pid == own_pid {
            continue;
        }
        let cgroup = match fs::read_to_string(format!("/proc/{pid}/cgroup")) {
            Ok(cgroup) => cgroup,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        if cgroup.contains(token) {
            matches.push(pid);
        }
    }
    matches.sort_unstable();
    Ok(matches)
}

#[derive(Clone, Copy)]
enum RequestedSignal {
    Term,
    Kill,
}

impl RequestedSignal {
    fn parse(value: &str) -> io::Result<Self> {
        match value.to_ascii_uppercase().as_str() {
            "TERM" | "SIGTERM" | "15" => Ok(Self::Term),
            "KILL" | "SIGKILL" | "9" => Ok(Self::Kill),
            _ => Err(invalid("--signal must be TERM or KILL")),
        }
    }

    fn number(self) -> libc::c_int {
        match self {
            Self::Term => libc::SIGTERM,
            Self::Kill => libc::SIGKILL,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Term => "SIGTERM",
            Self::Kill => "SIGKILL",
        }
    }
}

fn process_kill(args: &[String]) -> io::Result<()> {
    let mut pid = None;
    let mut executable = None;
    let mut cgroup_token = None;
    let mut signal = RequestedSignal::Term;
    let mut all = false;
    let mut dry_run = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--pid" if index + 1 < args.len() => {
                pid = Some(parse_u64(&args[index + 1], "PID")?);
                index += 2;
            }
            "--exe" if index + 1 < args.len() => {
                executable = Some(args[index + 1].clone());
                index += 2;
            }
            "--cgroup-contains" if index + 1 < args.len() => {
                cgroup_token = Some(args[index + 1].clone());
                index += 2;
            }
            "--signal" if index + 1 < args.len() => {
                signal = RequestedSignal::parse(&args[index + 1])?;
                index += 2;
            }
            "--all" => {
                all = true;
                index += 1;
            }
            "--dry-run" => {
                dry_run = true;
                index += 1;
            }
            other => return Err(invalid(format!("unknown process-kill option {other:?}"))),
        }
    }
    if usize::from(pid.is_some())
        + usize::from(executable.is_some())
        + usize::from(cgroup_token.is_some())
        != 1
    {
        return Err(invalid(
            "select exactly one of --pid, --exe, or --cgroup-contains",
        ));
    }
    require_allow()?;
    let pids = if let Some(pid) = pid {
        let pid = u32::try_from(pid).map_err(|_| invalid("PID exceeds u32"))?;
        if pid <= 1 || pid == std::process::id() {
            return Err(invalid(
                "refusing to signal PID 0, PID 1, or the toolbox itself",
            ));
        }
        vec![pid]
    } else if let Some(name) = executable.as_deref() {
        let matches = matching_pids(name)?;
        if matches.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no process has executable basename {name:?}"),
            ));
        }
        if matches.len() != 1 && !all {
            return Err(invalid(format!(
                "executable {name:?} matched {} processes; pass --all or use --pid",
                matches.len()
            )));
        }
        matches
    } else {
        let token = cgroup_token.as_deref().expect("exclusive selector");
        let matches = matching_cgroup_pids(token)?;
        if matches.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no process belongs to container ID {token:?}"),
            ));
        }
        if matches.len() != 1 && !all {
            return Err(invalid(format!(
                "container ID {token:?} matched {} processes; pass --all to signal the complete container",
                matches.len()
            )));
        }
        matches
    };
    for pid in pids {
        let exe = executable_name(pid)?.unwrap_or_else(|| "unknown".into());
        println!(
            "{}",
            json!({
                "event": if dry_run { "process_kill_planned" } else { "process_killed" },
                "pid": pid,
                "executable": exe,
                "signal": signal.label(),
            })
        );
        if !dry_run {
            let pid = libc::pid_t::try_from(pid).map_err(|_| invalid("PID exceeds pid_t"))?;
            // SAFETY: PID and signal were validated and the caller explicitly
            // enabled host-process fault injection.
            if unsafe { libc::kill(pid, signal.number()) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

fn validate_experiment_id(value: &str) -> io::Result<String> {
    if value.is_empty()
        || value.len() > 40
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(invalid(
            "--experiment must contain 1-40 lowercase ASCII letters, digits, or hyphens",
        ));
    }
    Ok(format!("zcchaos_{}", value.replace('-', "_")))
}

fn nft_program() -> String {
    env::var("ZCCUSAN_CHAOS_NFT").unwrap_or_else(|_| "nft".into())
}

fn run_nft_script(script: &str) -> io::Result<()> {
    let mut child = Command::new(nft_program())
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("nft stdin unavailable"))?
        .write_all(script.as_bytes())?;
    let output = child.wait_with_output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "nft failed status={} stdout={:?} stderr={:?}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}

fn nft_table_exists(table: &str) -> io::Result<bool> {
    let output = Command::new(nft_program())
        .args(["list", "table", "inet", table])
        .output()?;
    Ok(output.status.success())
}

fn restore_network_table(table: &str, dry_run: bool) -> io::Result<bool> {
    if dry_run {
        return Ok(false);
    }
    if !nft_table_exists(table)? {
        return Ok(false);
    }
    run_nft_script(&format!("delete table inet {table}\n"))?;
    Ok(true)
}

#[derive(Clone, Copy)]
enum Direction {
    Ingress,
    Egress,
    Both,
}

impl Direction {
    fn parse(value: &str) -> io::Result<Self> {
        match value {
            "ingress" => Ok(Self::Ingress),
            "egress" => Ok(Self::Egress),
            "both" => Ok(Self::Both),
            _ => Err(invalid("--direction must be ingress, egress, or both")),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Ingress => "ingress",
            Self::Egress => "egress",
            Self::Both => "both",
        }
    }
}

fn address_expression(peer: Option<IpAddr>, direction: &str) -> String {
    match peer {
        Some(IpAddr::V4(address)) => format!("ip {direction} {address} "),
        Some(IpAddr::V6(address)) => format!("ip6 {direction} {address} "),
        None => String::new(),
    }
}

fn network_table_script(
    table: &str,
    port: u16,
    protocol: &str,
    direction: Direction,
    peer: Option<IpAddr>,
) -> String {
    let mut script = format!("add table inet {table}\n");
    if matches!(direction, Direction::Ingress | Direction::Both) {
        script.push_str(&format!(
            "add chain inet {table} ingress {{ type filter hook input priority -150; policy accept; }}\n"
        ));
        let address = address_expression(peer, "saddr");
        script.push_str(&format!(
            "add rule inet {table} ingress {address}{protocol} dport {port} drop\n"
        ));
        script.push_str(&format!(
            "add rule inet {table} ingress {address}{protocol} sport {port} drop\n"
        ));
    }
    if matches!(direction, Direction::Egress | Direction::Both) {
        script.push_str(&format!(
            "add chain inet {table} egress {{ type filter hook output priority -150; policy accept; }}\n"
        ));
        let address = address_expression(peer, "daddr");
        script.push_str(&format!(
            "add rule inet {table} egress {address}{protocol} dport {port} drop\n"
        ));
        script.push_str(&format!(
            "add rule inet {table} egress {address}{protocol} sport {port} drop\n"
        ));
    }
    script
}

struct NetworkGuard {
    table: String,
    armed: bool,
}

impl Drop for NetworkGuard {
    fn drop(&mut self) {
        if self.armed {
            if let Err(error) = restore_network_table(&self.table, false) {
                eprintln!(
                    "{}",
                    json!({"event":"network_restore_failed","table":self.table,"error":error.to_string()})
                );
            } else {
                println!("{}", json!({"event":"network_restored","table":self.table}));
            }
        }
    }
}

fn network_blackhole(args: &[String]) -> io::Result<()> {
    let mut experiment = None;
    let mut port = None;
    let mut peer = None;
    let mut protocol = "tcp".to_string();
    let mut direction = Direction::Both;
    let mut duration_seconds = None;
    let mut dry_run = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--experiment" if index + 1 < args.len() => {
                experiment = Some(args[index + 1].clone());
                index += 2;
            }
            "--port" if index + 1 < args.len() => {
                port = Some(parse_u16(&args[index + 1], "port")?);
                index += 2;
            }
            "--peer" if index + 1 < args.len() => {
                peer = Some(
                    args[index + 1]
                        .parse::<IpAddr>()
                        .map_err(|error| invalid(format!("invalid peer IP: {error}")))?,
                );
                index += 2;
            }
            "--protocol" if index + 1 < args.len() => {
                protocol = args[index + 1].to_ascii_lowercase();
                index += 2;
            }
            "--direction" if index + 1 < args.len() => {
                direction = Direction::parse(&args[index + 1])?;
                index += 2;
            }
            "--duration-seconds" if index + 1 < args.len() => {
                duration_seconds = Some(parse_u64(&args[index + 1], "duration")?);
                index += 2;
            }
            "--dry-run" => {
                dry_run = true;
                index += 1;
            }
            other => {
                return Err(invalid(format!(
                    "unknown network-blackhole option {other:?}"
                )));
            }
        }
    }
    require_allow()?;
    if !matches!(protocol.as_str(), "tcp" | "udp") {
        return Err(invalid("--protocol must be tcp or udp"));
    }
    let table = validate_experiment_id(
        experiment
            .as_deref()
            .ok_or_else(|| invalid("--experiment is required"))?,
    )?;
    let port = port.ok_or_else(|| invalid("--port is required"))?;
    let duration_seconds =
        duration_seconds.ok_or_else(|| invalid("--duration-seconds is required"))?;
    if duration_seconds == 0 || duration_seconds > MAX_NETWORK_FAULT_SECONDS {
        return Err(invalid(format!(
            "duration must be 1-{MAX_NETWORK_FAULT_SECONDS} seconds"
        )));
    }
    let script = network_table_script(&table, port, &protocol, direction, peer);
    if dry_run {
        println!(
            "{}",
            json!({
                "event":"network_blackhole_planned",
                "table":table,
                "peer":peer.map(|value| value.to_string()),
                "port":port,
                "protocol":protocol,
                "direction":direction.label(),
                "duration_seconds":duration_seconds,
            })
        );
        return Ok(());
    }
    if nft_table_exists(&table)? {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("nft table {table} already exists; restore it before reusing the ID"),
        ));
    }
    run_nft_script(&script)?;
    let _guard = NetworkGuard {
        table: table.clone(),
        armed: true,
    };
    println!(
        "{}",
        json!({
            "event":"network_blackhole_applied",
            "table":table,
            "peer":peer.map(|value| value.to_string()),
            "port":port,
            "protocol":protocol,
            "direction":direction.label(),
            "duration_seconds":duration_seconds,
        })
    );
    io::stdout().flush()?;
    install_signal_handlers();
    let deadline = Instant::now() + Duration::from_secs(duration_seconds);
    while Instant::now() < deadline && !INTERRUPTED.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

fn network_restore(args: &[String]) -> io::Result<()> {
    let mut experiment = None;
    let mut dry_run = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--experiment" if index + 1 < args.len() => {
                experiment = Some(args[index + 1].clone());
                index += 2;
            }
            "--dry-run" => {
                dry_run = true;
                index += 1;
            }
            other => return Err(invalid(format!("unknown network-restore option {other:?}"))),
        }
    }
    require_allow()?;
    let table = validate_experiment_id(
        experiment
            .as_deref()
            .ok_or_else(|| invalid("--experiment is required"))?,
    )?;
    let restored = restore_network_table(&table, dry_run)?;
    println!(
        "{}",
        json!({
            "event": if dry_run { "network_restore_planned" } else { "network_restore_complete" },
            "table":table,
            "restored":restored,
        })
    );
    Ok(())
}

fn node_poweroff(args: &[String]) -> io::Result<()> {
    let mut confirm_node = None;
    let mut dry_run = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--confirm-node" if index + 1 < args.len() => {
                confirm_node = Some(args[index + 1].clone());
                index += 2;
            }
            "--dry-run" => {
                dry_run = true;
                index += 1;
            }
            other => return Err(invalid(format!("unknown node-poweroff option {other:?}"))),
        }
    }
    require_allow()?;
    if !allow_enabled("ZCCUSAN_CHAOS_ALLOW_NODE_POWEROFF") {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "node poweroff requires ZCCUSAN_CHAOS_ALLOW_NODE_POWEROFF=1",
        ));
    }
    let actual = env::var("ZCCUSAN_CHAOS_NODE_NAME")
        .map_err(|_| invalid("ZCCUSAN_CHAOS_NODE_NAME is required"))?;
    let confirmed = confirm_node.ok_or_else(|| invalid("--confirm-node is required"))?;
    if confirmed != actual {
        return Err(invalid(format!(
            "confirmed node {confirmed:?} does not match this toolbox node {actual:?}"
        )));
    }
    println!(
        "{}",
        json!({
            "event":if dry_run { "node_poweroff_planned" } else { "node_poweroff_requested" },
            "node":actual,
            "shutdown_style":"kernel-poweroff",
        })
    );
    io::stdout().flush()?;
    if dry_run {
        return Ok(());
    }
    thread::sleep(Duration::from_millis(100));
    // SAFETY: this syscall is reachable only through two explicit opt-ins and
    // an exact node-name confirmation. The chart omits CAP_SYS_BOOT unless its
    // separate nodePoweroff value is enabled.
    let result = unsafe { libc::reboot(libc::RB_POWER_OFF) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn preflight() -> io::Result<()> {
    let nft = nft_program();
    let nft_available = Command::new(&nft)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    let pid_one = fs::read_link("/proc/1/exe")
        .ok()
        .and_then(|value| value.to_str().map(str::to_string));
    println!(
        "{}",
        json!({
            "event":"chaos_toolbox_preflight",
            "faults_enabled":allow_enabled("ZCCUSAN_CHAOS_ALLOWED"),
            "node_poweroff_enabled":allow_enabled("ZCCUSAN_CHAOS_ALLOW_NODE_POWEROFF"),
            "node":env::var("ZCCUSAN_CHAOS_NODE_NAME").ok(),
            "pid_one_executable":pid_one,
            "nft_program":nft,
            "nft_available":nft_available,
        })
    );
    if !nft_available {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "nft is unavailable; process faults remain usable but network faults do not",
        ));
    }
    Ok(())
}

fn agent() -> io::Result<()> {
    install_signal_handlers();
    println!(
        "{}",
        json!({
            "event":"chaos_toolbox_ready",
            "node":env::var("ZCCUSAN_CHAOS_NODE_NAME").ok(),
            "faults_enabled":allow_enabled("ZCCUSAN_CHAOS_ALLOWED"),
        })
    );
    io::stdout().flush()?;
    while !INTERRUPTED.load(Ordering::Acquire) {
        thread::sleep(Duration::from_secs(1));
    }
    Ok(())
}

fn main() -> io::Result<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    let Some((command, rest)) = args.split_first() else {
        return Err(usage());
    };
    match command.as_str() {
        "agent" => agent(),
        "preflight" => preflight(),
        "process-kill" => process_kill(rest),
        "network-blackhole" => network_blackhole(rest),
        "network-restore" => network_restore(rest),
        "node-poweroff" => node_poweroff(rest),
        "version" | "--version" => {
            println!(env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "help" | "--help" | "-h" => {
            println!("{}", usage());
            Ok(())
        }
        _ => Err(usage()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn experiment_ids_are_nft_safe() {
        assert_eq!(
            validate_experiment_id("leaf-a-1").unwrap(),
            "zcchaos_leaf_a_1"
        );
        assert!(validate_experiment_id("UPPER").is_err());
        assert!(validate_experiment_id("bad;flush").is_err());
        assert!(validate_experiment_id("").is_err());
    }

    #[test]
    fn network_script_is_exactly_scoped() {
        let script = network_table_script(
            "zcchaos_test",
            26000,
            "tcp",
            Direction::Both,
            Some("10.2.3.4".parse().unwrap()),
        );
        assert!(script.contains("hook input"));
        assert!(script.contains("hook output"));
        assert!(script.contains("ip saddr 10.2.3.4 tcp dport 26000 drop"));
        assert!(script.contains("ip daddr 10.2.3.4 tcp sport 26000 drop"));
        assert!(!script.contains("policy drop"));
    }

    #[test]
    fn executable_selector_rejects_paths() {
        assert!(matching_pids("/usr/bin/sleep").is_err());
    }

    #[test]
    fn cgroup_selector_requires_a_specific_container_id() {
        assert!(validate_cgroup_token("abc123").is_err());
        assert!(validate_cgroup_token("g0000000000000000000000000000000").is_err());
        assert_eq!(
            validate_cgroup_token("0123456789abcdef0123456789abcdef").unwrap(),
            "0123456789abcdef0123456789abcdef"
        );
    }

    #[test]
    fn duration_and_port_parsers_are_bounded_inputs() {
        assert_eq!(parse_u16("29000", "port").unwrap(), 29000);
        assert!(parse_u16("0", "port").is_err());
        assert!(parse_u64("not-a-number", "duration").is_err());
    }
}
