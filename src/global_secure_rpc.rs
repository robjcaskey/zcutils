//! Encrypted framing and optional mutually authenticated TLS for global RPC.
//!
//! Native frames expose only a fixed protocol marker, direction/flags, a
//! random nonce, and ciphertext length. The complete RPC document—including
//! federation, node, topology, policy and error fields—is inside AES-256-GCM.

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use sha2::{Digest, Sha256};
use std::env;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const FRAME_MAGIC: &[u8; 8] = b"ZCGRPC01";
const FRAME_HEADER_BYTES: usize = 28;
const FRAME_TAG_BYTES: usize = 16;
const FRAME_FLAG_NONE: u8 = 0;
const TLS_ALPN: &[u8] = b"zcglobal-rpc/1";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GlobalRpcTransport {
    #[default]
    NativeAead,
    NativeAeadWithTls,
}

impl GlobalRpcTransport {
    pub fn from_env() -> io::Result<Self> {
        match env::var("ZCGLOBAL_RPC_TRANSPORT")
            .unwrap_or_else(|_| "native-aead".into())
            .as_str()
        {
            "native-aead" => Ok(Self::NativeAead),
            "native-aead+tls" | "native-aead-with-tls" => Ok(Self::NativeAeadWithTls),
            other => Err(invalid(format!(
                "ZCGLOBAL_RPC_TRANSPORT must be native-aead or native-aead+tls, got {other:?}"
            ))),
        }
    }

    pub fn uses_tls(self) -> bool {
        self == Self::NativeAeadWithTls
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::NativeAead => "native-aead",
            Self::NativeAeadWithTls => "native-aead+tls",
        }
    }

    pub fn headline_performance_eligible(self) -> bool {
        self == Self::NativeAead
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameDirection {
    Request = 1,
    Response = 2,
}

#[derive(Clone, Debug)]
pub struct TlsIdentityFiles {
    ca_file: PathBuf,
    certificate_file: PathBuf,
    private_key_file: PathBuf,
    server_name: String,
}

impl TlsIdentityFiles {
    pub fn from_env(transport: GlobalRpcTransport) -> io::Result<Option<Self>> {
        if !transport.uses_tls() {
            return Ok(None);
        }
        Ok(Some(Self {
            ca_file: required_path_env("ZCGLOBAL_TLS_CA_FILE")?,
            certificate_file: required_path_env("ZCGLOBAL_TLS_CERT_FILE")?,
            private_key_file: required_path_env("ZCGLOBAL_TLS_KEY_FILE")?,
            server_name: env::var("ZCGLOBAL_TLS_SERVER_NAME")
                .map_err(|_| invalid("ZCGLOBAL_TLS_SERVER_NAME is required for TLS"))?,
        }))
    }

    /// Build for every new connection. Atomic file replacement therefore
    /// rotates certificates/keys and overlapping CA bundles without restart.
    pub fn client_stream(&self, socket: TcpStream) -> io::Result<GlobalRpcIo> {
        let roots = self.load_roots()?;
        let certificate_chain = load_certificates(&self.certificate_file)?;
        let private_key = load_private_key(&self.private_key_file)?;
        let mut config =
            rustls::ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
                .with_root_certificates(roots)
                .with_client_auth_cert(certificate_chain, private_key)
                .map_err(|error| {
                    invalid(format!("build global RPC TLS client identity: {error}"))
                })?;
        config.alpn_protocols = vec![TLS_ALPN.to_vec()];
        let server_name = ServerName::try_from(self.server_name.clone())
            .map_err(|_| invalid("ZCGLOBAL_TLS_SERVER_NAME is not a valid DNS name or IP"))?;
        let connection = rustls::ClientConnection::new(Arc::new(config), server_name)
            .map_err(|error| invalid(format!("create global RPC TLS client: {error}")))?;
        Ok(GlobalRpcIo::ClientTls(rustls::StreamOwned::new(
            connection, socket,
        )))
    }

    pub fn server_stream(&self, socket: TcpStream) -> io::Result<GlobalRpcIo> {
        let roots = self.load_roots()?;
        let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
            .build()
            .map_err(|error| invalid(format!("build global RPC TLS client verifier: {error}")))?;
        let certificate_chain = load_certificates(&self.certificate_file)?;
        let private_key = load_private_key(&self.private_key_file)?;
        let mut config =
            rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
                .with_client_cert_verifier(verifier)
                .with_single_cert(certificate_chain, private_key)
                .map_err(|error| {
                    invalid(format!("build global RPC TLS server identity: {error}"))
                })?;
        config.alpn_protocols = vec![TLS_ALPN.to_vec()];
        let connection = rustls::ServerConnection::new(Arc::new(config))
            .map_err(|error| invalid(format!("create global RPC TLS server: {error}")))?;
        Ok(GlobalRpcIo::ServerTls(rustls::StreamOwned::new(
            connection, socket,
        )))
    }

    fn load_roots(&self) -> io::Result<rustls::RootCertStore> {
        let mut roots = rustls::RootCertStore::empty();
        for certificate in load_certificates(&self.ca_file)? {
            roots.add(certificate).map_err(|error| {
                invalid(format!(
                    "invalid global RPC TLS CA in {}: {error}",
                    self.ca_file.display()
                ))
            })?;
        }
        if roots.is_empty() {
            return Err(invalid("global RPC TLS CA bundle is empty"));
        }
        Ok(roots)
    }
}

pub enum GlobalRpcIo {
    Native(TcpStream),
    ClientTls(rustls::StreamOwned<rustls::ClientConnection, TcpStream>),
    ServerTls(rustls::StreamOwned<rustls::ServerConnection, TcpStream>),
}

impl Read for GlobalRpcIo {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Native(stream) => stream.read(buffer),
            Self::ClientTls(stream) => stream.read(buffer),
            Self::ServerTls(stream) => stream.read(buffer),
        }
    }
}

