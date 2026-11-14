use crate::io::device::DeviceHandle;
use crate::user::epoll::EpollHandle;
use crate::user::eventfd::EventFdHandle;
use crate::user::linux as linux_abi;
use crate::user::memfd::MemfdHandle;
use crate::user::pipe::PipeHandle;
use crate::user::socket::SocketHandle;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use kernel_ipc_runtime::api::{
    ChannelIdentity, KernelTransferTicket, KernelTransferredHandle, ProcessIdentity,
    ServiceIdentity, TransferContext,
};
use kernel_object::api::handle::{
    DeviceHandleRights, FileHandleRights, HandleOwner, HandleRights, HandleToken,
    JobHandleRights, SharedRegionRights, SocketHandleRights,
};
use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};

#[path = "handles/console_registry.rs"]
mod console_registry;
#[path = "handles/display_surface.rs"]
mod display_surface;
#[path = "handles/device_buffer.rs"]
mod device_buffer;
#[path = "handles/table.rs"]
mod table;
mod transfer_registry;
#[path = "handles/vfs.rs"]
mod vfs;

pub use transfer_registry::*;

pub use display_surface::DisplaySurfaceHandle;
pub use device_buffer::{app_buffer_pool, BufferCapability, BufferError, BufferIdentity, BufferMappingHold, BufferOwner, BufferPool, BufferRights};
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

/// Live console open descriptions, keyed by their generation-tagged token.
static CONSOLE_OPEN_DESCRIPTIONS: TrackedSpinLock<
    console_registry::ConsoleRegistry,
    { LockClass::ConsoleRegistry as u8 },
> = TrackedSpinLock::new(console_registry::ConsoleRegistry::new());

impl ConsoleHandle {
    pub fn new(stream: ConsoleStreamKind) -> Self {
        let token = loop {
            let reserved = CONSOLE_OPEN_DESCRIPTIONS.lock().reserve();
            if let Some(token) = reserved {
                break token;
            }
            // No free slot: reclaim descriptions that vanished without a final
            // release, and only then grow. The chunk is built with no lock
            // held and installed without allocating; a racing creator may
            // install first, in which case the loop just finds its free slot.
            let mut descriptions = CONSOLE_OPEN_DESCRIPTIONS.lock();
            descriptions.reclaim_orphans();
            if let Some(token) = descriptions.reserve() {
                break token;
            }
            drop(descriptions);
            let chunk = console_registry::build_chunk()
                .expect("console open-description registry could not grow");
            assert!(
                CONSOLE_OPEN_DESCRIPTIONS.lock().grow(chunk),
                "console open-description registry exhausted its ceiling"
            );
        };
        let description = Arc::new(ConsoleOpenDescription { token, stream });
        CONSOLE_OPEN_DESCRIPTIONS
            .lock()
            .install(token, &description, stream);
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
            .is_last_reference(self.token_id())
    }

    // TEST-HARNESS: production paths use the fallible variant so a stale
    // description is refused rather than asserted on.
    #[cfg(test)]
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
        CONSOLE_OPEN_DESCRIPTIONS.lock().try_acquire(self.token_id())
    }

    /// Drops one fd-table reference and reports whether it was the final one.
    pub fn release_descriptor_reference(&self) -> bool {
        CONSOLE_OPEN_DESCRIPTIONS.lock().release(self.token_id())
    }

    pub fn token_is_live(token: u64) -> bool {
        Self::stream_for_token(token).is_some()
    }

    pub fn stream_for_token(token: u64) -> Option<ConsoleStreamKind> {
        if token == 0 {
            return None;
        }
        CONSOLE_OPEN_DESCRIPTIONS.lock().stream(token)
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
    /// An identity, not ambient authority: install with explicit Job rights.
    Job(crate::multitask::JobId),
    Console(ConsoleHandle),
    Device(DeviceHandle),
    Epoll(EpollHandle),
    EventFd(EventFdHandle),
    InetSocket(InetSocketHandle),
    Memfd(MemfdHandle),
    Pipe(PipeHandle),
    RemoteVfs(RemoteVfsHandle),
    Socket(SocketHandle),
    VfsDirectory(VfsDirectoryHandle),
    DisplaySurface(DisplaySurfaceHandle),
    DeviceBuffer(BufferCapability),
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
    library: LibraryBinding,
}

