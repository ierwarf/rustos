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

    pub fn service_pending() {}

    /// 프로세스 종료(정상/비정상) 시 해당 프로세스가 소유한 IPC 서비스 엔드포인트를 모두 해제한다.
    pub fn cleanup_service_endpoints_for_process(process_id: u64) {
        crate::user::syscall::linux::cleanup_service_endpoints_for_process(process_id);
    }
}

pub use syscall::{init as init_syscalls, service_pending};
