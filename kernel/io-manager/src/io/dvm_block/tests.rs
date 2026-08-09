//! In-memory DVM block transport state and protocol tests.
//!
//! The explicit parent `#[path]` keeps every test at
//! `io::dvm_block::tests::<name>`.

use super::*;
use alloc::vec;
use driver_domain_protocol::{
    DVM_BLOCK_DATA_SLOT_BYTES, DVM_BLOCK_FEATURE_FLUSH, DVM_BLOCK_FEATURE_FUA,
    DVM_BLOCK_FLAG_DVM_READY,
};
use ed25519_dalek::{Signer, SigningKey};

#[test]
fn fixed_nonblock_ivshmem_topology_is_negative_cached_only_after_enumeration() {
    assert!(!fixed_pci_topology_lacks_block_aperture(0, 0));
    assert!(fixed_pci_topology_lacks_block_aperture(2, 0));
    assert!(!fixed_pci_topology_lacks_block_aperture(2, 1));
}

#[test]
fn block_shared_aperture_requires_prefetchable_write_back_atomic_memory() {
    assert!(fixed_block_shared_bar_shape(
        false,
        true,
        DVM_BLOCK_APERTURE_BYTES
    ));
    assert!(!fixed_block_shared_bar_shape(
        true,
        true,
        DVM_BLOCK_APERTURE_BYTES
    ));
    assert!(!fixed_block_shared_bar_shape(
        false,
        false,
        DVM_BLOCK_APERTURE_BYTES
    ));
    assert!(!fixed_block_shared_bar_shape(
        false,
        true,
        DVM_BLOCK_APERTURE_BYTES - 1
    ));

    let source = include_str!("../dvm_block.rs");
    assert_eq!(source.matches("mmio::map_shared_write_back(").count(), 1);
    assert!(!source.contains("mmio::map_write_combining(resource.start, resource_len)"));
}

fn aperture_bytes(aperture: &mut [u64]) -> &mut [u8] {
    // SAFETY: The u64 backing guarantees the production ABI's alignment;
    // the byte slice spans the exact same initialized allocation.
    unsafe {
        core::slice::from_raw_parts_mut(
            aperture.as_mut_ptr().cast::<u8>(),
            core::mem::size_of_val(aperture),
        )
    }
}

fn test_state() -> (alloc::vec::Vec<u64>, alloc::vec::Vec<u32>, DvmBlockState) {
    let mut aperture = vec![0_u64; DVM_BLOCK_APERTURE_BYTES as usize / 8];
    let mut registers = vec![0_u32; (IVSHMEM_DOORBELL_OFFSET + core::mem::size_of::<u32>()) / 4];
    let mut header = DvmBlockHeader::new(
        7,
        1024 * 1024,
        4096,
        4096,
        DVM_BLOCK_FEATURE_FLUSH | DVM_BLOCK_FEATURE_FUA,
    )
    .with_epoch_signature([0x5a; 64]);
    header.flags |= DVM_BLOCK_FLAG_RUSTOS_READY | DVM_BLOCK_FLAG_DVM_READY;
    aperture_bytes(&mut aperture)[..DVM_BLOCK_HEADER_RECORD_BYTES]
        .copy_from_slice(&header.encode());
    let state = DvmBlockState::new(
        aperture.as_mut_ptr().cast::<u8>(),
        registers.as_mut_ptr().cast::<u8>(),
        header,
    );
    (aperture, registers, state)
}

#[test]
fn revoked_transport_accepts_only_a_signed_newer_epoch() {
    let (mut aperture, _registers, mut state) = test_state();
    let key = SigningKey::from_bytes(&[0x42; 32]);
    state.revoke(DvmBlockRevokeReason::TestManual);

    let unsigned = DvmBlockHeader::new(
        8,
        1024 * 1024,
        4096,
        4096,
        DVM_BLOCK_FEATURE_FLUSH | DVM_BLOCK_FEATURE_FUA,
    );
    let forged = unsigned.with_epoch_signature([0x33; 64]);
    aperture_bytes(&mut aperture)[..DVM_BLOCK_HEADER_RECORD_BYTES]
        .copy_from_slice(&forged.encode());
    assert!(!try_rebind_signed_epoch_with_key(
        &mut state,
        key.verifying_key().to_bytes()
    ));
    assert!(state.revoked);

    let signed =
        unsigned.with_epoch_signature(key.sign(&unsigned.epoch_signing_bytes()).to_bytes());
    aperture_bytes(&mut aperture)[..DVM_BLOCK_HEADER_RECORD_BYTES]
        .copy_from_slice(&signed.encode());
    assert!(try_rebind_signed_epoch_with_key(
        &mut state,
        key.verifying_key().to_bytes()
    ));
    assert_eq!(state.geometry.generation, 8);
    assert!(!state.revoked);
    assert_ne!(
        load_u32(state.base, FLAGS_OFFSET, Ordering::Acquire) & DVM_BLOCK_FLAG_RUSTOS_READY,
        0
    );
}

