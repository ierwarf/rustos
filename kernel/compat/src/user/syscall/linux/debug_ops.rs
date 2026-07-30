use super::*;
use core::sync::atomic::{AtomicUsize, Ordering};

const SECONDARY_LINUX_SYSCALL_DEBUG_LIMIT: usize = 64;
static SECONDARY_LINUX_SYSCALL_DEBUG_LOGS: AtomicUsize = AtomicUsize::new(0);

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
    let mut chunk = [0_u8; 256];
    while written < len {
        let chunk_len = (len - written).min(chunk.len());
        let ptr = match user_ptr.checked_add(written as u64) {
            Some(ptr) => ptr,
            None => return linux_errno(LINUX_EINVAL),
        };
        if let Err(err) = usermem::copy_from_current_user_exact(ptr, &mut chunk[..chunk_len]) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        debug::write_bytes(&chunk[..chunk_len]);
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
    debug::record_milestone(debug::LogCategory::Compat, name, arg0, arg1);
    0
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
        assert_eq!(product_milestone_name(0), None);
        assert_eq!(product_milestone_name(u64::MAX), None);
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
