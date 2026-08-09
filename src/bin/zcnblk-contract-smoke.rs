use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::ptr;

const BLOCK_BYTES: usize = 4096;
const IOPRIO_WHO_PROCESS: libc::c_int = 1;
const IOPRIO_CLASS_BE: libc::c_int = 2;
const IOPRIO_LEVEL: libc::c_int = 1;
const F_SET_RW_HINT: libc::c_int = 1036;
const F_SET_FILE_RW_HINT: libc::c_int = 1038;
const RWH_WRITE_LIFE_SHORT: u64 = 2;

struct AlignedBuffer(*mut u8);

impl AlignedBuffer {
    fn new(fill: u8) -> io::Result<Self> {
        let mut ptr = ptr::null_mut();
        let ret = unsafe { libc::posix_memalign(&mut ptr, BLOCK_BYTES, BLOCK_BYTES) };
        if ret != 0 {
            return Err(io::Error::from_raw_os_error(ret));
        }
        unsafe { ptr::write_bytes(ptr.cast::<u8>(), fill, BLOCK_BYTES) };
        Ok(Self(ptr.cast()))
    }

    fn as_ptr(&self) -> *mut u8 {
        self.0
    }
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        unsafe { libc::free(self.0.cast()) };
    }
}

fn set_ioprio() -> io::Result<u16> {
    let ioprio = (IOPRIO_CLASS_BE << 13) | IOPRIO_LEVEL;
    let ret = unsafe { libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, ioprio) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ioprio as u16)
}

fn set_write_lifetime(fd: libc::c_int) -> io::Result<&'static str> {
    let hint = RWH_WRITE_LIFE_SHORT;
    for (command, label) in [
        (F_SET_FILE_RW_HINT, "file-short"),
        (F_SET_RW_HINT, "inode-short"),
    ] {
        let ret = unsafe { libc::fcntl(fd, command, &hint) };
        if ret == 0 {
            return Ok(label);
        }
    }
    Err(io::Error::last_os_error())
}

fn main() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let target = args.next().unwrap_or_else(|| "/dev/zcnblk0".to_string());
    let block = args
        .next()
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
        .unwrap_or(7);
    if args.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: zcnblk-contract-smoke [target] [4k-block]",
        ));
    }
    let offset = block
        .checked_mul(BLOCK_BYTES as u64)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "block offset overflow"))?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT)
        .open(&target)?;
    let ioprio = set_ioprio()?;
    let write_lifetime = set_write_lifetime(file.as_raw_fd())
        .unwrap_or("unsupported-by-vfs")
        .to_string();
    let write = AlignedBuffer::new(0xaa)?;
    let read = AlignedBuffer::new(0)?;
    let iov = libc::iovec {
        iov_base: write.as_ptr().cast(),
        iov_len: BLOCK_BYTES,
    };
    let wrote = unsafe {
        libc::pwritev2(
            file.as_raw_fd(),
            &iov,
            1,
            offset as libc::off_t,
            libc::RWF_DSYNC,
        )
    };
    if wrote < 0 {
        return Err(io::Error::last_os_error());
    }
    if wrote as usize != BLOCK_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            format!("short RWF_DSYNC write: {wrote}"),
        ));
    }
    let read_bytes = unsafe {
        libc::pread(
            file.as_raw_fd(),
            read.as_ptr().cast(),
            BLOCK_BYTES,
            offset as libc::off_t,
        )
    };
    if read_bytes < 0 {
        return Err(io::Error::last_os_error());
    }
    if read_bytes as usize != BLOCK_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("short readback: {read_bytes}"),
        ));
    }
    let matches =
        unsafe { libc::memcmp(write.as_ptr().cast(), read.as_ptr().cast(), BLOCK_BYTES) == 0 };
    if !matches {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "RWF_DSYNC readback mismatch",
        ));
    }
    println!(
        "zcnblk-contract-smoke: PASS target={target} block={block} fua=RWF_DSYNC ioprio={ioprio:#06x} write_lifetime={write_lifetime} readback=true"
    );
    Ok(())
}
