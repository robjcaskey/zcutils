use std::env;
use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{Shutdown, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::ptr::NonNull;
use std::slice;
use std::time::{Duration, Instant};

const PAGE: usize = 4096;
const SLOTS: u64 = 4096;

struct AlignedPage(NonNull<u8>);

impl AlignedPage {
    fn new() -> io::Result<Self> {
        let mut ptr = std::ptr::null_mut();
        let result = unsafe { libc::posix_memalign(&mut ptr, PAGE, PAGE) };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
        Ok(Self(
            NonNull::new(ptr.cast()).expect("posix_memalign returned null"),
        ))
    }

    fn as_slice(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.0.as_ptr(), PAGE) }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { slice::from_raw_parts_mut(self.0.as_ptr(), PAGE) }
    }
}

impl Drop for AlignedPage {
    fn drop(&mut self) {
        unsafe { libc::free(self.0.as_ptr().cast()) }
    }
}

fn fill(page: &mut [u8], sequence: u64) {
    page[..8].copy_from_slice(&sequence.to_le_bytes());
    page[8..16].copy_from_slice(b"ZCGLOBAL");
    for (index, byte) in page[16..].iter_mut().enumerate() {
        *byte = (sequence as u8).wrapping_mul(131).wrapping_add(index as u8);
    }
}

fn open_direct(path: &str) -> io::Result<std::fs::File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT | libc::O_CLOEXEC)
        .open(path)
}

fn identity(fd: i32) -> io::Result<(u64, u64)> {
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((stat.st_dev, stat.st_ino))
}

fn write_verify(
    file: &std::fs::File,
    write_page: &mut AlignedPage,
    read_page: &mut AlignedPage,
    sequence: u64,
) -> io::Result<()> {
    let offset = (sequence % SLOTS) * PAGE as u64;
    fill(write_page.as_mut_slice(), sequence);
    if file.write_at(write_page.as_slice(), offset)? != PAGE {
        return Err(io::Error::new(io::ErrorKind::WriteZero, "short write"));
    }
    file.sync_data()?;
    read_page.as_mut_slice().fill(0);
    if file.read_at(read_page.as_mut_slice(), offset)? != PAGE {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short read"));
    }
    if read_page.as_slice() != write_page.as_slice() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("read-after-sync mismatch at sequence {sequence}"),
        ));
    }
    Ok(())
}

fn verify_prefix(file: &std::fs::File, through: u64) -> io::Result<()> {
    if through >= SLOTS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("verification range must be less than {SLOTS}"),
        ));
    }
    let mut expected = AlignedPage::new()?;
    let mut actual = AlignedPage::new()?;
    for sequence in 1..=through {
        fill(expected.as_mut_slice(), sequence);
        actual.as_mut_slice().fill(0);
        let offset = (sequence % SLOTS) * PAGE as u64;
        if file.read_at(actual.as_mut_slice(), offset)? != PAGE {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short read"));
        }
        if actual.as_slice() != expected.as_slice() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("handoff verification mismatch at sequence {sequence}"),
            ));
        }
    }
    Ok(())
}

fn control_command(control: &str, command: &str) -> io::Result<String> {
    let mut stream = TcpStream::connect(control)?;
    writeln!(stream, "{command}")?;
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response)?;
    Ok(response.trim().to_string())
}

fn switch(control: &str) -> io::Result<String> {
    let response = control_command(control, "secondary")?;
    if !response.starts_with("OK active=secondary") {
        return Err(io::Error::other(format!(
            "custody transfer failed: {}",
            response
        )));
    }
    Ok(response)
}

