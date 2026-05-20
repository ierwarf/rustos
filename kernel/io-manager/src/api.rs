use alloc::string::{String, ToString};
use alloc::vec::Vec;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BootPathExtent {
    pub disk_offset: u64,
    pub len: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BootPathExtentLease {
    pub file_len: u64,
    pub generation: u64,
    pub extents: Vec<BootPathExtent>,
}

fn map_block_descriptor(
    descriptor: crate::storage::block::BlockDeviceDescriptor,
) -> BlockDescriptor {
    BlockDescriptor {
        id: descriptor.id,
        path: descriptor.path.to_string(),
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
    use storage_core::BlockDevice;

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

    pub fn boot_volume_descriptor() -> Option<BlockDescriptor> {
        crate::storage::block::current_boot_volume_handle()
            .and_then(crate::storage::block::descriptor)
            .map(map_block_descriptor)
    }

    pub fn read_boot_volume_blocks(lba: u64, out: &mut [u8]) -> storage_core::IoResult<()> {
        let handle = crate::storage::block::current_boot_volume_handle()
            .ok_or(storage_core::StorageError::NotPresent)?;
        let mut device = crate::storage::block::FatRegistryDevice::new(handle);
        device.read_blocks(lba, out)
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
            pub fn consumer_acquire() {
                crate::driver::linux::input::consumer_acquire();
            }

            pub fn consumer_release() {
                crate::driver::linux::input::consumer_release();
            }

            pub fn debug_lock_snapshot() -> (usize, u64) {
                crate::driver::linux::input::debug_lock_snapshot()
            }
        }

        pub mod runtime {
            pub fn debug_irq_lock_snapshot() -> (usize, usize) {
                (0, 0)
            }

            pub fn tick_jiffies(delta: u64) -> u64 {
                delta
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
}

pub mod input {
    pub mod event_queue {
        pub use crate::input::event_queue::InputEventQueueDebugSnapshot;

        pub fn debug_snapshot() -> InputEventQueueDebugSnapshot {
            crate::input::event_queue::debug_snapshot()
        }

        pub fn drain_events(dest: &mut [crate::user::abi::device::InputEvent]) -> usize {
            crate::input::event_queue::read_input_events(dest)
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

    pub fn service_pending() -> usize {
        crate::input::service_pending()
    }
}

pub mod device {
    pub use crate::io::device::{DeviceAccessKind, DeviceError, DeviceHandle, DeviceId};

    pub fn read_to_current_user(
        handle: kernel_object::api::device::DeviceHandle,
        user_ptr: u64,
        user_len: usize,
    ) -> Result<usize, DeviceError> {
        crate::io::device::read_to_current_user(handle.into(), user_ptr, user_len)
    }

    pub fn read_to_user(
        handle: kernel_object::api::device::DeviceHandle,
        process_state: &mut crate::user::process_state::UserProcessState,
        user_ptr: u64,
        user_len: usize,
    ) -> Result<usize, DeviceError> {
        crate::io::device::read_to_user(handle.into(), process_state, user_ptr, user_len)
    }

    pub fn ioctl_from_user(
        handle: kernel_object::api::device::DeviceHandle,
        process_state: &mut crate::user::process_state::UserProcessState,
        request: u64,
        arg: u64,
    ) -> Result<u64, DeviceError> {
        crate::io::device::ioctl_from_user(handle.into(), process_state, request, arg)
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

    pub const STATE_QUEUED: u16 = 1;
    pub const STATE_LOADING_IMAGE: u16 = 2;
    pub const STATE_SPAWNING: u16 = 3;
    pub const STATE_RUNNING: u16 = 4;
    pub const STATE_CLOSING: u16 = 5;

    pub const TITLE_CAPACITY: usize = crate::io::session::CONSOLE_SESSION_TITLE_CAPACITY;
    pub const PATH_CAPACITY: usize = crate::io::session::CONSOLE_SESSION_PATH_CAPACITY;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum CreateConsoleSessionError {
        NoCapacity,
    }

    pub fn create(
        program_id: u32,
        title: &str,
        exec_path: &str,
    ) -> Result<ConsoleSessionHandle, CreateConsoleSessionError> {
        crate::io::session::create_console_session(program_id, title, exec_path).map_err(|err| {
            match err {
                crate::io::session::CreateConsoleSessionError::NoCapacity => {
                    CreateConsoleSessionError::NoCapacity
                }
            }
        })
    }

    pub fn remove(handle: ConsoleSessionHandle) -> bool {
        crate::io::session::remove_console_session(handle)
    }

    pub fn transition_state(handle: ConsoleSessionHandle, state: u16) -> Option<bool> {
        let kernel_state = match state {
            STATE_QUEUED => crate::io::session::ConsoleSessionState::Queued,
            STATE_LOADING_IMAGE => crate::io::session::ConsoleSessionState::LoadingImage,
            STATE_SPAWNING => crate::io::session::ConsoleSessionState::Spawning,
            STATE_RUNNING => crate::io::session::ConsoleSessionState::Running,
            STATE_CLOSING => crate::io::session::ConsoleSessionState::Closing,
            _ => return None,
        };
        Some(crate::io::session::transition_console_session_state(
            handle,
            kernel_state,
        ))
    }

    pub fn set_focus(handle: ConsoleSessionHandle) -> bool {
        crate::io::session::set_focused_console_session(handle)
    }
}

pub mod io {
    pub mod gui {
        pub type GuiDisplayInfo = crate::io::gui::GuiDisplayInfo;

        pub fn display_info() -> Option<GuiDisplayInfo> {
            crate::io::gui::display_info()
        }

        pub fn is_userspace_display_active() -> bool {
            crate::io::gui::is_userspace_display_active()
        }

        pub fn present_userspace_frame_from_kernel_bgra8888(
            src_ptr: *const u8,
            width: usize,
            height: usize,
            stride_bytes: usize,
        ) -> bool {
            crate::io::gui::present_userspace_frame_from_kernel_bgra8888(
                src_ptr,
                width,
                height,
                stride_bytes,
            )
        }

        pub fn present_userspace_frame_rect_from_kernel_bgra8888(
            src_ptr: *const u8,
            width: usize,
            height: usize,
            stride_bytes: usize,
            x: usize,
            y: usize,
            rect_width: usize,
            rect_height: usize,
        ) -> bool {
            crate::io::gui::present_userspace_frame_rect_from_kernel_bgra8888(
                src_ptr,
                width,
                height,
                stride_bytes,
                x,
                y,
                rect_width,
                rect_height,
            )
        }

        pub fn try_present_panic_blackout() -> bool {
            crate::io::gui::try_present_panic_blackout()
        }

        pub fn write_panic_console_line(bytes: &[u8]) -> bool {
            crate::io::gui::write_panic_console_line(bytes)
        }

        pub fn flush_debug_console() {
            crate::io::gui::flush_debug_console();
        }

        pub fn service_pending() -> usize {
            crate::driver::virtio_gpu::service_pending()
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

    pub fn service_pending() -> usize {
        crate::usb::service_pending()
    }
}

pub mod vfs {
    use super::{BootPathExtent, BootPathExtentLease, MountError, VfsError, VfsMetadata};
    use alloc::string::ToString;
    use fatfs::Error as FatError;
    use storage_core::StorageError;

    pub fn init() {}

    pub fn path_inode(path: &[u8]) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for byte in path {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash.max(1)
    }

    pub fn normalize_kernel_path(path: &str) -> Result<alloc::string::String, VfsError> {
        if path.starts_with('/') {
            Ok(path.to_string())
        } else {
            Err(VfsError::InvalidArgument)
        }
    }

    pub fn mount_for_current_process(
        _source_path: &str,
        _target_path: &str,
        _filesystem_type: &str,
        _flags: u64,
        _options: Option<&str>,
    ) -> Result<(), MountError> {
        Err(MountError::UnsupportedFilesystem)
    }

    pub fn umount_for_current_process(_target_path: &str) -> Result<(), MountError> {
        Err(MountError::InvalidArgument)
    }

    pub fn open_path_for_current_process(
        _absolute_path: &str,
        _flags: u64,
        _mode: u64,
    ) -> Result<u64, VfsError> {
        Err(VfsError::Unsupported)
    }

    pub fn metadata_for_current_process_path(
        _absolute_path: &str,
    ) -> Result<VfsMetadata, VfsError> {
        Err(VfsError::Unsupported)
    }

    pub fn check_access_for_user_process(
        _absolute_path: &str,
        _mode: u64,
        _abi: crate::user::UserAbi,
        _process_state: &mut crate::user::UserProcessState,
    ) -> Result<(), VfsError> {
        Err(VfsError::Unsupported)
    }

    fn boot_image_path(path: &str) -> Result<&str, VfsError> {
        let path = path.strip_prefix('/').unwrap_or(path);
        if path.is_empty() {
            return Err(VfsError::InvalidArgument);
        }
        if path.starts_with("services/")
            || path.starts_with("apps/")
            || path.starts_with("applications/")
            || path.starts_with("etc/")
            || path.starts_with("lib/")
            || path.starts_with("lib64/")
            || path.starts_with("usr/lib/")
            || path.starts_with("usr/lib64/")
            || path.starts_with("system/")
        {
            Ok(path)
        } else {
            Err(VfsError::Unsupported)
        }
    }

    fn map_boot_volume_error(error: FatError<StorageError>) -> VfsError {
        match error {
            FatError::Io(StorageError::NotPresent) | FatError::NotFound => VfsError::NotFound,
            FatError::Io(StorageError::Unsupported) => VfsError::Unsupported,
            FatError::InvalidInput
            | FatError::InvalidFileNameLength
            | FatError::UnsupportedFileNameCharacter => VfsError::InvalidArgument,
            FatError::CorruptedFileSystem => VfsError::InvalidArgument,
            FatError::Io(_) | FatError::UnexpectedEof => VfsError::Unsupported,
            _ => VfsError::Unsupported,
        }
    }

    pub fn read_path_to_vec_for_kernel(path: &str) -> Result<alloc::vec::Vec<u8>, VfsError> {
        let path = boot_image_path(path)?;
        crate::storage::boot_volume::read_file_to_vec(path).map_err(map_boot_volume_error)
    }

    pub fn boot_path_file_len_for_kernel(path: &str) -> Result<u64, VfsError> {
        let path = boot_image_path(path)?;
        crate::storage::boot_volume::metadata(path)
            .map(|metadata| metadata.len)
            .map_err(map_boot_volume_error)
    }

    pub fn boot_path_extent_lease_for_kernel(
        path: &str,
    ) -> Result<Option<BootPathExtentLease>, VfsError> {
        let path = boot_image_path(path)?;
        crate::storage::boot_volume::boot_file_extent_lease(path)
            .map(|lease| {
                lease.map(|lease| BootPathExtentLease {
                    file_len: lease.len,
                    generation: lease.generation,
                    extents: lease
                        .extents
                        .into_iter()
                        .map(|extent| BootPathExtent {
                            disk_offset: extent.offset,
                            len: extent.len,
                        })
                        .collect(),
                })
            })
            .map_err(map_boot_volume_error)
    }

    pub fn readlink_for_current_process(
        _absolute_path: &str,
    ) -> Result<alloc::string::String, VfsError> {
        Err(VfsError::Unsupported)
    }
}
