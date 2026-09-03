//! Lock-free current-task pager authority and normal-context VMA publication.
//!
//! Exception entry consumes only the CPU-local identity and atomic VMA
//! publication. Mapping publication and revocation retain the target process
//! in normal syscall context, but publication no longer builds page tables:
//! it reserves address space and proves the range is free.

use core::sync::atomic::{AtomicU64, Ordering};

use x86_64::{VirtAddr, instructions::interrupts};

use super::{current_user_process_id, published_current_identity};
use crate::multitask::{cpu_local, process_table};

/// Anonymous pages admitted by reservation, and the intermediate page-table
/// frames reservation-time preparation would have had to build for them.
///
/// The second number is the whole point of the change and is otherwise
/// invisible: preparation allocated one table frame per 2 MiB of *reserved*
/// address space before a byte was touched, so it is what a boot stops paying.
/// It is an upper bound - preparation reused a table that already existed -
/// and it is reported against `pager-fault-table-installed`, which counts the
/// tables the fault path actually builds for the memory a process touches.
static RESERVED_ANONYMOUS_PAGES: AtomicU64 = AtomicU64::new(0);
static AVOIDED_PREPARATION_TABLES: AtomicU64 = AtomicU64::new(0);

/// The tables reservation-time preparation would have built for `page_count`:
/// one per block at each level, plus one per level for the partial ends.
fn preparation_table_cost(page_count: u64) -> u64 {
    const PT_SPAN: u64 = 512;
    const PD_SPAN: u64 = PT_SPAN * 512;
    const PDPT_SPAN: u64 = PD_SPAN * 512;
    page_count.div_ceil(PT_SPAN) + page_count.div_ceil(PD_SPAN) + page_count.div_ceil(PDPT_SPAN)
}

fn record_reservation(page_count: u64) {
    let pages = RESERVED_ANONYMOUS_PAGES.fetch_add(page_count, Ordering::Relaxed) + page_count;
    let cost = preparation_table_cost(page_count);
    let before = AVOIDED_PREPARATION_TABLES.fetch_add(cost, Ordering::Relaxed);
    let avoided = before + cost;
    // A reservation adds more than one table, so an exact-multiple gate would
    // step over its boundary and stop reporting after the first line.
    if before == 0 || before / 256 != avoided / 256 {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Memory,
            "pager-anon-reservation",
            pages,
            avoided,
        );
    }
}

fn published_current_pager_binding() -> Option<(
    u64,
    process_table::ProcessHandle,
    process_table::ProcessIdentity,
)> {
    let identity = published_current_identity()?;
    let (task_id, _, handle, _) = identity.user_binding()?;
    let process = process_table::published_live_process_identity(handle)?;
    Some((task_id, handle, process))
}

/// Lock-free authority used to bill one pager fault to the exact native or
/// IPC-donated scheduling context that was executing at exception entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PagerChargeSnapshot {
    pub context_slot: u64,
    pub context_generation: u64,
    pub scheduling_domain: u64,
    pub policy_epoch: u64,
    pub period_ns: u64,
    pub charge_token: u64,
}

/// Resolve pager billing without acquiring the scheduler lock.
pub fn current_pager_charge_snapshot() -> Option<PagerChargeSnapshot> {
    interrupts::without_interrupts(|| {
        let current_slot = cpu_local::current_cpu_task_slot()?;
        let (owner_slot, donated_reply) =
            crate::multitask::scheduler::borrowed_context_charge_token(current_slot)
                .unwrap_or((current_slot, 0));
        let stamp = crate::multitask::current_identity::read(owner_slot)?.pager_charge?;
        let expected_context_slot = u64::try_from(owner_slot).ok()?.checked_add(1)?;
        if stamp.context_slot != expected_context_slot
            || stamp.context_generation == 0
            || stamp.scheduling_domain == 0
            || stamp.policy_epoch == 0
            || stamp.period_ns == 0
        {
            return None;
        }
        Some(PagerChargeSnapshot {
            context_slot: stamp.context_slot,
            context_generation: stamp.context_generation,
            scheduling_domain: stamp.scheduling_domain,
            policy_epoch: stamp.policy_epoch,
            period_ns: stamp.period_ns,
            charge_token: if donated_reply != 0 {
                donated_reply
            } else {
                stamp.context_generation
            },
        })
    })
}

/// Stamp and publish a pager region for the current exact process/MM epoch.
pub fn publish_current_pager_vma(
    template: rustos_user_abi::pager::PagerVmRegionWire,
) -> Result<rustos_user_abi::pager::PagerVmRegionWire, crate::multitask::pager_vma::PagerVmaError> {
    let process_id =
        current_user_process_id().ok_or(crate::multitask::pager_vma::PagerVmaError::Stale)?;
    publish_pager_vma_for_process(process_id, template)
}