#[test]
fn revoke_reason_codes_are_nonzero_and_unique() {
    for (index, reason) in DvmBlockRevokeReason::ALL.iter().enumerate() {
        assert_ne!(*reason as u64, 0);
        for earlier in &DvmBlockRevokeReason::ALL[..index] {
            assert_ne!(*reason as u64, *earlier as u64);
        }
    }
}

#[test]
fn revoke_reports_once_before_clearing_and_is_terminal() {
    let (_aperture, _registers, mut state) = test_state();
    let ticket = state
        .submit(DvmBlockOperation::Read, 8, &[], 4096, false)
        .expect("valid request must occupy a slot before revoke");
    let expected_flags = state.geometry.flags & !DVM_BLOCK_FLAG_DVM_READY;
    let observed_flags = state.geometry.flags;
    let mut reports = 0;
    assert!(
        state.revoke_with_observer(DvmBlockRevokeReason::TestManual, |observation| {
            reports += 1;
            assert_eq!(observation.reason, DvmBlockRevokeReason::TestManual);
            assert_eq!(observation.generation, ticket.generation);
            assert_eq!(observation.flags, observed_flags);
            assert_eq!(observation.expected_fixed_flags, expected_flags);
            assert_eq!(observation.request_producer, 1);
            assert_eq!(observation.request_consumer, 0);
            assert_eq!(observation.completion_producer, 0);
            assert_eq!(observation.completion_consumer, 0);
        })
    );
    assert!(state.revoked);
    assert!(state.pending.iter().all(Option::is_none));
    assert_eq!(
        load_u32(state.base, FLAGS_OFFSET, Ordering::Acquire) & DVM_BLOCK_FLAG_RUSTOS_READY,
        0
    );
    assert!(
        !state.revoke_with_observer(DvmBlockRevokeReason::CursorInvalid, |_| {
            reports += 1;
        })
    );
    assert_eq!(reports, 1);
    assert_eq!(
        state.submit(DvmBlockOperation::Read, 8, &[], 4096, false),
        Err(DvmBlockError::Revoked)
    );
}

#[test]
fn readiness_publication_is_conditional_and_non_mutating_on_mismatch() {
    let (_aperture, _registers, state) = test_state();
    let original = load_u32(state.base, FLAGS_OFFSET, Ordering::Acquire);
    assert!(!publish_rustos_ready(state.base, 0));
    assert_eq!(
        load_u32(state.base, FLAGS_OFFSET, Ordering::Acquire),
        original
    );

    fetch_and_u32(
        state.base,
        FLAGS_OFFSET,
        !DVM_BLOCK_FLAG_RUSTOS_READY,
        Ordering::AcqRel,
    );
    assert!(publish_rustos_ready(state.base, DVM_BLOCK_FLAG_DVM_READY));
    assert_eq!(
        load_u32(state.base, FLAGS_OFFSET, Ordering::Acquire),
        DVM_BLOCK_FLAG_DVM_READY | DVM_BLOCK_FLAG_RUSTOS_READY
    );
}

