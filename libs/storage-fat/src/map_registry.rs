//! Per-file read-map revocation.
//!
//! A read map describes where one file's bytes live on the device. It stays
//! valid exactly while nothing that determined those bytes has been written:
//! the data extents themselves, the FAT entries that link the cluster chain,
//! and the file's directory entry (first cluster, size, name). The registry
//! records those byte ranges per admitted file; `FatDisk::write` revokes every
//! registration overlapping a write before the write can reach the media.
//!
//! Consequences, all deliberate:
//! - A write to an unrelated file or directory never revokes the map.
//! - Append, truncate, rename, unlink, delete+recreate and in-place data
//!   overwrite of the mapped file always revoke it, so a valid map's extents
//!   and the bytes behind them are exactly what they were when it was planned.
//! - A generation is a registration id. It is shared by every plan of an
//!   unchanged file (stable identity) and never reused after revocation.
//! - Absence means "not current". Eviction under the entry/byte caps therefore
//!   only costs liveness (a replan), never correctness.
//!
//! All mutation happens under the shared device lock, and registration only
//! happens inside `plan_read` under the namespace lock, so no write can slip
//! between dependency discovery and registration.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use storage_core::file_io::RangeError;

/// Half-open device byte range `[start, end)`.
pub(crate) type Span = (u64, u64);

pub(crate) const MAX_REGISTERED_MAPS: usize = 128;
pub(crate) const MAX_REGISTERED_BYTES: usize = 16 * 1024 * 1024;

struct Entry {
    generation: u64,
    deps: Vec<Span>,
}

pub(crate) struct MapRegistry {
    next_generation: u64,
    entries: BTreeMap<u64, Entry>,
    bytes: usize,
}

/// Sorts and coalesces overlapping or touching spans; drops empty ones.
pub(crate) fn normalize(spans: &mut Vec<Span>) {
    spans.retain(|span| span.0 < span.1);
    spans.sort_unstable();
    let mut out = 0;
    for index in 0..spans.len() {
        let span = spans[index];
        if out != 0 && span.0 <= spans[out - 1].1 {
            spans[out - 1].1 = spans[out - 1].1.max(span.1);
        } else {
            spans[out] = span;
            out += 1;
        }
    }
    spans.truncate(out);
}

/// `deps` is normalized (sorted, disjoint).
fn overlaps(deps: &[Span], start: u64, end: u64) -> bool {
    let index = deps.partition_point(|span| span.1 <= start);
    deps.get(index).is_some_and(|span| span.0 < end)
}

impl MapRegistry {
    pub(crate) const fn new() -> Self {
        Self { next_generation: 1, entries: BTreeMap::new(), bytes: 0 }
    }

    /// Revokes every registration whose dependencies intersect `[start, end)`.
    pub(crate) fn revoke(&mut self, start: u64, end: u64) {
        if self.entries.is_empty() || start >= end {
            return;
        }
        let mut freed = 0;
        self.entries.retain(|_, entry| {
            let hit = overlaps(&entry.deps, start, end);
            if hit {
                freed += entry.deps.len() * core::mem::size_of::<Span>();
            }
            !hit
        });
        self.bytes -= freed;
    }

    /// Generation a plan of `object` must carry: the live registration's, or
    /// the one `admit` will assign. Only meaningful while the namespace lock
    /// is held across the matching `admit`.
    pub(crate) fn expected_generation(&self, object: u64) -> u64 {
        self.entries.get(&object).map_or(self.next_generation, |entry| entry.generation)
    }

    pub(crate) fn is_current(&self, object: u64, generation: u64) -> bool {
        self.entries.get(&object).is_some_and(|entry| entry.generation == generation)
    }

