use std::env;
use std::io;
use std::path::Path;
use zcutils::racing_mirror::{run_client_window, run_first_hop, run_leaf, scan_log};

fn usage() -> ! {
    eprintln!(
        "usage:\n  zcracing-mirror client ADDR FRAMES PAYLOAD_BYTES [WINDOW]\n  zcracing-mirror first-hop LISTEN REMOTE LOCAL_LOG\n  zcracing-mirror leaf LISTEN TERMINAL_LOG [ACK_DELAY_MS]\n  zcracing-mirror verify TERMINAL_LOG EXPECTED_FRAMES"
    );
    std::process::exit(2)
}

fn number<T: std::str::FromStr>(value: Option<String>, name: &str) -> T {
    value.and_then(|v| v.parse().ok()).unwrap_or_else(|| {
        eprintln!("invalid or missing {name}");
        usage()
    })
}

fn main() -> io::Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("client") => {
            let addr = args.next().unwrap_or_else(|| usage());
            let frames = number(args.next(), "frames");
            let payload = number(args.next(), "payload bytes");
            let window = args
                .next()
                .map(|v| v.parse().unwrap_or_else(|_| usage()))
                .unwrap_or(1);
            let result = run_client_window(&addr, frames, payload, window)?;
            let seconds = result.elapsed.as_secs_f64();
            println!(
                "RACING_MIRROR_CLIENT_PASS appended_frames={} recovered_from={} payload_bytes={} window={} durable_ack=both-legs elapsed_s={seconds:.6} representative_benchmark=false source_payload_copy=vmsplice first_hop_payload_userspace_copy_bytes=0 remote_payload_userspace_copy_bytes=0",
                result.frames, result.recovered_from, result.payload_bytes, result.window
            );
        }
        Some("first-hop") => {
            let listen = args.next().unwrap_or_else(|| usage());
            let remote = args.next().unwrap_or_else(|| usage());
            let path = args.next().unwrap_or_else(|| usage());
            run_first_hop(&listen, &remote, Path::new(&path))?;
        }
        Some("leaf") => {
            let listen = args.next().unwrap_or_else(|| usage());
            let path = args.next().unwrap_or_else(|| usage());
            let delay = args
                .next()
                .map(|v| v.parse().unwrap_or_else(|_| usage()))
                .unwrap_or(0);
            run_leaf(&listen, Path::new(&path), delay)?;
        }
        Some("verify") => {
            let path = args.next().unwrap_or_else(|| usage());
            let expected: u64 = number(args.next(), "expected frames");
            let scan = scan_log(Path::new(&path), true)?;
            if scan.frames != expected || scan.incomplete_tail_bytes != 0 {
                return Err(io::Error::other(format!(
                    "verify expected {expected} complete frames, got {scan:?}"
                )));
            }
            println!(
                "RACING_MIRROR_VERIFY_PASS path={path} frames={} valid_bytes={} payload_pattern=valid incomplete_tail_bytes=0",
                scan.frames, scan.valid_bytes
            );
        }
        _ => usage(),
    }
    Ok(())
}
