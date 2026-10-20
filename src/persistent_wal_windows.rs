//! Windows custody and allocation admission for the shared persistent WAL.
//! Only ordinary NTFS/ReFS files are admitted; no raw disk handles are opened.
use super::*;
use std::ffi::c_void;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;

#[repr(C)]
#[derive(Default)]
struct StandardInfo {
    allocation_size: i64,
    end_of_file: i64,
    links: u32,
    delete_pending: u8,
    directory: u8,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetFileInformationByHandle(
        handle: *mut c_void,
        class: u32,
        info: *const c_void,
        size: u32,
    ) -> i32;
    fn GetFileInformationByHandleEx(
        handle: *mut c_void,
        class: u32,
        info: *mut c_void,
        size: u32,
    ) -> i32;
    fn GetFileInformationByHandle(handle: *mut c_void, info: *mut u32) -> i32;
    fn GetVolumeInformationByHandleW(
        handle: *mut c_void,
        name: *mut u16,
        name_len: u32,
        serial: *mut u32,
        max_component: *mut u32,
        flags: *mut u32,
        fs_name: *mut u16,
        fs_name_len: u32,
    ) -> i32;
}

pub(super) fn validate_persistent_file(file: &File) -> io::Result<()> {
    let mut fs_name = [0u16; 32];
    let success = unsafe {
        GetVolumeInformationByHandleW(
            file.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs_name.as_mut_ptr(),
            fs_name.len() as u32,
        )
    };
    if success == 0 {
        return Err(io::Error::last_os_error());
    }
    let end = fs_name
        .iter()
        .position(|c| *c == 0)
        .unwrap_or(fs_name.len());
    let name = String::from_utf16_lossy(&fs_name[..end]);
    if !matches!(name.as_str(), "NTFS" | "ReFS") {
        return Err(invalid(format!(
            "persistent WAL requires NTFS/ReFS files on Windows, found {name}"
        )));
    }
    // This checks filesystem capabilities, not the honesty of hardware caches
    // or whether a virtual disk ultimately resides on persistent host media.
    Ok(())
}

pub(super) fn open_backing(
    path: &Path,
    required_bytes: u64,
    role: &str,
    provisioning: FileProvisioning,
    io_mode: BackingIoMode,
) -> io::Result<(File, BackingInfo)> {
    let length = i64::try_from(required_bytes)
        .map_err(|_| invalid("WAL size exceeds Windows file limit"))?;
    let mut options = OpenOptions::new();
    // Deny other writers and deletion BEFORE sizing. Readers remain allowed
    // for alias detection. Keep the journal write-through. Base-image writes
    // are buffered until the reducer's explicit sync_data checkpoint, before
    // it advances the durable reduced prefix. Flushing every 4 KiB base write
    // defeats that batching and can stall an entire full-journal drain.
    options.read(true).write(true).share_mode(1).custom_flags(
        (if role == "journal" { 0x80000000 } else { 0 })
            | 0x00200000
            | if io_mode == BackingIoMode::Direct {
                0x20000000
            } else {
                0
            },
    );
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            options.create_new(true).open(path)?
        }
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    // OPEN_REPARSE_POINT prevents following a final symlink/junction.
    if !metadata.is_file() || metadata.file_attributes() & (0x200 | 0x800 | 0x400) != 0 {
        return Err(invalid(format!(
            "WAL {role} must be an ordinary, nonsparse, uncompressed file"
        )));
    }
    validate_persistent_file(&file)?;
    if provisioning == FileProvisioning::Preallocate {
        // Reserve through the existing EOF; never truncate existing data.
        let reserve =
            length.max(i64::try_from(metadata.len()).map_err(|_| invalid("file too large"))?);
        if unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                5,
                (&reserve as *const i64).cast(),
                std::mem::size_of::<i64>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    if metadata.len() < required_bytes {
        if provisioning == FileProvisioning::RequireAllocated {
            return Err(invalid("WAL file is smaller than required allocation"));
        }
        file.set_len(required_bytes)?;
    }
    let mut info = StandardInfo::default();
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            1,
            (&mut info as *mut StandardInfo).cast(),
            std::mem::size_of::<StandardInfo>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if info.allocation_size < length || info.end_of_file < length || info.directory != 0 {
        return Err(invalid("Windows WAL file is not fully allocated"));
    }
    let available_bytes = info.end_of_file as u64;
    Ok((
        file,
        BackingInfo {
            kind: BackingKind::RegularFile,
            available_bytes,
            required_bytes,
            allocated_extents: 0,
            allocation_evidence: AllocationEvidence::WindowsAllocation,
        },
    ))
}

pub(super) fn reject_same_backing(journal: &File, base: &File) -> io::Result<()> {
    fn identity(file: &File) -> io::Result<(u32, u32, u32)> {
        // BY_HANDLE_FILE_INFORMATION: DWORD fields, FILETIMEs each two DWORDs.
        let mut info = [0u32; 13];
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((info[7], info[11], info[12]))
    }
    if identity(journal)? == identity(base)? {
        return Err(invalid(
            "persistent WAL journal and base must use distinct backing stores",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn DeviceIoControl(
            handle: *mut c_void,
            code: u32,
            input: *mut c_void,
            input_len: u32,
            output: *mut c_void,
            output_len: u32,
            returned: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
    }

    #[test]
    fn sparse_windows_file_is_rejected_before_resize() {
        let root = std::env::temp_dir().join(format!(
            "zc-wal-sparse-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("sparse");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let mut returned = 0;
        assert_ne!(
            unsafe {
                DeviceIoControl(
                    file.as_raw_handle(),
                    0x900c4,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    0,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            },
            0
        );
        file.set_len(16384).unwrap();
        drop(file);
        for policy in [
            FileProvisioning::RequireAllocated,
            FileProvisioning::Preallocate,
        ] {
            assert!(open_backing(&path, 32768, "test", policy, BackingIoMode::Buffered).is_err());
            assert_eq!(std::fs::metadata(&path).unwrap().len(), 16384);
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
