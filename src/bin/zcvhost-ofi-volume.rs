use std::env;
use std::io;
use zcutils::vhost_ofi::{VhostOfiTargetConfig, run_vhost_ofi_target};

fn usage() -> &'static str {
    "usage: zcvhost-ofi-volume --bind ADDRESS --base-service PORT --lanes N --capacity-bytes N [--provider NAME] [--endpoint rdm] [--domain NAME] [--lane-cpus CSV] [--require-hugetlb]"
}

fn value(arguments: &mut impl Iterator<Item = String>, name: &str) -> io::Result<String> {
    arguments
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("missing {name}")))
}

fn parse_usize(arguments: &mut impl Iterator<Item = String>, name: &str) -> io::Result<usize> {
    value(arguments, name)?
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("invalid {name}")))
}

fn run() -> io::Result<()> {
    let mut arguments = env::args().skip(1);
    let mut bind = None;
    let mut base_service = None;
    let mut lanes = None;
    let mut capacity_bytes = None;
    let mut provider = "sockets".to_string();
    let mut endpoint = "rdm".to_string();
    let mut domain = None;
    let mut lane_cpus = Vec::new();
    let mut require_hugetlb = false;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--bind" => bind = Some(value(&mut arguments, "--bind")?),
            "--base-service" => base_service = Some(parse_usize(&mut arguments, "--base-service")?),
            "--lanes" => lanes = Some(parse_usize(&mut arguments, "--lanes")?),
            "--capacity-bytes" => {
                capacity_bytes = Some(
                    value(&mut arguments, "--capacity-bytes")?
                        .parse::<u64>()
                        .map_err(|_| {
                            io::Error::new(io::ErrorKind::InvalidInput, "invalid --capacity-bytes")
                        })?,
                )
            }
            "--provider" => provider = value(&mut arguments, "--provider")?,
            "--endpoint" => endpoint = value(&mut arguments, "--endpoint")?,
            "--domain" => domain = Some(value(&mut arguments, "--domain")?),
            "--lane-cpus" => {
                lane_cpus = value(&mut arguments, "--lane-cpus")?
                    .split(',')
                    .map(|value| {
                        value.parse::<usize>().map_err(|_| {
                            io::Error::new(io::ErrorKind::InvalidInput, "invalid --lane-cpus")
                        })
                    })
                    .collect::<io::Result<Vec<_>>>()?
            }
            "--require-hugetlb" => require_hugetlb = true,
            "--help" | "-h" => {
                println!("{}", usage());
                return Ok(());
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown option {argument}\n{}", usage()),
                ));
            }
        }
    }
    run_vhost_ofi_target(VhostOfiTargetConfig {
        provider,
        endpoint,
        bind: bind.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, usage()))?,
        domain,
        base_service: u16::try_from(
            base_service.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, usage()))?,
        )
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "--base-service exceeds u16"))?,
        lanes: lanes.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, usage()))?,
        capacity_bytes: capacity_bytes
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, usage()))?,
        lane_cpus,
        require_hugetlb,
    })
}

fn main() {
    if let Err(error) = run() {
        eprintln!("zcvhost-ofi-volume: {error}");
        std::process::exit(1);
    }
}
