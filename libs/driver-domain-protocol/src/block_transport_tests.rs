use super::{
    DVM_BLOCK_APERTURE_BYTES, DVM_BLOCK_FEATURE_DISCARD, DVM_BLOCK_FEATURE_FLUSH,
    DVM_BLOCK_FEATURE_FUA, DVM_BLOCK_FEATURE_WRITE_ZEROES, DVM_BLOCK_FEATURE_WRITEBACK,
    DVM_BLOCK_FLAG_DVM_READY, DVM_BLOCK_FLAG_RUSTOS_READY, DVM_BLOCK_REQUEST_FLAG_FUA,
    DVM_BLOCK_USED_BYTES, DvmBlockCompletion, DvmBlockCompletionStatus, DvmBlockHeader,
    DvmBlockOperation, DvmBlockRequest,
};

fn ready_header() -> DvmBlockHeader {
    let mut header = DvmBlockHeader::new(
        7,
        1024 * 1024,
        4096,
        4096,
        DVM_BLOCK_FEATURE_FLUSH
            | DVM_BLOCK_FEATURE_DISCARD
            | DVM_BLOCK_FEATURE_WRITE_ZEROES
            | DVM_BLOCK_FEATURE_FUA
            | DVM_BLOCK_FEATURE_WRITEBACK,
    )
    .with_epoch_signature([0x5a; 64]);
    header.flags |= DVM_BLOCK_FLAG_RUSTOS_READY | DVM_BLOCK_FLAG_DVM_READY;
    header
}

#[test]
fn block_header_is_fixed_bounded_and_reserved_zero() {
    let header = ready_header();
    assert!(DVM_BLOCK_APERTURE_BYTES.is_power_of_two());
    const {
        assert!(DVM_BLOCK_USED_BYTES <= DVM_BLOCK_APERTURE_BYTES);
    }
    assert!(header.is_valid());
    assert_eq!(DvmBlockHeader::decode(&header.encode()), Some(header));

    let mut unknown_feature = header;
    unknown_feature.features |= 1 << 63;
    assert!(!unknown_feature.is_valid());

    let mut overrun = header;
    overrun.request_producer = 65;
    assert!(!overrun.is_valid());

    let mut reserved = header.encode();
    reserved[168] = 1;
    assert!(DvmBlockHeader::decode(&reserved).is_none());

    let mut unsigned = header;
    unsigned.epoch_signature = [0; 64];
    assert!(!unsigned.is_valid());
}

#[test]
fn block_requests_are_address_free_epoch_bound_and_range_checked() {
    let header = ready_header();
    let request = DvmBlockRequest {
        generation: header.generation,
        request_id: 1,
        operation_id: 9,
        operation: DvmBlockOperation::Write,
        flags: DVM_BLOCK_REQUEST_FLAG_FUA,
        data_slot: 3,
        sector: 8,
        data_len: 4096,
    };
    assert!(request.is_valid_for(header));
    assert_eq!(DvmBlockRequest::decode(&request.encode()), Some(request));

    let mut stale = request;
    stale.generation -= 1;
    assert!(!stale.is_valid_for(header));

    let mut misaligned = request;
    misaligned.sector = 1;
    assert!(!misaligned.is_valid_for(header));

    let mut outside = request;
    outside.sector = header.capacity_sectors;
    assert!(!outside.is_valid_for(header));

    let flush = DvmBlockRequest {
        generation: header.generation,
        request_id: 2,
        operation_id: 10,
        operation: DvmBlockOperation::Flush,
        flags: 0,
        data_slot: 7,
        sector: 0,
        data_len: 0,
    };
    assert!(flush.is_valid_for(header));
}

#[test]
fn block_completion_binds_request_and_explicit_durability() {
    let header = ready_header();
    let write = DvmBlockRequest {
        generation: header.generation,
        request_id: 3,
        operation_id: 11,
        operation: DvmBlockOperation::Write,
        flags: DVM_BLOCK_REQUEST_FLAG_FUA,
        data_slot: 4,
        sector: 16,
        data_len: 4096,
    };
    let completion = DvmBlockCompletion {
        generation: header.generation,
        request_id: write.request_id,
        operation_id: write.operation_id,
        status: DvmBlockCompletionStatus::Success,
        data_slot: write.data_slot,
        completed_bytes: write.data_len,
        durable_through_operation_id: write.operation_id,
    };
    assert!(completion.is_valid_for(header, write));
    assert_eq!(
        DvmBlockCompletion::decode(&completion.encode()),
        Some(completion)
    );

    let mut fabricated_stability = completion;
    fabricated_stability.durable_through_operation_id = 0;
    assert!(!fabricated_stability.is_valid_for(header, write));

    let mut foreign = completion;
    foreign.request_id += 1;
    assert!(!foreign.is_valid_for(header, write));
}
