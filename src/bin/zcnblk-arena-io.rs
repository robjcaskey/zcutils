use std::env;
use std::fs;
use std::io;
use std::time::Duration;
use zcutils::zcnblk_app_arena::{ZcnblkAppArena, open_block_direct, pin_current_thread};

fn counter(state: &str, name: &str) -> io::Result<u64> {
    state
        .split_ascii_whitespace()
        .find_map(|field| {
            field
                .strip_prefix(&format!("{name}="))
                .and_then(|v| v.parse().ok())
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("missing {name} counter"),
            )
        })
}

fn main() -> io::Result<()> {
    let mut args = env::args().skip(1);
    let socket = args
        .next()
        .unwrap_or_else(|| "/tmp/zcnblk-app-arena.sock".into());
    let device = args.next().unwrap_or_else(|| "/dev/zcnblk0".into());
    let lane = args
        .next()
        .as_deref()
        .unwrap_or("0")
        .parse::<u32>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let cpu = args
        .next()
        .as_deref()
        .unwrap_or("0")
        .parse::<usize>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let block = args
        .next()
        .as_deref()
        .unwrap_or("61")
        .parse::<u64>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    if args.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many arguments",
        ));
    }

    pin_current_thread(cpu)?;
    let before = fs::read_to_string("/sys/kernel/debug/zcnblk/state")?;
    let before_writes = counter(&before, "bio_alias_writes")?;
    let before_reads = counter(&before, "bio_alias_reads")?;
    let before_fallbacks = counter(&before, "bio_alias_busy_fallbacks")?;
    let before_rejects = counter(&before, "bio_alias_required_rejects")?;

    let arena = ZcnblkAppArena::connect(socket)?;
    let device = open_block_direct(device)?;
    let mut buffer = arena.allocate(lane)?;
    if arena.slot_bytes() != 4096 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "smoke test requires 4096-byte slots",
        ));
    }
    for (index, byte) in buffer.as_mut_slice()?.iter_mut().enumerate() {
        *byte = 0xa7 ^ (index as u8).wrapping_mul(29);
    }
    let expected = buffer.as_slice()?.to_vec();
    let offset = block
        .checked_mul(4096)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "block offset overflow"))?;
    buffer.write_at(&device, offset)?;
    buffer.sync_and_reacquire(&device, Duration::from_secs(5))?;
    buffer.as_mut_slice()?.fill(0);
    buffer.read_at(&device, offset)?;
    if buffer.as_slice()? != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "aliased read data mismatch",
        ));
    }

    let after = fs::read_to_string("/sys/kernel/debug/zcnblk/state")?;
    let writes = counter(&after, "bio_alias_writes")? - before_writes;
    let reads = counter(&after, "bio_alias_reads")? - before_reads;
    let fallbacks = counter(&after, "bio_alias_busy_fallbacks")? - before_fallbacks;
    let rejects = counter(&after, "bio_alias_required_rejects")? - before_rejects;
    if writes != 1 || reads != 1 || fallbacks != 0 || rejects != 0 {
        return Err(io::Error::other(format!(
            "unexpected alias counters writes={writes} reads={reads} fallbacks={fallbacks} rejects={rejects}"
        )));
    }
    println!(
        "zcnblk-arena-io: PASS lane={lane} cpu={cpu} slot={} writes=1 reads=1 payload_copies=0",
        buffer.slot(),
    );
    Ok(())
}
