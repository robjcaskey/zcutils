use std::env;
use std::fs;
use std::io;
use std::process::{Command, Output};
use zcutils::global_failover::{
    AdapterKind, GlobalFailoverCommand, WorkloadAction, WorkloadFailoverPolicy,
};

const REGION_LABEL: &str = "topology.zcutils.io/region";
const BINDING_LABEL: &str = "zcutils.io/failover-binding";
const CUSTODY_TAINT: &str = "failover.zcutils.io/custody";

struct Kubectl {
    program: String,
    prefix: Vec<String>,
    request_timeout: String,
}

impl Kubectl {
    fn from_env() -> Self {
        let program = env::var("ZCGLOBAL_KUBECTL").unwrap_or_else(|_| "kubectl".into());
        let prefix = env::var("ZCGLOBAL_KUBECTL_PREFIX")
            .ok()
            .into_iter()
            .flat_map(|value| {
                value
                    .split_ascii_whitespace()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect();
        let request_timeout =
            env::var("ZCGLOBAL_KUBECTL_REQUEST_TIMEOUT").unwrap_or_else(|_| "5s".into());
        Self {
            program,
            prefix,
            request_timeout,
        }
    }

    fn output(&self, args: &[String]) -> io::Result<Output> {
        Command::new(&self.program)
            .args(&self.prefix)
            .arg(format!("--request-timeout={}", self.request_timeout))
            .args(args)
            .output()
    }

    fn run(&self, args: &[String], allow_missing_taint: bool) -> io::Result<String> {
        let output = self.output(args)?;
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if output.status.success()
            || (allow_missing_taint
                && (stderr.contains("not found") || stderr.contains("not tainted")))
        {
            return Ok(stdout);
        }
        Err(io::Error::other(format!(
            "kubectl {:?} failed status={} stdout={stdout:?} stderr={stderr:?}",
            args, output.status
        )))
    }
}

fn selector(binding: &str, region: &str) -> String {
    format!("{BINDING_LABEL}={binding},{REGION_LABEL}={region}")
}

fn validate_label_value(name: &str, value: &str) -> io::Result<()> {
    let bytes = value.as_bytes();
    let boundary_is_alphanumeric = |byte: u8| byte.is_ascii_alphanumeric();
    if bytes.is_empty()
        || bytes.len() > 63
        || !boundary_is_alphanumeric(bytes[0])
        || !boundary_is_alphanumeric(bytes[bytes.len() - 1])
        || bytes
            .iter()
            .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} is not a valid Kubernetes label value: {value:?}"),
        ));
    }
    Ok(())
}

fn controller_names(kubectl: &Kubectl, binding: &str, region: &str) -> io::Result<Vec<String>> {
    let selector = selector(binding, region);
    // Scaling a Deployment and its owned ReplicaSet independently would race
    // its controller. Select exactly one highest-level available kind.
    for kind in ["deployment", "statefulset", "replicaset"] {
        let output = kubectl.run(
            &[
                "get".into(),
                kind.into(),
                "-l".into(),
                selector.clone(),
                "-o".into(),
                "name".into(),
            ],
            false,
        )?;
        let names = output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if !names.is_empty() {
            return Ok(names);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("no scalable controller matches {selector}"),
    ))
}

fn scale(kubectl: &Kubectl, binding: &str, region: &str, replicas: u32) -> io::Result<()> {
    let names = controller_names(kubectl, binding, region)?;
    let mut args = vec!["scale".into()];
    args.extend(names);
    args.push(format!("--replicas={replicas}"));
    kubectl.run(&args, false).map(|_| ())
}

fn taint_region(kubectl: &Kubectl, region: &str, tainted: bool) -> io::Result<()> {
    let taint = if tainted {
        format!("{CUSTODY_TAINT}=moving:NoSchedule")
    } else {
        format!("{CUSTODY_TAINT}-")
    };
    kubectl
        .run(
            &[
                "taint".into(),
                "nodes".into(),
                "-l".into(),
                format!("{REGION_LABEL}={region}"),
                taint,
                "--overwrite".into(),
            ],
            !tainted,
        )
        .map(|_| ())
}

