//! A process-lifetime ceiling for application-frame random-IV GCM attempts.
//! Only approved_crypto::seal reserves here; TLS and direct dependency crypto
//! calls are not observed by this registry.
//! This is not a distributed counter: reuse on another process or after restart
//! must be covered separately by the deployment's aggregate key budget.
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

const MAX_ATTEMPTS: u64 = 1 << 32;
// Never evict a used key: eviction would reset its invocation budget. Bound
// memory instead, failing closed for previously unseen encryption keys.
const MAX_TRACKED_KEYS: usize = 65_536;

struct Registry {
    keys: Mutex<HashMap<[u8; 32], Arc<AtomicU64>>>,
    capacity: usize,
    #[cfg(test)]
    limit: u64,
}

impl Registry {
    fn counter(&self, key_id: [u8; 32]) -> io::Result<Arc<AtomicU64>> {
        let mut keys = self
            .keys
            .lock()
            .map_err(|_| io::Error::other("FIPS key usage registry poisoned"))?;
        if let Some(counter) = keys.get(&key_id) {
            Ok(Arc::clone(counter))
        } else {
            if keys.len() >= self.capacity {
                return Err(io::Error::other("FIPS encryption key registry exhausted"));
            }
            let counter = Arc::new(AtomicU64::new(0));
            keys.insert(key_id, Arc::clone(&counter));
            Ok(counter)
        }
    }

    fn snapshot(&self) -> io::Result<BTreeMap<&'static str, u64>> {
        let keys = self
            .keys
            .lock()
            .map_err(|_| io::Error::other("FIPS key usage registry poisoned"))?;
        let mut max_used = 0;
        let mut exhausted = 0;
        for counter in keys.values() {
            let used = counter.load(Ordering::Relaxed);
            max_used = max_used.max(used);
            exhausted += u64::from(used >= MAX_ATTEMPTS);
        }
        Ok(BTreeMap::from([
            ("process_tracked_keys", keys.len() as u64),
            ("process_max_key_consumed_attempts", max_used),
            (
                "process_min_key_remaining_attempts",
                MAX_ATTEMPTS.saturating_sub(max_used),
            ),
            ("per_key_attempt_limit", MAX_ATTEMPTS),
            ("process_registry_key_capacity", self.capacity as u64),
            ("process_exhausted_keys", exhausted),
        ]))
    }

    #[cfg(test)]
    fn reserve(&self, key_id: [u8; 32]) -> io::Result<()> {
        reserve_counter(self.counter(key_id)?.as_ref(), self.limit)
    }
}

/// Reserve before calling the encryption primitive. Failed encryption attempts
/// consume reservations too; there is no rollback or reset API.
fn process_registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| Registry {
        keys: Mutex::new(HashMap::new()),
        capacity: MAX_TRACKED_KEYS,
        #[cfg(test)]
        limit: MAX_ATTEMPTS,
    })
}

fn reserve_counter(counter: &AtomicU64, limit: u64) -> io::Result<()> {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
            (used < limit).then(|| used + 1)
        })
        .map(|_| ())
        .map_err(|_| io::Error::other("FIPS GCM per-process key invocation limit reached"))
}

pub(super) struct Budget {
    key_id: [u8; 32],
    counter: OnceLock<Arc<AtomicU64>>,
}

impl Budget {
    pub(super) fn new(key_id: [u8; 32]) -> Self {
        Self {
            key_id,
            counter: OnceLock::new(),
        }
    }

    pub(super) fn reserve(&self) -> io::Result<()> {
        if self.counter.get().is_none() {
            // Racing initializers receive the same registry counter. A key
            // enters the bounded registry only when first used to encrypt.
            let counter = process_registry().counter(self.key_id)?;
            let _ = self.counter.set(counter);
        }
        // Subsequent frames need one atomic reservation, no global mutex.
        reserve_counter(
            self.counter.get().expect("counter initialized"),
            MAX_ATTEMPTS,
        )
    }
}

