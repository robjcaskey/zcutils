use serde_json::Value;
use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const ARCHIVE_SHA256: &str = "fe408fa438850786396faf79eba9ea4116c3802e60f3a95865f0dd2adb64c9f1";
const PREFIX: &str = "aws_lc_fips_0_13_11_";
const BUILD_CWD: &str = "aws-lc-AWS-LC-FIPS-3.1.0/build";

fn sha256(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

fn required_file(path: PathBuf, label: &str) -> PathBuf {
    assert!(path.is_file(), "missing {label}: {}", path.display());
    path
}

fn receipt_string<'a>(receipt: &'a Value, pointer: &str) -> &'a str {
    receipt
        .pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("provider receipt lacks {pointer}"))
}

fn unprefixed_bindings(target: &str) -> String {
    let source = match target {
        "x86_64-unknown-linux-gnu" => include_str!("bindings/x86_64_unknown_linux_gnu_crypto.rs"),
        "aarch64-unknown-linux-gnu" => include_str!("bindings/aarch64_unknown_linux_gnu_crypto.rs"),
        _ => panic!("certificate-5314 provider adapter does not support target {target}"),
    };
    let marker = format!("#[link_name = \"\\u{{1}}{PREFIX}");
    let mut removed = 0usize;
    let source = source.replace("#![allow(", "#[allow(");
    let mut output = String::with_capacity(source.len());
    for line in source.lines() {
        if line.trim_start().starts_with(&marker) {
            removed += 1;
        } else {
            output.push_str(line);
            output.push('\n');
        }
    }
    assert!(
        removed > 2500,
        "unexpected 0.13.11 binding prefix count: {removed}"
    );
    assert!(
        !output.contains(PREFIX),
        "prefixed symbol survived binding conversion"
    );
    output
}

fn run_probe(out_dir: &Path, include: &Path, crypto: &Path) {
    let source = out_dir.join("provider_probe.c");
    let binary = out_dir.join("provider_probe");
    fs::write(
        &source,
        r#"
#include <openssl/crypto.h>
#include <openssl/service_indicator.h>
#include <string.h>
int main(void) {
  return BORINGSSL_integrity_test() == 1 && FIPS_mode() == 1 &&
         strcmp(awslc_version_string(), "AWS-LC FIPS 3.1.0") == 0 ? 0 : 1;
}
"#,
    )
    .expect("write provider probe");
    let status = Command::new(env::var("CC").unwrap_or_else(|_| "cc".into()))
        .arg(&source)
        .arg("-I")
        .arg(include)
        .arg(crypto)
        .args(["-pthread", "-ldl", "-o"])
        .arg(&binary)
        .status()
        .expect("execute provider probe compiler");
    assert!(status.success(), "provider probe did not link");
    let status = Command::new(&binary)
        .status()
        .expect("execute provider probe");
    assert!(
        status.success(),
        "provider failed integrity, FIPS-mode, or identity probe"
    );
}

