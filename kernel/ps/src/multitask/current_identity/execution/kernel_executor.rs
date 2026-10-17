//! Owner: exact kernel executor lifetime, not backing-as-resume permission for users.
//! Boundary: Terminal50 reservation exclusion -> Identity51 -> complete Resources53 -> backing ref.
//! Lifecycle: kernel dispatch remains eligible; stop/reuse cannot cross a live scope; Drop releases.
//! Concurrency: producer reserve and capture share Terminal50, eliminating check-before-capture races.
//! Failure: user/foreign/incomplete/stopped/exec/cohort/busy identities refuse before counter effects.
//! Forbidden: naked user pin, copied scope, replacement global lock, or external work under a leaf.
//! Evidence: native exact binding/producer refusal witnesses; no physical hardware proof.
use super::*;

pub(in crate::multitask::current_identity) fn check_metadata(
    writer: &IdentityWriter,
    next: Option<TaskIdentity>,
) {
    assert!(
        writer.kernel_executors == 0
            || next.is_some_and(|i| i.task_id == writer.bound_task_id
                && !i.user_mode
                && i.abi.is_none()
                && i.process_handle.is_none()
                && i.process_id.is_none()),
        "metadata writer withdrew live kernel executor admission"
    );
}

pub(in crate::multitask) fn admit_stop_wait(slot: usize, id: Option<u64>) -> bool {
    with_writer(slot, |writer| {
        let id = id?;
        let identity = writer.identity?;
        if writer.bound_task_id != Some(id)
            || identity.task_id != Some(id)
            || identity.user_mode
            || identity.abi.is_some()
            || identity.process_handle.is_some()
            || identity.process_id.is_some()
            || !writer.lifecycle.dispatchable()
        {
            return None;
        }
        // The last executor may have released after the initial terminal
        // refusal. Persist its exact stop even then; a count==0 rollback
        // would lose the request in that publication gap.
        crate::multitask::scheduler::stack_resources::with_kernel_executor_binding(slot, id, || {
            true
        })
    })
    .flatten()
    .unwrap_or(false)
}

pub(in crate::multitask) struct Executor {
    binding: BindingPin,
    _not_send: core::marker::PhantomData<*mut ()>,
}
impl Executor {
    /// Caller owns the exact Terminal50 row with no admitted retirement producer.
    /// This low-level join never infers Current from a copied slot.
    pub(in crate::multitask) fn capture_binding(
        slot: usize,
        id: u64,
        existing_only: bool,
    ) -> Option<Self> {
        with_writer(slot, |writer| {
            let identity = writer.identity?;
            if identity.user_mode
                || identity.abi.is_some()
                || identity.process_handle.is_some()
                || identity.process_id.is_some()
                || identity.task_id != Some(id)
                || writer.bound_task_id != Some(id)
                || !writer.lifecycle.dispatchable()
                || writer.exec_intent.is_some()
                || !super::super::cohort::admission_ready(slot)
                || (existing_only && writer.kernel_executors == 0)
            {
                return None;
            }
            let next = writer.kernel_executors.checked_add(1)?;
            crate::multitask::scheduler::stack_resources::with_kernel_executor_binding(
                slot,
                id,
                || {
                    let permit = acquire(slot)?;
                    writer.kernel_executors = next;
                    Some(Self {
                        binding: BindingPin { permit, id },
                        _not_send: core::marker::PhantomData,
                    })
                },
            )?
        })?
    }

    pub(in crate::multitask) fn assert_current(&self) {
        let mut preemption = nucleus_core::util::lockdep::DispatchPreemption::enter();
        preemption.mask_irqs();
        self.assert_current_in(&preemption);
    }
    /// Borrow an existing non-yielding CPU scope instead of entering a nested
    /// modeled DispatchPreemption interval during independent construction.
    pub(in crate::multitask) fn assert_current_in(
        &self,
        preemption: &nucleus_core::util::lockdep::DispatchPreemption,
    ) {
        assert_eq!(
            preemption.cpu(),
            nucleus_core::util::lockdep::current_cpu_index()
        );
        assert!(
            crate::multitask::cpu_local::current_cpu_task_slot_admitted()
                && crate::multitask::cpu_local::current_cpu_task_slot()
                    == Some(self.binding.slot()),
            "kernel executor moved outside exact Current custody"
        );
        assert_eq!(
            binding(self.binding.slot()),
            Some(self.binding.id()),
            "kernel executor lost original UID"
        );
    }
}
impl Drop for Executor {
    fn drop(&mut self) {
        with_writer(self.binding.slot(), |writer| {
            assert_eq!(
                writer.bound_task_id,
                Some(self.binding.id()),
                "kernel executor release lost UID"
            );
            assert!(
                writer.kernel_executors != 0,
                "kernel executor release lost admission"
            );
            writer.kernel_executors -= 1;
        });
        // Wake the durable stop worker after last logical resume loan drains.
        // No Class50 lock nests inside Identity51; this is only its existing hint.
        crate::multitask::scheduler::retirement_cleanup::mark_reap_pending();
        // BindingPin drops after Identity51; no backing destruction inside the leaf.
    }
}