impl Write for GlobalRpcIo {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            Self::Native(stream) => stream.write(buffer),
            Self::ClientTls(stream) => stream.write(buffer),
            Self::ServerTls(stream) => stream.write(buffer),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Native(stream) => stream.flush(),
            Self::ClientTls(stream) => stream.flush(),
            Self::ServerTls(stream) => stream.flush(),
        }
    }
}

pub fn write_encrypted_frame(
    stream: &mut impl Write,
    secret: &str,
    direction: FrameDirection,
    plaintext: &[u8],
) -> io::Result<()> {
    let ciphertext_len = plaintext
        .len()
        .checked_add(FRAME_TAG_BYTES)
        .and_then(|length| u32::try_from(length).ok())
        .ok_or_else(|| invalid("global RPC encrypted frame exceeds u32"))?;
    let mut nonce = [0u8; 12];
    File::open("/dev/urandom")?.read_exact(&mut nonce)?;
    let header = frame_header(direction, nonce, ciphertext_len);
    let cipher = frame_cipher(secret, direction)?;
    let mut ciphertext = plaintext.to_vec();
    cipher
        .seal_in_place_append_tag(
            aws_lc_rs::aead::Nonce::assume_unique_for_key(nonce),
            aws_lc_rs::aead::Aad::from(&header),
            &mut ciphertext,
        )
        .map_err(|_| io::Error::other("encrypt global RPC frame"))?;
    stream.write_all(&header)?;
    stream.write_all(&ciphertext)?;
    stream.flush()
}

pub fn read_encrypted_frame(
    stream: &mut impl Read,
    secrets: &[String],
    direction: FrameDirection,
    maximum_plaintext_bytes: usize,
) -> io::Result<(Vec<u8>, String)> {
    if secrets.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "no currently valid global RPC credential",
        ));
    }
    let mut header = [0u8; FRAME_HEADER_BYTES];
    stream.read_exact(&mut header)?;
    let (nonce, ciphertext_len) = validate_header(&header, direction)?;
    let maximum_ciphertext = maximum_plaintext_bytes
        .checked_add(FRAME_TAG_BYTES)
        .ok_or_else(|| invalid("global RPC encrypted frame limit overflow"))?;
    let ciphertext_len = ciphertext_len as usize;
    if ciphertext_len < FRAME_TAG_BYTES || ciphertext_len > maximum_ciphertext {
        return Err(invalid(
            "global RPC ciphertext length exceeds structural limit",
        ));
    }
    let mut ciphertext = vec![0u8; ciphertext_len];
    stream.read_exact(&mut ciphertext)?;
    for secret in secrets {
        let cipher = frame_cipher(secret, direction)?;
        let mut candidate = ciphertext.clone();
        if let Ok(plaintext) = cipher.open_in_place(
            aws_lc_rs::aead::Nonce::assume_unique_for_key(nonce),
            aws_lc_rs::aead::Aad::from(&header),
            &mut candidate,
        ) {
            let length = plaintext.len();
            candidate.truncate(length);
            return Ok((candidate, secret.clone()));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "global RPC authenticated decryption failed",
    ))
}

