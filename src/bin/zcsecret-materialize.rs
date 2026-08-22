//! Small credential materializer that keeps cloud SDKs out of data-plane
//! binaries. AWS CLI uses its ambient credential chain; Vault Agent/CSI owns
//! Vault authentication and renders a local source file.

use std::env;
#[cfg(test)]
use std::fs;
use std::io;
#[cfg(test)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;
#[allow(dead_code)]
#[path = "../secret_lifecycle.rs"]
mod secret_lifecycle;
use secret_lifecycle::{
    SecretBundle, SecretPolicy, parse_bundle, read_bundle, unix_now_ms, write_bundle_atomic,
};

#[derive(Clone, Debug)]
enum Provider {
    AwsSsm {
        name: String,
        region: Option<String>,
    },
    AwsSecretsManager {
        secret_id: String,
        region: Option<String>,
    },
    VaultAgent {
        source_file: PathBuf,
    },
}

#[derive(Debug)]
struct Options {
    provider: Provider,
    output: PathBuf,
    interval: Option<Duration>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("zcsecret-materialize: {error}");
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let options = parse_args(env::args().skip(1).collect())?;
    let policy = policy_from_env()?;
    let mut published = if options.output.exists() {
        // Preserve the generation watermark across an outage even when the
        // previously published active credential has expired. Timestamp zero
        // validates every structural/lifecycle constraint without reviving it.
        Some(read_bundle(&options.output, 0, &policy).map_err(|error| {
            invalid(format!(
                "invalid existing credential generation watermark: {error}"
            ))
        })?)
    } else {
        None
    };
    loop {
        match fetch(&options.provider, &policy)
            .and_then(|candidate| publish_if_advanced(&options.output, &mut published, candidate))
        {
            Ok(()) => {}
            Err(error) if published.is_some() && options.interval.is_some() => {
                eprintln!(
                    "ZCSECRET_REFRESH_RETAIN provider={} generation={} error={error}",
                    provider_name(&options.provider),
                    published.as_ref().map_or(0, |bundle| bundle.generation)
                );
            }
            Err(error) => return Err(error),
        }
        let Some(interval) = options.interval else {
            return Ok(());
        };
        thread::sleep(interval);
    }
}

fn publish_if_advanced(
    output: &Path,
    published: &mut Option<SecretBundle>,
    candidate: SecretBundle,
) -> io::Result<()> {
    match published.as_ref() {
        Some(current) if candidate.generation < current.generation => {
            return Err(invalid(format!(
                "provider credential generation rolled back from {} to {}",
                current.generation, candidate.generation
            )));
        }
        Some(current) if candidate.generation == current.generation && candidate != *current => {
            return Err(invalid(
                "provider reused a credential generation with different contents",
            ));
        }
        Some(current) if candidate == *current => return Ok(()),
        _ => {}
    }
    write_bundle_atomic(output, &candidate)?;
    eprintln!(
        "ZCSECRET_PUBLISHED path={} generation={} active_id={} expires_at_unix_ms={} secret=redacted",
        output.display(),
        candidate.generation,
        candidate.active_id,
        candidate
            .credentials
            .iter()
            .find(|credential| credential.id == candidate.active_id)
            .map_or(0, |credential| credential.expires_at_unix_ms)
    );
    *published = Some(candidate);
    Ok(())
}

fn fetch(provider: &Provider, policy: &SecretPolicy) -> io::Result<SecretBundle> {
    let now_ms = unix_now_ms()?;
    match provider {
        Provider::AwsSsm { name, region } => aws_secret(
            "ssm",
            "get-parameter",
            "Parameter.Value",
            &["--name", name, "--with-decryption"],
            region.as_deref(),
        )
        .and_then(|encoded| parse_bundle(&encoded, now_ms, policy)),
        Provider::AwsSecretsManager { secret_id, region } => aws_secret(
            "secretsmanager",
            "get-secret-value",
            "SecretString",
            &["--secret-id", secret_id],
            region.as_deref(),
        )
        .and_then(|encoded| parse_bundle(&encoded, now_ms, policy)),
        Provider::VaultAgent { source_file } => read_bundle(source_file, now_ms, policy),
    }
}

