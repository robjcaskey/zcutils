use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::thread;
use std::time::Duration;

use zcutils::persistent_wal::{
    AllocationEvidence, BackingIoMode, BackingKind, FileProvisioning, IntegrityMode, PersistentWal,
    PersistentWalOpenOptions,
};

const BLOCK: usize = 4096;
const DATA_START: u64 = (BLOCK * 2) as u64;
const LOGICAL_BYTES: u64 = 32 * 1024 * 1024;
const JOURNAL_BYTES: u64 = 32 * 1024 * 1024;

fn main() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let phase = args.next().ok_or_else(|| invalid("missing phase"))?;
    match phase.as_str() {
        "file-matrix" => file_matrix(required(&mut args, "directory")?),
        "block-init" => block_init(
            required(&mut args, "journal block")?,
            required(&mut args, "base block")?,
        ),
        "direct-block" => direct_block(
            required(&mut args, "journal block")?,
            required(&mut args, "base block")?,
        ),
        "crash-before-publish" => crash_before_publish(
            required(&mut args, "journal block")?,
            required(&mut args, "base block")?,
        ),
        "verify-before-publish" => verify_before_publish(
            required(&mut args, "journal block")?,
            required(&mut args, "base block")?,
        ),
        "crash-after-commit" => crash_after_commit(
            required(&mut args, "journal block")?,
            required(&mut args, "base block")?,
        ),
        "verify-after-commit" => verify_after_commit(
            required(&mut args, "journal block")?,
            required(&mut args, "base block")?,
        ),
        "crash-unsynced" => crash_unsynced(
            required(&mut args, "journal block")?,
            required(&mut args, "base block")?,
        ),
        "verify-unsynced" => verify_unsynced(
            required(&mut args, "journal block")?,
            required(&mut args, "base block")?,
        ),
        "corrupt-block" => corrupt_block(
            required(&mut args, "journal block")?,
            required(&mut args, "base block")?,
        ),
        _ => Err(invalid(format!("unknown phase {phase:?}"))),
    }
}

fn required(args: &mut impl Iterator<Item = String>, name: &str) -> io::Result<String> {
    args.next()
        .ok_or_else(|| invalid(format!("missing {name}")))
}

fn open(journal: impl AsRef<Path>, base: impl AsRef<Path>) -> io::Result<PersistentWal> {
    PersistentWal::open(journal, base, LOGICAL_BYTES, JOURNAL_BYTES)
}

fn page(value: u8) -> Vec<u8> {
    vec![value; BLOCK]
}

#[repr(align(4096))]
struct AlignedPage([u8; BLOCK]);

fn direct_block(journal: String, base: String) -> io::Result<()> {
    let options = PersistentWalOpenOptions {
        io_mode: BackingIoMode::Direct,
        ..PersistentWalOpenOptions::default()
    };
    let wal = PersistentWal::open_with_options(
        journal,
        base,
        LOGICAL_BYTES,
        JOURNAL_BYTES,
        IntegrityMode::Crc32c,
        options,
    )?;
    let payload = AlignedPage([0xd1; BLOCK]);
    wal.append_contiguous(0, &payload.0)?;
    wal.sync()?;
    let mut out = AlignedPage([0; BLOCK]);
    wal.read_at(0, &mut out.0)?;
    if out.0 != payload.0 || wal.stats().io_mode != BackingIoMode::Direct {
        return Err(invalid("direct terminal-block lifecycle mismatch"));
    }
    println!("ZCPWAL_QEMU_DIRECT_BLOCK_PASS");
    Ok(())
}

fn expect_page(wal: &PersistentWal, page_index: u64, value: u8) -> io::Result<()> {
    let mut out = vec![0u8; BLOCK];
    wal.read_at(page_index * BLOCK as u64, &mut out)?;
    if out != page(value) {
        return Err(invalid(format!(
            "page {page_index} mismatch: got first byte {:#x}, expected {value:#x}",
            out[0]
        )));
    }
    Ok(())
}

fn marker_then_wait(marker: &str) -> ! {
    println!("{marker}");
    io::stdout().flush().expect("flush QEMU marker");
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}