#[test]
fn request_and_completion_bind_exact_slot_epoch_and_durability() {
    let (mut aperture, registers, mut state) = test_state();
    let data = vec![0x5a_u8; 4096];
    let ticket = state
        .submit(DvmBlockOperation::Write, 8, &data, 4096, true)
        .unwrap();
    let request_offset = DVM_BLOCK_REQUEST_RING_OFFSET as usize;
    let request: [u8; DVM_BLOCK_RECORD_BYTES] = aperture_bytes(&mut aperture)
        [request_offset..request_offset + DVM_BLOCK_RECORD_BYTES]
        .try_into()
        .unwrap();
    let request = DvmBlockRequest::decode(&request).unwrap();
    assert_eq!(request.request_id, ticket.request_id);
    assert_eq!(
        registers[IVSHMEM_DOORBELL_OFFSET / 4],
        ivshmem_doorbell_value(BLOCK_DVM_PEER_ID, BLOCK_DVM_REQUEST_VECTOR_INDEX)
    );
    assert_eq!(
        &aperture_bytes(&mut aperture)
            [DVM_BLOCK_DATA_OFFSET as usize..DVM_BLOCK_DATA_OFFSET as usize + data.len()],
        data.as_slice()
    );

    let completion = DvmBlockCompletion {
        generation: request.generation,
        request_id: request.request_id,
        operation_id: request.operation_id,
        status: DvmBlockCompletionStatus::Success,
        data_slot: request.data_slot,
        completed_bytes: request.data_len,
        durable_through_operation_id: request.operation_id,
    };
    let completion_offset = DVM_BLOCK_COMPLETION_RING_OFFSET as usize;
    aperture_bytes(&mut aperture)[completion_offset..completion_offset + DVM_BLOCK_RECORD_BYTES]
        .copy_from_slice(&completion.encode());
    let base = aperture.as_mut_ptr().cast::<u8>();
    store_u64(base, REQUEST_CONSUMER_OFFSET, 1, Ordering::Release);
    store_u64(base, COMPLETION_PRODUCER_OFFSET, 1, Ordering::Release);
    assert_eq!(
        state.poll(ticket, &mut []).unwrap(),
        DvmBlockPoll::Completed(0)
    );
}