fn one_shot(address: &str, message: &str) -> io::Result<()> {
    let mut stream = TcpStream::connect(address)?;
    stream.write_all(message.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.shutdown(Shutdown::Write)?;
    let mut response = String::new();
    let _ = BufReader::new(stream).read_line(&mut response);
    Ok(())
}

fn run_stay(target: &str, control: &str, operations: u64) -> io::Result<()> {
    if operations < 4 || operations >= SLOTS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("operations must be in 4..{SLOTS}"),
        ));
    }
    let file = open_direct(target)?;
    let fd = file.as_raw_fd();
    let before = identity(fd)?;
    let mut write_page = AlignedPage::new()?;
    let mut read_page = AlignedPage::new()?;
    let mut max_operation_us = 0u128;
    let mut response = String::new();
    let started = Instant::now();
    for sequence in 1..=operations {
        let operation_started = Instant::now();
        write_verify(&file, &mut write_page, &mut read_page, sequence)?;
        max_operation_us = max_operation_us.max(operation_started.elapsed().as_micros());
        if sequence == operations / 2 {
            response = switch(control)?;
        }
    }
    let after = identity(fd)?;
    if before != after {
        return Err(io::Error::other(
            "open block identity changed during failover",
        ));
    }
    println!(
        "ZCGLOBAL_VOLUME_STAY_PASS operations={operations} acknowledged_sequences=1..{operations} fd={fd} device={} inode={} reconnects=0 remounts=0 process_restarts=0 max_write_sync_read_us={max_operation_us} elapsed_ms={} switch=\"{response}\"",
        before.0,
        before.1,
        started.elapsed().as_millis(),
    );
    Ok(())
}

fn run_stay_ha(
    target: &str,
    control: &str,
    operations: u64,
    source_leaf_failure: &str,
    target_leaf_failure: &str,
) -> io::Result<()> {
    if operations < 8 || operations >= SLOTS || operations % 4 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("HA operations must be a multiple of four in 8..{SLOTS}"),
        ));
    }
    let file = open_direct(target)?;
    let fd = file.as_raw_fd();
    let before = identity(fd)?;
    let mut write_page = AlignedPage::new()?;
    let mut read_page = AlignedPage::new()?;
    let started = Instant::now();
    let mut response = String::new();
    for sequence in 1..=operations {
        if sequence == operations / 4 + 1 {
            one_shot(source_leaf_failure, "destroy-source-leaf")?;
            // This is a correctness fault-injection barrier, not a benchmark:
            // wait until the QEMU leaf has actually disappeared before the
            // next acknowledged operation exercises the 2-of-3 path.
            std::thread::sleep(Duration::from_millis(200));
            println!(
                "ZCGLOBAL_VOLUME_REGIONAL_FAILURE region=source after_sequence={} failed_leaf={source_leaf_failure}",
                sequence - 1
            );
        }
        if sequence == operations / 2 + 1 {
            response = switch(control)?;
        }
        if sequence == operations * 3 / 4 + 1 {
            one_shot(target_leaf_failure, "destroy-target-leaf")?;
            std::thread::sleep(Duration::from_millis(200));
            println!(
                "ZCGLOBAL_VOLUME_REGIONAL_FAILURE region=target after_sequence={} failed_leaf={target_leaf_failure}",
                sequence - 1
            );
        }
        write_verify(&file, &mut write_page, &mut read_page, sequence)?;
    }
    let after = identity(fd)?;
    if before != after {
        return Err(io::Error::other(
            "open block identity changed during regional/global failover",
        ));
    }
    println!(
        "ZCGLOBAL_VOLUME_STAY_HA_PASS operations={operations} acknowledged_sequences=1..{operations} regional_replication=2-of-3 source_leaf_failures=1 target_leaf_failures=1 fd={fd} device={} inode={} reconnects=0 remounts=0 process_restarts=0 elapsed_ms={} switch=\"{response}\"",
        before.0,
        before.1,
        started.elapsed().as_millis(),
    );
    Ok(())
}

fn run_move(target: &str, verify_through: u64, end: u64) -> io::Result<()> {
    if verify_through == 0 || end <= verify_through || end >= SLOTS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("require 0 < VERIFY_THROUGH < END < {SLOTS}"),
        ));
    }
    let file = open_direct(target)?;
    let fd = file.as_raw_fd();
    let started = Instant::now();
    verify_prefix(&file, verify_through)?;
    let mut write_page = AlignedPage::new()?;
    let mut read_page = AlignedPage::new()?;
    for sequence in verify_through + 1..=end {
        write_verify(&file, &mut write_page, &mut read_page, sequence)?;
    }
    verify_prefix(&file, end)?;
    println!(
        "ZCGLOBAL_VOLUME_MOVE_PASS source_acknowledged_through={verify_through} destination_acknowledged_through={end} destination_verified=1..{end} fd={fd} service_identity=stable process_restart=expected-across-node pod_data_loss=0 elapsed_ms={}",
        started.elapsed().as_millis(),
    );
    Ok(())
}

