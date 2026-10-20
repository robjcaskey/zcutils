//! Portable diagnostic client; all placement remains behind the TCP endpoint.
use std::env;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::time::Duration;
use zcutils::block::client::{MAX_IO_BYTES, ZcnblkClient};

const USAGE: &str = "usage: zcnblk-client read ADDRESS OFFSET LENGTH OUTPUT | write ADDRESS OFFSET INPUT | verify ADDRESS OFFSET INPUT OUTPUT | sync ADDRESS | backup ADDRESS BYTES NEW_FILE --source-quiesced | restore ADDRESS FILE\nADDRESS is a numeric IP:port for one plain TCP lane; OFFSET and LENGTH are decimal bytes, aligned to 4096. write and verify issue SYNC after WRITE_ACK.";

fn input_error(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn main() -> io::Result<()> {
    zcutils::crypto_policy::initialize_or_exit();
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args.as_slice() == ["--help"] || args.as_slice() == ["-h"] {
        println!("{USAGE}");
        return Ok(());
    }
    if args
        .first()
        .is_some_and(|arg| arg == "backup" || arg == "restore")
    {
        return image_transfer(&args);
    }
    let valid_shape = match args.first().map(String::as_str) {
        Some("read" | "verify") => args.len() == 5,
        Some("write") => args.len() == 4,
        Some("sync") => args.len() == 2,
        _ => false,
    };
    if !valid_shape {
        return Err(input_error(USAGE));
    }
    let address = args[1]
        .parse::<SocketAddr>()
        .map_err(|e| input_error(e.to_string()))?;
    let offset = if args[0] == "sync" {
        0
    } else {
        args[2]
            .parse::<u64>()
            .map_err(|e| input_error(e.to_string()))?
    };
    let mut data = match args[0].as_str() {
        "write" | "verify" => {
            let mut data = Vec::new();
            File::open(&args[3])?
                .take(MAX_IO_BYTES as u64 + 1)
                .read_to_end(&mut data)?;
            if data.len() > MAX_IO_BYTES {
                return Err(input_error("input exceeds 16 MiB"));
            }
            data
        }
        "read" => {
            let len = args[3]
                .parse::<usize>()
                .map_err(|e| input_error(e.to_string()))?;
            if len == 0 || len > MAX_IO_BYTES {
                return Err(input_error("read length must be 1..=16 MiB"));
            }
            vec![0; len]
        }
        _ => Vec::new(),
    };
    // Validate all local arguments before sending any request.
    if args[0] != "sync"
        && (data.is_empty()
            || data.len() % 4096 != 0
            || offset % 4096 != 0
            || offset.checked_add(data.len() as u64).is_none())
    {
        return Err(input_error(
            "offset and nonempty I/O length must be 4096-byte aligned and within u64 offsets",
        ));
    }
    let mut client = ZcnblkClient::connect(address, Duration::from_secs(15))?;
    match args[0].as_str() {
        "sync" => client.sync()?,
        "read" => {
            client.read_at(offset, &mut data)?;
            fs::write(&args[4], &data)?;
        }
        "write" | "verify" => {
            client.write_at(offset, &data)?;
            client.sync()?;
            if args[0] == "verify" {
                let mut actual = vec![0; data.len()];
                client.read_at(offset, &mut actual)?;
                fs::write(&args[4], &actual)?;
                if actual != data {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "readback differs from input",
                    ));
                }
            }
        }
        _ => unreachable!(),
    }
    println!(
        "zcnblk-client: OK operation={} address={address} offset={offset} bytes={} placement=remote-userspace",
        args[0],
        data.len()
    );
    Ok(())
}

/// Explicit full-image transfer: the caller must quiesce the source. This is
/// not a substitute for a live, consistent snapshot or a replication receipt.
fn image_transfer(args: &[String]) -> io::Result<()> {
    use sha2::{Digest, Sha256};
    let backup = args[0] == "backup";
    if (backup && (args.len() != 5 || args[4] != "--source-quiesced"))
        || (!backup && args.len() != 3)
    {
        return Err(input_error(USAGE));
    }
    let address = args[1]
        .parse::<SocketAddr>()
        .map_err(|e| input_error(e.to_string()))?;
    let path = std::path::Path::new(&args[if backup { 3 } else { 2 }]);
    let bytes = if backup {
        args[2]
            .parse::<u64>()
            .map_err(|e| input_error(e.to_string()))?
    } else {
        fs::metadata(path)?.len()
    };
    if bytes == 0 || bytes % 4096 != 0 {
        return Err(input_error(
            "image capacity must be nonzero and aligned to 4096 bytes",
        ));
    }
    let mut file = if backup {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?
    } else {
        File::open(path)?
    };
    let result = (|| {
        let mut client = ZcnblkClient::connect(address, Duration::from_secs(30))?;
        let mut digest = Sha256::new();
        let mut data = vec![0; 1024 * 1024];
        let mut offset = 0;
        while offset < bytes {
            let length = (bytes - offset).min(data.len() as u64) as usize;
            let page = &mut data[..length];
            if backup {
                client.read_at(offset, page)?;
                file.write_all(page)?;
            } else {
                file.read_exact(page)?;
                client.write_at(offset, page)?;
            }
            digest.update(page);
            offset += length as u64;
        }
        if backup {
            file.sync_all()?;
            #[cfg(target_os = "macos")]
            {
                use std::os::fd::AsRawFd;
                if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
            #[cfg(unix)]
            {
                let parent = path
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or_else(|| std::path::Path::new("."));
                File::open(parent)?.sync_all()?;
            }
        } else {
            client.sync()?;
        }
        println!(
            "zcnblk-client: OK operation={} bytes={bytes} sha256={:x} source_consistency=caller-quiesced",
            args[0],
            digest.finalize()
        );
        Ok(())
    })();
    // Do not leave an apparently usable backup after a failed transfer.
    if backup && result.is_err() {
        drop(file);
        let _ = fs::remove_file(path);
    }
    result
}
