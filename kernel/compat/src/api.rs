pub mod linux {
    pub use crate::user::linux::*;

    pub fn current_task_offset() -> usize {
        crate::user::syscall::linux_compat_current_task_offset()
    }

    pub fn stack_guard_offset() -> usize {
        crate::user::syscall::linux_compat_stack_guard_offset()
    }
}

pub mod windows {
    pub use crate::windows::*;
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

    pub fn service_pending() {
        kernel_io_manager::api::driver::linux::runtime::service_compat_pending();
    }
}

pub use syscall::{init as init_syscalls, service_pending};
