use std::env;
use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, Write};
use std::net::TcpStream;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::ptr::NonNull;
use std::slice;
use std::time::Instant;

const PAGE: usize = 4096;
const SLOTS: u64 = 1024;

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

fn identity(fd: i32) -> io::Result<(u64, u64)> {
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((stat.st_dev, stat.st_ino))
}

fn fill(page: &mut [u8], sequence: u64) {
    page[..8].copy_from_slice(&sequence.to_le_bytes());
    for (index, byte) in page[8..].iter_mut().enumerate() {
        *byte = (sequence as u8).wrapping_mul(131).wrapping_add(index as u8);
    }
}

fn switch(control: &str) -> io::Result<String> {
    let mut stream = TcpStream::connect(control)?;
    stream.write_all(b"secondary\n")?;
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response)?;
    if !response.starts_with("OK active=secondary") {
        return Err(io::Error::other(format!(
            "route switch failed: {}",
            response.trim()
        )));
    }
    Ok(response.trim().to_string())
}

fn main() -> io::Result<()> {
    let mut args = env::args().skip(1);
    let target = args.next().unwrap_or_else(|| "/dev/zcnblk0".to_string());
    let control = args.next().unwrap_or_else(|| "127.0.0.1:29110".to_string());
    let operations = args
        .next()
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
        .unwrap_or(4096);
    if operations < 4 || args.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: zcnblk-live-cutover-smoke [TARGET] [CONTROL_ADDR] [OPERATIONS>=4]",
        ));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT | libc::O_CLOEXEC)
        .open(&target)?;
    let fd = file.as_raw_fd();
    let identity_before = identity(fd)?;
    let mut write_page = AlignedPage::new()?;
    let mut read_page = AlignedPage::new()?;
    let mut max_operation_us = 0u128;
    let mut switch_response = String::new();
    let started = Instant::now();
    for sequence in 1..=operations {
        let offset = (sequence % SLOTS) * PAGE as u64;
        fill(write_page.as_mut_slice(), sequence);
        let operation_started = Instant::now();
        if file.write_at(write_page.as_slice(), offset)? != PAGE {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "short live write"));
        }
        file.sync_data()?;
        read_page.as_mut_slice().fill(0);
        if file.read_at(read_page.as_mut_slice(), offset)? != PAGE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "short live read",
            ));
        }
        if read_page.as_slice() != write_page.as_slice() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("read-after-sync mismatch at acknowledged sequence {sequence}"),
            ));
        }
        max_operation_us = max_operation_us.max(operation_started.elapsed().as_micros());
        if sequence == operations / 2 {
            switch_response = switch(&control)?;
        }
    }
    let identity_after = identity(fd)?;
    if identity_after != identity_before {
        return Err(io::Error::other(
            "open block-device identity changed during cutover",
        ));
    }
    println!(
        "ZCNBLK_LIVE_CUTOVER_PASS operations={operations} acknowledged_sequences=1..{operations} fd={fd} device={} inode={} reconnects=0 remounts=0 max_write_sync_read_us={} elapsed_ms={} switch=\"{}\"",
        identity_before.0,
        identity_before.1,
        max_operation_us,
        started.elapsed().as_millis(),
        switch_response
    );
    Ok(())
}
