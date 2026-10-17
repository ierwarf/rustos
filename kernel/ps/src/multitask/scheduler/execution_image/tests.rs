//! Owner: reserved dynamic kernel frame and producer publication witnesses.
//! Boundary: trusted frame bytes/metadata, never physical iret/CR3/TSS proof.
//! Lifecycle: constructor owns bytes before exact descriptor/Blocked ACK.
//! Concurrency: shared native fixture; generic/nested writers cannot borrow producer.
//! Failure: refusal preserves bytes/custody; native release is not hardware ACK.
//! Forbidden: no policy/MM/CPU permission inferred from a kernel frame.
//! Evidence: actual production reserved writer and bounded source/native witnesses.
use crate::multitask::scheduler::{
    self as scheduler, current_identity, execution_image::reserved_kernel_entry, root_slots, runqueue, stack_resources,
};
use core::mem;
use scheduler::{
    BlockReason, ConsoleSessionHandle, NICE_0_LOAD, SavedContext, TaskContext, TaskStart,
};
use std::panic::{AssertUnwindSafe, catch_unwind};
const SLOT: usize = 110;
const RIP: u64 = 0xffff_8000_0000_0100;
fn frame(ticket: &stack_resources::admission::Ticket, rsp: usize) -> SavedContext {
    ticket.with_image(|writer| {
        let task_rsp =
            writer.stack_top() - scheduler::TASK_ENTRY_STACK_RESERVE_QWORDS * mem::size_of::<u64>();
        assert_eq!(rsp, task_rsp - mem::size_of::<SavedContext>());
        // SAFETY: the exact exclusive producer retains initialized POD bytes;
        // the checked ABI address lies wholly inside its owned stack allocation.
        unsafe { (rsp as *const SavedContext).read() }
    })
}
fn kernel_metadata(id: u64, rsp: usize) -> TaskContext {
    TaskContext {
        scheduling_context: scheduler::scheduling_context::SchedulingContext::bind(SLOT, id),
        saved_rsp: rsp,
        test_ready: true,
        ready_since_ticks: 0,
        blocked: false,
        blocked_since_ticks: 0,
        wake_armed: false,
        block_reason: BlockReason::None,
        weight: NICE_0_LOAD,
        vruntime_ns: 0,
        exec_start_ticks: 0,
        address_space_root: 0,
        kernel_stack_base: 0,
        kernel_stack_top: 0,
        alternate_kernel_stack_base: 0,
        alternate_kernel_stack_top: 0,
        user_mode: false,
        user_abi: None,
        console_session: ConsoleSessionHandle::SYSTEM,
        process_handle: None,
        process_id: None,
        user_stack: None,
        windows_thread_state: None,
    }
}
fn cleanup() {
    assert!(stack_resources::close_for_reclaim(SLOT));
    stack_resources::task_start::clear(SLOT);
    stack_resources::context::clear(SLOT);
    drop(stack_resources::take_storage(SLOT));
    current_identity::clear(SLOT);
}
#[test]
fn reserved_kernel_entry_frame_keeps_exact_rsp_trailer_registers_flags_and_canary_under_producer() {
    let _fixture = scheduler::tests::boxed_scheduler();
    let ticket = root_slots::reserve(SLOT, 110001).unwrap();
    let rsp = reserved_kernel_entry(&ticket, 8, 16, 0x202, RIP, 0xfeed);
    let saved = frame(&ticket, rsp);
    assert_eq!(
        (saved.cs, saved.ss, saved.rflags, saved.rip, saved.rdi),
        (8, 16, 0x202, RIP, 0xfeed)
    );
    assert_eq!(
        (
            saved.rax, saved.rcx, saved.rdx, saved.rbx, saved.rbp, saved.rsi
        ),
        (0, 0, 0, 0, 0, 0)
    );
    ticket.with_image(|writer| {
        assert_eq!(
            saved.rsp as usize,
            writer.stack_top() - scheduler::TASK_ENTRY_STACK_RESERVE_QWORDS * mem::size_of::<u64>()
        );
        // SAFETY: initialized fixed trailer is inside this exact exclusive allocation.
        let trailer = unsafe { core::slice::from_raw_parts(saved.rsp as *const u64, 3) };
        assert_eq!(trailer, &[saved.rsp, 16, 0]);
    });
    assert_eq!(stack_resources::owned_canary_intact(SLOT), Some(true));
    assert!(stack_resources::context::read(SLOT).is_none());
    root_slots::cancel_unpublished(ticket);
}
#[test]
fn reserved_kernel_writer_refuses_unqualified_boot_writer_and_nested_producer_without_byte_effect()
{
    let _fixture = scheduler::tests::boxed_scheduler();
    let ticket = root_slots::reserve(SLOT, 110002).unwrap();
    let rsp = reserved_kernel_entry(&ticket, 8, 16, 2, RIP, 7);
    let mut entered = false;
    assert!(
        catch_unwind(AssertUnwindSafe(|| {
            stack_resources::frame_access::with_unpublished(SLOT, |_| entered = true)
        }))
        .is_err()
    );
    assert!(!entered);
    ticket.with_image(|_| {
        assert!(
            catch_unwind(AssertUnwindSafe(
                || reserved_kernel_entry(&ticket, 8, 16, 2, RIP + 16, 99)
            ))
            .is_err()
        );
    });
    let saved = frame(&ticket, rsp);
    assert_eq!((saved.rip, saved.rdi), (RIP, 7));
    root_slots::cancel_unpublished(ticket);
}
#[test]
fn complete_kernel_uid_zero_descriptor_is_not_admission_until_reserved_blocked_ack() {
    let _fixture = scheduler::tests::boxed_scheduler();
    let ticket = root_slots::reserve(SLOT, 0).unwrap();
    let rsp = reserved_kernel_entry(&ticket, 8, 16, 2, RIP, 0);
    ticket.begin_commit();
    ticket.install_context(kernel_metadata(0, rsp));
    ticket.publish_start(TaskStart {
        entry: crate::multitask::noop_task_entry,
        id: 0,
    });
    let base = stack_resources::storage_base(SLOT);
    stack_resources::publish_binding(
        SLOT,
        0,
        scheduler::context_validation::PublishedFrameView {
            user_mode_task: false,
            kernel_process_task: true,
            kernel_stack_base: (base + scheduler::TASK_STACK_GUARD_BYTES) as u64,
            kernel_stack_top: scheduler::align_kernel_stack_top(base + scheduler::TASK_STACK_SIZE)
                as u64,
            alternate_stack_base: 0,
            alternate_stack_top: 0,
            stack_canary_intact: true,
            scheduler_storage_base: 0,
            scheduler_storage_end: 0,
        },
    );
    runqueue::affinity_payload::initialize_affinity(SLOT, 1, 1);
    current_identity::publish_metadata(
        SLOT,
        Some(current_identity::TaskIdentity {
            task_id: Some(0),
            ..current_identity::TaskIdentity::default()
        }),
    );
    assert!(stack_resources::with_cold_binding(SLOT, 0, || ()).is_none());
    assert!(current_identity::cohort::Cohort::begin(&[(SLOT, 0)]).is_none());
    assert!(root_slots::admit_blocked(ticket).is_ok());
    assert_eq!(
        runqueue::owner(SLOT).state,
        runqueue::RunOwnerState::Blocked
    );
    assert_eq!(stack_resources::with_cold_binding(SLOT, 0, || ()), Some(()));
    let context = stack_resources::context::read(SLOT).unwrap();
    assert!(!context.user_mode);
    assert!(context.process_handle.is_none());
    assert!(context.scheduling_context.policy().is_none());
    cleanup();
}
