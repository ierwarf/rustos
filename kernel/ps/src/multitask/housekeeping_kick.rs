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

struct KickSlot {
    task: AtomicU64,
    kicked: AtomicBool,
}

impl KickSlot {
    const fn new() -> Self {
        Self {
            task: AtomicU64::new(0),
            kicked: AtomicBool::new(false),
        }
    }

    fn register(&self, id: u64) {
        // ORDERING: Release publishes the identity before any kick can read it.
        self.task.store(id, Ordering::Release);
    }

    fn arm_and_target(&self) -> Option<u64> {
        // ORDERING: Release publishes the producer's work before the flag; the
        // consumer clears the flag (Acquire) before it scans for that work.
        if self.kicked.swap(true, Ordering::AcqRel) {
            return None;
        }
        // ORDERING: Acquire pairs with the registration store.
        let id = self.task.load(Ordering::Acquire);
        (id != 0).then_some(id)
    }

    fn is_kicked(&self) -> bool {
        self.kicked.load(Ordering::Acquire)
    }

    fn take_kick(&self) -> bool {
        self.kicked.swap(false, Ordering::AcqRel)
    }
}

static HOUSEKEEPING: KickSlot = KickSlot::new();
static SERVICE_CLEANUP: KickSlot = KickSlot::new();

/// Called by the housekeeping task itself, before its first pass.
pub fn register_housekeeping_task() {
    HOUSEKEEPING.register(super::current_task_id().unwrap_or(0));
}

/// Called by the dedicated Ring3 service-cleanup task itself, before its first pass.
pub fn register_service_cleanup_task() {
    SERVICE_CLEANUP.register(super::current_task_id().unwrap_or(0));
}

/// Marks pure kernel housekeeping work ready and wakes only the kernel
/// housekeeping executor (used after a retired-task service cleanup ACK so the
/// reaper immediately recycles the slot without re-kicking service cleanup).
pub fn kick_housekeeping_executor() {
    if let Some(id) = HOUSEKEEPING.arm_and_target() {
        let _ = super::wake_task(id);
    }
}

/// Marks Ring3 service cleanup work ready and wakes the service-cleanup task.
pub fn kick_service_cleanup() {
    if let Some(id) = SERVICE_CLEANUP.arm_and_target() {
        let _ = super::wake_task(id);
    }
}

/// Marks cleanup work ready and wakes both the pure kernel housekeeping
/// executor and the Ring3 service-cleanup task out of their naps.
pub fn kick_housekeeping() {
    kick_housekeeping_executor();
    kick_service_cleanup();
}

/// Ensures the registered housekeeping task is awake, brought to the active
/// runqueue latency floor, and hinted for dispatch even if a prior kick flag
/// was already set while housekeeping lagged behind CPU-bound tasks.
pub fn prioritize_housekeeping() {
    HOUSEKEEPING.kicked.store(true, Ordering::Release);
    prioritize_registered_housekeeping();
}

fn prioritize_registered_housekeeping() {
    // ORDERING: Acquire pairs with the registration store.
    let id = HOUSEKEEPING.task.load(Ordering::Acquire);
    if id != 0 {
        let _ = super::wake_task(id);
        let _ = super::scheduler::ordering_publication::housekeeping(id);
    }
}

/// Whether the scheduler has a task slot a new process could be given right
/// now. An exited task's slot is returned by the housekeeping pass, not by the
/// parent's `wait`, so a loop of fork and wait can briefly find none; a caller
/// that is allowed to wait kicks the pass and asks again.
pub fn task_slot_available() -> bool {
    super::scheduler::task_slot_available()
}

pub fn housekeeping_kicked() -> bool {
    HOUSEKEEPING.is_kicked()
}

/// Consumes the kick; the caller scans for work afterwards.
pub fn take_housekeeping_kick() -> bool {
    HOUSEKEEPING.take_kick()
}

pub fn service_cleanup_kicked() -> bool {
    SERVICE_CLEANUP.is_kicked()
}

/// Consumes the service-cleanup kick; the caller scans for work afterwards.
pub fn take_service_cleanup_kick() -> bool {
    SERVICE_CLEANUP.take_kick()
}

#[cfg(test)]
mod tests {
    use super::KickSlot;

    #[test]
    fn kick_slot_coalesces_duplicate_kicks_until_taken() {
        let slot = KickSlot::new();
        slot.register(42);
        assert!(!slot.is_kicked());
        assert_eq!(slot.arm_and_target(), Some(42));
        assert!(slot.is_kicked());
        assert_eq!(slot.arm_and_target(), None);
        assert!(slot.take_kick());
        assert!(!slot.is_kicked());
        assert_eq!(slot.arm_and_target(), Some(42));
    }

    #[test]
    fn kick_housekeeping_wakes_both_slots_while_executor_kick_is_targeted() {
        let source = include_str!("housekeeping_kick.rs")
            .split_once("#[cfg(test)]")
            .expect("tests below production")
            .0;
        let kick_body = source
            .split_once("pub fn kick_housekeeping() {")
            .expect("kick_housekeeping")
            .1
            .split_once('}')
            .expect("closing brace")
            .0;
        assert!(kick_body.contains("kick_housekeeping_executor()"));
        assert!(kick_body.contains("kick_service_cleanup()"));
        let exec_body = source
            .split_once("pub fn kick_housekeeping_executor() {")
            .expect("kick_housekeeping_executor")
            .1
            .split_once("pub fn kick_service_cleanup()")
            .expect("kick_service_cleanup follows")
            .0;
        assert!(exec_body.contains("HOUSEKEEPING.arm_and_target()"));
        assert!(!exec_body.contains("SERVICE_CLEANUP"));
    }
}

