extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use boot_protocol::{BootInfo, BootVolumeIdentity, BootVolumeTransport};
use driver_abi::DriverClass;

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
    pub pointer_absolute_submits: u64,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BlockDescriptor {
    pub id: u32,
    pub path: String,
    pub transport: storage_core::TransportKind,
    pub readonly: bool,
    pub logical_block_size: usize,
    pub start_block: u64,
    pub block_count: u64,
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
        snapshot: kernel_io_manager::api::input::event_queue::InputEventQueueDebugSnapshot,
    ) -> InputEventQueueDebugSnapshot {
        InputEventQueueDebugSnapshot {
            pointer_packet_submits: snapshot.pointer_packet_submits,
            pointer_absolute_submits: snapshot.pointer_absolute_submits,
            read_calls: snapshot.read_calls,
            read_events: snapshot.read_events,
            lock_active: snapshot.lock_active,
            lock_last_seq: snapshot.lock_last_seq,
            queued: snapshot.queued,
            pending_coalesced: snapshot.pending_coalesced,
            pending_pointer_position: snapshot.pending_pointer_position,
            dropped_discrete: snapshot.dropped_discrete,
            dropped_lossy: snapshot.dropped_lossy,
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

    pub(crate) fn gui_try_present_panic_blackout() -> bool {
        kernel_io_manager::api::io::gui::try_present_panic_blackout()
    }

    pub(crate) fn display_service_pending() -> usize {
        kernel_io_manager::api::io::gui::service_pending()
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

    pub(crate) fn register_boot_volume_opener() {
        kernel_io_manager::api::block::register_boot_volume_opener();
    }

    pub(crate) fn init_block_devices() {
        kernel_io_manager::api::block::init_block_devices();
    }

    pub(crate) fn block_descriptors() -> Vec<BlockDescriptor> {
        kernel_io_manager::api::block::descriptors()
            .into_iter()
            .map(|descriptor| BlockDescriptor {
                id: descriptor.id,
                path: descriptor.path,
                transport: descriptor.transport,
                readonly: descriptor.readonly,
                logical_block_size: descriptor.logical_block_size,
                start_block: descriptor.start_block,
                block_count: descriptor.block_count,
            })
            .collect()
    }

    pub(crate) fn init_linux_cpu_local_symbols() {
        kernel_io_manager::api::driver::init_linux_cpu_local_symbols();
    }

    pub(crate) fn initialize_loadable_modules_for_class(class: DriverClass) -> bool {
        kernel_io_manager::api::driver::initialize_loadable_modules_for_class(class)
    }

    pub(crate) fn dispatch_pic_irq(irq: u8) -> bool {
        kernel_io_manager::api::driver::irq::dispatch_pic_irq(irq)
    }

    pub(crate) fn debug_irq_lock_snapshot() -> (usize, usize) {
        kernel_io_manager::api::driver::linux::runtime::debug_irq_lock_snapshot()
    }

    pub(crate) fn tick_jiffies(delta: u64) -> u64 {
        kernel_io_manager::api::driver::linux::runtime::tick_jiffies(delta)
    }

    pub(crate) fn debug_input_lock_snapshot() -> (usize, u64) {
        kernel_io_manager::api::driver::linux::input::debug_lock_snapshot()
    }

    pub(crate) fn init_input() {
        kernel_io_manager::api::input::init();
    }

    pub(crate) fn on_keyboard_interrupt() {
        kernel_io_manager::api::input::on_keyboard_interrupt();
    }

    pub(crate) fn on_mouse_interrupt() {
        kernel_io_manager::api::input::on_mouse_interrupt();
    }

    pub(crate) fn input_service_pending() -> usize {
        kernel_io_manager::api::input::service_pending()
    }

    pub(crate) fn input_debug_snapshot() -> InputEventQueueDebugSnapshot {
        map_input_snapshot(kernel_io_manager::api::input::event_queue::debug_snapshot())
    }

    pub(crate) fn init_usb() {
        kernel_io_manager::api::usb::init();
    }

    pub(crate) fn usb_service_pending() -> usize {
        kernel_io_manager::api::usb::service_pending()
    }

    pub(crate) fn debug_transfer_event_count() -> u64 {
        kernel_io_manager::api::usb::debug_transfer_event_count()
    }

    pub(crate) fn debug_pointer_report_count() -> u64 {
        kernel_io_manager::api::usb::debug_pointer_report_count()
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
    block_descriptors, boot_volume_identity, boot_volume_transport_hint, bootstrap_phase,
    console_write, debug_input_lock_snapshot, debug_irq_lock_snapshot, debug_pointer_report_count,
    debug_transfer_event_count, dispatch_pic_irq, display_service_pending,
    enter_kernel_vfs_runtime, enter_userspace_runtime, gui_init, gui_try_present_panic_blackout,
    init_block_devices, init_boot_info, init_input, init_linux_cpu_local_symbols, init_usb,
    init_vfs, initialize_loadable_modules_for_class, input_debug_snapshot, input_service_pending,
    on_keyboard_interrupt, on_mouse_interrupt, register_boot_volume_opener,
    system_console_session_raw, tick_jiffies, tty_init, usb_service_pending,
    userspace_display_active, userspace_ready,
};
