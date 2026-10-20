//! Darwin file admission for the shared WAL format and reducer.
//! Only local APFS/HFS regular files are admitted. Raw disks are never opened.
use super::*;
use std::ffi::CStr;

pub(super) fn validate_persistent_file(file: &File) -> io::Result<()> {
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::fstatfs(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    let name = unsafe { CStr::from_ptr(stat.f_fstypename.as_ptr()) }.to_bytes();
    if stat.f_flags & libc::MNT_LOCAL as u32 == 0 || !matches!(name, b"apfs" | b"hfs") {
        return Err(invalid(
            "persistent macOS WAL requires local APFS/HFS files",
        ));
    }
    Ok(())
}

pub(super) fn open_backing(
    path: &Path,
    required_bytes: u64,
    role: &str,
    provisioning: FileProvisioning,
    io_mode: BackingIoMode,
) -> io::Result<(File, BackingInfo)> {
    // F_NOCACHE is not O_DIRECT: reject rather than silently change its contract.
    if io_mode == BackingIoMode::Direct {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "macOS WAL supports buffered I/O with F_FULLFSYNC; O_DIRECT is unavailable",
        ));
    }
    let length = i64::try_from(required_bytes).map_err(|_| invalid("WAL size exceeds off_t"))?;
    match std::fs::symlink_metadata(path) {
        Ok(meta) if !meta.is_file() => {
            return Err(invalid("WAL backing must be an ordinary regular file"));
        }
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            options.create_new(true).open(path)?
        }
        Err(error) => return Err(error),
    };
    // Custody must precede allocation, resizing, and recovery, including aliases.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if !file.metadata()?.is_file() {
        return Err(invalid(format!(
            "WAL {role} must be an ordinary regular file"
        )));
    }
    validate_persistent_file(&file)?;
    if provisioning == FileProvisioning::Preallocate
        && file.metadata()?.blocks().saturating_mul(512) < required_bytes
    {
        let mut reserve = libc::fstore_t {
            fst_flags: libc::F_ALLOCATEALL,
            fst_posmode: libc::F_PEOFPOSMODE,
            fst_offset: 0,
            fst_length: length,
            fst_bytesalloc: 0,
        };
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PREALLOCATE, &mut reserve) } == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    if provisioning == FileProvisioning::Preallocate && file.metadata()?.len() < required_bytes {
        file.set_len(required_bytes)?;
    }
    let metadata = file.metadata()?;
    if metadata.len() < required_bytes || metadata.blocks().saturating_mul(512) < required_bytes {
        return Err(invalid(format!("WAL {role} is not fully allocated")));
    }
    Ok((
        file,
        BackingInfo {
            kind: BackingKind::RegularFile,
            available_bytes: metadata.len(),
            required_bytes,
            allocated_extents: 0,
            allocation_evidence: AllocationEvidence::MacOsPreallocation,
        },
    ))
}

pub(super) fn reject_same_backing(journal: &File, base: &File) -> io::Result<()> {
    let j = journal.metadata()?;
    let b = base.metadata()?;
    if (j.dev(), j.ino()) == (b.dev(), b.ino()) {
        return Err(invalid(
            "persistent WAL journal and base must use distinct backing stores",
        ));
    }
    Ok(())
}
