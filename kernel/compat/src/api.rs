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
    pub fn drain_syscall_profile() -> usize {
        crate::user::syscall::drain_syscall_profile()
    }

    pub fn drain_ipc_call_profile() -> usize {
        crate::user::syscall::linux::drain_ipc_call_profile()
    }

    /// Emits the bounded per-second IPC receive/reply-path cycle attribution.
    /// This is diagnostics only; nothing consumes it to make a decision.
    pub fn drain_ipc_server_profile() -> usize {
        crate::user::syscall::linux::ipc_server_profile::drain_ipc_server_profile()
    }

    pub const RETIRED_TASK_CLEANUP_BUDGET: usize =
        crate::user::syscall::linux::RETIRED_TASK_CLEANUP_BUDGET;

    pub fn service_retired_task_runtime_cleanup(limit: usize) -> usize {
        crate::user::syscall::linux::service_retired_task_runtime_cleanup(limit)
    }
}

pub use syscall::init as init_syscalls;

pub mod pager {
    /// Runs a fixed amount of normal-time pager work from nucleus housekeeping.
    ///
    /// Returns the number of dispatch and adoption steps performed, so the
    /// housekeeping caller can account for the work it drove.
    pub fn service_deferred_work() -> usize {
        crate::pager::service_deferred_work()
    }

    pub use crate::pager::AnonymousFaultOutcome;

    /// Publishes one census of the ring0 anonymous fault path. See
    /// `crate::pager::record_anonymous_fault_census`.
    pub fn record_anonymous_fault_census() {
        crate::pager::record_anonymous_fault_census();
    }

    /// Serves one anonymous first-touch fault in the faulting task's own
    /// context, with no pager round trip. See
    /// `crate::pager::serve_anonymous_first_touch`.
    pub fn serve_anonymous_first_touch(
        request: rustos_user_abi::pager::PagerFaultRequestWire,
        prot: u32,
    ) -> AnonymousFaultOutcome {
        crate::pager::serve_anonymous_first_touch(request, prot)
    }
}
