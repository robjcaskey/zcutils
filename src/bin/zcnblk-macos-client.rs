#[path = "../macos_scsi.rs"]
mod macos_scsi;
#[cfg(target_os = "macos")]
mod native {
    use super::macos_scsi::{BLOCK, Disk, Transport};
    use std::{
        ffi::{c_char, c_void},
        io,
        net::SocketAddr,
        time::Duration,
    };
    use zcutils::block::client::ZcnblkClient;
    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOServiceNameMatching(name: *const c_char) -> *mut c_void;
        fn IOServiceGetMatchingService(port: u32, dict: *mut c_void) -> u32;
        fn IOServiceOpen(service: u32, task: u32, kind: u32, connection: *mut u32) -> i32;
        fn IOServiceClose(connection: u32) -> i32;
        fn IOObjectRelease(object: u32) -> i32;
        fn IOConnectCallScalarMethod(
            connection: u32,
            selector: u32,
            input: *const u64,
            input_count: u32,
            output: *mut u64,
            output_count: *mut u32,
        ) -> i32;
        fn IOConnectMapMemory64(
            connection: u32,
            kind: u32,
            task: u32,
            address: *mut u64,
            size: *mut u64,
            options: u32,
        ) -> i32;
        fn IOConnectUnmapMemory64(connection: u32, kind: u32, task: u32, address: u64) -> i32;
    }
    unsafe extern "C" {
        static mach_task_self_: u32;
    }
    struct Connection {
        handle: u32,
        address: u64,
    }
    impl Connection {
        fn call(&self, selector: u32, input: &[u64], output: &mut [u64]) -> io::Result<()> {
            let mut count = output.len() as u32;
            let code = unsafe {
                IOConnectCallScalarMethod(
                    self.handle,
                    selector,
                    input.as_ptr(),
                    input.len() as u32,
                    output.as_mut_ptr(),
                    &mut count,
                )
            };
            if code != 0 {
                return Err(io::Error::other(format!(
                    "DriverKit selector {selector}: {code:#x}"
                )));
            }
            if count as usize != output.len() {
                return Err(io::Error::other("DriverKit ABI output count mismatch"));
            }
            Ok(())
        }
    }
    impl Drop for Connection {
        fn drop(&mut self) {
            unsafe {
                if self.address != 0 {
                    IOConnectUnmapMemory64(self.handle, 0, mach_task_self_, self.address);
                }
                IOServiceClose(self.handle);
            }
        }
    }
    struct Network(ZcnblkClient);
    impl Transport for Network {
        fn read(&mut self, o: u64, b: &mut [u8]) -> io::Result<()> {
            self.0.read_at(o, b)
        }
        fn write(&mut self, o: u64, b: &[u8]) -> io::Result<()> {
            self.0.write_at(o, b)
        }
        fn sync(&mut self) -> io::Result<()> {
            self.0.sync()
        }
    }
    pub fn run() -> io::Result<()> {
        let args: Vec<_> = std::env::args().collect();
        if args.len() != 3 {
            return Err(io::Error::other(
                "usage: zcnblk-macos-client ENDPOINT CAPACITY_BYTES (4096-byte sectors; exclusive writer required)",
            ));
        }
        let address: SocketAddr = args[1].parse().map_err(io::Error::other)?;
        let capacity: u64 = args[2].parse().map_err(io::Error::other)?;
        let mut disk = Disk::new(
            Network(ZcnblkClient::connect(address, Duration::from_secs(15))?),
            capacity,
        )?;
        // Verify endpoint connectivity before publishing media. Never infer capacity.
        disk.transport.sync()?;
        let service = unsafe {
            IOServiceGetMatchingService(0, IOServiceNameMatching(c"ZcController".as_ptr()))
        };
        if service == 0 {
            return Err(io::Error::other(
                "ZcController not found; activate the DriverKit extension first",
            ));
        }
        let mut handle = 0;
        let code = unsafe {
            let r = IOServiceOpen(service, mach_task_self_, 0, &mut handle);
            IOObjectRelease(service);
            r
        };
        if code != 0 {
            return Err(io::Error::other(format!(
                "IOServiceOpen: {code:#x}; check userclient-access entitlement"
            )));
        }
        let mut connection = Connection { handle, address: 0 };
        let mut size = 0;
        let code = unsafe {
            IOConnectMapMemory64(
                handle,
                0,
                mach_task_self_,
                &mut connection.address,
                &mut size,
                1,
            )
        };
        if code != 0 || size < BLOCK as u64 {
            return Err(io::Error::other(format!(
                "map DriverKit staging buffer: {code:#x}"
            )));
        }
        connection.call(0, &[1], &mut [])?;
        eprintln!(
            "DriverKit disk active: {capacity} bytes, 4096-byte sectors, one outstanding task; no performance claim"
        );
        loop {
            let mut meta = [0u64; 6];
            connection.call(1, &[], &mut meta)?;
            if meta[0] == 0 {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            let len = meta[1] as usize;
            let cmdlen = meta[3] as usize;
            if len > BLOCK || cmdlen > 16 || meta[2] > 2 {
                return Err(io::Error::other("invalid DriverKit request"));
            }
            let mut cdb = [0; 16];
            cdb[..8].copy_from_slice(&meta[4].to_le_bytes());
            cdb[8..].copy_from_slice(&meta[5].to_le_bytes());
            // Local snapshot: the driver must not observe broker writes until completion.
            let mut data = [0u8; BLOCK];
            if meta[2] == 2 {
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        connection.address as *const u8,
                        data.as_mut_ptr(),
                        len,
                    )
                }
            }
            let result = disk.execute(&cdb[..cmdlen], meta[2] as u8, &mut data[..len]);
            if result.key == 0 && meta[2] == 1 {
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        data.as_ptr(),
                        connection.address as *mut u8,
                        result.bytes,
                    )
                }
            }
            connection.call(
                2,
                &[
                    meta[0],
                    result.bytes as u64,
                    result.key as u64,
                    result.asc as u64,
                ],
                &mut [],
            )?;
            if result.key == 4 {
                return Err(io::Error::other(
                    "remote transport failed; disk session retired",
                ));
            }
        }
    }
}
fn main() -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        zcutils::crypto_policy::initialize_or_exit();
        native::run()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(std::io::Error::other("requires macOS DriverKit"))
    }
}
