//! Rootd replay bridge for kernel-resident checkpoint operations.
//!
//! Every mutation accepted by the in-memory DAG is appended before the
//! service receives its ACK. On rootd process restart the same operation log
//! is replayed into a fresh DAG; stale or conflicting operations fail closed.
//! This journal is *not* disk-durable, and retaining it forever is bounded:
//! ENOSPC stops future mutations instead of silently forgetting history.
use super::service_checkpoint::ServiceCheckpointStore;
use rustos_user_abi::syscall::{
    RustosCheckpointJournalArgs, ServiceCheckpointRecordWire,
    CHECKPOINT_JOURNAL_ABI_VERSION, CHECKPOINT_JOURNAL_OP_APPEND,
    CHECKPOINT_JOURNAL_OP_READ, CHECKPOINT_JOURNAL_RECORD_MUTATE,
    CHECKPOINT_JOURNAL_RECORD_COMPACT, SYS_RUSTOS_CHECKPOINT_JOURNAL,
};

fn syscall(mut args: RustosCheckpointJournalArgs)
    -> Result<RustosCheckpointJournalArgs, i32>
{
    let status = super::syscall1(
        SYS_RUSTOS_CHECKPOINT_JOURNAL,
        &mut args as *mut RustosCheckpointJournalArgs as u64,
    );
    if status < 0 {
        Err((-status) as i32)
    } else {
        Ok(args)
    }
}

pub(super) fn append(
    service: u64,
    kind: u16,
    record: ServiceCheckpointRecordWire,
) -> Result<u64, i32> {
    if service == 0
        || !matches!(kind, CHECKPOINT_JOURNAL_RECORD_MUTATE | CHECKPOINT_JOURNAL_RECORD_COMPACT)
    {
        return Err(22);
    }
    let reply = syscall(RustosCheckpointJournalArgs {
        abi_version: CHECKPOINT_JOURNAL_ABI_VERSION,
        op: CHECKPOINT_JOURNAL_OP_APPEND,
        service_id: service,
        record_kind: kind,
        record,
        ..RustosCheckpointJournalArgs::default()
    })?;
    if reply.service_id != service || reply.record_kind != kind
        || reply.record != record || reply.next_cursor == 0 || reply.done != 0
    {
        return Err(71);
    }
    Ok(reply.next_cursor)
}

pub(super) fn replay(store: &mut ServiceCheckpointStore) -> Result<u64, i32> {
    let mut cursor = 0_u64;
    loop {
        let reply = syscall(RustosCheckpointJournalArgs {
            abi_version: CHECKPOINT_JOURNAL_ABI_VERSION,
            op: CHECKPOINT_JOURNAL_OP_READ,
            cursor,
            ..RustosCheckpointJournalArgs::default()
        })?;
        if reply.done != 0 {
            if reply.done != 1 || reply.next_cursor != cursor {
                return Err(71);
            }
            return Ok(cursor);
        }
        if reply.next_cursor != cursor.checked_add(1).ok_or(75)?
            || reply.service_id == 0
        {
            return Err(71);
        }
        match reply.record_kind {
            CHECKPOINT_JOURNAL_RECORD_MUTATE => {
                store.mutate(reply.service_id, reply.record)?;
            }
            CHECKPOINT_JOURNAL_RECORD_COMPACT => {
                store.compact(reply.service_id, reply.record)?;
            }
            _ => return Err(71),
        };
        cursor = reply.next_cursor;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_journal_contract_has_distinct_mutation_and_compaction_kinds() {
        assert_ne!(CHECKPOINT_JOURNAL_RECORD_MUTATE, CHECKPOINT_JOURNAL_RECORD_COMPACT);
    }
}
