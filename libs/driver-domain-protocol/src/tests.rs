use super::{
    DVM_DISPLAY_FLAG_HOST_ARMED, DVM_DISPLAY_FLAG_READY, DVM_DISPLAY_HEADER_BYTES,
    DVM_DISPLAY_INVITATION_GENERATION_OFFSET, DVM_DISPLAY_READY_ACK_GENERATION_OFFSET,
    DVM_GPU_BLEND_REPLACE, DVM_GPU_BLEND_SOURCE_OVER, DVM_GPU_COMMAND_FLAG_CLIP_OUTPUT,
    DVM_GPU_FIXED_ONE, DVM_GPU_NO_OUTPUT, DVM_GPU_NO_SOURCE, DVM_GPU_PIXEL_FORMAT_BGRA8888,
    DVM_GPU_RENDER_FLAG_PRESENT_ON_COMPLETE, DVM_GPU_SOURCE_REQUIRED_FLAGS,
    DVM_INPUT_RING_APERTURE_BYTES, DVM_INPUT_RING_CONSUMER_OFFSET,
    DVM_INPUT_RING_CONSUMER_WAKE_GENERATION_OFFSET, DVM_INPUT_RING_FLAG_POLICY_CONSUMER_READY,
    DVM_INPUT_RING_FLAG_RUSTOS_READY, DVM_INPUT_RING_PRODUCER_OFFSET, DVM_NET_APERTURE_BYTES,
    DVM_NET_FLAG_DVM_READY, DVM_NET_MIN_REGION_BYTES, DvmDisplayDamage, DvmDisplayHeader,
    DvmGpuPipelineState, DvmGpuPresentCompletion, DvmGpuPrimeCompletion,
    DvmGpuPrimeCompletionStatus, DvmGpuRenderBatchHeader, DvmGpuRenderCommand,
    DvmGpuRenderCommandKind, DvmGpuRenderCompletion, DvmGpuRenderCompletionStatus,
    DvmGpuRenderSource, DvmGpuTimeline, DvmGpuTimelineError, DvmGuiSurfaceMessage,
    DvmInputRingHeader, DvmNetHeader, ETHERTYPE_ARP, ETHERTYPE_IPV4, EthernetFrameError,
    RUSTOS_INPUT_KIND_KEY, RUSTOS_INPUT_KIND_POINTER_POSITION, RUSTOS_INPUT_VERSION,
    RustosInputFrame, dvm_gpu_render_batch_is_valid, validate_dvm_ethernet_frame,
};

#[test]
fn round_trip_is_fixed_width_and_validated() {
    let header = DvmDisplayHeader::new(8 * 1024 * 1024, 1600, 900, 1);
    assert!(header.is_valid());
    assert_eq!(header.encode().len(), DvmDisplayHeader::encoded_len());
    assert_eq!(DvmDisplayHeader::decode(&header.encode()), Some(header));
}

#[test]
fn display_damage_is_explicit_and_bounds_checked() {
    let header = DvmDisplayHeader::new(8 * 1024 * 1024, 1600, 900, 2);
    let full = DvmDisplayDamage::full();
    assert_eq!(DvmDisplayDamage::decode(&full.encode()), full);
    assert!(full.is_valid_for(header));
    assert!(DvmDisplayDamage::rect(1599, 899, 1, 1).is_valid_for(header));
    assert!(!DvmDisplayDamage::rect(1599, 899, 2, 1).is_valid_for(header));
    assert!(!DvmDisplayDamage::rect(0, 0, 0, 1).is_valid_for(header));
}

#[test]
fn gui_surface_control_is_fixed_and_capability_bounded() {
    let header = DvmDisplayHeader::new(8 * 1024 * 1024, 1600, 900, 2);
    let present = DvmGuiSurfaceMessage::present(2, 7, DvmDisplayDamage::rect(1599, 899, 1, 1));
    assert_eq!(present.encode().len(), DvmGuiSurfaceMessage::encoded_len());
    assert_eq!(
        DvmGuiSurfaceMessage::decode(&present.encode()),
        Some(present)
    );
    assert!(present.is_valid_for(header));
    assert!(DvmGuiSurfaceMessage::release(0, 7).is_valid_for(header));
    assert!(DvmGuiSurfaceMessage::focus(1).is_valid_for(header));
    assert!(
        !DvmGuiSurfaceMessage::present(3, 7, DvmDisplayDamage::full()).is_valid_for(header)
    );
    assert!(
        !DvmGuiSurfaceMessage::present(0, 0, DvmDisplayDamage::full()).is_valid_for(header)
    );
}

