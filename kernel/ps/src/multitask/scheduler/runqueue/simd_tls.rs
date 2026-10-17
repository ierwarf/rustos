//! Per-slot SIMD and TLS execution payloads, outside Scheduler.
//! SIMD bytes have an exact immutable task binding and a consume-once exclusive
//! access guard. Current/Prepared allocation custody qualifies live access;
//! cold construction/exec/terminal commands retain their enclosing transaction.
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::super::stack_resources::{cpu_leases, task_start};
use super::{MAX_TASK, RunOwnerState, owner};
use crate::arch::simd::{SimdState, restore_state, save_state};

static TLS_FS_BASE: [AtomicU64; MAX_TASK] = [const { AtomicU64::new(0) }; MAX_TASK];

struct SimdImage {
    task_id: Option<u64>,
    state: SimdState,
}
struct SlotSimdState {
    busy: AtomicBool,
    image: UnsafeCell<SimdImage>,
}
// SAFETY: all image access is enclosed by the exclusive atomic guard. Its
// production callers additionally exclude local IRQs throughout access.
unsafe impl Sync for SlotSimdState {}
struct SimdAccess<'a>(&'a SlotSimdState);
impl Drop for SimdAccess<'_> {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::Release);
    }
}
impl SlotSimdState {
    const fn new() -> Self {
        Self {
            busy: AtomicBool::new(false),
            image: UnsafeCell::new(SimdImage {
                task_id: None,
                state: SimdState::new(),
            }),
        }
    }
    fn claim(&self) -> SimdAccess<'_> {
        assert!(
            self.busy
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok(),
            "overlapping live SIMD access"
        );
        SimdAccess(self)
    }
    fn with<R>(&self, f: impl FnOnce(&mut SimdImage) -> R) -> R {
        let _access = self.claim();
        // SAFETY: only this consume-once guard can expose the image.
        f(unsafe { &mut *self.image.get() })
    }
    fn initialize(&self, task_id: u64) {
        self.with(|image| {
            assert!(
                image.task_id.is_none(),
                "live SIMD image rebound without terminal release"
            );
            image.state = SimdState::new();
            image.task_id = Some(task_id);
        });
    }
    fn bound<R>(&self, task_id: u64, f: impl FnOnce(&mut SimdState) -> R) -> R {
        self.with(|image| {
            assert_eq!(image.task_id, Some(task_id), "foreign SIMD task identity");
            f(&mut image.state)
        })
    }
    fn replace(&self, task_id: u64, terminal: bool) {
        self.with(|image| {
            assert_eq!(
                image.task_id,
                Some(task_id),
                "foreign SIMD replacement identity"
            );
            image.state = SimdState::new();
            if terminal {
                image.task_id = None;
            }
        });
    }
}
static SIMD_STATES: [SlotSimdState; MAX_TASK] = [const { SlotSimdState::new() }; MAX_TASK];

fn irq_excluded<R>(f: impl FnOnce() -> R) -> R {
    #[cfg(not(test))]
    {
        x86_64::instructions::interrupts::without_interrupts(f)
    }
    #[cfg(test)]
    {
        f()
    }
}

pub(crate) fn reset_before_publication() {
    for tls in &TLS_FS_BASE {
        tls.store(0, Ordering::Release);
    }
    for state in &SIMD_STATES {
        state.with(|image| {
            image.state = SimdState::new();
            image.task_id = None;
        });
    }
}
#[inline]
pub(crate) fn tls_fs_base(slot: usize) -> u64 {
    TLS_FS_BASE
        .get(slot)
        .expect("scheduler TLS FS base slot exceeds capacity")
        .load(Ordering::Acquire)
}
#[inline]
pub(crate) fn set_tls_fs_base(slot: usize, value: u64) {
    TLS_FS_BASE
        .get(slot)
        .expect("scheduler TLS FS base slot exceeds capacity")
        .store(value, Ordering::Release);
}
pub(crate) fn clear_tls_fs_base(slot: usize) {
    TLS_FS_BASE[slot].store(0, Ordering::Release);
}

pub(crate) fn initialize_simd_state(slot: usize, task_id: u64) {
    irq_excluded(|| {
        assert_eq!(
            owner(slot).state,
            RunOwnerState::Dormant,
            "scheduler SIMD state initialized after owner publication"
        );
        assert!(
            cpu_leases::simd_unpublished_custody(slot),
            "SIMD construction has live CPU references"
        );
        SIMD_STATES[slot].initialize(task_id);
    });
}

pub(crate) fn reset_simd_state(slot: usize) {
    irq_excluded(|| {
        let task_id = task_start::read(slot)
            .expect("SIMD reset lacks task descriptor")
            .id;
        let terminal = cpu_leases::simd_reset_custody(slot)
            .expect("SIMD reset lacks exclusive lifecycle custody");
        SIMD_STATES[slot].replace(task_id, terminal);
    });
}

fn access_has_custody(current: bool, prepared: bool, restoring: bool) -> bool {
    current || (restoring && prepared)
}

fn live_identity(slot: usize, restoring: bool) -> u64 {
    let cpu = nucleus_core::util::lockdep::current_cpu_index();
    let current = cpu_leases::owns_current(cpu, slot);
    let prepared = cpu_leases::owns_prepared(cpu, slot);
    assert!(
        access_has_custody(current, prepared, restoring),
        "SIMD access lacks CPU execution/selection custody"
    );
    task_start::read(slot)
        .expect("SIMD access lacks task descriptor")
        .id
}
#[inline]
pub(crate) fn save_simd_state(slot: usize) {
    irq_excluded(|| {
        let task_id = live_identity(slot, false);
        SIMD_STATES[slot].bound(task_id, |state| unsafe { save_state(state) });
    });
}
#[inline]
pub(crate) fn restore_simd_state(slot: usize) {
    irq_excluded(|| {
        let task_id = live_identity(slot, true);
        SIMD_STATES[slot].bound(task_id, |state| unsafe { restore_state(state) });
    });
}
#[cfg(test)]
mod tests;