#[test]
fn valid_flush_completion_keeps_transport_live_for_first_64kib_read() {
    let (mut aperture, _registers, mut state) = test_state();
    let flush = state
        .submit(DvmBlockOperation::Flush, 0, &[], 0, false)
        .expect("valid flush must submit");
    let request_offset = DVM_BLOCK_REQUEST_RING_OFFSET as usize;
    let request = DvmBlockRequest::decode(
        &aperture_bytes(&mut aperture)[request_offset..request_offset + DVM_BLOCK_RECORD_BYTES]
            .try_into()
            .unwrap(),
    )
    .expect("flush request must encode");
    let completion = DvmBlockCompletion {
        generation: request.generation,
        request_id: request.request_id,
        operation_id: request.operation_id,
        status: DvmBlockCompletionStatus::Success,
        data_slot: request.data_slot,
        completed_bytes: 0,
        durable_through_operation_id: request.operation_id,
    };
    let completion_offset = DVM_BLOCK_COMPLETION_RING_OFFSET as usize;
    aperture_bytes(&mut aperture)[completion_offset..completion_offset + DVM_BLOCK_RECORD_BYTES]
        .copy_from_slice(&completion.encode());
    store_u64(state.base, REQUEST_CONSUMER_OFFSET, 1, Ordering::Release);
    store_u64(state.base, COMPLETION_PRODUCER_OFFSET, 1, Ordering::Release);
    assert_eq!(
        state.poll(flush, &mut []).expect("valid flush completion"),
        DvmBlockPoll::Completed(0)
    );
    state
        .finish(flush)
        .expect("completed flush must release its slot");

    let read = state
        .submit(
            DvmBlockOperation::Read,
            8,
            &[],
            DVM_BLOCK_DATA_SLOT_BYTES,
            false,
        )
        .expect("first 64KiB read after a valid flush must submit");
    let read_request_offset = request_offset + DVM_BLOCK_RECORD_BYTES;
    let read_request = DvmBlockRequest::decode(
        &aperture_bytes(&mut aperture)
            [read_request_offset..read_request_offset + DVM_BLOCK_RECORD_BYTES]
            .try_into()
            .unwrap(),
    )
    .expect("second request record must encode the 64KiB read");
    assert_eq!(read.generation, 7);
    assert_eq!(read.request_id, 2);
    assert_eq!(read.data_slot, 1);
    assert_eq!(read_request.generation, read.generation);
    assert_eq!(read_request.request_id, read.request_id);
    assert_eq!(read_request.operation_id, 0);
    assert!(matches!(read_request.operation, DvmBlockOperation::Read));
    assert_eq!(read_request.flags, 0);
    assert_eq!(read_request.sector, 8);
    assert_eq!(read_request.data_len, DVM_BLOCK_DATA_SLOT_BYTES);
    assert_eq!(read_request.data_slot, read.data_slot);
    assert_eq!(state.next_request_id, 3);
    assert_eq!(state.next_operation_id, 2);
    assert_eq!(state.request_producer, 2);
    assert_eq!(
        load_u64(state.base, REQUEST_PRODUCER_OFFSET, Ordering::Acquire),
        2
    );

    let read_data_start = DVM_BLOCK_DATA_OFFSET as usize
        + read.data_slot as usize * DVM_BLOCK_DATA_SLOT_BYTES as usize;
    let read_data_end = read_data_start + DVM_BLOCK_DATA_SLOT_BYTES as usize;
    aperture_bytes(&mut aperture)[read_data_start..read_data_end].fill(0xa5);
    let read_completion = DvmBlockCompletion {
        generation: read_request.generation,
        request_id: read_request.request_id,
        operation_id: read_request.operation_id,
        status: DvmBlockCompletionStatus::Success,
        data_slot: read_request.data_slot,
        completed_bytes: read_request.data_len,
        durable_through_operation_id: 0,
    };
    let read_completion_offset = completion_offset + DVM_BLOCK_RECORD_BYTES;
    aperture_bytes(&mut aperture)
        [read_completion_offset..read_completion_offset + DVM_BLOCK_RECORD_BYTES]
        .copy_from_slice(&read_completion.encode());
    store_u64(state.base, REQUEST_CONSUMER_OFFSET, 2, Ordering::Release);
    store_u64(state.base, COMPLETION_PRODUCER_OFFSET, 2, Ordering::Release);
    let mut output = vec![0_u8; DVM_BLOCK_DATA_SLOT_BYTES as usize];
    assert_eq!(
        state
            .poll(read, &mut output)
            .expect("matching 64KiB read completion must remain live"),
        DvmBlockPoll::Completed(DVM_BLOCK_DATA_SLOT_BYTES as usize)
    );
    assert!(output.iter().all(|byte| *byte == 0xa5));
    assert_eq!(state.completion_consumer, 2);
    assert_eq!(
        load_u64(state.base, COMPLETION_CONSUMER_OFFSET, Ordering::Acquire),
        2
    );
    state
        .finish(read)
        .expect("completed 64KiB read must release its slot");
    assert!(!state.revoked);
    assert!(state.current_header().is_ok());
}

#[test]
fn stale_completion_revokes_the_transport() {
    let (mut aperture, _registers, mut state) = test_state();
    let ticket = state
        .submit(DvmBlockOperation::Read, 8, &[], 4096, false)
        .unwrap();
    let completion = DvmBlockCompletion {
        generation: ticket.generation - 1,
        request_id: ticket.request_id,
        operation_id: 0,
        status: DvmBlockCompletionStatus::Success,
        data_slot: ticket.data_slot,
        completed_bytes: 4096,
        durable_through_operation_id: 0,
    };
    let completion_offset = DVM_BLOCK_COMPLETION_RING_OFFSET as usize;
    aperture_bytes(&mut aperture)[completion_offset..completion_offset + DVM_BLOCK_RECORD_BYTES]
        .copy_from_slice(&completion.encode());
    let base = aperture.as_mut_ptr().cast::<u8>();
    store_u64(base, REQUEST_CONSUMER_OFFSET, 1, Ordering::Release);
    store_u64(base, COMPLETION_PRODUCER_OFFSET, 1, Ordering::Release);
    assert_eq!(
        state.poll(ticket, &mut [0_u8; 4096]),
        Err(DvmBlockError::Protocol)
    );
    assert!(state.revoked);
}