#[test]
fn gui_surface_pool_is_fixed_three_slot_geometry() {
    let header = super::DvmGuiSurfacePoolHeader::new(32 * 1024 * 1024, 1600, 900);
    assert!(header.is_valid());
    assert_eq!(
        super::DvmGuiSurfacePoolHeader::decode(&header.encode()),
        Some(header)
    );
    assert_eq!(header.slot_offset(0), Some(4096));
    assert_eq!(header.slot_offset(2), Some(4096 + header.slot_bytes * 2));
    assert_eq!(header.slot_offset(3), None);
    assert_eq!(header.slot_bytes % 4096, 0);
    assert_eq!(header.slot_offset(1).unwrap() % 4096, 0);
    assert!(
        header.slot_offset(1).unwrap() >= header.slot_offset(0).unwrap() + header.slot_bytes
    );

    let mut encoded = header.encode();
    encoded[44..48].copy_from_slice(&2_u32.to_le_bytes());
    assert!(super::DvmGuiSurfacePoolHeader::decode(&encoded).is_none());
}

fn gpu_batch(submit_value: u64) -> DvmGpuRenderBatchHeader {
    DvmGpuRenderBatchHeader {
        command_count: 3,
        context_id: 7,
        context_epoch: 11,
        submit_value,
        acquire_value: 1,
        budget_us: 16_000,
        source_count: 1,
        flags: DVM_GPU_RENDER_FLAG_PRESENT_ON_COMPLETE,
    }
}

fn prime_timeline(timeline: &mut DvmGpuTimeline, epoch: u32) {
    timeline.begin_prime(9).unwrap();
    let completion = DvmGpuPrimeCompletion {
        context_id: 7,
        context_epoch: epoch,
        status: DvmGpuPrimeCompletionStatus::Ready,
        submit_flags: super::DVM_GPU_ATLAS_SUBMIT_FLAG_STAGED_COPY,
        fence_value: 9,
        duration_ns: 400_000,
    };
    assert_eq!(
        DvmGpuPrimeCompletion::decode(&completion.encode()),
        Some(completion)
    );
    let direct = DvmGpuPrimeCompletion {
        submit_flags: super::DVM_GPU_ATLAS_SUBMIT_FLAG_DIRECT_DMABUF,
        ..completion
    };
    assert_eq!(
        DvmGpuPrimeCompletion::decode(&direct.encode()),
        Some(direct)
    );
    let ambiguous = DvmGpuPrimeCompletion {
        submit_flags: super::DVM_GPU_ATLAS_SUBMIT_KNOWN_FLAGS,
        ..completion
    };
    assert!(!ambiguous.is_valid());
    let mut legacy = completion.encode();
    legacy[8..12].copy_from_slice(&super::DVM_GPU_RENDER_VERSION.to_le_bytes());
    assert_eq!(DvmGpuPrimeCompletion::decode(&legacy), None);
    timeline.complete_prime(completion).unwrap();
    timeline.signal_acquire(1).unwrap();
}

fn gpu_completion(
    submit_value: u64,
    status: DvmGpuRenderCompletionStatus,
) -> DvmGpuRenderCompletion {
    let completed = status == DvmGpuRenderCompletionStatus::Completed;
    DvmGpuRenderCompletion {
        context_id: 7,
        context_epoch: 11,
        status,
        output_index: if completed { 0 } else { DVM_GPU_NO_OUTPUT },
        submit_value,
        completion_value: submit_value,
        render_time_ns: if completed { 900_000 } else { 0 },
        release_value: submit_value,
    }
}

fn gpu_source(token: u64, binding_slot: u32) -> DvmGpuRenderSource {
    DvmGpuRenderSource {
        token,
        generation: 2,
        acquire_value: 1,
        width: 64,
        height: 64,
        stride_bytes: 256,
        pixel_format: DVM_GPU_PIXEL_FORMAT_BGRA8888,
        flags: DVM_GPU_SOURCE_REQUIRED_FLAGS,
        binding_slot,
        content_epoch: 4,
    }
}

fn gpu_clear() -> DvmGpuRenderCommand {
    DvmGpuRenderCommand {
        kind: DvmGpuRenderCommandKind::Clear,
        flags: 0,
        source_index: DVM_GPU_NO_SOURCE,
        blend_mode: DVM_GPU_BLEND_REPLACE,
        destination_x: 0,
        destination_y: 0,
        destination_width: 0,
        destination_height: 0,
        source_u: 0,
        source_v: 0,
        source_width: 0,
        source_height: 0,
        rgba: 0xff10_1010,
        depth: 0,
        rotation: 0,
        tilt_x: 0,
        tilt_y: 0,
        perspective: 0,
    }
}

