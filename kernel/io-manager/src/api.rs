use alloc::string::String;

pub type ConsoleSessionHandle = crate::io::session::ConsoleSessionHandle;

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

fn map_vfs_error(error: crate::vfs::VfsError) -> VfsError {
    match error {
        crate::vfs::VfsError::BadFileDescriptor => VfsError::BadFileDescriptor,
        crate::vfs::VfsError::InvalidArgument => VfsError::InvalidArgument,
        crate::vfs::VfsError::NotFound => VfsError::NotFound,
        crate::vfs::VfsError::NotDirectory => VfsError::NotDirectory,
        crate::vfs::VfsError::PermissionDenied => VfsError::PermissionDenied,
        crate::vfs::VfsError::ReadOnlyFilesystem => VfsError::ReadOnlyFilesystem,
        crate::vfs::VfsError::Unsupported => VfsError::Unsupported,
    }
}

fn map_mount_error(error: crate::vfs::MountError) -> MountError {
    match error {
        crate::vfs::MountError::Busy => MountError::Busy,
        crate::vfs::MountError::InvalidArgument => MountError::InvalidArgument,
        crate::vfs::MountError::InvalidSource => MountError::InvalidSource,
        crate::vfs::MountError::NotDirectory => MountError::NotDirectory,
        crate::vfs::MountError::NotFound => MountError::NotFound,
        crate::vfs::MountError::PermissionDenied => MountError::PermissionDenied,
        crate::vfs::MountError::ReadOnlyFilesystem => MountError::ReadOnlyFilesystem,
        crate::vfs::MountError::UnsupportedFilesystem => MountError::UnsupportedFilesystem,
        crate::vfs::MountError::UnsupportedMountFlags => MountError::UnsupportedMountFlags,
    }
}

fn map_node_kind(kind: crate::vfs::VfsNodeKind) -> VfsNodeKind {
    match kind {
        crate::vfs::VfsNodeKind::File => VfsNodeKind::File,
        crate::vfs::VfsNodeKind::Directory => VfsNodeKind::Directory,
        crate::vfs::VfsNodeKind::Device => VfsNodeKind::Device,
    }
}

fn map_timestamp(ts: crate::vfs::VfsTimestamp) -> VfsTimestamp {
    VfsTimestamp {
        sec: ts.sec,
        nsec: ts.nsec,
    }
}

