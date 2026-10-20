//! Userspace transport for the native Windows block edge. No placement policy.
use sha2::{Digest, Sha256};
use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::{offset_of, size_of};
use std::net::SocketAddr;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;
use zcutils::block::client::ZcnblkClient;

const MAGIC: u64 = 0x5a434e4257494e31;
const VERSION: u32 = 1;
const QUERY: u32 = 1;
const CONNECT: u32 = 2;
const EXCHANGE: u32 = 3;
const DISCONNECT: u32 = 4;
const WRITE: u32 = 1;
const READ: u32 = 2;
const SYNC: u32 = 7;
const FUA: u32 = 1;
const HEADER: usize = 80;
const MAX_IO: usize = 65536;
// CTL_CODE(IOCTL_SCSI_BASE, 0x040e, METHOD_BUFFERED, FILE_READ_ACCESS | FILE_WRITE_ACCESS)
const IOCTL_MINIPORT_PROCESS_SERVICE_IRP: u32 = (4 << 16) | (3 << 14) | (0x040e << 2);
static STOP: AtomicBool = AtomicBool::new(false);

#[repr(C)]
struct Message {
    magic: u64,
    version: u32,
    command: u32,
    session: u64,
    request_id: u64,
    offset: u64,
    capacity: u64,
    op: u32,
    length: u32,
    flags: u32,
    status: u32,
    unit_id: [u8; 16],
    data: [u8; MAX_IO],
}

impl Message {
    fn new(command: u32, session: u64) -> Self {
        Self {
            magic: MAGIC,
            version: VERSION,
            command,
            session,
            request_id: 0,
            offset: 0,
            capacity: 0,
            op: 0,
            length: 0,
            flags: 0,
            status: 0,
            unit_id: [0; 16],
            data: [0; MAX_IO],
        }
    }
}

const _: () = assert!(size_of::<Message>() == HEADER + MAX_IO);
const _: () = assert!(offset_of!(Message, data) == HEADER);

#[link(name = "kernel32")]
unsafe extern "system" {
    fn DeviceIoControl(
        handle: *mut c_void,
        code: u32,
        input: *mut c_void,
        input_len: u32,
        output: *mut c_void,
        output_len: u32,
        returned: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn SetConsoleCtrlHandler(
        handler: Option<unsafe extern "system" fn(u32) -> i32>,
        add: i32,
    ) -> i32;
}

unsafe extern "system" fn control_handler(event: u32) -> i32 {
    if matches!(event, 0 | 1 | 2 | 5 | 6) {
        STOP.store(true, Ordering::Release);
        1
    } else {
        0
    }
}

struct Adapter {
    file: File,
    path: String,
    session: u64,
}

impl Adapter {
    fn open(path: String) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(3)
            .open(&path)
            .map_err(|e| io::Error::new(e.kind(), format!("open {path}: {e}")))?;
        let adapter = Self {
            file,
            path,
            session: 0,
        };
        let mut message = Message::new(QUERY, 0);
        adapter
            .ioctl(&mut message)
            .map_err(|e| io::Error::new(e.kind(), format!("query {}: {e}", adapter.path)))?;
        Ok(adapter)
    }

    fn ioctl(&self, message: &mut Message) -> io::Result<()> {
        let input = HEADER
            + if message.command == EXCHANGE
                && message.request_id != 0
                && message.op == READ
                && message.status == 0
            {
                message.length as usize
            } else {
                0
            };
        if input > size_of::<Message>() {
            return Err(io::Error::other(
                "native client input length exceeds ABI capacity",
            ));
        }
        let mut returned = 0;
        let pointer = (message as *mut Message).cast();
        let success = unsafe {
            DeviceIoControl(
                self.file.as_raw_handle(),
                IOCTL_MINIPORT_PROCESS_SERVICE_IRP,
                pointer,
                input as u32,
                pointer,
                size_of::<Message>() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if success == 0 {
            return Err(io::Error::last_os_error());
        }
        if returned < HEADER as u32
            || message.magic != MAGIC
            || message.version != VERSION
            || message.length as usize > MAX_IO
            || (message.op == WRITE && returned < HEADER as u32 + message.length)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid native miniport response",
            ));
        }
        Ok(())
    }

    fn connect(&mut self, capacity: u64, unit_id: [u8; 16]) -> io::Result<()> {
        let mut message = Message::new(CONNECT, 0);
        message.capacity = capacity;
        message.unit_id = unit_id;
        self.ioctl(&mut message)?;
        if message.session == 0 {
            return Err(io::Error::other("miniport did not establish a session"));
        }
        self.session = message.session;
        Ok(())
    }
}

