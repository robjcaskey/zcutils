//! Expiring, overlapping credential bundles for control-plane secrets.
//!
//! A bundle has one preferred credential and may retain older, independently
//! expiring credentials while a rotation is propagating.  Authentication is a
//! control-plane operation: callers should publish a validated immutable
//! bundle to request handlers rather than consult this module from an I/O hot
//! path.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const SECRET_BUNDLE_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_SECRET_BYTES: usize = 32;
static TEMPORARY_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SecretVersion {
    /// Non-secret identifier used for audit and rotation observability.
    pub id: String,
    pub secret: String,
    pub not_before_unix_ms: u64,
    pub expires_at_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SecretBundle {
    pub schema_version: u32,
    pub generation: u64,
    pub active_id: String,
    pub credentials: Vec<SecretVersion>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SecretPolicy {
    pub minimum_secret_bytes: usize,
    pub maximum_ttl_ms: u64,
    pub rotate_before_ms: u64,
    pub activation_clock_skew_ms: u64,
    pub maximum_versions: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretStatus {
    pub generation: u64,
    pub active_id: String,
    pub active_expires_at_unix_ms: u64,
    pub active_expires_in_ms: u64,
    pub rotation_due: bool,
    pub accepted_versions: usize,
}

impl SecretPolicy {
    pub fn validate(&self) -> io::Result<()> {
        if self.minimum_secret_bytes < 32 {
            return Err(invalid("minimum secret length must be at least 32 bytes"));
        }
        if self.maximum_ttl_ms == 0 {
            return Err(invalid("maximum secret TTL must be greater than zero"));
        }
        if self.rotate_before_ms >= self.maximum_ttl_ms {
            return Err(invalid(
                "rotate-before threshold must be smaller than maximum secret TTL",
            ));
        }
        if self.maximum_versions < 2 {
            return Err(invalid(
                "credential bundles must permit at least two overlapping versions",
            ));
        }
        Ok(())
    }
}

impl SecretBundle {
    pub fn new(now_ms: u64, ttl_ms: u64, policy: &SecretPolicy) -> io::Result<Self> {
        policy.validate()?;
        validate_ttl(ttl_ms, policy)?;
        let credential = generate_version(1, now_ms, ttl_ms)?;
        let bundle = Self {
            schema_version: SECRET_BUNDLE_SCHEMA_VERSION,
            generation: 1,
            active_id: credential.id.clone(),
            credentials: vec![credential],
        };
        bundle.validate_at(now_ms, policy)?;
        Ok(bundle)
    }

    /// Add a new preferred credential while retaining still-valid predecessors.
    /// Their original expiry is never extended by rotation.
    pub fn rotate(&mut self, now_ms: u64, ttl_ms: u64, policy: &SecretPolicy) -> io::Result<()> {
        policy.validate()?;
        validate_ttl(ttl_ms, policy)?;
        self.credentials
            .retain(|credential| credential.expires_at_unix_ms > now_ms);
        if self.credentials.len() >= policy.maximum_versions {
            return Err(invalid(format!(
                "rotation would exceed {} overlapping credential versions; shorten TTL, rotate less often, or raise the configured limit",
                policy.maximum_versions
            )));
        }
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("secret generation overflow"))?;
        let credential = generate_version(self.generation, now_ms, ttl_ms)?;
        self.active_id = credential.id.clone();
        self.credentials.push(credential);
        self.validate_at(now_ms, policy)
    }

    pub fn active_secret<'a>(&'a self, now_ms: u64, policy: &SecretPolicy) -> io::Result<&'a str> {
        self.validate_at(now_ms, policy)?;
        let active = self.active().expect("validated active credential");
        if !is_active(active, now_ms, policy.activation_clock_skew_ms) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "active credential is not currently valid",
            ));
        }
        Ok(&active.secret)
    }

    pub fn accepts(&self, provided: &[u8], now_ms: u64, policy: &SecretPolicy) -> bool {
        self.credentials.iter().any(|credential| {
            is_active(credential, now_ms, policy.activation_clock_skew_ms)
                && constant_time_eq(provided, credential.secret.as_bytes())
        })
    }

    pub fn status(&self, now_ms: u64, policy: &SecretPolicy) -> io::Result<SecretStatus> {
        self.validate_at(now_ms, policy)?;
        let active = self.active().expect("validated active credential");
        let expires_in = active.expires_at_unix_ms.saturating_sub(now_ms);
        Ok(SecretStatus {
            generation: self.generation,
            active_id: active.id.clone(),
            active_expires_at_unix_ms: active.expires_at_unix_ms,
            active_expires_in_ms: expires_in,
            rotation_due: expires_in <= policy.rotate_before_ms,
            accepted_versions: self
                .credentials
                .iter()
                .filter(|credential| is_active(credential, now_ms, policy.activation_clock_skew_ms))
                .count(),
        })
    }

    pub fn validate_at(&self, now_ms: u64, policy: &SecretPolicy) -> io::Result<()> {
        policy.validate()?;
        if self.schema_version != SECRET_BUNDLE_SCHEMA_VERSION {
            return Err(invalid(format!(
                "unsupported secret bundle schema version {}",
                self.schema_version
            )));
        }
        if self.generation == 0 || self.credentials.is_empty() {
            return Err(invalid(
                "secret bundle generation and credential list must be non-zero",
            ));
        }
        if self.credentials.len() > policy.maximum_versions {
            return Err(invalid("secret bundle has too many credential versions"));
        }
        let mut ids = BTreeSet::new();
        for credential in &self.credentials {
            if credential.id.is_empty() || !ids.insert(&credential.id) {
                return Err(invalid(
                    "secret credential ids must be non-empty and unique",
                ));
            }
            if credential.secret.as_bytes().len() < policy.minimum_secret_bytes
                || credential.secret.len() > 1024
                || credential
                    .secret
                    .bytes()
                    .any(|byte| byte <= b' ' || byte == 0x7f)
            {
                return Err(invalid(
                    "secret must be at least the configured entropy length, at most 1024 bytes, and printable without whitespace",
                ));
            }
            if credential.not_before_unix_ms >= credential.expires_at_unix_ms {
                return Err(invalid(
                    "secret credential has an invalid validity interval",
                ));
            }
            let ttl = credential
                .expires_at_unix_ms
                .saturating_sub(credential.not_before_unix_ms);
            if ttl > policy.maximum_ttl_ms {
                return Err(invalid("secret credential exceeds configured maximum TTL"));
            }
        }
        let active = self
            .active()
            .ok_or_else(|| invalid("active credential id is absent from secret bundle"))?;
        if active.expires_at_unix_ms <= now_ms {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "active credential has expired",
            ));
        }
        Ok(())
    }

    fn active(&self) -> Option<&SecretVersion> {
        self.credentials
            .iter()
            .find(|credential| credential.id == self.active_id)
    }
}