fn gpu_textured(source_index: u32) -> DvmGpuRenderCommand {
    DvmGpuRenderCommand {
        kind: DvmGpuRenderCommandKind::TexturedQuad,
        flags: DVM_GPU_COMMAND_FLAG_CLIP_OUTPUT,
        source_index,
        blend_mode: DVM_GPU_BLEND_SOURCE_OVER,
        destination_x: 8,
        destination_y: 8,
        destination_width: 48,
        destination_height: 48,
        source_u: 0,
        source_v: 0,
        source_width: u16::MAX,
        source_height: u16::MAX,
        rgba: u32::MAX,
        depth: 0,
        rotation: 0,
        tilt_x: 0,
        tilt_y: 0,
        perspective: 0,
    }
}

#[test]
fn gpu_render_contract_is_fixed_bounded_and_address_free() {
    let header = gpu_batch(1);
    assert_eq!(
        DvmGpuRenderBatchHeader::decode(&header.encode()),
        Some(header)
    );
    assert_eq!(header.encoded_batch_len(), Some(64 + 64 + 3 * 64));

    let source = DvmGpuRenderSource {
        token: 0x100,
        generation: 2,
        acquire_value: 1,
        width: 1600,
        height: 900,
        stride_bytes: 6400,
        pixel_format: DVM_GPU_PIXEL_FORMAT_BGRA8888,
        flags: DVM_GPU_SOURCE_REQUIRED_FLAGS,
        binding_slot: 0,
        content_epoch: 4,
    };
    assert_eq!(DvmGpuRenderSource::decode(&source.encode()), Some(source));
    let mut writable = source;
    writable.flags = 0;
    assert!(!writable.is_valid());

    let mut oversized = header;
    oversized.command_count = super::DVM_GPU_RENDER_MAX_COMMANDS + 1;
    assert!(!oversized.is_valid());
    let mut late = header;
    late.acquire_value = 2;
    assert!(!late.is_valid());

    let mut bounded_jitter = header;
    bounded_jitter.budget_us = super::DVM_GPU_RENDER_MAX_BUDGET_US;
    assert!(bounded_jitter.is_valid());
    assert!(bounded_jitter.budget_us > super::DVM_GPU_FRAME_TARGET_US);
    bounded_jitter.budget_us += 1;
    assert!(!bounded_jitter.is_valid());
}

#[test]
fn gpu_batch_admission_binds_one_atlas_to_a_physical_pool_slot() {
    let mut header = gpu_batch(1);
    header.command_count = 2;
    let source = gpu_source(0x100, 2);
    let commands = [gpu_clear(), gpu_textured(0)];
    assert!(dvm_gpu_render_batch_is_valid(
        header,
        &[source],
        &commands,
        128,
        128
    ));

    let rebound = gpu_source(0x101, 1);
    header.source_count = 2;
    header.command_count = 3;
    assert!(!dvm_gpu_render_batch_is_valid(
        header,
        &[source, rebound],
        &[gpu_clear(), gpu_textured(0), gpu_textured(1)],
        128,
        128
    ));

    let mut outside_pool = source;
    outside_pool.binding_slot = super::DVM_GPU_RENDER_MAX_IN_FLIGHT;
    assert!(!outside_pool.is_valid());

    let mut late = source;
    late.acquire_value = 2;
    header.source_count = 1;
    header.command_count = 2;
    assert!(!dvm_gpu_render_batch_is_valid(
        header,
        &[late],
        &commands,
        128,
        128
    ));

    let mut oversized = source;
    oversized.width = super::DVM_GPU_RENDER_MAX_DIMENSION;
    oversized.height = super::DVM_GPU_RENDER_MAX_DIMENSION;
    oversized.stride_bytes = oversized.width * 4 + 4;
    assert!(!oversized.is_valid());
}

