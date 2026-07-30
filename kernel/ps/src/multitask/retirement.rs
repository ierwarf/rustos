/// Exact identity of a retired user task whose subsystem-local wait records
/// must be removed before the scheduler may recycle its slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetiredTaskCleanup {
    pub(super) task_id: u64,
    pub(super) process_id: u64,
    pub(super) process_terminal: bool,
    pub(super) clear_child_tid: u64,
    pub(super) robust_list_head: u64,
    pub(super) robust_list_len: u64,
}

impl RetiredTaskCleanup {
    pub const fn task_id(self) -> u64 {
        self.task_id
    }

    pub const fn process_id(self) -> u64 {
        self.process_id
    }

    pub const fn process_terminal(self) -> bool {
        self.process_terminal
    }

    pub const fn clear_child_tid(self) -> u64 {
        self.clear_child_tid
    }

    pub const fn robust_list_head(self) -> u64 {
        self.robust_list_head
    }

    pub const fn robust_list_len(self) -> u64 {
        self.robust_list_len
    }
}