pub fn unix_now_ms() -> io::Result<u64> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| invalid("system clock is before the Unix epoch"))?
        .as_millis();
    u64::try_from(millis).map_err(|_| invalid("Unix timestamp exceeds u64 milliseconds"))
}

pub fn read_bundle(path: &Path, now_ms: u64, policy: &SecretPolicy) -> io::Result<SecretBundle> {
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(invalid("secret bundle must be a regular file"));
    }
    if metadata.len() > 64 * 1024 {
        return Err(invalid("secret bundle exceeds 64 KiB"));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "secret bundle {} must not be group/world accessible",
                path.display()
            ),
        ));
    }
    let mut encoded = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut encoded)?;
    parse_bundle(&encoded, now_ms, policy)
}

pub fn parse_bundle(
    encoded: &[u8],
    now_ms: u64,
    policy: &SecretPolicy,
) -> io::Result<SecretBundle> {
    if encoded.len() > 64 * 1024 {
        return Err(invalid("secret bundle exceeds 64 KiB"));
    }
    let bundle: SecretBundle = serde_json::from_slice(encoded)
        .map_err(|error| invalid(format!("invalid secret bundle JSON: {error}")))?;
    bundle.validate_at(now_ms, policy)?;
    Ok(bundle)
}

pub fn write_bundle_atomic(path: &Path, bundle: &SecretBundle) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!(
        "tmp-{}-{}-{}",
        std::process::id(),
        unix_now_ms()?,
        TEMPORARY_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let encoded = serde_json::to_vec_pretty(bundle).map_err(io::Error::other)?;
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&encoded)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn generate_version(generation: u64, now_ms: u64, ttl_ms: u64) -> io::Result<SecretVersion> {
    let expires_at_unix_ms = now_ms
        .checked_add(ttl_ms)
        .ok_or_else(|| invalid("secret expiry timestamp overflow"))?;
    let mut random = [0u8; DEFAULT_SECRET_BYTES];
    File::open("/dev/urandom")?.read_exact(&mut random)?;
    let mut secret = String::with_capacity(random.len() * 2);
    for byte in random {
        use std::fmt::Write as _;
        write!(&mut secret, "{byte:02x}").map_err(|_| invalid("format random secret"))?;
    }
    Ok(SecretVersion {
        id: format!("credential-{generation}-{now_ms}"),
        secret,
        not_before_unix_ms: now_ms,
        expires_at_unix_ms,
    })
}

