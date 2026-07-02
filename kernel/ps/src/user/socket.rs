// RING3-MIGRATION-REFERENCE START: already migrated: netd owns socket
// namespace, lifecycle, and readiness policy. Ring0 keeps fd-table socket
// token substrate only.
use crate::user::handles::KernelHandle;
use kernel_object::api::handle::HandleRights;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SocketError {
    AddressInUse,
    BrokenPipe,
    ConnectionRefused,
    InvalidArgument,
    IsConnected,
    NotConnected,
    NotFound,
    PermissionDenied,
    TryAgain,
}

#[derive(Clone, Debug)]
pub struct PassedHandle {
    handle: KernelHandle,
    status_flags: u64,
    rights: HandleRights,
}

impl PassedHandle {
    pub fn new_with_rights(handle: KernelHandle, status_flags: u64, rights: HandleRights) -> Self {
        Self {
            handle,
            status_flags,
            rights,
        }
    }

    pub fn handle(&self) -> &KernelHandle {
        &self.handle
    }

    pub const fn status_flags(&self) -> u64 {
        self.status_flags
    }

    pub const fn rights(&self) -> HandleRights {
        self.rights
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SocketCredentials {
    pid: i32,
    uid: u32,
    gid: u32,
}

impl SocketCredentials {
    pub const fn new(pid: i32, uid: u32, gid: u32) -> Self {
        Self { pid, uid, gid }
    }

    pub const fn pid(self) -> i32 {
        self.pid
    }

    pub const fn uid(self) -> u32 {
        self.uid
    }

    pub const fn gid(self) -> u32 {
        self.gid
    }
}

#[derive(Clone, Debug)]
pub struct SocketHandle {
    token: u64,
    domain: u64,
    type_: u64,
    protocol: u64,
}

impl SocketHandle {
    pub const fn from_token(token: u64, domain: u64, type_: u64, protocol: u64) -> Self {
        Self {
            token,
            domain,
            type_,
            protocol,
        }
    }

    pub const fn token_id(&self) -> u64 {
        self.token
    }

    pub const fn domain(&self) -> u64 {
        self.domain
    }

    pub const fn type_(&self) -> u64 {
        self.type_
    }

    pub const fn protocol(&self) -> u64 {
        self.protocol
    }

    pub fn bound_path(&self) -> Option<alloc::string::String> {
        None
    }
}
// RING3-MIGRATION-REFERENCE END: netd-owned socket policy token substrate.