#[test]
fn gpu_render_commands_reject_unknown_or_out_of_bounds_work() {
    let clear = gpu_clear();
    assert!(clear.is_valid_for(1600, 900, 1));
    assert_eq!(DvmGpuRenderCommand::decode(&clear.encode()), Some(clear));

    let mut textured = gpu_textured(0);
    textured.destination_x = 100;
    textured.destination_y = 80;
    textured.destination_width = 800;
    textured.destination_height = 600;
    textured.depth = DVM_GPU_FIXED_ONE / 4;
    textured.rotation = DVM_GPU_FIXED_ONE / 12;
    textured.tilt_x = DVM_GPU_FIXED_ONE / 8;
    textured.perspective = DVM_GPU_FIXED_ONE / 16;
    assert!(textured.is_valid_for(1600, 900, 1));
    assert_eq!(
        DvmGpuRenderCommand::decode(&textured.encode()),
        Some(textured)
    );

    let mut outside = textured;
    outside.destination_width = 1600;
    assert!(!outside.is_valid_for(1600, 900, 1));
    let mut foreign_source = textured;
    foreign_source.source_index = 1;
    assert!(!foreign_source.is_valid_for(1600, 900, 1));
    let mut transform_overflow = textured;
    transform_overflow.perspective = super::DVM_GPU_TRANSFORM_LIMIT + 1;
    assert!(!transform_overflow.is_valid_for(1600, 900, 1));
}

#[test]
fn gpu_atlas_transport_separates_immutable_sources_from_completions() {
    let region_bytes = 128_u64 * 1024 * 1024;
    let gui_pool = super::DvmGuiSurfacePoolHeader::new(region_bytes, 1600, 900);
    assert!(gui_pool.is_valid());
    let atlas = super::DvmGpuAtlasPoolHeader::new(region_bytes, gui_pool, 2048, 2048)
        .expect("atlas layout fits fixed backing");
    assert_eq!(
        super::DvmGpuAtlasPoolHeader::decode(&atlas.encode()),
        Some(atlas)
    );
    let gui_end = gui_pool.slot_offset(2).unwrap() + gui_pool.slot_bytes;
    assert!(atlas.command_offset >= gui_end);
    assert!(atlas.atlas_slot_offset(2).unwrap() + atlas.atlas_slot_bytes <= region_bytes);
    assert_eq!(atlas.command_slot_offset(3), None);
    assert_eq!(atlas.atlas_slot_offset(3), None);
    assert!(
        super::DVM_GPU_ATLAS_PRIME_COMPLETION_OFFSET
            >= super::DVM_GPU_ATLAS_COMPLETION_POOL_OFFSET
                + super::DVM_GPU_ATLAS_COMPLETION_SLOT_BYTES
                    * super::DVM_GPU_RENDER_MAX_IN_FLIGHT as usize
    );
    const {
        assert!(
            super::DVM_GPU_ATLAS_PRIME_COMPLETION_OFFSET
                + super::DVM_GPU_PRIME_COMPLETION_BYTES
                <= super::DVM_GPU_ATLAS_HOST_INVITATION_OFFSET
        );
    }
    assert!(
        super::DVM_GPU_ATLAS_PRIME_FENCE_OFFSET + core::mem::size_of::<u64>()
            <= super::DVM_GUI_SURFACE_POOL_HEADER_BYTES as usize
    );

    let header = gpu_batch(1);
    let source = gpu_source(0x100, 2);
    let full_damage = super::DvmGpuAtlasDamage {
        x: 0,
        y: 0,
        width: atlas.atlas_width,
        height: atlas.atlas_height,
    };
    assert!(super::dvm_gpu_atlas_damage_is_valid(
        &[full_damage],
        atlas.atlas_width,
        atlas.atlas_height,
        true,
    ));
    assert!(!super::dvm_gpu_atlas_damage_is_valid(
        &[
            super::DvmGpuAtlasDamage {
                x: 0,
                y: 0,
                width: 64,
                height: 64,
            },
            super::DvmGpuAtlasDamage {
                x: 32,
                y: 32,
                width: 64,
                height: 64,
            },
        ],
        atlas.atlas_width,
        atlas.atlas_height,
        false,
    ));
    let submit = super::DvmGpuAtlasSubmit {
        slot: 2,
        batch_bytes: header.encoded_batch_len().unwrap() as u32,
        generation: source.generation,
        sequence: 1,
        context_epoch: header.context_epoch,
        flags: super::DVM_GPU_ATLAS_SUBMIT_FLAG_STAGED_COPY,
        content_epoch: source.content_epoch,
        damage_count: 1,
    };
    assert_eq!(
        super::DvmGpuAtlasSubmit::decode(&submit.encode()),
        Some(submit)
    );
    assert!(submit.matches_batch(header, source));
    let mut ambiguous = submit;
    ambiguous.flags = super::DVM_GPU_ATLAS_SUBMIT_KNOWN_FLAGS;
    assert!(!ambiguous.is_valid());

    let render = gpu_completion(1, DvmGpuRenderCompletionStatus::Completed);
    let present = DvmGpuPresentCompletion {
        context_id: 7,
        context_epoch: 11,
        output_index: 0,
        submit_value: 1,
        present_value: 1,
        previous_submit_value: 0,
        present_time_ns: 1,
    };
    let completion = super::DvmGpuAtlasCompletion {
        slot: 2,
        flags: super::DVM_GPU_ATLAS_COMPLETION_REQUIRED_FLAGS,
        generation: source.generation,
        sequence: 1,
        render,
        present,
    };
    assert_eq!(
        super::DvmGpuAtlasCompletion::decode(&completion.encode()),
        Some(completion)
    );
    let mut accepted_only = completion;
    accepted_only.flags = super::DVM_GPU_ATLAS_COMPLETION_FLAG_GPU_DONE;
    assert!(!accepted_only.is_valid());
}

