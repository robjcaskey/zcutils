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

#[cfg(test)]
mod tests {
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
