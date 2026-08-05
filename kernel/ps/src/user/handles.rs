use crate::io::device::DeviceHandle;
use crate::user::epoll::EpollHandle;
use crate::user::linux as linux_abi;
use crate::user::memfd::MemfdHandle;
use crate::user::socket::SocketHandle;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use kernel_ipc_runtime::api::{
    ChannelIdentity, KernelTransferTicket, KernelTransferredHandle, ProcessIdentity,
    ServiceIdentity, TransferContext,
};
use kernel_object::api::handle::{
    DeviceHandleRights, FileHandleRights, HandleOwner, HandleRights, HandleToken,
    SharedRegionRights, SocketHandleRights,
};
use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};

#[path = "handles/display_surface.rs"]
mod display_surface;
#[path = "handles/table.rs"]
mod table;
mod transfer_registry;
#[path = "handles/vfs.rs"]
mod vfs;

pub use transfer_registry::*;

pub use display_surface::DisplaySurfaceHandle;
pub use table::{HandleEntry, HandleTable, TransferredHandleEntry};
pub use vfs::{
    FileHandleSeekError, FileHandleSeekWhence, VfsDirectoryEntry, VfsDirectoryEntryKind,
    VfsDirectoryHandle,
};

pub const FIRST_DYNAMIC_FD: u32 = 3;
/// Per-process descriptor ceiling. This prevents sparse descriptor requests
/// from turning a Linux ABI integer into an unbounded ring-0 `Vec` resize.
pub const MAX_DYNAMIC_FD: u64 = 65_535;
pub const FD_CLOEXEC: u32 = 0x1;
const MAX_PENDING_IPC_TRANSFER_OBJECTS: usize = 1024;
const STATUS_FLAG_MASK: u64 =
    linux_abi::O_ACCMODE | linux_abi::O_APPEND | linux_abi::O_NONBLOCK | linux_abi::O_NOCTTY;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConsoleStreamKind {
    Input,
    Output,
    Error,
}

#[derive(Debug)]
struct ConsoleOpenDescription {
    token: u64,
    stream: ConsoleStreamKind,
}

/// One Linux console open description. Cloning this handle models dup/fork;
/// the token remains stable until the last descriptor reference is dropped.
#[derive(Clone, Debug)]
pub struct ConsoleHandle {
    description: Arc<ConsoleOpenDescription>,
}

static NEXT_CONSOLE_OPEN_DESCRIPTION_TOKEN: AtomicU64 = AtomicU64::new(1);
const CONSOLE_OPEN_DESCRIPTION_CAPACITY: usize = 256;

static CONSOLE_OPEN_DESCRIPTIONS: TrackedSpinLock<
    [Option<ConsoleDescriptionRegistryEntry>; CONSOLE_OPEN_DESCRIPTION_CAPACITY],
    { LockClass::ConsoleRegistry as u8 },
> = TrackedSpinLock::new([const { None }; CONSOLE_OPEN_DESCRIPTION_CAPACITY]);

struct ConsoleDescriptionRegistryEntry {
    token: u64,
    description: Weak<ConsoleOpenDescription>,
    stream: ConsoleStreamKind,
    descriptor_refs: usize,
}

