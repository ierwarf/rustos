use alloc::string::String;

pub type ConsoleSessionHandle = kernel_base::io::session::ConsoleSessionHandle;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootstrapPhase {
    EarlyBootstrap,
    CoreHostsLaunching,
    KernelVfsReady,
    UserspaceReady,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VfsError {
    BadFileDescriptor,
    InvalidArgument,
    NotFound,
    NotDirectory,
    PermissionDenied,
    ReadOnlyFilesystem,
    Unsupported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MountError {
    Busy,
    InvalidArgument,
    InvalidSource,
    NotDirectory,
    NotFound,
    PermissionDenied,
    ReadOnlyFilesystem,
    UnsupportedFilesystem,
    UnsupportedMountFlags,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VfsNodeKind {
    File,
    Directory,
    Device,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VfsTimestamp {
    pub sec: i64,
    pub nsec: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VfsMetadata {
    pub inode: u64,
    pub kind: VfsNodeKind,
    pub len: u64,
    pub block_size: u64,
    pub blocks: u64,
    pub link_count: u64,
    pub atime: VfsTimestamp,
    pub mtime: VfsTimestamp,
    pub ctime: VfsTimestamp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockHandle {
    pub id: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockDescriptor {
    pub id: u32,
    pub path: String,
    pub transport: storage_core::TransportKind,
    pub readonly: bool,
    pub logical_block_size: usize,
    pub start_block: u64,
    pub block_count: u64,
}

fn map_vfs_error(error: kernel_base::vfs::VfsError) -> VfsError {
    match error {
        kernel_base::vfs::VfsError::BadFileDescriptor => VfsError::BadFileDescriptor,
        kernel_base::vfs::VfsError::InvalidArgument => VfsError::InvalidArgument,
        kernel_base::vfs::VfsError::NotFound => VfsError::NotFound,
        kernel_base::vfs::VfsError::NotDirectory => VfsError::NotDirectory,
        kernel_base::vfs::VfsError::PermissionDenied => VfsError::PermissionDenied,
        kernel_base::vfs::VfsError::ReadOnlyFilesystem => VfsError::ReadOnlyFilesystem,
        kernel_base::vfs::VfsError::Unsupported => VfsError::Unsupported,
    }
}

fn map_mount_error(error: kernel_base::vfs::MountError) -> MountError {
    match error {
        kernel_base::vfs::MountError::Busy => MountError::Busy,
        kernel_base::vfs::MountError::InvalidArgument => MountError::InvalidArgument,
        kernel_base::vfs::MountError::InvalidSource => MountError::InvalidSource,
        kernel_base::vfs::MountError::NotDirectory => MountError::NotDirectory,
        kernel_base::vfs::MountError::NotFound => MountError::NotFound,
        kernel_base::vfs::MountError::PermissionDenied => MountError::PermissionDenied,
        kernel_base::vfs::MountError::ReadOnlyFilesystem => MountError::ReadOnlyFilesystem,
        kernel_base::vfs::MountError::UnsupportedFilesystem => MountError::UnsupportedFilesystem,
        kernel_base::vfs::MountError::UnsupportedMountFlags => MountError::UnsupportedMountFlags,
    }
}

fn map_node_kind(kind: kernel_base::vfs::VfsNodeKind) -> VfsNodeKind {
    match kind {
        kernel_base::vfs::VfsNodeKind::File => VfsNodeKind::File,
        kernel_base::vfs::VfsNodeKind::Directory => VfsNodeKind::Directory,
        kernel_base::vfs::VfsNodeKind::Device => VfsNodeKind::Device,
    }
}

fn map_timestamp(ts: kernel_base::vfs::VfsTimestamp) -> VfsTimestamp {
    VfsTimestamp {
        sec: ts.sec,
        nsec: ts.nsec,
    }
}

fn map_metadata(metadata: kernel_base::vfs::VfsMetadata) -> VfsMetadata {
    VfsMetadata {
        inode: metadata.inode,
        kind: map_node_kind(metadata.kind),
        len: metadata.len,
        block_size: metadata.block_size,
        blocks: metadata.blocks,
        link_count: metadata.link_count,
        atime: map_timestamp(metadata.atime),
        mtime: map_timestamp(metadata.mtime),
        ctime: map_timestamp(metadata.ctime),
    }
}

fn map_block_descriptor(
    descriptor: kernel_base::storage::block::BlockDeviceDescriptor,
) -> BlockDescriptor {
    BlockDescriptor {
        id: descriptor.id,
        path: descriptor.path,
        transport: descriptor.transport,
        readonly: descriptor.readonly,
        logical_block_size: descriptor.logical_block_size,
        start_block: descriptor.start_block,
        block_count: descriptor.block_count,
    }
}

fn map_bootstrap_phase(phase: kernel_base::storage::boot_volume::BootstrapPhase) -> BootstrapPhase {
    match phase {
        kernel_base::storage::boot_volume::BootstrapPhase::EarlyBootstrap => {
            BootstrapPhase::EarlyBootstrap
        }
        kernel_base::storage::boot_volume::BootstrapPhase::CoreHostsLaunching => {
            BootstrapPhase::CoreHostsLaunching
        }
        kernel_base::storage::boot_volume::BootstrapPhase::KernelVfsReady => {
            BootstrapPhase::KernelVfsReady
        }
        kernel_base::storage::boot_volume::BootstrapPhase::UserspaceReady => {
            BootstrapPhase::UserspaceReady
        }
    }
}

pub mod api {
    use super::*;
    use boot_protocol::{BootInfo, BootVolumeIdentity, BootVolumeTransport};
    use driver_abi::DriverClass;

    pub fn init_gui(boot_info_ptr: *const BootInfo) {
        kernel_base::io::gui::init(boot_info_ptr);
    }

    pub fn boot_volume_transport_hint() -> Option<BootVolumeTransport> {
        kernel_base::storage::boot_volume::boot_volume_transport_hint()
    }

    pub fn init_boot_info(boot_info_ptr: *const BootInfo) {
        kernel_base::storage::boot_volume::init_boot_info(boot_info_ptr);
    }

    pub fn boot_volume_identity() -> Option<BootVolumeIdentity> {
        kernel_base::storage::boot_volume::boot_volume_identity()
    }

    pub fn register_boot_volume_opener() {
        kernel_base::storage::block::register_boot_volume_opener();
    }

    pub fn init_block_devices() {
        kernel_base::storage::block::init();
    }

    pub fn init_vfs() {
        kernel_base::vfs::init();
    }

    pub fn initialize_loadable_modules_for_class(class: DriverClass) -> bool {
        kernel_base::driver::initialize_loadable_modules_for_class(class)
    }

    pub fn init_usb() {
        kernel_base::usb::init();
    }

    pub fn init_input() {
        kernel_base::input::init();
    }

    pub fn init_console() {
        kernel_base::io::console::init();
    }

    pub fn init_tty() {
        kernel_base::io::tty::init();
    }

    pub fn write_console(bytes: &[u8]) {
        kernel_base::io::console::write(bytes);
    }

    pub fn service_input_pending() -> usize {
        kernel_base::input::service_pending()
    }

    pub fn service_console() -> usize {
        kernel_base::io::console::service()
    }

    pub const fn system_console_session() -> ConsoleSessionHandle {
        kernel_base::io::session::ConsoleSessionHandle::SYSTEM
    }

    pub fn bootstrap_phase() -> BootstrapPhase {
        map_bootstrap_phase(kernel_base::storage::boot_volume::bootstrap_phase())
    }

    pub fn userspace_ready() -> bool {
        kernel_base::storage::boot_volume::userspace_runtime_active()
    }

    pub fn enter_kernel_vfs_runtime() {
        kernel_base::storage::boot_volume::enter_kernel_vfs_runtime();
    }

    pub fn enter_userspace_runtime() {
        kernel_base::storage::boot_volume::enter_userspace_runtime();
    }

    pub fn path_inode(path: &[u8]) -> u64 {
        crate::vfs_core::path_inode(path)
    }

    pub fn mount_for_current_process(
        source_path: &str,
        target_path: &str,
        filesystem_type: &str,
        flags: u64,
        options: Option<&str>,
    ) -> Result<(), MountError> {
        kernel_base::vfs::mount_for_current_process(
            source_path,
            target_path,
            filesystem_type,
            flags,
            options,
        )
        .map_err(map_mount_error)
    }

    pub fn umount_for_current_process(target_path: &str) -> Result<(), MountError> {
        kernel_base::vfs::umount_for_current_process(target_path).map_err(map_mount_error)
    }

    pub fn open_path_for_current_process(
        absolute_path: &str,
        flags: u64,
        mode: u64,
    ) -> Result<u64, VfsError> {
        kernel_base::vfs::open_path_for_current_process(absolute_path, flags, mode)
            .map_err(map_vfs_error)
    }

    pub fn metadata_for_current_process_path(absolute_path: &str) -> Result<VfsMetadata, VfsError> {
        kernel_base::vfs::metadata_for_current_process_path(absolute_path)
            .map(map_metadata)
            .map_err(map_vfs_error)
    }

    pub fn block_descriptors() -> alloc::vec::Vec<BlockDescriptor> {
        kernel_base::storage::block::descriptors()
            .into_iter()
            .map(map_block_descriptor)
            .collect()
    }

    pub fn lookup_block(path: &str) -> Option<BlockHandle> {
        kernel_base::storage::block::lookup(path).map(|handle| BlockHandle { id: handle.id() })
    }
}
