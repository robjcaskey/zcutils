use std::env;
use std::io;
use zcutils::cloud_topology::{Ec2Imds, apply_overrides, detect_ec2};

fn main() -> io::Result<()> {
    let mut overrides = Vec::new();
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--set" => overrides.push(args.next().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "--set requires key=value")
            })?),
            "--help" | "-h" => {
                println!("usage: zctopology-detect [--set key=json-or-text]...");
                return Ok(());
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown argument {arg}"),
                ));
            }
        }
    }
    if let Ok(value) = env::var("ZC_TOPOLOGY_CHARACTERISTICS") {
        overrides.extend(
            value
                .split(',')
                .filter(|value| !value.is_empty())
                .map(str::to_string),
        );
    }
    let source = Ec2Imds::connect()?;
    let mut facts = detect_ec2(&source)?;
    apply_overrides(&mut facts, overrides)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&facts)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
    );
    Ok(())
}
