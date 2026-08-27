//! Lane-local look-aside cache fencing for live placement changes.
//!
//! A control task publishes an ownership epoch plus the destination applied
//! HWM. Each lane observes that mailbox only at a batch boundary. Clean reads
//! belong to one placement epoch and are discarded on an epoch change. Dirty
//! payload references are logical write overlays; they survive the move until
//! the destination applied HWM covers their sequence.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteHwmSnapshot {
    pub generation: u64,
    pub placement_epoch: u64,
    pub applied_hwm: u64,
    pub effective_ns: u64,
}

#[repr(align(64))]
pub struct RouteHwmMailbox {
    sequence: AtomicU64,
    generation: AtomicU64,
    placement_epoch: AtomicU64,
    applied_hwm: AtomicU64,
    effective_ns: AtomicU64,
}

impl RouteHwmMailbox {
    pub fn new(initial: RouteHwmSnapshot) -> io::Result<Self> {
        validate_snapshot(initial)?;
        Ok(Self {
            sequence: AtomicU64::new(2),
            generation: AtomicU64::new(initial.generation),
            placement_epoch: AtomicU64::new(initial.placement_epoch),
            applied_hwm: AtomicU64::new(initial.applied_hwm),
            effective_ns: AtomicU64::new(initial.effective_ns),
        })
    }

    /// Publish a normal migration/custody cut. Both generation and placement
    /// epoch advance, and a clean move never rolls back the applied HWM.
    pub fn publish_clean_cutover(&self, next: RouteHwmSnapshot) -> io::Result<()> {
        let current = self.load_raw();
        validate_snapshot(next)?;
        if next.generation <= current.generation
            || next.placement_epoch <= current.placement_epoch
            || next.applied_hwm < current.applied_hwm
            || next.effective_ns < current.effective_ns
        {
            return Err(invalid(
                "clean cutover must advance generation/epoch/time without reducing applied HWM",
            ));
        }
        self.publish(next);
        Ok(())
    }

    /// Advance only the applied HWM within one ownership epoch.
    pub fn publish_hwm(&self, next: RouteHwmSnapshot) -> io::Result<()> {
        let current = self.load_raw();
        validate_snapshot(next)?;
        if next.generation <= current.generation
            || next.placement_epoch != current.placement_epoch
            || next.applied_hwm < current.applied_hwm
            || next.effective_ns < current.effective_ns
        {
            return Err(invalid(
                "HWM publication must advance generation/HWM in the current placement epoch",
            ));
        }
        self.publish(next);
        Ok(())
    }

    /// A declared-loss failover may intentionally select a lower HWM. It must
    /// still advance the epoch and explicitly name the accepted durable cut.
    pub fn publish_declared_loss(&self, next: RouteHwmSnapshot) -> io::Result<()> {
        let current = self.load_raw();
        validate_snapshot(next)?;
        if next.generation <= current.generation
            || next.placement_epoch <= current.placement_epoch
            || next.effective_ns < current.effective_ns
        {
            return Err(invalid(
                "declared-loss cutover must advance generation/epoch/time",
            ));
        }
        self.publish(next);
        Ok(())
    }

    fn publish(&self, next: RouteHwmSnapshot) {
        let odd = self.sequence.load(Ordering::Relaxed).wrapping_add(1) | 1;
        self.sequence.store(odd, Ordering::Release);
        self.generation.store(next.generation, Ordering::Relaxed);
        self.placement_epoch
            .store(next.placement_epoch, Ordering::Relaxed);
        self.applied_hwm.store(next.applied_hwm, Ordering::Relaxed);
        self.effective_ns
            .store(next.effective_ns, Ordering::Relaxed);
        self.sequence.store(odd.wrapping_add(1), Ordering::Release);
    }

    fn load_raw(&self) -> RouteHwmSnapshot {
        loop {
            let before = self.sequence.load(Ordering::Acquire);
            if before & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let value = RouteHwmSnapshot {
                generation: self.generation.load(Ordering::Relaxed),
                placement_epoch: self.placement_epoch.load(Ordering::Relaxed),
                applied_hwm: self.applied_hwm.load(Ordering::Relaxed),
                effective_ns: self.effective_ns.load(Ordering::Relaxed),
            };
            if before == self.sequence.load(Ordering::Acquire) {
                return value;
            }
        }
    }

    pub fn load_effective(&self, now_ns: u64) -> Option<RouteHwmSnapshot> {
        let value = self.load_raw();
        (now_ns >= value.effective_ns).then_some(value)
    }
}

