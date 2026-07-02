// RING3-MIGRATION-REFERENCE START: ABI substrate: vfsd/devmgrd/sessiond own
// sysop namespace policy. Ring0 keeps sysop module routing substrate.
pub(crate) mod console;
pub(crate) mod device;
pub mod linux;
pub(crate) mod stat;
pub mod usermem {
    pub use kernel_ps::api::sysops::usermem::*;
}
// RING3-MIGRATION-REFERENCE END: service-owned sysop module ABI substrate.