impl ConsoleHandle {
    pub fn new(stream: ConsoleStreamKind) -> Self {
        let token = NEXT_CONSOLE_OPEN_DESCRIPTION_TOKEN
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                (current != 0).then(|| current.checked_add(1)).flatten()
            })
            .expect("console open-description token exhausted");
        let description = Arc::new(ConsoleOpenDescription { token, stream });
        let mut descriptions = CONSOLE_OPEN_DESCRIPTIONS.lock();
        for slot in descriptions.iter_mut() {
            if slot.as_ref().is_some_and(|entry| {
                entry.descriptor_refs == 0 || entry.description.strong_count() == 0
            }) {
                *slot = None;
            }
        }
        let slot = descriptions
            .iter_mut()
            .find(|slot| slot.is_none())
            .expect("console open-description registry exhausted");
        *slot = Some(ConsoleDescriptionRegistryEntry {
            token,
            description: Arc::downgrade(&description),
            stream,
            descriptor_refs: 1,
        });
        drop(descriptions);
        Self { description }
    }

    pub fn token_id(&self) -> u64 {
        self.description.token
    }

    pub fn stream(&self) -> ConsoleStreamKind {
        self.description.stream
    }

    /// True after the final descriptor-table reference has been removed.
    /// Transient syscall snapshots deliberately do not affect this count.
    pub fn is_last_reference(&self) -> bool {
        CONSOLE_OPEN_DESCRIPTIONS
            .lock()
            .iter()
            .flatten()
            .find(|entry| entry.token == self.token_id())
            .is_none_or(|entry| entry.descriptor_refs == 0)
    }

    pub(crate) fn acquire_descriptor_reference(&self) {
        assert!(
            self.try_acquire_descriptor_reference(),
            "live console description missing from registry"
        );
    }

    /// Pins one still-live descriptor-table reference for a cross-service
    /// transaction. Returns false after final close and never resurrects a
    /// zero-reference console description.
    pub fn try_acquire_descriptor_reference(&self) -> bool {
        let mut descriptions = CONSOLE_OPEN_DESCRIPTIONS.lock();
        let Some(entry) = descriptions
            .iter_mut()
            .flatten()
            .find(|entry| entry.token == self.token_id())
        else {
            return false;
        };
        if entry.descriptor_refs == 0 || entry.description.strong_count() == 0 {
            return false;
        }
        entry.descriptor_refs = entry
            .descriptor_refs
            .checked_add(1)
            .expect("console descriptor reference count exhausted");
        true
    }

    /// Drops one fd-table reference and reports whether it was the final one.
    pub fn release_descriptor_reference(&self) -> bool {
        let mut descriptions = CONSOLE_OPEN_DESCRIPTIONS.lock();
        let entry = descriptions
            .iter_mut()
            .flatten()
            .find(|entry| entry.token == self.token_id())
            .expect("live console description missing from registry");
        assert!(
            entry.descriptor_refs != 0,
            "console descriptor reference count underflow"
        );
        entry.descriptor_refs -= 1;
        entry.descriptor_refs == 0
    }

    pub fn token_is_live(token: u64) -> bool {
        Self::stream_for_token(token).is_some()
    }

    pub fn stream_for_token(token: u64) -> Option<ConsoleStreamKind> {
        if token == 0 {
            return None;
        }
        CONSOLE_OPEN_DESCRIPTIONS
            .lock()
            .iter()
            .flatten()
            .find(|entry| entry.token == token)
            .filter(|entry| entry.descriptor_refs != 0 && entry.description.strong_count() != 0)
            .map(|entry| entry.stream)
    }
}

fn fresh_ipc_transfer_nonce() -> u64 {
    loop {
        let mut bytes = [0_u8; core::mem::size_of::<u64>()];
        nucleus_core::util::random::Random::new().fill_bytes(&mut bytes);
        let nonce = u64::from_le_bytes(bytes);
        if nonce != 0 {
            return nonce;
        }
    }
}

fn allocate_nonwrapping_identity(counter: &AtomicU64) -> Option<u64> {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            (current != 0).then(|| current.checked_add(1)).flatten()
        })
        .ok()
}

