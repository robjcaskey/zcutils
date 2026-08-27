use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const BLOCK_BYTES: usize = 4096;
const MAGIC: u64 = 0x5a43_4d49_4752_4154;
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn request_stop(_: libc::c_int) {
    STOP.store(true, Ordering::Release);
}

struct AlignedBlock {
    pointer: NonNull<u8>,
}

impl AlignedBlock {
    fn new() -> io::Result<Self> {
        let layout = Layout::from_size_align(BLOCK_BYTES, BLOCK_BYTES)
            .map_err(|error| invalid(error.to_string()))?;
        let pointer = NonNull::new(unsafe { alloc_zeroed(layout) })
            .ok_or_else(|| io::Error::other("aligned block allocation failed"))?;
        Ok(Self { pointer })
    }

    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.pointer.as_ptr(), BLOCK_BYTES) }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.pointer.as_ptr(), BLOCK_BYTES) }
    }
}

impl Drop for AlignedBlock {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(BLOCK_BYTES, BLOCK_BYTES).expect("valid block layout");
        unsafe { dealloc(self.pointer.as_ptr(), layout) };
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("zcnblk-edge-continuity: ERROR: {error}");
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let mut args = env::args().skip(1);
    let target = args.next().unwrap_or_else(|| "/dev/zcnblk0".into());
    let base_offset = parse_u64(args.next(), "BASE_OFFSET")?;
    let slots = parse_u64(args.next(), "SLOTS")?;
    let interval_us = parse_u64(args.next(), "INTERVAL_US")?;
    let sync_every = parse_u64(args.next(), "SYNC_EVERY")?;
    if args.next().is_some() || base_offset % BLOCK_BYTES as u64 != 0 || slots == 0 {
        return Err(invalid(
            "usage: zcnblk-edge-continuity TARGET BASE_OFFSET SLOTS INTERVAL_US SYNC_EVERY; offset must be 4096-aligned and slots non-zero",
        ));
    }
    let slots_usize = usize::try_from(slots).map_err(|_| invalid("SLOTS exceeds usize"))?;
    install_stop_handlers()?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT | libc::O_CLOEXEC)
        .open(&target)?;
    let initial = file.metadata()?;
    let end_offset = base_offset
        .checked_add(
            slots
                .checked_mul(BLOCK_BYTES as u64)
                .ok_or_else(|| invalid("continuity proof length overflow"))?,
        )
        .ok_or_else(|| invalid("continuity proof range overflow"))?;
    if initial.size() != 0 && end_offset > initial.size() {
        return Err(invalid(format!(
            "continuity proof range ends at {end_offset}, past device size {}",
            initial.size()
        )));
    }
    if let Ok(path) = env::var("ZCNBLK_EDGE_CONTINUITY_PID_FILE") {
        std::fs::write(path, format!("{}\n", std::process::id()))?;
    }

    let mut write_block = AlignedBlock::new()?;
    let mut read_block = AlignedBlock::new()?;
    let mut last_sequences = vec![0u64; slots_usize];
    let mut sequence = 0u64;
    let mut writes = 0u64;
    let mut reads = 0u64;
    let mut dirty_read_matches = 0u64;
    let mut syncs = 0u64;
    let mut identity_checks = 0u64;
    let started = Instant::now();

    // Seed and globally drain every slot before migration starts. Subsequent
    // writes exercise dirty-overlay reads while the route epoch changes.
    for slot in 0..slots {
        sequence = sequence.saturating_add(1);
        fill_block(write_block.as_mut_slice(), slot, sequence);
        exact_pwrite(
            &file,
            write_block.as_slice(),
            slot_offset(base_offset, slot)?,
        )?;
        writes += 1;
        exact_pread(
            &file,
            read_block.as_mut_slice(),
            slot_offset(base_offset, slot)?,
        )?;
        reads += 1;
        compare_block(
            write_block.as_slice(),
            read_block.as_slice(),
            slot,
            sequence,
        )?;
        dirty_read_matches += 1;
        last_sequences[slot as usize] = sequence;
    }
    file.sync_data()?;
    syncs += 1;
    println!(
        "zcnblk-edge-continuity-start: target={target} device={} inode={} open_descriptors=1 base_offset={base_offset} slots={slots} block_bytes={BLOCK_BYTES} interval_us={interval_us} sync_every={sync_every} ordinary_write_completion=early-local-retained-wal-admission sync_completion=remote-global-hwm-drain",
        initial.dev(),
        initial.ino(),
    );
    io::stdout().flush()?;

    while !STOP.load(Ordering::Acquire) {
        let slot = sequence % slots;
        sequence = sequence.saturating_add(1);
        fill_block(write_block.as_mut_slice(), slot, sequence);
        exact_pwrite(
            &file,
            write_block.as_slice(),
            slot_offset(base_offset, slot)?,
        )?;
        writes += 1;
        exact_pread(
            &file,
            read_block.as_mut_slice(),
            slot_offset(base_offset, slot)?,
        )?;
        reads += 1;
        compare_block(
            write_block.as_slice(),
            read_block.as_slice(),
            slot,
            sequence,
        )?;
        dirty_read_matches += 1;
        last_sequences[slot as usize] = sequence;
        if sync_every != 0 && writes % sync_every == 0 {
            file.sync_data()?;
            syncs += 1;
        }
        if writes % 256 == 0 {
            let current = file.metadata()?;
            if current.dev() != initial.dev() || current.ino() != initial.ino() {
                return Err(io::Error::other(format!(
                    "block identity changed from device={} inode={} to device={} inode={}",
                    initial.dev(),
                    initial.ino(),
                    current.dev(),
                    current.ino()
                )));
            }
            identity_checks += 1;
        }
        if interval_us != 0 {
            thread::sleep(Duration::from_micros(interval_us));
        }
    }

    file.sync_data()?;
    syncs += 1;
    let mut final_slot_matches = 0u64;
    for (slot, expected_sequence) in last_sequences.into_iter().enumerate() {
        fill_block(write_block.as_mut_slice(), slot as u64, expected_sequence);
        exact_pread(
            &file,
            read_block.as_mut_slice(),
            slot_offset(base_offset, slot as u64)?,
        )?;
        reads += 1;
        compare_block(
            write_block.as_slice(),
            read_block.as_slice(),
            slot as u64,
            expected_sequence,
        )?;
        final_slot_matches += 1;
    }
    let final_metadata = file.metadata()?;
    if final_metadata.dev() != initial.dev() || final_metadata.ino() != initial.ino() {
        return Err(io::Error::other(
            "block identity changed at final verification",
        ));
    }
    let elapsed = started.elapsed();
    println!(
        "ZCNBLK_EDGE_CONTINUITY_PASS target={target} device={} inode={} identity_stable=true open_descriptor_replaced=false writes={writes} reads={reads} dirty_read_matches={dirty_read_matches} final_slot_matches={final_slot_matches} syncs={syncs} identity_checks={identity_checks} mismatches=0 elapsed_seconds={:.6} proof_iops={:.0} final_completion=remote-global-hwm-drain",
        initial.dev(),
        initial.ino(),
        elapsed.as_secs_f64(),
        (writes + reads) as f64 / elapsed.as_secs_f64().max(f64::MIN_POSITIVE),
    );
    Ok(())
}

