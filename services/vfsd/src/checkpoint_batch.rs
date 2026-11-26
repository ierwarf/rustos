//! Ordered, batched rootd checkpoint commits for vfsd.
//!
//! An OPEN publishes a staging record, one record per 64-byte path chunk and a
//! final handle record. Those used to be separate rootd IPCs; this module
//! sends them as `SERVICE_CHECKPOINT_MUTATE_BATCH` requests instead.
//!
//! Semantics are unchanged from N single MUTATE calls: rootd preflights,
//! journals and applies the records strictly in order, so the DAG order
//! (staging parent -> path children -> final revision) is preserved and a
//! crash between records leaves the same recoverable staging prefix. The
//! reply reports the committed prefix, and vfsd's mirror advances by exactly
//! that prefix, so a failed batch never leaves vfsd believing a record
//! committed that did not (nor forgetting one that did when rootd says so).
//! The only unknown is a transport failure, which reports nothing; vfsd then
//! publishes nothing and the exact retry is idempotent by operation id.
//!
//! Lives in the library target because the binary's test harness is disabled.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use rustos_svc_runtime::checkpoint_idl;
use rustos_user_abi::syscall::{
    ServiceCheckpointRecordWire, COMMERCIAL_MAX_PROTOCOL_PAYLOAD_CAPACITY,
    SERVICE_CHECKPOINT_BATCH_MAX_RECORDS, SERVICE_CHECKPOINT_VALUE_CAPACITY,
};

use crate::{checkpoint_path_key, VFSD_CHECKPOINT_HANDLE_TAG};

const EIO: i32 = 5;
const EINVAL: i32 = 22;
const EOVERFLOW: i32 = 75;

/// `(parent_hi, parent_lo, key_hi, key_lo)`.
pub type CheckpointKey = (u64, u64, u64, u64);

pub fn checkpoint_key(record: &ServiceCheckpointRecordWire) -> CheckpointKey {
    (
        record.parent_hi,
        record.parent_lo,
        record.key_hi,
        record.key_lo,
    )
}

/// One logical mutation: the record body (revision and operation are filled in
/// by [`commit_ordered`]) and the operation id that makes its retry idempotent.
#[derive(Clone, Copy, Debug)]
pub struct BatchItem {
    pub record: ServiceCheckpointRecordWire,
    pub operation_hi: u64,
    pub operation_lo: u64,
}

/// Rootd's reply to one MUTATE_BATCH. `applied` is the committed prefix length
/// and is meaningful even when `status != 0`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BatchReply {
    pub status: i32,
    pub applied: u64,
    pub duplicates: u64,
}

pub trait CheckpointTransport {
    /// Send `count` framed records in `payload` as ONE rootd request. `Err` is
    /// a transport failure with no information about what rootd committed.
    fn mutate_batch(&mut self, count: usize, payload: &[u8]) -> Result<BatchReply, i32>;
}

/// vfsd's view of what rootd has acknowledged.
pub trait CheckpointMirror {
    fn current(&self, key: CheckpointKey) -> Option<ServiceCheckpointRecordWire>;
    fn publish(&mut self, record: ServiceCheckpointRecordWire);
}

/// Record for one path chunk, a child of the open-description handle record.
pub fn checkpoint_path_record(
    remote_id: u64,
    chunk_index: usize,
    bytes: &[u8],
) -> Result<ServiceCheckpointRecordWire, i32> {
    if bytes.is_empty() || bytes.len() > SERVICE_CHECKPOINT_VALUE_CAPACITY {
        return Err(EINVAL);
    }
    let (key_hi, key_lo) = checkpoint_path_key(remote_id, chunk_index).ok_or(EOVERFLOW)?;
    let mut record = ServiceCheckpointRecordWire {
        key_hi,
        key_lo,
        parent_hi: remote_id,
        parent_lo: VFSD_CHECKPOINT_HANDLE_TAG,
        value_len: bytes.len() as u32,
        ..ServiceCheckpointRecordWire::default()
    };
    record.value[..bytes.len()].copy_from_slice(bytes);
    Ok(record)
}

