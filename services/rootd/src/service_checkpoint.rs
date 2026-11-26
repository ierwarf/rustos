use super::session_accounting::SessionLedger;
use super::provider_billing::ProviderBilling;
use super::service_health::HealthBook;
use super::service_updates::UpdateBook;
use alloc::collections::BTreeMap;
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
const MAX_RECORDS_PER_SERVICE: usize = 16_384;

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
    /// Index from `(service_id, key_hi, key_lo)` to index in `records`.
    /// Maintained on every insert and remapped in place on compaction so `find`,
    /// `find_key`, and `descendant_mask` avoid O(n) linear scans.
    key_index: BTreeMap<(u64, u64, u64), usize>,
    /// Per-service record counts, matching `records.iter().filter(|r| r.service_id == id).count()`.
    service_counts: BTreeMap<u64, usize>,
    /// Supervisor control state: session accounts are independent of the
    /// checkpoint records but share rootd's one serialized service loop.
    pub(crate) sessions: SessionLedger,
    /// Generic provider-session state is independent of the console ledger.
    pub(crate) providers: ProviderBilling,
    pub(crate) health: HealthBook,
    pub(crate) updates: UpdateBook,
    pub(crate) activation: super::activation_owner::ActivationOwner,
    pub(crate) personality: super::personality_policy::PersonalityPolicy,
}

/// Result of a fully committed `mutate_batch`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BatchCommit {
    pub(crate) applied: usize,
    pub(crate) duplicates: usize,
}

/// A batch stopped at record index `applied`; records before it are committed.
/// `after_journal` means the failing record was already journaled but could not
/// be applied: the journal and the DAG diverge and the supervisor must fail
/// closed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BatchFailure {
    pub(crate) errno: i32,
    pub(crate) after_journal: bool,
    pub(crate) applied: usize,
    pub(crate) duplicates: usize,
}

pub(crate) struct MutatePlan {
    existing_index: Option<usize>,
    descendants: Option<Vec<bool>>,
    duplicate: bool,
}

impl ServiceCheckpointStore {
    pub(crate) fn new() -> Self {
        // The ABI bound is a *limit*, not a request to reserve tens of
        // thousands of wire records from rootd's finite bootstrap heap.
        // Allocate incrementally on successful admission instead.
        Self {
            records: Vec::new(),
            key_index: BTreeMap::new(),
            service_counts: BTreeMap::new(),
            sessions: SessionLedger::new(),
            providers: ProviderBilling::new(),
            health: HealthBook::new(),
            updates: UpdateBook::new(),
            activation: super::activation_owner::ActivationOwner::new(),
            personality: super::personality_policy::PersonalityPolicy::new(),
        }
    }

    /// Evaluate one checkpoint mutation without publishing it.
    /// Kernel journal append follows only a successful dry-run, so ENOSPC
    /// cannot mutate the rootd DAG without a corresponding log entry. The
    /// dry-run is the exact planning half of `mutate` (no ledger copy), and
    /// reserves record-vector capacity before commit. Index insertion still
    /// follows BTreeMap's allocator-failure semantics.
    pub(crate) fn preflight_mutate(
        &mut self,
        service_id: u64,
        record: ServiceCheckpointRecordWire,
    ) -> Result<MutatePlan, i32> {
        let plan = self.plan_mutate(service_id, record)?;
        if !plan.duplicate && plan.existing_index.is_none() {
            self.records.try_reserve(1).map_err(|_| ENOMEM)?;
        }
        Ok(plan)
    }