fn aws_secret(
    service: &str,
    operation: &str,
    query: &str,
    provider_args: &[&str],
    region: Option<&str>,
) -> io::Result<Vec<u8>> {
    let workload_identity_only =
        env::var("ZCSECRET_REQUIRE_AWS_WORKLOAD_IDENTITY").as_deref() != Ok("0");
    if workload_identity_only
        && [
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_PROFILE",
            "AWS_DEFAULT_PROFILE",
            "AWS_SHARED_CREDENTIALS_FILE",
        ]
        .iter()
        .any(|name| env::var_os(name).is_some())
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "static AWS access-key/profile configuration is disabled; use an instance role, IRSA, EKS Pod Identity, web identity, or set ZCSECRET_REQUIRE_AWS_WORKLOAD_IDENTITY=0 only for migration",
        ));
    }
    let aws_cli = env::var("ZCSECRET_AWS_CLI").unwrap_or_else(|_| "aws".into());
    let mut command = Command::new(&aws_cli);
    command
        .arg(service)
        .arg(operation)
        .args(provider_args)
        .args(["--query", query, "--output", "json", "--no-cli-pager"]);
    if let Some(region) = region {
        command.args(["--region", region]);
    }
    if workload_identity_only {
        // Prevent the child from silently falling back to ~/.aws credentials or
        // credential_process. IRSA/web identity, EC2 IMDS, ECS credentials, and
        // EKS Pod Identity do not require either shared file.
        command
            .env("AWS_SHARED_CREDENTIALS_FILE", "/dev/null")
            .env("AWS_CONFIG_FILE", "/dev/null")
            .env_remove("AWS_PROFILE")
            .env_remove("AWS_DEFAULT_PROFILE")
            .env_remove("AWS_SDK_LOAD_CONFIG");
    }
    // No access key, secret key, session token, profile, or password is passed.
    // The AWS CLI resolves workload identity from its standard ambient chain.
    let output = command.output().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("execute AWS CLI {service} materializer: {error}"),
        )
    })?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "AWS CLI {service} materializer exited with {}",
            output.status
        )));
    }
    if output.stdout.len() > 128 * 1024 {
        return Err(invalid("AWS secret response exceeds 128 KiB"));
    }
    let document: String = serde_json::from_slice(&output.stdout)
        .map_err(|error| invalid(format!("AWS secret value is not a JSON string: {error}")))?;
    Ok(document.into_bytes())
}

fn parse_args(args: Vec<String>) -> io::Result<Options> {
    if args.is_empty() || matches!(args[0].as_str(), "-h" | "--help" | "help") {
        return Err(usage());
    }
    let mut provider = None;
    let mut name = None;
    let mut secret_id = None;
    let mut region = None;
    let mut source_file = None;
    let mut output = None;
    let mut interval = None;
    let mut index = usize::from(args.first().is_some_and(|arg| arg == "sync"));
    while index < args.len() {
        let flag = &args[index];
        index += 1;
        let value = args
            .get(index)
            .ok_or_else(|| invalid(format!("{flag} requires a value")))?;
        index += 1;
        match flag.as_str() {
            "--provider" => provider = Some(value.clone()),
            "--name" => name = Some(value.clone()),
            "--secret-id" => secret_id = Some(value.clone()),
            "--region" => region = Some(value.clone()),
            "--source-file" => source_file = Some(value.into()),
            "--output" => output = Some(value.into()),
            "--interval" => interval = Some(parse_duration(value)?),
            _ => return Err(invalid(format!("unknown argument {flag}"))),
        }
    }
    let provider = match provider.as_deref() {
        Some("aws-ssm") => Provider::AwsSsm {
            name: name.ok_or_else(|| invalid("aws-ssm requires --name"))?,
            region,
        },
        Some("aws-secrets-manager") | Some("aws-secretsmanager") => Provider::AwsSecretsManager {
            secret_id: secret_id
                .ok_or_else(|| invalid("aws-secrets-manager requires --secret-id"))?,
            region,
        },
        Some("vault-agent") => Provider::VaultAgent {
            source_file: source_file
                .ok_or_else(|| invalid("vault-agent requires --source-file"))?,
        },
        _ => return Err(usage()),
    };
    Ok(Options {
        provider,
        output: output.ok_or_else(|| invalid("--output is required"))?,
        interval,
    })
}

fn policy_from_env() -> io::Result<SecretPolicy> {
    let policy = SecretPolicy {
        minimum_secret_bytes: 32,
        maximum_ttl_ms: duration_env_ms("ZCSECRET_MAX_TTL", "ZCGLOBAL_ADMIN_MAX_TTL", "90d")?,
        rotate_before_ms: duration_env_ms(
            "ZCSECRET_ROTATE_BEFORE",
            "ZCGLOBAL_ADMIN_ROTATE_BEFORE",
            "7d",
        )?,
        activation_clock_skew_ms: duration_env_ms(
            "ZCSECRET_ACTIVATION_CLOCK_SKEW",
            "ZCGLOBAL_ADMIN_ACTIVATION_CLOCK_SKEW",
            "2s",
        )?,
        maximum_versions: env::var("ZCSECRET_MAX_VERSIONS")
            .or_else(|_| env::var("ZCGLOBAL_ADMIN_MAX_VERSIONS"))
            .unwrap_or_else(|_| "16".into())
            .parse()
            .map_err(|_| invalid("ZCSECRET_MAX_VERSIONS must be an integer"))?,
    };
    policy.validate()?;
    Ok(policy)
}

