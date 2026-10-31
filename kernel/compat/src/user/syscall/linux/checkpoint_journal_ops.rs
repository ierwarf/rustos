//! Kernel-resident, rootd-only checkpoint operation journal.
//!
//! Ring0 is only the bounded storage and exact-once sequence substrate:
//! checkpoint format, parenting, compaction and replay policy remain in rootd.
//! It survives rootd process death but NOT reboot/power loss. The journal
//! adds no work to syscalls other than this explicit slow management call.
use super::*;
use core::mem::size_of;
use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};
use rustos_user_abi::syscall::{
    RustosCheckpointJournalArgs, ServiceCheckpointRecordWire,
    CHECKPOINT_JOURNAL_ABI_VERSION, CHECKPOINT_JOURNAL_OP_APPEND,
    CHECKPOINT_JOURNAL_OP_READ, CHECKPOINT_JOURNAL_RECORD_COMPACT,
    CHECKPOINT_JOURNAL_RECORD_MUTATE, SERVICE_CHECKPOINT_ABI_VERSION,
    IPC_SERVICE_CAP_ROOT_SUPERVISOR,
};

const MAX_JOURNAL_ENTRIES: usize = 8_192;
const ENOSPC: i64 = 28;
const EPROTO: i64 = 71;

#[derive(Clone, Copy)]
struct JournalEntry {
    service: u64,
    kind: u16,
    record: ServiceCheckpointRecordWire,
}

struct Journal {
    entries: [Option<JournalEntry>; MAX_JOURNAL_ENTRIES],
    len: usize,
}

impl Journal {
    const fn new() -> Self {
        Self { entries: [const { None }; MAX_JOURNAL_ENTRIES], len: 0 }
    }

    fn append(&mut self, service: u64, kind: u16, record: ServiceCheckpointRecordWire)
        -> Result<u64, i64>
    {
        // Untrusted user records never change a previously published journal
        // operation. A duplicate retry after copyout failure is idempotent.
        if let Some((position, older)) = self.entries[..self.len].iter()
            .enumerate()
            .filter_map(|(position, value)| value.as_ref().map(|entry| (position, entry)))
            .find(|(_, entry)| entry.service == service
                && entry.kind == kind
                && entry.record.operation_hi == record.operation_hi
                && entry.record.operation_lo == record.operation_lo)
        {
            return if older.record == record {
                Ok((position + 1) as u64)
            } else {
                Err(EPROTO)
            };
        }
        if self.len >= self.entries.len() {
            return Err(ENOSPC);
        }
        self.entries[self.len] = Some(JournalEntry { service, kind, record });
        self.len += 1;
        Ok(self.len as u64)
    }

    fn read(&self, cursor: u64) -> Result<Option<(u64, JournalEntry)>, i64> {
        let index = usize::try_from(cursor).map_err(|_| LINUX_EINVAL)?;
        if index > self.len {
            return Err(LINUX_EINVAL);
        }
        if index == self.len {
            return Ok(None);
        }
        self.entries[index].map(|record| ((index + 1) as u64, record))
            .ok_or(EPROTO)
            .map(Some)
    }
}

static JOURNAL: TrackedSpinLock<Journal, { LockClass::ProcBrokerRegistry as u8 }> =
    TrackedSpinLock::new(Journal::new());

fn valid_request(args: &RustosCheckpointJournalArgs) -> bool {
    if args.abi_version != CHECKPOINT_JOURNAL_ABI_VERSION
        || args.flags != 0 || args.reserved != 0
        || args.next_cursor != 0 || args.done != 0
    {
        return false;
    }
    match args.op {
        CHECKPOINT_JOURNAL_OP_APPEND => {
            args.cursor == 0 && args.service_id != 0
                && matches!(args.record_kind,
                    CHECKPOINT_JOURNAL_RECORD_MUTATE | CHECKPOINT_JOURNAL_RECORD_COMPACT)
                && args.record.version == SERVICE_CHECKPOINT_ABI_VERSION
                && args.record.revision != 0
                && (args.record.key_hi != 0 || args.record.key_lo != 0)
                && args.record.value_len as usize <= args.record.value.len()
                && (args.record.operation_hi != 0 || args.record.operation_lo != 0)
        }
        CHECKPOINT_JOURNAL_OP_READ => {
            args.service_id == 0 && args.record_kind == 0
                && args.record == ServiceCheckpointRecordWire::default()
        }
        _ => false,
    }
}

pub(super) fn syscall_linux_rustos_checkpoint_journal(ptr: u64) -> u64 {
    if !ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_ROOT_SUPERVISOR) {
        return linux_errno(LINUX_EPERM);
    }
    let mut args = match usermem::read_current_user_struct::<RustosCheckpointJournalArgs>(ptr) {
        Ok(args) => args,
        Err(error) => return linux_errno(address_space_error_to_linux_errno(error)),
    };
    if !valid_request(&args) {
        return linux_errno(LINUX_EINVAL);
    }
    // Preflight copyout before any irreversible append. A subsequent copyout
    // failure can be resolved through the exact operation-ID duplicate lookup.
    if let Err(error) = usermem::validate_current_user_write_buffer(
        ptr, size_of::<RustosCheckpointJournalArgs>(),
    ) {
        return linux_errno(address_space_error_to_linux_errno(error));
    }
    let outcome = {
        let mut journal = JOURNAL.lock();
        match args.op {
            CHECKPOINT_JOURNAL_OP_APPEND => {
                let next = journal.append(args.service_id, args.record_kind, args.record);
                next.map(|next_cursor| {
                    args.next_cursor = next_cursor;
                    args.done = 0;
                })
            }
            CHECKPOINT_JOURNAL_OP_READ => {
                match journal.read(args.cursor) {
                    Ok(Some((next_cursor, record))) => {
                        args.next_cursor = next_cursor;
                        args.service_id = record.service;
                        args.record_kind = record.kind;
                        args.record = record.record;
                        args.done = 0;
                        Ok(())
                    }
                    Ok(None) => {
                        args.next_cursor = args.cursor;
                        args.done = 1;
                        Ok(())
                    }
                    Err(errno) => Err(errno),
                }
            }
            _ => Err(LINUX_EINVAL),
        }
    };
    if let Err(errno) = outcome {
        return linux_errno(errno);
    }
    match usermem::write_current_user_struct(ptr, &args) {
        Ok(()) => 0,
        Err(error) => linux_errno(address_space_error_to_linux_errno(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_append_replay_and_conflicting_duplicate() {
        let mut log = Journal::new();
        let mut record = ServiceCheckpointRecordWire::default();
        record.key_hi = 1;
        record.operation_hi = 8;
        record.revision = 1;
        assert_eq!(log.append(2, CHECKPOINT_JOURNAL_RECORD_MUTATE, record), Ok(1));
        assert_eq!(log.append(2, CHECKPOINT_JOURNAL_RECORD_MUTATE, record), Ok(1));
        let (_, first) = log.read(0).unwrap().unwrap();
        assert_eq!(first.record, record);
        assert_eq!(log.read(1), Ok(None));
        record.revision = 2;
        assert_eq!(log.append(2, CHECKPOINT_JOURNAL_RECORD_MUTATE, record), Err(EPROTO));
    }
}
