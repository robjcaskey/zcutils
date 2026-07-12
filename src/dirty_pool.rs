use std::collections::BTreeMap;

pub const ZC_DIRTY_NO_SEQUENCE: u64 = u64::MAX;
pub const ZC_DIRTY_NO_POOL: u32 = u32::MAX;
pub const ZC_DIRTY_NO_SLOT: u64 = u64::MAX;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ZcDirtyRecordRef {
    pub sequence: u64,
    pub pool_id: u32,
    pub slot: u64,
    pub byte_offset: u64,
    pub byte_len: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ZcDirtyExtentRef {
    pub logical_start: u64,
    pub record_count: u32,
    pub sequence: u64,
    pub pool_id: u32,
    pub slot: u64,
    pub byte_offset: u64,
    pub byte_len: u64,
}

impl ZcDirtyRecordRef {
    pub const NONE: Self = Self {
        sequence: ZC_DIRTY_NO_SEQUENCE,
        pool_id: ZC_DIRTY_NO_POOL,
        slot: ZC_DIRTY_NO_SLOT,
        byte_offset: 0,
        byte_len: 0,
    };

    pub fn new(sequence: u64, pool_id: u32, slot: u64, byte_offset: u64, byte_len: u32) -> Self {
        Self {
            sequence,
            pool_id,
            slot,
            byte_offset,
            byte_len,
        }
    }

    pub fn is_present(self) -> bool {
        self.sequence != ZC_DIRTY_NO_SEQUENCE
    }

    pub fn descriptor_token(self) -> u64 {
        self.sequence
            ^ ((self.pool_id as u64) << 48)
            ^ self.slot.rotate_left(17)
            ^ self.byte_offset.rotate_left(7)
            ^ self.byte_len as u64
    }
}

impl ZcDirtyExtentRef {
    pub fn new(
        logical_start: u64,
        record_count: u32,
        sequence: u64,
        pool_id: u32,
        slot: u64,
        byte_offset: u64,
        byte_len: u64,
    ) -> Self {
        Self {
            logical_start,
            record_count,
            sequence,
            pool_id,
            slot,
            byte_offset,
            byte_len,
        }
    }

    pub fn logical_end(self) -> Option<u64> {
        self.logical_start.checked_add(self.record_count as u64)
    }

    pub fn covers(self, logical_record: u64, records: u32) -> bool {
        logical_record >= self.logical_start
            && logical_record
                .checked_add(records as u64)
                .zip(self.logical_end())
                .is_some_and(|(want_end, extent_end)| want_end <= extent_end)
    }

    pub fn descriptor_token(self) -> u64 {
        self.sequence
            ^ ((self.pool_id as u64) << 48)
            ^ self.slot.rotate_left(17)
            ^ self.byte_offset.rotate_left(7)
            ^ self.byte_len.rotate_left(29)
            ^ (self.record_count as u64).rotate_left(43)
    }
}

#[derive(Clone, Debug)]
pub struct ZcDirtyLatestMap {
    records: Vec<ZcDirtyRecordRef>,
}

#[derive(Clone, Debug, Default)]
pub struct ZcDirtyExtentMap {
    extents: BTreeMap<u64, Vec<ZcDirtyExtentRef>>,
    present_records: usize,
}

impl ZcDirtyLatestMap {
    pub fn new(record_count: usize) -> Self {
        Self {
            records: vec![ZcDirtyRecordRef::NONE; record_count],
        }
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn admit(
        &mut self,
        logical_record: usize,
        desc: ZcDirtyRecordRef,
    ) -> Option<ZcDirtyRecordRef> {
        let entry = self.records.get_mut(logical_record)?;
        if !entry.is_present() || entry.sequence <= desc.sequence {
            let old = *entry;
            *entry = desc;
            old.is_present().then_some(old)
        } else {
            None
        }
    }

    pub fn get(&self, logical_record: usize) -> Option<ZcDirtyRecordRef> {
        self.records
            .get(logical_record)
            .copied()
            .filter(|desc| desc.is_present())
    }

    pub fn clear_if_current(&mut self, logical_record: usize, sequence: u64) -> bool {
        let Some(entry) = self.records.get_mut(logical_record) else {
            return false;
        };
        if entry.sequence == sequence {
            *entry = ZcDirtyRecordRef::NONE;
            true
        } else {
            false
        }
    }

    pub fn clear_through_sequence(&mut self, sequence: u64) -> usize {
        let mut cleared = 0usize;
        for entry in &mut self.records {
            if entry.is_present() && entry.sequence <= sequence {
                *entry = ZcDirtyRecordRef::NONE;
                cleared = cleared.saturating_add(1);
            }
        }
        cleared
    }

    pub fn present_records(&self) -> usize {
        self.records.iter().filter(|desc| desc.is_present()).count()
    }
}

impl ZcDirtyExtentMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn admit(&mut self, desc: ZcDirtyExtentRef) -> Option<ZcDirtyExtentRef> {
        let inserted_records = desc.record_count as usize;
        let entry = self.extents.entry(desc.logical_start).or_default();
        let replace_idx = entry
            .iter()
            .position(|existing| existing.sequence <= desc.sequence);
        let old = if let Some(idx) = replace_idx {
            Some(std::mem::replace(&mut entry[idx], desc))
        } else {
            entry.push(desc);
            self.present_records = self.present_records.saturating_add(inserted_records);
            None
        };
        entry.sort_by_key(|extent| extent.sequence);
        old
    }

    pub fn get_covering(&self, logical_record: u64, records: u32) -> Option<ZcDirtyExtentRef> {
        let mut best = None::<ZcDirtyExtentRef>;
        for (_start, candidates) in self.extents.range(..=logical_record).rev() {
            for candidate in candidates.iter().rev().copied() {
                if candidate.covers(logical_record, records)
                    && best.is_none_or(|best| candidate.sequence >= best.sequence)
                {
                    best = Some(candidate);
                }
            }
            if best.is_some() {
                break;
            }
        }
        best
    }

    pub fn clear_if_current(
        &mut self,
        logical_start: u64,
        record_count: u32,
        sequence: u64,
    ) -> bool {
        let mut changed = false;
        let Some(commit_end) = logical_start.checked_add(record_count as u64) else {
            return false;
        };
        let keys = self.extents.keys().copied().collect::<Vec<_>>();
        for key in keys {
            let Some(candidates) = self.extents.remove(&key) else {
                continue;
            };
            let mut kept = Vec::with_capacity(candidates.len() + 1);
            for extent in candidates {
                if extent.sequence != sequence {
                    kept.push(extent);
                    continue;
                }
                let Some(extent_end) = extent.logical_end() else {
                    kept.push(extent);
                    continue;
                };
                let overlap_start = extent.logical_start.max(logical_start);
                let overlap_end = extent_end.min(commit_end);
                if overlap_start >= overlap_end {
                    kept.push(extent);
                    continue;
                }
                let removed_records = usize::try_from(overlap_end - overlap_start).unwrap_or(0);
                self.present_records = self.present_records.saturating_sub(removed_records);
                changed = true;
                if extent.logical_start < overlap_start {
                    let left_records = u32::try_from(overlap_start - extent.logical_start).ok();
                    if let Some(left_records) = left_records {
                        kept.push(ZcDirtyExtentRef {
                            record_count: left_records,
                            byte_len: extent.byte_len.saturating_mul(left_records as u64)
                                / extent.record_count.max(1) as u64,
                            ..extent
                        });
                    }
                }
                if overlap_end < extent_end {
                    let right_records = u32::try_from(extent_end - overlap_end).ok();
                    let skipped_records = overlap_end.saturating_sub(extent.logical_start);
                    if let Some(right_records) = right_records {
                        let bytes_per_record = extent.byte_len / extent.record_count.max(1) as u64;
                        kept.push(ZcDirtyExtentRef {
                            logical_start: overlap_end,
                            record_count: right_records,
                            byte_offset: extent
                                .byte_offset
                                .saturating_add(bytes_per_record.saturating_mul(skipped_records)),
                            byte_len: bytes_per_record.saturating_mul(right_records as u64),
                            ..extent
                        });
                    }
                }
            }
            if !kept.is_empty() {
                kept.sort_by_key(|extent| extent.sequence);
                self.extents.insert(key, kept);
            }
        }
        changed
    }

    pub fn clear_through_sequence(&mut self, sequence: u64) -> usize {
        let mut cleared = 0usize;
        self.extents.retain(|_, candidates| {
            let before = candidates.len();
            candidates.retain(|extent| extent.sequence > sequence);
            cleared = cleared.saturating_add(before.saturating_sub(candidates.len()));
            !candidates.is_empty()
        });
        self.present_records = self.extents.values().fold(0usize, |total, candidates| {
            candidates.iter().fold(total, |total, extent| {
                total.saturating_add(extent.record_count as usize)
            })
        });
        cleared
    }

    pub fn present_records(&self) -> usize {
        self.present_records
    }

    pub fn present_extents(&self) -> usize {
        self.extents.values().map(Vec::len).sum()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ZcDirtyHwmTracker {
    replica_hwm: u64,
    reduce_hwm: u64,
    free_hwm: u64,
    pins: BTreeMap<u64, usize>,
}

impl ZcDirtyHwmTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn replica_hwm(&self) -> u64 {
        self.replica_hwm
    }

    pub fn reduce_hwm(&self) -> u64 {
        self.reduce_hwm
    }

    pub fn free_hwm(&self) -> u64 {
        self.free_hwm
    }

    pub fn advance_replica_hwm(&mut self, hwm: u64) -> u64 {
        self.replica_hwm = self.replica_hwm.max(hwm);
        self.refresh_free_hwm()
    }

    pub fn advance_reduce_hwm(&mut self, hwm: u64) -> u64 {
        self.reduce_hwm = self.reduce_hwm.max(hwm);
        self.refresh_free_hwm()
    }

    pub fn pin_from(&mut self, hwm: u64) -> u64 {
        *self.pins.entry(hwm).or_insert(0) += 1;
        self.refresh_free_hwm()
    }

    pub fn unpin_from(&mut self, hwm: u64) -> u64 {
        if let Some(count) = self.pins.get_mut(&hwm) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.pins.remove(&hwm);
            }
        }
        self.refresh_free_hwm()
    }

    pub fn releasable_hwm(&self) -> u64 {
        let owner_hwm = self.replica_hwm.min(self.reduce_hwm);
        self.pins
            .keys()
            .next()
            .copied()
            .map_or(owner_hwm, |pin| owner_hwm.min(pin))
    }

    pub fn refresh_free_hwm(&mut self) -> u64 {
        self.free_hwm = self.free_hwm.max(self.releasable_hwm());
        self.free_hwm
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_map_preserves_newer_overwrites() {
        let mut map = ZcDirtyLatestMap::new(4);
        let old = ZcDirtyRecordRef::new(10, 1, 2, 0, 4096);
        let new = ZcDirtyRecordRef::new(11, 1, 3, 4096, 4096);

        assert_eq!(map.admit(2, old), None);
        assert_eq!(map.admit(2, new), Some(old));
        assert_eq!(map.get(2), Some(new));
        assert!(!map.clear_if_current(2, old.sequence));
        assert_eq!(map.get(2), Some(new));
        assert!(map.clear_if_current(2, new.sequence));
        assert_eq!(map.get(2), None);
    }

    #[test]
    fn hwm_tracker_waits_for_replica_reduce_and_pins() {
        let mut hwm = ZcDirtyHwmTracker::new();
        hwm.advance_replica_hwm(8);
        assert_eq!(hwm.free_hwm(), 0);
        hwm.advance_reduce_hwm(6);
        assert_eq!(hwm.free_hwm(), 6);
        hwm.pin_from(4);
        hwm.advance_replica_hwm(16);
        hwm.advance_reduce_hwm(16);
        assert_eq!(hwm.free_hwm(), 6);
        hwm.unpin_from(4);
        assert_eq!(hwm.free_hwm(), 16);
    }

    #[test]
    fn extent_map_returns_latest_covering_extent() {
        let mut map = ZcDirtyExtentMap::new();
        let old = ZcDirtyExtentRef::new(8, 8, 3, 1, 10, 4096, 8 * 4096);
        let new = ZcDirtyExtentRef::new(8, 8, 4, 1, 11, 4096, 8 * 4096);
        map.admit(old);
        map.admit(new);

        assert_eq!(map.get_covering(10, 2), Some(new));
        assert_eq!(map.present_records(), 8);
        assert_eq!(map.present_extents(), 1);
    }

    #[test]
    fn extent_map_splits_current_commits() {
        let mut map = ZcDirtyExtentMap::new();
        let extent = ZcDirtyExtentRef::new(8, 8, 7, 1, 10, 0, 8 * 4096);
        map.admit(extent);

        assert!(map.clear_if_current(10, 2, 7));
        assert_eq!(map.present_records(), 6);
        assert!(map.get_covering(8, 2).is_some());
        assert!(map.get_covering(12, 4).is_some());
        assert!(map.get_covering(10, 1).is_none());
    }
}
