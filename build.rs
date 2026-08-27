use std::env;
use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=src/ofi_shim.c");
    println!("cargo:rerun-if-env-changed=ZCUTILS_DISABLE_LIBFABRIC");
    // Installing or upgrading the EFA userspace stack changes the provider
    // extension ABI without changing this repository.  Track those headers
    // so a long-lived checkout cannot silently retain a shim compiled against
    // the distro libfabric headers after /opt/amazon/efa appears.
    println!("cargo:rerun-if-changed=/opt/amazon/efa/include/rdma/fabric.h");
    println!("cargo:rerun-if-changed=/opt/amazon/efa/include/rdma/fi_ext_efa.h");
    println!("cargo:rustc-check-cfg=cfg(zc_has_libfabric)");

    // A cross build may intentionally produce a TCP-only helper for a stage
    // that never owns an OFI endpoint (for example, the local block onramp).
    // Native release builds leave this unset and retain the full libfabric
    // path, so compatibility packaging cannot slow or weaken EFA builds.
    if env::var("ZCUTILS_DISABLE_LIBFABRIC")
        .ok()
        .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "yes" | "on"))
    {
        println!("cargo:warning=libfabric explicitly disabled for this build");
        return;
    }

    // Prefer the provider's matching headers and library when the AWS EFA
    // stack is installed.  Ubuntu's distro libfabric can be older than EFA's
    // userspace provider and lacks the endpoint options used by the shim.
    let include_override = env::var("ZCUTILS_LIBFABRIC_INCLUDE_DIR").ok();
    let include_dir = include_override.as_deref().or_else(|| {
        ["/opt/amazon/efa/include", "/usr/include"]
            .into_iter()
            .find(|dir| Path::new(dir).join("rdma/fabric.h").exists())
    });
    let Some(include_dir) = include_dir else {
        println!("cargo:warning=libfabric headers not found; OFI WAL commands disabled");
        return;
    };
    let lib_override = env::var("ZCUTILS_LIBFABRIC_LIB_DIR").ok();
    let lib_dir = lib_override.as_deref().or_else(|| {
        [
            "/opt/amazon/efa/lib",
            "/usr/lib",
            "/usr/lib64",
            "/usr/lib/aarch64-linux-gnu",
            "/usr/lib/x86_64-linux-gnu",
        ]
        .into_iter()
        .find(|dir| Path::new(dir).join("libfabric.so").exists())
    });

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR set by cargo");
    let obj = format!("{out_dir}/ofi_shim.o");
    let lib = format!("{out_dir}/libzc_ofi_shim.a");

    let cc = env::var("ZCUTILS_LIBFABRIC_CC").unwrap_or_else(|_| "cc".to_string());
    let ar = env::var("ZCUTILS_LIBFABRIC_AR").unwrap_or_else(|_| "ar".to_string());
    let cc_status = Command::new(&cc)
        .args([
            "-std=c11",
            "-O2",
            "-fPIC",
            "-Wall",
            "-Wextra",
            "-I",
            include_dir,
            "-c",
            "src/ofi_shim.c",
            "-o",
            &obj,
        ])
        .status()
        .expect("failed to run cc for OFI shim");
    if !cc_status.success() {
        panic!("cc failed while compiling src/ofi_shim.c");
    }

    let ar_status = Command::new(&ar)
        .args(["crs", &lib, &obj])
        .status()
        .expect("failed to run ar for OFI shim");
    if !ar_status.success() {
        panic!("ar failed while archiving OFI shim");
    }

    println!("cargo:rustc-link-search=native={out_dir}");
    if let Some(lib_dir) = lib_dir {
        println!("cargo:rustc-link-search=native={lib_dir}");
    }
    println!("cargo:rustc-link-lib=static=zc_ofi_shim");
    println!("cargo:rustc-link-lib=fabric");
    println!("cargo:rustc-cfg=zc_has_libfabric");
}