/// Whether this open file description is known to be a sealed shared library
/// object. The record lives in the description's handle, so it follows
/// dup/fork and dies with the last close, exec-cloexec or process exit; no
/// separate revocation list exists to go stale. It is only ever written by the
/// MM broker after vfsd attested the file identity, and every use revalidates
/// the object generation against the registry.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum LibraryBinding {
    /// Not yet attested.
    Unknown,
    /// Attested equal to the sealed library object `object` (registry handle).
    Bound { object: u64 },
    /// No sealed object matched at registry epoch `registry_epoch`. Valid only
    /// until the registry changes.
    Ineligible { registry_epoch: u64 },
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
            library: LibraryBinding::Unknown,
        })
    }

    pub const fn token_id(&self) -> u64 {
        self.token
    }

    pub const fn library_binding(&self) -> LibraryBinding {
        self.library
    }

    pub fn set_library_binding(&mut self, binding: LibraryBinding) {
        self.library = binding;
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
            Self::Job(job) => job.handle_token()
                .expect("job descriptor requires a nonzero generation"),
            Self::Console(console) => {
                HandleToken::from_nonreusable_open_description(HandleOwner::Ps, console.token_id())
                    .expect("console open-description token is nonzero and nonreusable")
            }
            Self::Device(device) => HandleToken::new(HandleOwner::Io, device.token_id()),
            Self::Epoll(epoll) => HandleToken::new(HandleOwner::Compat, epoll.token_id()),
            Self::EventFd(eventfd) => HandleToken::from_nonreusable_open_description(
                HandleOwner::Ps,
                eventfd.token_id(),
            )
            .expect("eventfd open-description token is nonzero and nonreusable"),
            Self::Pipe(pipe) => HandleToken::from_nonreusable_open_description(
                HandleOwner::Ps,
                pipe.token_id(),
            )
            .expect("pipe open-description token is nonzero and nonreusable"),
            Self::InetSocket(socket) => HandleToken::from_nonreusable_open_description(
                HandleOwner::Compat,
                socket.token_id(),
            )
            .unwrap_or_else(|| HandleToken::new(HandleOwner::Compat, socket.token_id())),
            Self::Memfd(memfd) => HandleToken::new(HandleOwner::Compat, memfd.token_id()),
            Self::RemoteVfs(remote) => {
                HandleToken::from_nonreusable_open_description(HandleOwner::Io, remote.token_id())
                    .expect("remote VFS open-description token is nonzero and nonreusable")
            }
            Self::Socket(socket) => HandleToken::new(HandleOwner::Compat, socket.token_id()),
            Self::VfsDirectory(directory) => {
                HandleToken::new(HandleOwner::Io, directory.token_id())
            }
            Self::DisplaySurface(surface) => {
                HandleToken::new(HandleOwner::Io, surface.generation())
            }
            Self::DeviceBuffer(buffer) => HandleToken::new(HandleOwner::DeviceBuffer, buffer.identity().allocation),
        }
    }

    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::Job(_) => "job",
            Self::Console(_) => "console",
            Self::Device(_) => "device",
            Self::Epoll(_) => "epoll",
            Self::EventFd(_) => "eventfd",
            Self::Pipe(_) => "pipe",
            Self::InetSocket(_) => "inet-socket",
            Self::Memfd(_) => "memfd",
            Self::RemoteVfs(_) => "remote-vfs",
            Self::Socket(_) => "socket",
            Self::VfsDirectory(_) => "vfs-dir",
            Self::DisplaySurface(_) => "display-surface",
            Self::DeviceBuffer(_) => "device-buffer",
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
            // Rewrapping a raw object must not recreate attenuated authority.
            Self::Job(_) => HandleRights::Job(JobHandleRights::empty()),
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
            // Anonymous kernel-local objects are not transferable (no
            // SCM_RIGHTS) and carry only the access their description allows.
            Self::EventFd(_) => HandleRights::File(
                FileHandleRights::READ.union(FileHandleRights::WRITE),
            ),
            Self::Pipe(pipe) => HandleRights::File(match pipe.role() {
                crate::user::pipe::PipeRole::Read => FileHandleRights::READ,
                crate::user::pipe::PipeRole::Write => FileHandleRights::WRITE,
            }),
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
            Self::DeviceBuffer(buffer) => {
                let mut rights = SharedRegionRights::empty();
                if buffer.rights().contains(BufferRights::READ) {
                    rights = rights.union(SharedRegionRights::READ).union(SharedRegionRights::MAP);
                }
                if buffer.rights().contains(BufferRights::WRITE) {
                    rights = rights.union(SharedRegionRights::WRITE);
                }
                if buffer.rights().contains(BufferRights::TRANSFER) {
                    rights = rights.union(SharedRegionRights::TRANSFER);
                }
                HandleRights::DeviceBuffer(rights)
            },
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
            Self::Job(_)
                | Self::Socket(_)
                | Self::Memfd(_)
                | Self::VfsDirectory(_)
                | Self::Device(_)
                | Self::RemoteVfs(_)
                | Self::DeviceBuffer(_)
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
            Self::Job(job) => alloc::format!("anon_inode:[rustos-job:{}]", job.to_raw()),
            Self::Console(_) => alloc::string::String::from("/dev/tty"),
            Self::Device(device) => alloc::string::String::from(device.device_id().path()),
            Self::Epoll(epoll) => alloc::string::String::from(epoll.path()),
            Self::EventFd(eventfd) => eventfd.path(),
            Self::Pipe(pipe) => pipe.path(),
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
            Self::DeviceBuffer(_) => alloc::format!("anon_inode:[rustos-device-buffer:{}]", token.object_id()),
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