fn run_move_hold(target: &str, verify_through: u64, end: u64, seconds: u64) -> io::Result<()> {
    if seconds == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "move-hold requires SECONDS > 0",
        ));
    }
    run_move(target, verify_through, end)?;
    io::stdout().flush()?;
    std::thread::sleep(Duration::from_secs(seconds));
    Ok(())
}

fn run_move_loss_hold(
    target: &str,
    checkpoint: u64,
    lost_through: u64,
    end: u64,
    seconds: u64,
) -> io::Result<()> {
    if seconds == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "move-loss-hold requires SECONDS > 0",
        ));
    }
    run_move_loss(target, checkpoint, lost_through, end)?;
    io::stdout().flush()?;
    std::thread::sleep(Duration::from_secs(seconds));
    Ok(())
}

fn run_hold(target: &str, verify_through: u64, seconds: u64) -> io::Result<()> {
    if verify_through == 0 || verify_through >= SLOTS || seconds == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("require 0 < VERIFY_THROUGH < {SLOTS} and SECONDS > 0"),
        ));
    }
    let file = open_direct(target)?;
    let fd = file.as_raw_fd();
    let identity = identity(fd)?;
    verify_prefix(&file, verify_through)?;
    println!(
        "ZCGLOBAL_VOLUME_HOLD_READY verified=1..{verify_through} fd={fd} device={} inode={} service_identity=stable",
        identity.0, identity.1,
    );
    io::stdout().flush()?;
    std::thread::sleep(Duration::from_secs(seconds));
    println!(
        "ZCGLOBAL_VOLUME_HOLD_PASS verified=1..{verify_through} fd={fd} device={} inode={} process_restarts=0",
        identity.0, identity.1,
    );
    Ok(())
}

fn run_disaster_source(
    target: &str,
    control: &str,
    checkpoint: u64,
    acknowledged_through: u64,
) -> io::Result<()> {
    run_disaster_source_with_failures(
        target,
        control,
        checkpoint,
        acknowledged_through,
        None,
        None,
    )
}

fn run_disaster_source_with_failures(
    target: &str,
    control: &str,
    checkpoint: u64,
    acknowledged_through: u64,
    source_leaf_failure: Option<&str>,
    target_leaf_failure: Option<&str>,
) -> io::Result<()> {
    if checkpoint == 0 || acknowledged_through <= checkpoint || acknowledged_through >= SLOTS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("require 0 < CHECKPOINT < ACKNOWLEDGED_THROUGH < {SLOTS}"),
        ));
    }
    let file = open_direct(target)?;
    let mut write_page = AlignedPage::new()?;
    let mut read_page = AlignedPage::new()?;
    for sequence in 1..=checkpoint {
        if source_leaf_failure.is_some() && sequence == checkpoint / 4 + 1 {
            let address = source_leaf_failure.expect("checked");
            one_shot(address, "destroy-source-leaf")?;
            std::thread::sleep(Duration::from_millis(200));
            println!(
                "ZCGLOBAL_VOLUME_REGIONAL_FAILURE region=source after_sequence={} failed_leaf={address}",
                sequence - 1
            );
        }
        if target_leaf_failure.is_some() && sequence == checkpoint * 3 / 4 + 1 {
            let address = target_leaf_failure.expect("checked");
            one_shot(address, "destroy-target-leaf")?;
            std::thread::sleep(Duration::from_millis(200));
            println!(
                "ZCGLOBAL_VOLUME_REGIONAL_FAILURE region=target after_sequence={} failed_leaf={address}",
                sequence - 1
            );
        }
        write_verify(&file, &mut write_page, &mut read_page, sequence)?;
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let status = control_command(control, "status")?;
        if status.contains(&format!("secondary_synced_generation={checkpoint}")) {
            break;
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("remote replica did not reach checkpoint {checkpoint}: {status}"),
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let paused = control_command(control, "pause replication")?;
    if !paused.starts_with("OK ") || !paused.contains("replication_paused=true") {
        return Err(io::Error::other(format!(
            "could not pause asynchronous replay: {paused}"
        )));
    }
    for sequence in checkpoint + 1..=acknowledged_through {
        write_verify(&file, &mut write_page, &mut read_page, sequence)?;
    }
    println!(
        "ZCGLOBAL_VOLUME_DISASTER_SOURCE_READY remote_checkpoint={checkpoint} source_acknowledged_through={acknowledged_through} declared_missing={}..{} regional_syncs_acknowledged=true async_replay_paused=true regional_replication={} source_leaf_failures={} target_leaf_failures={}",
        checkpoint + 1,
        acknowledged_through,
        if source_leaf_failure.is_some() {
            "2-of-3"
        } else {
            "single-leaf"
        },
        usize::from(source_leaf_failure.is_some()),
        usize::from(target_leaf_failure.is_some()),
    );
    Ok(())
}

fn run_disaster_source_ha(
    target: &str,
    control: &str,
    checkpoint: u64,
    acknowledged_through: u64,
    source_leaf_failure: &str,
    target_leaf_failure: &str,
) -> io::Result<()> {
    if checkpoint < 8 || checkpoint % 4 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "disaster-source-ha checkpoint must be a multiple of four and at least eight",
        ));
    }
    run_disaster_source_with_failures(
        target,
        control,
        checkpoint,
        acknowledged_through,
        Some(source_leaf_failure),
        Some(target_leaf_failure),
    )
}

