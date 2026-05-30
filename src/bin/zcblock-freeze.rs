use std::env;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use zcutils::block::control as control_api;

const DEFAULT_CONTROL_SOCKET: &str = "/var/lib/zcblock-csi/control.sock";

#[derive(Debug)]
struct Args {
    command: String,
    socket: PathBuf,
    control_url: Option<String>,
    barrier: Option<String>,
    ttl_ms: Option<u64>,
    line: Option<String>,
}

fn main() {
    match run() {
        Ok(response) => {
            print!("{response}");
            if response.starts_with("ERR") {
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("zcblock-freeze: {e}");
            std::process::exit(2);
        }
    }
}

fn run() -> Result<String, String> {
    let args = parse_args()?;
    if let Some(control_url) = args.control_url.as_ref() {
        return send_rest_command(control_url, &args);
    }
    let request = match args.command.as_str() {
        "freeze" => {
            let barrier = args
                .barrier
                .as_deref()
                .ok_or("freeze requires --barrier <id>")?;
            validate_token(barrier, "barrier")?;
            let ttl_ms = args
                .ttl_ms
                .ok_or("freeze requires --ttl-ms <milliseconds>")?;
            format!("FREEZE barrier={barrier} ttl_ms={ttl_ms}\n")
        }
        "release" => {
            let barrier = args
                .barrier
                .as_deref()
                .ok_or("release requires --barrier <id>")?;
            validate_token(barrier, "barrier")?;
            format!("RELEASE barrier={barrier}\n")
        }
        "status" => "STATUS\n".to_string(),
        "raw" => {
            let line = args
                .line
                .as_deref()
                .ok_or("raw requires --line <request>")?;
            validate_raw_line(line)?;
            format!("{line}\n")
        }
        other => {
            return Err(format!(
                "unknown command {other}; expected freeze, release, status, or raw"
            ));
        }
    };
    send_command(&args.socket, &request)
}

fn parse_args() -> Result<Args, String> {
    let mut command = None;
    let mut socket = env::var("ZCBLOCK_CSI_CONTROL_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_CONTROL_SOCKET));
    let mut control_url = env::var("ZCBLOCK_CONTROL_URL").ok();
    let mut barrier = None;
    let mut ttl_ms = None;
    let mut line = None;
    let args = env::args().skip(1).collect::<Vec<_>>();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-h" | "--help" => return Err(usage()),
            "--socket" => {
                i += 1;
                socket = PathBuf::from(args.get(i).ok_or("--socket requires a value")?);
            }
            "--control-url" => {
                i += 1;
                control_url = Some(args.get(i).ok_or("--control-url requires a value")?.clone());
            }
            "--barrier" => {
                i += 1;
                barrier = Some(args.get(i).ok_or("--barrier requires a value")?.clone());
            }
            "--ttl-ms" | "--ttl_ms" => {
                i += 1;
                ttl_ms = Some(
                    args.get(i)
                        .ok_or("--ttl-ms requires a value")?
                        .parse::<u64>()
                        .map_err(|_| "--ttl-ms must be an integer".to_string())?,
                );
            }
            "--line" => {
                i += 1;
                line = Some(args.get(i).ok_or("--line requires a value")?.clone());
            }
            _ if arg.starts_with("--socket=") => {
                socket = PathBuf::from(arg.trim_start_matches("--socket="));
            }
            _ if arg.starts_with("--control-url=") => {
                control_url = Some(arg.trim_start_matches("--control-url=").to_string());
            }
            _ if arg.starts_with("--barrier=") => {
                barrier = Some(arg.trim_start_matches("--barrier=").to_string());
            }
            _ if arg.starts_with("--ttl-ms=") => {
                ttl_ms = Some(
                    arg.trim_start_matches("--ttl-ms=")
                        .parse::<u64>()
                        .map_err(|_| "--ttl-ms must be an integer".to_string())?,
                );
            }
            _ if arg.starts_with("--line=") => {
                line = Some(arg.trim_start_matches("--line=").to_string());
            }
            _ if arg.starts_with('-') => return Err(format!("unknown flag {arg}")),
            _ => {
                if command.is_some() {
                    return Err(format!("unexpected argument {arg}"));
                }
                command = Some(arg.to_ascii_lowercase());
            }
        }
        i += 1;
    }

    Ok(Args {
        command: command.ok_or_else(usage)?,
        socket,
        control_url,
        barrier,
        ttl_ms,
        line,
    })
}

fn send_rest_command(control_url: &str, args: &Args) -> Result<String, String> {
    let client = control_api::HttpControlClient::new(control_url)?;
    match args.command.as_str() {
        "freeze" => {
            let barrier = args
                .barrier
                .as_deref()
                .ok_or("freeze requires --barrier <id>")?;
            validate_token(barrier, "barrier")?;
            let ttl_ms = args
                .ttl_ms
                .ok_or("freeze requires --ttl-ms <milliseconds>")?;
            let response = client.freeze(&control_api::FreezeRequest {
                barrier_id: barrier.to_string(),
                ttl_ms,
            })?;
            Ok(format_freeze_response(&response))
        }
        "release" => {
            let barrier = args
                .barrier
                .as_deref()
                .ok_or("release requires --barrier <id>")?;
            validate_token(barrier, "barrier")?;
            let response = client.release(barrier)?;
            Ok(format_freeze_response(&response))
        }
        "status" => {
            let response = client.freeze_status()?;
            if response.active {
                Ok(format!(
                    "OK active=true barrier={} frozen={} remaining_ms={} mounts={}\n",
                    response.barrier_id.unwrap_or_default(),
                    response.frozen_mounts.len(),
                    response.remaining_ms,
                    join_mounts(&response.frozen_mounts)
                ))
            } else {
                Ok("OK active=false\n".to_string())
            }
        }
        "raw" => Err("raw is only supported against the legacy Unix socket".to_string()),
        other => Err(format!(
            "unknown command {other}; expected freeze, release, status, or raw"
        )),
    }
}

fn format_freeze_response(response: &control_api::FreezeResponse) -> String {
    format!(
        "OK barrier={} frozen={} remaining_ms={} mounts={}\n",
        response.barrier_id,
        response.frozen_mounts.len(),
        response.remaining_ms,
        join_mounts(&response.frozen_mounts)
    )
}

fn join_mounts(mounts: &[String]) -> String {
    if mounts.is_empty() {
        "-".to_string()
    } else {
        mounts.join(",")
    }
}

fn send_command(socket: &PathBuf, request: &str) -> Result<String, String> {
    let mut stream =
        UnixStream::connect(socket).map_err(|e| format!("connect {}: {e}", socket.display()))?;
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("write command: {e}"))?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(|e| format!("shutdown write: {e}"))?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|e| format!("read response: {e}"))?;
    Ok(response)
}

fn validate_token(value: &str, name: &str) -> Result<(), String> {
    if value.is_empty() || value.chars().any(char::is_whitespace) || value.contains('\0') {
        return Err(format!(
            "{name} must be non-empty and contain no whitespace or NUL"
        ));
    }
    Ok(())
}

fn validate_raw_line(value: &str) -> Result<(), String> {
    if value.is_empty() || value.contains('\n') || value.contains('\0') {
        return Err("raw request must be one non-empty line without NUL".to_string());
    }
    Ok(())
}

fn usage() -> String {
    "usage: zcblock-freeze [--socket PATH | --control-url URL] <freeze --barrier ID --ttl-ms MS | release --barrier ID | status | raw --line REQUEST>".to_string()
}
