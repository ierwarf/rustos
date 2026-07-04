//! Planning-only inventory for pager-facing memory policy migration.
//!
//! This file is not linked into the memory module.  It separates policy that
//! should become pagerd/syscalld state from ring0 mechanisms that must remain
//! privileged page-table, frame, and TLB substrate.

// RING3-MIGRATION-REFERENCE START: pager/syscalld memory slow-path policy candidates.
// Keep frame allocation, kernel virtual mapping, page-table mutation, TLB
// invalidation, and address-space switch mechanics in ring0.  Move Linux/Win32
// mapping admission, VMA metadata, backing-object policy, and accounting into
// pagerd/syscalld without changing observable mmap/memfd/PE behavior.
#[allow(dead_code)]
struct PagerSlowPathMigrationReference {
    area: &'static str,
    ring0_substrate_scope: &'static str,
    ring3_owner: &'static str,
    source_surfaces: &'static [&'static str],
    first_step: &'static str,
    fallback_removal_gate: &'static str,
}

#[allow(dead_code)]
const PAGER_SLOW_PATH_RING3_BATCHES: &[PagerSlowPathMigrationReference] = &[
    PagerSlowPathMigrationReference {
        area: "VMA layout and mmap admission",
        ring0_substrate_scope: "page-table edits, address-space activation, guard-page enforcement",
        ring3_owner: "pagerd/syscalld/loaderd",
        source_surfaces: &[
            "kernel/mm/src/memory/address_space.rs",
            "kernel/compat/src/user/syscall/linux/memory_ops.rs",
            "kernel/compat/src/user/syscall/linux/mm_broker_ops.rs",
        ],
        first_step: "represent user-visible VMA ranges as pagerd-owned leases while ring0 applies committed mappings",
        fallback_removal_gate: "mmap/brk/mprotect conflict tests fail closed when pagerd denies the layout",
    },
    PagerSlowPathMigrationReference {
        area: "backing object and file mapping policy",
        ring0_substrate_scope: "handle lookup, page copy, physical frame install, shared-map rights enforcement",
        ring3_owner: "pagerd/vfsd/devmgrd",
        source_surfaces: &[
            "kernel/compat/src/user/syscall/linux/mm_broker_ops.rs",
            "kernel/ps/src/user/memfd.rs",
            "kernel/compat/src/user/syscall/linux/service_ops/vfs_socket.rs",
        ],
        first_step: "move file/device/memfd mapping descriptions and cache policy into pagerd-owned mapping sessions",
        fallback_removal_gate: "fd-backed mmap cannot infer read/shared rights from ring0-only handle classes",
    },
    PagerSlowPathMigrationReference {
        area: "PE and loader mapping materialization policy",
        ring0_substrate_scope: "copy committed bytes, install pages, switch address spaces",
        ring3_owner: "loaderd/procd/pagerd",
        source_surfaces: &[
            "kernel/compat/src/user/syscall/linux/proc_broker_ops.rs",
            "kernel/mm/src/memory/address_space.rs",
            "kernel/mm/src/memory/kernel_vm.rs",
        ],
        first_step: "make loaderd/pagerd own image segment plans and let ring0 only validate/apply prepared map operations",
        fallback_removal_gate: "ELF and PE launch fail if loaderd does not provide an explicit mapping plan",
    },
    PagerSlowPathMigrationReference {
        area: "memory accounting and pressure policy",
        ring0_substrate_scope: "frame allocator safety, kernel heap integrity, emergency mappings",
        ring3_owner: "pagerd/rootd/syscalld",
        source_surfaces: &[
            "kernel/mm/src/memory/phys.rs",
            "kernel/mm/src/memory/heap.rs",
            "kernel/ps/src/user/process_state.rs",
        ],
        first_step: "publish bounded memory facts to pagerd/rootd and keep allocation policy outside generic app syscalls",
        fallback_removal_gate: "allocation pressure diagnostics identify service policy decisions instead of ring0 heuristics",
    },
];
// RING3-MIGRATION-REFERENCE END: pager/syscalld memory slow-path policy candidates.