fn duration_env_ms(primary: &str, fallback: &str, default: &str) -> io::Result<u64> {
    let value = env::var(primary)
        .or_else(|_| env::var(fallback))
        .unwrap_or_else(|_| default.into());
    parse_duration_ms(&value, primary)
}

fn parse_duration(value: &str) -> io::Result<Duration> {
    let millis = parse_duration_ms(value, "interval")?;
    if millis == 0 {
        return Err(invalid("interval must be greater than zero"));
    }
    Ok(Duration::from_millis(millis))
}

fn parse_duration_ms(value: &str, name: &str) -> io::Result<u64> {
    let (number, multiplier) = if let Some(number) = value.strip_suffix("ms") {
        (number, 1u64)
    } else if let Some(number) = value.strip_suffix('s') {
        (number, 1_000)
    } else if let Some(number) = value.strip_suffix('m') {
        (number, 60_000)
    } else if let Some(number) = value.strip_suffix('h') {
        (number, 3_600_000)
    } else if let Some(number) = value.strip_suffix('d') {
        (number, 86_400_000)
    } else {
        (value, 1)
    };
    number
        .parse::<u64>()
        .map_err(|_| invalid(format!("{name} must be an integer duration")))?
        .checked_mul(multiplier)
        .ok_or_else(|| invalid(format!("{name} duration overflow")))
}

fn provider_name(provider: &Provider) -> &'static str {
    match provider {
        Provider::AwsSsm { .. } => "aws-ssm",
        Provider::AwsSecretsManager { .. } => "aws-secrets-manager",
        Provider::VaultAgent { .. } => "vault-agent",
    }
}

fn usage() -> io::Error {
    invalid(
        "usage:\n  zcsecret-materialize sync --provider aws-ssm --name PARAMETER [--region REGION] --output FILE [--interval DURATION]\n  zcsecret-materialize sync --provider aws-secrets-manager --secret-id ID [--region REGION] --output FILE [--interval DURATION]\n  zcsecret-materialize sync --provider vault-agent --source-file FILE --output FILE [--interval DURATION]",
    )
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_PATH: AtomicU64 = AtomicU64::new(1);

    fn test_path(label: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "zcsecret-materialize-{}-{label}-{}",
            std::process::id(),
            NEXT_PATH.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn test_policy() -> SecretPolicy {
        SecretPolicy {
            minimum_secret_bytes: 32,
            maximum_ttl_ms: 60_000,
            rotate_before_ms: 10_000,
            activation_clock_skew_ms: 0,
            maximum_versions: 4,
        }
    }

    #[test]
    fn publication_rejects_rollback_without_replacing_output() {
        let path = test_path("rollback");
        let policy = test_policy();
        let first = SecretBundle::new(1_000_000, 30_000, &policy).unwrap();
        let mut second = first.clone();
        second.rotate(1_000_001, 30_000, &policy).unwrap();
        let mut published = None;
        publish_if_advanced(&path, &mut published, second.clone()).unwrap();
        let error = publish_if_advanced(&path, &mut published, first)
            .unwrap_err()
            .to_string();
        assert!(error.contains("rolled back"));
        let on_disk: SecretBundle = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(on_disk, second);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn publication_rejects_same_generation_with_different_contents() {
        let path = test_path("generation-reuse");
        let policy = test_policy();
        let current = SecretBundle::new(2_000_000, 30_000, &policy).unwrap();
        let mut changed = current.clone();
        changed.credentials[0].secret = "f".repeat(64);
        let mut published = None;
        publish_if_advanced(&path, &mut published, current.clone()).unwrap();
        let error = publish_if_advanced(&path, &mut published, changed)
            .unwrap_err()
            .to_string();
        assert!(error.contains("different contents"));
        let on_disk: SecretBundle = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(on_disk, current);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn provider_arguments_are_explicit() {
        let options = parse_args(vec![
            "sync".into(),
            "--provider".into(),
            "aws-ssm".into(),
            "--name".into(),
            "/zc/test/admin".into(),
            "--region".into(),
            "us-east-1".into(),
            "--output".into(),
            "/run/zcsecrets/admin.json".into(),
            "--interval".into(),
            "250ms".into(),
        ])
        .unwrap();
        assert!(matches!(options.provider, Provider::AwsSsm { .. }));
        assert_eq!(options.interval, Some(Duration::from_millis(250)));
    }
}
