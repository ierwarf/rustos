//! Per-slot SIMD and TLS execution payloads, outside Scheduler.
//! SIMD bytes have an exact immutable task binding and a consume-once exclusive
//! access guard. Current/Prepared allocation custody qualifies live access;
//! cold construction/exec/terminal commands retain their enclosing transaction.
use crate::multitask::task_table;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

use super::super::stack_resources::{cpu_leases, task_start};
use super::{RunOwnerState, owner};
use crate::arch::simd::{SimdState, restore_state, save_state};

struct SimdImage {
    task_id: Option<u64>,
    register_ipc: bool,
    state: SimdState,
}
pub(in crate::multitask) struct SlotSimdState {
    busy: AtomicBool,
    image: UnsafeCell<SimdImage>,
}
// SAFETY: all image access is enclosed by the exclusive guard. Its production
// callers exclude local IRQs throughout access and own the slot's Current or
// Prepared bank, which is what excludes every other CPU.
unsafe impl Sync for SlotSimdState {}
struct SimdAccess<'a>(&'a SlotSimdState);
impl Drop for SimdAccess<'_> {
    fn drop(&mut self) {
        #[cfg(test)]
        self.0.busy.store(false, Ordering::Release);
        #[cfg(not(test))]
        {
            core::sync::atomic::compiler_fence(Ordering::SeqCst);
            self.0.busy.store(false, Ordering::Relaxed);
        }
    }
}
impl SlotSimdState {
    #[cfg(test)]
    pub(in crate::multitask) const fn new() -> Self {
        Self {
            busy: AtomicBool::new(false),
            image: UnsafeCell::new(SimdImage {
                task_id: None,
                register_ipc: false,
                state: SimdState::new(),
            }),
        }
    }

    /// Initializes a freshly allocated `SlotSimdState` in place without putting
    /// a multi-kilobyte `SimdState` temporary on the caller's stack.
    ///
    /// # Safety
    /// `dst` must point to valid, aligned, writable storage for `Self`.
    pub(in crate::multitask) unsafe fn init_in_place(dst: *mut Self) {
        // SAFETY: `SimdState` and atomics/bool/Option<u64> inside `SlotSimdState`
        // have no non-zero invariants except setting `None` explicitly below.
        unsafe {
            core::ptr::write_bytes(dst.cast::<u8>(), 0, core::mem::size_of::<Self>());
            core::ptr::addr_of_mut!((*dst).busy).write(AtomicBool::new(false));
            let image = (*dst).image.get();
            core::ptr::addr_of_mut!((*image).task_id).write(None);
            core::ptr::addr_of_mut!((*image).register_ipc).write(false);
        }
    }
    fn claim(&self) -> SimdAccess<'_> {
        // Native tests run many threads as one modeled CPU, so they keep the
        // atomic exclusion. A shipping image has IRQs masked here and the bank
        // that excludes other CPUs, which makes the guard an assertion of that
        // contract; a lock-prefixed compare-exchange (a store-buffer barrier,
        // eight times per register-IPC round trip) is not needed to hold it.
        #[cfg(test)]
        assert!(
            self.busy
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok(),
            "overlapping live SIMD access"
        );
        #[cfg(not(test))]
        {
            assert!(!self.busy.load(Ordering::Relaxed), "overlapping live SIMD access");
            self.busy.store(true, Ordering::Relaxed);
            core::sync::atomic::compiler_fence(Ordering::SeqCst);
        }
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
            task_table::get(self.slot).simd.with(|image| set_register_ipc(image, self.task_id, false));
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
        task_table::get(slot).simd.with(|image| set_register_ipc(image, task_id, true));
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
        task_table::get(slot).simd.with(|image| {
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

#[inline]
fn irq_excluded<R>(f: impl FnOnce() -> R) -> R {
    #[cfg(not(test))]
    {
        if x86_64::instructions::interrupts::are_enabled() {
            x86_64::instructions::interrupts::without_interrupts(f)
        } else {
            f()
        }
    }
    #[cfg(test)]
    {
        f()
    }
}

pub(crate) fn reset_before_publication() {
    for slot in task_table::allocated_slots() {
        let task = task_table::get(slot);
        task.tls_fs_base.store(0, Ordering::Release);
        task.simd.with(|image| {
            image.state = SimdState::new();
            image.register_ipc = false;
            image.task_id = None;
        });
    }
}
#[inline]
pub(crate) fn tls_fs_base(slot: usize) -> u64 {
    task_table::get(slot).tls_fs_base.load(Ordering::Acquire)
}
#[inline]
pub(crate) fn set_tls_fs_base(slot: usize, value: u64) {
    super::super::outgoing_image::assert_mutable(slot);
    task_table::get(slot).tls_fs_base.store(value, Ordering::Release);
}
pub(crate) fn clear_tls_fs_base(slot: usize) {
    super::super::outgoing_image::assert_mutable(slot);
    task_table::get(slot).tls_fs_base.store(0, Ordering::Release);
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
        task_table::get(slot).simd.initialize(task_id);
    });
}

pub(crate) fn reset_simd_state(slot: usize) {
    irq_excluded(|| {
        let task_id = task_start::read(slot)
            .expect("SIMD reset lacks task descriptor")
            .id;
        let terminal = cpu_leases::simd_reset_custody(slot)
            .expect("SIMD reset lacks exclusive lifecycle custody");
        task_table::get(slot).simd.replace(task_id, terminal);
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
        task_table::get(slot).simd.bound(task_id, |state| unsafe { save_state(state) });
    });
}
#[inline]
pub(crate) fn restore_simd_state(slot: usize) {
    irq_excluded(|| {
        let task_id = live_identity(slot, true);
        task_table::get(slot).simd.bound(task_id, |state| unsafe { restore_state(state) });
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
    let image = unsafe { &mut *task_table::get(slot).simd.image.get() };
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
    let image = unsafe { &mut *task_table::get(slot).simd.image.get() };
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
