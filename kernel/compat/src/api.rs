pub mod linux {
    pub use crate::user::linux::*;

    pub fn current_task_offset() -> usize {
        crate::user::syscall::linux_compat_current_task_offset()
    }

    pub fn stack_guard_offset() -> usize {
        crate::user::syscall::linux_compat_stack_guard_offset()
    }
}

pub mod shared {
    pub mod console_host {
        pub use crate::user::console_host::*;
    }
}

pub mod console_host {
    pub use crate::user::console_host::*;
}

pub mod syscall {
    pub use crate::user::syscall::{
        activate_linux_compat_cpu_local, init, linux_compat_current_task_offset,
        linux_compat_stack_guard_offset, set_linux_compat_current_task_ptr,
        set_linux_compat_stack_guard, with_kernel_gs_base,
    };

    /// Classify one x86 user exception and commit Linux process cleanup only
    /// after the scheduler actually retires its final thread.
    pub fn retire_current_linux_task_due_to_fault(
        vector: u8,
        error_code: Option<u64>,
        cr2: u64,
        rip: u64,
        rsp: u64,
    ) -> kernel_ps::api::UserFaultDisposition {
        crate::user::syscall::retire_current_linux_task_due_to_fault(
            vector, error_code, cr2, rip, rsp,
        )
    }

    pub fn service_deferred_transfer_releases() -> usize {
        crate::user::syscall::linux::service_deferred_transfer_releases()
    }

    /// Emits the bounded per-second IPC call-path cycle attribution. This is
    /// diagnostics only; nothing consumes it to make a decision.
    pub fn drain_ipc_call_profile() -> usize {
        crate::user::syscall::linux::drain_ipc_call_profile()
    }

    pub fn cleanup_retired_task_runtime_state(
        task_id: u64,
        process_id: u64,
        process_terminal: bool,
        clear_child_tid: u64,
        robust_list_head: u64,
        robust_list_len: u64,
    ) -> usize {
        crate::user::syscall::cleanup_retired_task_runtime_state(
            task_id,
            process_id,
            process_terminal,
            clear_child_tid,
            robust_list_head,
            robust_list_len,
        )
    }
}

pub use syscall::init as init_syscalls;