impl Drop for Adapter {
    fn drop(&mut self) {
        if self.session != 0 {
            let _ = self.ioctl(&mut Message::new(DISCONNECT, self.session));
        }
    }
}

fn find_adapter(explicit: Option<String>) -> io::Result<Adapter> {
    if let Some(path) = explicit {
        return Adapter::open(path);
    }
    for port in 0..64 {
        if let Ok(adapter) = Adapter::open(format!(r"\\.\Scsi{port}:")) {
            return Ok(adapter);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "native zcnblk adapter not found; install zcnblk.inf and run elevated",
    ))
}

pub fn run() -> io::Result<()> {
    let usage = "usage: zcnblk-windows-client --remote IP:PORT --capacity-mib N [--adapter \\\\.\\ScsiN:] | --list\nRequires the native zcnblk Storport driver. Kernel placement=none; transport and all storage policy remain userspace. Functional polling implementation, not a performance benchmark.";
    let mut args = std::env::args().skip(1);
    let mut remote = None;
    let mut capacity = None;
    let mut adapter_path = None;
    let mut list = false;
    let invalid = || io::Error::new(io::ErrorKind::InvalidInput, usage);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--remote" => remote = Some(args.next().ok_or_else(invalid)?),
            "--capacity-mib" => capacity = Some(args.next().ok_or_else(invalid)?),
            "--adapter" => adapter_path = Some(args.next().ok_or_else(invalid)?),
            "--list" => list = true,
            "--help" | "-h" => {
                println!("{usage}");
                return Ok(());
            }
            _ => return Err(invalid()),
        }
    }
    let mut adapter = find_adapter(adapter_path)?;
    if list {
        println!("native zcnblk adapter: {} ABI={VERSION}", adapter.path);
        return Ok(());
    }
    let remote = remote.ok_or_else(invalid)?;
    let address: SocketAddr = remote.parse().map_err(|_| invalid())?;
    let capacity = capacity
        .ok_or_else(invalid)?
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(1024 * 1024))
        .filter(|n| *n > 0)
        .ok_or_else(invalid)?;
    // The portable target can apply up to 20 seconds of WAL backpressure.
    // Allow it to complete before the miniport's 30-second owner lease.
    let mut transport = ZcnblkClient::connect(address, Duration::from_secs(25))?;
    let digest = Sha256::digest(format!("zcnblk-native:{remote}:{capacity}"));
    let unit_id: [u8; 16] = digest[..16].try_into().unwrap();
    adapter.connect(capacity, unit_id)?;
    unsafe {
        SetConsoleCtrlHandler(Some(control_handler), 1);
    }
    println!(
        "zcnblk-windows-client: attached adapter={} remote={remote} capacity_bytes={capacity} sector_bytes=4096 placement=userspace kernel_placement=no polling=true cpu_mapping=os-scheduled",
        adapter.path
    );
    let mut message = Message::new(EXCHANGE, adapter.session);
    while !STOP.load(Ordering::Acquire) {
        adapter.ioctl(&mut message)?;
        if message.request_id == 0 {
            thread::sleep(Duration::from_millis(1));
            continue;
        }
        let len = message.length as usize;
        if (message.op != SYNC && (len == 0 || len % 4096 != 0 || message.offset % 4096 != 0))
            || message
                .offset
                .checked_add(len as u64)
                .is_none_or(|end| end > capacity)
            || message.flags & !FUA != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "miniport request is outside the declared volume contract",
            ));
        }
        let result = match message.op {
            READ => transport.read_at(message.offset, &mut message.data[..len]),
            WRITE => transport
                .write_at(message.offset, &message.data[..len])
                .and_then(|()| {
                    if message.flags & FUA != 0 {
                        transport.sync()
                    } else {
                        Ok(())
                    }
                }),
            SYNC if len == 0 => transport.sync(),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported native block operation",
            )),
        };
        message.status = u32::from(result.is_err());
        if let Err(error) = result {
            let _ = adapter.ioctl(&mut message);
            return Err(error);
        }
    }
    println!("zcnblk-windows-client: disconnecting");
    Ok(())
}
