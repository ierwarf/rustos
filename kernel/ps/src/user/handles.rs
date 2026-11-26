use crate::io::device::DeviceHandle;
use crate::user::epoll::EpollHandle;
use crate::user::eventfd::EventFdHandle;
use crate::user::linux as linux_abi;
use crate::user::memfd::MemfdHandle;
use crate::user::pipe::PipeHandle;
use crate::user::timerfd::TimerFdHandle;
use crate::user::unix_socket::UnixSocketHandle;
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
/// `O_PATH` is a status flag fixed at open: it marks a descriptor that names a
/// location only, and `file_rights_from_status_flags` strips every right but
/// transfer from it.
const STATUS_FLAG_MASK: u64 = linux_abi::O_ACCMODE
    | linux_abi::O_APPEND
    | linux_abi::O_NONBLOCK
    | linux_abi::O_NOCTTY
    | linux_abi::O_PATH;

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
    TimerFd(TimerFdHandle),
    InetSocket(InetSocketHandle),
    Memfd(MemfdHandle),
    Pipe(PipeHandle),
    RemoteVfs(RemoteVfsHandle),
    UnixSocket(UnixSocketHandle),
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

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct RemoteVfsCursorState {
    cursor: u64,
    durable_cursor: u64,
    durable_epoch: u64,
}

/// Kernel-owned open-description cursor state for regular remote VFS files.
/// Shared across `dup`/`fork`/`SCM_RIGHTS` clones of one [`RemoteVfsHandle`].
pub struct RemoteVfsOpenDescription {
    cursor_state: TrackedSpinLock<RemoteVfsCursorState, { LockClass::LocalFdObject as u8 }>,
}

impl core::fmt::Debug for RemoteVfsOpenDescription {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RemoteVfsOpenDescription")
            .finish_non_exhaustive()
    }
}

impl RemoteVfsOpenDescription {
    pub const fn new(durable_epoch: u64) -> Self {
        Self {
            cursor_state: TrackedSpinLock::new(RemoteVfsCursorState {
                cursor: 0,
                durable_cursor: 0,
                durable_epoch,
            }),
        }
    }

    pub fn cursor(&self) -> u64 {
        self.cursor_state.lock().cursor
    }

    pub fn durable_cursor(&self) -> u64 {
        self.cursor_state.lock().durable_cursor
    }

    pub fn durable_epoch(&self) -> u64 {
        self.cursor_state.lock().durable_epoch
    }

    pub fn advance_cursor(&self, delta: usize) -> u64 {
        let mut state = self.cursor_state.lock();
        let next = state
            .cursor
            .saturating_add(delta as u64)
            .min(i64::MAX as u64);
        state.cursor = next;
        next
    }

    pub fn set_synced_cursor(&self, cursor: u64, durable_epoch: u64) {
        let bounded = cursor.min(i64::MAX as u64);
        let mut state = self.cursor_state.lock();
        state.cursor = bounded;
        state.durable_cursor = bounded;
        state.durable_epoch = durable_epoch;
    }

    pub fn is_dirty(&self, current_epoch: u64) -> bool {
        let state = self.cursor_state.lock();
        state.cursor != state.durable_cursor || state.durable_epoch != current_epoch
    }

    pub fn mark_settled(&self, settled_cursor: u64, durable_epoch: u64) {
        let bounded = settled_cursor.min(i64::MAX as u64);
        let mut state = self.cursor_state.lock();
        state.durable_cursor = bounded;
        state.durable_epoch = durable_epoch;
    }
}

#[derive(Debug, Clone)]
pub struct RemoteVfsHandle {
    token: u64,
    remote_id: u64,
    kind: RemoteVfsHandleKind,
    /// Shared by every dup/fork/transfer clone of the description, so a
    /// descriptor copy does not duplicate (up to `PATH_MAX`) the path.
    path: Arc<str>,
    len: u64,
    device_access: u16,
    library: LibraryBinding,
    description: Arc<RemoteVfsOpenDescription>,
}

impl PartialEq for RemoteVfsHandle {
    fn eq(&self, other: &Self) -> bool {
        self.token == other.token
            && self.remote_id == other.remote_id
            && self.kind == other.kind
            && self.path == other.path
            && self.len == other.len
            && self.device_access == other.device_access
            && self.library == other.library
            && Arc::ptr_eq(&self.description, &other.description)
    }
}