#[test]
fn gpu_timeline_is_monotonic_bounded_and_reset_by_epoch() {
    let mut timeline = DvmGpuTimeline::new(7, 11).expect("valid context");
    assert_eq!(
        timeline.admit(gpu_batch(1)),
        Err(DvmGpuTimelineError::PipelineNotReady)
    );
    prime_timeline(&mut timeline, 11);
    timeline.admit(gpu_batch(1)).unwrap();
    timeline.admit(gpu_batch(2)).unwrap();
    timeline.admit(gpu_batch(3)).unwrap();
    assert_eq!(timeline.in_flight(), 3);
    assert_eq!(
        timeline.admit(gpu_batch(4)),
        Err(DvmGpuTimelineError::QueueFull)
    );

    let completion = gpu_completion(1, DvmGpuRenderCompletionStatus::Completed);
    assert_eq!(
        DvmGpuRenderCompletion::decode(&completion.encode()),
        Some(completion)
    );
    timeline.complete(completion).unwrap();
    assert_eq!(timeline.completed_value(), 1);
    assert_eq!(timeline.in_flight(), 2);

    timeline
        .complete(gpu_completion(2, DvmGpuRenderCompletionStatus::TimedOut))
        .unwrap();
    assert!(!timeline.is_active());
    assert_eq!(
        timeline.admit(gpu_batch(4)),
        Err(DvmGpuTimelineError::ContextInactive)
    );
    timeline.reset(12).unwrap();
    assert!(timeline.is_active());
    assert_eq!(timeline.pipeline_state(), DvmGpuPipelineState::Unprimed);
    let mut fresh = gpu_batch(1);
    fresh.context_epoch = 12;
    assert_eq!(
        timeline.admit(fresh),
        Err(DvmGpuTimelineError::PipelineNotReady)
    );
    prime_timeline(&mut timeline, 12);
    timeline.admit(fresh).unwrap();

    let mut stale = gpu_completion(1, DvmGpuRenderCompletionStatus::Completed);
    stale.context_epoch = 11;
    assert_eq!(
        timeline.complete(stale),
        Err(DvmGpuTimelineError::ContextMismatch)
    );
}

#[test]
fn cold_submissions_get_the_raised_render_budget_and_steady_state_does_not() {
    use super::{
        DVM_GPU_COLD_RENDER_BUDGET_US, DVM_GPU_COLD_SUBMIT_COUNT, dvm_gpu_effective_budget_us,
    };
    assert_eq!(dvm_gpu_effective_budget_us(16_000, 0), 16_000);
    assert_eq!(
        dvm_gpu_effective_budget_us(16_000, 1),
        DVM_GPU_COLD_RENDER_BUDGET_US
    );
    assert_eq!(
        dvm_gpu_effective_budget_us(16_000, DVM_GPU_COLD_SUBMIT_COUNT),
        DVM_GPU_COLD_RENDER_BUDGET_US
    );
    assert_eq!(
        dvm_gpu_effective_budget_us(16_000, DVM_GPU_COLD_SUBMIT_COUNT + 1),
        16_000
    );
    // The allowance never lowers a larger request.
    assert_eq!(
        dvm_gpu_effective_budget_us(DVM_GPU_COLD_RENDER_BUDGET_US + 1, 1),
        DVM_GPU_COLD_RENDER_BUDGET_US + 1
    );

    let mut timeline = DvmGpuTimeline::new(7, 11).unwrap();
    prime_timeline(&mut timeline, 11);
    timeline.signal_acquire(1).unwrap();
    for submit in 1..=3 {
        timeline.admit(gpu_batch(submit)).unwrap();
    }
    // 200 ms is legitimate for the cold submission, not for steady state.
    let mut cold = gpu_completion(1, DvmGpuRenderCompletionStatus::Completed);
    cold.render_time_ns = 200_000_000;
    timeline.complete(cold).unwrap();
    let mut second = gpu_completion(2, DvmGpuRenderCompletionStatus::Completed);
    second.output_index = 1;
    second.render_time_ns = u64::from(DVM_GPU_COLD_RENDER_BUDGET_US) * 1_000;
    timeline.complete(second).unwrap();
    let mut steady = gpu_completion(3, DvmGpuRenderCompletionStatus::Completed);
    steady.output_index = 2;
    steady.render_time_ns = 200_000_000;
    assert_eq!(
        timeline.complete(steady),
        Err(DvmGpuTimelineError::DeadlineExceeded)
    );
    assert!(!timeline.is_active());
}