fn parse_u64(value: Option<String>, label: &str) -> io::Result<u64> {
    value
        .ok_or_else(|| invalid(format!("missing {label}")))?
        .parse::<u64>()
        .map_err(|error| invalid(format!("invalid {label}: {error}")))
}

fn install_stop_handlers() -> io::Result<()> {
    let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
    action.sa_sigaction = request_stop as *const () as usize;
    if unsafe { libc::sigemptyset(&mut action.sa_mask) } != 0 {
        return Err(io::Error::last_os_error());
    }
    for signal in [libc::SIGINT, libc::SIGTERM] {
        if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn exact_pwrite(file: &File, block: &[u8], offset: u64) -> io::Result<()> {
    loop {
        let result = unsafe {
            libc::pwrite(
                file.as_raw_fd(),
                block.as_ptr().cast(),
                block.len(),
                offset as libc::off_t,
            )
        };
        if result == block.len() as isize {
            return Ok(());
        }
        if result < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(if result < 0 {
            io::Error::last_os_error()
        } else {
            io::Error::new(
                io::ErrorKind::WriteZero,
                format!("short direct write: {result}/{}", block.len()),
            )
        });
    }
}

fn exact_pread(file: &File, block: &mut [u8], offset: u64) -> io::Result<()> {
    loop {
        let result = unsafe {
            libc::pread(
                file.as_raw_fd(),
                block.as_mut_ptr().cast(),
                block.len(),
                offset as libc::off_t,
            )
        };
        if result == block.len() as isize {
            return Ok(());
        }
        if result < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(if result < 0 {
            io::Error::last_os_error()
        } else {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("short direct read: {result}/{}", block.len()),
            )
        });
    }
}

fn fill_block(block: &mut [u8], slot: u64, sequence: u64) {
    for (index, word) in block.chunks_exact_mut(8).enumerate() {
        let value = mix64(sequence ^ slot.rotate_left(17) ^ index as u64);
        word.copy_from_slice(&value.to_le_bytes());
    }
    block[0..8].copy_from_slice(&MAGIC.to_le_bytes());
    block[8..16].copy_from_slice(&slot.to_le_bytes());
    block[16..24].copy_from_slice(&sequence.to_le_bytes());
    block[24..32].copy_from_slice(&(!sequence).to_le_bytes());
}

fn compare_block(expected: &[u8], actual: &[u8], slot: u64, sequence: u64) -> io::Result<()> {
    if expected != actual {
        let differing_byte = expected
            .iter()
            .zip(actual)
            .position(|(left, right)| left != right)
            .unwrap_or(0);
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "continuity mismatch slot={slot} sequence={sequence} byte={differing_byte} expected={} actual={}",
                expected[differing_byte], actual[differing_byte]
            ),
        ));
    }
    Ok(())
}

fn slot_offset(base: u64, slot: u64) -> io::Result<u64> {
    base.checked_add(
        slot.checked_mul(BLOCK_BYTES as u64)
            .ok_or_else(|| invalid("slot length overflow"))?,
    )
    .ok_or_else(|| invalid("slot offset overflow"))
}

fn mix64(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}