    /// Registers `deps` (normalized) for `object` under `generation`, which
    /// must be the value `expected_generation` returned. A live registration
    /// with different dependencies means the media changed behind the
    /// registry; it is dropped and the plan reports `Stale` so the caller
    /// replans under a fresh generation.
    pub(crate) fn admit(&mut self, object: u64, generation: u64, deps: Vec<Span>) -> Result<(), RangeError> {
        if let Some(entry) = self.entries.get(&object) {
            if entry.generation == generation && entry.deps == deps {
                return Ok(());
            }
            self.remove(object);
            return Err(RangeError::Stale);
        }
        if generation != self.next_generation {
            return Err(RangeError::Stale);
        }
        let cost = deps.len() * core::mem::size_of::<Span>();
        if cost > MAX_REGISTERED_BYTES {
            return Err(RangeError::Budget);
        }
        let next = generation.checked_add(1).ok_or(RangeError::Invalid)?;
        while self.entries.len() >= MAX_REGISTERED_MAPS || self.bytes + cost > MAX_REGISTERED_BYTES {
            let oldest = self.entries.iter().min_by_key(|(_, entry)| entry.generation).map(|(object, _)| *object);
            match oldest {
                Some(object) => self.remove(object),
                None => break,
            }
        }
        self.bytes += cost;
        self.next_generation = next;
        self.entries.insert(object, Entry { generation, deps });
        Ok(())
    }

    fn remove(&mut self, object: u64) {
        if let Some(entry) = self.entries.remove(&object) {
            self.bytes -= entry.deps.len() * core::mem::size_of::<Span>();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn admit(registry: &mut MapRegistry, object: u64, deps: &[Span]) -> u64 {
        let mut deps = deps.to_vec();
        normalize(&mut deps);
        let generation = registry.expected_generation(object);
        registry.admit(object, generation, deps).unwrap();
        generation
    }

    #[test]
    fn normalize_merges_touching_and_overlapping_spans() {
        let mut spans = vec![(10, 20), (0, 5), (5, 8), (15, 30), (40, 40)];
        normalize(&mut spans);
        assert_eq!(spans, vec![(0, 8), (10, 30)]);
    }

    #[test]
    fn revoke_is_exact_on_span_edges() {
        let mut registry = MapRegistry::new();
        let a = admit(&mut registry, 1, &[(100, 200), (400, 500)]);
        for (start, end) in [(0, 100), (200, 400), (500, 900)] {
            registry.revoke(start, end);
            assert!(registry.is_current(1, a), "{start}..{end} touches nothing");
        }
        registry.revoke(199, 201);
        assert!(!registry.is_current(1, a));
        assert_eq!(registry.bytes, 0);
    }

    #[test]
    fn unchanged_file_keeps_generation_and_revoked_file_never_reuses_it() {
        let mut registry = MapRegistry::new();
        let first = admit(&mut registry, 7, &[(0, 32)]);
        assert_eq!(admit(&mut registry, 7, &[(0, 32)]), first);
        let other = admit(&mut registry, 8, &[(64, 96)]);
        assert_ne!(first, other);
        registry.revoke(0, 1);
        assert!(registry.is_current(8, other));
        let second = admit(&mut registry, 7, &[(0, 32)]);
        assert!(second > other && second != first);
    }

    #[test]
    fn mismatched_live_registration_is_dropped_and_replanned() {
        let mut registry = MapRegistry::new();
        let generation = admit(&mut registry, 1, &[(0, 32)]);
        assert_eq!(registry.admit(1, generation, vec![(0, 64)]), Err(RangeError::Stale));
        assert!(!registry.is_current(1, generation));
        assert_ne!(admit(&mut registry, 1, &[(0, 64)]), generation);
    }

    #[test]
    fn caps_evict_oldest_and_stale_means_not_current() {
        let mut registry = MapRegistry::new();
        let first = admit(&mut registry, 0, &[(0, 1)]);
        for object in 1..=MAX_REGISTERED_MAPS as u64 {
            admit(&mut registry, object, &[(object * 10, object * 10 + 1)]);
        }
        assert_eq!(registry.entries.len(), MAX_REGISTERED_MAPS);
        assert!(!registry.is_current(0, first));
        assert!(registry.is_current(MAX_REGISTERED_MAPS as u64, registry.expected_generation(MAX_REGISTERED_MAPS as u64)));
    }
}