fn frame_cipher(
    secret: &str,
    direction: FrameDirection,
) -> io::Result<aws_lc_rs::aead::LessSafeKey> {
    let mut digest = Sha256::new();
    digest.update(b"zcglobal-rpc-native-aead-v1\0");
    digest.update([direction as u8]);
    digest.update(secret.as_bytes());
    let key = digest.finalize();
    let key = aws_lc_rs::aead::UnboundKey::new(&aws_lc_rs::aead::AES_256_GCM, &key)
        .map_err(|_| io::Error::other("derive global RPC AES-256-GCM key"))?;
    Ok(aws_lc_rs::aead::LessSafeKey::new(key))
}

fn frame_header(direction: FrameDirection, nonce: [u8; 12], ciphertext_len: u32) -> [u8; 28] {
    let mut header = [0u8; FRAME_HEADER_BYTES];
    header[..8].copy_from_slice(FRAME_MAGIC);
    header[8] = direction as u8;
    header[9] = FRAME_FLAG_NONE;
    header[12..24].copy_from_slice(&nonce);
    header[24..28].copy_from_slice(&ciphertext_len.to_be_bytes());
    header
}

fn validate_header(
    header: &[u8; FRAME_HEADER_BYTES],
    direction: FrameDirection,
) -> io::Result<([u8; 12], u32)> {
    if &header[..8] != FRAME_MAGIC
        || header[8] != direction as u8
        || header[9] != FRAME_FLAG_NONE
        || header[10..12] != [0, 0]
    {
        return Err(invalid("invalid global RPC public envelope"));
    }
    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(&header[12..24]);
    let ciphertext_len = u32::from_be_bytes(header[24..28].try_into().expect("u32"));
    Ok((nonce, ciphertext_len))
}

fn load_certificates(path: &Path) -> io::Result<Vec<CertificateDer<'static>>> {
    let certificates = CertificateDer::pem_file_iter(path)
        .map_err(|error| invalid(format!("read TLS certificate {}: {error}", path.display())))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| invalid(format!("parse TLS certificate {}: {error}", path.display())))?;
    if certificates.is_empty() {
        return Err(invalid(format!(
            "TLS certificate file {} contains no certificates",
            path.display()
        )));
    }
    Ok(certificates)
}

fn load_private_key(path: &Path) -> io::Result<PrivateKeyDer<'static>> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "TLS private key {} must be a mode-0600 regular file",
                path.display()
            ),
        ));
    }
    PrivateKeyDer::from_pem_file(path)
        .map_err(|error| invalid(format!("read TLS private key {}: {error}", path.display())))
}

fn required_path_env(name: &str) -> io::Result<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| invalid(format!("{name} is required for TLS")))
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn secret() -> String {
        "0123456789abcdef".repeat(4)
    }

    #[test]
    fn native_frame_round_trips_both_directions() {
        for direction in [FrameDirection::Request, FrameDirection::Response] {
            let payload = br#"{"federation_id":"private-us","volume":"secret-volume","offset":42}"#;
            let mut wire = Vec::new();
            write_encrypted_frame(&mut wire, &secret(), direction, payload).unwrap();
            assert!(
                !wire
                    .windows(b"private-us".len())
                    .any(|part| part == b"private-us")
            );
            assert!(
                !wire
                    .windows(b"secret-volume".len())
                    .any(|part| part == b"secret-volume")
            );
            let (decoded, matched) =
                read_encrypted_frame(&mut Cursor::new(wire), &[secret()], direction, 4096).unwrap();
            assert_eq!(decoded, payload);
            assert_eq!(matched, secret());
        }
    }

    #[test]
    fn direction_and_credentials_are_authenticated() {
        let mut wire = Vec::new();
        write_encrypted_frame(&mut wire, &secret(), FrameDirection::Request, b"sensitive").unwrap();
        assert!(
            read_encrypted_frame(
                &mut Cursor::new(wire.clone()),
                &["f".repeat(64)],
                FrameDirection::Request,
                4096,
            )
            .is_err()
        );
        assert!(
            read_encrypted_frame(
                &mut Cursor::new(wire),
                &[secret()],
                FrameDirection::Response,
                4096,
            )
            .is_err()
        );
    }
}
