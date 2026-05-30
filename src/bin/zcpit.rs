use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::path::PathBuf;

use zcutils::{zc_pit_is_reflink_unsupported, zc_pit_reflink_file};

#[derive(Debug)]
struct Args {
    source: PathBuf,
    snapshot: PathBuf,
    bytes: Option<u64>,
    mode: String,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("zcpit: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let raw_args = env::args().skip(1).collect::<Vec<_>>();
    if raw_args.is_empty() || matches!(raw_args[0].as_str(), "-h" | "--help" | "help") {
        print!("{}", usage());
        return Ok(());
    }
    let args = parse_args(raw_args)?;
    let mode = args.mode.as_str();
    match mode {
        "reflink" => {
            zc_pit_reflink_file(&args.source, &args.snapshot, args.bytes)
                .map_err(|e| format!("reflink PIT snapshot failed: {e}"))?;
            println!(
                "zcpit-result mode=reflink source={} snapshot={}",
                args.source.display(),
                args.snapshot.display()
            );
        }
        "auto" => match zc_pit_reflink_file(&args.source, &args.snapshot, args.bytes) {
            Ok(()) => println!(
                "zcpit-result mode=reflink source={} snapshot={}",
                args.source.display(),
                args.snapshot.display()
            ),
            Err(e) if zc_pit_is_reflink_unsupported(&e) => {
                copy_snapshot(&args)?;
                println!(
                    "zcpit-result mode=copy fallback=reflink-unsupported source={} snapshot={}",
                    args.source.display(),
                    args.snapshot.display()
                );
            }
            Err(e) => return Err(format!("reflink PIT snapshot failed: {e}")),
        },
        "copy" => {
            copy_snapshot(&args)?;
            println!(
                "zcpit-result mode=copy source={} snapshot={}",
                args.source.display(),
                args.snapshot.display()
            );
        }
        _ => return Err("mode must be reflink, auto, or copy".to_string()),
    }
    Ok(())
}

fn parse_args(args: Vec<String>) -> Result<Args, String> {
    let mut source = None;
    let mut snapshot = None;
    let mut bytes = None;
    let mut mode = "reflink".to_string();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "snapshot" if i == 0 => {}
            "--source" | "--input" => {
                i += 1;
                source = Some(PathBuf::from(value(&args, i, arg)?));
            }
            "--snapshot" | "--output" => {
                i += 1;
                snapshot = Some(PathBuf::from(value(&args, i, arg)?));
            }
            "--bytes" => {
                i += 1;
                bytes = Some(parse_u64(value(&args, i, "--bytes")?, "--bytes")?);
            }
            "--mode" => {
                i += 1;
                mode = normalize_mode(value(&args, i, "--mode")?)?;
            }
            _ if arg.starts_with("--source=") => {
                source = Some(PathBuf::from(arg.trim_start_matches("--source=")));
            }
            _ if arg.starts_with("--input=") => {
                source = Some(PathBuf::from(arg.trim_start_matches("--input=")));
            }
            _ if arg.starts_with("--snapshot=") => {
                snapshot = Some(PathBuf::from(arg.trim_start_matches("--snapshot=")));
            }
            _ if arg.starts_with("--output=") => {
                snapshot = Some(PathBuf::from(arg.trim_start_matches("--output=")));
            }
            _ if arg.starts_with("--bytes=") => {
                bytes = Some(parse_u64(arg.trim_start_matches("--bytes="), "--bytes")?);
            }
            _ if arg.starts_with("--mode=") => {
                mode = normalize_mode(arg.trim_start_matches("--mode="))?;
            }
            _ => return Err(format!("unknown argument {arg}\n{}", usage())),
        }
        i += 1;
    }

    Ok(Args {
        source: source.ok_or_else(|| format!("--source is required\n{}", usage()))?,
        snapshot: snapshot.ok_or_else(|| format!("--snapshot is required\n{}", usage()))?,
        bytes,
        mode,
    })
}

fn copy_snapshot(args: &Args) -> Result<(), String> {
    if let Some(parent) = args.snapshot.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("create snapshot parent {}: {e}", parent.display()))?;
    }
    let mut src = OpenOptions::new()
        .read(true)
        .open(&args.source)
        .map_err(|e| format!("open source {}: {e}", args.source.display()))?;
    let mut dst = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&args.snapshot)
        .map_err(|e| format!("open snapshot {}: {e}", args.snapshot.display()))?;
    let copied = match args.bytes {
        Some(bytes) => io::copy(&mut src.take(bytes), &mut dst),
        None => io::copy(&mut src, &mut dst),
    }
    .map_err(|e| {
        let _ = fs::remove_file(&args.snapshot);
        format!("copy snapshot bytes: {e}")
    })?;
    if let Some(bytes) = args.bytes {
        dst.set_len(bytes)
            .map_err(|e| format!("size snapshot {}: {e}", args.snapshot.display()))?;
    }
    dst.sync_all()
        .map_err(|e| format!("sync snapshot {}: {e}", args.snapshot.display()))?;
    if args.bytes.is_some() && copied != args.bytes.unwrap() {
        return Err(format!(
            "source ended after {copied} bytes before requested {} bytes",
            args.bytes.unwrap()
        ));
    }
    Ok(())
}

fn value<'a>(args: &'a [String], index: usize, flag: &str) -> Result<&'a str, String> {
    args.get(index)
        .map(String::as_str)
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn parse_u64(value: &str, flag: &str) -> Result<u64, String> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| format!("{flag} must be an integer"))?;
    if parsed == 0 {
        return Err(format!("{flag} must be greater than zero"));
    }
    Ok(parsed)
}

fn normalize_mode(value: &str) -> Result<String, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "reflink" | "cow" | "zero-copy" | "zerocopy" => Ok("reflink".to_string()),
        "auto" => Ok("auto".to_string()),
        "copy" | "full-copy" => Ok("copy".to_string()),
        other => Err(format!(
            "unsupported mode {other:?}; expected reflink, auto, or copy"
        )),
    }
}

fn usage() -> &'static str {
    "usage: zcpit snapshot --source PATH --snapshot PATH [--bytes N] [--mode reflink|auto|copy]"
}
