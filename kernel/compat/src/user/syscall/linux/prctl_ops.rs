//! Linux `prctl(2)` (x86_64 syscall 157).
//!
//! Supported options, each backed by real state:
//!
//! | option | state | scope |
//! | --- | --- | --- |
//! | `PR_SET_NAME` / `PR_GET_NAME` | `LinuxThreadState::comm` | per thread; cloned with the thread state, reset by exec |
//! | `PR_SET_NO_NEW_PRIVS` / `PR_GET_NO_NEW_PRIVS` | `ProcessSecurityContext::no_new_privs` | per process, one-way, fork-inherited, exec-preserved |
//! | `PR_SET_DUMPABLE` / `PR_GET_DUMPABLE` | `ProcessSecurityContext::dumpable` | per process, fork-inherited, exec-reset |
//!
//! Everything else returns `EINVAL`, never fake success: parent-death
//! signals (no delivery hook on parent exit), seccomp (no filter engine; run
//! Chrome with `--no-sandbox`), `PR_SET_VMA` (no VMA naming), timer slack (the
//! scheduler has no slack semantics), `PR_CAPBSET_READ` (no capability
//! bounding set; `logical_admin` is not a Linux capability set) and every
//! unknown option.

use super::*;

pub(crate) const PR_SET_PDEATHSIG: u64 = 1;
pub(crate) const PR_GET_PDEATHSIG: u64 = 2;
pub(crate) const PR_GET_DUMPABLE: u64 = 3;
pub(crate) const PR_SET_DUMPABLE: u64 = 4;
pub(crate) const PR_GET_SECCOMP: u64 = 21;
pub(crate) const PR_SET_SECCOMP: u64 = 22;
pub(crate) const PR_CAPBSET_READ: u64 = 23;
pub(crate) const PR_SET_NAME: u64 = 15;
pub(crate) const PR_GET_NAME: u64 = 16;
pub(crate) const PR_SET_TIMERSLACK: u64 = 29;
pub(crate) const PR_GET_TIMERSLACK: u64 = 30;
pub(crate) const PR_SET_NO_NEW_PRIVS: u64 = 38;
pub(crate) const PR_GET_NO_NEW_PRIVS: u64 = 39;
pub(crate) const PR_SET_VMA: u64 = 0x5356_4d41;

const COMM_LEN: usize = rustos_user_abi::linux::LINUX_TASK_COMM_LEN;
/// Longest name payload; the 16th byte is always NUL.
const COMM_MAX_CHARS: usize = COMM_LEN - 1;
const USER_PAGE: u64 = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PrctlAction {
    SetName(u64),
    GetName(u64),
    SetNoNewPrivs,
    GetNoNewPrivs,
    SetDumpable(bool),
    GetDumpable,
    Invalid,
}

