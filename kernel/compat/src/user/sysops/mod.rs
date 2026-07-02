// RING3-MIGRATION-REFERENCE START: vfsd/devmgrd/sessiond should own sysop
// namespace policy. Ring0 keeps sysop module routing substrate.
pub(crate) mod console;
pub(crate) mod device;
pub(crate) mod file;
pub mod linux;
pub(crate) mod stat;
pub mod usermem {
    pub use kernel_ps::api::sysops::usermem::*;
}
// RING3-MIGRATION-REFERENCE END: service-owned sysop module routing policy.