fn file_matrix(directory: String) -> io::Result<()> {
    let root = Path::new(&directory);
    fs::create_dir_all(root)?;
    let sparse_journal = root.join("sparse-journal");
    let sparse_base = root.join("sparse-base");
    File::create(&sparse_journal)?.set_len(JOURNAL_BYTES)?;
    File::create(&sparse_base)?.set_len(LOGICAL_BYTES)?;
    let strict = PersistentWalOpenOptions {
        file_provisioning: FileProvisioning::RequireAllocated,
        ..PersistentWalOpenOptions::default()
    };
    if PersistentWal::open_with_options(
        &sparse_journal,
        &sparse_base,
        LOGICAL_BYTES,
        JOURNAL_BYTES,
        IntegrityMode::Crc32c,
        strict,
    )
    .is_ok()
    {
        return Err(invalid("strict allocation admitted sparse files"));
    }
    println!("ZCPWAL_QEMU_SPARSE_REJECT_PASS");

    let wal = open(&sparse_journal, &sparse_base)?;
    let stats = wal.stats();
    if stats.journal_backing.kind != BackingKind::RegularFile
        || stats.base_backing.kind != BackingKind::RegularFile
        || stats.journal_backing.allocation_evidence != AllocationEvidence::Fiemap
        || stats.base_backing.allocation_evidence != AllocationEvidence::Fiemap
    {
        return Err(invalid(format!(
            "ext4 file admission did not use FIEMAP: journal={:?} base={:?}",
            stats.journal_backing, stats.base_backing
        )));
    }
    wal.append_contiguous(0, &page(0x51))?;
    wal.sync()?;
    drop(wal);
    let wal = PersistentWal::open_with_options(
        &sparse_journal,
        &sparse_base,
        LOGICAL_BYTES,
        JOURNAL_BYTES,
        IntegrityMode::Crc32c,
        strict,
    )?;
    expect_page(&wal, 0, 0x51)?;
    println!(
        "ZCPWAL_QEMU_FILE_PASS journal_extents={} base_extents={}",
        wal.stats().journal_backing.allocated_extents,
        wal.stats().base_backing.allocated_extents
    );
    drop(wal);

    let corrupt_journal = root.join("corrupt-journal");
    let corrupt_base = root.join("corrupt-base");
    {
        let wal = open(&corrupt_journal, &corrupt_base)?;
        wal.append_contiguous(0, &page(0x62))?;
        wal.sync()?;
    }
    let journal_file = OpenOptions::new().write(true).open(&corrupt_journal)?;
    journal_file.write_at(&[0x99], DATA_START + BLOCK as u64)?;
    journal_file.sync_data()?;
    if open(&corrupt_journal, &corrupt_base).is_ok() {
        return Err(invalid("CRC recovery admitted a corrupted file payload"));
    }
    println!("ZCPWAL_QEMU_FILE_CORRUPTION_PASS");

    let short_journal = root.join("short-journal");
    let short_base = root.join("short-base");
    {
        let wal = open(&short_journal, &short_base)?;
        wal.append_contiguous(0, &page(0x68))?;
        wal.sync()?;
    }
    OpenOptions::new()
        .write(true)
        .open(&short_journal)?
        .set_len(DATA_START + BLOCK as u64 + (BLOCK / 2) as u64)?;
    if open(&short_journal, &short_base).is_ok() {
        return Err(invalid("recovery admitted a shortened committed frame"));
    }
    println!("ZCPWAL_QEMU_SHORT_COMMITTED_FRAME_PASS");

    let partial_journal = root.join("partial-journal");
    let partial_base = root.join("partial-base");
    {
        let wal = open(&partial_journal, &partial_base)?;
        wal.append_contiguous(0, &page(0x73))?;
    }
    let wal = open(&partial_journal, &partial_base)?;
    expect_page(&wal, 0, 0)?;
    println!("ZCPWAL_QEMU_PARTIAL_TAIL_PASS");

    let enospc_journal = root.join("enospc-journal");
    let enospc_base = root.join("enospc-base");
    let oversized = 256 * 1024 * 1024;
    let error = PersistentWal::open(&enospc_journal, &enospc_base, oversized, oversized)
        .err()
        .ok_or_else(|| invalid("oversized preallocation unexpectedly succeeded"))?;
    if error.raw_os_error() != Some(libc::ENOSPC)
        && !error.to_string().contains("No space left on device")
    {
        return Err(invalid(format!(
            "expected ENOSPC during admission, got: {error}"
        )));
    }
    println!("ZCPWAL_QEMU_ENOSPC_AT_OPEN_PASS");
    Ok(())
}

