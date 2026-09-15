//! Process-wide provider selection for the FIPS-aspiring image.
//! This checks runtime mode, not CMVP certificate or service coverage.
use std::io;

#[path = "fips_evidence.rs"]
mod evidence;

/// Deliberately separate from mode checks: this exercises the linked module,
/// but certificate, application service coverage and environment need review.
pub fn collect_evidence() -> serde_json::Value {
    evidence::collect()
}

pub fn emit_evidence_if_requested() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--fips-evidence"] {
        let report = collect_evidence();
        let passed = report["passed"] == true;
        println!("{report}");
        std::process::exit(if passed { 0 } else { 1 });
    }
}

pub fn initialize() -> io::Result<()> {
    #[cfg(feature = "fips")]
    {
        aws_lc_rs::try_fips_mode().map_err(io::Error::other)?;
        // Dependencies can also enable ring. Install explicitly before any
        // client is constructed, then reject a conflicting installed provider.
        let _ = rustls::crypto::default_fips_provider().install_default();
        if !rustls::crypto::CryptoProvider::get_default().is_some_and(|p| p.fips()) {
            return Err(io::Error::other("process TLS provider is not in FIPS mode"));
        }
    }
    Ok(())
}

pub fn host_fips_enabled() -> bool {
    std::fs::read_to_string("/proc/sys/crypto/fips_enabled").is_ok_and(|value| value.trim() == "1")
}

pub fn require_mode(require_provider: bool, require_host: bool) -> io::Result<()> {
    initialize()?;
    if require_provider && !cfg!(feature = "fips") {
        return Err(io::Error::other("binary was built without --features fips"));
    }
    if require_host && !host_fips_enabled() {
        return Err(io::Error::other(
            "guest/node kernel FIPS mode is not enabled",
        ));
    }
    Ok(())
}

/// Called by every first-party executable shipped in Dockerfile.fips.
pub fn initialize_or_exit() {
    // Run before CLI parsing, storage access or service startup in every shipped
    // executable. The evidence command never changes the host FIPS setting.
    emit_evidence_if_requested();
    let required = std::env::var("ZC_REQUIRE_HOST_FIPS").as_deref() == Ok("1");
    if let Err(error) = require_mode(required, required) {
        eprintln!("zcutils crypto preflight: {error}");
        std::process::exit(1);
    }
}

/// Low-cardinality gauges for application-frame GCM in this process only.
/// Rustls/TLS and direct dependency crypto calls are not observed.
/// Never expose raw keys, key hashes,
/// or a misleading sum of independent per-key budgets as one shared budget.
pub fn fips_key_usage_counters() -> std::collections::BTreeMap<&'static str, u64> {
    #[allow(unused_mut)]
    let mut values = std::collections::BTreeMap::from([
        ("enabled", u64::from(cfg!(feature = "fips"))),
        ("process_snapshot_available", 0),
        ("cross_process_accounting_complete", 0),
        ("scope_application_frames_only", 1),
        ("tls_record_accounting_complete", 0),
    ]);
    #[cfg(feature = "fips")]
    if let Ok(snapshot) = crate::fips_key_usage::snapshot() {
        values.extend(snapshot);
        values.insert("process_snapshot_available", 1);
    }
    values
}

pub fn fips_key_usage_metrics() -> String {
    let mut out = String::new();
    for (name, value) in fips_key_usage_counters() {
        out.push_str(&format!(
            "# TYPE zccusan_fips_application_frame_gcm_{name} gauge\nzccusan_fips_application_frame_gcm_{name} {value}\n"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn budget_metrics_state_scope_and_do_not_publish_key_labels() {
        let metrics = super::fips_key_usage_metrics();
        assert!(
            metrics.contains(
                "zccusan_fips_application_frame_gcm_cross_process_accounting_complete 0\n"
            )
        );
        assert!(
            metrics
                .contains("zccusan_fips_application_frame_gcm_scope_application_frames_only 1\n")
        );
        assert!(
            metrics
                .contains("zccusan_fips_application_frame_gcm_tls_record_accounting_complete 0\n")
        );
        assert!(!metrics.contains("zccusan_fips_gcm_"));
        assert!(!metrics.contains('{'));
        #[cfg(feature = "fips")]
        {
            assert!(
                metrics.contains(
                    "zccusan_fips_application_frame_gcm_per_key_attempt_limit 4294967296\n"
                )
            );
            assert!(
                metrics
                    .contains("zccusan_fips_application_frame_gcm_process_snapshot_available 1\n")
            );
        }
    }

    #[test]
    fn required_provider_matches_build() {
        assert_eq!(
            super::require_mode(true, false).is_ok(),
            cfg!(feature = "fips")
        );
    }

    #[cfg(feature = "fips")]
    #[test]
    fn initialized_tls_configuration_is_fips() {
        super::initialize().unwrap();
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
        assert!(config.fips());
    }
}
