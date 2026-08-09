use std::io;

pub(crate) const ZCNBLK_WAL_FEATURE_FUA: u32 = 1 << 0;
pub(crate) const ZCNBLK_WAL_FEATURE_POLLED_COMPLETION: u32 = 1 << 1;
pub(crate) const ZCNBLK_WAL_FEATURE_BATCH_SUBMISSION: u32 = 1 << 2;
pub(crate) const ZCNBLK_WAL_FEATURE_IO_PRIORITY: u32 = 1 << 3;
pub(crate) const ZCNBLK_WAL_FEATURE_REGISTERED_LEASE: u32 = 1 << 4;
pub(crate) const ZCNBLK_WAL_FEATURE_ATOMIC_WRITE: u32 = 1 << 5;
pub(crate) const ZCNBLK_WAL_FEATURE_WRITE_LIFETIME: u32 = 1 << 6;
pub(crate) const ZCNBLK_WAL_FEATURE_ALL: u32 = ZCNBLK_WAL_FEATURE_FUA
    | ZCNBLK_WAL_FEATURE_POLLED_COMPLETION
    | ZCNBLK_WAL_FEATURE_BATCH_SUBMISSION
    | ZCNBLK_WAL_FEATURE_IO_PRIORITY
    | ZCNBLK_WAL_FEATURE_REGISTERED_LEASE
    | ZCNBLK_WAL_FEATURE_ATOMIC_WRITE
    | ZCNBLK_WAL_FEATURE_WRITE_LIFETIME;

const ZCNBLK_WAL_CONTRACT_VALID: u32 = 1 << 31;
const ZCNBLK_WAL_CONTRACT_FUA: u32 = 1 << 0;
const ZCNBLK_WAL_CONTRACT_POLLED_COMPLETION: u32 = 1 << 1;
const ZCNBLK_WAL_CONTRACT_REGISTERED_LEASE: u32 = 1 << 2;
const ZCNBLK_WAL_CONTRACT_ATOMIC_WRITE: u32 = 1 << 3;
const ZCNBLK_WAL_CONTRACT_FLAG_MASK: u32 = 0xff;
const ZCNBLK_WAL_CONTRACT_KNOWN_FLAGS: u32 = ZCNBLK_WAL_CONTRACT_FUA
    | ZCNBLK_WAL_CONTRACT_POLLED_COMPLETION
    | ZCNBLK_WAL_CONTRACT_REGISTERED_LEASE
    | ZCNBLK_WAL_CONTRACT_ATOMIC_WRITE;
const ZCNBLK_WAL_CONTRACT_IOPRIO_SHIFT: u32 = 8;
const ZCNBLK_WAL_CONTRACT_IOPRIO_MASK: u32 = 0xffff << ZCNBLK_WAL_CONTRACT_IOPRIO_SHIFT;
const ZCNBLK_WAL_CONTRACT_WRITE_LIFETIME_SHIFT: u32 = 24;
const ZCNBLK_WAL_CONTRACT_WRITE_LIFETIME_MASK: u32 =
    0x0f << ZCNBLK_WAL_CONTRACT_WRITE_LIFETIME_SHIFT;
const ZCNBLK_WAL_CONTRACT_RESERVED_MASK: u32 = 0x7000_0000;

pub(crate) const ZCNBLK_WAL_WRITE_LIFE_NOT_SET: u8 = 0;
pub(crate) const ZCNBLK_WAL_WRITE_LIFE_NONE: u8 = 1;
pub(crate) const ZCNBLK_WAL_WRITE_LIFE_SHORT: u8 = 2;
pub(crate) const ZCNBLK_WAL_WRITE_LIFE_MEDIUM: u8 = 3;
pub(crate) const ZCNBLK_WAL_WRITE_LIFE_LONG: u8 = 4;
pub(crate) const ZCNBLK_WAL_WRITE_LIFE_EXTREME: u8 = 5;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ZcnblkWalIoContract {
    pub(crate) fua: bool,
    pub(crate) polled_completion: bool,
    pub(crate) registered_lease: bool,
    pub(crate) atomic_write: bool,
    pub(crate) ioprio: u16,
    pub(crate) write_lifetime: u8,
    pub(crate) lease_id: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ZcnblkWalIoOperation {
    Read,
    Write,
}

pub(crate) fn zcnblk_wal_validate_features(features: u32) -> io::Result<u32> {
    let unknown = features & !ZCNBLK_WAL_FEATURE_ALL;
    if unknown != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("WAL I/O capability mask contains unknown bits {unknown:#x}"),
        ));
    }
    Ok(features)
}

