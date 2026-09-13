//! Bounded, synchronous acceptance diagnostics for the actual linked module.
//! The negative controls intentionally request non-approved services using
//! disposable test data. This diagnostic process is not an approved workload.
use serde_json::{json, Value};

#[cfg(feature = "fips")]
fn indicated(name: &str, approved: bool, operation: impl FnOnce() -> bool) -> Value {
    // The counters are thread-local. No async work or unrelated crypto may run
    // between these calls. A successful operation alone is never approval.
    let before = unsafe { aws_lc_fips_sys::FIPS_service_indicator_before_call() };
    let functional = operation();
    let after = unsafe { aws_lc_fips_sys::FIPS_service_indicator_after_call() };
    json!({"name": name, "functional": functional, "expected_approved": approved,
           "indicator_before": before, "indicator_after": after,
           "approved": before != after, "passed": functional && (before != after) == approved})
}

#[cfg(feature = "fips")]
fn module_tests() -> Value {
    use aws_lc_rs::{
        aead, digest, hmac,
        rand::{SecureRandom, SystemRandom},
    };
    // All inputs are disposable public test data, never application secrets.
    let module_version = unsafe {
        // AWS-LC owns a static, NUL-terminated string. Do not infer its value
        // from Cargo versions or the crate README.
        let pointer = aws_lc_fips_sys::awslc_version_string();
        if pointer.is_null() {
            None
        } else {
            Some(
                std::ffi::CStr::from_ptr(pointer)
                    .to_string_lossy()
                    .into_owned(),
            )
        }
    };
    let self_test = unsafe { aws_lc_fips_sys::BORINGSSL_self_test() == 1 };
    let integrity_test = unsafe { aws_lc_fips_sys::BORINGSSL_integrity_test() == 1 };
    let mut tests = vec![indicated("sha256_known_answer", true, || {
        digest::digest(&digest::SHA256, b"abc").as_ref()
            == [
                0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
                0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
                0xf2, 0x00, 0x15, 0xad,
            ]
    })];
    let mac_key = hmac::Key::new(hmac::HMAC_SHA256, &[0x0b; 20]);
    tests.push(indicated("hmac_sha256_known_answer", true, || {
        hmac::sign(&mac_key, b"Hi There").as_ref()
            == [
                0xb0, 0x34, 0x4c, 0x61, 0xd8, 0xdb, 0x38, 0x53, 0x5c, 0xa8, 0xaf, 0xce, 0xaf, 0x0b,
                0xf1, 0x2b, 0x88, 0x1d, 0xc2, 0x00, 0xc9, 0x83, 0x3d, 0xa7, 0x26, 0xe9, 0x37, 0x6c,
                0x2e, 0x32, 0xcf, 0xf7,
            ]
    }));
    let mut key = [0u8; 32];
    tests.push(indicated("module_random", true, || {
        SystemRandom::new().fill(&mut key).is_ok()
    }));
    if let Ok(cipher) = aead::RandomizedNonceKey::new(&aead::AES_256_GCM, &key) {
        let plaintext = b"zcutils FIPS acceptance diagnostic";
        let mut buffer = plaintext.to_vec();
        let mut nonce = None;
        tests.push(indicated("aes256_gcm_internal_nonce_encrypt", true, || {
            nonce = cipher
                .seal_in_place_append_tag(aead::Aad::empty(), &mut buffer)
                .ok();
            nonce.is_some()
        }));
        if let Some(nonce) = nonce {
            let nonce_bytes: [u8; 12] = *nonce.as_ref();
            let mut damaged = buffer.clone();
            *damaged.last_mut().expect("GCM ciphertext includes a tag") ^= 1;
            tests.push(indicated("aes256_gcm_decrypt", true, || {
                cipher
                    .open_in_place(nonce, aead::Aad::empty(), &mut buffer)
                    .is_ok_and(|result| result == plaintext)
            }));
            let rejects_tamper = cipher
                .open_in_place(
                    aead::Nonce::assume_unique_for_key(nonce_bytes),
                    aead::Aad::empty(),
                    &mut damaged,
                )
                .is_err();
            tests.push(json!({"name": "rejects_modified_ciphertext", "passed": rejects_tamper}));
        }
    }
    // This is the generic external-nonce API used by our legacy frame paths.
    // It may successfully encrypt, but must not be mistaken for an approved
    // random-IV service. A fresh key is used only once in this diagnostic.
    if let Ok(key) = aead::UnboundKey::new(&aead::AES_256_GCM, &[0x42; 32]) {
        let key = aead::LessSafeKey::new(key);
        let mut data = b"external nonce negative control".to_vec();
        tests.push(indicated("external_nonce_is_not_approved", false, || {
            key.seal_in_place_append_tag(
                aead::Nonce::assume_unique_for_key([0u8; 12]),
                aead::Aad::empty(),
                &mut data,
            )
            .is_ok()
        }));
    }
    tests.push(indicated(
        "standalone_sha2_is_outside_module",
        false,
        || {
            use sha2::Digest;
            std::hint::black_box(sha2::Sha256::digest(std::hint::black_box(
                b"outside module",
            )))
            .len()
                == 32
        },
    ));
    json!({"module_version_string": module_version, "self_test": self_test,
           "integrity_test": integrity_test,
           "provider_receipt_sha256": aws_lc_fips_sys::ZC_AWS_LC_PROVIDER_RECEIPT_SHA256,
           "provider_libcrypto_sha256": aws_lc_fips_sys::ZC_AWS_LC_PROVIDER_LIBCRYPTO_SHA256,
           "provider_bcm_sha256": aws_lc_fips_sys::ZC_AWS_LC_PROVIDER_BCM_SHA256,
           "services": tests})
}

pub fn collect() -> Value {
    let initialization = super::initialize();
    let mode = aws_lc_rs::try_fips_mode().is_ok();
    let provider = rustls::crypto::CryptoProvider::get_default();
    let provider_fips = provider.is_some_and(|value| value.fips());
    let tls_config_fips = if initialization.is_ok() && provider_fips {
        rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth()
            .fips()
    } else {
        false
    };
    #[cfg(feature = "fips")]
    let module = if initialization.is_ok() && mode {
        module_tests()
    } else {
        Value::Null
    };
    #[cfg(not(feature = "fips"))]
    let module = Value::Null;
    let services_pass = module["services"]
        .as_array()
        .is_some_and(|tests| tests.len() == 8 && tests.iter().all(|test| test["passed"] == true));
    let passed = cfg!(feature = "fips")
        && mode
        && provider_fips
        && tls_config_fips
        && module["self_test"] == true
        && module["integrity_test"] == true
        && services_pass;
    json!({"schema": 2, "fips_feature": cfg!(feature = "fips"),
           "aws_lc_fips_mode": mode, "tls_provider_fips": provider_fips,
           "tls_client_config_fips": tls_config_fips,
           "host_fips_enabled": super::host_fips_enabled(),
           "architecture": std::env::consts::ARCH,
           "container_os_release": std::fs::read_to_string("/etc/os-release").ok(),
           "module": module, "passed": passed,
           "error": initialization.err().map(|error| error.to_string()),
           "validation_claim": "none; diagnostic controls, not a CMVP validation or application service audit"})
}
