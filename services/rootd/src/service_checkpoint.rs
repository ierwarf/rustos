use super::session_accounting::SessionLedger;
use super::service_health::HealthBook;
use alloc::vec::Vec;
use rustos_user_abi::syscall::{
    ServiceCheckpointRecordWire, SERVICE_CHECKPOINT_ABI_VERSION, SERVICE_CHECKPOINT_FLAG_TOMBSTONE,
    SERVICE_CHECKPOINT_MAX_RECORDS,
};

const EINVAL: i32 = 22;
const ENOMEM: i32 = 12;
const ENOSPC: i32 = 28;
const EPROTO: i32 = 71;
const ESTALE: i32 = 116;

/// No one service can consume the entire rootd checkpoint ledger.
/// The global ABI limit remains unchanged for compatibility.
const MAX_RECORDS_PER_SERVICE: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StoredRecord {
    service_id: u64,
    wire: ServiceCheckpointRecordWire,
}

/// Rootd retains opaque service state across a supervised service restart.
/// It authenticates the namespace in the caller and enforces only generic
/// crash-consistency rules; record contents remain owned by the service.
pub(crate) struct ServiceCheckpointStore {
    records: Vec<StoredRecord>,
    /// Supervisor control state: session accounts are independent of the
    /// checkpoint records but share rootd's one serialized service loop.
    pub(crate) sessions: SessionLedger,
    pub(crate) health: HealthBook,
}

impl ServiceCheckpointStore {
    pub(crate) fn new() -> Self {
        // The ABI bound is a *limit*, not a request to reserve tens of
        // thousands of wire records from rootd's finite bootstrap heap.
        // Allocate incrementally on successful admission instead.
        Self {
            records: Vec::new(),
            sessions: SessionLedger::new(),
            health: HealthBook::new(),
        }
    }

    pub(crate) fn mutate(
        &mut self,
        service_id: u64,
        incoming: ServiceCheckpointRecordWire,
    ) -> Result<bool, i32> {
        validate_wire(&incoming)?;
        let existing_index = self.find(
            service_id,
            incoming.parent_hi,
            incoming.parent_lo,
            incoming.key_hi,
            incoming.key_lo,
        );
        if let Some(index) = existing_index {
            let current = self.records[index].wire;
            if current.operation_hi == incoming.operation_hi
                && current.operation_lo == incoming.operation_lo
            {
                return if current == incoming {
                    Ok(true)
                } else {
                    Err(EPROTO)
                };
            }
            if incoming.revision != current.revision.checked_add(1).ok_or(ESTALE)? {
                return Err(ESTALE);
            }
        } else {
            if incoming.revision != 1 {
                return Err(ESTALE);
            }
            // A checkpoint key is unique across every parent within one
            // service. Without this, the same key could be reparented and
            // become ambiguous to a descendant or a compaction receipt.
            if self
                .find_key(service_id, incoming.key_hi, incoming.key_lo)
                .is_some()
            {
                return Err(ESTALE);
            }
            if self.records.len() >= SERVICE_CHECKPOINT_MAX_RECORDS
                || self
                    .records
                    .iter()
                    .filter(|record| record.service_id == service_id)
                    .count()
                    >= MAX_RECORDS_PER_SERVICE
            {
                return Err(ENOSPC);
            }
        }

        if incoming.parent_hi != 0 || incoming.parent_lo != 0 {
            let parent = self
                .find_key(service_id, incoming.parent_hi, incoming.parent_lo)
                .map(|index| self.records[index].wire)
                .ok_or(ESTALE)?;
            if parent.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE != 0 {
                return Err(ESTALE);
            }
        }

        // A single fallible, parent-before-child pass computes a bounded
        // descendant bitmap. Reuse that exact snapshot both for the revision
        // preflight and for the final commit, never re-traverse the changed
        // tree after publishing the parent tombstone.
        let descendants = if incoming.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE != 0 {
            let mask = self.descendant_mask(service_id, (incoming.key_hi, incoming.key_lo))?;
            for (index, &selected) in mask.iter().enumerate() {
                if selected
                    && self.records[index].wire.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE == 0
                {
                    self.records[index]
                        .wire
                        .revision
                        .checked_add(1)
                        .ok_or(ESTALE)?;
                }
            }
            Some(mask)
        } else {
            None
        };

        match existing_index {
            Some(index) => self.records[index].wire = incoming,
            None => {
                // Reserve before changing any state (including dependent
                // tombstones); OOM must not partially publish a checkpoint.
                self.records.try_reserve(1).map_err(|_| ENOMEM)?;
                self.records.push(StoredRecord {
                    service_id,
                    wire: incoming,
                });
            }
        }

        if let Some(descendants) = descendants {
            for (index, selected) in descendants.into_iter().enumerate() {
                if selected
                    && self.records[index].wire.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE == 0
                {
                    let child = &mut self.records[index].wire;
                    child.flags = SERVICE_CHECKPOINT_FLAG_TOMBSTONE;
                    child.value_len = 0;
                    child.value.fill(0);
                    child.operation_hi = incoming.operation_hi;
                    child.operation_lo = incoming.operation_lo;
                    child.revision += 1;
                }
            }
        }
        Ok(false)
    }

