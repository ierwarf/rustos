//! CPU-private allocation custody. Atomics park owned pins, not slot hints.
//! Only the IRQ-excluded owning CPU mutates its bank. Assembly commits by
//! releasing a parked pin, without allocation or an owner lock.

use super::{RESOURCES, StackPin};
use core::sync::atomic::{AtomicUsize, Ordering};
use nucleus_core::util::lockdep::MAX_TRACKED_CPUS;

struct CpuLeases {
    prepared: AtomicUsize,
    current: AtomicUsize,
    outgoing: AtomicUsize,
}

impl CpuLeases {
    const fn new() -> Self {
        Self {
            prepared: AtomicUsize::new(0),
            current: AtomicUsize::new(0),
            outgoing: AtomicUsize::new(0),
        }
    }

    fn park(cell: &AtomicUsize, pin: StackPin) -> Option<StackPin> {
        let encoded = pin.slot.checked_add(1).expect("CPU resource slot overflow");
        // The atomic bank takes precisely this reference. The only decoding
        // paths below move it back into a unique RAII pin or release it once.
        core::mem::forget(pin);
        let previous = cell.swap(encoded, Ordering::AcqRel);
        (previous != 0).then(|| StackPin { slot: previous - 1 })
    }

    fn take(cell: &AtomicUsize) -> Option<StackPin> {
        let previous = cell.swap(0, Ordering::AcqRel);
        (previous != 0).then(|| StackPin { slot: previous - 1 })
    }

    fn initialize(&self, pin: StackPin) {
        assert_eq!(
            self.current.load(Ordering::Acquire),
            0,
            "CPU resource bank initialized twice"
        );
        assert_eq!(self.prepared.load(Ordering::Acquire), 0);
        assert_eq!(self.outgoing.load(Ordering::Acquire), 0);
        assert!(Self::park(&self.current, pin).is_none());
    }

    fn prepare(&self, pin: StackPin) {
        // A replaced/lost selection releases its old reference outside all
        // leaf owners; the newly parked reference precedes the old release.
        drop(Self::park(&self.prepared, pin));
    }

    fn publish(&self, from: usize, to: usize) {
        assert_eq!(
            self.current.load(Ordering::Acquire),
            from + 1,
            "CPU outgoing allocation lacks current custody"
        );
        assert_eq!(
            self.outgoing.load(Ordering::Acquire),
            0,
            "CPU allocation transition overlaps an uncommitted switch"
        );
        let incoming = Self::take(&self.prepared);
        if from == to && incoming.is_none() {
            return;
        }
        let incoming = incoming.expect("CPU incoming allocation lacks validated claim custody");
        assert_eq!(
            incoming.slot, to,
            "CPU incoming allocation differs from selected frame"
        );
        if from != to {
            let outgoing = Self::take(&self.current).expect("CPU outgoing allocation vanished");
            assert!(Self::park(&self.outgoing, outgoing).is_none());
        }
        // An incoming reference already exists before either prior reference
        // can disappear. Same-task refresh also never passes through zero.
        drop(Self::park(&self.current, incoming));
    }

    fn owns_current(&self, slot: usize) -> bool {
        self.current.load(Ordering::Acquire) == slot + 1
    }

    fn commit(&self) {
        // Called only after assembly installed incoming RSP. StackPin::drop
        // is one lock-free decrement and optional reaper notification.
        drop(Self::take(&self.outgoing));
    }
}

static CPU_LEASES: [CpuLeases; MAX_TRACKED_CPUS] = [const { CpuLeases::new() }; MAX_TRACKED_CPUS];

