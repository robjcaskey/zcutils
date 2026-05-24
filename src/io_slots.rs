#![allow(dead_code)]

use std::io;
use std::path::PathBuf;
use std::process::Command;

use crate::{IoUringSqe, io_uring_register};

pub(crate) const IORING_OP_SLOT_RW: u32 = 65;
pub(crate) const IORING_REGISTER_IO_SLOT: u32 = 38;
pub(crate) const IORING_UNREGISTER_IO_SLOT: u32 = 39;
pub(crate) const IORING_SLOT_RW_WRITE: u32 = 1 << 0;

const SLOT_HELPER_SYMBOLS: &[&str] = &[
    "io_uring_prep_slot_rw",
    "io_uring_register_io_slot",
    "io_uring_unregister_io_slot",
    "io_uring_slot_reg",
];
const SLOT_RW_ALIGNMENT: u64 = 512;

#[derive(Debug)]
pub(crate) struct LiburingSlotHelperProbe {
    pub(crate) version: Option<String>,
    pub(crate) header: Option<PathBuf>,
    pub(crate) checked_headers: Vec<PathBuf>,
    pub(crate) missing_symbols: Vec<&'static str>,
}

impl LiburingSlotHelperProbe {
    pub(crate) fn available(&self) -> bool {
        self.header.is_some() && self.missing_symbols.is_empty()
    }

    pub(crate) fn summary(&self) -> String {
        let available = if self.available() { "yes" } else { "no" };
        let version = self.version.as_deref().unwrap_or("unknown");
        let header = self
            .header
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "not-found".to_string());
        let missing = if self.missing_symbols.is_empty() {
            "none".to_string()
        } else {
            self.missing_symbols.join(",")
        };
        let checked = if self.checked_headers.is_empty() {
            "none".to_string()
        } else {
            self.checked_headers
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(",")
        };

        format!(
            "available={available} version={version} header={header} missing={missing} \
             checked={checked}"
        )
    }
}

pub(crate) fn submission_backend_label() -> &'static str {
    "zcutils/io-slot-v0(raw-uapi)"
}

pub(crate) fn local_compat_summary() -> String {
    format!(
        "available=yes api=zcutils/io-slot-v0 backend=raw-uapi op={} register={}/{} alignment={}",
        IORING_OP_SLOT_RW, IORING_REGISTER_IO_SLOT, IORING_UNREGISTER_IO_SLOT, SLOT_RW_ALIGNMENT
    )
}

pub(crate) fn liburing_slot_helper_probe() -> LiburingSlotHelperProbe {
    let version = pkg_config_value(&["--modversion", "liburing"]);
    let mut candidates = Vec::new();

    if let Some(include_dir) = pkg_config_value(&["--variable=includedir", "liburing"]) {
        let include_dir = PathBuf::from(include_dir);
        candidates.push(include_dir.join("liburing.h"));
        candidates.push(include_dir.join("liburing/io_uring.h"));
    }
    candidates.extend([
        PathBuf::from("/usr/local/include/liburing.h"),
        PathBuf::from("/usr/local/include/liburing/io_uring.h"),
        PathBuf::from("/usr/include/liburing.h"),
        PathBuf::from("/usr/include/liburing/io_uring.h"),
        PathBuf::from("/home/rob/src/liburing/src/include/liburing.h"),
        PathBuf::from("/home/rob/src/liburing/src/include/liburing/io_uring.h"),
    ]);
    candidates.sort();
    candidates.dedup();

    let mut checked_headers = Vec::new();
    let mut best_header = None;
    let mut best_missing = SLOT_HELPER_SYMBOLS.to_vec();

    for path in candidates {
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        checked_headers.push(path.clone());
        let missing = SLOT_HELPER_SYMBOLS
            .iter()
            .copied()
            .filter(|symbol| !contents.contains(symbol))
            .collect::<Vec<_>>();
        if best_header.is_none() || missing.len() < best_missing.len() {
            best_header = Some(path);
            best_missing = missing;
        }
        if best_missing.is_empty() {
            break;
        }
    }

    LiburingSlotHelperProbe {
        version,
        header: best_header,
        checked_headers,
        missing_symbols: best_missing,
    }
}

fn pkg_config_value(args: &[&str]) -> Option<String> {
    let output = Command::new("pkg-config").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }

    String::from_utf8(output.stdout)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[repr(C)]
#[derive(Default, Clone, Copy, Debug)]
pub(crate) struct IoUringSlotReg {
    pub(crate) buf_index: u32,
    pub(crate) file_index: u32,
    pub(crate) resv: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct IoSlotId(u16);

impl IoSlotId {
    fn new(slot_id: u32) -> io::Result<Self> {
        let slot_id = u16::try_from(slot_id).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "kernel returned slot id that exceeds SQE buf_index width",
            )
        })?;
        Ok(Self(slot_id))
    }

    pub(crate) fn as_u32(self) -> u32 {
        self.0 as u32
    }

    fn as_u16(self) -> u16 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SlotDescriptor {
    buf_index: u32,
    file_index: u32,
}