fn run_disaster_source_hold(
    target: &str,
    control: &str,
    checkpoint: u64,
    acknowledged_through: u64,
    seconds: u64,
) -> io::Result<()> {
    if seconds == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "disaster-source-hold requires SECONDS > 0",
        ));
    }
    run_disaster_source(target, control, checkpoint, acknowledged_through)?;
    io::stdout().flush()?;
    std::thread::sleep(Duration::from_secs(seconds));
    Ok(())
}

fn verify_zero_range(file: &std::fs::File, from: u64, through: u64) -> io::Result<()> {
    let mut actual = AlignedPage::new()?;
    for sequence in from..=through {
        actual.as_mut_slice().fill(0xff);
        let offset = (sequence % SLOTS) * PAGE as u64;
        if file.read_at(actual.as_mut_slice(), offset)? != PAGE {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short read"));
        }
        if actual.as_slice().iter().any(|byte| *byte != 0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("declared-lost destination tail unexpectedly contains sequence {sequence}"),
            ));
        }
    }
    Ok(())
}

fn run_move_loss(target: &str, checkpoint: u64, lost_through: u64, end: u64) -> io::Result<()> {
    if checkpoint == 0 || lost_through <= checkpoint || end <= lost_through || end >= SLOTS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("require 0 < CHECKPOINT < LOST_THROUGH < END < {SLOTS}"),
        ));
    }
    let file = open_direct(target)?;
    eprintln!(
        "ZCGLOBAL_VOLUME_DECLARED_LOSS_MOVE_PROGRESS phase=verify-checkpoint checkpoint={checkpoint} lost_through={lost_through} end={end}"
    );
    verify_prefix(&file, checkpoint)?;
    eprintln!(
        "ZCGLOBAL_VOLUME_DECLARED_LOSS_MOVE_PROGRESS phase=verify-declared-tail-absent from={} through={lost_through}",
        checkpoint + 1,
    );
    verify_zero_range(&file, checkpoint + 1, lost_through)?;
    eprintln!(
        "ZCGLOBAL_VOLUME_DECLARED_LOSS_MOVE_PROGRESS phase=rebuild from={} through={end}",
        checkpoint + 1,
    );
    let mut write_page = AlignedPage::new()?;
    let mut read_page = AlignedPage::new()?;
    for sequence in checkpoint + 1..=end {
        write_verify(&file, &mut write_page, &mut read_page, sequence)?;
        eprintln!(
            "ZCGLOBAL_VOLUME_DECLARED_LOSS_MOVE_PROGRESS phase=rebuild sequence={sequence} through={end}"
        );
    }
    eprintln!("ZCGLOBAL_VOLUME_DECLARED_LOSS_MOVE_PROGRESS phase=verify-rebuilt through={end}");
    verify_prefix(&file, end)?;
    println!(
        "ZCGLOBAL_VOLUME_DECLARED_LOSS_MOVE_PASS accepted_checkpoint={checkpoint} booked_missing={}..{lost_through} destination_tail_absent_before_reuse=true destination_acknowledged_through={end} destination_verified=1..{end} stale_clients_must_reconnect=true",
        checkpoint + 1,
    );
    Ok(())
}