/// Borrow the ORIGINAL parked Current StackPin under its owning masked CPU.
/// This does not take/reconstruct, clone or newly admit a physical reference.
/// The scope prevents local bank movement until the borrow's last effect;
/// remote reclaim still observes the parked pin itself, not a scalar epoch.
pub(in crate::multitask::scheduler) struct CurrentAllocation<'scope, 'irq> {
    bank: &'static CpuLeases,
    scope: &'scope nucleus_core::util::lockdep::DispatchPreemption<'irq>,
    slot: usize,
    id: u64,
    context: super::super::TaskContext,
    affinity: core::marker::PhantomData<*mut ()>,
}
impl<'scope, 'irq> CurrentAllocation<'scope, 'irq> {
    pub(in crate::multitask::scheduler) fn borrow(
        scope: &'scope nucleus_core::util::lockdep::DispatchPreemption<'irq>,
    ) -> Option<Self> {
        if !scope.is_masked() || scope.cpu() != nucleus_core::util::lockdep::current_cpu_index()
            || nucleus_core::util::lockdep::held_spin_lock_depth() != 0 {
            return None;
        }
        let bank = CPU_LEASES.get(scope.cpu())?;
        let slot = bank.current.load(Ordering::Acquire).checked_sub(1)?;
        let cell = RESOURCES.get(slot)?;
        let (id, context) = cell.with(|r| {
            let id = r.start?.id;
            let context = r.context?;
            if cell.pins.load(Ordering::Acquire) == 0 || r.reservation.is_some()
                || r.reclaim.is_some() || r.frame_writing
                || r.binding.is_none_or(|(bound, _)| bound != id) {
                return None;
            }
            let root = super::super::runqueue::address_space::root(slot);
            match context.process_handle {
                Some(handle) if !r.mm_pin.as_ref().is_some_and(|mm| mm.matches(handle, root)) => return None,
                None if context.user_mode || r.mm_pin.is_some() => return None,
                _ => {}
            }
            Some((id, context))
        })?;
        Some(Self { bank, scope, slot, id, context, affinity: core::marker::PhantomData })
    }
    pub(in crate::multitask::scheduler) fn check(&self) {
        assert!(self.scope.is_masked());
        assert_eq!(self.scope.cpu(), nucleus_core::util::lockdep::current_cpu_index());
        assert!(self.bank.owns_current(self.slot), "original Current allocation moved during borrow");
    }
    pub(in crate::multitask::scheduler) fn slot(&self) -> usize { self.check(); self.slot }
    pub(in crate::multitask::scheduler) fn id(&self) -> u64 { self.check(); self.id }
    pub(in crate::multitask::scheduler) fn context(&self) -> super::super::TaskContext {
        self.check(); self.context
    }
}

/// Independent physical pin qualifier; no CPU bank effects on refusal.
pub(super) fn pin_initial(slot: usize, task_id: u64) -> Option<StackPin> {
    let cell = RESOURCES.get(slot)?;
    let acquired = cell.with(|resources| {
        if resources.closing
            || resources.reservation.is_some()
            || resources.frame_writing
            || resources.start.is_none_or(|start| start.id != task_id)
            || resources.binding.map(|(id, _)| id) != Some(task_id)
        {
            return false;
        }
        cell.pins
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_add(1)
            })
            .expect("CPU allocation pin overflow");
        true
    });
    acquired.then(|| StackPin { slot })
}

pub(in crate::multitask) fn initialize(image: &super::super::bootstrap::InitialCpu<'_>) {
    image.assert_publication();
    let cpu = image.cpu();
    let slot = image.slot();
    let task_id = image.id();
    RESOURCES[slot].with(|resources| {
        assert!(
            resources.context.is_some_and(|context| !context.user_mode
                && context.user_abi.is_none()
                && context.process_handle.is_none()
                && context.scheduling_context.is_bound_to(task_id))
                && resources.mm_pin.is_none(),
            "initial CPU bank lacks exact kernel context"
        )
    });
    let execution = crate::multitask::current_identity::execution::initial(slot, task_id);
    #[cfg(not(test))]
    assert!(
        execution.is_some(),
        "initial CPU lacks exact lifecycle admission"
    );
    let pin =
        pin_initial(slot, task_id).expect("initial CPU allocation lacks exact resource binding");
    CPU_LEASES[cpu].initialize(pin);
    if let Some(execution) = execution {
        crate::multitask::current_identity::execution::banks::initialize(cpu, execution);
    }
}

