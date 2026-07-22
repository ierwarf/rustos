use alloc::vec::Vec;
use rustos_user_abi::syscall::{
    ServiceCheckpointRecordWire, SERVICE_CHECKPOINT_ABI_VERSION, SERVICE_CHECKPOINT_FLAG_TOMBSTONE,
    SERVICE_CHECKPOINT_MAX_RECORDS,
};

const EINVAL: i32 = 22;
const ENOSPC: i32 = 28;
const EPROTO: i32 = 71;
const ESTALE: i32 = 116;

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
}

impl ServiceCheckpointStore {
    pub(crate) fn new() -> Self {
        Self {
            records: Vec::with_capacity(SERVICE_CHECKPOINT_MAX_RECORDS),
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
            if self.records.len() == SERVICE_CHECKPOINT_MAX_RECORDS {
                return Err(ENOSPC);
            }
        }

        if incoming.parent_hi != 0 || incoming.parent_lo != 0 {
            let parent = self
                .find(service_id, 0, 0, incoming.parent_hi, incoming.parent_lo)
                .map(|index| self.records[index].wire)
                .ok_or(ESTALE)?;
            if parent.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE != 0 {
                return Err(ESTALE);
            }
        }

        if incoming.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE != 0 {
            for record in self.records.iter().filter(|record| {
                record.service_id == service_id
                    && record.wire.parent_hi == incoming.key_hi
                    && record.wire.parent_lo == incoming.key_lo
                    && record.wire.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE == 0
            }) {
                record.wire.revision.checked_add(1).ok_or(ESTALE)?;
            }
        }

        match existing_index {
            Some(index) => self.records[index].wire = incoming,
            None => self.records.push(StoredRecord {
                service_id,
                wire: incoming,
            }),
        }

        if incoming.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE != 0 {
            for child in self.records.iter_mut().filter(|record| {
                record.service_id == service_id
                    && record.wire.parent_hi == incoming.key_hi
                    && record.wire.parent_lo == incoming.key_lo
                    && record.wire.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE == 0
            }) {
                child.wire.flags = SERVICE_CHECKPOINT_FLAG_TOMBSTONE;
                child.wire.value_len = 0;
                child.wire.value.fill(0);
                child.wire.operation_hi = incoming.operation_hi;
                child.wire.operation_lo = incoming.operation_lo;
                child.wire.revision += 1;
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
        let records = self
            .records
            .iter()
            .filter(|record| record.service_id == service_id)
            .skip(cursor)
            .take(max)
            .map(|record| record.wire)
            .collect::<Vec<_>>();
        let next = cursor.saturating_add(records.len());
        Ok((records, next))
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
}
