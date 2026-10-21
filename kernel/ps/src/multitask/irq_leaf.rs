//! IRQ-shared scheduler leaves: local mask precedes the unchanged ranked raw
//! acquisition and survives its release. No dispatch unit, task grant, external
//! callback or global scheduler authority is introduced. Native tests model only
//! the privileged IF effects; they use the same guard and release ordering.
use core::{marker::PhantomData, ops::{Deref, DerefMut}};
use nucleus_core::util::lockdep::{TrackedSpinLock, TrackedSpinGuard};

pub(super) struct IrqLeaf<T, const CLASS: u8> {
    inner: TrackedSpinLock<T, CLASS>,
}

// Field order is lifetime-critical: raw unlock FIRST, restore local IF LAST.
// Mask's !Send also prevents an IRQ restore on a different execution context.
pub(super) struct Guard<'a, T, const CLASS: u8> {
    inner: TrackedSpinGuard<'a, T, CLASS>, _mask: Mask,
}
struct Mask {
    restore: bool,
    _local: PhantomData<*mut ()>,
}
impl Mask {
    fn enter() -> Self {
        let restore = enabled();
        disable();
        Self { restore, _local: PhantomData }
    }
}
impl Drop for Mask {
    fn drop(&mut self) {
        assert!(!enabled(), "IRQ leaf lost continuous local exclusion");
        if self.restore { enable(); }
    }
}
impl<T, const CLASS: u8> IrqLeaf<T, CLASS> {
    pub(super) const fn new(value: T) -> Self {
        Self { inner: TrackedSpinLock::new(value) }
    }
    #[track_caller]
    pub(super) fn lock(&self) -> Guard<'_, T, CLASS> {
        let mask = Mask::enter();
        // SAFETY: `mask` disabled local IRQs and, by field order, outlives the
        // raw guard, so IF stays clear from acquisition through release.
        let inner = unsafe { self.inner.lock_masked() };
        Guard { inner, _mask: mask }
    }
    #[track_caller]
    pub(super) fn try_lock(&self) -> Option<Guard<'_, T, CLASS>> {
        let mask = Mask::enter();
        let inner = self.inner.try_lock()?;
        Some(Guard { inner, _mask: mask })
    }
}
impl<T, const CLASS: u8> Deref for Guard<'_, T, CLASS> {
    type Target = T;
    fn deref(&self) -> &T { &self.inner }
}
impl<T, const CLASS: u8> DerefMut for Guard<'_, T, CLASS> {
    fn deref_mut(&mut self) -> &mut T { &mut self.inner }
}
fn enabled() -> bool {
    #[cfg(rustos_boot_image)]
    { x86_64::instructions::interrupts::are_enabled() }
    #[cfg(all(not(rustos_boot_image), test))]
    { MODEL_IF.with(|state| state.get()) }
    #[cfg(all(not(rustos_boot_image), not(test)))]
    { false }
}
fn disable() {
    #[cfg(rustos_boot_image)]
    x86_64::instructions::interrupts::disable();
    #[cfg(all(not(rustos_boot_image), test))]
    MODEL_IF.with(|state| state.set(false));
}
fn enable() {
    #[cfg(rustos_boot_image)]
    x86_64::instructions::interrupts::enable();
    #[cfg(all(not(rustos_boot_image), test))]
    {
        // Observe the actual raw unlock before the modeled IF restoration.
        let check = RESTORE_CHECK.with(|check| check.borrow_mut().take());
        if let Some(check) = check { check(); }
        MODEL_IF.with(|state| state.set(true));
    }
}
#[cfg(all(not(rustos_boot_image), test))]
std::thread_local! {
    static MODEL_IF: core::cell::Cell<bool> = const { core::cell::Cell::new(true) };
    static RESTORE_CHECK: core::cell::RefCell<Option<alloc::boxed::Box<dyn FnOnce()>>> = const {
        core::cell::RefCell::new(None)
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    const CLASS: u8 = nucleus_core::util::lockdep::LockClass::SchedulerRetirementCleanup as u8;
    #[test]
    fn shared_leaf_preserves_outer_cpu_owner_and_original_irq_state_through_nested_release() {
        let _namespace = super::super::scheduler::execution_test_serial_guard();
        let mut cpu = nucleus_core::util::lockdep::DispatchPreemption::enter();
        cpu.mask_irqs();
        let first = IrqLeaf::<_, CLASS>::new(1);
        let second = IrqLeaf::<_, CLASS>::new(2);
        assert!(enabled()); // explicit native IF input, NOT dispatch permission
        let guard = first.lock();
        assert!(!enabled());
        let nested = second.lock();
        assert!(!enabled());
        drop(nested);
        assert!(!enabled());
        assert_eq!(nucleus_core::util::lockdep::DispatchPreemption::model_depth_on(cpu.cpu()), 1);
        drop(guard);
        assert!(enabled());
        drop(cpu);
        MODEL_IF.with(|state| state.set(false));
        drop(first.lock());
        assert!(!enabled());
        MODEL_IF.with(|state| state.set(true));
    }
    #[test]
    fn failed_try_and_unwind_restore_only_the_original_mask_after_unlock() {
        let leaf = alloc::sync::Arc::new(IrqLeaf::<_, CLASS>::new(7));
        let held = leaf.lock();
        assert!(leaf.try_lock().is_none());
        assert!(!enabled());
        let probe = leaf.clone();
        RESTORE_CHECK.with(|check| *check.borrow_mut() = Some(alloc::boxed::Box::new(move || {
            assert!(probe.inner.try_lock().is_some(), "IF restored before actual raw unlock");
        })));
        drop(held);
        assert!(enabled());
        assert!(catch_unwind(AssertUnwindSafe(|| {
            let mut held = leaf.lock();
            *held = 9;
            assert!(!enabled());
            panic!("native unwind after leaf effect");
        })).is_err());
        assert!(enabled());
        assert_eq!(*leaf.lock(), 9);
        assert!(enabled());
    }
}