fn block_init(journal: String, base: String) -> io::Result<()> {
    let wal = open(journal, base)?;
    let stats = wal.stats();
    if stats.journal_backing.kind != BackingKind::BlockDevice
        || stats.base_backing.kind != BackingKind::BlockDevice
    {
        return Err(invalid("raw backing was not classified as block devices"));
    }
    wal.append_contiguous(0, &page(0x11))?;
    wal.sync()?;
    expect_page(&wal, 0, 0x11)?;
    println!(
        "ZCPWAL_QEMU_BLOCK_INIT_PASS journal_bytes={} base_bytes={}",
        stats.journal_backing.available_bytes, stats.base_backing.available_bytes
    );
    Ok(())
}

fn crash_before_publish(journal: String, base: String) -> io::Result<()> {
    let wal = open(journal, base)?;
    wal.append_contiguous(BLOCK as u64, &page(0x22))?;
    wal.sync_with_payload_durable_hook(|| marker_then_wait("ZCPWAL_CRASH_PAYLOAD_DURABLE"))?;
    unreachable!()
}

fn verify_before_publish(journal: String, base: String) -> io::Result<()> {
    let wal = open(journal, base)?;
    expect_page(&wal, 0, 0x11)?;
    expect_page(&wal, 1, 0)?;
    println!("ZCPWAL_VERIFY_OLD_PREFIX_PASS");
    Ok(())
}

fn crash_after_commit(journal: String, base: String) -> io::Result<()> {
    let wal = open(journal, base)?;
    wal.append_contiguous((BLOCK * 2) as u64, &page(0x33))?;
    wal.sync()?;
    marker_then_wait("ZCPWAL_CRASH_COMMIT_DURABLE")
}

fn verify_after_commit(journal: String, base: String) -> io::Result<()> {
    let wal = open(journal, base)?;
    expect_page(&wal, 0, 0x11)?;
    expect_page(&wal, 1, 0)?;
    expect_page(&wal, 2, 0x33)?;
    println!("ZCPWAL_VERIFY_NEW_PREFIX_PASS");
    Ok(())
}

fn crash_unsynced(journal: String, base: String) -> io::Result<()> {
    let wal = open(journal, base)?;
    wal.append_contiguous((BLOCK * 3) as u64, &page(0x44))?;
    marker_then_wait("ZCPWAL_CRASH_UNSYNCED_APPENDED")
}

fn verify_unsynced(journal: String, base: String) -> io::Result<()> {
    let wal = open(journal, base)?;
    expect_page(&wal, 2, 0x33)?;
    expect_page(&wal, 3, 0)?;
    println!("ZCPWAL_VERIFY_UNSYNCED_IGNORED_PASS");
    Ok(())
}

fn corrupt_block(journal: String, base: String) -> io::Result<()> {
    let wal = open(&journal, &base)?;
    let payload_offset = DATA_START
        .checked_add(wal.stats().journal_used_bytes)
        .and_then(|offset| offset.checked_add(BLOCK as u64))
        .ok_or_else(|| invalid("corruption offset overflow"))?;
    wal.append_contiguous((BLOCK * 4) as u64, &page(0x55))?;
    wal.sync()?;
    drop(wal);
    let journal_file = OpenOptions::new().write(true).open(&journal)?;
    journal_file.write_at(&[0xa6], payload_offset)?;
    journal_file.sync_data()?;
    let error = open(&journal, &base)
        .err()
        .ok_or_else(|| invalid("CRC recovery admitted corrupted block payload"))?;
    if !error.to_string().contains("payload checksum mismatch") {
        return Err(invalid(format!("unexpected corruption error: {error}")));
    }
    println!("ZCPWAL_QEMU_BLOCK_CORRUPTION_PASS");
    Ok(())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
