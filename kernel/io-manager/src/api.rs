//! Public ring0 device and bootstrap-I/O substrate API.
//!
//! - **Owner:** `kernel-io-manager`; services reach privileged mechanism only
//!   through capability-gated Compat brokers.
//! - **Boundary:** Exports preserve exact aperture, consumer, generation,
//!   range, and bootstrap allowlist admission.
//! - **Lifecycle:** APIs expose install/publish, bounded operation,
//!   revoke/withdraw, and reclaim without hidden provider selection.
//! - **Concurrency:** IRQ leaves are lock-free or bounded wake-only callbacks;
//!   policy work remains in schedulable service context.
//! - **Failure:** Missing provider, capacity, stale generation, and malformed
//!   transport return exact terminal errors.
//! - **Forbidden:** No physical disk descriptor, AHCI/NVMe policy, native
//!   input/network fallback, raw shared-memory export, or cross-crate private
//!   reach-through.
//! - **Evidence:** `input-delivery-lifecycle`, DVM ingress flows, and
//!   `bootstrap-content-admission`.
pub use crate::storage::boot_volume::BootstrapImageError;
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
    InvalidArgument,
    NotFound,
    Unsupported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DvmBlockTransportInfo {
    pub generation: u64,
    pub capacity_sectors: u64,
    pub logical_block_size: u32,
    pub physical_block_size: u32,
    pub features: u64,
    pub read_only: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DvmBlockTicket {
    pub generation: u64,
    pub request_id: u64,
    pub data_slot: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DvmBlockPoll {
    Pending,
    Completed(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DvmBlockError {
    Unavailable,
    Busy,
    Invalid,
    Protocol,
    Revoked,
    DeviceFault,
    Unsupported,
    Cancelled,
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
    use super::{DvmBlockError, DvmBlockPoll, DvmBlockTicket, DvmBlockTransportInfo};

    pub fn read_bootstrap_file_range(
        path: &str,
        offset: u64,
        out: &mut [u8],
    ) -> Result<Option<usize>, crate::storage::boot_volume::BootstrapImageError> {
        crate::storage::boot_volume::read_file_range(path, offset, out)
    }

    pub fn bootstrap_file_len(
        path: &str,
    ) -> Result<u64, crate::storage::boot_volume::BootstrapImageError> {
        crate::storage::boot_volume::file_len(path)
    }

    pub fn dvm_info() -> Result<DvmBlockTransportInfo, DvmBlockError> {
        crate::io::dvm_block::info()
            .map(|info| DvmBlockTransportInfo {
                generation: info.generation,
                capacity_sectors: info.capacity_sectors,
                logical_block_size: info.logical_block_size,
                physical_block_size: info.physical_block_size,
                features: info.features,
                read_only: info.read_only,
            })
            .map_err(map_dvm_error)
    }

    /// Submit only to the exact admitted epoch, checked under the transport lock.
    pub fn submit_dvm_read(expected_generation: u64, sector: u64, data_len: u32) -> Result<DvmBlockTicket, DvmBlockError> {
        crate::io::dvm_block::submit_read(expected_generation, sector, data_len)
            .map(map_dvm_ticket)
            .map_err(map_dvm_error)
    }

    pub fn submit_dvm_write(
        expected_generation: u64,
        sector: u64,
        data: &[u8],
        fua: bool,
    ) -> Result<DvmBlockTicket, DvmBlockError> {
        crate::io::dvm_block::submit_write(expected_generation, sector, data, fua)
            .map(map_dvm_ticket)
            .map_err(map_dvm_error)
    }

    pub fn submit_dvm_flush(expected_generation: u64) -> Result<DvmBlockTicket, DvmBlockError> {
        crate::io::dvm_block::submit_flush(expected_generation)
            .map(map_dvm_ticket)
            .map_err(map_dvm_error)
    }

    pub fn poll_dvm_ready(
        ticket: DvmBlockTicket,
        expected_len: usize,
    ) -> Result<DvmBlockPoll, DvmBlockError> {
        crate::io::dvm_block::poll_ready(unmap_dvm_ticket(ticket), expected_len)
            .map(|poll| match poll {
                crate::io::dvm_block::DvmBlockPoll::Pending => DvmBlockPoll::Pending,
                crate::io::dvm_block::DvmBlockPoll::Completed(bytes) => {
                    DvmBlockPoll::Completed(bytes)
                }
            })
            .map_err(map_dvm_error)
    }

    pub fn poll_dvm(ticket: DvmBlockTicket, out: &mut [u8]) -> Result<DvmBlockPoll, DvmBlockError> {
        crate::io::dvm_block::poll(unmap_dvm_ticket(ticket), out)
            .map(|poll| match poll {
                crate::io::dvm_block::DvmBlockPoll::Pending => DvmBlockPoll::Pending,
                crate::io::dvm_block::DvmBlockPoll::Completed(bytes) => {
                    DvmBlockPoll::Completed(bytes)
                }
            })
            .map_err(map_dvm_error)
    }

    pub fn cancel_dvm(ticket: DvmBlockTicket) -> Result<(), DvmBlockError> {
        crate::io::dvm_block::cancel(unmap_dvm_ticket(ticket)).map_err(map_dvm_error)
    }

    pub fn finish_dvm(ticket: DvmBlockTicket) -> Result<(), DvmBlockError> {
        crate::io::dvm_block::finish(unmap_dvm_ticket(ticket)).map_err(map_dvm_error)
    }

    pub fn dvm_completion_or_fault_pending() -> bool {
        crate::io::dvm_block::completion_or_fault_pending()
    }

    pub fn arm_dvm_waiter(task_id: u64) -> bool {
        crate::io::dvm_block::arm_waiter(task_id)
    }

    pub fn disarm_dvm_waiter(task_id: u64) -> bool {
        crate::io::dvm_block::disarm_waiter(task_id)
    }

    fn map_dvm_ticket(ticket: crate::io::dvm_block::DvmBlockTicket) -> DvmBlockTicket {
        DvmBlockTicket {
            generation: ticket.generation,
            request_id: ticket.request_id,
            data_slot: ticket.data_slot,
        }
    }

    fn unmap_dvm_ticket(ticket: DvmBlockTicket) -> crate::io::dvm_block::DvmBlockTicket {
        crate::io::dvm_block::DvmBlockTicket {
            generation: ticket.generation,
            request_id: ticket.request_id,
            data_slot: ticket.data_slot,
        }
    }

    fn map_dvm_error(error: crate::io::dvm_block::DvmBlockError) -> DvmBlockError {
        match error {
            crate::io::dvm_block::DvmBlockError::Unavailable => DvmBlockError::Unavailable,
            crate::io::dvm_block::DvmBlockError::Busy => DvmBlockError::Busy,
            crate::io::dvm_block::DvmBlockError::Invalid => DvmBlockError::Invalid,
            crate::io::dvm_block::DvmBlockError::Protocol => DvmBlockError::Protocol,
            crate::io::dvm_block::DvmBlockError::Revoked => DvmBlockError::Revoked,
            crate::io::dvm_block::DvmBlockError::DeviceFault => DvmBlockError::DeviceFault,
            crate::io::dvm_block::DvmBlockError::Unsupported => DvmBlockError::Unsupported,
            crate::io::dvm_block::DvmBlockError::Cancelled => DvmBlockError::Cancelled,
        }
    }
}

pub mod boot {
    use super::{BootstrapPhase, map_bootstrap_phase};
    use boot_protocol::{BootInfo, BootVolumeIdentity, BootVolumeTransport};

    pub fn init_gui(boot_info_ptr: *const BootInfo) {
        crate::io::gui::init(boot_info_ptr);
    }

    /// The boot framebuffer is allocation-free, while a DVM shared aperture
    /// needs tracked MMIO mapping state. Call only after the kernel heap is
    /// available.
    pub fn init_dvm_display_provider() -> bool {
        crate::io::dvm_display::try_install()
    }

    pub fn init_dvm_network_provider() -> bool {
        crate::io::dvm_network::try_install()
    }

    pub fn init_dvm_block_provider() -> bool {
        crate::io::dvm_block::try_install()
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

}

pub mod console {
    pub fn init_tty() {
        crate::io::tty::init();
    }

    pub fn write(bytes: &[u8]) {
        crate::io::console::write(bytes);
    }
}

pub mod network {
    pub use crate::network::{PACKET_MTU, PacketError, PacketTransportStatus};
    pub use crate::io::dvm_network::DvmTransportPublicationGuard;

    /// Acquire in sleepable context before the registry mutation spinlock.
    /// The authorized registrar resets predecessor authority only on commit.
    pub fn begin_dvm_transport_publication() -> DvmTransportPublicationGuard<'static> {
        crate::io::dvm_network::begin_transport_publication()
    }

    /// The exact owner/epoch is revalidated while the transport lease is held.
    /// The callback must be bounded, nonblocking registry validation only.
    pub fn transport_status(authorize: impl FnOnce() -> bool) -> Result<PacketTransportStatus, PacketError> {
        crate::network::transport_status(authorize)
    }

    pub fn transmit_frame(frame: &[u8], authorize: impl FnOnce() -> bool) -> Result<usize, PacketError> {
        crate::network::transmit_frame(frame, authorize)
    }

    pub fn receive_frame(out: &mut [u8], authorize: impl FnOnce() -> bool) -> Result<usize, PacketError> {
        crate::network::receive_frame(out, authorize)
    }

    pub fn wait_receive(timeout_ms: u64, authorize: impl Fn() -> bool) -> Result<(), PacketError> {
        crate::io::dvm_network::wait_receive(timeout_ms, authorize)
    }

    pub fn grant_dvm_transport_lease(generation: u32, authorize: impl FnOnce() -> bool) -> bool {
        crate::io::dvm_network::grant_transport_lease(generation, authorize)
    }

    pub fn revoke_dvm_transport_lease(generation: u32, authorize: impl FnOnce() -> bool) -> bool {
        crate::io::dvm_network::revoke_transport_lease(generation, authorize)
    }

    pub fn reset_dvm_transport_lease(authorize: impl FnOnce() -> bool) -> bool {
        crate::io::dvm_network::reset_transport_lease(authorize)
    }
}

pub mod input {
    pub mod transport {
        pub use crate::input::dvm_ring::InputTransportDebugSnapshot;

        pub fn debug_snapshot() -> InputTransportDebugSnapshot {
            crate::input::dvm_ring::debug_snapshot()
        }

        pub fn has_pending_records() -> bool {
            crate::input::dvm_ring::has_pending_records()
        }

        pub fn arm_consumer_wake() -> bool {
            crate::input::dvm_ring::arm_consumer_wake()
        }

        pub fn disarm_input_waiter(task_id: u64) -> bool {
            crate::input::wait_queue::disarm_input_waiter(task_id)
        }

        pub fn arm_inputd_ingestion_waiter(task_id: u64) -> bool {
            crate::input::wait_queue::arm_inputd_ingestion_waiter(task_id)
        }

        pub fn disarm_inputd_ingestion_waiter(task_id: u64) -> bool {
            crate::input::wait_queue::disarm_inputd_ingestion_waiter(task_id)
        }

        pub fn withdraw_policy_consumer() {
            crate::input::dvm_ring::withdraw_policy_consumer();
        }
    }

    pub fn init() {
        crate::input::init();
    }

    /// Bounded hardware transport drain for the capability-gated Linux-DVM
    /// input ingress broker. Input policy and event translation stay in
    /// `inputd`.
    pub fn service_dvm_input_pending(
        dest: &mut [rustos_user_abi::syscall::InputDvmRecordWire],
    ) -> usize {
        crate::input::service_dvm_input_pending(dest)
    }

    pub fn mark_dvm_policy_consumer_ready() -> bool {
        crate::input::mark_dvm_policy_consumer_ready()
    }
}

pub mod device {
    pub use crate::io::device::{DeviceAccessKind, DeviceError, DeviceHandle, DeviceId};

    pub fn ioctl_from_user(
        handle: kernel_object::api::device::DeviceHandle,
        process_id: u64,
        process_state: &mut crate::user::process_state::UserProcessState,
        request: u64,
        arg: u64,
    ) -> Result<u64, DeviceError> {
        crate::io::device::ioctl_from_user(handle.into(), process_id, process_state, request, arg)
    }

    /// Exact RustOS display-device ABI entry used when VFS represents
    /// `/dev/display0` as a remote device handle. Policy routing remains in
    /// devmgrd/uiserver; this is only the common user-copy/handle substrate.
    pub fn ioctl_display_from_user(
        process_id: u64,
        process_state: &mut crate::user::process_state::UserProcessState,
        request: u64,
        arg: u64,
    ) -> Result<u64, DeviceError> {
        crate::io::device::display::ioctl(process_id, process_state, request, arg)
    }

    pub fn display_gpu_atlas_create_slot_from_user(
        process_state: &crate::user::process_state::UserProcessState,
        request: u64,
        arg: u64,
    ) -> Option<u32> {
        crate::io::device::display::gpu_atlas_create_slot_from_user(process_state, request, arg)
    }

    pub fn prepare_display_ioctl(request: u64, gpu_atlas_create_slot: Option<u32>) {
        crate::io::device::display::prepare_ioctl(request, gpu_atlas_create_slot);
    }
}

pub mod tty {
    pub use crate::io::session::ConsoleSessionHandle;

    pub fn disarm_input_waiter(task_id: u64) -> bool {
        crate::io::tty::disarm_input_waiter(task_id)
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
}

pub mod io {
    pub mod gui {
        pub use crate::io::gui::GuiDisplayInfo;

        pub fn display_info() -> Option<GuiDisplayInfo> {
            crate::io::gui::display_info()
        }

        pub fn is_userspace_display_active() -> bool {
            crate::io::gui::is_userspace_display_active()
        }
    }
}

pub mod vfs {
    use super::VfsError;
    use alloc::string::ToString;

    pub fn init() {}

    pub fn normalize_kernel_path(path: &str) -> Result<alloc::string::String, VfsError> {
        if path.starts_with('/') {
            Ok(path.to_string())
        } else {
            Err(VfsError::InvalidArgument)
        }
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

    fn map_boot_volume_error(error: crate::storage::boot_volume::BootstrapImageError) -> VfsError {
        match error {
            crate::storage::boot_volume::BootstrapImageError::NotFound => VfsError::NotFound,
            crate::storage::boot_volume::BootstrapImageError::Unavailable => VfsError::Unsupported,
            crate::storage::boot_volume::BootstrapImageError::Invalid => VfsError::InvalidArgument,
        }
    }

    pub fn read_path_to_vec_for_kernel(path: &str) -> Result<alloc::vec::Vec<u8>, VfsError> {
        let path = boot_image_path(path)?;
        crate::storage::boot_volume::read_file_to_vec(path).map_err(map_boot_volume_error)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn bootstrap_transport_failure_is_not_reported_as_missing_file() {
            assert_eq!(
                map_boot_volume_error(
                    crate::storage::boot_volume::BootstrapImageError::Unavailable
                ),
                VfsError::Unsupported
            );
            assert_eq!(
                map_boot_volume_error(crate::storage::boot_volume::BootstrapImageError::NotFound),
                VfsError::NotFound
            );
        }
    }
}

/// Fixed GUI-DVM app-buffer transport. Callers must hold the authenticated
/// service publication lease and retain the allocation capability.
pub mod app_buffer {
    pub use crate::io::dvm_display::AppBufferTransportError;
    /// Verified physical address of `[offset, offset+len)` in the app aperture.
    pub fn aperture_phys(offset: u64, len: u64) -> Option<u64> {
        crate::io::dvm_display::app_buffer_aperture_phys(offset, len)
    }
    pub fn read_range(offset: u64, dst: &mut [u8]) -> bool {
        crate::io::dvm_display::app_buffer_read_range(offset, dst)
    }
    pub fn scrub_range(offset: u64, len: u64) -> bool {
        crate::io::dvm_display::app_buffer_scrub_range(offset, len)
    }
    pub fn begin(capability: &kernel_ps::api::handles::BufferCapability,
                 record: driver_domain_protocol::DvmAppBuffer, revoke: bool)
        -> Result<u64, AppBufferTransportError>
    {
        crate::io::dvm_display::begin_app_buffer(capability, record, revoke)
    }
    pub fn take(sequence: u64)
        -> Result<Option<kernel_ps::api::handles::BufferCapability>, AppBufferTransportError>
    {
        crate::io::dvm_display::take_app_buffer(sequence)
    }
}
