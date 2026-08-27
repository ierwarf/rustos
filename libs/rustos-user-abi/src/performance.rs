//! Cross-subsystem performance contracts.
//!
//! These values are ABI-adjacent acceptance limits, not scheduler tuning
//! suggestions.  Owners may use a shorter deadline, but widening one of these
//! limits requires updating the owning contract and its executable evidence.

use crate::syscall::WAITSET_MAX_INTERESTS;

/// Optimization objective from kernel entry to an interactive desktop.
pub const BOOT_TO_UI_TARGET_MS: u64 = 3_000;
/// Product acceptance ceiling from kernel entry to an interactive desktop.
/// The 3 s objective remains unchanged; this 10 s qualification rail allows
/// bounded diagnosis and recovery without converting a slow boot into an
/// unbounded success path.
pub const BOOT_TO_UI_HARD_LIMIT_MS: u64 = 10_000;

/// One 60 Hz frame, rounded up to the next microsecond.
pub const UI_FRAME_HARD_LIMIT_US: u64 = 16_667;
/// CPU-side preparation may consume at most half of a 60 Hz frame.
pub const UI_FRAME_CPU_TARGET_US: u64 = 8_000;
/// Input arrival to visible cursor progress must stay below this rail.
pub const UI_INPUT_TO_PRESENT_HARD_LIMIT_US: u64 = 50_000;
/// After the first CPU-presented boot frame, the mandatory DVM GPU path gets
/// this bounded local-only activation turn before unrelated policy startup.
pub const UI_BOOT_GPU_ACTIVATION_BUDGET_MS: u64 = 750;

/// Non-consuming readiness and lifecycle maintenance queries.
pub const IPC_READINESS_QUERY_HARD_LIMIT_MS: u64 = 16;
/// Policy-only work that may affect an interactive syscall.
pub const IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS: u64 = 100;
/// Maximum already-queued control requests one service processes before it
/// returns to lifecycle, launch, socket, or other policy work.  The kernel
/// endpoint admission ceiling must retain at least two such bursts so a busy
/// control owner cannot turn bounded draining into starvation.
pub const IPC_CONTROL_DRAIN_BUDGET: usize = 32;
/// Maximum steady-state delay before rootd rechecks lifecycle and control
/// work. Rootd has no multi-source wait object yet, so its supervisor uses the
/// capability-gated timer broker between bounded nonblocking drains. Keeping
/// this at or below the readiness rail prevents an idle root supervisor from
/// remaining runnable while still bounding admission latency.
pub const ROOTD_SUPERVISOR_IDLE_POLL_MS: u64 = 10;
/// Boot/control work that may hash, validate, or commit service state.
pub const IPC_BOOT_CONTROL_HARD_LIMIT_MS: u64 = 5_000;
/// One immutable executable snapshot on an interactive launch path. The
/// remaining boot/launch budget must still cover image validation, mapping,
/// activation, Wayland configure, and the first presented frame.
pub const EXECUTABLE_SNAPSHOT_HARD_LIMIT_MS: u64 = 2_000;
/// One shared absolute deadline covers DVM block readiness and its first
/// generation-bound flush proof during product boot.
pub const DVM_STORAGE_BOOT_READY_HARD_LIMIT_MS: u64 = 4_000;
/// Bulk data operations whose external device may legitimately be slow.
pub const IPC_BULK_DATA_HARD_LIMIT_MS: u64 = 30_000;

/// A live frame/present turn may not synchronously enter another policy
/// service. GPU submission is a direct, capability-checked broker operation.
pub const UI_FRAME_MAX_SYNCHRONOUS_POLICY_IPC: u32 = 0;
/// A previously admitted service publication must be reused without another
/// rootd authorization round trip.
pub const SERVICE_LOOKUP_MAX_IPC_WITH_EXACT_GRANT: u32 = 0;
/// A stable kernel service-registry lookup may not acquire the global
/// publication/revocation mutation lock.
pub const SERVICE_ENDPOINT_STABLE_LOOKUP_MAX_LOCK_ACQUISITIONS: u32 = 0;

/// Address-space binds a batched user-memory admission or write may perform.
///
/// Binding re-derives the caller's identity and takes the per-process state
/// lock -- about 1,240 cycles -- against roughly 110 for a range check and 310
/// for a copy of a few dozen bytes. The batched forms exist so a report of
/// several small buffers binds once, and that is the whole content of them, so
/// it is a declared ceiling rather than a comment.
pub const USER_COPY_BATCH_MAX_ADDRESS_SPACE_BINDS: u32 = 1;
/// Address-space binds one synchronous receive may perform.
///
/// A receive reports a message, a reply capability, and two sender identifiers:
/// it admits the four output ranges once and writes them once. This path bound
/// eight times before the batched forms existed. Nothing was wrong with the
/// bytes it produced, which is exactly why no assertion objected and only a
/// benchmark did, at 12,500 cycles per call.
pub const IPC_RECEIVE_REPORT_MAX_ADDRESS_SPACE_BINDS: u32 = 2;
/// Endpoint response-queue polls in one turn of a synchronous reply wait.
///
/// One before the block is armed, which answers an already completed reply
/// without arming anything, and one after, which is the race fix for a reply
/// that lands between them. Each poll acquires the reply object and the message
/// object, 3,197 cycles measured. A third is a busy-wait, and a busy-wait here
/// returns the same reply to the same caller, so it is invisible except as
/// latency. `PollsPerTurn` in `formal/ipc-reply-deadline/IpcReplyDeadline.tla`
/// is this number.
pub const IPC_REPLY_WAIT_POLLS_PER_TURN: u32 = 2;