fn map_metadata(metadata: crate::vfs::VfsMetadata) -> VfsMetadata {
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
    descriptor: crate::storage::block::BlockDeviceDescriptor,
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

fn map_bootstrap_phase(phase: crate::storage::boot_volume::BootstrapPhase) -> BootstrapPhase {
    match phase {
        crate::storage::boot_volume::BootstrapPhase::EarlyBootstrap => {
            BootstrapPhase::EarlyBootstrap
        }
        crate::storage::boot_volume::BootstrapPhase::CoreHostsLaunching => {
            BootstrapPhase::CoreHostsLaunching
        }
        crate::storage::boot_volume::BootstrapPhase::KernelVfsReady => {
            BootstrapPhase::KernelVfsReady
        }
        crate::storage::boot_volume::BootstrapPhase::UserspaceReady => {
            BootstrapPhase::UserspaceReady
        }
    }
}

pub mod block {
    use super::{BlockDescriptor, BlockHandle, map_block_descriptor};

    pub fn register_boot_volume_opener() {
        crate::storage::block::register_boot_volume_opener();
    }

    pub fn init_block_devices() {
        crate::storage::block::init();
    }

    pub fn descriptors() -> alloc::vec::Vec<BlockDescriptor> {
        crate::storage::block::descriptors()
            .into_iter()
            .map(map_block_descriptor)
            .collect()
    }

    pub fn lookup(path: &str) -> Option<BlockHandle> {
        crate::storage::block::lookup(path).map(|handle| BlockHandle { id: handle.id() })
    }
}

pub mod boot {
    use super::{BootstrapPhase, ConsoleSessionHandle, map_bootstrap_phase};
    use boot_protocol::{BootInfo, BootVolumeIdentity, BootVolumeTransport};

    pub fn init_gui(boot_info_ptr: *const BootInfo) {
        crate::io::gui::init(boot_info_ptr);
    }

    pub fn boot_volume_transport_hint() -> Option<BootVolumeTransport> {
        crate::storage::boot_volume::boot_volume_transport_hint()
    }

    pub fn init_boot_info(boot_info_ptr: *const BootInfo) {
        crate::storage::boot_volume::init_boot_info(boot_info_ptr);
    }

    pub fn boot_volume_identity() -> Option<BootVolumeIdentity> {
        crate::storage::boot_volume::boot_volume_identity()
    }

    pub fn bootstrap_phase() -> BootstrapPhase {
        map_bootstrap_phase(crate::storage::boot_volume::bootstrap_phase())
    }

    pub fn userspace_ready() -> bool {
        crate::storage::boot_volume::userspace_runtime_active()
    }

    pub fn enter_kernel_vfs_runtime() {
        crate::storage::boot_volume::enter_kernel_vfs_runtime();
    }

    pub fn enter_userspace_runtime() {
        crate::storage::boot_volume::enter_userspace_runtime();
    }

    pub const fn system_console_session() -> ConsoleSessionHandle {
        crate::io::session::ConsoleSessionHandle::SYSTEM
    }
}

pub mod console {
    pub use crate::io::console::*;

    pub fn init() {
        crate::io::console::init();
    }

    pub fn init_tty() {
        crate::io::tty::init();
    }

    pub fn write(bytes: &[u8]) {
        crate::io::console::write(bytes);
    }

    pub fn service() -> usize {
        crate::io::console::service()
    }
}

pub mod driver {
    pub mod irq {
        pub fn dispatch_pic_irq(irq: u8) -> bool {
            crate::driver::irq::dispatch_pic_irq(irq)
        }
    }

    pub mod linux {
        pub mod input {
            pub fn debug_lock_snapshot() -> (usize, u64) {
                crate::driver::linux::input::debug_lock_snapshot()
            }

            pub fn consumer_acquire() {
                crate::driver::linux::input::consumer_acquire();
            }

            pub fn consumer_release() {
                crate::driver::linux::input::consumer_release();
            }
        }

        pub mod runtime {
            pub fn debug_irq_lock_snapshot() -> (usize, usize) {
                crate::driver::linux::runtime::debug_irq_lock_snapshot()
            }

            pub fn service_compat_pending() {
                crate::driver::linux::runtime::service_compat_pending();
            }

            pub fn tick_jiffies(delta: u64) -> u64 {
                crate::driver::linux::runtime::tick_jiffies(delta)
            }
        }
    }

    use driver_abi::DriverClass;

    pub fn initialize_loadable_modules_for_class(class: DriverClass) -> bool {
        crate::driver::initialize_loadable_modules_for_class(class)
    }

    pub fn init_linux_cpu_local_symbols() {
        crate::driver::linux::init_cpu_local_symbols();
    }

    pub fn service_compat_pending() {
        crate::driver::linux::runtime::service_compat_pending();
    }
}

pub mod input {
    pub mod event_queue {
        pub use crate::input::event_queue::InputEventQueueDebugSnapshot;

        pub fn debug_snapshot() -> InputEventQueueDebugSnapshot {
            crate::input::event_queue::debug_snapshot()
        }
    }

    pub fn init() {
        crate::input::init();
    }

    pub fn on_keyboard_interrupt() {
        crate::input::on_keyboard_interrupt();
    }

    pub fn on_mouse_interrupt() {
        crate::input::on_mouse_interrupt();
    }

    pub fn serio_lower_half_service_pending() -> usize {
        crate::input::serio_lower_half_service_pending()
    }

    pub fn service_pending() -> usize {
        crate::input::service_pending()
    }
}

pub mod device {
    pub use crate::io::device::{
        DeviceAccessKind, DeviceError, DeviceHandle, DeviceId, DeviceLookupError,
    };

    pub fn open(path: &str) -> Result<DeviceHandle, DeviceLookupError> {
        crate::io::device::open(path)
    }

    pub fn read_to_current_user(
        handle: DeviceHandle,
        user_ptr: u64,
        user_len: usize,
    ) -> Result<usize, DeviceError> {
        crate::io::device::read_to_current_user(handle, user_ptr, user_len)
    }

    pub fn read_to_user(
        handle: DeviceHandle,
        process_state: &mut crate::user::process_state::UserProcessState,
        user_ptr: u64,
        user_len: usize,
    ) -> Result<usize, DeviceError> {
        crate::io::device::read_to_user(handle, process_state, user_ptr, user_len)
    }

    pub fn ioctl_from_user(
        handle: DeviceHandle,
        process_state: &mut crate::user::process_state::UserProcessState,
        request: u64,
        arg: u64,
    ) -> Result<u64, DeviceError> {
        crate::io::device::ioctl_from_user(handle, process_state, request, arg)
    }

    pub mod input {
        pub fn has_pending_events() -> bool {
            crate::io::device::input::has_pending_events()
        }
    }
}

pub mod tty {
    pub type LinuxTermios = crate::user::linux::LinuxTermios;
    pub use crate::io::session::ConsoleSessionHandle;

    pub fn has_pending_input_for_session(session: ConsoleSessionHandle) -> bool {
        crate::io::tty::has_pending_input_for_session(session)
    }

    pub fn pending_input_len_for_session(session: ConsoleSessionHandle) -> usize {
        crate::io::tty::pending_input_len_for_session(session)
    }

    pub fn termios_for_session(session: ConsoleSessionHandle) -> LinuxTermios {
        crate::io::tty::termios_for_session(session)
    }

    pub fn set_termios_for_session(
        session: ConsoleSessionHandle,
        termios: LinuxTermios,
        flush_input: bool,
    ) {
        crate::io::tty::set_termios_for_session(session, termios, flush_input);
    }

    pub fn write_to_session(session: ConsoleSessionHandle, bytes: &[u8]) -> usize {
        crate::io::tty::write_to_session(session, bytes)
    }

    pub fn read_input_for_session(session: ConsoleSessionHandle, dest: &mut [u8]) -> usize {
        crate::io::tty::read_input_for_session(session, dest)
    }

    pub fn read_input_blocking_for_session(
        session: ConsoleSessionHandle,
        dest: &mut [u8],
    ) -> usize {
        crate::io::tty::read_input_blocking_for_session(session, dest)
    }
}

pub mod session {
    pub use crate::io::session::ConsoleSessionHandle;

    pub const fn system_console_session() -> ConsoleSessionHandle {
        crate::io::session::ConsoleSessionHandle::SYSTEM
    }
}

pub mod io {
    pub mod gui {
        pub fn is_userspace_display_active() -> bool {
            crate::io::gui::is_userspace_display_active()
        }

        pub fn try_present_panic_blackout() -> bool {
            crate::io::gui::try_present_panic_blackout()
        }

        pub fn flush_debug_console() {
            crate::io::gui::flush_debug_console();
        }
    }
}

pub mod usb {
    pub fn debug_pointer_report_count() -> u64 {
        crate::usb::debug_pointer_report_count()
    }

    pub fn debug_transfer_event_count() -> u64 {
        crate::usb::debug_transfer_event_count()
    }

    pub fn init() {
        crate::usb::init();
    }
}

pub mod vfs {
    use super::{MountError, VfsError, VfsMetadata, map_metadata, map_mount_error, map_vfs_error};

    pub fn init() {
        crate::vfs::init();
    }

    pub fn path_inode(path: &[u8]) -> u64 {
        crate::vfs_core::path_inode(path)
    }

    pub fn normalize_kernel_path(path: &str) -> Result<alloc::string::String, VfsError> {
        crate::vfs::normalize_kernel_path(path).map_err(map_vfs_error)
    }

    pub fn mount_for_current_process(
        source_path: &str,
        target_path: &str,
        filesystem_type: &str,
        flags: u64,
        options: Option<&str>,
    ) -> Result<(), MountError> {
        crate::vfs::mount_for_current_process(
            source_path,
            target_path,
            filesystem_type,
            flags,
            options,
        )
        .map_err(map_mount_error)
    }

    pub fn umount_for_current_process(target_path: &str) -> Result<(), MountError> {
        crate::vfs::umount_for_current_process(target_path).map_err(map_mount_error)
    }

    pub fn open_path_for_current_process(
        absolute_path: &str,
        flags: u64,
        mode: u64,
    ) -> Result<u64, VfsError> {
        crate::vfs::open_path_for_current_process(absolute_path, flags, mode).map_err(map_vfs_error)
    }

    pub fn metadata_for_current_process_path(absolute_path: &str) -> Result<VfsMetadata, VfsError> {
        crate::vfs::metadata_for_current_process_path(absolute_path)
            .map(map_metadata)
            .map_err(map_vfs_error)
    }

    pub fn check_access_for_current_process(
        absolute_path: &str,
        mode: u64,
    ) -> Result<(), VfsError> {
        crate::vfs::check_access_for_current_process(absolute_path, mode).map_err(map_vfs_error)
    }

    pub fn read_path_to_vec_for_kernel(absolute_path: &str) -> Result<alloc::vec::Vec<u8>, VfsError> {
        crate::vfs::read_path_to_vec_for_kernel(absolute_path).map_err(map_vfs_error)
    }

    pub fn readlink_for_current_process(absolute_path: &str) -> Result<alloc::string::String, VfsError> {
        crate::vfs::readlink_for_current_process(absolute_path).map_err(map_vfs_error)
    }
}

pub use block::{
    descriptors as block_descriptors, init_block_devices, lookup as lookup_block,
    register_boot_volume_opener,
};
pub use boot::{
    boot_volume_identity, boot_volume_transport_hint, bootstrap_phase, enter_kernel_vfs_runtime,
    enter_userspace_runtime, init_boot_info, init_gui, system_console_session, userspace_ready,
};
pub use console::{
    init as init_console, init_tty, service as service_console, write as write_console,
};
pub use driver::initialize_loadable_modules_for_class;
pub use input::{init as init_input, service_pending as service_input_pending};
pub use usb::init as init_usb;
pub use vfs::{
    check_access_for_current_process, init as init_vfs, metadata_for_current_process_path,
    mount_for_current_process, open_path_for_current_process, path_inode,
    umount_for_current_process,
};
