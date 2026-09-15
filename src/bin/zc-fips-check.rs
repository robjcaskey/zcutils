#[cfg(feature = "fips")]
#[path = "../fips_key_usage.rs"]
mod fips_key_usage;
// Compile the probe independently of the storage library's platform code.
#[path = "../crypto_policy.rs"]
#[allow(dead_code)]
mod crypto_policy;

fn main() {
    crypto_policy::emit_evidence_if_requested();
    let mut require_provider = false;
    let mut require_host = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--require-fips" => require_provider = true,
            "--require-host-fips" => require_host = true,
            "--help" => {
                println!(
                    "zc-fips-check [--require-fips] [--require-host-fips]\nzc-fips-check --fips-evidence\nReports runtime evidence only; never asserts CMVP validation."
                );
                return;
            }
            _ => {
                eprintln!("unknown argument: {arg}");
                std::process::exit(2);
            }
        }
    }
    let result = crypto_policy::require_mode(require_provider, require_host);
    println!(
        "{}",
        serde_json::json!({
            "schema": 1,
            "fips_feature": cfg!(feature = "fips"),
            "aws_lc_fips_mode": aws_lc_rs::try_fips_mode().is_ok(),
            "tls_provider_fips": rustls::crypto::CryptoProvider::get_default().is_some_and(|p| p.fips()),
            "host_fips_enabled": crypto_policy::host_fips_enabled(),
            "architecture": std::env::consts::ARCH,
            "validation_claim": "none; FIPS-aspiring runtime check only",
            "passed": result.is_ok(),
            "error": result.as_ref().err().map(ToString::to_string),
        })
    );
    if result.is_err() {
        std::process::exit(1);
    }
}
