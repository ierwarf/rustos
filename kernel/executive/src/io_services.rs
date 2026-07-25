extern crate alloc;

use boot_protocol::{BootInfo, BootVolumeIdentity, BootVolumeTransport};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BootstrapPhase {
    EarlyBootstrap,
    CoreHostsLaunching,
    KernelVfsReady,
    UserspaceReady,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct InputEventQueueDebugSnapshot {
    pub pointer_packet_submits: u64,
    pub read_calls: u64,
    pub read_events: u64,
    pub lock_active: u64,
    pub lock_last_seq: u64,
    pub queued: usize,
    pub pending_coalesced: bool,
    pub pending_pointer_position: bool,
    pub dropped_discrete: u64,
    pub dropped_lossy: u64,
}

mod backend {
    use super::*;

    fn map_bootstrap_phase(phase: kernel_io_manager::api::BootstrapPhase) -> BootstrapPhase {
        match phase {
            kernel_io_manager::api::BootstrapPhase::EarlyBootstrap => {
                BootstrapPhase::EarlyBootstrap
            }
            kernel_io_manager::api::BootstrapPhase::CoreHostsLaunching => {
                BootstrapPhase::CoreHostsLaunching
            }
            kernel_io_manager::api::BootstrapPhase::KernelVfsReady => {
                BootstrapPhase::KernelVfsReady
            }
            kernel_io_manager::api::BootstrapPhase::UserspaceReady => {
                BootstrapPhase::UserspaceReady
            }
        }
    }

    fn map_input_snapshot(
        snapshot: kernel_io_manager::api::input::transport::InputTransportDebugSnapshot,
    ) -> InputEventQueueDebugSnapshot {
        InputEventQueueDebugSnapshot {
            // Heartbeat ABI names predate the ring3 decoder migration. These
            // counters now describe raw transport progress only.
            pointer_packet_submits: snapshot.records_copied,
            read_calls: snapshot.broker_calls,
            read_events: snapshot.records_copied,
            lock_active: 0,
            lock_last_seq: 0,
            queued: snapshot.queued,
            pending_coalesced: false,
            pending_pointer_position: false,
            dropped_discrete: 0,
            dropped_lossy: snapshot.revoke_count,
        }
    }

    pub(crate) fn console_write(bytes: &[u8]) {
        kernel_io_manager::api::console::write(bytes);
    }

    pub(crate) fn tty_init() {
        kernel_io_manager::api::console::init_tty();
    }

    pub(crate) fn gui_init(boot_info_ptr: *const BootInfo) {
        kernel_io_manager::api::boot::init_gui(boot_info_ptr);
    }

    pub(crate) fn init_dvm_display_provider() -> bool {
        kernel_io_manager::api::boot::init_dvm_display_provider()
    }

    pub(crate) fn init_dvm_network_provider() -> bool {
        kernel_io_manager::api::boot::init_dvm_network_provider()
    }

    pub(crate) fn init_dvm_block_provider() -> bool {
        kernel_io_manager::api::boot::init_dvm_block_provider()
    }

    pub(crate) fn gui_try_present_panic_blackout() -> bool {
        kernel_io_manager::api::io::gui::try_present_panic_blackout()
    }

    pub(crate) fn userspace_display_active() -> bool {
        kernel_io_manager::api::io::gui::is_userspace_display_active()
    }

    pub(crate) fn init_boot_info(boot_info_ptr: *const BootInfo) {
        kernel_io_manager::api::boot::init_boot_info(boot_info_ptr);
    }

    pub(crate) fn boot_volume_transport_hint() -> Option<BootVolumeTransport> {
        kernel_io_manager::api::boot::boot_volume_transport_hint()
    }

    pub(crate) fn boot_volume_identity() -> Option<BootVolumeIdentity> {
        kernel_io_manager::api::boot::boot_volume_identity()
    }

    pub(crate) fn bootstrap_phase() -> BootstrapPhase {
        map_bootstrap_phase(kernel_io_manager::api::boot::bootstrap_phase())
    }

    pub(crate) fn userspace_ready() -> bool {
        kernel_io_manager::api::boot::userspace_ready()
    }

    pub(crate) fn enter_kernel_vfs_runtime() {
        kernel_io_manager::api::boot::enter_kernel_vfs_runtime();
    }

    pub(crate) fn enter_userspace_runtime() {
        kernel_io_manager::api::boot::enter_userspace_runtime();
    }

    pub(crate) fn init_input() {
        kernel_io_manager::api::input::init();
    }

    pub(crate) fn input_debug_snapshot() -> InputEventQueueDebugSnapshot {
        map_input_snapshot(kernel_io_manager::api::input::transport::debug_snapshot())
    }

    pub(crate) fn init_vfs() {
        kernel_io_manager::api::vfs::init();
    }

    pub(crate) fn system_console_session_raw() -> kernel_object::api::session::ConsoleSessionHandle
    {
        kernel_io_manager::api::session::ConsoleSessionHandle::SYSTEM.into()
    }
}

pub(crate) use backend::{
    boot_volume_identity, boot_volume_transport_hint, bootstrap_phase, console_write,
    enter_kernel_vfs_runtime, enter_userspace_runtime, gui_init, gui_try_present_panic_blackout,
    init_boot_info, init_dvm_block_provider, init_dvm_display_provider, init_dvm_network_provider,
    init_input, init_vfs, input_debug_snapshot, system_console_session_raw, tty_init,
    userspace_display_active, userspace_ready,
};
