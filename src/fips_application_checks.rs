//! Exercise real application crypto paths with disposable, in-memory data.
//! These diagnostic calls may enter non-approved mode. They must never be
//! described as an approved workload or as complete service-coverage evidence.
use serde_json::{Value, json};

#[cfg(feature = "fips")]
fn observe<T>(
    checks: &mut Vec<Value>,
    name: &str,
    operation: impl FnOnce() -> std::io::Result<T>,
) -> Option<T> {
    let before = unsafe { aws_lc_fips_sys::FIPS_service_indicator_before_call() };
    let result = operation();
    let after = unsafe { aws_lc_fips_sys::FIPS_service_indicator_after_call() };
    checks.push(json!({"name": name, "functional": result.is_ok(),
        "indicator_before": before, "indicator_after": after,
        "approved_service_observed": before != after,
        "passed": result.is_ok() && before != after,
        "error": result.as_ref().err().map(ToString::to_string)}));
    result.ok()
}

#[cfg(feature = "fips")]
fn application_checks() -> Vec<Value> {
    use crate::global_secure_rpc::{self, FrameDirection};
    let mut checks = Vec::new();
    let token = "11".repeat(32);
    let _ = observe(
        &mut checks,
        "native_token_rng",
        super::zc_tcpmux_generate_token,
    );
    let _ = observe(&mut checks, "secret_lifecycle_rng", || {
        let policy = crate::secret_lifecycle::SecretPolicy {
            minimum_secret_bytes: 32,
            maximum_ttl_ms: 60_000,
            rotate_before_ms: 10_000,
            activation_clock_skew_ms: 0,
            maximum_versions: 3,
        };
        crate::secret_lifecycle::SecretBundle::new(1_000, 30_000, &policy)
    });
    let _ = observe(&mut checks, "native_byte_rng", || {
        super::zc_random_bytes(&mut [0u8; 32])
    });
    let _ = observe(&mut checks, "native_key_derivation", || {
        super::zc_aes256_cipher(&token)
    });
    let _ = observe(&mut checks, "native_lane_key_derivation", || {
        super::zc_aes256_lane_cipher(&token, 7)
    });
    // Isolate encryption from key derivation so a successful KDF cannot mask
    // a non-approved encrypt operation in the before/after service counter.
    if let Ok(key) = super::zc_aes256_cipher_from_key(&[0x35; 32]) {
        let plaintext = b"native frame acceptance diagnostic";
        let encrypted = observe(&mut checks, "native_frame_encrypt", || {
            super::zc_aes256_encrypt_frame(&key, [0u8; 12], b"test", plaintext.to_vec(), || {
                "diagnostic encrypt".into()
            })
        });
        if let Some(encrypted) = encrypted {
            let mut damaged = encrypted.clone();
            *damaged.last_mut().expect("AEAD tag") ^= 1;
            let result = observe(&mut checks, "native_frame_decrypt", || {
                let clear =
                    super::zc_aes256_decrypt_frame(&key, [0u8; 12], b"test", encrypted, || {
                        "diagnostic decrypt".into()
                    })?;
                if clear != plaintext {
                    return Err(std::io::Error::other("round-trip plaintext mismatch"));
                }
                Ok(())
            });
            let _ = result;
            let rejects = super::zc_aes256_decrypt_frame(&key, [0u8; 12], b"test", damaged, || {
                "diagnostic tamper".into()
            })
            .is_err();
            checks.push(json!({"name": "native_tamper_rejected", "passed": rejects}));
        }
    }
    let _ = observe(&mut checks, "zcnblk_key_derivation", || {
        super::zcnblk_aes256_lane_cipher(&token, 7, b"client-to-target")
    });
    let _ = observe(&mut checks, "global_rpc_key_derivation", || {
        global_secure_rpc::frame_cipher(&token, FrameDirection::Request)
    });
    if let Ok(cipher) = super::zc_aes256_cipher_from_key(&[0x37; 32]) {
        let plaintext = b"RPC frame acceptance diagnostic";
        if let Some((mut header, ciphertext)) = observe(&mut checks, "global_rpc_encrypt", || {
            global_secure_rpc::encrypt_payload(&cipher, FrameDirection::Request, plaintext)
        }) {
            let mut wire = header[12..24].to_vec();
            wire.extend_from_slice(&ciphertext);
            header[12..24].fill(0);
            let _ = observe(&mut checks, "global_rpc_decrypt", || {
                let clear = crate::approved_crypto::open(&cipher, b"", &header, &wire)?;
                if clear != plaintext {
                    return Err(std::io::Error::other("RPC plaintext mismatch"));
                }
                Ok(())
            });
        }
    }
    checks
}

pub fn collect() -> Value {
    let initialization = crate::crypto_policy::initialize();
    let mode = aws_lc_rs::try_fips_mode().is_ok();
    #[cfg(feature = "fips")]
    let checks = if initialization.is_ok() && mode {
        application_checks()
    } else {
        Vec::new()
    };
    #[cfg(not(feature = "fips"))]
    let checks: Vec<Value> = Vec::new();
    let passed = mode && checks.len() == 12 && checks.iter().all(|check| check["passed"] == true);
    json!({"schema": 1, "fips_feature": cfg!(feature = "fips"), "aws_lc_fips_mode": mode,
           "checks": checks, "passed": passed,
           "error": initialization.err().map(|error| error.to_string()),
           "validation_claim": "none; selected application paths only; mixed calls still require service-map review"})
}

pub fn emit_if_requested() {
    if std::env::args().skip(1).collect::<Vec<_>>() == ["--fips-application-evidence"] {
        let report = collect();
        println!("{report}");
        std::process::exit(if report["passed"] == true { 0 } else { 1 });
    }
}
