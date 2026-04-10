pub mod linux {
    pub fn init_cpu_local_symbols() {
        kernel_base::driver::linux::init_cpu_local_symbols();
    }

    pub fn current_task_offset() -> usize {
        kernel_base::user::syscall::linux_compat_current_task_offset()
    }

    pub fn stack_guard_offset() -> usize {
        kernel_base::user::syscall::linux_compat_stack_guard_offset()
    }
}

pub mod windows {}

pub mod console_host {
    pub use kernel_base::user::console_host::{
        ConsoleHostError, ConsoleProgramSpec, ExecutableImage, LoadedExecutableImage,
        load_executable_image, load_executable_image_by_path, prime_executable_image,
        spawn_program_in_session,
    };
}

pub fn init_syscalls() {
    kernel_base::user::syscall::init();
}

pub fn service_pending() {
    kernel_base::driver::linux::runtime::service_compat_pending();
}
