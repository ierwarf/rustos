#![no_std]

extern crate alloc;

use alloc::format;

#[macro_export]
macro_rules! executive_debug_println {
    () => {{
        nucleus_core::debug::println_newline();
    }};
    ($($arg:tt)*) => {{
        nucleus_core::debug::println_fmt(format_args!($($arg)*));
    }};
}

#[allow(unused_imports, unused_macros)]
pub mod debug {
    pub use crate::executive_debug_println as println;
    pub use nucleus_core::debug::*;
}

pub(crate) fn flow_info(event_id: u16, _message: &str) {
    let _ = event_id;
    debug::info!(service, "{}", _message);
}

pub(crate) fn flow_debug(event_id: u16, _message: &str) {
    let _ = event_id;
    debug::debug!(service, "{}", _message);
}

pub(crate) fn announce_ready(name: &str, console_line: &[u8]) {
    flow_info(20, format!("{name} initialized").as_str());
    crate::io_services::console_write(console_line);
}

mod hal_hooks {
    use kernel_compat::api as compat_api;
    use kernel_hal::api as hal_api;
    use kernel_io_manager::api as io_api;
    use kernel_ps::api as ps_api;

    fn retire_current_user_task_due_to_fault(
        vector: u8,
        error_code: Option<u64>,
        cr2: u64,
        rip: u64,
        rsp: u64,
    ) -> hal_api::UserFaultDisposition {
        // fault로 종료되는 프로세스의 IPC 엔드포인트를 즉시 해제한다.
        // 정상 종료는 syscall_process_exit에서 처리하므로 여기서는 fault 경우만 커버한다.
        if let Some(process_id) = ps_api::current_user_process_id() {
            compat_api::syscall::cleanup_service_endpoints_for_process(process_id);
        }
        match ps_api::retire_current_user_task_due_to_fault(vector, error_code, cr2, rip, rsp) {
            ps_api::UserFaultDisposition::Resumed => hal_api::UserFaultDisposition::Resumed,
            ps_api::UserFaultDisposition::Retired => hal_api::UserFaultDisposition::Retired,
            ps_api::UserFaultDisposition::Unhandled => hal_api::UserFaultDisposition::Unhandled,
        }
    }

    fn current_user_snapshot() -> Option<hal_api::CurrentUserSnapshot> {
        ps_api::current_user_snapshot().map(|snapshot| hal_api::CurrentUserSnapshot {
            abi: snapshot.abi(),
            thread_id: snapshot.thread_id(),
            process_id: snapshot.process_id(),
            console_session_raw: snapshot.console_session().raw(),
        })
    }

    fn current_debug_user_context() -> Option<nucleus_core::debug::CurrentUserLogContext> {
        ps_api::current_user_log_ids().map(|(process_id, thread_id)| {
            nucleus_core::debug::CurrentUserLogContext {
                process_id,
                thread_id,
            }
        })
    }

    fn dispatch_pic_irq(irq: u8) -> bool {
        compat_api::syscall::with_kernel_gs_base(|| crate::io_services::dispatch_pic_irq(irq))
    }

    fn handle_keyboard_interrupt() {
        compat_api::syscall::with_kernel_gs_base(|| {
            crate::io_services::on_keyboard_interrupt();
            let _ = crate::io_services::dispatch_pic_irq(1);
        });
    }

    fn handle_mouse_interrupt() {
        compat_api::syscall::with_kernel_gs_base(|| {
            crate::io_services::on_mouse_interrupt();
            let _ = crate::io_services::dispatch_pic_irq(12);
        });
    }

    fn heartbeat_snapshot() -> hal_api::HeartbeatSnapshot {
        let input = crate::io_services::input_debug_snapshot();
        let (linux_irq_owner_count, linux_irq_total_depth) =
            crate::io_services::debug_irq_lock_snapshot();
        let (linux_input_lock_active, linux_input_lock_last_seq) =
            crate::io_services::debug_input_lock_snapshot();
        hal_api::HeartbeatSnapshot {
            userspace_display_active: crate::io_services::userspace_display_active(),
            xhci_transfer_count: crate::io_services::debug_transfer_event_count(),
            hid_pointer_report_count: crate::io_services::debug_pointer_report_count(),
            input: hal_api::InputEventQueueDebugSnapshot {
                pointer_packet_submits: input.pointer_packet_submits,
                pointer_absolute_submits: input.pointer_absolute_submits,
                read_calls: input.read_calls,
                read_events: input.read_events,
                lock_active: input.lock_active,
                lock_last_seq: input.lock_last_seq,
                queued: input.queued,
                pending_coalesced: input.pending_coalesced,
                pending_pointer_position: input.pending_pointer_position,
                dropped_discrete: input.dropped_discrete,
                dropped_lossy: input.dropped_lossy,
            },
            linux_irq_owner_count,
            linux_irq_total_depth: linux_irq_total_depth as u64,
            linux_input_lock_active: linux_input_lock_active != 0,
            linux_input_lock_last_seq,
        }
    }

    pub fn register() {
        nucleus_core::debug::register_runtime_hooks(nucleus_core::debug::DebugRuntimeHooks {
            ticks: Some(hal_api::arch::rtc::ticks),
            ticks_per_second: Some(hal_api::arch::rtc::ticks_per_second),
            current_user_context: Some(current_debug_user_context),
        });
        ps_api::register_tick_jiffies_hook(crate::io_services::tick_jiffies);
        ps_api::register_input_consumer_hooks(
            io_api::driver::linux::input::consumer_acquire,
            io_api::driver::linux::input::consumer_release,
        );
        hal_api::register_task_hooks(hal_api::TaskHooks {
            retire_current_user_task_due_to_fault: Some(retire_current_user_task_due_to_fault),
            halt_current_retired_task: Some(ps_api::halt_current_retired_task),
            current_user_snapshot: Some(current_user_snapshot),
            is_scheduler_initialized: Some(ps_api::is_initialized),
            current_task_id: Some(ps_api::current_task_id),
            current_user_thread_id: Some(ps_api::current_user_id),
            block_current_user_task: Some(ps_api::block_current_user_task),
            wake_user_task: Some(ps_api::wake_user_task),
            yield_now: Some(ps_api::yield_now),
        });
        hal_api::register_interrupt_hooks(hal_api::InterruptHooks {
            dispatch_pic_irq: Some(dispatch_pic_irq),
            handle_keyboard_interrupt: Some(handle_keyboard_interrupt),
            handle_mouse_interrupt: Some(handle_mouse_interrupt),
        });
        hal_api::register_heartbeat_hooks(hal_api::HeartbeatHooks {
            snapshot: Some(heartbeat_snapshot),
        });
    }
}

mod fatal;
mod io_services;
mod tasks;

pub mod boot;