fn main() {
    println!("cargo:rerun-if-env-changed=AWS_LC_FIPS_SYS_SYSTEM_DIR");
    let root = PathBuf::from(
        env::var_os("AWS_LC_FIPS_SYS_SYSTEM_DIR")
            .expect("AWS_LC_FIPS_SYS_SYSTEM_DIR is required; bundled fallback is forbidden"),
    );
    assert!(root.is_absolute(), "provider directory must be absolute");
    let include = root.join("include");
    required_file(include.join("openssl/base.h"), "provider base.h");
    let crypto = required_file(root.join("lib/libcrypto.a"), "provider libcrypto.a");
    let receipt_path = required_file(
        root.join("share/zcutils/fips/provider-receipt.json"),
        "provider receipt",
    );
    let receipt_bytes = fs::read(&receipt_path).expect("read provider receipt");
    let receipt: Value = serde_json::from_slice(&receipt_bytes).expect("parse provider receipt");
    assert_eq!(receipt.pointer("/schema").and_then(Value::as_u64), Some(1));
    assert_eq!(
        receipt
            .pointer("/certificate_number")
            .and_then(Value::as_u64),
        Some(5314)
    );
    assert_eq!(
        receipt
            .pointer("/build_procedure_passed")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        receipt
            .pointer("/certificate_profile_environment")
            .and_then(Value::as_bool),
        Some(true),
        "provider was not built on a certificate-profile environment"
    );
    assert_eq!(
        receipt_string(&receipt, "/module_version_string"),
        "AWS-LC FIPS 3.1.0"
    );
    assert_eq!(
        receipt_string(&receipt, "/source/archive_sha256"),
        ARCHIVE_SHA256
    );
    assert_eq!(
        receipt_string(&receipt, "/provider/format"),
        "zc-aws-lc-fips-provider-v1"
    );
    assert_eq!(
        receipt_string(&receipt, "/provider/linkage"),
        "static-unprefixed"
    );
    assert_eq!(
        receipt_string(&receipt, "/provider/ffi_abi"),
        "aws-lc-fips-sys-0.13.11"
    );
    let commands = receipt
        .pointer("/commands")
        .and_then(Value::as_array)
        .expect("provider receipt lacks commands");
    assert_eq!(
        commands.len(),
        2,
        "provider receipt must contain exactly two build commands"
    );
    assert_eq!(
        commands[0].pointer("/cwd").and_then(Value::as_str),
        Some(BUILD_CWD)
    );
    assert_eq!(
        commands[0]
            .pointer("/argv")
            .and_then(Value::as_array)
            .map(|argv| argv.iter().filter_map(Value::as_str).collect::<Vec<_>>()),
        Some(vec!["cmake3", "-DFIPS=1", ".."])
    );
    assert_eq!(
        commands[1].pointer("/cwd").and_then(Value::as_str),
        Some(BUILD_CWD)
    );
    assert_eq!(
        commands[1]
            .pointer("/argv")
            .and_then(Value::as_array)
            .map(|argv| argv.iter().filter_map(Value::as_str).collect::<Vec<_>>()),
        Some(vec!["make"])
    );
    assert_eq!(
        receipt_string(&receipt, "/artifacts/libcrypto.a/sha256"),
        receipt_string(&receipt, "/provider/libcrypto_sha256")
    );
    assert_eq!(
        sha256(&fs::read(&crypto).expect("read provider libcrypto.a")),
        receipt_string(&receipt, "/provider/libcrypto_sha256")
    );
    let bcm = required_file(root.join("lib/bcm.o"), "provider bcm.o");
    assert_eq!(
        sha256(&fs::read(&bcm).expect("read provider bcm.o")),
        receipt_string(&receipt, "/provider/bcm_sha256")
    );

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    fs::write(
        out_dir.join("bindings.rs"),
        unprefixed_bindings(&env::var("TARGET").unwrap()),
    )
    .expect("write unprefixed bindings");
    fs::write(
        out_dir.join("provider_metadata.rs"),
        format!(
            "pub const ZC_AWS_LC_PROVIDER_RECEIPT_SHA256: &str = {:?};\n\
         pub const ZC_AWS_LC_PROVIDER_LIBCRYPTO_SHA256: &str = {:?};\n\
         pub const ZC_AWS_LC_PROVIDER_BCM_SHA256: &str = {:?};\n",
            sha256(&receipt_bytes),
            receipt_string(&receipt, "/provider/libcrypto_sha256"),
            receipt_string(&receipt, "/provider/bcm_sha256")
        ),
    )
    .expect("write provider metadata");
    run_probe(&out_dir, &include, &crypto);

    fs::write(
        out_dir.join("fips_startup_check.c"),
        r#"
#include <openssl/crypto.h>
#include <stdlib.h>
__attribute__((constructor)) static void zc_aws_lc_fips_startup_check(void) {
  if (BORINGSSL_integrity_test() != 1 || FIPS_mode() != 1) abort();
}
"#,
    )
    .expect("write startup check");
    cc::Build::new()
        .file(out_dir.join("fips_startup_check.c"))
        .include(&include)
        .warnings(false)
        .cargo_metadata(false)
        .compile("zc_aws_lc_fips_startup_check");

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    // The constructor is otherwise an unreferenced archive member. Force it into
    // every final executable so provider integrity is checked before main().
    println!("cargo:rustc-link-lib=static:+whole-archive=zc_aws_lc_fips_startup_check");
    println!(
        "cargo:rustc-link-search=native={}",
        root.join("lib").display()
    );
    println!("cargo:rustc-link-lib=static=crypto");
    println!("cargo:rustc-link-lib=pthread");
    println!("cargo:rustc-link-lib=dl");
    println!("cargo:include={}", include.display());
    println!("cargo:root={}", root.display());
    println!("cargo:libcrypto=crypto");
    println!("cargo:libcrypto_path={}", crypto.display());
    println!("cargo:link_kind=static");
    println!("cargo:rerun-if-changed={}", crypto.display());
    println!("cargo:rerun-if-changed={}", receipt_path.display());
}
