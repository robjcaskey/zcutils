use std::env;
use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=src/ofi_shim.c");
    println!("cargo:rustc-check-cfg=cfg(zc_has_libfabric)");

    // Prefer the provider's matching headers and library when the AWS EFA
    // stack is installed.  Ubuntu's distro libfabric can be older than EFA's
    // userspace provider and lacks the endpoint options used by the shim.
    let include_dir = ["/opt/amazon/efa/include", "/usr/include"]
        .into_iter()
        .find(|dir| Path::new(dir).join("rdma/fabric.h").exists());
    let Some(include_dir) = include_dir else {
        println!("cargo:warning=libfabric headers not found; OFI WAL commands disabled");
        return;
    };
    let lib_dir = [
        "/opt/amazon/efa/lib",
        "/usr/lib",
        "/usr/lib64",
        "/usr/lib/aarch64-linux-gnu",
        "/usr/lib/x86_64-linux-gnu",
    ]
    .into_iter()
    .find(|dir| Path::new(dir).join("libfabric.so").exists());

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR set by cargo");
    let obj = format!("{out_dir}/ofi_shim.o");
    let lib = format!("{out_dir}/libzc_ofi_shim.a");

    let cc_status = Command::new("cc")
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

    let ar_status = Command::new("ar")
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
