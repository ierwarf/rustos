//! Kernel-attested thread listing behind `/proc/<pid>/task`.
//!
//! - **Owner:** `kernel-ps` owns the task/process binding published per
//!   scheduler slot; `vfsd` owns the procfs namespace and never sees this list.
//! - **Boundary:** the caller names its own pid (the front-end uses the current
//!   process); the answer is the ascending Linux thread ids bound to it.
//! - **Concurrency:** each slot is observed under its identity writer leaf, one
//!   at a time; no process-state or scheduler-wide lock is taken and nothing is
//!   held across slots, so the list is a bounded, instantaneous-per-slot read.
//! - **Failure:** a slot caught mid-publication or already retired is skipped,
//!   which is the same answer `tgkill` would give for it.
//! - **Forbidden:** acting on the ids as authority. They are a snapshot; a
//!   thread may exit before the caller uses one.

use alloc::vec::Vec;

use super::super::{current_identity, scheduler::MAX_TASK};

/// Linux thread ids of the live, non-retired user threads of `pid`, ascending.
pub fn live_user_thread_ids(pid: u64) -> Vec<u64> {
    let mut ids = Vec::new();
    for slot in 0..MAX_TASK {
        if current_identity::observe_user_process_task_abi(slot, pid, 0).is_none() {
            continue;
        }
        let Some(identity) = current_identity::read(slot) else {
            continue;
        };
        if identity.user_mode && identity.process_id == Some(pid) {
            if let Some(task_id) = identity.task_id {
                ids.push(task_id);
            }
        }
    }
    ids.sort_unstable();
    ids.dedup();
    ids
}
