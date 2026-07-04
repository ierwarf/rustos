//! Planning-only inventory for procd-owned process policy migration.
//!
//! This file is not linked into the process-state module.  It tracks the
//! remaining policy batches that should become procd/rootd/loaderd state while
//! ring0 keeps task IDs, scheduling, wait wakeups, and address-space substrate.

// RING3-MIGRATION-REFERENCE START: procd process slow-path policy candidates.
// Keep run queues, task lifecycle safety, signal-frame substrate, and wakeup
// mechanics in ring0.  Move process-visible hierarchy, exec authorization,
// wait/reap policy, process groups, sessions, credentials, and rusage state to
// procd/syscalld/rootd through explicit brokers.
#[allow(dead_code)]
struct ProcessSlowPathMigrationReference {
    area: &'static str,
    ring0_substrate_scope: &'static str,
    ring3_owner: &'static str,
    source_surfaces: &'static [&'static str],
    first_step: &'static str,
    fallback_removal_gate: &'static str,
}

#[allow(dead_code)]
const PROCESS_SLOW_PATH_RING3_BATCHES: &[ProcessSlowPathMigrationReference] = &[
    ProcessSlowPathMigrationReference {
        area: "process hierarchy and wait policy",
        ring0_substrate_scope: "task exit state, wait wakeup, zombie lifetime safety",
        ring3_owner: "procd",
        source_surfaces: &[
            "kernel/ps/src/user/process_state.rs",
            "kernel/ps/src/multitask/process_table.rs",
            "kernel/compat/src/user/syscall/linux/syscalld_ops.rs",
        ],
        first_step: "make procd the source of parent/child, wait status, and rusage decisions",
        fallback_removal_gate: "wait4/waitpid cannot synthesize policy-only statuses from ring0 process tables",
    },
    ProcessSlowPathMigrationReference {
        area: "exec/fork lifecycle admission",
        ring0_substrate_scope: "task creation, address-space attach, scheduler enrollment, fork register image",
        ring3_owner: "procd/loaderd/rootd",
        source_surfaces: &[
            "kernel/compat/src/user/syscall/linux/proc_broker_ops.rs",
            "kernel/compat/src/user/process/mod.rs",
            "kernel/compat/src/user/process/linux.rs",
        ],
        first_step: "require procd/loaderd tickets for every post-bootstrap exec/fork materialization",
        fallback_removal_gate: "post-bootstrap spawn/exec cannot succeed without a service-issued prepare/commit session",
    },
    ProcessSlowPathMigrationReference {
        area: "credentials, sessions, and process groups",
        ring0_substrate_scope: "security-critical kernel subject IDs and scheduler task identity",
        ring3_owner: "syscalld/procd/sessiond",
        source_surfaces: &[
            "kernel/compat/src/user/syscall/linux/syscalld_ops.rs",
            "kernel/ps/src/user/process_state.rs",
            "kernel/io-manager/src/io/session.rs",
        ],
        first_step: "treat Linux-visible uid/gid/pgid/sid as service-owned state with ring0 subject IDs only as broker facts",
        fallback_removal_gate: "set*id/setpgid/setsid denial cannot be bypassed by mutating kernel process metadata",
    },
    ProcessSlowPathMigrationReference {
        area: "signals and thread-group slow path",
        ring0_substrate_scope: "pending-signal delivery substrate, signal-frame construction, scheduler wake",
        ring3_owner: "procd/syscalld",
        source_surfaces: &[
            "kernel/compat/src/user/syscall/linux/support.rs",
            "kernel/compat/src/user/syscall/linux/service_ops/futex_thread.rs",
            "kernel/ps/src/multitask/scheduler.rs",
        ],
        first_step: "move signal selection, disposition, and thread-group policy to procd while ring0 builds frames",
        fallback_removal_gate: "kill/tgkill/signal-disposition tests show procd decides delivery and ring0 only applies it",
    },
];
// RING3-MIGRATION-REFERENCE END: procd process slow-path policy candidates.
