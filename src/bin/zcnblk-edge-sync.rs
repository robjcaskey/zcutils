use std::env;
use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::time::Instant;

fn identity(fd: i32) -> io::Result<(u64, u64)> {
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((stat.st_dev, stat.st_ino))
}

fn main() -> io::Result<()> {
    let mut args = env::args().skip(1);
    let target = args.next().unwrap_or_else(|| "/dev/zcnblk0".to_string());
    if args.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: zcnblk-edge-sync [TARGET]",
        ));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(&target)?;
    let before = identity(file.as_raw_fd())?;
    let started = Instant::now();
    file.sync_data()?;
    let elapsed_ns = started.elapsed().as_nanos();
    let after = identity(file.as_raw_fd())?;
    if before != after {
        return Err(io::Error::other(
            "block edge identity changed across the global HWM drain",
        ));
    }
    println!(
        "zcnblk-edge-sync: target={target} completion_semantics=remote-global-sync-drain elapsed_ns={elapsed_ns} device={} inode={} identity_stable=true",
        before.0, before.1
    );
    Ok(())
}
