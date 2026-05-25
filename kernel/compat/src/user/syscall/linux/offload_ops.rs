use alloc::vec::Vec;

use lazy_static::lazy_static;
use rustos_user_abi::syscall::{
    LIFECYCLE_DRAIN_MAX_EVENTS, LIFECYCLE_EVENT_EXIT, LifecycleDrainBrokerArgs, LifecycleEventWire,
    VFS_IPC_OP_PREAD64, VFS_IPC_PAYLOAD_CAPACITY,
};
use spin::Mutex;

use super::*;

// RING3-MIGRATION-COMMENTED-OUT START: remote VFS read path, process-exit
// recording, and lifecycle event drain are syscalld/procd policy. Ring0 keeps
// only the IPC transport substrate.
/*
pub(crate) fn call_remote_vfs_read_bytes(
    remote_id: u64,
    offset: u64,
    len: usize,
) -> Result<Vec<u8>, i64> {
    let mut bytes = Vec::new();
    while bytes.len() < len {
        let chunk_len = (len - bytes.len()).min(VFS_IPC_PAYLOAD_CAPACITY);
        let mut request = new_vfs_request(VFS_IPC_OP_PREAD64);
        request.remote_id = remote_id;
        request.arg0 = offset.saturating_add(bytes.len() as u64);
        request.arg1 = chunk_len as u64;
        let response = call_vfs_ipc_request(&request)?;
        ensure_vfs_status(&response)?;
        let read = response.payload_len as usize;
        if read > chunk_len || read > response.payload.len() {
            return Err(LINUX_EINVAL);
        }
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&response.payload[..read]);
        if read < chunk_len {
            break;
        }
    }
    Ok(bytes)
}

lazy_static! {
    static ref LIFECYCLE_EVENTS: Mutex<Vec<LifecycleEventWire>> = Mutex::new(Vec::new());
}

pub(crate) fn record_process_exit(pid: u64, parent_pid: u64, exit_status: i32) {
    let mut events = LIFECYCLE_EVENTS.lock();
    if events.len() >= LIFECYCLE_DRAIN_MAX_EVENTS {
        events.remove(0);
    }
    events.push(LifecycleEventWire {
        event: LIFECYCLE_EVENT_EXIT,
        pid,
        parent_pid,
        exit_status,
        ..LifecycleEventWire::default()
    });
}

pub(super) fn drain_lifecycle_events(args: &LifecycleDrainBrokerArgs) -> Result<u64, i64> {
    if args.out_capacity as usize > LIFECYCLE_DRAIN_MAX_EVENTS || args.out_count_ptr == 0 {
        return Err(LINUX_EINVAL);
    }
    let capacity = args.out_capacity as usize;
    if capacity != 0 && args.out_events_ptr == 0 {
        return Err(LINUX_EINVAL);
    }
    let mut drained = Vec::new();
    {
        let mut events = LIFECYCLE_EVENTS.lock();
        let count = capacity.min(events.len());
        for _ in 0..count {
            drained.push(events.remove(0));
        }
    }
    let out_count = drained.len() as u32;
    if out_count != 0 {
        let bytes = unsafe {
            core::slice::from_raw_parts(
                drained.as_ptr().cast::<u8>(),
                drained.len() * core::mem::size_of::<LifecycleEventWire>(),
            )
        };
        usermem::write_current_user_bytes(args.out_events_ptr, bytes)
            .map_err(address_space_error_to_linux_errno)?;
    }
    usermem::write_current_user_bytes(args.out_count_ptr, &out_count.to_le_bytes())
        .map_err(address_space_error_to_linux_errno)?;
    Ok(out_count as u64)
}

*/
// RING3-MIGRATION-COMMENTED-OUT END
