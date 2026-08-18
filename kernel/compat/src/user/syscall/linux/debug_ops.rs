use super::*;
use core::sync::atomic::{AtomicUsize, Ordering};

const SECONDARY_LINUX_SYSCALL_DEBUG_LIMIT: usize = 64;
static SECONDARY_LINUX_SYSCALL_DEBUG_LOGS: AtomicUsize = AtomicUsize::new(0);
const SMP_QUALIFICATION_FINISH_MILESTONE_NAME: &str = "smp-qualification-finish";
const SMP_QUALIFICATION_COMPLETE_MILESTONE_NAME: &str = "smp-qualification-complete";
const USER_DEBUG_CHUNK_BYTES: usize = 256;

pub(super) fn syscall_linux_rustos_debug_print(user_ptr: u64, user_len: u64) -> u64 {
    let requested_len = match usize::try_from(user_len) {
        Ok(len) => len,
        Err(_) => return linux_errno(LINUX_EINVAL),
    };
    if requested_len == 0 {
        return 0;
    }

    let len = requested_len.min(MAX_RUSTOS_DEBUG_PRINT_BYTES);
    let mut written = 0usize;
    let mut chunk = [0_u8; USER_DEBUG_CHUNK_BYTES];
    while written < len {
        let chunk_len = (len - written).min(chunk.len());
        let ptr = match user_ptr.checked_add(written as u64) {
            Some(ptr) => ptr,
            None => return linux_errno(LINUX_EINVAL),
        };
        if let Err(err) = usermem::copy_from_current_user_exact(ptr, &mut chunk[..chunk_len]) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        // A Ring3 caller must never be able to inject a fresh debugcon line
        // that is indistinguishable from a kernel-stamped milestone frame.
        // Every chunk gets a kernel-owned envelope and every control/quote
        // byte is escaped before the serialized output lock is acquired.
        debug::write_user_bytes_serialized(&chunk[..chunk_len]);
        written += chunk_len;
    }
    written as u64
}

fn product_milestone_name(milestone: u64) -> Option<(&'static str, bool)> {
    match milestone {
        linux_abi::PRODUCT_MILESTONE_ROOT_CORE_READY => Some(("product-root-core-ready", false)),
        linux_abi::PRODUCT_MILESTONE_DISPLAY_READY => Some(("product-display-ready", true)),
        linux_abi::PRODUCT_MILESTONE_STORAGE_READY => Some(("product-storage-ready", true)),
        linux_abi::PRODUCT_MILESTONE_EXECUTABLE_SNAPSHOT_SEALED => {
            Some(("product-executable-snapshot-sealed", true))
        }
        linux_abi::PRODUCT_MILESTONE_FIRST_FRAME => Some(("product-first-frame", false)),
        PRODUCT_MILESTONE_INIT_IDENTITY_READY => Some(("product-init-identity-ready", false)),
        linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_READY => {
            Some(("smp-qualification-ready", false))
        }
        linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_START => {
            Some(("smp-qualification-start", false))
        }
        linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_FINISH => {
            Some((SMP_QUALIFICATION_FINISH_MILESTONE_NAME, false))
        }
        linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_COMPLETE => {
            Some((SMP_QUALIFICATION_COMPLETE_MILESTONE_NAME, false))
        }
        _ => None,
    }
}

pub(super) fn syscall_linux_rustos_product_milestone(milestone: u64, arg0: u64, arg1: u64) -> u64 {
    let Some((name, requires_nonzero_arg0)) = product_milestone_name(milestone) else {
        return linux_errno(LINUX_EINVAL);
    };
    if requires_nonzero_arg0 && arg0 == 0 {
        return linux_errno(LINUX_EINVAL);
    }
    let current_cpu = nucleus_core::util::lockdep::current_cpu_index();
    let qualification_phase = matches!(
        milestone,
        linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_READY
            | linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_START
            | linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_FINISH
            | linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_COMPLETE
    );
    let recorded_arg1 = if qualification_phase {
        match admit_smp_qualification_milestone(milestone, arg0, arg1, current_cpu) {
            Ok(encoded_work) => encoded_work,
            Err(errno) => return linux_errno(errno),
        }
    } else {
        arg1
    };
    // The binding lock was released by admission before emitting debug output.
    // Ring3 supplies the naked work word; only ring0 emits the binding-stamped
    // evidence word consumed by the host qualification validator.
    debug::record_milestone(debug::LogCategory::Compat, name, arg0, recorded_arg1);
    0
}

/// Flushes the IPC-call and user-copy phase profiles immediately instead of
/// waiting for their ordinary once-per-second housekeeping drain. Diagnostics
/// only: nothing reads these counters to make a decision, and this grants no
/// authority beyond emitting the same milestones housekeeping would emit
/// anyway. `ipcbench --isolate-probe` is the sole caller, bracketing one
/// probe's own measurement window so a probe that finishes inside one window
/// cannot leave its tail-end charges stranded, undrained, and invisible to a
/// log capture that stops as soon as the probe's own end marker appears.
pub(super) fn syscall_linux_rustos_phase_profile_drain() -> u64 {
    (super::ipc_profile::force_drain_ipc_call_profile()
        + super::ipc_server_profile::force_drain_ipc_server_profile()
        + kernel_ps::api::force_drain_user_copy_profile()) as u64
}

