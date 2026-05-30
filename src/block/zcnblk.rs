use std::io;

const ZCNBLK_FRAME_MAGIC: &[u8; 8] = b"ZCNBLK01";
const ZCNBLK_FRAME_VERSION: u16 = 2;

pub(crate) const ZCNBLK_FRAME_HEADER_LEN: usize = 64;
pub(crate) const ZCNBLK_TOPOLOGY_VALID: u32 = 1 << 0;
pub(crate) const ZCNBLK_TOPOLOGY_PORT_LANE: u32 = 1 << 1;
pub(crate) const ZCNBLK_OP_WRITE: u16 = 1;
pub(crate) const ZCNBLK_OP_READ: u16 = 2;
pub(crate) const ZCNBLK_OP_READ_RESP: u16 = 3;
pub(crate) const ZCNBLK_OP_WRITE_ACK: u16 = 4;
pub(crate) const ZCNBLK_OP_BATCH: u16 = 5;
pub(crate) const ZCNBLK_OP_BATCH_RESP: u16 = 6;

#[derive(Clone, Copy, Default)]
pub(crate) struct ZcnblkFrameTopology {
    pub(crate) lane_id: u32,
    pub(crate) lane_count: u32,
    pub(crate) preferred_worker: u32,
    pub(crate) queue_id: u32,
    pub(crate) request_id: u64,
    pub(crate) tier_id: u32,
    pub(crate) topology_flags: u32,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct ZcnblkFrameHeader {
    pub(crate) op: u16,
    pub(crate) flags: u16,
    pub(crate) shard: u32,
    pub(crate) len: u32,
    pub(crate) offset: u64,
    pub(crate) topology: ZcnblkFrameTopology,
}

impl ZcnblkFrameHeader {
    pub(crate) fn with_flags(
        op: u16,
        flags: u16,
        shard: usize,
        len: usize,
        offset: u64,
    ) -> io::Result<Self> {
        Self::with_topology(
            op,
            flags,
            shard,
            len,
            offset,
            ZcnblkFrameTopology::default(),
        )
    }

    pub(crate) fn with_topology(
        op: u16,
        flags: u16,
        shard: usize,
        len: usize,
        offset: u64,
        topology: ZcnblkFrameTopology,
    ) -> io::Result<Self> {
        if !matches!(
            op,
            ZCNBLK_OP_WRITE
                | ZCNBLK_OP_READ
                | ZCNBLK_OP_READ_RESP
                | ZCNBLK_OP_WRITE_ACK
                | ZCNBLK_OP_BATCH
                | ZCNBLK_OP_BATCH_RESP
        ) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unsupported zcnblk op {op}"),
            ));
        }
        Ok(Self {
            op,
            flags,
            shard: u32::try_from(shard).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "zcnblk shard index exceeds u32",
                )
            })?,
            len: u32::try_from(len).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "zcnblk frame length exceeds u32",
                )
            })?,
            offset,
            topology,
        })
    }

    pub(crate) fn topology_valid(self) -> bool {
        self.topology.topology_flags & ZCNBLK_TOPOLOGY_VALID != 0
    }

    pub(crate) fn encode(self) -> [u8; ZCNBLK_FRAME_HEADER_LEN] {
        let mut buf = [0u8; ZCNBLK_FRAME_HEADER_LEN];
        buf[0..8].copy_from_slice(ZCNBLK_FRAME_MAGIC);
        buf[8..10].copy_from_slice(&ZCNBLK_FRAME_VERSION.to_le_bytes());
        buf[10..12].copy_from_slice(&(ZCNBLK_FRAME_HEADER_LEN as u16).to_le_bytes());
        buf[12..14].copy_from_slice(&self.op.to_le_bytes());
        buf[14..16].copy_from_slice(&self.flags.to_le_bytes());
        buf[16..20].copy_from_slice(&self.shard.to_le_bytes());
        buf[20..24].copy_from_slice(&self.len.to_le_bytes());
        buf[24..32].copy_from_slice(&self.offset.to_le_bytes());
        buf[32..36].copy_from_slice(&self.topology.lane_id.to_le_bytes());
        buf[36..40].copy_from_slice(&self.topology.lane_count.to_le_bytes());
        buf[40..44].copy_from_slice(&self.topology.preferred_worker.to_le_bytes());
        buf[44..48].copy_from_slice(&self.topology.queue_id.to_le_bytes());
        buf[48..56].copy_from_slice(&self.topology.request_id.to_le_bytes());
        buf[56..60].copy_from_slice(&self.topology.tier_id.to_le_bytes());
        buf[60..64].copy_from_slice(&self.topology.topology_flags.to_le_bytes());
        buf
    }

    pub(crate) fn decode(buf: &[u8; ZCNBLK_FRAME_HEADER_LEN]) -> io::Result<Self> {
        if &buf[0..8] != ZCNBLK_FRAME_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "zcnblk frame magic mismatch",
            ));
        }
        let version = u16::from_le_bytes(buf[8..10].try_into().expect("u16"));
        let header_len = u16::from_le_bytes(buf[10..12].try_into().expect("u16")) as usize;
        let op = u16::from_le_bytes(buf[12..14].try_into().expect("u16"));
        if version != ZCNBLK_FRAME_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported zcnblk frame version {version}"),
            ));
        }
        if header_len != ZCNBLK_FRAME_HEADER_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported zcnblk frame header length {header_len}"),
            ));
        }
        if !matches!(
            op,
            ZCNBLK_OP_WRITE
                | ZCNBLK_OP_READ
                | ZCNBLK_OP_READ_RESP
                | ZCNBLK_OP_WRITE_ACK
                | ZCNBLK_OP_BATCH
                | ZCNBLK_OP_BATCH_RESP
        ) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported zcnblk frame op {op}"),
            ));
        }
        Ok(Self {
            op,
            flags: u16::from_le_bytes(buf[14..16].try_into().expect("u16")),
            shard: u32::from_le_bytes(buf[16..20].try_into().expect("u32")),
            len: u32::from_le_bytes(buf[20..24].try_into().expect("u32")),
            offset: u64::from_le_bytes(buf[24..32].try_into().expect("u64")),
            topology: ZcnblkFrameTopology {
                lane_id: u32::from_le_bytes(buf[32..36].try_into().expect("u32")),
                lane_count: u32::from_le_bytes(buf[36..40].try_into().expect("u32")),
                preferred_worker: u32::from_le_bytes(buf[40..44].try_into().expect("u32")),
                queue_id: u32::from_le_bytes(buf[44..48].try_into().expect("u32")),
                request_id: u64::from_le_bytes(buf[48..56].try_into().expect("u64")),
                tier_id: u32::from_le_bytes(buf[56..60].try_into().expect("u32")),
                topology_flags: u32::from_le_bytes(buf[60..64].try_into().expect("u32")),
            },
        })
    }
}
