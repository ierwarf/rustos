use crate::multitask;

pub(crate) fn exit_process(status: u64) -> u64 {
    if let Some(process_id) = multitask::current_user_process_id() {
        let wait_status = ((status as i32) & 0xff) << 8;
        let _ = multitask::note_process_exit_status(process_id, wait_status);
    }
    multitask::exit_current_user_task()
}
