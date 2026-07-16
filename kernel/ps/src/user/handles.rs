use crate::io::device::DeviceHandle;
use crate::ipc::KernelSharedRegionHandle;
use crate::user::epoll::EpollHandle;
use crate::user::linux as linux_abi;
use crate::user::memfd::MemfdHandle;
use crate::user::socket::SocketHandle;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use kernel_ipc_runtime::api::KernelTransferredHandle;
use kernel_object::api::handle::{
    DeviceHandleRights, FileHandleRights, HandleOwner, HandleRights, HandleToken,
    SharedRegionRights, SocketHandleRights,
};
use lazy_static::lazy_static;
use spin::Mutex;

#[path = "handles/display_surface.rs"]
mod display_surface;
#[path = "handles/table.rs"]
mod table;
#[path = "handles/vfs.rs"]
mod vfs;

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

// The pending transfer registry belongs to the process/handle substrate rather
// than compat. Endpoint cancellation and task exit run below compat, so they
// must be able to discard opaque transfer descriptors without a reverse
// kernel_ps -> kernel_compat dependency.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IpcTransferRegistryError {
    Exhausted,
    InvalidDescriptor,
    StaleDescriptor,
}

lazy_static! {
    static ref IPC_TRANSFER_OBJECTS: Mutex<BTreeMap<u64, TransferredHandleEntry>> =
        Mutex::new(BTreeMap::new());
}

static NEXT_IPC_TRANSFER_ID: AtomicU64 = AtomicU64::new(1);

pub fn register_ipc_transfer_entries(
    entries: Vec<TransferredHandleEntry>,
) -> Result<Vec<KernelTransferredHandle>, IpcTransferRegistryError> {
    let mut objects = IPC_TRANSFER_OBJECTS.lock();
    if objects.len().saturating_add(entries.len()) > MAX_PENDING_IPC_TRANSFER_OBJECTS {
        return Err(IpcTransferRegistryError::Exhausted);
    }

    let mut inserted_ids = Vec::with_capacity(entries.len());
    let mut descriptors = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(transfer_id) = allocate_ipc_transfer_id(&objects) else {
            for transfer_id in inserted_ids {
                objects.remove(&transfer_id);
            }
            return Err(IpcTransferRegistryError::Exhausted);
        };
        let Some(descriptor) = entry.ipc_descriptor(transfer_id) else {
            for transfer_id in inserted_ids {
                objects.remove(&transfer_id);
            }
            return Err(IpcTransferRegistryError::InvalidDescriptor);
        };
        objects.insert(transfer_id, entry);
        inserted_ids.push(transfer_id);
        descriptors.push(descriptor);
    }
    Ok(descriptors)
}

pub fn take_ipc_transfer_entries(
    descriptors: &[KernelTransferredHandle],
) -> Result<Vec<TransferredHandleEntry>, IpcTransferRegistryError> {
    let mut objects = IPC_TRANSFER_OBJECTS.lock();
    for (index, descriptor) in descriptors.iter().enumerate() {
        if descriptors[..index]
            .iter()
            .any(|prior| prior.transfer_id() == descriptor.transfer_id())
        {
            return Err(IpcTransferRegistryError::InvalidDescriptor);
        }
        let Some(entry) = objects.get(&descriptor.transfer_id()) else {
            return Err(IpcTransferRegistryError::StaleDescriptor);
        };
        if entry.ipc_descriptor(descriptor.transfer_id()) != Some(*descriptor) {
            return Err(IpcTransferRegistryError::InvalidDescriptor);
        }
    }

    let mut entries = Vec::with_capacity(descriptors.len());
    for descriptor in descriptors {
        let entry = objects
            .remove(&descriptor.transfer_id())
            .expect("validated IPC transfer descriptor disappeared while taking");
        entries.push(entry);
    }
    Ok(entries)
}

pub fn drop_ipc_transfer_descriptors(descriptors: &[KernelTransferredHandle]) {
    if descriptors.is_empty() {
        return;
    }
    let mut objects = IPC_TRANSFER_OBJECTS.lock();
    for descriptor in descriptors {
        objects.remove(&descriptor.transfer_id());
    }
}

fn allocate_ipc_transfer_id(objects: &BTreeMap<u64, TransferredHandleEntry>) -> Option<u64> {
    for _ in 0..MAX_PENDING_IPC_TRANSFER_OBJECTS {
        let id = NEXT_IPC_TRANSFER_ID.fetch_add(1, Ordering::Relaxed);
        if id != 0 && !objects.contains_key(&id) {
            return Some(id);
        }
    }
    None
}

#[derive(Debug, Clone)]
pub enum KernelHandle {
    Console(ConsoleStreamKind),
    Device(DeviceHandle),
    Epoll(EpollHandle),
    InetSocket(InetSocketHandle),
    Memfd(MemfdHandle),
    RemoteVfs(RemoteVfsHandle),
    SharedRegion(KernelSharedRegionHandle),
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
    ) -> Self {
        static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);
        Self {
            token: NEXT_TOKEN.fetch_add(1, Ordering::Relaxed),
            remote_id,
            kind,
            path,
            len,
            device_access,
        }
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
    pub fn new(domain: u64, type_: u64, protocol: u64) -> Self {
        static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);
        Self {
            token: NEXT_TOKEN.fetch_add(1, Ordering::Relaxed),
            domain,
            type_,
            protocol,
        }
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
            Self::Console(stream) => HandleToken::new(
                HandleOwner::Ps,
                match stream {
                    ConsoleStreamKind::Input => 0,
                    ConsoleStreamKind::Output => 1,
                    ConsoleStreamKind::Error => 2,
                },
            ),
            Self::Device(device) => HandleToken::new(
                HandleOwner::Io,
                ((match device.device_id() {
                    crate::io::device::DeviceId::Console => 0_u64,
                    crate::io::device::DeviceId::Display => 1_u64,
                    crate::io::device::DeviceId::Input => 2_u64,
                }) << 8)
                    | match device.access_kind() {
                        crate::io::device::DeviceAccessKind::Native => 0_u64,
                        crate::io::device::DeviceAccessKind::Evdev => 1_u64,
                    },
            ),
            Self::Epoll(epoll) => HandleToken::new(HandleOwner::Compat, epoll.token_id()),
            Self::InetSocket(socket) => HandleToken::new(HandleOwner::Compat, socket.token_id()),
            Self::Memfd(memfd) => HandleToken::new(HandleOwner::Compat, memfd.token_id()),
            Self::RemoteVfs(remote) => HandleToken::new(HandleOwner::Io, remote.token_id()),
            Self::SharedRegion(region) => HandleToken::new(HandleOwner::Ipc, region.raw()),
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
            Self::SharedRegion(_) => "ipc-region",
            Self::Socket(_) => "socket",
            Self::VfsDirectory(_) => "vfs-dir",
            Self::DisplaySurface(_) => "display-surface",
        }
    }

    pub const fn console_stream(&self) -> Option<ConsoleStreamKind> {
        match self {
            Self::Console(stream) => Some(*stream),
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
            Self::SharedRegion(_) => HandleRights::SharedRegion(
                SharedRegionRights::READ
                    .union(SharedRegionRights::WRITE)
                    .union(SharedRegionRights::MAP),
            ),
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
            Self::SharedRegion(_) => {
                alloc::format!("anon_inode:[rustos-ipc-region:{}]", token.object_id())
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