#[test]
fn fault_points_cover_reads_mutations_and_durability() {
    assert_eq!(
        fault_point_for_operation(DvmBlockOperation::Read),
        "block.read"
    );
    assert_eq!(
        fault_point_for_operation(DvmBlockOperation::Write),
        "block.write"
    );
    assert_eq!(
        fault_point_for_operation(DvmBlockOperation::Discard),
        "block.write"
    );
    assert_eq!(
        fault_point_for_operation(DvmBlockOperation::WriteZeroes),
        "block.write"
    );
    assert_eq!(
        fault_point_for_operation(DvmBlockOperation::Flush),
        "block.flush"
    );

    for (operation, data, data_len) in [
        (DvmBlockOperation::Read, &[][..], 4096),
        (DvmBlockOperation::Write, &[0x5a_u8; 4096][..], 4096),
    ] {
        let (mut aperture, registers, mut state) = test_state();
        let request_id = state.next_request_id;
        let operation_id = state.next_operation_id;
        assert_eq!(
            state.submit_with_fault_decision(operation, 8, data, data_len, false, |_| true),
            Err(DvmBlockError::DeviceFault)
        );
        assert_eq!(state.next_request_id, request_id);
        assert_eq!(state.next_operation_id, operation_id);
        assert_eq!(state.request_producer, 0);
        assert!(state.pending.iter().all(Option::is_none));
        assert_eq!(
            load_u64(state.base, REQUEST_PRODUCER_OFFSET, Ordering::Acquire),
            0
        );
        assert_eq!(registers[IVSHMEM_DOORBELL_OFFSET / 4], 0);
        assert!(
            aperture_bytes(&mut aperture)
                [DVM_BLOCK_DATA_OFFSET as usize..DVM_BLOCK_DATA_OFFSET as usize + 4096]
                .iter()
                .all(|byte| *byte == 0)
        );
    }

    let (mut aperture, registers, mut state) = test_state();
    let ticket = state
        .submit_with_fault_decision(DvmBlockOperation::Flush, 0, &[], 0, false, |_| true)
        .expect("fault injection must preserve a real DVM request/completion round trip");
    assert_eq!(state.request_producer, 1);
    assert_eq!(
        load_u64(state.base, REQUEST_PRODUCER_OFFSET, Ordering::Acquire),
        1
    );
    assert_ne!(registers[IVSHMEM_DOORBELL_OFFSET / 4], 0);

    let request_offset = DVM_BLOCK_REQUEST_RING_OFFSET as usize;
    let request = DvmBlockRequest::decode(
        &aperture_bytes(&mut aperture)[request_offset..request_offset + DVM_BLOCK_RECORD_BYTES]
            .try_into()
            .unwrap(),
    )
    .unwrap();
    let completion = DvmBlockCompletion {
        generation: request.generation,
        request_id: request.request_id,
        operation_id: request.operation_id,
        status: DvmBlockCompletionStatus::Success,
        data_slot: request.data_slot,
        completed_bytes: 0,
        durable_through_operation_id: request.operation_id,
    };
    let completion_offset = DVM_BLOCK_COMPLETION_RING_OFFSET as usize;
    aperture_bytes(&mut aperture)[completion_offset..completion_offset + DVM_BLOCK_RECORD_BYTES]
        .copy_from_slice(&completion.encode());
    store_u64(state.base, REQUEST_CONSUMER_OFFSET, 1, Ordering::Release);
    store_u64(state.base, COMPLETION_PRODUCER_OFFSET, 1, Ordering::Release);
    assert_eq!(state.poll(ticket, &mut []), Err(DvmBlockError::DeviceFault));
    state.finish(ticket).unwrap();
    assert!(state.pending.iter().all(Option::is_none));
}

#[test]
fn cancellation_keeps_the_slot_owned_until_the_exact_completion() {
    let (mut aperture, _registers, mut state) = test_state();
    let ticket = state
        .submit(DvmBlockOperation::Read, 8, &[], 4096, false)
        .unwrap();
    state.cancel(ticket).unwrap();
    assert!(state.pending[ticket.data_slot as usize].is_some());
    assert_eq!(
        state.poll(ticket, &mut [0_u8; 4096]),
        Err(DvmBlockError::Cancelled)
    );

    let completion = DvmBlockCompletion {
        generation: ticket.generation,
        request_id: ticket.request_id,
        operation_id: 0,
        status: DvmBlockCompletionStatus::Success,
        data_slot: ticket.data_slot,
        completed_bytes: 4096,
        durable_through_operation_id: 0,
    };
    let completion_offset = DVM_BLOCK_COMPLETION_RING_OFFSET as usize;
    aperture_bytes(&mut aperture)[completion_offset..completion_offset + DVM_BLOCK_RECORD_BYTES]
        .copy_from_slice(&completion.encode());
    let base = aperture.as_mut_ptr().cast::<u8>();
    store_u64(base, REQUEST_CONSUMER_OFFSET, 1, Ordering::Release);
    store_u64(base, COMPLETION_PRODUCER_OFFSET, 1, Ordering::Release);
    assert_eq!(
        state.poll(ticket, &mut [0_u8; 4096]),
        Err(DvmBlockError::Revoked)
    );
    assert!(state.pending[ticket.data_slot as usize].is_none());
    assert!(!state.revoked);
}

