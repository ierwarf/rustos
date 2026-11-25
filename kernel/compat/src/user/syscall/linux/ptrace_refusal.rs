//! Linux `ptrace(2)` (x86_64 syscall 101): an honest refusal.
//!
//! RustOS has no tracer/tracee relation: no stop states, no `wait4` ptrace
//! stops, no register or memory access on behalf of another task. Nothing is
//! therefore ever traced, and every request is answered as Linux answers it
//! for a caller that holds neither `CAP_SYS_PTRACE` nor a Yama exemption:
//!
//! | request | result |
//! | --- | --- |
//! | `PTRACE_TRACEME` | `EPERM` (cannot arrange a tracer) |
//! | `PTRACE_ATTACH`, `PTRACE_SEIZE` | `ESRCH` if the target does not exist, else `EPERM` |
//! | every other request (including unknown ones) | `ESRCH`: the target is never a tracee of the caller |
//!
//! A recognised request never reports `ENOSYS`, so programs such as Chromium's
//! crashpad handler take their documented "cannot attach" paths. A target tid
//! in another process is reported as `ESRCH` (no cross-process tid index is
//! consulted here), where Linux would say `EPERM` for `ATTACH`/`SEIZE`.

use super::*;

pub(super) const PTRACE_TRACEME: u64 = 0;
pub(super) const PTRACE_ATTACH: u64 = 16;
pub(super) const PTRACE_SEIZE: u64 = 0x4206;

/// Errno for `ptrace(request, pid, ..)`; `target_exists` is only consulted for
/// the attach family.
pub(super) fn ptrace_errno(request: u64, target_exists: impl FnOnce() -> bool) -> i64 {
    // `request` is a C `long` but the kernel compares the full register.
    match request {
        PTRACE_TRACEME => LINUX_EPERM,
        PTRACE_ATTACH | PTRACE_SEIZE => {
            if target_exists() {
                LINUX_EPERM
            } else {
                LINUX_ESRCH
            }
        }
        _ => LINUX_ESRCH,
    }
}

fn pid_exists(pid: u64) -> bool {
    if pid == 0 || pid > i32::MAX as u64 {
        return false;
    }
    multitask::with_process_state_by_pid(pid, |_| ()).is_some()
        || multitask::current_user_process_id()
            .is_some_and(|own| multitask::live_user_thread_ids(own).contains(&pid))
}

pub(super) fn syscall_linux_ptrace(request: u64, pid: u64, _addr: u64, _data: u64) -> u64 {
    linux_errno(ptrace_errno(request, || pid_exists(pid)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traceme_and_attach_family_are_eperm_or_esrch_never_enosys() {
        assert_eq!(ptrace_errno(PTRACE_TRACEME, || unreachable!()), LINUX_EPERM);
        assert_eq!(ptrace_errno(PTRACE_ATTACH, || true), LINUX_EPERM);
        assert_eq!(ptrace_errno(PTRACE_SEIZE, || true), LINUX_EPERM);
        assert_eq!(ptrace_errno(PTRACE_ATTACH, || false), LINUX_ESRCH);
        assert_eq!(ptrace_errno(PTRACE_SEIZE, || false), LINUX_ESRCH);
    }

    #[test]
    fn requests_on_an_untraced_target_are_esrch() {
        // PEEKTEXT, CONT, DETACH, GETREGS, SETOPTIONS, GETREGSET, and an unknown value.
        for request in [1, 7, 17, 12, 0x4200, 0x4204, 0xdead] {
            assert_eq!(ptrace_errno(request, || unreachable!()), LINUX_ESRCH);
        }
    }
}