impl SlotDescriptor {
    pub(crate) fn new(buf_index: u32, file_index: u32) -> Self {
        Self {
            buf_index,
            file_index,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SlotRw {
    Read,
    Write,
}

impl SlotRw {
    fn rw_flags(self) -> u32 {
        match self {
            SlotRw::Read => 0,
            SlotRw::Write => IORING_SLOT_RW_WRITE,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SlotRwRequest {
    slot_id: IoSlotId,
    buf_offset: u64,
    file_offset: u64,
    len: u32,
    direction: SlotRw,
}

impl SlotRwRequest {
    pub(crate) fn new(
        slot_id: IoSlotId,
        buf_offset: u64,
        file_offset: u64,
        len: u32,
        direction: SlotRw,
    ) -> io::Result<Self> {
        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "slot rw len must be non-zero",
            ));
        }
        if buf_offset % SLOT_RW_ALIGNMENT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("slot rw buffer offset must be {SLOT_RW_ALIGNMENT}-byte aligned"),
            ));
        }
        if file_offset % SLOT_RW_ALIGNMENT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("slot rw file offset must be {SLOT_RW_ALIGNMENT}-byte aligned"),
            ));
        }
        if (len as u64) % SLOT_RW_ALIGNMENT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("slot rw len must be {SLOT_RW_ALIGNMENT}-byte aligned"),
            ));
        }

        Ok(Self {
            slot_id,
            buf_offset,
            file_offset,
            len,
            direction,
        })
    }
}

pub(crate) fn register_io_slot(ring_fd: i32, descriptor: SlotDescriptor) -> io::Result<IoSlotId> {
    let mut reg = IoUringSlotReg {
        buf_index: descriptor.buf_index,
        file_index: descriptor.file_index,
        ..IoUringSlotReg::default()
    };
    let slot_id = io_uring_register(
        ring_fd,
        IORING_REGISTER_IO_SLOT,
        &mut reg as *mut IoUringSlotReg as *mut libc::c_void,
        1,
    )?;

    u32::try_from(slot_id)
        .map_err(|_| io::Error::other("kernel returned negative or invalid slot id"))
        .and_then(IoSlotId::new)
}

pub(crate) fn unregister_io_slot(ring_fd: i32, slot_id: IoSlotId) -> io::Result<()> {
    let mut slot_id = slot_id.as_u32();
    io_uring_register(
        ring_fd,
        IORING_UNREGISTER_IO_SLOT,
        &mut slot_id as *mut u32 as *mut libc::c_void,
        1,
    )?;
    Ok(())
}

pub(crate) fn prep_slot_rw_request(sqe: &mut IoUringSqe, request: SlotRwRequest) {
    sqe.opcode = IORING_OP_SLOT_RW as u8;
    sqe.buf_index = request.slot_id.as_u16();
    sqe.addr = request.buf_offset;
    sqe.off = request.file_offset;
    sqe.len = request.len;
    sqe.rw_flags = request.direction.rw_flags();
}

pub(crate) fn prep_slot_rw(
    sqe: &mut IoUringSqe,
    slot_id: u16,
    buf_offset: u64,
    file_offset: u64,
    len: u32,
    direction: SlotRw,
) {
    let request = SlotRwRequest::new(IoSlotId(slot_id), buf_offset, file_offset, len, direction)
        .expect("legacy prep_slot_rw called with invalid slot request");
    prep_slot_rw_request(sqe, request);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, size_of};

    #[test]
    fn slot_reg_layout_matches_uapi() {
        assert_eq!(size_of::<IoUringSlotReg>(), 16);
        assert_eq!(align_of::<IoUringSlotReg>(), 8);
    }

    #[test]
    fn prep_slot_rw_sets_expected_sqe_fields() {
        let mut sqe = IoUringSqe::default();
        prep_slot_rw(&mut sqe, 7, 4096, 8192, 16384, SlotRw::Write);

        assert_eq!(sqe.opcode, IORING_OP_SLOT_RW as u8);
        assert_eq!(sqe.buf_index, 7);
        assert_eq!(sqe.addr, 4096);
        assert_eq!(sqe.off, 8192);
        assert_eq!(sqe.len, 16384);
        assert_eq!(sqe.rw_flags, IORING_SLOT_RW_WRITE);
    }

    #[test]
    fn slot_rw_request_rejects_unaligned_offsets() {
        let slot = IoSlotId::new(1).unwrap();
        assert!(SlotRwRequest::new(slot, 1, 0, 4096, SlotRw::Read).is_err());
        assert!(SlotRwRequest::new(slot, 0, 1, 4096, SlotRw::Read).is_err());
        assert!(SlotRwRequest::new(slot, 0, 0, 513, SlotRw::Read).is_err());
    }

    #[test]
    fn local_compat_summary_identifies_owned_backend() {
        let summary = local_compat_summary();
        assert!(summary.contains("zcutils/io-slot-v0"));
        assert!(summary.contains("backend=raw-uapi"));
    }
}