#[test]
fn invalid_submission_does_not_consume_request_or_operation_identity() {
    let (_aperture, registers, mut state) = test_state();
    let request_id = state.next_request_id;
    let operation_id = state.next_operation_id;
    assert_eq!(
        state.submit_with_fault_decision(
            DvmBlockOperation::Write,
            u64::MAX,
            &[0_u8; 4096],
            4096,
            false,
            |_| false,
        ),
        Err(DvmBlockError::Invalid)
    );
    assert_eq!(state.next_request_id, request_id);
    assert_eq!(state.next_operation_id, operation_id);
    assert_eq!(state.request_producer, 0);
    assert!(state.pending.iter().all(Option::is_none));
    assert_eq!(registers[IVSHMEM_DOORBELL_OFFSET / 4], 0);
}

#[test]
fn retired_task_disarm_releases_block_waiter_exactly_once() {
    let task_id = u64::MAX - 701;
    assert!(arm_waiter(task_id));
    assert!(disarm_waiter(task_id));
    assert!(!disarm_waiter(task_id));
}

#[test]
#[allow(
    clippy::assertions_on_constants,
    reason = "the mutation witness must compile a reduced capacity and fail at runtime"
)]
fn block_waiter_capacity_covers_every_scheduler_task() {
    assert!(WAITERS_CAPACITY >= crate::multitask::MAX_SCHEDULER_TASKS);
}

#[test]
fn readiness_may_arrive_once_but_cannot_be_withdrawn() {
    let (mut aperture, _registers, mut state) = test_state();
    let base = aperture.as_mut_ptr().cast::<u8>();
    let flags = load_u32(base, FLAGS_OFFSET, Ordering::Acquire);
    unsafe {
        AtomicU32::from_ptr(base.add(FLAGS_OFFSET).cast::<u32>())
            .store(flags & !DVM_BLOCK_FLAG_DVM_READY, Ordering::Release);
    }
    state.ready_observed = false;
    assert_eq!(state.current_header(), Err(DvmBlockError::Busy));
    assert!(!state.revoked);

    unsafe {
        AtomicU32::from_ptr(base.add(FLAGS_OFFSET).cast::<u32>()).store(flags, Ordering::Release);
    }
    assert!(state.current_header().is_ok());
    unsafe {
        AtomicU32::from_ptr(base.add(FLAGS_OFFSET).cast::<u32>())
            .store(flags & !DVM_BLOCK_FLAG_DVM_READY, Ordering::Release);
    }
    assert_eq!(state.current_header(), Err(DvmBlockError::Revoked));
    assert!(state.revoked);
}

#[test]
fn startup_not_ready_is_sleepable_not_a_fault_event() {
    let (mut aperture, _registers, mut state) = test_state();
    let base = aperture.as_mut_ptr().cast::<u8>();
    let flags = load_u32(base, FLAGS_OFFSET, Ordering::Acquire);
    unsafe {
        AtomicU32::from_ptr(base.add(FLAGS_OFFSET).cast::<u32>())
            .store(flags & !DVM_BLOCK_FLAG_DVM_READY, Ordering::Release);
    }
    state.ready_observed = false;

    assert!(!state.completion_or_fault_pending());
    assert!(!state.revoked);

    unsafe {
        AtomicU32::from_ptr(base.add(FLAGS_OFFSET).cast::<u32>()).store(flags, Ordering::Release);
    }
    assert!(state.completion_or_fault_pending());
    assert!(!state.completion_or_fault_pending());
    assert!(state.ready_observed);
}