/// Admit a range and publish a VMA for one retained process.
///
/// No page table is built here. Reservation-time preparation made `mmap` cost
/// one frame per 2 MiB of *reserved* address space, so a large reservation paid
/// for memory it might never touch; the fault path now publishes the tables it
/// needs, and this admission only has to prove the range is free.
pub fn publish_pager_vma_for_process(
    process_id: u64,
    template: rustos_user_abi::pager::PagerVmRegionWire,
) -> Result<rustos_user_abi::pager::PagerVmRegionWire, crate::multitask::pager_vma::PagerVmaError> {
    let retained = process_table::retain_process_by_pid(process_id)
        .ok_or(crate::multitask::pager_vma::PagerVmaError::Stale)?;
    let page_count = usize::try_from(
        (template.end.saturating_sub(template.start)) / rustos_user_abi::pager::PAGER_PAGE_BYTES,
    )
    .map_err(|_| crate::multitask::pager_vma::PagerVmaError::Pressure)?;
    let vacant = retained
        .with_state_mut(|_, state| {
            state
                .address_space()
                .user_range_is_unmapped(VirtAddr::new(template.start), page_count)
        })
        .map_err(|_| crate::multitask::pager_vma::PagerVmaError::Pressure)?;
    if !vacant {
        return Err(crate::multitask::pager_vma::PagerVmaError::Overlap);
    }
    record_reservation(page_count as u64);
    let identity = retained
        .live_identity()
        .ok_or(crate::multitask::pager_vma::PagerVmaError::Stale)?;
    crate::multitask::pager_vma::publish(retained.handle(), identity, template)
}

/// Resolve one current-task fault without scheduler or process-state locks.
pub fn current_pager_vma_snapshot(
    address: u64,
    access: u16,
) -> Result<crate::multitask::pager_vma::PagerVmaSnapshot, crate::multitask::pager_vma::PagerVmaError>
{
    interrupts::without_interrupts(|| {
        let (task_id, handle, process) = published_current_pager_binding()
            .ok_or(crate::multitask::pager_vma::PagerVmaError::Stale)?;
        let region = crate::multitask::pager_vma::lookup(handle, process, address, access)?;
        Ok(crate::multitask::pager_vma::PagerVmaSnapshot {
            task_id,
            process_id: process.process_id(),
            region,
        })
    })
}

/// Acquires an exact current-task publication permit for the one IRQ-off leaf
/// CAS that resolves an anonymous first-touch fault.  The permit proves that a
/// concurrent VMA writer has either not started, or will wait for this CAS to
/// finish after withdrawing the publication.
pub fn current_pager_fault_install_permit(
    request: rustos_user_abi::pager::PagerFaultRequestWire,
) -> Result<
    (
        crate::multitask::pager_vma::PagerFaultInstallPermit,
        rustos_user_abi::pager::PagerVmRegionWire,
    ),
    crate::multitask::pager_vma::PagerVmaError,
> {
    interrupts::without_interrupts(|| {
        let (task_id, handle, process) = published_current_pager_binding()
            .ok_or(crate::multitask::pager_vma::PagerVmaError::Stale)?;
        if request.task_id != task_id {
            return Err(crate::multitask::pager_vma::PagerVmaError::Stale);
        }
        crate::multitask::pager_vma::acquire_fault_install(handle, process, request)
    })
}

/// Revoke the exact current-process VMA generation before removing PTEs.
pub fn revoke_current_pager_vma(
    start: u64,
    vma_generation: u64,
) -> Result<rustos_user_abi::pager::PagerVmRegionWire, crate::multitask::pager_vma::PagerVmaError> {
    interrupts::without_interrupts(|| {
        let (_, handle, process) = published_current_pager_binding()
            .ok_or(crate::multitask::pager_vma::PagerVmaError::Stale)?;
        crate::multitask::pager_vma::revoke(handle, process, start, vma_generation)
    })
}

/// Withdraw one exact non-current pager VMA before its PTEs are removed.
pub fn revoke_pager_vma_for_process(
    process_id: u64,
    start: u64,
    vma_generation: u64,
) -> Result<rustos_user_abi::pager::PagerVmRegionWire, crate::multitask::pager_vma::PagerVmaError> {
    let retained = process_table::retain_process_by_pid(process_id)
        .ok_or(crate::multitask::pager_vma::PagerVmaError::Stale)?;
    let identity = retained
        .live_identity()
        .ok_or(crate::multitask::pager_vma::PagerVmaError::Stale)?;
    crate::multitask::pager_vma::revoke(retained.handle(), identity, start, vma_generation)
}