#[derive(Debug, Clone)]
pub enum KernelHandle {
    Console(ConsoleHandle),
    Device(DeviceHandle),
    Epoll(EpollHandle),
    InetSocket(InetSocketHandle),
    Memfd(MemfdHandle),
    RemoteVfs(RemoteVfsHandle),
    Socket(SocketHandle),
    VfsDirectory(VfsDirectoryHandle),
    DisplaySurface(DisplaySurfaceHandle),
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct InetSocketHandle {
    token: u64,
    domain: u64,
    type_: u64,
    protocol: u64,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct RemoteVfsHandle {
    token: u64,
    remote_id: u64,
    kind: RemoteVfsHandleKind,
    path: String,
    len: u64,
    device_access: u16,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum RemoteVfsHandleKind {
    File,
    Directory,
    Device,
}

impl RemoteVfsHandle {
    pub fn new(
        remote_id: u64,
        kind: RemoteVfsHandleKind,
        path: String,
        len: u64,
        device_access: u16,
    ) -> Option<Self> {
        static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);
        Some(Self {
            token: allocate_nonwrapping_identity(&NEXT_TOKEN)?,
            remote_id,
            kind,
            path,
            len,
            device_access,
        })
    }

    pub const fn token_id(&self) -> u64 {
        self.token
    }

    pub const fn remote_id(&self) -> u64 {
        self.remote_id
    }

    pub const fn kind(&self) -> RemoteVfsHandleKind {
        self.kind
    }

    pub fn path(&self) -> String {
        self.path.clone()
    }

    pub const fn len(&self) -> u64 {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub const fn device_access(&self) -> u16 {
        self.device_access
    }
}

impl InetSocketHandle {
    pub fn new(domain: u64, type_: u64, protocol: u64) -> Option<Self> {
        static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);
        Some(Self {
            token: allocate_nonwrapping_identity(&NEXT_TOKEN)?,
            domain,
            type_,
            protocol,
        })
    }

    pub const fn from_token(token: u64, domain: u64, type_: u64, protocol: u64) -> Self {
        Self {
            token,
            domain,
            type_,
            protocol,
        }
    }

    pub const fn token_id(self) -> u64 {
        self.token
    }

    pub const fn domain(self) -> u64 {
        self.domain
    }

    pub const fn type_(self) -> u64 {
        self.type_
    }

    pub const fn protocol(self) -> u64 {
        self.protocol
    }
}

impl KernelHandle {
    pub(crate) fn token(&self) -> HandleToken {
        match self {
            Self::Console(console) => HandleToken::new(HandleOwner::Ps, console.token_id()),
            Self::Device(device) => HandleToken::new(HandleOwner::Io, device.token_id()),
            Self::Epoll(epoll) => HandleToken::new(HandleOwner::Compat, epoll.token_id()),
            Self::InetSocket(socket) => HandleToken::new(HandleOwner::Compat, socket.token_id()),
            Self::Memfd(memfd) => HandleToken::new(HandleOwner::Compat, memfd.token_id()),
            Self::RemoteVfs(remote) => HandleToken::new(HandleOwner::Io, remote.token_id()),
            Self::Socket(socket) => HandleToken::new(HandleOwner::Compat, socket.token_id()),
            Self::VfsDirectory(directory) => {
                HandleToken::new(HandleOwner::Io, directory.token_id())
            }
            Self::DisplaySurface(surface) => {
                HandleToken::new(HandleOwner::Io, surface.generation())
            }
        }
    }

    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::Console(_) => "console",
            Self::Device(_) => "device",
            Self::Epoll(_) => "epoll",
            Self::InetSocket(_) => "inet-socket",
            Self::Memfd(_) => "memfd",
            Self::RemoteVfs(_) => "remote-vfs",
            Self::Socket(_) => "socket",
            Self::VfsDirectory(_) => "vfs-dir",
            Self::DisplaySurface(_) => "display-surface",
        }
    }

    pub fn console_stream(&self) -> Option<ConsoleStreamKind> {
        match self {
            Self::Console(console) => Some(console.stream()),
            _ => None,
        }
    }

    pub fn console_handle(&self) -> Option<&ConsoleHandle> {
        match self {
            Self::Console(handle) => Some(handle),
            _ => None,
        }
    }

    pub const fn device_handle(&self) -> Option<DeviceHandle> {
        match self {
            Self::Device(handle) => Some(*handle),
            _ => None,
        }
    }

