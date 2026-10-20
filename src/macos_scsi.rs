//! Placement-free SCSI disk personality used by the macOS DriverKit broker.
//! All storage access goes through the existing userspace zcnblk client.
use std::io;
pub const BLOCK: usize = 4096;
pub trait Transport {
    fn read(&mut self, offset: u64, data: &mut [u8]) -> io::Result<()>;
    fn write(&mut self, offset: u64, data: &[u8]) -> io::Result<()>;
    fn sync(&mut self) -> io::Result<()>;
}
#[derive(Debug, PartialEq)]
pub struct Completion {
    pub bytes: usize,
    pub key: u8,
    pub asc: u8,
}
impl Completion {
    fn good(bytes: usize) -> Self {
        Self {
            bytes,
            key: 0,
            asc: 0,
        }
    }
    fn error(key: u8, asc: u8) -> Self {
        Self { bytes: 0, key, asc }
    }
}
pub struct Disk<T> {
    pub transport: T,
    blocks: u64,
}
impl<T: Transport> Disk<T> {
    pub fn new(transport: T, bytes: u64) -> io::Result<Self> {
        if bytes < BLOCK as u64 || bytes % BLOCK as u64 != 0 {
            return Err(io::Error::other(
                "capacity must be a positive multiple of 4096",
            ));
        }
        Ok(Self {
            transport,
            blocks: bytes / BLOCK as u64,
        })
    }
    // direction: 0=no data, 1=from disk, 2=to disk. One page per task.
    pub fn execute(&mut self, cdb: &[u8], direction: u8, data: &mut [u8]) -> Completion {
        let Some(&op) = cdb.first() else {
            return Completion::error(5, 0x24);
        };
        let required = match op {
            0x00 | 0x03 | 0x12 | 0x1a | 0x1b | 0x1e => 6,
            0x25 | 0x28 | 0x2a | 0x35 | 0x5a => 10,
            0x88 | 0x8a | 0x91 | 0x9e => 16,
            0xa0 => 12,
            _ => return Completion::error(5, 0x20),
        };
        if cdb.len() != required || data.len() > BLOCK {
            return Completion::error(5, 0x24);
        }
        if matches!(op, 0x28 | 0x2a | 0x88 | 0x8a) {
            let read = op == 0x28 || op == 0x88;
            let (lba, count) = if required == 10 {
                (be(&cdb[2..6]), be(&cdb[7..9]))
            } else {
                (be(&cdb[2..10]), be(&cdb[10..14]))
            };
            if cdb[1] & !0x18 != 0
                || direction != if read { 1 } else { 2 }
                || count > 1
                || data.len() != count as usize * BLOCK
            {
                return Completion::error(5, 0x24);
            }
            if lba >= self.blocks || count > self.blocks - lba {
                return Completion::error(5, 0x21);
            }
            if count == 0 {
                return Completion::good(0);
            }
            let offset = lba * BLOCK as u64;
            let result = if read {
                self.transport.read(offset, data)
            } else {
                self.transport.write(offset, data).and_then(|()| {
                    if cdb[1] & 8 != 0 {
                        self.transport.sync()
                    } else {
                        Ok(())
                    }
                })
            };
            return if result.is_ok() {
                Completion::good(data.len())
            } else {
                Completion::error(4, 0x44)
            };
        }
        if matches!(op, 0x35 | 0x91 | 0x1b) {
            // IMMED is not advertised: never acknowledge a drain before completion.
            if cdb[1] != 0 || !data.is_empty() || direction != 0 {
                return Completion::error(5, 0x24);
            }
            return if self.transport.sync().is_ok() {
                Completion::good(0)
            } else {
                Completion::error(4, 0x44)
            };
        }
        if matches!(op, 0x00 | 0x1e) {
            return if data.is_empty() && direction == 0 {
                Completion::good(0)
            } else {
                Completion::error(5, 0x24)
            };
        }
        if direction != 1 {
            return Completion::error(5, 0x24);
        }
        let mut reply = [0u8; 256];
        let (len, allocation) = match op {
            0x03 => {
                if cdb[1] != 0 {
                    return Completion::error(5, 0x24);
                }
                reply[0] = 0x70;
                reply[7] = 10;
                (18, cdb[4] as usize)
            }
            0x12 => {
                if cdb[1] & !1 != 0 {
                    return Completion::error(5, 0x24);
                }
                if cdb[1] & 1 == 0 {
                    if cdb[2] != 0 {
                        return Completion::error(5, 0x24);
                    }
                    reply[2] = 6;
                    reply[3] = 2;
                    reply[4] = 31;
                    reply[8..16].copy_from_slice(b"ZCUTILS ");
                    reply[16..32].copy_from_slice(b"ZCNBLK NETWORK  ");
                    reply[32..36].copy_from_slice(b"0001");
                    (36, be(&cdb[3..5]) as usize)
                } else {
                    reply[1] = cdb[2];
                    let n = match cdb[2] {
                        0 => {
                            reply[3] = 3;
                            reply[4..7].copy_from_slice(&[0, 0x80, 0xb0]);
                            7
                        }
                        0x80 => {
                            reply[3] = 16;
                            reply[4..20].copy_from_slice(b"zcutils-dev-0001");
                            20
                        }
                        0xb0 => {
                            reply[3] = 0x3c;
                            reply[8..12].copy_from_slice(&1u32.to_be_bytes());
                            64
                        }
                        _ => return Completion::error(5, 0x24),
                    };
                    (n, be(&cdb[3..5]) as usize)
                }
            }
            0x25 => {
                reply[..4].copy_from_slice(
                    &((self.blocks - 1).min(u32::MAX as u64) as u32).to_be_bytes(),
                );
                reply[4..8].copy_from_slice(&(BLOCK as u32).to_be_bytes());
                (8, 8)
            }
            0x9e => {
                if cdb[1] != 0x10 {
                    return Completion::error(5, 0x24);
                }
                reply[..8].copy_from_slice(&(self.blocks - 1).to_be_bytes());
                reply[8..12].copy_from_slice(&(BLOCK as u32).to_be_bytes());
                (32, be(&cdb[10..14]) as usize)
            }
            0xa0 => {
                reply[3] = 8;
                (16, be(&cdb[6..10]) as usize)
            }
            0x1a | 0x5a => {
                if cdb[2] & 0xc0 != 0 || !matches!(cdb[2] & 0x3f, 8 | 0x3f) || cdb[3] != 0 {
                    return Completion::error(5, 0x24);
                }
                let header = if op == 0x1a { 4 } else { 8 };
                // Caching page: WCE=1, and FUA maps to a remote SYNC.
                reply[header] = 8;
                reply[header + 1] = 18;
                reply[header + 2] = 4;
                let len = header + 20;
                if op == 0x1a {
                    reply[0] = (len - 1) as u8;
                    reply[2] = 0x10;
                } else {
                    reply[1] = (len - 2) as u8;
                    reply[3] = 0x10;
                }
                (
                    len,
                    if op == 0x1a {
                        cdb[4] as usize
                    } else {
                        be(&cdb[7..9]) as usize
                    },
                )
            }
            _ => unreachable!(),
        };
        let n = len.min(allocation).min(data.len());
        data[..n].copy_from_slice(&reply[..n]);
        Completion::good(n)
    }
}
fn be(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0, |n, b| (n << 8) | *b as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Memory {
        data: Vec<u8>,
        operations: Vec<&'static str>,
        fail_sync: bool,
    }
    impl Transport for Memory {
        fn read(&mut self, o: u64, d: &mut [u8]) -> io::Result<()> {
            self.operations.push("read");
            d.copy_from_slice(&self.data[o as usize..o as usize + d.len()]);
            Ok(())
        }
        fn write(&mut self, o: u64, d: &[u8]) -> io::Result<()> {
            self.operations.push("write");
            self.data[o as usize..o as usize + d.len()].copy_from_slice(d);
            Ok(())
        }
        fn sync(&mut self) -> io::Result<()> {
            self.operations.push("sync");
            if self.fail_sync {
                Err(io::Error::other("failed drain"))
            } else {
                Ok(())
            }
        }
    }
    fn disk() -> Disk<Memory> {
        Disk::new(
            Memory {
                data: vec![0; 8192],
                ..Default::default()
            },
            8192,
        )
        .unwrap()
    }
    #[test]
    fn fua_and_flush_are_remote_drains() {
        let mut d = disk();
        let mut bytes = [0xa5; BLOCK];
        let write = [0x2a, 8, 0, 0, 0, 1, 0, 0, 1, 0];
        assert_eq!(d.execute(&write, 2, &mut bytes), Completion::good(BLOCK));
        assert_eq!(d.transport.operations, vec!["write", "sync"]);
        bytes.fill(0);
        assert_eq!(
            d.execute(&[0x28, 0, 0, 0, 0, 1, 0, 0, 1, 0], 1, &mut bytes),
            Completion::good(BLOCK)
        );
        assert_eq!(bytes, [0xa5; BLOCK]);
        d.transport.fail_sync = true;
        assert_eq!(d.execute(&write, 2, &mut bytes).key, 4);
        assert_eq!(
            d.execute(&[0x35, 0, 0, 0, 0, 0, 0, 0, 0, 0], 0, &mut [])
                .key,
            4
        );
    }
    #[test]
    fn invalid_io_never_reaches_transport() {
        let mut d = disk();
        let mut b = [0; BLOCK];
        for c in [
            [0x2a, 0, 0, 0, 0, 2, 0, 0, 1, 0],
            [0x2a, 0, 0, 0, 0, 0, 0, 0, 2, 0],
        ] {
            assert_ne!(d.execute(&c, 2, &mut b).key, 0);
        }
        assert_ne!(
            d.execute(&[0x2a, 0, 0, 0, 0, 0, 0, 0, 1, 0], 1, &mut b).key,
            0
        );
        assert!(d.transport.operations.is_empty());
    }
    #[test]
    fn discovery_capacity_and_allocation_bounds() {
        let mut d = disk();
        let mut b = [0xcc; 64];
        assert_eq!(d.execute(&[0x12, 0, 0, 0, 6, 0], 1, &mut b).bytes, 6);
        assert_eq!(b[6], 0xcc);
        assert_eq!(
            d.execute(&[0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0], 1, &mut b)
                .bytes,
            8
        );
        assert_eq!(&b[..8], &[0, 0, 0, 1, 0, 0, 0x10, 0]);
        assert_eq!(d.execute(&[0xff; 6], 1, &mut b).asc, 0x20);
        assert!(Disk::new(Memory::default(), 8193).is_err());
        for page in [0, 0x80, 0xb0] {
            assert_eq!(d.execute(&[0x12, 1, page, 0, 64, 0], 1, &mut b).key, 0);
        }
        for op in [0x1a, 0x5a] {
            let mut cdb = vec![0; if op == 0x1a { 6 } else { 10 }];
            cdb[0] = op;
            cdb[2] = 8;
            if op == 0x1a {
                cdb[4] = 64;
            } else {
                cdb[8] = 64;
            }
            assert_eq!(d.execute(&cdb, 1, &mut b).key, 0);
        }
    }
}