/// The ordered OPEN publication: staging parent, every path chunk, final
/// handle record. `suboperation(n)` yields the operation id of step `n`
/// (0 = staging, 1..=chunks = path, chunks+1 = final).
pub fn open_description_items(
    remote_id: u64,
    path: &[u8],
    staging: ServiceCheckpointRecordWire,
    final_record: ServiceCheckpointRecordWire,
    suboperation: impl Fn(u64) -> (u64, u64),
) -> Result<Vec<BatchItem>, i32> {
    if path.is_empty() {
        return Err(EINVAL);
    }
    let chunk_count = path.len().div_ceil(SERVICE_CHECKPOINT_VALUE_CAPACITY);
    let mut items = Vec::new();
    items.try_reserve_exact(chunk_count + 2).map_err(|_| EIO)?;
    let step = |ordinal: u64, record| {
        let (operation_hi, operation_lo) = suboperation(ordinal);
        BatchItem {
            record,
            operation_hi,
            operation_lo,
        }
    };
    items.push(step(0, staging));
    for chunk_index in 0..chunk_count {
        let start = chunk_index * SERVICE_CHECKPOINT_VALUE_CAPACITY;
        let end = (start + SERVICE_CHECKPOINT_VALUE_CAPACITY).min(path.len());
        let record = checkpoint_path_record(remote_id, chunk_index, &path[start..end])?;
        items.push(step(chunk_index as u64 + 1, record));
    }
    items.push(step(chunk_count as u64 + 1, final_record));
    Ok(items)
}