fn validate_ttl(ttl_ms: u64, policy: &SecretPolicy) -> io::Result<()> {
    if ttl_ms == 0 || ttl_ms > policy.maximum_ttl_ms {
        return Err(invalid(format!(
            "secret TTL must be between 1 and {} milliseconds",
            policy.maximum_ttl_ms
        )));
    }
    Ok(())
}

fn is_active(credential: &SecretVersion, now_ms: u64, activation_clock_skew_ms: u64) -> bool {
    credential
        .not_before_unix_ms
        .saturating_sub(activation_clock_skew_ms)
        <= now_ms
        && now_ms < credential.expires_at_unix_ms
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        difference |= usize::from(
            left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0),
        );
    }
    difference == 0
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_policy() -> SecretPolicy {
        SecretPolicy {
            minimum_secret_bytes: 32,
            maximum_ttl_ms: 30_000,
            rotate_before_ms: 10_000,
            activation_clock_skew_ms: 250,
            maximum_versions: 8,
        }
    }

    #[test]
    fn thirty_second_credentials_rotate_every_ten_seconds_without_a_gap() {
        let policy = test_policy();
        let mut bundle = SecretBundle::new(1_000_000, 30_000, &policy).unwrap();
        let first = bundle.active_secret(1_000_000, &policy).unwrap().to_owned();

        bundle.rotate(1_010_000, 30_000, &policy).unwrap();
        let second = bundle.active_secret(1_010_000, &policy).unwrap().to_owned();
        assert_ne!(first, second);
        assert!(bundle.accepts(first.as_bytes(), 1_019_999, &policy));
        assert!(bundle.accepts(second.as_bytes(), 1_019_999, &policy));

        bundle.rotate(1_020_000, 30_000, &policy).unwrap();
        let third = bundle.active_secret(1_020_000, &policy).unwrap().to_owned();
        assert!(bundle.accepts(first.as_bytes(), 1_029_999, &policy));
        assert!(!bundle.accepts(first.as_bytes(), 1_030_000, &policy));
        assert!(bundle.accepts(second.as_bytes(), 1_030_000, &policy));
        assert!(bundle.accepts(third.as_bytes(), 1_030_000, &policy));
    }

    #[test]
    fn rotation_never_extends_a_predecessor_expiry() {
        let policy = test_policy();
        let mut bundle = SecretBundle::new(2_000_000, 30_000, &policy).unwrap();
        let first_expiry = bundle.credentials[0].expires_at_unix_ms;
        bundle.rotate(2_010_000, 30_000, &policy).unwrap();
        assert_eq!(bundle.credentials[0].expires_at_unix_ms, first_expiry);
    }

    #[test]
    fn expired_active_credential_fails_closed() {
        let policy = test_policy();
        let bundle = SecretBundle::new(3_000_000, 30_000, &policy).unwrap();
        assert!(bundle.active_secret(3_030_000, &policy).is_err());
        assert!(!bundle.accepts(bundle.credentials[0].secret.as_bytes(), 3_030_000, &policy));
    }
}