    pub(crate) fn scan(
        &self,
        service_id: u64,
        cursor: usize,
        max: usize,
    ) -> Result<(Vec<ServiceCheckpointRecordWire>, usize), i32> {
        if max == 0 {
            return Err(EINVAL);
        }
        // Two bounded passes avoid infallible `collect` in the supervisor.
        // Reserve only the actual result count, never the untrusted `max`.
        let matching = || {
            self.records
                .iter()
                .filter(|record| record.service_id == service_id)
                .skip(cursor)
                .take(max)
                .map(|record| record.wire)
        };
        let count = matching().count();
        let mut records = Vec::new();
        records.try_reserve_exact(count).map_err(|_| ENOMEM)?;
        records.extend(matching());
        let next = cursor.saturating_add(records.len());
        Ok((records, next))
    }

    /// Reclaims one exact tombstoned object. An absent key is already compact
    /// and therefore succeeds, making a lost compaction reply retry-safe.
    pub(crate) fn compact(
        &mut self,
        service_id: u64,
        proof: ServiceCheckpointRecordWire,
    ) -> Result<bool, i32> {
        validate_wire(&proof)?;
        if proof.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE == 0 {
            return Err(EINVAL);
        }
        let Some(index) = self.find_key(service_id, proof.key_hi, proof.key_lo) else {
            return Ok(true);
        };
        if self.records[index].wire != proof {
            return Err(ESTALE);
        }

        // Collect the exact subtree as a stable per-record bitmap *before*
        // mutating the ledger. The allocation can fail without consuming the
        // proof or removing any other service's records.
        let mut remove = self.descendant_mask(service_id, (proof.key_hi, proof.key_lo))?;
        for (i, selected) in remove.iter_mut().enumerate() {
            *selected |= i == index;
            if *selected && self.records[i].wire.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE == 0 {
                return Err(ESTALE);
            }
        }
        let mut cursor = 0;
        self.records.retain(|_| {
            let keep = !remove[cursor];
            cursor += 1;
            keep
        });
        Ok(false)
    }

    /// Unique key lookup. Parent is deliberately *not* part of identity.
    /// Parent links never cross a service namespace.
    fn find_key(&self, service_id: u64, key_hi: u64, key_lo: u64) -> Option<usize> {
        self.records.iter().position(|record| {
            record.service_id == service_id
                && record.wire.key_hi == key_hi
                && record.wire.key_lo == key_lo
        })
    }

    /// A bounded ancestry walk, without recursion, heap allocation or a
    /// kernel call. The caller uses it only for slow checkpoint mutation and
    /// compaction, never for IPC transport or scheduler hot paths.
    /// Parent-before-child insertion is the index invariant. This one-pass
    /// closure is bounded by service checkpoint count; unlike repeated
    /// ancestry searches it cannot explode with pathological tree depth.
    /// The bitmap is allocated before any state mutation and recycled by
    /// the caller for either revocation or compaction.
    fn descendant_mask(&self, service_id: u64, ancestor_key: (u64, u64)) -> Result<Vec<bool>, i32> {
        let mut descendants = Vec::new();
        descendants
            .try_reserve_exact(self.records.len())
            .map_err(|_| ENOMEM)?;
        for (index, record) in self.records.iter().enumerate() {
            if record.service_id != service_id {
                descendants.push(false);
                continue;
            }
            let parent = (record.wire.parent_hi, record.wire.parent_lo);
            let selected = if parent == (0, 0) {
                false
            } else if parent == ancestor_key {
                true
            } else {
                let parent_index = self
                    .find_key(service_id, parent.0, parent.1)
                    .ok_or(ESTALE)?;
                if parent_index >= index {
                    return Err(EPROTO);
                }
                descendants[parent_index]
            };
            descendants.push(selected);
        }
        Ok(descendants)
    }

