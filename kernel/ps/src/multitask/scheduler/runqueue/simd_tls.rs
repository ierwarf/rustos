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
    register_ipc: bool,
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
                register_ipc: false,
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
            image.register_ipc = false;
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
            image.register_ipc = false;
            if terminal {
                image.task_id = None;
            }
        });
    }
}
static SIMD_STATES: [SlotSimdState; MAX_TASK] = [const { SlotSimdState::new() }; MAX_TASK];

/// Exact-task, non-transferable custody of a register-IPC syscall continuation.
/// This is not a CPU-local flag: migration carries the original slot binding.
#[must_use]
pub struct IpcRegisterSimdGuard {
    slot: usize,
    task_id: u64,
    _not_send: core::marker::PhantomData<*mut ()>,
}

fn set_register_ipc(image: &mut SimdImage, task_id: u64, active: bool) {
    assert_eq!(
        image.task_id,
        Some(task_id),
        "foreign register IPC SIMD identity"
    );
    assert_ne!(
        image.register_ipc, active,
        "unpaired register IPC SIMD boundary"
    );
    image.register_ipc = active;
}

impl Drop for IpcRegisterSimdGuard {
    fn drop(&mut self) {
        irq_excluded(|| {
            assert_eq!(live_identity(self.slot, false), self.task_id);
            SIMD_STATES[self.slot].with(|image| set_register_ipc(image, self.task_id, false));
        });
    }
}

/// # Safety
/// The outer syscall boundary has saved the entering MXCSR/x87 control word,
/// declares all FP/vector data caller-clobbered, and sanitizes that data and
/// restores those controls before *either* user-return route. No wide-SIMD
/// section may span an admitted IPC lane suspension. Interrupt/ordinary
/// scheduler transitions must continue to save and restore the full image.
pub unsafe fn begin_ipc_register_simd() -> IpcRegisterSimdGuard {
    irq_excluded(|| {
        let slot = crate::multitask::cpu_local::current_cpu_task_slot()
            .expect("register IPC SIMD entry lacks current task");
        let task_id = live_identity(slot, false);
        SIMD_STATES[slot].with(|image| set_register_ipc(image, task_id, true));
        IpcRegisterSimdGuard {
            slot,
            task_id,
            _not_send: core::marker::PhantomData,
        }
    })
}

/// Re-form the guard of a register-IPC syscall whose Rust frames were abandoned
/// at a direct block: the marker `begin_ipc_register_simd` set stays set while
/// the task is parked, so this only proves it and takes over the drop.
///
/// # Safety
/// The caller is the parked task's own resume entry, on the CPU that task now
/// runs on, and the syscall that set the marker has not yet returned to user.
pub unsafe fn adopt_ipc_register_simd() -> IpcRegisterSimdGuard {
    irq_excluded(|| {
        let slot = crate::multitask::cpu_local::current_cpu_task_slot()
            .expect("parked register IPC resume lacks current task");
        let task_id = live_identity(slot, false);
        SIMD_STATES[slot].with(|image| {
            assert_eq!(image.task_id, Some(task_id), "foreign parked register IPC SIMD identity");
            assert!(image.register_ipc, "parked register IPC lost its SIMD marker");
        });
        IpcRegisterSimdGuard {
            slot,
            task_id,
            _not_send: core::marker::PhantomData,
        }
    })
}

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
            image.register_ipc = false;
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
    super::super::outgoing_image::assert_mutable(slot);
    TLS_FS_BASE
        .get(slot)
        .expect("scheduler TLS FS base slot exceeds capacity")
        .store(value, Ordering::Release);
}
pub(crate) fn clear_tls_fs_base(slot: usize) {
    super::super::outgoing_image::assert_mutable(slot);
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
/// Lane variants of the two accessors above for a caller that already holds
/// the masked IRQ scope and the slot's Current (save) or Prepared (restore)
/// allocation bank, and knows the task id: they skip the per-call CPU-index
/// derivation, bank-ownership assertion, descriptor read and busy-flag CAS.
///
/// # Safety
/// IRQs are masked on this CPU, this CPU's Current/Prepared bank is `slot`'s,
/// and `task_id` is `slot`'s bound task. No other CPU touches the image while
/// a bank covers it.
#[inline]
pub(crate) unsafe fn save_simd_state_lane(slot: usize, task_id: u64) {
    // SAFETY: caller contract; the bank excludes every other accessor.
    let image = unsafe { &mut *SIMD_STATES[slot].image.get() };
    debug_assert!(
        image.task_id == Some(task_id),
        "lane SIMD image names another task"
    );
    let _ = task_id;
    // SAFETY: the area is the slot's own image and IRQs are masked.
    if !image.register_ipc {
        unsafe { save_state(&mut image.state) };
    }
}
/// # Safety
/// As [`save_simd_state_lane`].
#[inline]
#[cfg(not(test))]
pub(crate) unsafe fn restore_simd_state_lane(slot: usize, task_id: u64) {
    // SAFETY: caller contract; the bank excludes every other accessor.
    let image = unsafe { &mut *SIMD_STATES[slot].image.get() };
    debug_assert!(
        image.task_id == Some(task_id),
        "lane SIMD image names another task"
    );
    let _ = task_id;
    // SAFETY: the area is the slot's own image and IRQs are masked.
    if !image.register_ipc {
        unsafe { restore_state(&image.state) };
    }
}

#[cfg(test)]
mod tests;