/// Debug-sink records one scheduler dispatch may render while it holds the
/// global scheduler guard.
///
/// A milestone is not a counter increment: rendering one writes the line to the
/// debug port, which is a VM exit per byte under KVM, and the emitter first
/// drains whatever deferred records are parked. Doing that inside the guard
/// put an unbounded host-side cost inside the kernel's most serializing
/// critical section. Measured on the budget-exhaustion marker, which fired
/// about sixty times a second: the accounting phase cost 5.9-27 microseconds
/// per dispatch and the guard's hold total was 358 ms per second; latching the
/// event and rendering it from the profile drain moved the same phase to
/// 0.02-0.03 microseconds and the hold total to 146 ms.
///
/// Zero is the ceiling because there is no such thing as a cheap one. A
/// diagnostic that must name an in-guard event latches it and lets the drain,
/// which already runs outside every tracked lock, render it.
pub const SCHEDULER_GUARD_MAX_DEBUG_SINK_RECORDS: u32 = 0;

/// Global task-catalog acquisitions one ordinary dispatch may make.
///
/// The per-CPU run queues, owner words, and wait state are already outside the
/// catalog, so a dispatch enters it once, for the task payload it still owns.
/// This is the Phase-3 ceiling in the form the runtime can check: the census
/// behind `kernel-scheduler-acquire-*` reports the callers, and this says how
/// many the decision itself is allowed to be.
pub const SCHEDULER_DISPATCH_MAX_CATALOG_ACQUISITIONS: u32 = 1;

/// Global process-table acquisitions one synchronous IPC syscall may make.
///
/// The table is a single lock over every process object, and a reference
/// retain plus its release are two acquisitions of it. A synchronous round trip
/// took roughly ten, measured by the per-class acquisition census, which made
/// it the most-acquired tracked lock class under IPC -- ahead of the endpoint,
/// the reply object, and the scheduler catalog itself.
///
/// The ceiling is what keeps a convenience `retain_process` from reappearing on
/// the path: a running thread already pins its own process object, so the hot
/// path answers from the published per-slot state instead.
pub const IPC_SYSCALL_MAX_PROCESS_TABLE_ACQUISITIONS: u32 = 2;

/// Global vfsd admission limit for live epoll provider objects.
///
/// The current kernel has 32 live process slots and 16-bit dynamic descriptor
/// numbers.  Eight independently owned event loops per live process keeps
/// ordinary runtime, UI, and compatibility use independent without admitting
/// an unbounded service registry.  Every object remains subject to
/// `WAITSET_MAX_INTERESTS`, so the product-wide persistent-interest ceiling is
/// machine-readable below.
pub const WAITSET_MAX_EPOLL_OBJECTS: usize = 8 * 32;
/// Maximum persistent vfsd epoll interests across every admitted object.
pub const WAITSET_MAX_GLOBAL_INTERESTS: usize = WAITSET_MAX_EPOLL_OBJECTS * WAITSET_MAX_INTERESTS;

const _: () = assert!(BOOT_TO_UI_TARGET_MS < BOOT_TO_UI_HARD_LIMIT_MS);
const _: () = assert!(UI_FRAME_CPU_TARGET_US < UI_FRAME_HARD_LIMIT_US);
const _: () = assert!(UI_BOOT_GPU_ACTIVATION_BUDGET_MS < BOOT_TO_UI_HARD_LIMIT_MS);
const _: () = assert!(IPC_READINESS_QUERY_HARD_LIMIT_MS < IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS);
const _: () = assert!(IPC_CONTROL_DRAIN_BUDGET > 0);
const _: () = assert!(
    ROOTD_SUPERVISOR_IDLE_POLL_MS > 0
        && ROOTD_SUPERVISOR_IDLE_POLL_MS <= IPC_READINESS_QUERY_HARD_LIMIT_MS
);
const _: () = assert!(IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS < IPC_BOOT_CONTROL_HARD_LIMIT_MS);
const _: () = assert!(EXECUTABLE_SNAPSHOT_HARD_LIMIT_MS < IPC_BOOT_CONTROL_HARD_LIMIT_MS);
const _: () = assert!(DVM_STORAGE_BOOT_READY_HARD_LIMIT_MS < BOOT_TO_UI_HARD_LIMIT_MS);
const _: () = assert!(IPC_BOOT_CONTROL_HARD_LIMIT_MS < IPC_BULK_DATA_HARD_LIMIT_MS);
// A receive reports four buffers through the two batched forms, so its ceiling
// is exactly two of theirs. Raising the batch ceiling without raising this one
// would let the receive silently regain a bind.
const _: () = assert!(
    IPC_RECEIVE_REPORT_MAX_ADDRESS_SPACE_BINDS == 2 * USER_COPY_BATCH_MAX_ADDRESS_SPACE_BINDS
);
// The pre-arm poll and the post-arm race fix. One would drop the race fix; more
// than two is a busy-wait.
const _: () = assert!(IPC_REPLY_WAIT_POLLS_PER_TURN == 2);
const _: () = assert!(WAITSET_MAX_EPOLL_OBJECTS <= u16::MAX as usize + 1);
const _: () = assert!(WAITSET_MAX_GLOBAL_INTERESTS >= WAITSET_MAX_INTERESTS);
