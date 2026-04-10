#![no_std]

extern crate alloc;

#[macro_export]
macro_rules! executive_debug_println {
    () => {{
        kernel_base::debug::println_newline();
    }};
    ($($arg:tt)*) => {{
        kernel_base::debug::println_fmt(format_args!($($arg)*));
    }};
}

pub mod debug {
    pub use crate::executive_debug_println as println;
    pub use kernel_base::debug::*;
}

pub mod compat_api {
    pub use kernel_compat::api::*;
}

pub mod hal_api {
    pub use kernel_hal::api::*;
}

pub mod io_manager_api {
    pub use kernel_io_manager::api::*;
}

pub mod mm_api {
    pub use kernel_mm::api::*;
}

pub mod ps_api {
    pub use kernel_ps::api::*;
}

pub mod user {
    pub mod console_host {
        pub use kernel_compat::api::console_host::*;
    }
}

pub mod util {
    pub mod random {
        pub use kernel_base::util::random::*;
    }
}

mod internal;

pub mod boot;
