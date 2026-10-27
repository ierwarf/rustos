//! Shared app/host vocabulary. Names describe actual operations, not errno paths.

pub const SYSCALL: &str = "core_syscalls";
pub const MEMORY: &str = "core_memory";
pub const DATA: &str = "core_data";
pub const SOCKET_BUFFERED: &str = "core_socket_buffered";
pub const SOCKET_BUFFERED_PROBES: &[&str] = &[
    "socket_buffered_read_4k",
    "socket_buffered_read_32k",
];
pub const STATE: &str = "core_state";
pub const SCHEDULER: &str = "core_scheduler";
// GUI samples are owned by uiserver, not emulated in ipcbench/Linux stdout.
pub const GUI: &str = "core_gui";
pub const SCHEDULER_PROBES: &[&str] = &["sched_yield", "wake_to_run_futex", "smp_balance_futex"];
pub const LIFECYCLE: &str = "core_lifecycle";
pub const LIFECYCLE_PROBES: &[&str] = &[
    "fork_exit_wait",
    "fork_exec_exit_wait",
    "thread_clone_exit_join",
    "exec_replace_single_thread",
    "spawn_activation_to_first_turn",
    "exit_retire_to_reap",
    "fork_cow_private_write",
];
pub const ITERS: usize = 2_000;
pub const WARMUP: usize = 2_000;
pub const DATA_ITERS: usize = 128;
pub const DATA_WARMUP: usize = 8;

pub const SYSCALL_PROBES: &[&str] = &[
    "syscall_getuid",
    "syscall_read_memfd",
    "syscall_write_memfd",
    "syscall_openat",
    "syscall_stat",
    "syscall_pipe2",
    "syscall_epoll_wait_empty",
    "syscall_futex_wake_empty",
];
pub const MEMORY_PROBES: &[&str] = &[
    "mmap_reserve_only",
    "munmap_resident_page",
    "mprotect_readonly",
    "page_fault_first_write",
    "cow_first_write",
];
pub const DATA_PROBES: &[&str] = &[
    "file_read_4k",
    "file_read_64k",
    "file_read_1m",
    "file_write_4k",
    "file_write_64k",
    "file_write_1m",
    "file_open_close",
    "small_files_open_close_128",
    "pipe_latency_1b",
    "socket_latency_1b",
    "pipe_transfer_4k",
    "pipe_transfer_64k",
    "pipe_transfer_1m",
    "socket_transfer_4k",
    "socket_transfer_64k",
    "socket_transfer_1m",
];
pub const STATE_TRANSITIONS: &[&str] = &["fork", "exec", "exit", "signal", "mmap", "thread"];

pub fn probes(suite: &str) -> Option<&'static [&'static str]> {
    match suite {
        SYSCALL => Some(SYSCALL_PROBES),
        MEMORY => Some(MEMORY_PROBES),
        DATA => Some(DATA_PROBES),
        SOCKET_BUFFERED => Some(SOCKET_BUFFERED_PROBES),
        LIFECYCLE => Some(LIFECYCLE_PROBES),
        SCHEDULER => Some(SCHEDULER_PROBES),
        _ => None,
    }
}

pub fn is_suite(name: &str) -> bool {
    probes(name).is_some() || name == STATE
}