impl Eq for RemoteVfsHandle {}

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
    Inotify,
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
            path: Arc::from(path),
            len,
            device_access,
            library: LibraryBinding::Unknown,
            description: Arc::new(RemoteVfsOpenDescription::new(0)),
        })
    }

    pub fn new_with_epoch(
        remote_id: u64,
        kind: RemoteVfsHandleKind,
        path: String,
        len: u64,
        device_access: u16,
        durable_epoch: u64,
    ) -> Option<Self> {
        let handle = Self::new(remote_id, kind, path, len, device_access)?;
        handle.description.mark_settled(0, durable_epoch);
        Some(handle)
    }

    pub fn description(&self) -> &Arc<RemoteVfsOpenDescription> {
        &self.description
    }

    pub fn cursor(&self) -> u64 {
        self.description.cursor()
    }

    pub fn durable_cursor(&self) -> u64 {
        self.description.durable_cursor()
    }

    pub fn durable_epoch(&self) -> u64 {
        self.description.durable_epoch()
    }

    pub fn advance_cursor(&self, delta: usize) -> u64 {
        self.description.advance_cursor(delta)
    }

    pub fn set_synced_cursor(&self, cursor: u64, durable_epoch: u64) {
        self.description.set_synced_cursor(cursor, durable_epoch);
    }

    pub fn is_dirty(&self, current_epoch: u64) -> bool {
        self.description.is_dirty(current_epoch)
    }

    pub fn mark_settled(&self, settled_cursor: u64, durable_epoch: u64) {
        self.description.mark_settled(settled_cursor, durable_epoch);
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
        String::from(&*self.path)
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
            Self::TimerFd(timerfd) => HandleToken::from_nonreusable_open_description(
                HandleOwner::Ps,
                timerfd.token_id(),
            )
            .expect("timerfd open-description token is nonzero and nonreusable"),
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
            Self::UnixSocket(socket) => HandleToken::from_nonreusable_open_description(
                HandleOwner::Ps,
                socket.token_id(),
            )
            .expect("unix socket open-description token is nonzero and nonreusable"),
            Self::VfsDirectory(directory) => {
                HandleToken::new(HandleOwner::Io, directory.token_id())
            }
            Self::DisplaySurface(surface) => {
                HandleToken::new(HandleOwner::Io, surface.generation())
            }
            Self::DeviceBuffer(buffer) => HandleToken::new(HandleOwner::DeviceBuffer, buffer.identity().allocation),
        }
    }

    #[cfg(test)]
    pub fn console_stream(&self) -> Option<ConsoleStreamKind> {
        match self {
            Self::Console(console) => Some(console.stream()),
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
            Self::EventFd(_) => HandleRights::File(
                FileHandleRights::READ.union(FileHandleRights::WRITE),
            ),
            Self::TimerFd(_) => HandleRights::File(FileHandleRights::READ),
            Self::Pipe(pipe) => {
                let access = match pipe.role() {
                    crate::user::pipe::PipeRole::Read => FileHandleRights::READ,
                    crate::user::pipe::PipeRole::Write => FileHandleRights::WRITE,
                };
                HandleRights::File(access.union(FileHandleRights::TRANSFER))
            },
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
                RemoteVfsHandleKind::Directory if status_flags & linux_abi::O_PATH != 0 => {
                    HandleRights::File(FileHandleRights::TRANSFER)
                }
                RemoteVfsHandleKind::Directory => {
                    HandleRights::File(FileHandleRights::READ.union(FileHandleRights::TRANSFER))
                }
                RemoteVfsHandleKind::Device if status_flags & linux_abi::O_PATH != 0 => {
                    HandleRights::File(FileHandleRights::TRANSFER)
                }
                RemoteVfsHandleKind::Device => HandleRights::File(
                    FileHandleRights::READ
                        .union(FileHandleRights::WRITE)
                        .union(FileHandleRights::TRANSFER),
                ),
                RemoteVfsHandleKind::Inotify => {
                    HandleRights::File(FileHandleRights::READ.union(FileHandleRights::TRANSFER))
                }
            },
            Self::UnixSocket(_) => HandleRights::Socket(
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
                | Self::UnixSocket(_)
                | Self::Memfd(_)
                | Self::Pipe(_)
                | Self::VfsDirectory(_)
                | Self::Device(_)
                | Self::RemoteVfs(_)
                | Self::DeviceBuffer(_)
        ) && rights.allows_transfer()
    }
}

fn file_rights_from_status_flags(status_flags: u64) -> FileHandleRights {
    let mut rights = FileHandleRights::TRANSFER;
    if status_flags & linux_abi::O_PATH != 0 {
        return rights;
    }
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
