//! Owner: original donor/root participant loans through reply reservation publication.
//! Boundary: Terminal50 -> Identity51 -> Resources53 capture, then Donation ledger leaf.
//! Lifecycle: candidate basis -> exact participant loans -> original basis revalidation -> publish.
//! Concurrency: no leaf spans participant capture; original pins exclude terminal drain/reuse.
//! Failure: missing/foreign/closed participants or cancelled funding source refuse before insert.
//! Forbidden: scalar Current, directory hints or derived object IDs as execution/funding grants.
//! Evidence: actual born resources and original ledger source; CPU/IRQ native entry is modeled.
use super::{
    current_identity as identity, donation_ledger, runqueue, stack_resources, task_directory,
    IpcCallAdmission, MAX_TASK, SYSTEM_CLASS_WEIGHT_FLAG,
};
use nucleus_core::util::lockdep::DispatchPreemption;

/// Actual non-Copy participants remain borrowed through the original ledger effect.
/// These loans own metadata/backing lifetime only, NOT an execution interval.
pub(super) struct Loans {
    donor: identity::execution::BindingPin,
    _root: Option<identity::execution::BindingPin>,
    basis: donation_ledger::FundingBasis,
    context: kernel_object::api::identity::ObjectIdentity,
}
impl Loans {
    pub(super) fn basis(&self) -> &donation_ledger::FundingBasis {
        &self.basis
    }
    fn capture(id: u64) -> Option<Self> {
        let donor = participant(id)?;
        let basis = donation_ledger::funding_basis(donor.slot(), donor.id())?;
        Self::from_original(donor, basis)
    }
    fn from_original(
        donor: identity::execution::BindingPin,
        basis: donation_ledger::FundingBasis,
    ) -> Option<Self> {
        let (root_slot, root_id) = basis.root();
        let root = if root_slot == donor.slot() && root_id == donor.id() {
            None
        } else {
            Some(identity::execution::pin_donation(root_slot, root_id)?)
        };
        let context = stack_resources::context::read(root_slot)?;
        if stack_resources::task_start::read(root_slot).map(|start| start.id) != Some(root_id)
            || !context.scheduling_context.is_bound_to(root_id)
        {
            return None;
        }
        Some(Self {
            donor,
            _root: root,
            basis,
            context: context.scheduling_context.identity(),
        })
    }
    pub(super) fn system_class(&self) -> bool {
        participant_system_class(&self.donor)
    }
}
pub(super) fn participant(id: u64) -> Option<identity::execution::BindingPin> {
    let inspect = |slot| identity::execution::pin_donation(slot, id);
    task_directory::hint(id)
        .and_then(inspect)
        .or_else(|| (0..MAX_TASK).find_map(inspect))
}
fn participant_system_class(pin: &identity::execution::BindingPin) -> bool {
    !runqueue::is_idle_slot(pin.slot())
        && (runqueue::weight::value(pin.slot()) & SYSTEM_CLASS_WEIGHT_FLAG != 0
            || donation_ledger::inherited_system(pin.slot()))
}
pub(super) struct ReceiveLoans<'a> {
    pub(super) donor: &'a Loans,
    pub(super) receiver: &'a identity::execution::BindingPin,
}
fn scope_valid(scope: &DispatchPreemption<'_>) -> bool {
    scope.is_masked() && scope.cpu() == nucleus_core::util::lockdep::current_cpu_index()
}
fn refused() -> IpcCallAdmission {
    IpcCallAdmission {
        system_class: false,
        donation_reserved: false,
        scheduling_context: None,
        scheduling_context_owner_task_id: None,
    }
}
/// Total donor resolution, including non-Current and root/idle bindings. No catalog fallback.
pub(in crate::multitask) fn reserve_call_with(
    scope: &DispatchPreemption<'_>,
    id: u64,
) -> IpcCallAdmission {
    if !scope_valid(scope) {
        return refused();
    }
    let Some(loans) = Loans::capture(id) else {
        return refused();
    };
    let system_class = loans.system_class();
    let donation_reserved = donation_ledger::reserve_owned(&loans, system_class);
    // The original actual root SC, not a pure derived UID or a freshly resolved hint.
    IpcCallAdmission {
        system_class,
        donation_reserved,
        scheduling_context: Some(loans.context),
        scheduling_context_owner_task_id: Some(loans.basis.root().1),
    }
}
pub(in crate::multitask) fn reserve_call(id: u64) -> IpcCallAdmission {
    let mut scope = DispatchPreemption::enter();
    scope.mask_irqs();
    reserve_call_with(&scope, id)
}
pub(in crate::multitask) fn reserve_priority_with(scope: &DispatchPreemption<'_>, id: u64) -> bool {
    if !scope_valid(scope) {
        return false;
    }
    let Some(loans) = Loans::capture(id) else {
        return false;
    };
    donation_ledger::reserve_owned(&loans, true)
}
pub(in crate::multitask) fn reserve_priority(id: u64) -> bool {
    let mut scope = DispatchPreemption::enter();
    scope.mask_irqs();
    reserve_priority_with(&scope, id)
}
pub(in crate::multitask) fn system_class_with(scope: &DispatchPreemption<'_>, id: u64) -> bool {
    scope_valid(scope) && participant(id).is_some_and(|pin| participant_system_class(&pin))
}
pub(in crate::multitask) fn system_class(id: u64) -> bool {
    let mut scope = DispatchPreemption::enter();
    scope.mask_irqs();
    system_class_with(&scope, id)
}

