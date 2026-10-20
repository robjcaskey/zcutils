// Keep the Linux data plane intact while exposing portable userspace components.
#[cfg(target_os = "linux")]
include!("lib.rs");

#[cfg(windows)]
include!("windows.rs");

#[cfg(target_os = "macos")]
include!("macos.rs");

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
compile_error!("zcutils currently supports Linux, macOS, and Windows");
