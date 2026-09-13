use super::*;

fn token() -> String {
    "12".repeat(32)
}

#[test]
fn crypto_stream_roundtrip_and_authentication() {
    let plain = vec![0x57; 9001];
    let mut wire = Vec::new();
    assert_eq!(
        zc_encrypt_aes256_stream(&mut plain.as_slice(), &mut wire, &token(), 4096).unwrap(),
        plain.len() as u64
    );
    let mut output = Vec::new();
    assert_eq!(
        zc_decrypt_aes256_stream(&mut wire.as_slice(), &mut output, &token()).unwrap(),
        plain.len() as u64
    );
    assert_eq!(output, plain);
    let mut wrong_key = Vec::new();
    assert!(
        zc_decrypt_aes256_stream(&mut wire.as_slice(), &mut wrong_key, &"34".repeat(32)).is_err()
    );
    assert!(wrong_key.is_empty());
    // IV, ciphertext and first tag must all be authenticated.
    let start = ZC_AES256_FRAME_MAGIC.len() + 12 + 4;
    for offset in [start, start + 12, start + 4096 + ZC_AES256_TAG_BYTES - 1] {
        let mut altered = wire.clone();
        altered[offset] ^= 1;
        let mut output = Vec::new();
        assert!(zc_decrypt_aes256_stream(&mut altered.as_slice(), &mut output, &token()).is_err());
        assert!(output.is_empty());
    }
    #[cfg(feature = "fips")]
    {
        let mut missing_eof = wire[..wire.len() - ZC_AES256_TAG_BYTES].to_vec();
        assert!(
            zc_decrypt_aes256_stream(&mut missing_eof.as_slice(), &mut Vec::new(), &token())
                .is_err()
        );
        missing_eof[..ZC_AES256_FRAME_MAGIC.len()].copy_from_slice(b"ZC_AES256_GCM_FRAME_V1");
        assert!(
            zc_decrypt_aes256_stream(&mut missing_eof.as_slice(), &mut Vec::new(), &token())
                .is_err()
        );
        // An attacker cannot replace the first frame by an unauthenticated EOF.
        let mut truncated = wire[..ZC_AES256_FRAME_MAGIC.len() + 12].to_vec();
        truncated.extend_from_slice(&0u32.to_be_bytes());
        truncated.extend_from_slice(&wire[wire.len() - ZC_AES256_TAG_BYTES..]);
        assert!(
            zc_decrypt_aes256_stream(&mut truncated.as_slice(), &mut Vec::new(), &token()).is_err()
        );
    }
}

#[test]
fn crypto_lane_direction_sequence_and_session_are_bound() {
    let key = zcnblk_aes256_lane_cipher(&token(), 7, b"client-to-target").unwrap();
    let binding = zc_aes256_nonce(&[0x72; 12], 4);
    let aad = zc_aes256_lane_aad(7, 4, 4096, 5);
    let wire =
        zc_aes256_encrypt_frame(&key, binding, &aad, b"hello".to_vec(), || "seal".into()).unwrap();
    assert_eq!(
        zc_aes256_decrypt_frame(&key, binding, &aad, wire.clone(), || "open".into()).unwrap(),
        b"hello"
    );
    for other in [
        zcnblk_aes256_lane_cipher(&token(), 8, b"client-to-target").unwrap(),
        zcnblk_aes256_lane_cipher(&token(), 7, b"target-to-client").unwrap(),
        zc_aes256_lane_cipher(&token(), 7).unwrap(),
    ] {
        assert!(
            zc_aes256_decrypt_frame(&other, binding, &aad, wire.clone(), || "wrong key".into())
                .is_err()
        );
    }
    for bad in [
        zc_aes256_lane_aad(8, 4, 4096, 5),
        zc_aes256_lane_aad(7, 5, 4096, 5),
        zc_aes256_lane_aad(7, 4, 4097, 5),
    ] {
        assert!(
            zc_aes256_decrypt_frame(&key, binding, &bad, wire.clone(), || "wrong AAD".into())
                .is_err()
        );
    }
    assert!(
        zc_aes256_decrypt_frame(
            &key,
            zc_aes256_nonce(&[0x73; 12], 4),
            &aad,
            wire.clone(),
            || "wrong session".into()
        )
        .is_err()
    );
    #[cfg(feature = "fips")]
    {
        let next =
            zc_aes256_encrypt_frame(&key, binding, &aad, b"hello".to_vec(), || "seal".into())
                .unwrap();
        assert_ne!(
            &wire[..12],
            &next[..12],
            "IVs come from module RNG, not the repeated application binding"
        );
        assert!(zc_aes256_cipher("weak-password").is_err());
        assert!(zcnblk_payload_aes256_cipher(&token()).is_err());
    }
}

#[test]
fn crypto_rpc_header_ciphertext_rotation_and_limits() {
    use crate::global_secure_rpc::{FrameDirection, read_encrypted_frame, write_encrypted_frame};
    let mut wire = Vec::new();
    write_encrypted_frame(&mut wire, &token(), FrameDirection::Request, b"hello").unwrap();
    let (plain, matched) = read_encrypted_frame(
        &mut wire.as_slice(),
        &["34".repeat(32), token()],
        FrameDirection::Request,
        5,
    )
    .unwrap();
    assert_eq!(plain, b"hello");
    assert_eq!(matched, token());
    for offset in 0..wire.len() {
        let mut bad = wire.clone();
        bad[offset] ^= 1;
        assert!(
            read_encrypted_frame(&mut bad.as_slice(), &[token()], FrameDirection::Request, 5)
                .is_err(),
            "unauthenticated byte {offset}"
        );
    }
    assert!(
        read_encrypted_frame(
            &mut wire.as_slice(),
            &[token()],
            FrameDirection::Response,
            5
        )
        .is_err()
    );
    assert!(
        read_encrypted_frame(&mut wire.as_slice(), &[token()], FrameDirection::Request, 4).is_err()
    );
    assert!(read_encrypted_frame(&mut wire.as_slice(), &[], FrameDirection::Request, 5).is_err());
    #[cfg(feature = "fips")]
    {
        wire[..8].copy_from_slice(b"ZCGRPC01");
        assert!(
            read_encrypted_frame(&mut wire.as_slice(), &[token()], FrameDirection::Request, 5)
                .is_err()
        );
    }
}

#[cfg(feature = "fips")]
#[test]
fn crypto_fips_application_services_are_individually_approved() {
    let report = crate::fips_application_checks::collect();
    assert_eq!(report["passed"], true, "{report}");
}

#[cfg(feature = "fips")]
#[test]
fn crypto_kdf_matches_independent_counter_hmac_vector() {
    // Independently calculated HMAC-SHA256(K, [1]32 || FixedInfo), K=0x12*32.
    // This pins the wire KDF, including hex decoding and context framing.
    let expected = [
        63, 212, 201, 225, 93, 3, 48, 223, 125, 99, 118, 24, 180, 146, 245, 63, 232, 60, 65, 202,
        61, 238, 240, 50, 52, 209, 215, 182, 116, 205, 138, 75,
    ];
    let derived = zc_aes256_cipher(&token()).unwrap();
    let reference = zc_aes256_cipher_from_key(&expected).unwrap();
    let wire = zc_aes256_encrypt_frame(&derived, [0; 12], b"", b"KDF vector".to_vec(), || {
        "seal".into()
    })
    .unwrap();
    assert_eq!(
        zc_aes256_decrypt_frame(&reference, [0; 12], b"", wire, || "open".into()).unwrap(),
        b"KDF vector"
    );
}