/// Assign revisions to `items` in order, send them to rootd in as few batches
/// as the request payload allows, and advance `mirror` by exactly the prefix
/// rootd reports committed.
///
/// A record whose key already carries the same operation id in `mirror` (or
/// earlier in this call) is an exact replay: it must be identical and is not
/// resent. Later records see earlier ones' revisions, so a staging record and
/// the final record sharing one key get revisions n and n+1 in order.
pub fn commit_ordered<M: CheckpointMirror, T: CheckpointTransport>(
    mirror: &mut M,
    transport: &mut T,
    items: &[BatchItem],
) -> Result<(), i32> {
    let mut overlay: BTreeMap<CheckpointKey, ServiceCheckpointRecordWire> = BTreeMap::new();
    let mut pending: Vec<ServiceCheckpointRecordWire> = Vec::new();
    pending.try_reserve(items.len()).map_err(|_| EIO)?;
    for item in items {
        let mut record = item.record;
        let key = checkpoint_key(&record);
        let current = overlay.get(&key).copied().or_else(|| mirror.current(key));
        if let Some(current) = current {
            if (current.operation_hi, current.operation_lo)
                == (item.operation_hi, item.operation_lo)
            {
                record.revision = current.revision;
                record.operation_hi = item.operation_hi;
                record.operation_lo = item.operation_lo;
                if record == current {
                    continue;
                }
                return Err(EIO);
            }
        }
        record.revision = current
            .map_or(0, |current| current.revision)
            .checked_add(1)
            .ok_or(EOVERFLOW)?;
        record.operation_hi = item.operation_hi;
        record.operation_lo = item.operation_lo;
        overlay.insert(key, record);
        pending.push(record);
    }

    let mut rest = pending.as_slice();
    let mut framed = Vec::new();
    while !rest.is_empty() {
        framed.clear();
        let mut count = 0;
        for record in rest.iter().take(SERVICE_CHECKPOINT_BATCH_MAX_RECORDS) {
            let before = framed.len();
            checkpoint_idl::append_framed(&mut framed, record).map_err(|_| EIO)?;
            if framed.len() > COMMERCIAL_MAX_PROTOCOL_PAYLOAD_CAPACITY {
                framed.truncate(before);
                break;
            }
            count += 1;
        }
        if count == 0 {
            return Err(EIO);
        }
        let (chunk, tail) = rest.split_at(count);
        let reply = transport.mutate_batch(count, &framed)?;
        let applied = usize::try_from(reply.applied).map_err(|_| EIO)?;
        if applied > count || (reply.status == 0 && applied != count) {
            // Protocol violation: trust nothing, publish nothing. The exact
            // retry is idempotent by operation id.
            return Err(EIO);
        }
        for record in &chunk[..applied] {
            mirror.publish(*record);
        }
        if reply.status != 0 {
            return Err(reply.status);
        }
        rest = tail;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{VFSD_OPEN_MUTATION_OPEN, VFSD_OPEN_MUTATION_STAGING};
    use rustos_user_abi::syscall::SERVICE_CHECKPOINT_FLAG_TOMBSTONE;

    const ESTALE: i32 = 116;

    #[derive(Default)]
    struct Mirror(BTreeMap<CheckpointKey, ServiceCheckpointRecordWire>);
    impl CheckpointMirror for Mirror {
        fn current(&self, key: CheckpointKey) -> Option<ServiceCheckpointRecordWire> {
            self.0.get(&key).copied()
        }
        fn publish(&mut self, record: ServiceCheckpointRecordWire) {
            self.0.insert(checkpoint_key(&record), record);
        }
    }

    /// Mimics rootd's in-order `mutate_batch`: per-record DAG preflight against
    /// the state left by its predecessors, stop at the first refusal.
    #[derive(Default)]
    struct FakeRootd {
        calls: usize,
        store: BTreeMap<(u64, u64), ServiceCheckpointRecordWire>,
        journal: Vec<(u64, u64, u64)>,
        fail_at_record: Option<usize>,
        seen: usize,
        lose_reply: bool,
    }
    impl CheckpointTransport for FakeRootd {
        fn mutate_batch(&mut self, count: usize, payload: &[u8]) -> Result<BatchReply, i32> {
            self.calls += 1;
            let records = checkpoint_idl::decode_framed(payload).unwrap();
            assert_eq!(records.len(), count);
            assert!(count <= SERVICE_CHECKPOINT_BATCH_MAX_RECORDS);
            let mut reply = BatchReply {
                status: 0,
                applied: 0,
                duplicates: 0,
            };
            for (index, record) in records.iter().copied().enumerate() {
                // Mirror of rootd's supersession rule: an older revision of a
                // key that a later record of this batch already holds verbatim.
                let key = (record.key_hi, record.key_lo);
                if self.store.get(&key).is_some_and(|cur| {
                    (cur.operation_hi, cur.operation_lo)
                        != (record.operation_hi, record.operation_lo)
                        && record.revision <= cur.revision
                }) && records[index + 1..].iter().any(|next| {
                    (next.key_hi, next.key_lo) == key && self.store.get(&key) == Some(next)
                }) {
                    reply.applied += 1;
                    reply.duplicates += 1;
                    continue;
                }
                self.seen += 1;
                if Some(self.seen - 1) == self.fail_at_record {
                    reply.status = ESTALE;
                    break;
                }
                let key = (record.key_hi, record.key_lo);
                match self.store.get(&key) {
                    Some(cur)
                        if (cur.operation_hi, cur.operation_lo)
                            == (record.operation_hi, record.operation_lo) =>
                    {
                        assert_eq!(*cur, record);
                        reply.duplicates += 1;
                    }
                    Some(cur) if record.revision != cur.revision + 1 => {
                        reply.status = ESTALE;
                        break;
                    }
                    None if record.revision != 1 => {
                        reply.status = ESTALE;
                        break;
                    }
                    _ => {}
                }
                if (record.parent_hi, record.parent_lo) != (0, 0) {
                    let parent = self.store.get(&(record.parent_hi, record.parent_lo));
                    if parent.is_none_or(|p| p.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE != 0) {
                        reply.status = ESTALE;
                        break;
                    }
                }
                self.journal
                    .push((record.key_hi, record.key_lo, record.revision));
                self.store.insert(key, record);
                reply.applied += 1;
            }
            if self.lose_reply {
                self.lose_reply = false;
                return Err(EIO);
            }
            Ok(reply)
        }
    }

    fn handle(remote_id: u64, last_mutation: u16) -> ServiceCheckpointRecordWire {
        let mut record = ServiceCheckpointRecordWire {
            key_hi: remote_id,
            key_lo: VFSD_CHECKPOINT_HANDLE_TAG,
            value_len: 2,
            ..ServiceCheckpointRecordWire::default()
        };
        record.value[..2].copy_from_slice(&last_mutation.to_le_bytes());
        record
    }

    fn open_items(path_len: usize) -> Vec<BatchItem> {
        let path = alloc::vec![b'a'; path_len];
        open_description_items(
            7,
            &path,
            handle(7, VFSD_OPEN_MUTATION_STAGING),
            handle(7, VFSD_OPEN_MUTATION_OPEN),
            |n| (0xAB00 + n, 0xCD00 + n),
        )
        .unwrap()
    }

    #[test]
    fn open_with_path_chunks_is_one_rootd_call_not_n() {
        // 200 bytes -> 4 chunks -> staging + 4 + final = 6 records.
        let items = open_items(200);
        assert_eq!(items.len(), 6);
        let (mut mirror, mut rootd) = (Mirror::default(), FakeRootd::default());
        commit_ordered(&mut mirror, &mut rootd, &items).unwrap();
        assert_eq!(rootd.calls, 1);
        assert_eq!(rootd.journal.len(), 6);
    }

    #[test]
    fn maximum_path_open_still_fits_one_request() {
        // VFS_IPC_PATH_CAPACITY (512) -> 8 chunks -> 10 records.
        let items = open_items(512);
        assert_eq!(items.len(), 10);
        let (mut mirror, mut rootd) = (Mirror::default(), FakeRootd::default());
        commit_ordered(&mut mirror, &mut rootd, &items).unwrap();
        assert_eq!(rootd.calls, 1);
    }

    #[test]
    fn dag_order_and_revisions_match_sequential_single_calls() {
        let items = open_items(130); // 3 chunks
        let (mut mirror, mut rootd) = (Mirror::default(), FakeRootd::default());
        commit_ordered(&mut mirror, &mut rootd, &items).unwrap();
        let handle_key = (7, VFSD_CHECKPOINT_HANDLE_TAG);
        let order: Vec<_> = rootd.journal.clone();
        // staging (rev 1) first, then chunks 0..3, then the same key at rev 2.
        assert_eq!(order[0], (handle_key.0, handle_key.1, 1));
        for (i, entry) in order[1..4].iter().enumerate() {
            assert_eq!((entry.0, entry.1), checkpoint_path_key(7, i).unwrap());
            assert_eq!(entry.2, 1);
        }
        assert_eq!(order[4], (handle_key.0, handle_key.1, 2));
        assert_eq!(mirror.0[&(0, 0, 7, VFSD_CHECKPOINT_HANDLE_TAG)].revision, 2);
    }

    #[test]
    fn rootd_refusal_commits_exactly_the_prefix_and_retry_resumes() {
        let items = open_items(200); // staging, 4 chunks, final
        let (mut mirror, mut rootd) = (Mirror::default(), FakeRootd::default());
        rootd.fail_at_record = Some(3); // refuse the third path chunk
        assert_eq!(commit_ordered(&mut mirror, &mut rootd, &items), Err(ESTALE));
        // staging + chunk0 + chunk1 committed in rootd AND acknowledged in vfsd;
        // nothing after the failing record was touched.
        assert_eq!(rootd.journal.len(), 3);
        assert_eq!(mirror.0.len(), 3);
        assert_eq!(rootd.calls, 1);
        // The exact retry skips the committed prefix (no resend) and finishes.
        commit_ordered(&mut mirror, &mut rootd, &items).unwrap();
        assert_eq!(rootd.calls, 2);
        assert_eq!(rootd.journal.len(), 6);
        assert_eq!(mirror.0[&(0, 0, 7, VFSD_CHECKPOINT_HANDLE_TAG)].revision, 2);
    }

    #[test]
    fn lost_reply_publishes_nothing_and_retry_is_idempotent() {
        let items = open_items(70);
        let (mut mirror, mut rootd) = (Mirror::default(), FakeRootd::default());
        rootd.lose_reply = true;
        assert_eq!(commit_ordered(&mut mirror, &mut rootd, &items), Err(EIO));
        assert!(mirror.0.is_empty());
        assert_eq!(rootd.journal.len(), 4); // rootd did commit them
        commit_ordered(&mut mirror, &mut rootd, &items).unwrap();
        // Staging is skipped as superseded; the rest are exact duplicates (which
        // rootd journals exactly as it does for single MUTATE). Same state.
        assert_eq!(rootd.store.len(), 3);
        assert_eq!(rootd.store[&(7, VFSD_CHECKPOINT_HANDLE_TAG)].revision, 2);
        assert_eq!(mirror.0.len(), 3);
        assert_eq!(mirror.0[&(0, 0, 7, VFSD_CHECKPOINT_HANDLE_TAG)].revision, 2);
    }

    #[test]
    fn status_zero_with_short_prefix_is_a_protocol_error() {
        struct Short;
        impl CheckpointTransport for Short {
            fn mutate_batch(&mut self, count: usize, _: &[u8]) -> Result<BatchReply, i32> {
                Ok(BatchReply {
                    status: 0,
                    applied: count as u64 - 1,
                    duplicates: 0,
                })
            }
        }
        let mut mirror = Mirror::default();
        assert_eq!(
            commit_ordered(&mut mirror, &mut Short, &open_items(10)),
            Err(EIO)
        );
        assert!(mirror.0.is_empty());
    }

    #[test]
    fn same_operation_with_different_body_is_rejected_locally() {
        let items = open_items(10);
        let (mut mirror, mut rootd) = (Mirror::default(), FakeRootd::default());
        commit_ordered(&mut mirror, &mut rootd, &items).unwrap();
        let mut forged = items.clone();
        // Staging key already holds the *final* record body from this op id.
        forged[0].record.value[10] ^= 1;
        forged[0].operation_hi = items[2].operation_hi;
        forged[0].operation_lo = items[2].operation_lo;
        assert_eq!(commit_ordered(&mut mirror, &mut rootd, &forged), Err(EIO));
    }
}
