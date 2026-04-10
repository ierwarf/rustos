pub mod abi;
pub mod console_host;
pub mod epoll;
pub mod handles;
pub mod linux {
    #[cfg(rustos_building_kernel_compat)]
    pub use kernel_base::process_linux::*;
    #[cfg(not(rustos_building_kernel_compat))]
    pub use crate::process_linux::*;
}
pub mod memfd;
pub mod process;
pub mod process_state {
    #[cfg(rustos_building_kernel_compat)]
    pub use kernel_base::process_state::*;
    #[cfg(not(rustos_building_kernel_compat))]
    pub use crate::process_state::*;
}
pub mod socket;
pub mod syscall;
pub mod sysops;
pub mod windows {
    #[cfg(rustos_building_kernel_compat)]
    pub use kernel_base::user_windows::*;
    #[cfg(not(rustos_building_kernel_compat))]
    pub use crate::user_windows::*;
}