/// Pure option table: argument validation exactly as Linux's `prctl` does it
/// for the supported options.
pub(crate) fn decide(option: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> PrctlAction {
    match option {
        PR_SET_NAME => PrctlAction::SetName(a2),
        PR_GET_NAME => PrctlAction::GetName(a2),
        PR_SET_NO_NEW_PRIVS if a2 == 1 && a3 == 0 && a4 == 0 && a5 == 0 => {
            PrctlAction::SetNoNewPrivs
        }
        PR_GET_NO_NEW_PRIVS if a2 == 0 && a3 == 0 && a4 == 0 && a5 == 0 => {
            PrctlAction::GetNoNewPrivs
        }
        // Only SUID_DUMP_DISABLE (0) and SUID_DUMP_USER (1) are valid.
        PR_SET_DUMPABLE if a2 <= 1 => PrctlAction::SetDumpable(a2 == 1),
        PR_GET_DUMPABLE => PrctlAction::GetDumpable,
        _ => PrctlAction::Invalid,
    }
}

/// Builds the stored comm from a user string prefix: stops at the first NUL,
/// keeps at most 15 bytes, zero-pads (`strncpy_from_user` semantics).
pub(crate) fn name_from_bytes(bytes: &[u8]) -> [u8; COMM_LEN] {
    let mut comm = [0_u8; COMM_LEN];
    let end = bytes
        .iter()
        .take(COMM_MAX_CHARS)
        .position(|&b| b == 0)
        .unwrap_or(bytes.len().min(COMM_MAX_CHARS));
    comm[..end].copy_from_slice(&bytes[..end]);
    comm
}

/// Name reported by `PR_GET_NAME`: the stored comm, or, when never set, the
/// basename of the executable path truncated to 15 bytes (Linux sets comm
/// that way at exec).
pub(crate) fn effective_name(comm: &[u8; COMM_LEN], exec_path: &str) -> [u8; COMM_LEN] {
    if comm[0] != 0 {
        let mut out = *comm;
        out[COMM_MAX_CHARS] = 0;
        return out;
    }
    let base = exec_path.rsplit('/').next().unwrap_or("");
    name_from_bytes(base.as_bytes())
}

/// Bytes that can be read at `ptr` without crossing a page, so a short
/// string right below an unmapped page is not faulted on by over-reading.
fn chunk_len(ptr: u64, filled: usize) -> usize {
    let at = ptr.wrapping_add(filled as u64);
    let page_remaining = (USER_PAGE - (at & (USER_PAGE - 1))) as usize;
    (COMM_MAX_CHARS - filled).min(page_remaining)
}

fn read_user_name(ptr: u64) -> Result<[u8; COMM_LEN], i64> {
    let mut buf = [0_u8; COMM_MAX_CHARS];
    let mut filled = 0;
    while filled < COMM_MAX_CHARS {
        let len = chunk_len(ptr, filled);
        let at = ptr
            .checked_add(filled as u64)
            .ok_or(LINUX_EFAULT)?;
        usermem::copy_from_current_user_exact(at, &mut buf[filled..filled + len])
            .map_err(user_pointer_error_to_linux_errno)?;
        if buf[filled..filled + len].contains(&0) {
            break;
        }
        filled += len;
    }
    Ok(name_from_bytes(&buf))
}

pub(super) fn syscall_linux_prctl(option: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> u64 {
    match decide(option, a2, a3, a4, a5) {
        PrctlAction::SetName(ptr) => {
            let comm = match read_user_name(ptr) {
                Ok(comm) => comm,
                Err(errno) => return linux_errno(errno),
            };
            let stored = multitask::with_current_user_linux_state_mut(
                |_, _, abi, _, _, thread_state| {
                    if abi != crate::user::abi::UserAbi::Linux {
                        return false;
                    }
                    match thread_state.as_mut() {
                        Some(state) => {
                            state.comm = comm;
                            true
                        }
                        None => false,
                    }
                },
            );
            if stored == Some(true) {
                0
            } else {
                linux_errno(LINUX_EINVAL)
            }
        }
        PrctlAction::GetName(ptr) => {
            let comm = multitask::with_current_user_linux_state_mut(
                |_, _, _, _, _, thread_state| thread_state.as_ref().map(|state| state.comm),
            )
            .flatten();
            let Some(comm) = comm else {
                return linux_errno(LINUX_EINVAL);
            };
            let exec_path = multitask::with_current_user_process_state(|_, _, state| {
                String::from(state.exec_path())
            })
            .unwrap_or_default();
            let name = effective_name(&comm, &exec_path);
            match usermem::write_current_user_bytes(ptr, &name) {
                Ok(()) => 0,
                Err(err) => linux_errno(user_pointer_error_to_linux_errno(err)),
            }
        }
        PrctlAction::SetNoNewPrivs => {
            match multitask::with_current_user_process_state_mut(|_, _, state| {
                state.set_no_new_privs()
            }) {
                Some(()) => 0,
                None => linux_errno(LINUX_EINVAL),
            }
        }
        PrctlAction::GetNoNewPrivs => multitask::with_current_process_credentials(|security| {
            u64::from(security.no_new_privs())
        })
        .unwrap_or_else(|| linux_errno(LINUX_EINVAL)),
        PrctlAction::SetDumpable(dumpable) => {
            match multitask::with_current_user_process_state_mut(|_, _, state| {
                state.set_dumpable(dumpable)
            }) {
                Some(()) => 0,
                None => linux_errno(LINUX_EINVAL),
            }
        }
        PrctlAction::GetDumpable => multitask::with_current_process_credentials(|security| {
            u64::from(security.dumpable())
        })
        .unwrap_or_else(|| linux_errno(LINUX_EINVAL)),
        PrctlAction::Invalid => linux_errno(LINUX_EINVAL),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_options_decode() {
        assert_eq!(decide(PR_SET_NAME, 0x10, 0, 0, 0), PrctlAction::SetName(0x10));
        assert_eq!(decide(PR_GET_NAME, 0x20, 0, 0, 0), PrctlAction::GetName(0x20));
        assert_eq!(decide(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), PrctlAction::SetNoNewPrivs);
        assert_eq!(decide(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0), PrctlAction::GetNoNewPrivs);
        assert_eq!(decide(PR_SET_DUMPABLE, 0, 0, 0, 0), PrctlAction::SetDumpable(false));
        assert_eq!(decide(PR_SET_DUMPABLE, 1, 0, 0, 0), PrctlAction::SetDumpable(true));
        assert_eq!(decide(PR_GET_DUMPABLE, 0, 0, 0, 0), PrctlAction::GetDumpable);
    }

    #[test]
    fn bad_arguments_and_unsupported_options_are_einval() {
        // NNP can only be set to 1, with zero trailing args; get takes zeros.
        assert_eq!(decide(PR_SET_NO_NEW_PRIVS, 0, 0, 0, 0), PrctlAction::Invalid);
        assert_eq!(decide(PR_SET_NO_NEW_PRIVS, 1, 1, 0, 0), PrctlAction::Invalid);
        assert_eq!(decide(PR_GET_NO_NEW_PRIVS, 1, 0, 0, 0), PrctlAction::Invalid);
        // SUID_DUMP_ROOT (2) and beyond are rejected.
        assert_eq!(decide(PR_SET_DUMPABLE, 2, 0, 0, 0), PrctlAction::Invalid);
        for option in [
            PR_SET_PDEATHSIG,
            PR_GET_PDEATHSIG,
            PR_GET_SECCOMP,
            PR_SET_SECCOMP,
            PR_CAPBSET_READ,
            PR_SET_TIMERSLACK,
            PR_GET_TIMERSLACK,
            PR_SET_VMA,
            0,
            9999,
            u64::MAX,
        ] {
            assert_eq!(decide(option, 1, 2, 3, 4), PrctlAction::Invalid, "{option}");
        }
    }

    #[test]
    fn option_numbers_match_linux_uapi() {
        assert_eq!(PR_SET_NAME, 15);
        assert_eq!(PR_GET_NAME, 16);
        assert_eq!(PR_SET_NO_NEW_PRIVS, 38);
        assert_eq!(PR_GET_NO_NEW_PRIVS, 39);
        assert_eq!(PR_SET_DUMPABLE, 4);
        assert_eq!(PR_GET_DUMPABLE, 3);
        assert_eq!(PR_SET_SECCOMP, 22);
        assert_eq!(PR_SET_VMA, 0x53564d41);
        assert_eq!(rustos_user_abi::linux::SYS_PRCTL, 157);
    }

    #[test]
    fn names_truncate_at_fifteen_bytes_and_stop_at_nul() {
        let long = name_from_bytes(b"0123456789abcdefghij");
        assert_eq!(&long[..15], b"0123456789abcde");
        assert_eq!(long[15], 0);
        let short = name_from_bytes(b"chrome\0garbage");
        assert_eq!(&short[..6], b"chrome");
        assert!(short[6..].iter().all(|&b| b == 0));
        assert_eq!(name_from_bytes(b"")[0], 0);
        let exact = name_from_bytes(b"123456789012345");
        assert_eq!(&exact[..15], b"123456789012345");
        assert_eq!(exact[15], 0);
    }

    #[test]
    fn unset_name_falls_back_to_exec_basename() {
        let unset = [0_u8; COMM_LEN];
        let name = effective_name(&unset, "/apps/chrome/chrome");
        assert_eq!(&name[..6], b"chrome");
        let long = effective_name(&unset, "/bin/a-very-long-program-name");
        assert_eq!(&long[..15], b"a-very-long-pro");
        assert_eq!(long[15], 0);
        assert_eq!(effective_name(&unset, "")[0], 0);
        let mut set = [0_u8; COMM_LEN];
        set[..3].copy_from_slice(b"abc");
        assert_eq!(&effective_name(&set, "/x/other")[..3], b"abc");
    }

    #[test]
    fn name_reads_never_cross_a_page_boundary() {
        // 3 bytes left in the page: read only those first.
        assert_eq!(chunk_len(0x1000 - 3, 0), 3);
        assert_eq!(chunk_len(0x1000 - 3, 3), 12);
        assert_eq!(chunk_len(0x1000, 0), 15);
        assert_eq!(chunk_len(0x1000 - 20, 0), 15);
        assert_eq!(chunk_len(0x1000 - 20, 14), 1);
    }
}