#[test]
fn gpu_timeline_requires_prime_and_acquire_and_retires_outputs_in_fence_order() {
    let mut timeline = DvmGpuTimeline::new(7, 11).unwrap();
    assert_eq!(
        timeline.reset(12),
        Err(DvmGpuTimelineError::ContextInactive)
    );
    timeline.begin_prime(4).unwrap();
    let too_slow_prime = DvmGpuPrimeCompletion {
        context_id: 7,
        context_epoch: 11,
        status: DvmGpuPrimeCompletionStatus::Ready,
        submit_flags: super::DVM_GPU_ATLAS_SUBMIT_FLAG_STAGED_COPY,
        fence_value: 4,
        duration_ns: u64::from(super::DVM_GPU_PIPELINE_PRIME_MAX_US) * 1_000 + 1,
    };
    assert!(!too_slow_prime.is_valid());
    assert_eq!(
        timeline.complete_prime(too_slow_prime),
        Err(DvmGpuTimelineError::InvalidContract)
    );
    timeline.timeout_prime().unwrap();
    timeline.reset(12).unwrap();
    prime_timeline(&mut timeline, 12);

    let mut first = gpu_batch(1);
    first.context_epoch = 12;
    let mut second = gpu_batch(2);
    second.context_epoch = 12;
    timeline.admit(first).unwrap();
    timeline.admit(second).unwrap();

    let mut first_done = gpu_completion(1, DvmGpuRenderCompletionStatus::Completed);
    first_done.context_epoch = 12;
    let mut second_done = gpu_completion(2, DvmGpuRenderCompletionStatus::Completed);
    second_done.context_epoch = 12;
    second_done.output_index = 1;
    timeline.complete(first_done).unwrap();
    timeline.complete(second_done).unwrap();

    let first_present = DvmGpuPresentCompletion {
        context_id: 7,
        context_epoch: 12,
        output_index: 0,
        submit_value: 1,
        present_value: 1,
        previous_submit_value: 0,
        present_time_ns: 1,
    };
    let second_present = DvmGpuPresentCompletion {
        context_id: 7,
        context_epoch: 12,
        output_index: 1,
        submit_value: 2,
        present_value: 2,
        previous_submit_value: 1,
        present_time_ns: 2,
    };
    assert_eq!(
        timeline.present(second_present),
        Err(DvmGpuTimelineError::TimelineOrder)
    );
    assert_eq!(
        DvmGpuPresentCompletion::decode(&first_present.encode()),
        Some(first_present)
    );
    timeline.present(first_present).unwrap();
    timeline.present(second_present).unwrap();
    assert_eq!(timeline.presented_value(), 2);

    let mut third = gpu_batch(3);
    third.context_epoch = 12;
    timeline.admit(third).unwrap();
    let mut output_reuse = gpu_completion(3, DvmGpuRenderCompletionStatus::Completed);
    output_reuse.context_epoch = 12;
    output_reuse.output_index = 1;
    assert_eq!(
        timeline.complete(output_reuse),
        Err(DvmGpuTimelineError::OutputBusy)
    );
    assert!(!timeline.is_active());

    let mut deadline = DvmGpuTimeline::new(7, 11).unwrap();
    prime_timeline(&mut deadline, 11);
    for submit in 1..=3 {
        deadline.admit(gpu_batch(submit)).unwrap();
    }
    for (submit, output) in [(1, 0), (2, 1)] {
        let mut cold = gpu_completion(submit, DvmGpuRenderCompletionStatus::Completed);
        cold.output_index = output;
        deadline.complete(cold).unwrap();
    }
    let mut late = gpu_completion(3, DvmGpuRenderCompletionStatus::Completed);
    late.output_index = 2;
    late.render_time_ns = 16_000_001;
    assert_eq!(
        deadline.complete(late),
        Err(DvmGpuTimelineError::DeadlineExceeded)
    );
    assert!(!deadline.is_active());
}