    fn find(
        &self,
        service_id: u64,
        parent_hi: u64,
        parent_lo: u64,
        key_hi: u64,
        key_lo: u64,
    ) -> Option<usize> {
        self.records.iter().position(|record| {
            record.service_id == service_id
                && record.wire.parent_hi == parent_hi
                && record.wire.parent_lo == parent_lo
                && record.wire.key_hi == key_hi
                && record.wire.key_lo == key_lo
        })
    }
}

fn validate_wire(record: &ServiceCheckpointRecordWire) -> Result<(), i32> {
    let value_len = record.value_len as usize;
    if record.version != SERVICE_CHECKPOINT_ABI_VERSION
        || record.flags & !SERVICE_CHECKPOINT_FLAG_TOMBSTONE != 0
        || (record.key_hi == 0 && record.key_lo == 0)
        || (record.operation_hi == 0 && record.operation_lo == 0)
        || record.revision == 0
        || value_len > record.value.len()
        || record.value[value_len..].iter().any(|byte| *byte != 0)
        || (record.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE != 0 && value_len != 0)
        || (record.parent_hi == record.key_hi && record.parent_lo == record.key_lo)
    {
        return Err(EINVAL);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(
        key: (u64, u64),
        parent: (u64, u64),
        revision: u64,
        op: u64,
    ) -> ServiceCheckpointRecordWire {
        let mut record = ServiceCheckpointRecordWire {
            key_hi: key.0,
            key_lo: key.1,
            parent_hi: parent.0,
            parent_lo: parent.1,
            operation_lo: op,
            revision,
            value_len: 1,
            ..ServiceCheckpointRecordWire::default()
        };
        record.value[0] = revision as u8;
        record
    }

    #[test]
    fn startup_does_not_reserve_full_checkpoint_limit() {
        let mut store = ServiceCheckpointStore::new();
        assert_eq!(store.records.len(), 0);
        assert_eq!(store.records.capacity(), 0);
        assert_eq!(store.mutate(3, record((1, 1), (0, 0), 1, 2)), Ok(false));
        assert!(store.records.capacity() < SERVICE_CHECKPOINT_MAX_RECORDS);
    }

    #[test]
    fn paginated_scan_is_exact_and_does_not_reserve_untrusted_max() {
        let mut store = ServiceCheckpointStore::new();
        let a = record((1, 1), (0, 0), 1, 10);
        let b = record((2, 2), (0, 0), 1, 11);
        let foreign = record((3, 3), (0, 0), 1, 12);
        store.mutate(5, a).unwrap();
        store.mutate(6, foreign).unwrap();
        store.mutate(5, b).unwrap();
        assert_eq!(store.scan(5, 0, 1), Ok((vec![a], 1)));
        assert_eq!(store.scan(5, 1, usize::MAX), Ok((vec![b], 2)));
        assert_eq!(
            store.scan(5, usize::MAX, usize::MAX),
            Ok((vec![], usize::MAX))
        );
        assert_eq!(store.scan(5, 0, 0), Err(EINVAL));
        assert_eq!(store.scan(6, 0, usize::MAX), Ok((vec![foreign], 1)));
    }

    #[test]
    fn exact_retry_is_idempotent_and_stale_retry_cannot_rollback() {
        let mut store = ServiceCheckpointStore::new();
        let first = record((7, 1), (0, 0), 1, 10);
        assert_eq!(store.mutate(3, first), Ok(false));
        assert_eq!(store.mutate(3, first), Ok(true));
        let second = record((7, 1), (0, 0), 2, 11);
        assert_eq!(store.mutate(3, second), Ok(false));
        assert_eq!(store.mutate(3, first), Err(ESTALE));
    }

    #[test]
    fn parent_tombstone_atomically_revokes_children() {
        let mut store = ServiceCheckpointStore::new();
        let parent = record((9, 1), (0, 0), 1, 20);
        let child = record((44, 2), (9, 1), 1, 21);
        store.mutate(3, parent).unwrap();
        store.mutate(3, child).unwrap();
        let mut tombstone = parent;
        tombstone.flags = SERVICE_CHECKPOINT_FLAG_TOMBSTONE;
        tombstone.value_len = 0;
        tombstone.value.fill(0);
        tombstone.operation_lo = 22;
        tombstone.revision = 2;
        store.mutate(3, tombstone).unwrap();
        let (records, _) = store.scan(3, 0, 8).unwrap();
        assert_eq!(records.len(), 2);
        assert!(records
            .iter()
            .all(|record| record.flags == SERVICE_CHECKPOINT_FLAG_TOMBSTONE));
        assert_eq!(store.mutate(3, child), Err(ESTALE));
    }

    #[test]
    fn exact_tombstone_compaction_is_idempotent_and_reclaims_children() {
        let mut store = ServiceCheckpointStore::new();
        let parent = record((9, 7), (0, 0), 1, 30);
        let child = record((44, 8), (9, 7), 1, 31);
        store.mutate(3, parent).unwrap();
        store.mutate(3, child).unwrap();
        let mut tombstone = parent;
        tombstone.flags = SERVICE_CHECKPOINT_FLAG_TOMBSTONE;
        tombstone.value_len = 0;
        tombstone.value.fill(0);
        tombstone.operation_lo = 32;
        tombstone.revision = 2;
        store.mutate(3, tombstone).unwrap();

        assert_eq!(store.compact(3, tombstone), Ok(false));
        assert_eq!(store.scan(3, 0, 8).unwrap().0.len(), 0);
        assert_eq!(store.compact(3, tombstone), Ok(true));
    }

    #[test]
    fn nested_checkpoint_revocation_is_transitive_and_compacts_exact_subtree() {
        let mut store = ServiceCheckpointStore::new();
        let top = record((100, 1), (0, 0), 1, 1000);
        let mid = record((100, 2), (100, 1), 1, 1001);
        let leaf = record((100, 3), (100, 2), 1, 1002);
        let deep = record((100, 4), (100, 3), 1, 1003);
        let sibling = record((100, 5), (0, 0), 1, 1004);
        let foreign = record((100, 6), (0, 0), 1, 1005);
        for item in [top, mid, leaf, deep, sibling] {
            assert_eq!(store.mutate(7, item), Ok(false));
        }
        store.mutate(8, foreign).unwrap();
        let mut tombstone = top;
        tombstone.flags = SERVICE_CHECKPOINT_FLAG_TOMBSTONE;
        tombstone.operation_lo = 1010;
        tombstone.revision = 2;
        tombstone.value_len = 0;
        tombstone.value.fill(0);
        store.mutate(7, tombstone).unwrap();
        let (records, _) = store.scan(7, 0, 20).unwrap();
        assert_eq!(records.len(), 5);
        for record in records.iter().take(4) {
            assert_ne!(record.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE, 0);
            assert_eq!(record.revision, 2);
            assert_eq!(record.value_len, 0);
        }
        assert_eq!(store.mutate(7, deep), Err(ESTALE));
        assert_eq!(store.compact(7, tombstone), Ok(false));
        assert_eq!(store.scan(7, 0, 20).unwrap().0, vec![sibling]);
        assert_eq!(store.scan(8, 0, 20).unwrap().0, vec![foreign]);
        assert_eq!(store.compact(7, tombstone), Ok(true));
    }

    #[test]
    fn deep_tree_compaction_is_bounded_and_preserves_foreign_service() {
        let mut store = ServiceCheckpointStore::new();
        let root = record((5, 1), (0, 0), 1, 10);
        store.mutate(1, root).unwrap();
        for n in 2..=128 {
            store
                .mutate(1, record((5, n), (5, n - 1), 1, 10 + n))
                .unwrap();
        }
        let other = record((8, 8), (0, 0), 1, 8);
        store.mutate(2, other).unwrap();
        let mut tombstone = root;
        tombstone.revision = 2;
        tombstone.operation_lo = 999;
        tombstone.flags = SERVICE_CHECKPOINT_FLAG_TOMBSTONE;
        tombstone.value_len = 0;
        tombstone.value.fill(0);
        store.mutate(1, tombstone).unwrap();
        assert_eq!(store.scan(1, 0, 300).unwrap().0.len(), 128);
        assert_eq!(store.compact(1, tombstone), Ok(false));
        assert_eq!(store.scan(1, 0, 300).unwrap().0.len(), 0);
        assert_eq!(store.scan(2, 0, 300).unwrap().0, vec![other]);
    }

    #[test]
    fn checkpoint_identity_is_per_service_not_per_parent() {
        let mut store = ServiceCheckpointStore::new();
        let root_a = record((1, 1), (0, 0), 1, 1);
        let root_b = record((1, 2), (0, 0), 1, 2);
        store.mutate(7, root_a).unwrap();
        store.mutate(7, root_b).unwrap();
        let descendant = record((4, 4), (1, 1), 1, 3);
        store.mutate(7, descendant).unwrap();
        assert_eq!(store.mutate(7, record((4, 4), (1, 2), 1, 4)), Err(ESTALE));
        assert_eq!(store.mutate(8, record((4, 4), (0, 0), 1, 4)), Ok(false));
    }

    #[test]
    fn tombstone_preflight_rejects_deep_revision_overflow_atomically() {
        let mut store = ServiceCheckpointStore::new();
        let top = record((1, 1), (0, 0), 1, 100);
        let mid = record((2, 2), (1, 1), 1, 101);
        let leaf = record((3, 3), (2, 2), 1, 102);
        for item in [top, mid, leaf] {
            store.mutate(7, item).unwrap();
        }
        // Create a valid but maximal child revision to simulate long-lived
        // checkpoint history before the supervisor tries to cascade.
        let index = store.find_key(7, 3, 3).unwrap();
        store.records[index].wire.revision = u64::MAX;
        let prior = store.scan(7, 0, 10).unwrap().0;
        let mut tombstone = top;
        tombstone.flags = SERVICE_CHECKPOINT_FLAG_TOMBSTONE;
        tombstone.operation_lo = 200;
        tombstone.revision = 2;
        tombstone.value_len = 0;
        tombstone.value.fill(0);
        assert_eq!(store.mutate(7, tombstone), Err(ESTALE));
        assert_eq!(store.scan(7, 0, 10).unwrap().0, prior);
    }

    #[test]
    fn compaction_proof_rejects_live_descendant_without_partial_reclaim() {
        let mut store = ServiceCheckpointStore::new();
        let top = record((1, 1), (0, 0), 1, 200);
        let leaf = record((2, 2), (1, 1), 1, 201);
        store.mutate(7, top).unwrap();
        store.mutate(7, leaf).unwrap();
        // The exact root receipt is tombstoned but the child is still live:
        // never compact an incomplete transaction.
        let mut tombstone = top;
        tombstone.flags = SERVICE_CHECKPOINT_FLAG_TOMBSTONE;
        tombstone.operation_lo = 202;
        tombstone.revision = 2;
        tombstone.value_len = 0;
        tombstone.value.fill(0);
        let idx = store.find_key(7, 1, 1).unwrap();
        store.records[idx].wire = tombstone;
        assert_eq!(store.compact(7, tombstone), Err(ESTALE));
        assert_eq!(store.records.len(), 2);
    }

    #[test]
    fn stale_compaction_proof_cannot_reclaim_a_reused_key() {
        let mut store = ServiceCheckpointStore::new();
        let first = record((12, 9), (0, 0), 1, 40);
        store.mutate(3, first).unwrap();
        let mut tombstone = first;
        tombstone.flags = SERVICE_CHECKPOINT_FLAG_TOMBSTONE;
        tombstone.value_len = 0;
        tombstone.value.fill(0);
        tombstone.operation_lo = 41;
        tombstone.revision = 2;
        store.mutate(3, tombstone).unwrap();
        store.compact(3, tombstone).unwrap();

        let replacement = record((12, 9), (0, 0), 1, 42);
        store.mutate(3, replacement).unwrap();
        assert_eq!(store.compact(3, tombstone), Err(ESTALE));
        assert_eq!(store.scan(3, 0, 8).unwrap().0, vec![replacement]);
    }
}
