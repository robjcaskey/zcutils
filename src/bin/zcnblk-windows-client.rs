#[cfg(windows)]
#[path = "../windows_native_client.rs"]
mod windows_native_client;

#[cfg(windows)]
fn main() -> std::io::Result<()> {
    zcutils::crypto_policy::initialize_or_exit();
    windows_native_client::run()
}

#[cfg(not(windows))]
fn main() -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "zcnblk-windows-client requires Windows and the zcnblk Storport driver",
    ))
}