#[test]
fn rejects_unready_or_truncated_regions() {
    let mut header = DvmDisplayHeader::new(8 * 1024 * 1024, 1600, 900, 1);
    header.flags = 0;
    assert!(!header.is_valid());

    header.flags = DVM_DISPLAY_FLAG_READY | DVM_DISPLAY_FLAG_HOST_ARMED;
    assert!(header.is_valid());
    header.flags |= 1 << 31;
    assert!(!header.is_valid());

    let mut encoded = DvmDisplayHeader::new(8 * 1024 * 1024, 1600, 900, 1).encode();
    encoded[12..16].copy_from_slice(&(DVM_DISPLAY_HEADER_BYTES - 1).to_le_bytes());
    assert!(DvmDisplayHeader::decode(&encoded).is_none());
}

#[test]
fn display_readiness_generations_stay_inside_the_fixed_header() {
    assert!(
        DVM_DISPLAY_INVITATION_GENERATION_OFFSET + core::mem::size_of::<u64>()
            <= DVM_DISPLAY_HEADER_BYTES as usize
    );
    assert!(
        DVM_DISPLAY_READY_ACK_GENERATION_OFFSET + core::mem::size_of::<u64>()
            <= DVM_DISPLAY_HEADER_BYTES as usize
    );
}

#[test]
fn net_contract_has_two_bounded_fixed_rings() {
    let header = DvmNetHeader::new(DVM_NET_MIN_REGION_BYTES, 1);
    assert!(header.is_valid());
    assert_eq!(DvmNetHeader::decode(&header.encode()), Some(header));
    assert_eq!(
        header.rx_ring_offset(),
        Some(DVM_NET_MIN_REGION_BYTES / 2 + 2048)
    );
    assert!(!header.dvm_ready());
    let ready = DvmNetHeader {
        flags: header.flags | DVM_NET_FLAG_DVM_READY,
        ..header
    };
    assert!(ready.is_valid());
    assert!(ready.dvm_ready());
    const {
        assert!(DVM_NET_APERTURE_BYTES >= DVM_NET_MIN_REGION_BYTES);
    }
    assert!(DVM_NET_APERTURE_BYTES.is_power_of_two());
}

fn ipv4_test_frame(fragment: u16) -> [u8; 34] {
    let mut frame = [0_u8; 34];
    frame[12..14].copy_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
    let ip = &mut frame[14..];
    ip[0] = 0x45;
    ip[2..4].copy_from_slice(&20_u16.to_be_bytes());
    ip[6..8].copy_from_slice(&fragment.to_be_bytes());
    ip[8] = 64;
    ip[9] = 1;
    ip[12..16].copy_from_slice(&[10, 0, 0, 1]);
    ip[16..20].copy_from_slice(&[10, 0, 0, 2]);
    let mut sum = 0_u32;
    for chunk in ip.chunks_exact(2) {
        sum += u32::from(u16::from_be_bytes([chunk[0], chunk[1]]));
        sum = (sum & 0xffff) + (sum >> 16);
    }
    ip[10..12].copy_from_slice(&(!(sum as u16)).to_be_bytes());
    frame
}

#[test]
fn dvm_ethernet_payload_rejects_bad_checksum_and_fragments() {
    let frame = ipv4_test_frame(0);
    assert_eq!(validate_dvm_ethernet_frame(&frame), Ok(()));

    let mut corrupt = frame;
    corrupt[22] ^= 1;
    assert_eq!(
        validate_dvm_ethernet_frame(&corrupt),
        Err(EthernetFrameError::InvalidIpv4Checksum)
    );

    assert_eq!(
        validate_dvm_ethernet_frame(&ipv4_test_frame(0x2000)),
        Err(EthernetFrameError::FragmentedIpv4)
    );
}

#[test]
fn dvm_ethernet_payload_accepts_only_bounded_ipv4_or_arp() {
    let mut arp = [0_u8; 42];
    arp[12..14].copy_from_slice(&ETHERTYPE_ARP.to_be_bytes());
    arp[14..16].copy_from_slice(&1_u16.to_be_bytes());
    arp[16..18].copy_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
    arp[18] = 6;
    arp[19] = 4;
    arp[20..22].copy_from_slice(&1_u16.to_be_bytes());
    assert_eq!(validate_dvm_ethernet_frame(&arp), Ok(()));

    arp[12..14].copy_from_slice(&0x86dd_u16.to_be_bytes());
    assert_eq!(
        validate_dvm_ethernet_frame(&arp),
        Err(EthernetFrameError::UnsupportedEtherType)
    );
    assert_eq!(
        validate_dvm_ethernet_frame(&[0_u8; 13]),
        Err(EthernetFrameError::Length)
    );
}

