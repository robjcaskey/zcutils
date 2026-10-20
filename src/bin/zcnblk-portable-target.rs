use std::{env, io, net::TcpListener, path::Path, sync::Arc};
use zcutils::block::portable_target::Target;

const USAGE: &str = "usage: zcnblk-portable-target --listen IP:PORT (--ram-mib N | --file PATH --size-mib N [--create] [--wal-journal PATH --journal-mib N])\nPlain TCP, one terminal RAM/file medium, 4096-byte sectors. --create refuses to overwrite existing files. RAM is volatile. WAL opens or creates exclusively held preallocated files; WRITE acknowledges append visibility, SYNC acknowledges durable WAL commit. Functional testing only; no performance fast path.";

fn main() -> io::Result<()> {
    zcutils::crypto_policy::initialize_or_exit();
    let mut listen = None;
    let mut ram = None;
    let mut file = None;
    let mut size = None;
    let mut create = false;
    let mut journal = None;
    let mut journal_size = None;
    let mut args = env::args().skip(1);
    let invalid = || io::Error::new(io::ErrorKind::InvalidInput, USAGE);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => listen = Some(args.next().ok_or_else(invalid)?),
            "--ram-mib" => ram = Some(args.next().ok_or_else(invalid)?),
            "--file" => file = Some(args.next().ok_or_else(invalid)?),
            "--size-mib" => size = Some(args.next().ok_or_else(invalid)?),
            "--create" => create = true,
            "--wal-journal" => journal = Some(args.next().ok_or_else(invalid)?),
            "--journal-mib" => journal_size = Some(args.next().ok_or_else(invalid)?),
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => return Err(invalid()),
        }
    }
    let bytes = |value: String| -> io::Result<u64> {
        value
            .parse::<u64>()
            .ok()
            .and_then(|n| n.checked_mul(1024 * 1024))
            .filter(|n| *n > 0)
            .ok_or_else(invalid)
    };
    let (target, capacity, backend) = match (ram, file, size, create, journal, journal_size) {
        (Some(mib), None, None, false, None, None) => {
            let capacity = bytes(mib)?;
            (
                Target::ram(usize::try_from(capacity).map_err(|_| invalid())?)?,
                capacity,
                "ram-volatile",
            )
        }
        (None, Some(path), Some(mib), create, None, None) => {
            let capacity = bytes(mib)?;
            (
                Target::file(Path::new(&path), capacity, create)?,
                capacity,
                "file-sync-all",
            )
        }
        (None, Some(path), Some(mib), false, Some(journal), Some(journal_mib)) => {
            let capacity = bytes(mib)?;
            (
                Target::wal(
                    Path::new(&journal),
                    Path::new(&path),
                    capacity,
                    bytes(journal_mib)?,
                )?,
                capacity,
                "persistent-wal-crc32c",
            )
        }
        _ => return Err(invalid()),
    };
    let listener = TcpListener::bind(listen.ok_or_else(invalid)?)?;
    eprintln!(
        "zcnblk-portable-target: listen={} capacity_bytes={capacity} backend={backend} placement=none sector_bytes=4096 functional_test_only=true workers=thread-per-connection cpu_mapping=os-scheduled hugetlb=false pinned=false io_uring=false",
        listener.local_addr()?
    );
    Arc::new(target).serve(listener)
}