fn validate_snapshot(snapshot: RouteHwmSnapshot) -> io::Result<()> {
    if snapshot.generation == 0 || snapshot.placement_epoch == 0 {
        return Err(invalid(
            "route HWM generation and placement epoch must be nonzero",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct CleanEntry<T> {
    epoch: u64,
    value: T,
}

#[derive(Clone, Debug)]
struct DirtyEntry<T> {
    sequence: u64,
    value: T,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheRefreshStats {
    pub clean_invalidated: usize,
    pub dirty_retired: usize,
    pub placement_changed: bool,
    pub hwm_advanced: bool,
}

/// Owned by exactly one data-plane lane. All map operations are ordinary
/// lane-local operations: no atomics, locks, or shared cache lines.
pub struct LaneLookasideCache<T> {
    observed: RouteHwmSnapshot,
    clean: HashMap<u64, CleanEntry<T>>,
    dirty: HashMap<u64, DirtyEntry<T>>,
}

impl<T> LaneLookasideCache<T> {
    pub fn new(initial: RouteHwmSnapshot) -> io::Result<Self> {
        validate_snapshot(initial)?;
        Ok(Self {
            observed: initial,
            clean: HashMap::new(),
            dirty: HashMap::new(),
        })
    }

    pub fn observed(&self) -> RouteHwmSnapshot {
        self.observed
    }

    pub fn clean_len(&self) -> usize {
        self.clean.len()
    }

    pub fn dirty_len(&self) -> usize {
        self.dirty.len()
    }

    pub fn admit_clean(&mut self, logical_block: u64, value: T) {
        self.clean.insert(
            logical_block,
            CleanEntry {
                epoch: self.observed.placement_epoch,
                value,
            },
        );
    }

    /// Admit a reference to the latest logical write. A late older completion
    /// cannot replace a newer overlay.
    pub fn admit_dirty(&mut self, logical_block: u64, sequence: u64, value: T) -> io::Result<()> {
        if sequence == 0 {
            return Err(invalid("dirty cache sequence must be nonzero"));
        }
        if self
            .dirty
            .get(&logical_block)
            .is_none_or(|existing| sequence >= existing.sequence)
        {
            self.dirty
                .insert(logical_block, DirtyEntry { sequence, value });
        }
        self.clean.remove(&logical_block);
        Ok(())
    }

    pub fn read(&self, logical_block: u64) -> Option<&T> {
        self.dirty
            .get(&logical_block)
            .map(|entry| &entry.value)
            .or_else(|| {
                self.clean
                    .get(&logical_block)
                    .filter(|entry| entry.epoch == self.observed.placement_epoch)
                    .map(|entry| &entry.value)
            })
    }

    /// Apply one effective control generation at a lane batch boundary.
    pub fn refresh(
        &mut self,
        now_ns: u64,
        mailbox: &RouteHwmMailbox,
    ) -> io::Result<CacheRefreshStats> {
        let Some(next) = mailbox.load_effective(now_ns) else {
            return Ok(CacheRefreshStats::default());
        };
        if next.generation <= self.observed.generation {
            return Ok(CacheRefreshStats::default());
        }
        if next.placement_epoch < self.observed.placement_epoch
            || (next.placement_epoch == self.observed.placement_epoch
                && next.applied_hwm < self.observed.applied_hwm)
        {
            return Err(invalid("lane observed a route/HWM rollback"));
        }
        let placement_changed = next.placement_epoch != self.observed.placement_epoch;
        let hwm_advanced = next.applied_hwm > self.observed.applied_hwm;
        let clean_invalidated = if placement_changed {
            let count = self.clean.len();
            self.clean.clear();
            count
        } else {
            0
        };
        let before = self.dirty.len();
        self.dirty
            .retain(|_, entry| entry.sequence > next.applied_hwm);
        let dirty_retired = before - self.dirty.len();
        self.observed = next;
        Ok(CacheRefreshStats {
            clean_invalidated,
            dirty_retired,
            placement_changed,
            hwm_advanced,
        })
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn initial() -> RouteHwmSnapshot {
        RouteHwmSnapshot {
            generation: 1,
            placement_epoch: 7,
            applied_hwm: 100,
            effective_ns: 0,
        }
    }

    #[test]
    fn clean_cutover_invalidates_clean_reads_but_keeps_unapplied_dirty_overlay() {
        let mailbox = RouteHwmMailbox::new(initial()).unwrap();
        let mut cache = LaneLookasideCache::new(initial()).unwrap();
        cache.admit_clean(1, "old-clean");
        cache.admit_dirty(2, 110, "covered").unwrap();
        cache.admit_dirty(3, 130, "tail").unwrap();
        mailbox
            .publish_clean_cutover(RouteHwmSnapshot {
                generation: 2,
                placement_epoch: 8,
                applied_hwm: 120,
                effective_ns: 1_000,
            })
            .unwrap();
        assert_eq!(cache.read(1), Some(&"old-clean"));
        assert_eq!(
            cache.refresh(999, &mailbox).unwrap(),
            CacheRefreshStats::default()
        );
        let stats = cache.refresh(1_000, &mailbox).unwrap();
        assert_eq!(stats.clean_invalidated, 1);
        assert_eq!(stats.dirty_retired, 1);
        assert!(stats.placement_changed);
        assert_eq!(cache.read(1), None);
        assert_eq!(cache.read(2), None);
        assert_eq!(cache.read(3), Some(&"tail"));
    }

    #[test]
    fn hwm_progress_retires_dirty_references_without_flushing_clean_epoch() {
        let mailbox = RouteHwmMailbox::new(initial()).unwrap();
        let mut cache = LaneLookasideCache::new(initial()).unwrap();
        cache.admit_clean(1, 11);
        cache.admit_dirty(2, 150, 22).unwrap();
        mailbox
            .publish_hwm(RouteHwmSnapshot {
                generation: 2,
                placement_epoch: 7,
                applied_hwm: 150,
                effective_ns: 10,
            })
            .unwrap();
        let stats = cache.refresh(10, &mailbox).unwrap();
        assert_eq!(stats.clean_invalidated, 0);
        assert_eq!(stats.dirty_retired, 1);
        assert_eq!(cache.read(1), Some(&11));
    }

    #[test]
    fn clean_migration_refuses_an_applied_hwm_rollback() {
        let mailbox = RouteHwmMailbox::new(initial()).unwrap();
        let error = mailbox
            .publish_clean_cutover(RouteHwmSnapshot {
                generation: 2,
                placement_epoch: 8,
                applied_hwm: 99,
                effective_ns: 1,
            })
            .unwrap_err();
        assert!(error.to_string().contains("without reducing applied HWM"));
    }

    #[test]
    fn older_dirty_completion_cannot_replace_a_newer_overlay() {
        let mut cache = LaneLookasideCache::new(initial()).unwrap();
        cache.admit_dirty(4, 200, "new").unwrap();
        cache.admit_dirty(4, 199, "old").unwrap();
        assert_eq!(cache.read(4), Some(&"new"));
    }
}
