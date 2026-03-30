use crate::multitask;

pub(crate) fn exit_process(_exit_code: u64) -> u64 {
    multitask::exit_current_user_task()
}
