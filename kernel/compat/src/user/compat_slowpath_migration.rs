//! Planning-only inventory for residual Linux/Win32 compat slow paths.
//!
//! This file is not linked into the compat module.  It records the remaining
//! high-level policy batches after the direct Linux/Windows app ABI remains
//! stable and ring0 keeps only syscall decode, user-copy, fd-table, wait, and
//! broker substrate.

// RING3-MIGRATION-REFERENCE START: compat syscall slow-path policy candidates.
// Keep syscall entry/decode, current-task user memory access, fd-table mutation
// substrates, and bounded sleep/wake mechanics in ring0.  Move observable
// Linux/Win32 policy, namespace decisions, admission, and long-lived state into
// the owning services without changing app-visible ABI.
#[allow(dead_code)]
struct CompatSlowPathMigrationReference {
    area: &'static str,
    ring0_broker_scope: &'static str,
    ring3_owner: &'static str,
    source_surfaces: &'static [&'static str],
    first_step: &'static str,
    fallback_removal_gate: &'static str,
}

#[allow(dead_code)]
const COMPAT_SLOW_PATH_RING3_BATCHES: &[CompatSlowPathMigrationReference] = &[
    CompatSlowPathMigrationReference {
        area: "Linux syscall dispatch residual policy",
        ring0_broker_scope: "syscall number decode, argument capture, user-copy, errno return",
        ring3_owner: "syscalld/procd/vfsd/netd/devmgrd/inputd",
        source_surfaces: &[
            "kernel/compat/src/user/syscall/linux.rs",
            "kernel/compat/src/user/syscall/linux/syscalld_ops.rs",
            "kernel/compat/src/user/syscall/linux/offload_ops.rs",
        ],
        first_step: "audit each direct syscall arm so non-substrate admission resolves in the owning service",
        fallback_removal_gate: "unknown or service-owned Linux syscalls cannot fall back to ring0 policy tables",
    },
    CompatSlowPathMigrationReference {
        area: "poll/ppoll/epoll readiness policy",
        ring0_broker_scope: "fd validation, epoll token handles, user-copy, bounded sleep waiter",
        ring3_owner: "vfsd/netd/inputd/syscalld",
        source_surfaces: &[
            "kernel/compat/src/user/syscall/linux/service_ops/poll_epoll.rs",
            "kernel/ps/src/user/epoll.rs",
        ],
        first_step: "move readiness classification and timeout admission to services while ring0 waits on explicit readiness tokens",
        fallback_removal_gate: "mixed fd poll/epoll tests prove no path computes service policy from ring0 handle class alone",
    },
    CompatSlowPathMigrationReference {
        area: "Linux memory syscall admission",
        ring0_broker_scope: "address-space mutation, page-table edits, current-task user-copy, fd handle lookup",
        ring3_owner: "syscalld/pagerd/vfsd/devmgrd",
        source_surfaces: &[
            "kernel/compat/src/user/syscall/linux/memory_ops.rs",
            "kernel/compat/src/user/syscall/linux/mm_broker_ops.rs",
            "kernel/ps/src/user/memfd.rs",
        ],
        first_step: "make syscalld/pagerd own mmap/brk/mprotect admission and fd-backed mapping policy",
        fallback_removal_gate: "mmap/mprotect/munmap/memfd tests show denied policy cannot be rescued by direct ring0 fallbacks",
    },
    CompatSlowPathMigrationReference {
        area: "process wait/exit/session slow path",
        ring0_broker_scope: "task teardown, scheduler removal, signal-frame substrate, wait wakeups",
        ring3_owner: "procd/syscalld/rootd",
        source_surfaces: &[
            "kernel/compat/src/user/syscall/linux/syscalld_ops.rs",
            "kernel/compat/src/user/syscall/linux/service_ops/futex_thread.rs",
            "kernel/ps/src/user/process_state.rs",
        ],
        first_step: "move wait status, rusage, process group, session, and parent/child policy state to procd",
        fallback_removal_gate: "fork/exec/wait/signal tests pass with ring0 storing only scheduler-required task state",
    },
    CompatSlowPathMigrationReference {
        area: "device and namespace ioctl routing policy",
        ring0_broker_scope: "device fd lookup, user-copy, direct hot ioctl execution for explicit fast paths",
        ring3_owner: "devmgrd/sessiond/uiserver/inputd",
        source_surfaces: &[
            "kernel/compat/src/user/sysops/device.rs",
            "kernel/compat/src/user/syscall/linux/device_broker_ops.rs",
            "kernel/io-manager/src/io/device/display.rs",
        ],
        first_step: "route every non-hot ioctl classification through devmgrd/sessiond leases",
        fallback_removal_gate: "ioctl route denial cannot use path strings or default native-device rights in ring0",
    },
    CompatSlowPathMigrationReference {
        area: "Windows syscall and PE runtime policy",
        ring0_broker_scope: "Win32 syscall decode, user-copy, narrow process/memory broker calls",
        ring3_owner: "syscalld/loaderd/procd",
        source_surfaces: &[
            "kernel/compat/src/user/syscall/windows/dispatch.rs",
            "kernel/compat/src/user/syscall/windows/api.rs",
            "kernel/compat/src/user/syscall/linux/proc_broker_ops.rs",
        ],
        first_step: "keep decode substrate in ring0 and move PE/Win32 namespace, import, and memory policy to services",
        fallback_removal_gate: "PE smoke tests exercise loaderd/syscalld policy without direct ring0 Win32 allowlists",
    },
];
// RING3-MIGRATION-REFERENCE END: compat syscall slow-path policy candidates.
