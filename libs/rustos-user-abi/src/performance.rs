//! Cross-subsystem performance contracts.
//!
//! These values are ABI-adjacent acceptance limits, not scheduler tuning
//! suggestions.  Owners may use a shorter deadline, but widening one of these
//! limits requires updating the owning contract and its executable evidence.

/// Optimization objective from kernel entry to an interactive desktop.
pub const BOOT_TO_UI_TARGET_MS: u64 = 3_000;
/// Product acceptance ceiling from kernel entry to an interactive desktop.
pub const BOOT_TO_UI_HARD_LIMIT_MS: u64 = 5_000;

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
/// Best-effort deferred maintenance charged to one foreground syscall turn.
pub const IPC_FOREGROUND_MAINTENANCE_SLICE_MS: u64 = 1;
/// Policy-only work that may affect an interactive syscall.
pub const IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS: u64 = 100;
/// Boot/control work that may hash, validate, or commit service state.
pub const IPC_BOOT_CONTROL_HARD_LIMIT_MS: u64 = 5_000;
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

const _: () = assert!(BOOT_TO_UI_TARGET_MS < BOOT_TO_UI_HARD_LIMIT_MS);
const _: () = assert!(UI_FRAME_CPU_TARGET_US < UI_FRAME_HARD_LIMIT_US);
const _: () = assert!(UI_BOOT_GPU_ACTIVATION_BUDGET_MS < BOOT_TO_UI_HARD_LIMIT_MS);
const _: () = assert!(IPC_FOREGROUND_MAINTENANCE_SLICE_MS < IPC_READINESS_QUERY_HARD_LIMIT_MS);
const _: () = assert!(IPC_READINESS_QUERY_HARD_LIMIT_MS < IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS);
const _: () = assert!(IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS < IPC_BOOT_CONTROL_HARD_LIMIT_MS);
const _: () = assert!(IPC_BOOT_CONTROL_HARD_LIMIT_MS < IPC_BULK_DATA_HARD_LIMIT_MS);