fn wait_source_pods_gone(kubectl: &Kubectl, binding: &str, region: &str) -> io::Result<()> {
    let names = kubectl.run(
        &[
            "get".into(),
            "pods".into(),
            "-l".into(),
            selector(binding, region),
            "-o".into(),
            "name".into(),
        ],
        false,
    )?;
    let names = names
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if names.is_empty() {
        return Ok(());
    }
    let timeout = env::var("ZCGLOBAL_KUBERNETES_DRAIN_TIMEOUT").unwrap_or_else(|_| "30s".into());
    let mut args = vec!["wait".into(), "--for=delete".into()];
    args.extend(names);
    args.push(format!("--timeout={timeout}"));
    kubectl.run(&args, false).map(|_| ())
}

fn force_delete_lost_source_pods(kubectl: &Kubectl, binding: &str, region: &str) -> io::Result<()> {
    let names = kubectl.run(
        &[
            "get".into(),
            "pods".into(),
            "-l".into(),
            selector(binding, region),
            "-o".into(),
            "name".into(),
        ],
        false,
    )?;
    let names = names
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if names.is_empty() {
        return Ok(());
    }
    let mut args = vec!["delete".into()];
    args.extend(names);
    args.extend([
        "--force".into(),
        "--grace-period=0".into(),
        "--wait=true".into(),
    ]);
    kubectl.run(&args, false).map(|_| ())
}

fn apply(kubectl: &Kubectl, action: &WorkloadAction) -> io::Result<()> {
    if action.adapter_kind != AdapterKind::Kubernetes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Kubernetes adapter received a non-Kubernetes action",
        ));
    }
    match action.policy {
        WorkloadFailoverPolicy::Stay | WorkloadFailoverPolicy::ObserveOnly => return Ok(()),
        WorkloadFailoverPolicy::FollowVolume => {}
    }
    if !action.add_source_taint || !action.remove_target_taint || action.source_replicas != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "follow-volume action omitted source fencing or scale-to-zero",
        ));
    }
    validate_label_value("binding_id", &action.binding_id)?;
    validate_label_value("source_region", &action.source_region)?;
    validate_label_value("target_region", &action.target_region)?;
    taint_region(kubectl, &action.source_region, true)?;
    scale(
        kubectl,
        &action.binding_id,
        &action.source_region,
        action.source_replicas,
    )?;
    if action.source_region_lost {
        force_delete_lost_source_pods(kubectl, &action.binding_id, &action.source_region)?;
    } else {
        wait_source_pods_gone(kubectl, &action.binding_id, &action.source_region)?;
    }
    taint_region(kubectl, &action.target_region, false)?;
    scale(
        kubectl,
        &action.binding_id,
        &action.target_region,
        action.target_replicas,
    )
}

fn usage() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "usage: zcglobal-kubernetes-adapter apply WORKLOAD_ACTION_JSON",
    )
}

fn main() -> io::Result<()> {
    let args = env::args().collect::<Vec<_>>();
    if args.len() != 3 || args[1] != "apply" {
        return Err(usage());
    }
    let action: WorkloadAction = serde_json::from_slice(&fs::read(&args[2])?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    apply(&Kubectl::from_env(), &action)?;
    let ack = GlobalFailoverCommand::AcknowledgeWorkloadAction {
        action_id: action.action_id,
        adapter_id: action.adapter_id,
    };
    println!("{}", serde_json::to_string(&ack).map_err(io::Error::other)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selectors_are_binding_and_region_scoped() {
        assert_eq!(
            selector("postgres-primary", "region-b"),
            "zcutils.io/failover-binding=postgres-primary,topology.zcutils.io/region=region-b"
        );
    }

    #[test]
    fn rejects_values_that_cannot_be_kubernetes_labels() {
        assert!(validate_label_value("binding_id", "postgres-primary").is_ok());
        assert!(validate_label_value("binding_id", "postgres/primary").is_err());
        assert!(validate_label_value("region", "-region-a").is_err());
        assert!(validate_label_value("region", &"a".repeat(64)).is_err());
    }
}
