//! Event wake of the kernel housekeeping task.
//!
//! - **Owner:** `kernel-ps` owns the registration and the kick flag; the executive
//!   owns the housekeeping loop that consumes them.
//! - **Boundary:** A kick is a hint that cleanup work may be ready. It grants no
//!   authority and never replaces the cleanup's own custody checks.
//! - **Lifecycle:** The housekeeping task registers its task id once; producers
//!   kick after publishing work; the task clears the flag before it scans.
//! - **Concurrency:** One atomic flag and one atomic task id; the wake itself is
//!   the ordinary deferrable scheduler wake, safe under raw locks and in IRQs.
//! - **Failure:** A lost or spurious kick costs at most the existing one-tick nap.
//! - **Forbidden:** No cleanup work, allocation or blocking in the kick.
//! - **Evidence:** `core_lifecycle` thread join / fork+wait latency and fork reuse.
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static HOUSEKEEPING_TASK: AtomicU64 = AtomicU64::new(0);
static KICKED: AtomicBool = AtomicBool::new(false);

/// Called by the housekeeping task itself, before its first pass.
pub fn register_housekeeping_task() {
    let id = super::current_task_id().unwrap_or(0);
    // ORDERING: Release publishes the identity before any kick can read it.
    HOUSEKEEPING_TASK.store(id, Ordering::Release);
}

/// Marks cleanup work ready and wakes the housekeeping task out of its nap.
pub fn kick_housekeeping() {
    // ORDERING: Release publishes the producer's work before the flag; the
    // consumer clears the flag (Acquire) before it scans for that work.
    if KICKED.swap(true, Ordering::AcqRel) {
        return;
    }
    // ORDERING: Acquire pairs with the registration store.
    let id = HOUSEKEEPING_TASK.load(Ordering::Acquire);
    if id != 0 {
        let _ = super::wake_task(id);
    }
}

pub fn housekeeping_kicked() -> bool {
    KICKED.load(Ordering::Acquire)
}

/// Consumes the kick; the caller scans for work afterwards.
pub fn take_housekeeping_kick() -> bool {
    KICKED.swap(false, Ordering::AcqRel)
}

