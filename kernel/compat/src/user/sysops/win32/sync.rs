use crate::multitask;

// RING3-MIGRATION-COMMENTED-OUT START: ExitProcess belongs in the Windows ABI
// user service. Ring0 keeps the task-exit substrate.
/*
pub(crate) fn exit_process(status: u64) -> u64 {
    if let Some(process_id) = multitask::current_user_process_id() {
        let wait_status = ((status as i32) & 0xff) << 8;
        let _ = multitask::note_process_exit_status(process_id, wait_status);
    }
    multitask::exit_current_user_task()
}

*/
// RING3-MIGRATION-COMMENTED-OUT END