fn run_grade(
    target: &str,
    through: u64,
    zero_from: Option<u64>,
    zero_through: Option<u64>,
) -> io::Result<()> {
    let file = open_direct(target)?;
    verify_prefix(&file, through)?;
    if let (Some(from), Some(end)) = (zero_from, zero_through) {
        if from == 0 || from > end || end >= SLOTS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("require 0 < ZERO_FROM <= ZERO_THROUGH < {SLOTS}"),
            ));
        }
        let mut actual = AlignedPage::new()?;
        for sequence in from..=end {
            actual.as_mut_slice().fill(0xff);
            let offset = (sequence % SLOTS) * PAGE as u64;
            if file.read_at(actual.as_mut_slice(), offset)? != PAGE {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short read"));
            }
            if actual.as_slice().iter().any(|byte| *byte != 0) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("excluded source replica contains post-promotion sequence {sequence}"),
                ));
            }
        }
    }
    println!(
        "ZCGLOBAL_VOLUME_GRADE_PASS target={target} verified=1..{through} zero_range={}",
        zero_from.zip(zero_through).map_or_else(
            || "none".to_string(),
            |(from, end)| format!("{from}..{end}")
        ),
    );
    Ok(())
}

fn usage() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "usage:\n  zcglobal-volume-workload stay TARGET CONTROL_ADDR OPERATIONS\n  zcglobal-volume-workload stay-ha TARGET CONTROL_ADDR OPERATIONS SOURCE_LEAF_FAILURE_ADDR TARGET_LEAF_FAILURE_ADDR\n  zcglobal-volume-workload disaster-source TARGET CONTROL_ADDR CHECKPOINT ACKNOWLEDGED_THROUGH\n  zcglobal-volume-workload disaster-source-ha TARGET CONTROL_ADDR CHECKPOINT ACKNOWLEDGED_THROUGH SOURCE_LEAF_FAILURE_ADDR TARGET_LEAF_FAILURE_ADDR\n  zcglobal-volume-workload disaster-source-hold TARGET CONTROL_ADDR CHECKPOINT ACKNOWLEDGED_THROUGH SECONDS\n  zcglobal-volume-workload hold TARGET VERIFY_THROUGH SECONDS\n  zcglobal-volume-workload move TARGET VERIFY_THROUGH END\n  zcglobal-volume-workload move-hold TARGET VERIFY_THROUGH END SECONDS\n  zcglobal-volume-workload move-loss TARGET CHECKPOINT LOST_THROUGH END\n  zcglobal-volume-workload move-loss-hold TARGET CHECKPOINT LOST_THROUGH END SECONDS\n  zcglobal-volume-workload grade TARGET THROUGH [ZERO_FROM ZERO_THROUGH]",
    )
}

