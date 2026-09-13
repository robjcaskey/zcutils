//! FIPS wire-v2 primitives. Certificate coverage still depends on the build
//! and operating environment; these APIs alone do not establish validation.
use aws_lc_rs::{aead, kdf};
use std::io;

pub(crate) fn random(bytes: &mut [u8]) -> io::Result<()> {
    use aws_lc_rs::rand::{SecureRandom, SystemRandom};
    SystemRandom::new()
        .fill(bytes)
        .map_err(|_| io::Error::other("AWS-LC random generation failed"))
}

pub(crate) type Cipher = aead::RandomizedNonceKey;

pub(crate) fn from_key(key: &[u8]) -> io::Result<Cipher> {
    Cipher::new(&aead::AES_256_GCM, key)
        .map_err(|_| io::Error::other("AWS-LC AES-256-GCM key initialization failed"))
}

pub(crate) fn derive(secret: &str, domain: &[u8], context: &[u8]) -> io::Result<Cipher> {
    // Credentials are 256-bit random keys, never passwords. Decode the random
    // component of a transfer token; bind its complete representation in info.
    let material = if secret.starts_with("zct1.") {
        let parts: Vec<_> = secret.split('.').collect();
        if parts.len() != 4 || parts[1].parse::<u64>().is_err() || parts[2].parse::<u64>().is_err()
        {
            return Err(io::Error::other("invalid transfer key envelope"));
        }
        parts[3]
    } else {
        secret
    };
    if material.len() != 64 || !material.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(io::Error::other(
            "FIPS encryption requires a 32-byte random hexadecimal credential",
        ));
    }
    let mut input = [0u8; 32];
    for (byte, pair) in input.iter_mut().zip(material.as_bytes().chunks_exact(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).expect("ASCII hex"), 16)
            .expect("hex checked");
    }
    // SP 800-108 Counter HMAC SHA-256, 32-bit counter supplied by AWS-LC.
    // Fixed info = Label || 0 || length-prefixed context || L (256 bits).
    let mut info = domain.to_vec();
    info.push(0);
    for part in [secret.as_bytes(), context] {
        let len =
            u32::try_from(part.len()).map_err(|_| io::Error::other("KDF context too large"))?;
        info.extend_from_slice(&len.to_be_bytes());
        info.extend_from_slice(part);
    }
    info.extend_from_slice(&256u32.to_be_bytes());
    let algorithm = kdf::get_kbkdf_ctr_hmac_algorithm(kdf::KbkdfCtrHmacAlgorithmId::Sha256)
        .expect("SHA256 supported");
    let mut key = [0u8; 32];
    let result = kdf::kbkdf_ctr_hmac(algorithm, &input, &info, &mut key)
        .map_err(|_| io::Error::other("AWS-LC SP 800-108 key derivation failed"))
        .and_then(|()| from_key(&key));
    // Zeroize raw key buffers and secret-bearing fixed info if the native call fails too.
    use zeroize::Zeroize;
    input.zeroize();
    key.zeroize();
    info.zeroize();
    result
}

/// The supplied binding is authenticated data (session/sequence), NOT an IV.
/// Wire: module-generated 96-bit IV || ciphertext || 128-bit GCM tag.
pub(crate) fn seal(
    cipher: &Cipher,
    binding: &[u8],
    aad: &[u8],
    mut plaintext: Vec<u8>,
) -> io::Result<Vec<u8>> {
    let mut authenticated = binding.to_vec();
    authenticated.extend_from_slice(aad);
    let nonce = cipher
        .seal_in_place_append_tag(aead::Aad::from(&authenticated), &mut plaintext)
        .map_err(|_| io::Error::other("AWS-LC internal-nonce AES-GCM encryption failed"))?;
    let mut wire = Vec::with_capacity(12 + plaintext.len());
    wire.extend_from_slice(nonce.as_ref());
    wire.extend_from_slice(&plaintext);
    Ok(wire)
}

pub(crate) fn open(
    cipher: &Cipher,
    binding: &[u8],
    aad: &[u8],
    wire: &[u8],
) -> io::Result<Vec<u8>> {
    if wire.len() < 28 {
        return Err(io::Error::other("short FIPS AES-GCM frame"));
    }
    let nonce = aead::Nonce::try_assume_unique_for_key(&wire[..12])
        .map_err(|_| io::Error::other("invalid GCM IV"))?;
    let mut authenticated = binding.to_vec();
    authenticated.extend_from_slice(aad);
    let mut plaintext = wire[12..].to_vec();
    let len = cipher
        .open_in_place(nonce, aead::Aad::from(&authenticated), &mut plaintext)
        .map_err(|_| io::Error::other("AES-GCM authentication failed"))?
        .len();
    plaintext.truncate(len);
    Ok(plaintext)
}
