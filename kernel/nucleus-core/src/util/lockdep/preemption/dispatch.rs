//! One non-lock, CPU-local dispatch preemption owner, independent of raw guards.
//! Raw depth still equals held + pending. This separate unit contributes only
//! to effective preemption depth; it cannot cancel or release a raw lock unit.
//! Admission masks IRQs atomically, then permits caller IRQs during a directory
//! wait (TLB/device IPIs must remain serviceable). Protected dispatch masks IRQs
//! continuously; a switched return transfers that owner through post-RSP commit.
use core::{
    marker::PhantomData,
    sync::atomic::{AtomicBool, Ordering},
};

struct Cell {
    live: AtomicBool,
}
impl Cell {
    const fn new() -> Self {
        Self {
            live: AtomicBool::new(false),
        }
    }
    fn acquire(&self, cpu: usize, restore_irq: bool) -> Option<DispatchPreemption<'_>> {
        self.live
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(DispatchPreemption {
            cell: self,
            cpu,
            restore_irq,
            masked: !restore_irq,
            affinity: PhantomData,
        })
    }
    fn release(&self) {
        assert!(
            self.live
                .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "dispatch preemption released without its exact live owner"
        );
    }
}
static CELLS: [Cell; super::super::MAX_TRACKED_CPUS] =
    [const { Cell::new() }; super::super::MAX_TRACKED_CPUS];

/// Unique CPU-local IRQ/preemption custody. Never represents a raw lock.
/// Only bounded non-sleeping directory acquisition may precede `mask_irqs`.
/// Never migrate, block, or explicitly reenable IRQs once protected dispatch starts.
pub struct DispatchPreemption<'a> {
    cell: &'a Cell,
    cpu: usize,
    restore_irq: bool,
    masked: bool,
    affinity: PhantomData<*mut ()>,
}
impl DispatchPreemption<'static> {
    #[track_caller]
    pub fn enter() -> Self {
        #[cfg(rustos_boot_image)]
        {
            let restore_irq = x86_64::instructions::interrupts::are_enabled();
            x86_64::instructions::interrupts::disable();
            let cpu = super::super::current_cpu_index();
            let owner = CELLS
                .get(cpu)
                .expect("dispatch preemption CPU exceeds capacity")
                .acquire(cpu, restore_irq)
                .expect("overlapping CPU dispatch preemption owner");
            // Scheduling IRQs observe the independent unit and defer; TLB and
            // device IPIs still run while a genuine cold directory is busy.
            if restore_irq {
                x86_64::instructions::interrupts::enable();
            }
            owner
        }
        #[cfg(not(rustos_boot_image))]
        Self::enter_model(super::super::current_cpu_index())
    }
    /// Host-only modeled CPU custody; no privileged IRQ or APIC instructions.
    #[cfg(not(rustos_boot_image))]
    pub fn enter_model(cpu: usize) -> Self {
        CELLS
            .get(cpu)
            .expect("modeled dispatch CPU exceeds capacity")
            .acquire(cpu, false)
            .expect("overlapping modeled dispatch preemption owner")
    }
    /// Host-only original CPU owner with caller IF modeled explicitly. This
    /// lends the same actual cell; IRQ-enabled admission is NOT protected work.
    #[cfg(not(rustos_boot_image))]
    pub fn enter_model_with_enabled_irqs(cpu: usize) -> Self {
        CELLS
            .get(cpu)
            .expect("modeled dispatch CPU exceeds capacity")
            .acquire(cpu, true)
            .expect("overlapping modeled dispatch preemption owner")
    }
    #[cfg(not(rustos_boot_image))]
    pub fn model_depth_on(cpu: usize) -> usize {
        depth_on(cpu)
    }
}
impl DispatchPreemption<'_> {
    pub fn cpu(&self) -> usize {
        self.cpu
    }
    pub fn mask_irqs(&mut self) {
        #[cfg(rustos_boot_image)]
        {
            x86_64::instructions::interrupts::disable();
            assert_eq!(
                super::super::current_cpu_index(),
                self.cpu,
                "dispatch protection moved to a foreign CPU"
            );
        }
        self.masked = true;
    }
    pub fn is_masked(&self) -> bool {
        self.masked
    }
    /// A different continuation must restore its own IF from the incoming frame,
    /// not enable IRQs with the outgoing continuation's saved flag.
    pub fn retain_irq_mask(&mut self) {
        self.restore_irq = false;
    }
}
pub(super) fn depth_on(cpu: usize) -> usize {
    usize::from(
        CELLS
            .get(cpu)
            .is_some_and(|cell| cell.live.load(Ordering::Acquire)),
    )
}
impl Drop for DispatchPreemption<'_> {
    fn drop(&mut self) {
        #[cfg(rustos_boot_image)]
        {
            assert!(
                !self.masked || !x86_64::instructions::interrupts::are_enabled(),
                "dispatch preemption lost continuous protected IRQ exclusion"
            );
            x86_64::instructions::interrupts::disable();
            assert_eq!(
                super::super::current_cpu_index(),
                self.cpu,
                "dispatch preemption owner moved to a foreign CPU"
            );
        }
        self.cell.release();
        #[cfg(rustos_boot_image)]
        if self.restore_irq {
            x86_64::instructions::interrupts::enable();
        }
    }
}
#[cfg(test)]
mod tests;