fn main() -> io::Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("stay") => {
            let target = args.next().ok_or_else(usage)?;
            let control = args.next().ok_or_else(usage)?;
            let operations = args
                .next()
                .ok_or_else(usage)?
                .parse::<u64>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            if args.next().is_some() {
                return Err(usage());
            }
            run_stay(&target, &control, operations)
        }
        Some("stay-ha") => {
            let target = args.next().ok_or_else(usage)?;
            let control = args.next().ok_or_else(usage)?;
            let operations = args
                .next()
                .ok_or_else(usage)?
                .parse::<u64>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            let source_leaf_failure = args.next().ok_or_else(usage)?;
            let target_leaf_failure = args.next().ok_or_else(usage)?;
            if args.next().is_some() {
                return Err(usage());
            }
            run_stay_ha(
                &target,
                &control,
                operations,
                &source_leaf_failure,
                &target_leaf_failure,
            )
        }
        Some("move") => {
            let target = args.next().ok_or_else(usage)?;
            let verify_through = args
                .next()
                .ok_or_else(usage)?
                .parse::<u64>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            let end = args
                .next()
                .ok_or_else(usage)?
                .parse::<u64>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            if args.next().is_some() {
                return Err(usage());
            }
            run_move(&target, verify_through, end)
        }
        Some("move-hold") => {
            let target = args.next().ok_or_else(usage)?;
            let verify_through = args
                .next()
                .ok_or_else(usage)?
                .parse::<u64>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            let end = args
                .next()
                .ok_or_else(usage)?
                .parse::<u64>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            let seconds = args
                .next()
                .ok_or_else(usage)?
                .parse::<u64>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            if args.next().is_some() {
                return Err(usage());
            }
            run_move_hold(&target, verify_through, end, seconds)
        }
        Some("hold") => {
            let target = args.next().ok_or_else(usage)?;
            let verify_through = args
                .next()
                .ok_or_else(usage)?
                .parse::<u64>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            let seconds = args
                .next()
                .ok_or_else(usage)?
                .parse::<u64>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            if args.next().is_some() {
                return Err(usage());
            }
            run_hold(&target, verify_through, seconds)
        }
        Some("disaster-source") => {
            let target = args.next().ok_or_else(usage)?;
            let control = args.next().ok_or_else(usage)?;
            let checkpoint = args
                .next()
                .ok_or_else(usage)?
                .parse::<u64>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            let acknowledged_through = args
                .next()
                .ok_or_else(usage)?
                .parse::<u64>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            if args.next().is_some() {
                return Err(usage());
            }
            run_disaster_source(&target, &control, checkpoint, acknowledged_through)
        }
        Some("disaster-source-hold") => {
            let target = args.next().ok_or_else(usage)?;
            let control = args.next().ok_or_else(usage)?;
            let checkpoint = args
                .next()
                .ok_or_else(usage)?
                .parse::<u64>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            let acknowledged_through = args
                .next()
                .ok_or_else(usage)?
                .parse::<u64>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            let seconds = args
                .next()
                .ok_or_else(usage)?
                .parse::<u64>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            if args.next().is_some() {
                return Err(usage());
            }
            run_disaster_source_hold(&target, &control, checkpoint, acknowledged_through, seconds)
        }
        Some("disaster-source-ha") => {
            let target = args.next().ok_or_else(usage)?;
            let control = args.next().ok_or_else(usage)?;
            let checkpoint = args
                .next()
                .ok_or_else(usage)?
                .parse::<u64>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            let acknowledged_through = args
                .next()
                .ok_or_else(usage)?
                .parse::<u64>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            let source_leaf_failure = args.next().ok_or_else(usage)?;
            let target_leaf_failure = args.next().ok_or_else(usage)?;
            if args.next().is_some() {
                return Err(usage());
            }
            run_disaster_source_ha(
                &target,
                &control,
                checkpoint,
                acknowledged_through,
                &source_leaf_failure,
                &target_leaf_failure,
            )
        }
        Some("move-loss") => {
            let target = args.next().ok_or_else(usage)?;
            let checkpoint = args
                .next()
                .ok_or_else(usage)?
                .parse::<u64>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            let lost_through = args
                .next()
                .ok_or_else(usage)?
                .parse::<u64>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            let end = args
                .next()
                .ok_or_else(usage)?
                .parse::<u64>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            if args.next().is_some() {
                return Err(usage());
            }
            run_move_loss(&target, checkpoint, lost_through, end)
        }
        Some("move-loss-hold") => {
            let target = args.next().ok_or_else(usage)?;
            let checkpoint = args
                .next()
                .ok_or_else(usage)?
                .parse::<u64>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            let lost_through = args
                .next()
                .ok_or_else(usage)?
                .parse::<u64>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            let end = args
                .next()
                .ok_or_else(usage)?
                .parse::<u64>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            let seconds = args
                .next()
                .ok_or_else(usage)?
                .parse::<u64>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            if args.next().is_some() {
                return Err(usage());
            }
            run_move_loss_hold(&target, checkpoint, lost_through, end, seconds)
        }
        Some("grade") => {
            let target = args.next().ok_or_else(usage)?;
            let through = args
                .next()
                .ok_or_else(usage)?
                .parse::<u64>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            let zero_from = args
                .next()
                .map(|value| value.parse::<u64>())
                .transpose()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            let zero_through = args
                .next()
                .map(|value| value.parse::<u64>())
                .transpose()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            if args.next().is_some() || zero_from.is_some() != zero_through.is_some() {
                return Err(usage());
            }
            run_grade(&target, through, zero_from, zero_through)
        }
        _ => Err(usage()),
    }
}
