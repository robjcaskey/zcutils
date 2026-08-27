use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{self, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use std::env;
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::ExitCode;
use zcutils::kernel_module_artifacts::{
    ZccusanKernelModuleBundle, ZccusanKernelModuleCatalog, ZccusanKernelModuleSource,
    inspect_kernel_module, validate_bundle_spec, validate_catalog_spec,
    validate_module_against_bundle, validate_source_spec,
};

fn usage() -> &'static str {
    "usage:\n  zccusan-kmod-bundle inspect MODULE.ko\n  zccusan-kmod-bundle validate-source SOURCE.yaml\n  zccusan-kmod-bundle validate-bundle BUNDLE.yaml\n  zccusan-kmod-bundle validate BUNDLE.yaml MODULE.ko\n  zccusan-kmod-bundle validate-catalog CATALOG.yaml\n  zccusan-kmod-bundle keygen PRIVATE.pk8 PUBLIC.raw\n  zccusan-kmod-bundle sign PAYLOAD PRIVATE.pk8 SIGNATURE.raw\n  zccusan-kmod-bundle verify PAYLOAD PUBLIC.raw SIGNATURE.raw"
}

fn open_yaml<T: serde::de::DeserializeOwned>(path: &str) -> Result<T, String> {
    let file = File::open(path).map_err(|error| format!("open {path}: {error}"))?;
    serde_yaml::from_reader(file).map_err(|error| format!("parse {path}: {error}"))
}

fn read_file(path: &str) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|error| format!("read {path}: {error}"))
}

fn write_new(path: &str, bytes: &[u8], private: bool) -> Result<(), String> {
    let mode = if private { 0o600 } else { 0o644 };
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(Path::new(path))
        .map_err(|error| format!("create {path} without overwriting: {error}"))?;
    file.write_all(bytes)
        .map_err(|error| format!("write {path}: {error}"))
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("inspect") => {
            let module = args.next().ok_or_else(|| usage().to_string())?;
            if args.next().is_some() {
                return Err(usage().into());
            }
            let observed = inspect_kernel_module(&module)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&observed).map_err(|error| error.to_string())?
            );
        }
        Some("validate") => {
            let bundle_path = args.next().ok_or_else(|| usage().to_string())?;
            let module_path = args.next().ok_or_else(|| usage().to_string())?;
            if args.next().is_some() {
                return Err(usage().into());
            }
            let bundle: ZccusanKernelModuleBundle = open_yaml(&bundle_path)?;
            let observed = inspect_kernel_module(&module_path)?;
            validate_module_against_bundle(&observed, &bundle.spec)?;
            println!(
                "valid module={} architecture={} kernelRelease={} sha256={}",
                observed.module_name,
                observed.architecture,
                observed.kernel_release,
                observed.sha256
            );
        }
        Some("validate-catalog") => {
            let catalog_path = args.next().ok_or_else(|| usage().to_string())?;
            if args.next().is_some() {
                return Err(usage().into());
            }
            let catalog: ZccusanKernelModuleCatalog = open_yaml(&catalog_path)?;
            validate_catalog_spec(&catalog.spec)?;
            println!(
                "valid catalogGeneration={} entries={} sha256={}",
                catalog.spec.catalog_generation,
                catalog.spec.entries.len(),
                catalog.spec.catalog.sha256
            );
        }
        Some("validate-source") => {
            let source_path = args.next().ok_or_else(|| usage().to_string())?;
            if args.next().is_some() {
                return Err(usage().into());
            }
            let source: ZccusanKernelModuleSource = open_yaml(&source_path)?;
            validate_source_spec(&source.spec)?;
            println!(
                "valid source endpoints={} catalogs={} keys={}",
                source.spec.endpoints.len(),
                source.spec.catalog_refs.len(),
                source.spec.trusted_public_key_refs.len()
            );
        }
        Some("keygen") => {
            let private_path = args.next().ok_or_else(|| usage().to_string())?;
            let public_path = args.next().ok_or_else(|| usage().to_string())?;
            if args.next().is_some() {
                return Err(usage().into());
            }
            let private = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
                .map_err(|_| "Ed25519 key generation failed".to_string())?;
            let key_pair = Ed25519KeyPair::from_pkcs8(private.as_ref())
                .map_err(|error| format!("generated PKCS#8 key was rejected: {error}"))?;
            write_new(&private_path, private.as_ref(), true)?;
            write_new(&public_path, key_pair.public_key().as_ref(), false)?;
            println!("generated Ed25519 keypair private={private_path} public={public_path}");
        }
        Some("sign") => {
            let payload_path = args.next().ok_or_else(|| usage().to_string())?;
            let private_path = args.next().ok_or_else(|| usage().to_string())?;
            let signature_path = args.next().ok_or_else(|| usage().to_string())?;
            if args.next().is_some() {
                return Err(usage().into());
            }
            let payload = read_file(&payload_path)?;
            let private = read_file(&private_path)?;
            let key_pair = Ed25519KeyPair::from_pkcs8(&private).map_err(|error| {
                format!("private key is not valid unencrypted Ed25519 PKCS#8: {error}")
            })?;
            let signature = key_pair.sign(&payload);
            write_new(&signature_path, signature.as_ref(), false)?;
            println!("signed payload={payload_path} signature={signature_path} format=Ed25519Raw");
        }
        Some("verify") => {
            let payload_path = args.next().ok_or_else(|| usage().to_string())?;
            let public_path = args.next().ok_or_else(|| usage().to_string())?;
            let signature_path = args.next().ok_or_else(|| usage().to_string())?;
            if args.next().is_some() {
                return Err(usage().into());
            }
            let payload = read_file(&payload_path)?;
            let public = read_file(&public_path)?;
            let signature_bytes = read_file(&signature_path)?;
            UnparsedPublicKey::new(&signature::ED25519, public)
                .verify(&payload, &signature_bytes)
                .map_err(|_| "Ed25519 signature verification failed".to_string())?;
            println!("valid signature payload={payload_path} publicKey={public_path}");
        }
        Some("validate-bundle") => {
            let bundle_path = args.next().ok_or_else(|| usage().to_string())?;
            if args.next().is_some() {
                return Err(usage().into());
            }
            let bundle: ZccusanKernelModuleBundle = open_yaml(&bundle_path)?;
            validate_bundle_spec(&bundle.spec)?;
            println!(
                "valid bundle module={} sha256={}",
                bundle.spec.module.name, bundle.spec.manifest.sha256
            );
        }
        _ => return Err(usage().into()),
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("zccusan-kmod-bundle: {error}");
            ExitCode::FAILURE
        }
    }
}