#[test]
fn input_ring_has_separate_cursor_cache_lines_and_rejects_tampering() {
    let header = DvmInputRingHeader::new(DVM_INPUT_RING_APERTURE_BYTES, 9);
    assert!(header.is_valid());
    assert_eq!(DvmInputRingHeader::decode(&header.encode()), Some(header));
    assert_eq!(
        DVM_INPUT_RING_PRODUCER_OFFSET % core::mem::align_of::<u64>(),
        0
    );
    assert_eq!(
        DVM_INPUT_RING_CONSUMER_OFFSET % core::mem::align_of::<u64>(),
        0
    );
    assert_eq!(
        DVM_INPUT_RING_CONSUMER_WAKE_GENERATION_OFFSET % core::mem::align_of::<u64>(),
        0
    );
    const {
        assert!(DVM_INPUT_RING_PRODUCER_OFFSET + 8 <= DVM_INPUT_RING_CONSUMER_OFFSET);
        assert!(DVM_INPUT_RING_CONSUMER_OFFSET - DVM_INPUT_RING_PRODUCER_OFFSET >= 64);
        assert!(
            DVM_INPUT_RING_CONSUMER_OFFSET + 8
                <= DVM_INPUT_RING_CONSUMER_WAKE_GENERATION_OFFSET
        );
    }

    let armed = DvmInputRingHeader {
        consumer_wake_generation: 17,
        ..header
    };
    assert_eq!(DvmInputRingHeader::decode(&armed.encode()), Some(armed));

    let mut reserved = header.encode();
    reserved[72] = 1;
    assert!(DvmInputRingHeader::decode(&reserved).is_none());

    let mut overrun = header.encode();
    overrun[DVM_INPUT_RING_PRODUCER_OFFSET..DVM_INPUT_RING_PRODUCER_OFFSET + 8]
        .copy_from_slice(&2049_u64.to_le_bytes());
    assert!(DvmInputRingHeader::decode(&overrun).is_none());

    let admitted = DvmInputRingHeader {
        flags: header.flags
            | DVM_INPUT_RING_FLAG_RUSTOS_READY
            | DVM_INPUT_RING_FLAG_POLICY_CONSUMER_READY,
        ..header
    };
    assert_eq!(
        DvmInputRingHeader::decode(&admitted.encode()),
        Some(admitted)
    );
}

#[test]
fn input_frame_requires_nonzero_provenance_bounds_and_stable_checksum() {
    assert!(RustosInputFrame::linux_evdev_key(0, 3, 30, 1).is_err());
    assert!(RustosInputFrame::linux_evdev_key(7, 0, 30, 1).is_err());
    assert!(RustosInputFrame::linux_evdev_key(7, 3, 0, 1).is_err());
    assert!(RustosInputFrame::linux_evdev_key(7, 3, 30, 3).is_err());

    let frame =
        RustosInputFrame::linux_evdev_key(7, 3, 30, 1).expect("bounded key frame admitted");
    let bytes = frame.as_bytes();
    assert_eq!(bytes[4], RUSTOS_INPUT_VERSION);
    assert_eq!(bytes[5], RUSTOS_INPUT_KIND_KEY);
    assert_eq!(&bytes[8..12], &7_u32.to_be_bytes());
    assert_eq!(&bytes[12..16], &3_u32.to_be_bytes());
    assert_eq!(&bytes[16..18], &30_u16.to_be_bytes());
    assert_eq!(bytes[18], 1);
    assert_eq!(&bytes[28..32], &[0x40, 0xf2, 0x19, 0x2d]);
}

#[test]
fn absolute_pointer_frame_is_bounded_and_keeps_position_semantics() {
    let frame = RustosInputFrame::linux_evdev_pointer_position(7, 1, 800, 450, 0, 0, 0)
        .expect("bounded absolute position");
    let bytes = frame.as_bytes();
    assert_eq!(bytes[4], RUSTOS_INPUT_VERSION);
    assert_eq!(bytes[5], RUSTOS_INPUT_KIND_POINTER_POSITION);
    assert_eq!(u16::from_be_bytes(bytes[16..18].try_into().unwrap()), 800);
    assert_eq!(u16::from_be_bytes(bytes[18..20].try_into().unwrap()), 450);
    assert!(RustosInputFrame::linux_evdev_pointer_position(7, 2, 2560, 450, 0, 0, 0).is_err());
    assert!(RustosInputFrame::linux_evdev_pointer_position(7, 2, 800, 1440, 0, 0, 0).is_err());
    assert!(RustosInputFrame::linux_evdev_pointer_position(7, 3, 2559, 1439, 0, 0, 0).is_ok());
}
