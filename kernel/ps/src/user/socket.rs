// RING3-MIGRATION-REFERENCE START: already migrated: netd owns socket
// namespace, lifecycle, and readiness policy. Ring0 keeps fd-table socket
// token substrate only.
use crate::user::handles::KernelHandle;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
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
    transfer: Arc<SocketTransferState>,
    domain: u64,
    type_: u64,
    protocol: u64,
}

#[derive(Debug)]
struct SocketTransferState {
    channel_id: AtomicU64,
    channel_side: AtomicU8,
    send_position: AtomicU64,
    recv_position: AtomicU64,
    send_in_flight: AtomicBool,
    recv_in_flight: AtomicBool,
    poisoned: AtomicBool,
}

pub struct SocketStreamGuard {
    transfer: Arc<SocketTransferState>,
    start: u64,
    max_end: u64,
    send: bool,
    settled: bool,
}

impl SocketHandle {
    pub fn from_token(token: u64, domain: u64, type_: u64, protocol: u64) -> Self {
        Self {
            token,
            transfer: Arc::new(SocketTransferState {
                channel_id: AtomicU64::new(0),
                channel_side: AtomicU8::new(0),
                send_position: AtomicU64::new(0),
                recv_position: AtomicU64::new(0),
                send_in_flight: AtomicBool::new(false),
                recv_in_flight: AtomicBool::new(false),
                poisoned: AtomicBool::new(false),
            }),
            domain,
            type_,
            protocol,
        }
    }

    pub const fn token_id(&self) -> u64 {
        self.token
    }

    pub fn channel_id(&self) -> u64 {
        self.transfer.channel_id.load(Ordering::Acquire)
    }

    pub fn channel_side(&self) -> u8 {
        self.transfer.channel_side.load(Ordering::Acquire)
    }

    pub fn bind_channel(&self, channel_id: u64, channel_side: u8) -> bool {
        if channel_id == 0 || !matches!(channel_side, 1 | 2) {
            return false;
        }
        if self
            .transfer
            .channel_id
            .compare_exchange(0, channel_id, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        self.transfer
            .channel_side
            .store(channel_side, Ordering::Release);
        true
    }

    pub fn begin_stream_send(&self, len: usize) -> Option<SocketStreamGuard> {
        self.begin_stream_transaction(len, true)
    }

    pub fn begin_stream_receive(&self) -> Option<SocketStreamGuard> {
        self.begin_stream_transaction(0, false)
    }

    fn begin_stream_transaction(&self, len: usize, send: bool) -> Option<SocketStreamGuard> {
        if self.channel_id() == 0
            || self.channel_side() == 0
            || self.transfer.poisoned.load(Ordering::Acquire)
        {
            return None;
        }
        let busy = if send {
            &self.transfer.send_in_flight
        } else {
            &self.transfer.recv_in_flight
        };
        busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        let position = if send {
            &self.transfer.send_position
        } else {
            &self.transfer.recv_position
        };
        let start = position.load(Ordering::Acquire);
        let max_end = if send {
            let len = len as u64;
            let Some(end) = start.checked_add(len) else {
                busy.store(false, Ordering::Release);
                return None;
            };
            end
        } else {
            start
        };
        Some(SocketStreamGuard {
            transfer: self.transfer.clone(),
            start,
            max_end,
            send,
            settled: false,
        })
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

impl SocketStreamGuard {
    pub const fn start(&self) -> u64 {
        self.start
    }

    pub const fn end(&self) -> u64 {
        self.max_end
    }

    pub fn commit_send(mut self, accepted: usize) -> bool {
        if !self.send {
            return false;
        }
        let Ok(accepted) = u64::try_from(accepted) else {
            return false;
        };
        let Some(end) = self.start.checked_add(accepted) else {
            return false;
        };
        if end > self.max_end
            || self
                .transfer
                .send_position
                .compare_exchange(self.start, end, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            self.transfer.poisoned.store(true, Ordering::Release);
            return false;
        }
        self.max_end = end;
        self.settled = true;
        true
    }

    pub fn commit_receive(mut self, len: usize) -> bool {
        if self.send {
            return false;
        }
        let Ok(len) = u64::try_from(len) else {
            return false;
        };
        let Some(end) = self.start.checked_add(len) else {
            return false;
        };
        if self
            .transfer
            .recv_position
            .compare_exchange(self.start, end, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        self.max_end = end;
        self.settled = true;
        true
    }
}

impl Drop for SocketStreamGuard {
    fn drop(&mut self) {
        let busy = if self.send {
            &self.transfer.send_in_flight
        } else {
            &self.transfer.recv_in_flight
        };
        busy.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::SocketHandle;

    #[test]
    fn partial_send_advances_only_the_provider_accepted_range() {
        let socket = SocketHandle::from_token(7, 1, 1, 0);
        assert!(socket.bind_channel(9, 1));

        let first = socket.begin_stream_send(64).expect("first reservation");
        assert_eq!((first.start(), first.end()), (0, 64));
        assert!(first.commit_send(4));

        let second = socket.begin_stream_send(8).expect("second reservation");
        assert_eq!((second.start(), second.end()), (4, 12));
        drop(second);

        let retry = socket.begin_stream_send(8).expect("aborted range retry");
        assert_eq!((retry.start(), retry.end()), (4, 12));
        assert!(retry.commit_send(8));
    }

    #[test]
    fn provider_cannot_accept_beyond_the_reserved_range() {
        let socket = SocketHandle::from_token(8, 1, 1, 0);
        assert!(socket.bind_channel(10, 2));
        let reservation = socket.begin_stream_send(3).expect("reservation");
        assert!(!reservation.commit_send(4));
        assert!(socket.begin_stream_send(1).is_none());
    }
}
// RING3-MIGRATION-REFERENCE END: netd-owned socket policy token substrate.