/// Original reservation funding is retained, not replaced with the donor's
/// currently selected source if another incoming reply has since become newest.
pub(in crate::multitask) fn bind_with(
    scope: &DispatchPreemption<'_>,
    reply: u64,
    donor_id: u64,
    receiver_id: u64,
    inherit: bool,
) -> bool {
    let Some(donor) = participant(donor_id) else {
        return false;
    };
    let Some(receiver) = participant(receiver_id) else {
        return false;
    };
    bind_original(scope, reply, donor, &receiver, inherit).is_some()
}

/// Transfer the original participants to the producer, which retains donor,
/// root and receiver through queue/FIFO publication and Current wait commit.
pub(super) fn bind_original(
    scope: &DispatchPreemption<'_>,
    reply: u64,
    donor: identity::execution::BindingPin,
    receiver: &identity::execution::BindingPin,
    inherit: bool,
) -> Option<Loans> {
    use super::ipc_donation::DonationNamespace::IpcReply;
    if !scope_valid(scope) || reply == 0 || donor.id() == receiver.id() {
        return None;
    }
    let basis = match donation_ledger::bind_basis(reply, IpcReply, donor.slot(), donor.id()) {
        Ok(Some(basis)) => Some(basis),
        Ok(None) if inherit => None,
        _ => return None,
    };
    let reserved = basis.is_some();
    let basis = basis.or_else(|| donation_ledger::funding_basis(donor.slot(), donor.id()))?;
    let donor = Loans::from_original(donor, basis)?;
    let loans = ReceiveLoans { donor: &donor, receiver };
    let bound = if reserved {
        donation_ledger::bind_owned(&loans, reply, IpcReply)
    } else {
        donation_ledger::upsert_owned(&loans, reply, IpcReply)
    };
    bound.then_some(donor)
}
pub(in crate::multitask) fn bind(
    reply: u64,
    donor_id: u64,
    receiver_id: u64,
    inherit: bool,
) -> bool {
    let mut scope = DispatchPreemption::enter();
    scope.mask_irqs();
    bind_with(&scope, reply, donor_id, receiver_id, inherit)
}
pub(in crate::multitask) fn pager_with(
    scope: &DispatchPreemption<'_>,
    token: u64,
    donor_id: u64,
    receiver_id: u64,
) -> bool {
    if !scope_valid(scope) || token == 0 || donor_id == receiver_id {
        return false;
    }
    let Some(donor) = Loans::capture(donor_id) else {
        return false;
    };
    let Some(receiver) = participant(receiver_id) else {
        return false;
    };
    let loans = ReceiveLoans { donor: &donor, receiver: &receiver };
    donation_ledger::upsert_owned(
        &loans,
        token,
        super::ipc_donation::DonationNamespace::PagerFault,
    )
}
pub(in crate::multitask) fn pager(token: u64, donor_id: u64, receiver_id: u64) -> bool {
    let mut scope = DispatchPreemption::enter();
    scope.mask_irqs();
    pager_with(&scope, token, donor_id, receiver_id)
}

#[cfg(test)]
pub(super) mod tests;
