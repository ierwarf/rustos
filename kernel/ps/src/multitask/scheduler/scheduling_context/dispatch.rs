//! Production dispatch preparation from exact published runtime budget custody.
//!
//! Exact funding stays in the selected transaction, not a CPU scratch cell,
//! until its frame/owner claim succeeds. Refusal releases Context→Domain pins.

use super::super::super::current_identity;
use super::super::{donation_ledger, local_dispatch, scheduling_runtime};
use scheduling_runtime::PublishedDispatchPreparation;

pub(in crate::multitask::scheduler) struct DispatchFunding {
    pub(in crate::multitask::scheduler) task_id: u64,
    pub(in crate::multitask::scheduler) funding: Option<scheduling_runtime::FundingPin>,
}

pub(in crate::multitask::scheduler) fn prepare_grant(slot: usize, cpu: usize) -> Option<DispatchFunding> {
    let Some(publication) = current_identity::read_dispatch(slot) else {
        return None;
    };
    let Some(task_id) = publication.task_id else {
        return None;
    };
    let token = donation_ledger::borrowed_context_charge_token(slot);
    let borrowed = token.map(|(owner_slot, _)| current_identity::read_dispatch(owner_slot));
    let Some(stamp) = local_dispatch::effective_charge_stamp(publication.pager_charge, borrowed)
    else {
        return None;
    };
    let owner_slot = token.map_or(slot, |(owner_slot, _)| owner_slot);
    let owner = match borrowed {
        Some(Some(owner)) => owner,
        Some(None) => return None,
        None => publication,
    };
    let Some(owner_task_id) = owner.task_id else {
        return None;
    };
    let now_ns = crate::arch::clock::monotonic_nanos();
    let funding = if let Some(stamp) = stamp {
        if u64::try_from(owner_slot)
            .ok()
            .and_then(|slot| slot.checked_add(1))
            != Some(stamp.context_slot)
        {
            return None;
        }
        match scheduling_runtime::prepare_published_stamp_dispatch_status(stamp, cpu, now_ns) {
            PublishedDispatchPreparation::Ready => {}
            PublishedDispatchPreparation::DomainRefused => {
                if let Some((domain_slot, cause)) =
                    scheduling_runtime::published_domain_refusal_cause(stamp, now_ns)
                {
                    super::latch_domain_budget_refusal(slot, domain_slot, cause);
                }
                return None;
            }
            PublishedDispatchPreparation::Invalid
            | PublishedDispatchPreparation::ContextRefused => return None,
        }
        let Some(pin) = scheduling_runtime::pin_published_funding(
            stamp,
            cpu,
            owner_task_id,
            token.map_or(0, |(_, reply)| reply),
        ) else {
        return None;
        };
        Some(pin)
    } else {
        None
    };
    let Some(own_now) = current_identity::read_dispatch(slot) else {
        return None;
    };
    let Some(owner_now) = current_identity::read_dispatch(owner_slot) else {
        return None;
    };
    if donation_ledger::borrowed_context_charge_token(slot) != token
        || own_now.task_id != publication.task_id
        || own_now.pager_charge != publication.pager_charge
        || owner_now.task_id != owner.task_id
        || owner_now.pager_charge != owner.pager_charge
    {
        return None;
    }
    // The caller owns this original reply/stamp even if donation moves later.
    Some(DispatchFunding { task_id, funding })
}
