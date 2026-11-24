use crate::user::handles::KernelHandle;
use kernel_object::api::handle::HandleRights;

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
