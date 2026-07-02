// RING3-MIGRATION-REFERENCE START: user compat module routing is a ring0 ABI
// substrate. Policy belongs in syscalld/loaderd/procd/vfsd/netd services.
pub mod abi {
    pub use kernel_ps::api::abi::*;
}
pub mod console_host;
pub mod epoll {
    pub use kernel_ps::api::epoll::*;
}
pub mod handles {
    pub use kernel_ps::api::handles::*;
}
pub mod linux {
    pub use kernel_ps::api::linux::*;
}
pub mod memfd {
    pub use kernel_ps::api::memfd::*;
}
pub mod process;
pub mod process_state {
    pub use kernel_ps::api::process_state::*;
}
pub mod socket {
    pub use kernel_ps::api::socket::*;
}
pub mod syscall;
pub mod sysops;
// RING3-MIGRATION-REFERENCE END: service-owned user compat module policy.