    /// Ordered batch of `mutate` records with *committed-prefix* semantics.
    ///
    /// Each record is planned against the DAG as left by its predecessors,
    /// handed to `journal` (the durable-within-boot commit point; it must not
    /// touch this store) and only then applied. This is exactly the sequence
    /// of N single-record mutations, so journal order, per-record preflight and
    /// DAG validation are unchanged. The first failing record stops the batch;
    /// everything before it stays committed and nothing after it is touched.
    pub(crate) fn mutate_batch(
        &mut self,
        service_id: u64,
        records: &[ServiceCheckpointRecordWire],
        mut journal: impl FnMut(ServiceCheckpointRecordWire) -> Result<(), i32>,
    ) -> Result<BatchCommit, BatchFailure> {
        let mut commit = BatchCommit { applied: 0, duplicates: 0 };
        for (position, &record) in records.iter().enumerate() {
            let fail = |errno, after_journal, commit: &BatchCommit| BatchFailure {
                errno,
                after_journal,
                applied: commit.applied,
                duplicates: commit.duplicates,
            };
            let plan = match self.preflight_mutate(service_id, record) {
                Ok(plan) => plan,
                // Replay of an already-committed batch whose reply was lost:
                // an earlier revision of a key that a *later record of this
                // same batch* already holds verbatim (OPEN's staging record
                // is superseded by its final record) is history the store has
                // provably passed. Skip it; it is not journaled again. Any
                // other staleness stays a hard refusal.
                Err(ESTALE) if self.superseded_by_committed(service_id, record, &records[position + 1..]) => {
                    commit.applied += 1;
                    commit.duplicates += 1;
                    continue;
                }
                Err(errno) => return Err(fail(errno, false, &commit)),
            };
            journal(record).map_err(|errno| fail(errno, false, &commit))?;
            let duplicate = self
                .apply_plan(service_id, record, plan)
                .map_err(|errno| fail(errno, true, &commit))?;
            commit.applied += 1;
            commit.duplicates += usize::from(duplicate);
        }
        Ok(commit)
    }

    fn superseded_by_committed(
        &self,
        service_id: u64,
        record: ServiceCheckpointRecordWire,
        later: &[ServiceCheckpointRecordWire],
    ) -> bool {
        later.iter().any(|next| {
            (next.parent_hi, next.parent_lo, next.key_hi, next.key_lo)
                == (record.parent_hi, record.parent_lo, record.key_hi, record.key_lo)
                && next.revision > record.revision
                && self
                    .find(service_id, next.parent_hi, next.parent_lo, next.key_hi, next.key_lo)
                    .is_some_and(|index| self.records[index].wire == *next)
        })
    }

    #[cfg(test)]
    fn preflight_compact(
        &self,
        service_id: u64,
        proof: ServiceCheckpointRecordWire,
    ) -> Result<(), i32> {
        self.plan_compact(service_id, proof).map(|_| ())
    }

    pub(crate) fn mutate(
        &mut self,
        service_id: u64,
        incoming: ServiceCheckpointRecordWire,
    ) -> Result<bool, i32> {
        let plan = self.plan_mutate(service_id, incoming)?;
        self.apply_plan(service_id, incoming, plan)
    }