impl ZcnblkWalIoContract {
    pub(crate) fn encode(self) -> io::Result<u32> {
        self.validate_common()?;
        let mut word = ZCNBLK_WAL_CONTRACT_VALID;
        if self.fua {
            word |= ZCNBLK_WAL_CONTRACT_FUA;
        }
        if self.polled_completion {
            word |= ZCNBLK_WAL_CONTRACT_POLLED_COMPLETION;
        }
        if self.registered_lease {
            word |= ZCNBLK_WAL_CONTRACT_REGISTERED_LEASE;
        }
        if self.atomic_write {
            word |= ZCNBLK_WAL_CONTRACT_ATOMIC_WRITE;
        }
        word |= u32::from(self.ioprio) << ZCNBLK_WAL_CONTRACT_IOPRIO_SHIFT;
        word |= u32::from(self.write_lifetime) << ZCNBLK_WAL_CONTRACT_WRITE_LIFETIME_SHIFT;
        Ok(word)
    }

    pub(crate) fn decode(word: u32, lease_id: u64) -> io::Result<Self> {
        if word == 0 {
            if lease_id != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "legacy WAL request carries a lease id without an I/O contract",
                ));
            }
            return Ok(Self::default());
        }
        if word & ZCNBLK_WAL_CONTRACT_VALID == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "WAL I/O contract is missing its valid bit",
            ));
        }
        let unknown_flags = word & ZCNBLK_WAL_CONTRACT_FLAG_MASK & !ZCNBLK_WAL_CONTRACT_KNOWN_FLAGS;
        if unknown_flags != 0 || word & ZCNBLK_WAL_CONTRACT_RESERVED_MASK != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "WAL I/O contract contains unknown bits {:#x}",
                    unknown_flags | (word & ZCNBLK_WAL_CONTRACT_RESERVED_MASK)
                ),
            ));
        }
        let contract = Self {
            fua: word & ZCNBLK_WAL_CONTRACT_FUA != 0,
            polled_completion: word & ZCNBLK_WAL_CONTRACT_POLLED_COMPLETION != 0,
            registered_lease: word & ZCNBLK_WAL_CONTRACT_REGISTERED_LEASE != 0,
            atomic_write: word & ZCNBLK_WAL_CONTRACT_ATOMIC_WRITE != 0,
            ioprio: ((word & ZCNBLK_WAL_CONTRACT_IOPRIO_MASK) >> ZCNBLK_WAL_CONTRACT_IOPRIO_SHIFT)
                as u16,
            write_lifetime: ((word & ZCNBLK_WAL_CONTRACT_WRITE_LIFETIME_MASK)
                >> ZCNBLK_WAL_CONTRACT_WRITE_LIFETIME_SHIFT) as u8,
            lease_id,
        };
        contract.validate_common()?;
        Ok(contract)
    }

    pub(crate) fn required_features(self, batched: bool) -> u32 {
        let mut features = 0;
        if self.fua {
            features |= ZCNBLK_WAL_FEATURE_FUA;
        }
        if self.polled_completion {
            features |= ZCNBLK_WAL_FEATURE_POLLED_COMPLETION;
        }
        if batched {
            features |= ZCNBLK_WAL_FEATURE_BATCH_SUBMISSION;
        }
        if self.ioprio != 0 {
            features |= ZCNBLK_WAL_FEATURE_IO_PRIORITY;
        }
        if self.registered_lease {
            features |= ZCNBLK_WAL_FEATURE_REGISTERED_LEASE;
        }
        if self.atomic_write {
            features |= ZCNBLK_WAL_FEATURE_ATOMIC_WRITE;
        }
        if self.write_lifetime != ZCNBLK_WAL_WRITE_LIFE_NOT_SET {
            features |= ZCNBLK_WAL_FEATURE_WRITE_LIFETIME;
        }
        features
    }

    pub(crate) fn validate_for_request(
        self,
        operation: ZcnblkWalIoOperation,
        logical_offset: u64,
        logical_len: u32,
        batched: bool,
        negotiated_features: u32,
    ) -> io::Result<()> {
        self.validate_common()?;
        zcnblk_wal_validate_features(negotiated_features)?;
        if operation != ZcnblkWalIoOperation::Write
            && (self.fua
                || self.atomic_write
                || self.write_lifetime != ZCNBLK_WAL_WRITE_LIFE_NOT_SET)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "FUA, atomic-write, and write-lifetime contracts are valid only for WAL writes",
            ));
        }
        if self.atomic_write {
            let len = u64::from(logical_len);
            if len == 0 || !len.is_power_of_two() || logical_offset % len != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "atomic WAL write must have power-of-two length and natural alignment: offset={logical_offset} len={logical_len}"
                    ),
                ));
            }
        }
        let required = self.required_features(batched);
        let missing = required & !negotiated_features;
        if missing != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("WAL request requires unnegotiated I/O features {missing:#x}"),
            ));
        }
        Ok(())
    }

    fn validate_common(self) -> io::Result<()> {
        if self.write_lifetime > ZCNBLK_WAL_WRITE_LIFE_EXTREME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "WAL write-lifetime hint {} exceeds {}",
                    self.write_lifetime, ZCNBLK_WAL_WRITE_LIFE_EXTREME
                ),
            ));
        }
        if self.registered_lease != (self.lease_id != 0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "WAL registered-lease flag and lease id must be present together",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_seven_features_are_distinct_and_known() {
        assert_eq!(ZCNBLK_WAL_FEATURE_ALL.count_ones(), 7);
        assert_eq!(
            zcnblk_wal_validate_features(ZCNBLK_WAL_FEATURE_ALL).unwrap(),
            0x7f
        );
    }

    #[test]
    fn io_contract_round_trips_all_per_request_features() {
        let expected = ZcnblkWalIoContract {
            fua: true,
            polled_completion: true,
            registered_lease: true,
            atomic_write: true,
            ioprio: 0x4123,
            write_lifetime: ZCNBLK_WAL_WRITE_LIFE_SHORT,
            lease_id: 0x1122_3344_5566_7788,
        };
        let word = expected.encode().unwrap();
        assert_eq!(
            ZcnblkWalIoContract::decode(word, expected.lease_id).unwrap(),
            expected
        );
        assert_eq!(expected.required_features(true), ZCNBLK_WAL_FEATURE_ALL);
    }

    #[test]
    fn atomic_contract_enforces_shape_and_capabilities() {
        let contract = ZcnblkWalIoContract {
            atomic_write: true,
            ..ZcnblkWalIoContract::default()
        };
        contract
            .validate_for_request(
                ZcnblkWalIoOperation::Write,
                8192,
                4096,
                false,
                ZCNBLK_WAL_FEATURE_ATOMIC_WRITE,
            )
            .unwrap();
        assert!(
            contract
                .validate_for_request(
                    ZcnblkWalIoOperation::Write,
                    4096,
                    8192,
                    false,
                    ZCNBLK_WAL_FEATURE_ATOMIC_WRITE,
                )
                .is_err()
        );
        assert!(
            contract
                .validate_for_request(ZcnblkWalIoOperation::Write, 8192, 4096, false, 0,)
                .is_err()
        );
    }

    #[test]
    fn lease_contract_fails_closed() {
        assert!(
            ZcnblkWalIoContract {
                registered_lease: true,
                lease_id: 0,
                ..ZcnblkWalIoContract::default()
            }
            .encode()
            .is_err()
        );
        assert!(ZcnblkWalIoContract::decode(0, 99).is_err());
    }
}
