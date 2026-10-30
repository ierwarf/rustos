//! Explicit rootd-to-loaderd application-realm provisioning. Existing spawn,
//! exec and activation envelopes are unchanged; unknown operations fail closed.
use super::*;

pub const LOADER_JOB_REALM_ABI_VERSION: u16 = 1;
pub const LOADER_OP_CONFIGURE_JOB_REALM: u16 = 5;
/// Opt-in hog-only quota shared by loader policy, rootd observation and KVM acceptance.
pub const LOADER_APPLICATION_LIMIT_PROBE_PAGES: u64 = 32_768;
pub const LOADER_APPLICATION_REALM_RIGHTS: u32 = JOB_RIGHT_INSPECT | JOB_RIGHT_CREATE_JOB
    | JOB_RIGHT_CREATE_PROCESS | JOB_RIGHT_SET_LIMITS | JOB_RIGHT_KILL
    | JOB_RIGHT_DUPLICATE | JOB_RIGHT_TRANSFER;

/// Accompanied by exactly one receiver-local typed Job descriptor. A numeric
/// job identity or fd in request bytes never conveys placement authority.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LoaderJobRealmRequest {
    pub version: u16,
    pub op: u16,
    pub reserved: u32,
    pub requester_pid: u64,
}

impl LoaderJobRealmRequest {
    pub fn is_valid(&self, sender_pid: u64) -> bool {
        self.version == LOADER_JOB_REALM_ABI_VERSION
            && self.op == LOADER_OP_CONFIGURE_JOB_REALM && self.reserved == 0
            && sender_pid != 0 && self.requester_pid == sender_pid
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn realm_provisioning_has_its_own_exact_sender_envelope_and_rootd_role() {
        assert_eq!(core::mem::size_of::<LoaderJobRealmRequest>(), 16);
        assert_eq!(core::mem::offset_of!(LoaderJobRealmRequest, requester_pid), 8);
        let request = LoaderJobRealmRequest { version: 1, op: 5, requester_pid: 42, reserved: 0 };
        assert!(request.is_valid(42));
        assert!(!request.is_valid(0));
        assert!(!request.is_valid(41));
        for bad in [
            LoaderJobRealmRequest { version: 2, ..request },
            LoaderJobRealmRequest { op: LOADER_OP_SPAWN_EXEC, ..request },
            LoaderJobRealmRequest { reserved: 1, ..request },
        ] { assert!(!bad.is_valid(42)); }
        assert!(loader_service_role_allows_operation(LOADER_OP_CONFIGURE_JOB_REALM, IPC_SERVICE_ROOTD));
        for role in [IPC_SERVICE_LOADERD, IPC_SERVICE_INITD, IPC_SERVICE_SESSIOND, IPC_SERVICE_PROCD, 0] {
            assert!(!loader_service_role_allows_operation(LOADER_OP_CONFIGURE_JOB_REALM, role));
        }
        assert_eq!(LOADER_APPLICATION_REALM_RIGHTS & (JOB_RIGHT_SET_POLICY | JOB_RIGHT_SET_CRITICAL | JOB_RIGHT_WAIT), 0);
    }
}