pub(super) fn snapshot() -> io::Result<BTreeMap<&'static str, u64>> {
    process_registry().snapshot()
}

#[cfg(test)]
pub(super) fn tracked_for_test(key_id: [u8; 32]) -> bool {
    process_registry()
        .keys
        .lock()
        .unwrap()
        .contains_key(&key_id)
}

#[cfg(test)]
pub(super) fn exhaust_for_test(key_id: [u8; 32]) {
    let registry = process_registry();
    registry.reserve(key_id).unwrap();
    registry.keys.lock().unwrap()[&key_id].store(MAX_ATTEMPTS, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(capacity: usize, limit: u64) -> Registry {
        Registry {
            keys: Mutex::new(HashMap::new()),
            capacity,
            limit,
        }
    }

    #[test]
    fn recreating_a_key_cannot_reset_its_budget() {
        let registry = registry(2, 2);
        assert!(registry.reserve([1; 32]).is_ok());
        assert!(registry.reserve([2; 32]).is_ok());
        assert!(registry.reserve([1; 32]).is_ok());
        assert!(registry.reserve([1; 32]).is_err());
        assert!(registry.reserve([2; 32]).is_ok());
    }

    #[test]
    fn full_registry_never_evicts_or_blocks_existing_budget() {
        let registry = registry(1, 2);
        assert!(registry.reserve([1; 32]).is_ok());
        assert!(registry.reserve([2; 32]).is_err());
        assert!(registry.reserve([1; 32]).is_ok());
        assert!(registry.reserve([1; 32]).is_err());
    }

    #[test]
    fn concurrent_attempts_cannot_overspend() {
        let registry = Arc::new(registry(1, 101));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let registry = Arc::clone(&registry);
                std::thread::spawn(move || {
                    (0..100)
                        .filter(|_| registry.reserve([1; 32]).is_ok())
                        .count()
                })
            })
            .collect();
        assert_eq!(
            workers
                .into_iter()
                .map(|w| w.join().unwrap())
                .sum::<usize>(),
            101
        );
    }

    #[test]
    fn production_ceiling_and_capacity_are_fixed() {
        assert_eq!(MAX_ATTEMPTS, 4_294_967_296);
        assert_eq!(MAX_TRACKED_KEYS, 65_536);
        assert!(Budget::new([0; 32]).reserve().is_ok());
    }

    #[test]
    fn snapshot_counts_actual_shared_keys_and_exposes_no_identifiers() {
        let registry = registry(8, MAX_ATTEMPTS);
        let first = registry.counter([71; 32]).unwrap();
        let same = registry.counter([71; 32]).unwrap();
        let other = registry.counter([72; 32]).unwrap();
        reserve_counter(&first, MAX_ATTEMPTS).unwrap();
        reserve_counter(&same, MAX_ATTEMPTS).unwrap();
        reserve_counter(&other, MAX_ATTEMPTS).unwrap();
        let stats = registry.snapshot().unwrap();
        assert_eq!(stats["process_tracked_keys"], 2);
        assert_eq!(stats["process_max_key_consumed_attempts"], 2);
        assert_eq!(
            stats["process_min_key_remaining_attempts"],
            MAX_ATTEMPTS - 2
        );
        first.store(MAX_ATTEMPTS, Ordering::Relaxed);
        let stats = registry.snapshot().unwrap();
        assert_eq!(stats["process_min_key_remaining_attempts"], 0);
        assert_eq!(stats["process_exhausted_keys"], 1);
        assert_eq!(stats.len(), 6);
        assert!(!format!("{stats:?}").contains("[71"));
    }

    #[test]
    fn poisoned_registry_is_fatal() {
        let registry = registry(1, 1);
        let _ = std::panic::catch_unwind(|| {
            let _guard = registry.keys.lock().unwrap();
            panic!("test poison");
        });
        assert!(registry.reserve([1; 32]).is_err());
    }
}
