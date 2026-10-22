//! Persistent-state regression rail. Check both null syscall and the real fused
//! IPC transcript before/after lifecycle transitions, not just the transition's
//! own result. Linux has no RustOS IPC ABI; its lane measures only null syscalls.

use crate::{core_paths, debug_line, ipc_probes, skip};
use std::{
    ffi::CString,
    ptr,
    sync::atomic::{AtomicUsize, Ordering},
};

static SIGNALS: AtomicUsize = AtomicUsize::new(0);
extern "C" fn on_signal(_: libc::c_int) {
    SIGNALS.fetch_add(1, Ordering::Relaxed);
}

fn checkpoint(phase: &str, khz: u64) {
    core_paths::identity(&format!("state_{phase}_null"), khz);
    if !crate::linux_output() {
        ipc_probes::probe_state_checkpoint(&format!("state_{phase}_ipc"), khz);
    }
}

fn reap(pid: libc::pid_t) -> bool {
    if pid <= 0 {
        return false;
    }
    let mut status = 0;
    let waited = unsafe { crate::wait_child(pid, &mut status) };
    let valid = waited.is_ok() && crate::child_exited_successfully(status);
    if !valid {
        debug_line(&format!(
            "ipcbench: state-child-failure pid={pid} wait={waited:?} status={status}"
        ));
    }
    valid
}

fn exec_transition() -> bool {
    let path = crate::benchmark_executable();
    let arg = CString::new("--lifecycle-child").unwrap();
    let argv = [path.as_ptr(), arg.as_ptr(), ptr::null()];
    let envp = [ptr::null()];
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        unsafe {
            libc::execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr());
            libc::_exit(crate::last_errno().clamp(1, 125));
        }
    }
    if pid < 0 {
        debug_line(&format!(
            "ipcbench: state-exec-failure stage=fork errno={}",
            crate::last_errno()
        ));
        return false;
    }
    reap(pid)
}

fn signal_transition() -> bool {
    SIGNALS.store(0, Ordering::Relaxed);
    unsafe {
        let old = libc::signal(libc::SIGUSR1, on_signal as *const () as libc::sighandler_t);
        if old == libc::SIG_ERR {
            return false;
        }
        let raised = libc::raise(libc::SIGUSR1) == 0;
        let restored = libc::signal(libc::SIGUSR1, old) != libc::SIG_ERR;
        raised && restored && SIGNALS.load(Ordering::Relaxed) == 1
    }
}

fn mapping_transition() -> bool {
    unsafe {
        let address = libc::mmap(
            ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        );
        if address == libc::MAP_FAILED {
            return false;
        }
        ptr::write_volatile(address.cast::<u8>(), 0x5a);
        let valid = ptr::read_volatile(address.cast::<u8>()) == 0x5a;
        libc::munmap(address, 4096) == 0 && valid
    }
}

pub fn run(khz: u64) {
    debug_line(&format!(
        "ipcbench: state-transitions begin v=1 count={}",
        crate::core_contract::STATE_TRANSITIONS.len()
    ));
    let Some(mut pristine) = crate::PRISTINE_NULL.lock().unwrap().take() else {
        skip("state_pristine_null", "missing-preflight-baseline");
        return;
    };
    crate::report("state_pristine_null", &crate::summarize(&mut pristine), khz);
    checkpoint("before", khz);
    // Quiesce each transaction before comparing steady-state costs. Keeping a
    // runnable child alive would change contention, not test a permanent tax.
    // The fork checkpoint includes normal child exit+wait; the separate exit
    // checkpoint also verifies publication/reap of a nonzero terminal status.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        unsafe {
            libc::_exit(0);
        }
    }
    if !reap(pid) {
        skip("state_fork_transition", "fork-wait-or-child-status-failed");
        return;
    }
    checkpoint("fork", khz);
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        unsafe {
            libc::_exit(42);
        }
    }
    let mut status = 0;
    if pid < 0
        || unsafe { crate::wait_child(pid, &mut status) }.is_err()
        || !libc::WIFEXITED(status)
        || libc::WEXITSTATUS(status) != 42
    {
        skip("state_exit_transition", "wait-or-terminal-status-failed");
        return;
    }
    checkpoint("exit", khz);
    for (phase, valid) in [
        ("signal", signal_transition as fn() -> bool),
        ("mmap", mapping_transition),
        ("thread", || {
            std::thread::Builder::new()
                .spawn(|| 0x5au8)
                .ok()
                .and_then(|thread| thread.join().ok())
                == Some(0x5a)
        }),
        // Exec is last so an unsupported replacement does not prevent the
        // independent signal/mapping/thread checkpoints from being collected.
        // Its failure still rejects the entire suite; it is never skipped as a pass.
        ("exec", exec_transition),
    ] {
        if !valid() {
            skip(
                &format!("state_{phase}_transition"),
                "transition-semantic-check-failed",
            );
            return;
        }
        checkpoint(phase, khz);
    }
    debug_line("ipcbench: state-transitions complete v=1");
}

// Kept separate from the IPC baseline: end-to-end process latency, not COW-only.
pub fn lifecycle(khz: u64) {
    crate::probe_fork_exit_wait(khz);
    crate::probe_fork_exec_exit_wait(khz);
    crate::probe_thread_clone_exit_join(khz);
    crate::probe_exec_replace_single_thread(khz);
    crate::probe_spawn_activation_to_first_turn(khz);
    crate::probe_exit_retire_to_reap(khz);
    crate::probe_fork_cow_private_write(khz, 64, 4);
}
