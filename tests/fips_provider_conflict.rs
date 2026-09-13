// Separate process from the normal provider tests: rustls's provider is global.
#[cfg(feature = "fips")]
#[test]
fn rejects_an_already_installed_non_fips_provider() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("fresh integration test process");
    let error = zcutils::crypto_policy::initialize().unwrap_err();
    assert!(error.to_string().contains("TLS provider is not in FIPS mode"));
}
