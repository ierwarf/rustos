//! Rootd-only pre-loader bootstrap launch. This native syscall takes one
//! versioned argument block, not Linux exec argv/env registers. Every child is
//! born into a Job descriptor with CREATE_PROCESS authority; fd zero is valid.

pub const BOOTSTRAP_SPAWN_ABI_VERSION: u16 = 1;
pub const BOOTSTRAP_SPAWN_FLAG_LOGICAL_ADMIN: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosBootstrapSpawnArgs {
    pub abi_version: u16,
    pub reserved0: u16,
    pub flags: u32,
    pub path_ptr: u64,
    /// Zero inherits rootd's console session.
    pub console_session: u64,
    pub weight_micros: u64,
    /// Caller-local typed Job descriptor; never a raw JobId.
    pub job_handle: u64,
}

pub const fn bootstrap_spawn_shape_valid(args: &RustosBootstrapSpawnArgs) -> bool {
    args.abi_version == BOOTSTRAP_SPAWN_ABI_VERSION
        && args.reserved0 == 0
        && args.flags & !BOOTSTRAP_SPAWN_FLAG_LOGICAL_ADMIN == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_spawn_wire_has_explicit_version_and_job_descriptor() {
        assert_eq!(core::mem::size_of::<RustosBootstrapSpawnArgs>(), 40);
        assert_eq!(core::mem::offset_of!(RustosBootstrapSpawnArgs, path_ptr), 8);
        assert_eq!(core::mem::offset_of!(RustosBootstrapSpawnArgs, console_session), 16);
        assert_eq!(core::mem::offset_of!(RustosBootstrapSpawnArgs, weight_micros), 24);
        assert_eq!(core::mem::offset_of!(RustosBootstrapSpawnArgs, job_handle), 32);
        let args = RustosBootstrapSpawnArgs { abi_version: BOOTSTRAP_SPAWN_ABI_VERSION, ..RustosBootstrapSpawnArgs::default() };
        assert!(bootstrap_spawn_shape_valid(&args), "fd zero is not a missing-job sentinel");
        assert!(bootstrap_spawn_shape_valid(&RustosBootstrapSpawnArgs { flags: BOOTSTRAP_SPAWN_FLAG_LOGICAL_ADMIN, job_handle: 9, ..args }));
        for bad in [
            RustosBootstrapSpawnArgs { abi_version: 0, ..args },
            RustosBootstrapSpawnArgs { reserved0: 1, ..args },
            RustosBootstrapSpawnArgs { flags: 2, ..args },
        ] { assert!(!bootstrap_spawn_shape_valid(&bad)); }
    }
}