pub(in crate::multitask::scheduler) fn prepare(cpu: usize, pin: StackPin) {
    CPU_LEASES[cpu].prepare(pin);
}

pub(in crate::multitask) fn publish(cpu: usize, from: usize, to: usize) {
    CPU_LEASES[cpu].publish(from, to);
}

pub(in crate::multitask) fn owns_current(cpu: usize, slot: usize) -> bool {
    CPU_LEASES
        .get(cpu)
        .is_some_and(|bank| bank.owns_current(slot))
}

pub(in crate::multitask::scheduler) fn owns_outgoing(cpu: usize, slot: usize) -> bool {
    CPU_LEASES.get(cpu).is_some_and(|bank| bank.outgoing.load(Ordering::Acquire) == slot + 1)
}

pub(in crate::multitask::scheduler) fn owns_prepared(cpu: usize, slot: usize) -> bool {
    CPU_LEASES
        .get(cpu)
        .is_some_and(|bank| bank.prepared.load(Ordering::Acquire) == slot + 1)
}

pub(in crate::multitask::scheduler) fn exclusive_image_bank_custody(
    slot: usize,
    cpu: usize,
) -> Option<bool> {
    let mut current = false;
    for (index, bank) in CPU_LEASES.iter().enumerate() {
        if bank.prepared.load(Ordering::Acquire) == slot + 1
            || bank.outgoing.load(Ordering::Acquire) == slot + 1
        {
            return None;
        }
        if bank.current.load(Ordering::Acquire) == slot + 1 {
            if index != cpu || current {
                return None;
            }
            current = true;
        }
    }
    Some(current)
}

pub(in crate::multitask::scheduler) fn simd_unpublished_custody(slot: usize) -> bool {
    let cpu = nucleus_core::util::lockdep::current_cpu_index();
    super::simd_resource_state(slot).is_some_and(|(pins, _, bound)| {
        pins == 0 && !bound && exclusive_image_bank_custody(slot, cpu) == Some(false)
    })
}

/// Terminal release or an exact self/fully quiesced peer exec. These commands
/// remain inside their existing cross-owner transaction; a slot hint alone is
/// insufficient. Prepared and outgoing references always refuse replacement.
pub(in crate::multitask::scheduler) fn simd_reset_custody(slot: usize) -> Option<bool> {
    let cpu = nucleus_core::util::lockdep::current_cpu_index();
    let quiesced = crate::multitask::current_identity::lifecycle(slot).exec_quiesced;
    let current = exclusive_image_bank_custody(slot, cpu)?;
    let (pins, closing, bound) = super::simd_resource_state(slot)?;
    if !bound {
        return None;
    }
    if current && pins == 1 && !closing {
        return Some(false);
    }
    if !current && pins == 0 && (closing || quiesced) {
        return Some(closing);
    }
    None
}

pub(in crate::multitask) fn commit(cpu: usize) {
    CPU_LEASES[cpu].commit();
}

#[cfg(test)]
pub(in crate::multitask::scheduler) fn with_test_bank<R>(
    cpu: usize,
    pin: StackPin,
    prepared: bool,
    f: impl FnOnce() -> R,
) -> R {
    let bank = &CPU_LEASES[cpu];
    assert_eq!(bank.current.load(Ordering::Acquire), 0);
    assert_eq!(bank.prepared.load(Ordering::Acquire), 0);
    assert_eq!(bank.outgoing.load(Ordering::Acquire), 0);
    let cell = if prepared {
        &bank.prepared
    } else {
        &bank.current
    };
    assert!(CpuLeases::park(cell, pin).is_none());
    struct Restore<'a>(&'a AtomicUsize);
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            drop(CpuLeases::take(self.0));
        }
    }
    let _restore = Restore(cell);
    f()
}
#[cfg(test)]
mod tests;