pub(super) fn linux_errno(errno: i64) -> u64 {
    (-errno) as u64
}

#[allow(
    clippy::items_after_test_module,
    reason = "the closed-vocabulary contract test is kept adjacent to the product milestone decoder"
)]
#[cfg(test)]
mod product_milestone_tests {
    use super::*;

    #[test]
    fn product_milestones_are_a_closed_fixed_name_vocabulary() {
        assert_eq!(
            product_milestone_name(linux_abi::PRODUCT_MILESTONE_ROOT_CORE_READY),
            Some(("product-root-core-ready", false))
        );
        assert_eq!(
            product_milestone_name(linux_abi::PRODUCT_MILESTONE_EXECUTABLE_SNAPSHOT_SEALED),
            Some(("product-executable-snapshot-sealed", true))
        );
        assert_eq!(
            product_milestone_name(PRODUCT_MILESTONE_INIT_IDENTITY_READY),
            Some(("product-init-identity-ready", false))
        );
        assert_eq!(
            product_milestone_name(linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_FINISH),
            Some(("smp-qualification-finish", false))
        );
        assert_eq!(
            product_milestone_name(linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_COMPLETE),
            Some(("smp-qualification-complete", false))
        );
        assert_eq!(product_milestone_name(0), None);
        assert_eq!(product_milestone_name(u64::MAX), None);
    }

    #[test]
    fn smp_qualification_milestone_binds_worker_to_the_kernel_observed_cpu() {
        let packed = linux_abi::pack_smp_qualification_worker(5, 5);
        assert!(linux_abi::smp_qualification_worker_shape_valid(
            packed, 1_000, 5
        ));
        assert!(!linux_abi::smp_qualification_worker_shape_valid(
            packed, 1_000, 4
        ));
    }

    #[test]
    fn smp_qualification_milestone_rejects_unbounded_workers_and_work() {
        let valid = linux_abi::pack_smp_qualification_worker(0, 0);
        let excess_worker =
            linux_abi::pack_smp_qualification_worker(0, linux_abi::SMP_QUALIFICATION_MAX_WORKERS);
        for packed in [valid, excess_worker] {
            for work_units in [0, 1, linux_abi::SMP_QUALIFICATION_MAX_WORK_UNITS + 1] {
                let expected = packed == valid
                    && (1..=linux_abi::SMP_QUALIFICATION_MAX_WORK_UNITS).contains(&work_units);
                assert_eq!(
                    linux_abi::smp_qualification_worker_shape_valid(packed, work_units, 0),
                    expected,
                );
            }
        }
    }

    #[test]
    fn user_debug_syscall_has_no_raw_debugcon_payload_path() {
        let source = include_str!("debug_ops.rs");
        let payload = "&chunk[..chunk_len]);";
        let serialized = ["debug::write_user_bytes_serialized(", payload].concat();
        let raw = ["debug::write_bytes(", payload].concat();
        assert!(source.contains(serialized.as_str()));
        assert!(!source.contains(raw.as_str()));
    }
}

// DIAGNOSTIC: Secondary syscall tracing is present only in diagnostic kernels.
#[cfg_attr(not(rustos_debug_print_enabled), allow(dead_code, unused_variables))]
pub(super) fn debug_log_secondary_linux_syscall(frame: &SyscallFrame) {
    if !debug::enabled!(compat, debug) {
        return;
    }
    let snapshot = multitask::current_user_snapshot();
    let pid = snapshot.map(|user| user.thread_id()).unwrap_or(0);
    if pid < 7 {
        return;
    }
    if SECONDARY_LINUX_SYSCALL_DEBUG_LOGS.fetch_add(1, Ordering::Relaxed)
        >= SECONDARY_LINUX_SYSCALL_DEBUG_LIMIT
    {
        return;
    }
    let Some(session) = snapshot.map(|user| user.console_session()) else {
        // Debug output must not invent a privileged console identity when a
        // syscall somehow arrives without its owning user task.
        return;
    };
    if frame.rax == linux_abi::SYS_OPENAT {
        debug::println!(
            "secondary linux syscall: pid={} session={} nr={} path={} flags={:#x}",
            pid,
            session.raw(),
            frame.rax,
            debug_user_path(frame.rsi),
            frame.rdx,
        );
    } else if frame.rax == linux_abi::SYS_ACCESS {
        debug::println!(
            "secondary linux syscall: pid={} session={} nr={} path={} mode={:#x}",
            pid,
            session.raw(),
            frame.rax,
            debug_user_path(frame.rdi),
            frame.rsi,
        );
    } else {
        debug::println!(
            "secondary linux syscall: pid={} session={} nr={} rip={:#x} rdi={:#x} rsi={:#x} rdx={:#x} r10={:#x}",
            pid,
            session.raw(),
            frame.rax,
            frame.user_rip,
            frame.rdi,
            frame.rsi,
            frame.rdx,
            frame.r10,
        );
    }
}

// DIAGNOSTIC: User-path rendering is compiled only with syscall diagnostics.
#[cfg_attr(not(rustos_debug_print_enabled), allow(dead_code))]
pub(super) fn debug_user_path(path_ptr: u64) -> String {
    match usermem::read_current_user_c_string(path_ptr, 256) {
        Ok(path) => path,
        Err(_) => String::from("<invalid>"),
    }
}