    /// Commit a plan produced by `plan_mutate` for the same, unchanged store.
    fn apply_plan(
        &mut self,
        service_id: u64,
        incoming: ServiceCheckpointRecordWire,
        plan: MutatePlan,
    ) -> Result<bool, i32> {
        if plan.duplicate {
            return Ok(true);
        }
        let MutatePlan { existing_index, descendants, .. } = plan;
        match existing_index {
            Some(index) => self.records[index].wire = incoming,
            None => {
                // A fallible vector reservation must precede any mutation,
                // including dependent tombstones. Index insertion below uses
                // BTreeMap's ordinary allocator-failure behavior.
                self.records.try_reserve(1).map_err(|_| ENOMEM)?;
                let new_index = self.records.len();
                self.records.push(StoredRecord {
                    service_id,
                    wire: incoming,
                });
                self.key_index
                    .insert((service_id, incoming.key_hi, incoming.key_lo), new_index);
                *self.service_counts.entry(service_id).or_insert(0) += 1;
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

    /// Read-only validation of one mutation. Everything that can refuse the
    /// request happens here, before any state changes.
    fn plan_mutate(
        &self,
        service_id: u64,
        incoming: ServiceCheckpointRecordWire,
    ) -> Result<MutatePlan, i32> {
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
                    Ok(MutatePlan { existing_index, descendants: None, duplicate: true })
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
                || self.service_counts.get(&service_id).copied().unwrap_or(0)
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
        Ok(MutatePlan { existing_index, descendants, duplicate: false })
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
        // The maintained namespace count gives the exact page size without
        // walking the skipped prefix twice. Never reserve the untrusted `max`.
        let count = self.service_counts.get(&service_id).copied().unwrap_or(0)
            .saturating_sub(cursor)
            .min(max);
        let mut records = Vec::new();
        records.try_reserve_exact(count).map_err(|_| ENOMEM)?;
        if count != 0 {
            records.extend(self.records.iter()
                .filter(|record| record.service_id == service_id)
                .skip(cursor)
                .take(count)
                .map(|record| record.wire));
        }
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
        self.compact_journaled(service_id, proof, || Ok(()))
    }

    /// Plan every fallible validation/allocation before the journal append,
    /// then apply that exact mask without allocating another one. The callback
    /// cannot mutate this serialized store between planning and publication.
    pub(crate) fn compact_journaled(
        &mut self,
        service_id: u64,
        proof: ServiceCheckpointRecordWire,
        journal: impl FnOnce() -> Result<(), i32>,
    ) -> Result<bool, i32> {
        let remove = self.plan_compact(service_id, proof)?;
        journal()?;
        Ok(self.apply_compact(service_id, remove))
    }

    fn apply_compact(&mut self, service_id: u64, remove: Option<Vec<bool>>) -> bool {
        let Some(remove) = remove else {
            return true;
        };
        let key_index = &mut self.key_index;
        let old_len = self.records.len();
        let mut cursor = 0;
        let mut retained = 0;
        self.records.retain(|record| {
            let keep = !remove[cursor];
            let key = (record.service_id, record.wire.key_hi, record.wire.key_lo);
            if keep {
                if retained != cursor {
                    *key_index.get_mut(&key).expect("checkpoint index") = retained;
                }
                retained += 1;
            } else {
                key_index.remove(&key);
            }
            cursor += 1;
            keep
        });
        // The plan only selects this namespace. Stable retention preserves
        // parent-before-child order; updating existing nodes avoids allocating
        // a second index after the compaction has already been journaled.
        let count = self.service_counts.get_mut(&service_id).expect("checkpoint count");
        *count -= old_len - retained;
        if *count == 0 {
            self.service_counts.remove(&service_id);
        }
        false
    }

    /// Read-only validation of one compaction. `None` means the key is
    /// already absent. The subtree is collected as a stable per-record bitmap
    /// *before* any mutation, so an allocation failure consumes nothing.
    fn plan_compact(
        &self,
        service_id: u64,
        proof: ServiceCheckpointRecordWire,
    ) -> Result<Option<Vec<bool>>, i32> {
        validate_wire(&proof)?;
        if proof.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE == 0 {
            return Err(EINVAL);
        }
        let Some(index) = self.find_key(service_id, proof.key_hi, proof.key_lo) else {
            return Ok(None);
        };
        if self.records[index].wire != proof {
            return Err(ESTALE);
        }
        let mut remove = self.descendant_mask(service_id, (proof.key_hi, proof.key_lo))?;
        for (i, selected) in remove.iter_mut().enumerate() {
            *selected |= i == index;
            if *selected && self.records[i].wire.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE == 0 {
                return Err(ESTALE);
            }
        }
        Ok(Some(remove))
    }

    /// Unique key lookup. Parent is deliberately *not* part of identity.
    /// Parent links never cross a service namespace.
    fn find_key(&self, service_id: u64, key_hi: u64, key_lo: u64) -> Option<usize> {
        self.key_index.get(&(service_id, key_hi, key_lo)).copied()
    }

    /// A bounded ancestry walk, without recursion or a kernel call. The caller
    /// uses it only for slow checkpoint mutation and compaction, never for IPC
    /// transport or scheduler hot paths.
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
                    .key_index
                    .get(&(service_id, parent.0, parent.1))
                    .copied()
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
        let index = self.find_key(service_id, key_hi, key_lo)?;
        let wire = &self.records[index].wire;
        (wire.parent_hi == parent_hi && wire.parent_lo == parent_lo).then_some(index)
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
    fn compaction_preserves_interleaved_namespaces_and_reused_keys() {
        let mut store = ServiceCheckpointStore::new();
        let parent = record((7, 1), (0, 0), 1, 40);
        let child = record((7, 2), (7, 1), 1, 41);
        let survivor = record((8, 1), (0, 0), 1, 42);
        store.mutate(3, parent).unwrap();
        store.mutate(4, parent).unwrap();
        store.mutate(3, child).unwrap();
        store.mutate(3, survivor).unwrap();
        store.mutate(4, child).unwrap();
        let mut tombstone = parent;
        tombstone.flags = SERVICE_CHECKPOINT_FLAG_TOMBSTONE;
        tombstone.value_len = 0;
        tombstone.value.fill(0);
        tombstone.revision = 2;
        tombstone.operation_lo = 43;
        store.mutate(3, tombstone).unwrap();
        store.compact(3, tombstone).unwrap();
        assert_eq!(store.scan(3, 0, usize::MAX), Ok((vec![survivor], 1)));
        assert_eq!(store.scan(4, 0, usize::MAX), Ok((vec![parent, child], 2)));

        let revised = record((8, 1), (0, 0), 2, 44);
        store.mutate(3, revised).unwrap();
        store.mutate(3, parent).unwrap();
        store.mutate(3, child).unwrap();
        // Removing the foreign tree must use its remapped indices and leave
        // the new incarnation of the same keys in service 3 intact.
        store.mutate(4, tombstone).unwrap();
        store.compact(4, tombstone).unwrap();
        assert_eq!(store.scan(4, 0, usize::MAX), Ok((vec![], 0)));
        assert_eq!(store.scan(3, 1, usize::MAX), Ok((vec![parent, child], 3)));
        assert_eq!(store.mutate(3, revised), Ok(true));
        assert_eq!(store.mutate(3, child), Ok(true));
    }

    #[test]
    fn journaled_compaction_refusal_leaves_the_validated_subtree_intact() {
        let mut store = ServiceCheckpointStore::new();
        let parent = record((3, 1), (0, 0), 1, 50);
        let child = record((3, 2), (3, 1), 1, 51);
        store.mutate(9, parent).unwrap();
        store.mutate(9, child).unwrap();
        let mut tombstone = parent;
        tombstone.flags = SERVICE_CHECKPOINT_FLAG_TOMBSTONE;
        tombstone.value_len = 0;
        tombstone.value.fill(0);
        tombstone.revision = 2;
        tombstone.operation_lo = 52;
        assert_eq!(store.compact_journaled(9, tombstone, || panic!("invalid proof")), Err(ESTALE));
        store.mutate(9, tombstone).unwrap();
        let before = store.scan(9, 0, usize::MAX).unwrap();
        assert_eq!(store.compact_journaled(9, tombstone, || Err(ENOSPC)), Err(ENOSPC));
        assert_eq!(store.scan(9, 0, usize::MAX).unwrap(), before);
        let mut journaled = 0;
        assert_eq!(store.compact_journaled(9, tombstone, || {
            journaled += 1;
            Ok(())
        }), Ok(false));
        assert_eq!(store.compact_journaled(9, tombstone, || {
            journaled += 1;
            Ok(())
        }), Ok(true));
        assert_eq!(journaled, 2, "lost replies retain journaled retry semantics");
        assert!(store.scan(9, 0, usize::MAX).unwrap().0.is_empty());
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

        // The reused key later becomes a tombstone of its own. The old proof
        // is a tombstone too, so only the exact-record comparison (not the
        // tombstone-flag check) can refuse it.
        let mut second = replacement;
        second.flags = SERVICE_CHECKPOINT_FLAG_TOMBSTONE;
        second.value_len = 0;
        second.value.fill(0);
        second.operation_lo = 43;
        second.revision = replacement.revision + 1;
        store.mutate(3, second).unwrap();
        assert_eq!(store.compact(3, tombstone), Err(ESTALE));
        assert_eq!(store.scan(3, 0, 8).unwrap().0, vec![second]);
        assert_eq!(store.compact(3, second), Ok(false));
        assert!(store.scan(3, 0, 8).unwrap().0.is_empty());
    }

    #[test]
    fn preflight_is_a_read_only_dry_run_that_agrees_with_the_commit() {
        let mut store = ServiceCheckpointStore::new();
        let root = record((1, 1), (0, 0), 1, 10);
        let child = record((2, 2), (1, 1), 1, 11);
        // Accepted dry-runs change nothing.
        assert!(store.preflight_mutate(3, root).is_ok());
        assert!(store.scan(3, 0, 8).unwrap().0.is_empty());
        store.mutate(3, root).unwrap();
        // A child of an absent parent is refused by both phases.
        assert!(store.preflight_mutate(3, child.clone()).is_ok());
        let orphan = record((9, 9), (8, 8), 1, 12);
        assert_eq!(store.preflight_mutate(3, orphan).err(), Some(ESTALE));
        assert_eq!(store.mutate(3, orphan), Err(ESTALE));
        store.mutate(3, child).unwrap();
        // Exact replay is accepted (idempotent) by the dry-run too.
        assert!(store.preflight_mutate(3, child).is_ok());
        // Tombstone dry-run does not touch descendants.
        let mut tomb = root;
        tomb.flags = SERVICE_CHECKPOINT_FLAG_TOMBSTONE;
        tomb.value_len = 0;
        tomb.value.fill(0);
        tomb.operation_lo = 20;
        tomb.revision = 2;
        assert!(store.preflight_mutate(3, tomb).is_ok());
        assert_eq!(store.scan(3, 0, 8).unwrap().0, vec![root, child]);
        assert_eq!(store.preflight_compact(3, tomb), Err(ESTALE));
        store.mutate(3, tomb).unwrap();
        assert_eq!(store.preflight_compact(3, tomb), Ok(()));
        assert_eq!(store.scan(3, 0, 8).unwrap().0.len(), 2);
        assert_eq!(store.compact(3, tomb), Ok(false));
        assert!(store.scan(3, 0, 8).unwrap().0.is_empty());
    }

    fn open_shape() -> Vec<ServiceCheckpointRecordWire> {
        // staging parent, two path children, final revision of the parent.
        vec![
            record((1, 1), (0, 0), 1, 10),
            record((1, 2), (1, 1), 1, 11),
            record((1, 3), (1, 1), 1, 12),
            record((1, 1), (0, 0), 2, 13),
        ]
    }

    #[test]
    fn batch_commits_in_order_through_one_journal_stream() {
        let mut store = ServiceCheckpointStore::new();
        let batch = open_shape();
        let mut journal = Vec::new();
        let commit = store
            .mutate_batch(3, &batch, |record| {
                journal.push(record);
                Ok(())
            })
            .unwrap();
        assert_eq!(commit, BatchCommit { applied: 4, duplicates: 0 });
        // Journal order is exactly the request order, so replay is identical
        // to four single MUTATE calls.
        assert_eq!(journal, batch);
        let stored = store.scan(3, 0, 8).unwrap().0;
        assert_eq!(stored.len(), 3);
        assert_eq!(stored[0], batch[3]);
    }

    #[test]
    fn batch_failure_keeps_the_committed_prefix_and_touches_nothing_after() {
        let mut store = ServiceCheckpointStore::new();
        let mut batch = open_shape();
        batch[2] = record((1, 3), (7, 7), 1, 12); // child of an absent parent
        let mut journal = Vec::new();
        let failure = store
            .mutate_batch(3, &batch, |record| {
                journal.push(record);
                Ok(())
            })
            .unwrap_err();
        assert_eq!(
            failure,
            BatchFailure { errno: ESTALE, after_journal: false, applied: 2, duplicates: 0 }
        );
        assert_eq!(journal, batch[..2].to_vec());
        assert_eq!(store.scan(3, 0, 8).unwrap().0, batch[..2].to_vec());
    }

    #[test]
    fn batch_journal_refusal_does_not_apply_that_record() {
        let mut store = ServiceCheckpointStore::new();
        let batch = open_shape();
        let mut calls = 0;
        let failure = store
            .mutate_batch(3, &batch, |_| {
                calls += 1;
                if calls == 2 { Err(ENOSPC) } else { Ok(()) }
            })
            .unwrap_err();
        assert_eq!(failure.errno, ENOSPC);
        assert_eq!((failure.applied, failure.after_journal), (1, false));
        assert_eq!(store.scan(3, 0, 8).unwrap().0, batch[..1].to_vec());
    }

    #[test]
    fn replayed_batch_after_lost_reply_is_idempotent_and_not_rejournaled() {
        let mut store = ServiceCheckpointStore::new();
        let batch = open_shape();
        store.mutate_batch(3, &batch, |_| Ok(())).unwrap();
        let mut journal = Vec::new();
        let commit = store
            .mutate_batch(3, &batch, |record| {
                journal.push(record);
                Ok(())
            })
            .unwrap();
        // Staging (rev 1) is superseded by the final record the store already
        // holds verbatim; the rest are exact duplicates.
        assert_eq!(commit, BatchCommit { applied: 4, duplicates: 4 });
        assert_eq!(journal.len(), 3); // exact duplicates are still journaled, as for single MUTATE
        assert_eq!(store.scan(3, 0, 8).unwrap().0.len(), 3);
    }

    #[test]
    fn superseded_revision_is_not_accepted_without_a_committed_later_record() {
        let mut store = ServiceCheckpointStore::new();
        let batch = open_shape();
        store.mutate_batch(3, &batch, |_| Ok(())).unwrap();
        // A stale rev-1 record alone (no later committed record in the batch)
        // remains a hard refusal.
        let stale = [batch[0]];
        let failure = store.mutate_batch(3, &stale, |_| Ok(())).unwrap_err();
        assert_eq!((failure.errno, failure.applied), (ESTALE, 0));
        // A "later" record that is not what the store holds does not excuse it.
        let mut forged = batch[3];
        forged.operation_lo = 99;
        let failure = store
            .mutate_batch(3, &[batch[0], forged], |_| Ok(()))
            .unwrap_err();
        assert_eq!((failure.errno, failure.applied), (ESTALE, 0));
    }
}