    pub fn default_rights(&self, status_flags: u64) -> HandleRights {
        match self {
            Self::Console(_) => HandleRights::Console,
            Self::Device(device) => {
                let mut rights = DeviceHandleRights::READ
                    .union(DeviceHandleRights::IOCTL)
                    .union(DeviceHandleRights::MAP)
                    .union(DeviceHandleRights::TRANSFER);
                if matches!(
                    device.access_kind(),
                    crate::io::device::DeviceAccessKind::Native
                ) {
                    rights = rights
                        .union(DeviceHandleRights::WRITE)
                        .union(DeviceHandleRights::ADMIN);
                }
                HandleRights::Device(rights)
            }
            Self::Epoll(_) => HandleRights::Epoll,
            Self::InetSocket(_) => HandleRights::Socket(
                SocketHandleRights::SEND
                    .union(SocketHandleRights::RECV)
                    .union(SocketHandleRights::TRANSFER),
            ),
            Self::Memfd(_) => HandleRights::Memfd(file_rights_from_status_flags(status_flags)),
            Self::RemoteVfs(remote) => match remote.kind() {
                RemoteVfsHandleKind::File => {
                    HandleRights::File(file_rights_from_status_flags(status_flags))
                }
                RemoteVfsHandleKind::Directory => {
                    HandleRights::File(FileHandleRights::READ.union(FileHandleRights::TRANSFER))
                }
                RemoteVfsHandleKind::Device => HandleRights::File(
                    FileHandleRights::READ
                        .union(FileHandleRights::WRITE)
                        .union(FileHandleRights::TRANSFER),
                ),
            },
            Self::Socket(_) => HandleRights::Socket(
                SocketHandleRights::SEND
                    .union(SocketHandleRights::RECV)
                    .union(SocketHandleRights::PASS_FD)
                    .union(SocketHandleRights::TRANSFER),
            ),
            Self::VfsDirectory(_) => {
                HandleRights::File(FileHandleRights::READ.union(FileHandleRights::TRANSFER))
            }
            Self::DisplaySurface(_) => HandleRights::DisplaySurface(
                SharedRegionRights::READ
                    .union(SharedRegionRights::WRITE)
                    .union(SharedRegionRights::MAP),
            ),
        }
    }

    pub fn supports_descriptor_transfer(&self, rights: HandleRights) -> bool {
        matches!(
            self,
            Self::Socket(_)
                | Self::Memfd(_)
                | Self::VfsDirectory(_)
                | Self::Device(_)
                | Self::RemoteVfs(_)
        ) && rights.allows_transfer()
    }

    pub fn socket_handle(&self) -> Option<&SocketHandle> {
        match self {
            Self::Socket(handle) => Some(handle),
            _ => None,
        }
    }

    pub fn procfs_link_target(&self, token: HandleToken) -> alloc::string::String {
        match self {
            Self::Console(_) => alloc::string::String::from("/dev/tty"),
            Self::Device(device) => alloc::string::String::from(device.device_id().path()),
            Self::Epoll(epoll) => alloc::string::String::from(epoll.path()),
            Self::InetSocket(socket) => {
                alloc::format!("socket:[rustos-inet:{}]", socket.token_id())
            }
            Self::Memfd(memfd) => memfd.path(),
            Self::RemoteVfs(remote) => remote.path(),
            Self::Socket(socket) => socket.bound_path().unwrap_or_else(|| {
                alloc::format!("socket:[rustos-unix-stream:{}]", token.object_id())
            }),
            Self::VfsDirectory(directory) => alloc::string::String::from(directory.path()),
            Self::DisplaySurface(_) => {
                alloc::format!("anon_inode:[rustos-display-surface:{}]", token.object_id())
            }
        }
    }
}

fn file_rights_from_status_flags(status_flags: u64) -> FileHandleRights {
    let mut rights = FileHandleRights::TRANSFER;
    match status_flags & linux_abi::O_ACCMODE {
        linux_abi::O_WRONLY => rights = rights.union(FileHandleRights::WRITE),
        linux_abi::O_RDWR => {
            rights = rights
                .union(FileHandleRights::READ)
                .union(FileHandleRights::WRITE);
        }
        _ => rights = rights.union(FileHandleRights::READ),
    }
    if status_flags & linux_abi::O_APPEND != 0 {
        rights = rights.union(FileHandleRights::APPEND);
    }
    if status_flags & linux_abi::O_NONBLOCK != 0 {
        rights = rights.union(FileHandleRights::NONBLOCK);
    }
    rights
}
