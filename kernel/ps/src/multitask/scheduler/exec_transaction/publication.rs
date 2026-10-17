//! Owner: exact self/peer exec image publication under original ExecImageLease.
//! Boundary: real Current bank, frozen intent/census, FrameWriter and staged MM.
//! Lifecycle: authenticate -> replace exec-only fields/bytes -> metadata -> MM/CPU ACK.
//! Concurrency: one own-CPU non-yielding turn; no raw leaf spans architecture work.
//! Failure: refuse before effects; any admitted partial image remains poisoned by intent.
//! Forbidden: sibling rescan, copied context replay, facade/global serialization or Ready self.
//! Evidence: shared physical owner witnesses; native architecture inputs are not hardware ACK.
use super::super::*;
use super::{ExecImageLease, ExecPublication};
use crate::multitask::{cpu_local, current_identity as identity};
use nucleus_core::util::lockdep::DispatchPreemption;
#[cfg(not(test))]
use x86_64::{PhysAddr, registers::model_specific::FsBase};

pub(in crate::multitask::scheduler) fn commit(
    lease: &ExecImageLease<'_>,
    turn: &DispatchPreemption,
    bootstrap: &mut UserTaskBootstrap,
    by_pid: Option<(u64, u64)>,
) -> Option<ExecPublication> {
    let slot = lease.slot;
    let id = lease.task_id;
    let handle = lease.reservation.process_handle();
    let actor = cpu_local::current_cpu_task_slot()?;
    if turn.cpu() != nucleus_core::util::lockdep::current_cpu_index()
        || !stack_resources::cpu_leases::owns_current(turn.cpu(), actor)
        || lease.current != (slot == actor)
        || (by_pid.is_none() && !lease.current)
        || !lease.matches(slot, id, handle, lease.root, lease.current)
    {
        return None;
    }
    let intent = lease.intent?;
    lease.census?.assert_image(intent.credential());
    let context = stack_resources::context::read(slot)?;
    let start = stack_resources::task_start::read(slot)?;
    let metadata = identity::read(slot)?;
    let lifecycle = identity::lifecycle(slot);
    if lifecycle.retired
        || retirement_cleanup::deferred(slot).is_some()
        || retirement_cleanup::pending(slot)
        || retirement_cleanup::kernel_pending(slot)
        || retirement_cleanup::reason(slot).is_some()
        || !context.user_mode
        || context.process_handle != Some(handle)
        || context.process_id != Some(id)
        || start.id != id
        || metadata.task_id != Some(id)
        || metadata.process_handle != Some(handle)
        || by_pid.is_some_and(|(pid, tid)| {
            pid != id || tid != id || context.user_abi != Some(UserAbi::Linux)
        })
    {
        return None;
    }
    if !lease.current {
        assert!(
            lifecycle.exec_quiesced,
            "scheduler invariant: target exec committed without quiesce admission"
        );
        assert!(
            cpu_local::task_execution_owner(slot).is_none(),
            "scheduler invariant: target exec replaced a remotely running task"
        );
    }
    let affinity = runqueue::affinity_payload::affinity_snapshot(slot);
    assert!(
        affinity.0 != 0 && affinity.1 != 0 && affinity.0 & !affinity.1 == 0,
        "scheduler invariant: exec target has invalid affinity state"
    );
    let stack_top = lease.writer.exec_stack_top(slot, id);
    linux_thread_state::prepare_exec(lease.writer, slot, id, &mut bootstrap.linux_thread_state);
    let fs = bootstrap.linux_thread_state.map(|s| s.fs_base).unwrap_or(0);
    let saved_rsp = if lease.current {
        // This address denotes a consumed frame. Never write or advertise it;
        // the next trap publishes the fresh live continuation.
        runqueue::saved_rsp(slot)
    } else {
        execution_image::write_user_entry_frame(
            stack_top,
            bootstrap,
            crate::arch::gdt::user_code_selector().0 as u64,
            crate::arch::gdt::user_data_selector().0 as u64,
            initial_task_rflags().bits(),
        )
    };
    stack_resources::context::replace_exec(
        slot,
        id,
        bootstrap.abi,
        bootstrap.console_session,
        bootstrap.user_stack,
    )
    .expect("owned exec image lost original context");
    runqueue::wait::set_blocked(slot, false);
    runqueue::wait::set_blocked_since_ticks(slot, 0);
    runqueue::wait::set_ready_since_ticks(
        slot,
        if lease.current {
            0
        } else {
            crate::arch::rtc::ticks()
        },
    );
    runqueue::address_space::replace(slot, lease.root);
    if !lease.current {
        runqueue::set_saved_rsp(slot, saved_rsp);
    }
    runqueue::simd_tls::reset_simd_state(slot);
    stack_resources::task_start::publish(
        slot,
        TaskStart {
            entry: crate::multitask::noop_task_entry,
            id,
        },
    );
    let (base, top) = runqueue::kernel_stack_bounds(slot);
    let (alternate_base, alternate_top) = runqueue::alternate_kernel_stack_bounds(slot);
    let view = PublishedFrameView {
        user_mode_task: true,
        kernel_process_task: false,
        kernel_stack_base: base,
        kernel_stack_top: top,
        alternate_stack_base: alternate_base,
        alternate_stack_top: alternate_top,
        stack_canary_intact: stack_resources::owned_canary_intact(slot)
            .expect("exec image lost guard"),
        scheduler_storage_base: 0,
        scheduler_storage_end: 0,
    };
    stack_resources::publish_binding(slot, id, view);
    frame_publication::publish(
        slot,
        saved_rsp,
        base,
        top,
        alternate_base,
        alternate_top,
        true,
        false,
        view.stack_canary_intact,
        0,
        0,
    );
    task_directory::record(id, slot);
    identity::publish_metadata(
        slot,
        Some(TaskIdentity {
            abi: Some(bootstrap.abi),
            console_session: bootstrap.console_session,
            ..metadata
        }),
    );
    linux_thread_state::install_exec(lease.writer, slot, id, bootstrap.linux_thread_state);
    if lease.current {
        install_current_architecture(lease.root, fs);
    }
    assert_eq!(
        runqueue::affinity_payload::affinity_snapshot(slot),
        affinity,
        "scheduler invariant: exec changed task/process affinity authority"
    );
    let published = lease.publication(handle);
    assert_eq!(cpu_local::current_cpu_task_slot(), Some(actor));
    assert!(stack_resources::cpu_leases::owns_current(turn.cpu(), actor));
    Some(published)
}

fn install_current_architecture(root: u64, fs: u64) {
    #[cfg(rustos_boot_image)]
    {
        crate::memory::paging::load_address_space_phys(PhysAddr::new(root));
        FsBase::write(VirtAddr::new(fs));
    }
    // Native namespace fixture cannot acknowledge physical CR3/FS installation.
    #[cfg(not(rustos_boot_image))]
    let _ = (root, fs);
}

#[cfg(test)]
pub(in crate::multitask::scheduler) mod tests;
