//! Exclusive frame byte mutation, distinct from allocation lifetime and frame
//! publication currency. Byte authority is independent of directory borrowing;
//! no resource leaf lock spans byte access. Exec/MM transactions remain guarded.
use super::super::{SAVED_CONTEXT_BYTES, TASK_STACK_GUARD_BYTES, frame_publication};
use super::{RESOURCES, cpu_leases};
use core::{ptr::NonNull, sync::atomic::Ordering};

pub(in crate::multitask::scheduler) struct FrameWriter {
    slot: usize,
    task_id: Option<u64>,
    base: NonNull<u8>,
    len: usize,
}
impl FrameWriter {
    fn acquire(
        slot: usize,
        task_id: Option<u64>,
        current: bool,
        intent: Option<crate::multitask::current_identity::exec_intent::Credential>,
        ticket: Option<&super::admission::Ticket>,
    ) -> Option<Self> {
        let cell = RESOURCES.get(slot)?;
        let cpu = current.then(nucleus_core::util::lockdep::current_cpu_index);
        // Exec refusal telemetry: only the sealed exec transaction passes an intent.
        let fail = |reason| {
            if intent.is_some() {
                crate::multitask::scheduler::exec_transaction::refuse(reason)
            } else {
                None
            }
        };
        use crate::multitask::scheduler::exec_transaction::ImageRefusal as Why;
        cell.with(|resources| {
            if match ticket {
                Some(ticket) => task_id.is_some() || !ticket.admits_image(slot, resources),
                None => resources.reservation.is_some(),
            } {
                return fail(Why::WriterReservation);
            }
            let owned = match intent {
                Some(intent) => task_id.is_some_and(|id| intent.admits_image(slot, id)),
                None => !crate::multitask::current_identity::exec_intent::busy(slot),
            };
            if !owned {
                return fail(Why::WriterIntent);
            }
            #[cfg(not(test))]
            if !crate::multitask::current_identity::execution::banks::image_custody(slot, cpu) {
                return fail(Why::WriterBankCustody);
            }
            #[cfg(test)]
            if crate::multitask::current_identity::execution::count(slot) != 0
                && !crate::multitask::current_identity::execution::banks::image_custody(slot, cpu)
            {
                return None;
            }
            if super::super::outgoing_image::frozen(slot) {
                return fail(Why::WriterFrozen);
            }
            if resources.closing {
                return fail(Why::WriterClosing);
            }
            if resources.frame_writing {
                return fail(Why::WriterBusyWriting);
            }
            // ORDERING: Acquire pairs with lock-free reader release.
            if cell.readers.load(Ordering::Acquire) != 0 {
                return fail(Why::WriterReaders);
            }
            if cell.pins.load(Ordering::Acquire) != u32::from(current) {
                return fail(Why::WriterPins);
            }
            match task_id {
                None if resources.start.is_none() && resources.binding.is_none() => {}
                Some(id)
                    if resources.start.is_some_and(|start| start.id == id)
                        && resources.binding.is_some_and(|(bound, _)| bound == id) => {}
                _ => return fail(Why::WriterBinding),
            }
            let stack = resources.stack.as_mut()?;
            assert!(
                stack.len() >= TASK_STACK_GUARD_BYTES + SAVED_CONTEXT_BYTES + 64,
                "frame writer allocation is too small"
            );
            let base = NonNull::new(stack.as_mut_ptr())?;
            let len = stack.len();
            resources.frame_writing = true;
            Some(Self {
                slot,
                task_id,
                base,
                len,
            })
        })
    }
    pub(in crate::multitask::scheduler) fn stack_top(&self) -> usize {
        super::super::align_kernel_stack_top(self.base.as_ptr() as usize + self.len)
    }
    pub(in crate::multitask::scheduler) fn exec_stack_top(
        &self,
        slot: usize,
        task_id: u64,
    ) -> usize {
        assert_eq!(
            (self.slot, self.task_id),
            (slot, Some(task_id)),
            "foreign exec frame lease"
        );
        self.stack_top()
    }
    fn initialize(&mut self, f: impl FnOnce(&mut [u8])) {
        // SAFETY: the exclusive writer prevents every new allocation reader,
        // replacement and teardown; acquisition checked no existing readers.
        f(unsafe { core::slice::from_raw_parts_mut(self.base.as_ptr(), self.len) });
    }
}
impl Drop for FrameWriter {
    fn drop(&mut self) {
        let closing = RESOURCES[self.slot].with(|resources| {
            assert!(
                resources.frame_writing,
                "frame writer released without custody"
            );
            resources.frame_writing = false;
            resources.closing
        });
        if closing {
            super::super::retirement_cleanup::mark_reap_pending();
        }
    }
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

pub(super) fn with_reserved_image<R>(
    slot: usize,
    ticket: &super::admission::Ticket,
    f: impl FnOnce(&FrameWriter) -> R,
) -> R {
    irq_excluded(|| {
        let writer = FrameWriter::acquire(slot, None, false, None, Some(ticket))
            .expect("reserved frame construction lacks exact ticket custody");
        f(&writer)
    })
}

pub(super) fn initialize_reserved_storage(
    slot: usize,
    ticket: &super::admission::Ticket,
    f: impl FnOnce(&mut [u8]),
) {
    irq_excluded(|| {
        let mut writer = FrameWriter::acquire(slot, None, false, None, Some(ticket))
            .expect("reserved stack initialization lacks exact ticket custody");
        writer.initialize(f);
    })
}

pub(in crate::multitask::scheduler) fn with_unpublished<R>(
    slot: usize,
    f: impl FnOnce(&FrameWriter) -> R,
) -> R {
    irq_excluded(|| {
        let writer = FrameWriter::acquire(slot, None, false, None, None)
            .expect("frame construction lacks exclusive unpublished custody");
        f(&writer)
    })
}

pub(super) fn initialize_storage(slot: usize, f: impl FnOnce(&mut [u8])) {
    irq_excluded(|| {
        let mut writer = FrameWriter::acquire(slot, None, false, None, None)
            .expect("stack initialization lacks unpublished custody");
        writer.initialize(f);
    });
}

/// Self exec cannot quiesce itself across a reschedulable barrier. Acquire
/// exact sole Current custody under IRQ exclusion before any image effects.
pub(in crate::multitask::scheduler) fn with_current_exec<R>(
    slot: usize,
    task_id: u64,
    f: impl FnOnce(&FrameWriter) -> R,
) -> Option<R> {
    irq_excluded(|| {
        let cpu = nucleus_core::util::lockdep::current_cpu_index();
        with_current_exec_on(slot, task_id, cpu, f)
    })
}
fn with_current_exec_on<R>(
    slot: usize,
    task_id: u64,
    cpu: usize,
    f: impl FnOnce(&FrameWriter) -> R,
) -> Option<R> {
    irq_excluded(|| {
        if cpu_leases::exclusive_image_bank_custody(slot, cpu) != Some(true) {
            return None;
        }
        let writer = FrameWriter::acquire(slot, Some(task_id), true, None, None)?;
        frame_publication::invalidate_before_image_write(slot);
        Some(f(&writer))
    })
}

/// Acquisition precedes all sibling/image side effects. Keep this lease until
/// the caller has published the entire new frame/identity/TLS and released its
/// exec gate. An old observation cannot read while the image is incomplete.
pub(in crate::multitask::scheduler) fn with_exec<R>(
    slot: usize,
    task_id: u64,
    f: impl FnOnce(&FrameWriter) -> R,
) -> Option<R> {
    irq_excluded(|| {
        if !crate::multitask::current_identity::lifecycle(slot).exec_quiesced {
            return None;
        }
        let cpu = nucleus_core::util::lockdep::current_cpu_index();
        let current = cpu_leases::exclusive_image_bank_custody(slot, cpu)?;
        let writer = FrameWriter::acquire(slot, Some(task_id), current, None, None)?;
        // Invalidate every earlier token before the first byte write. This is
        // publication-only; full frame publication remains the caller's job.
        frame_publication::invalidate_before_image_write(slot);
        Some(f(&writer))
    })
}

/// Only the sealed cold exec transaction may acquire this image writer.
pub(in crate::multitask::scheduler) fn with_owned_exec<R>(
    slot: usize,
    task_id: u64,
    current: bool,
    intent: crate::multitask::current_identity::exec_intent::Credential,
    f: impl FnOnce(&FrameWriter) -> R,
) -> Option<R> {
    irq_excluded(|| {
        let cpu = nucleus_core::util::lockdep::current_cpu_index();
        use crate::multitask::scheduler::exec_transaction::{ImageRefusal, refuse};
        let Some(actual) = cpu_leases::exclusive_image_bank_custody(slot, cpu) else {
            return refuse(ImageRefusal::BankBusy);
        };
        if actual != current {
            return refuse(ImageRefusal::CurrentMismatch);
        }
        if !current && !crate::multitask::current_identity::lifecycle(slot).exec_quiesced {
            return refuse(ImageRefusal::NotQuiesced);
        }
        let Some(writer) = FrameWriter::acquire(slot, Some(task_id), current, Some(intent), None)
        else {
            return refuse(ImageRefusal::WriterAcquire);
        };
        frame_publication::invalidate_before_image_write(slot);
        Some(f(&writer))
    })
}
#[cfg(test)]
mod tests;
