//! GUI-DVM peer readiness, shared control records and epoch revocation.
//!
//! - **Owner:** kernel-io-manager owns fixed control mechanics; the DVM owns
//!   GPU execution and uiserver owns scene policy.
//! - **Boundary:** Invitations, prime proofs and releases are untrusted.
//! - **Lifecycle:** Admit exact prime context/fence, confirm invitation, drain
//!   exact releases, revoke old epoch, then re-prime replacement context.
//! - **Concurrency:** IRQ leaves mark pending; normal consumers drain records.
//! - **Failure:** Malformed/stale records revoke rather than revive authority.
//! - **Forbidden:** No forged frame, stale completion or GPU policy in ring0.
//! - **Evidence:** `dvm-display-ingress` and `gpu-frame-lifecycle`.

use super::*;

/// Handle the DVM's one fixed return record. A guest cannot free a writing
/// slot, invent a generation, or advance the sequence more than once without
/// a host acknowledgement. Unsupported focus changes fail closed because the
/// current single-domain input broker has no multi-domain focus authority.
pub(super) fn drain_dvm_control() {
    if GUI_DVM_OFFLINE_IRQ_PENDING.swap(false, Ordering::AcqRel) {
        GPU_LIFECYCLE.request_drain();
        // ORDERING: drain closes new claims before Release publishes pending
        // reset and peer-unready state.
        GPU_EPOCH_RESET_PENDING.store(true, Ordering::Release);
        GUI_DVM_PEER_READY.store(false, Ordering::Release);
        GUI_DVM_EXPECTED_INVITATION.store(0, Ordering::Release);
        GPU_PRIME_DURATION_NS.store(0, Ordering::Release);
        GPU_SUBMIT_FLAGS.store(0, Ordering::Release);
        GPU_SESSION_SUBMISSIONS.store(0, Ordering::Release);
        // A future relay must not mistake an old confirmation for approval of
        // a reused generation after DVM restart.
        write_u64(DVM_GUI_SURFACE_POOL_READY_CONFIRMATION_OFFSET, 0);
        write_bytes(
            DVM_GPU_ATLAS_PRIME_COMPLETION_OFFSET,
            &[0; DVM_GPU_PRIME_COMPLETION_BYTES],
        );
        if !service_gpu_epoch_reset() {
            return;
        }
    }
    if !GUI_DVM_CONTROL_IRQ_PENDING.swap(false, Ordering::AcqRel) {
        return;
    }
    let expected = GUI_DVM_EXPECTED_INVITATION.load(Ordering::Acquire);
    let ready_ack = read_u64(DVM_GUI_SURFACE_POOL_READY_ACK_OFFSET);
    if expected != 0 && expected == ready_ack {
        let Some(prime) = read_gpu_prime_completion() else {
            revoke_transport("gpu-prime-completion-invalid", ready_ack);
            return;
        };
        GPU_PRIME_DURATION_NS.store(prime.duration_ns, Ordering::Release);
        GPU_SUBMIT_FLAGS.store(prime.submit_flags, Ordering::Release);
        GUI_DVM_PEER_READY.store(true, Ordering::Release);
        write_u64(DVM_GUI_SURFACE_POOL_READY_CONFIRMATION_OFFSET, expected);
        // Log the rebind once per invitation. The confirmation itself is
        // idempotent and re-runs on every control interrupt, which produced 928
        // identical lines in one acceptance run - about 55 KB of debugcon port
        // writes, each one a host exit taken under a global lock with
        // interrupts disabled, on the same transport the acceptance proof reads
        // its per-second windows from. The protocol path below is unchanged.
        let already_confirmed =
            GUI_DVM_LOGGED_INVITATION.swap(expected, Ordering::AcqRel) == expected;
        if !already_confirmed {
            nucleus_core::debug::write_debugcon_only_line(
                alloc::format!(
                    "gui-dvm: peer ready lease rebound invitation={} context_epoch={}",
                    expected,
                    // ORDERING: Peer-ready Release publication precedes this
                    // diagnostic Acquire snapshot; the value is evidence only.
                    GPU_CONTEXT_EPOCH.load(Ordering::Acquire)
                )
                .as_bytes(),
            );
        }
        // The first host present can legitimately precede Linux-DVM boot. In
        // that order its original doorbell is unobservable, so confirmation
        // is also the single replay-safe wakeup that lets the DVM consume the
        // already committed slot. `GUI_DVM_PEER_READY` is set first, therefore
        // `signal_gui_dvm` cannot rewrite the accepted invitation.
        signal_gui_dvm(expected);
    }
    let sequence = read_u64(DVM_GUI_SURFACE_POOL_DVM_SEQUENCE_OFFSET);
    let acknowledged = GUI_DVM_ACKED_CONTROL_SEQUENCE.load(Ordering::Acquire);
    if sequence == 0 || sequence <= acknowledged {
        return;
    }
    let Some(message) = read_dvm_record() else {
        revoke_transport("malformed-dvm-control-record", sequence);
        return;
    };
    let valid_release = matches!(message.kind, DvmGuiSurfaceMessageKind::Release)
        && message.is_valid_for_dimensions(
            POOL_WIDTH.load(Ordering::Acquire),
            POOL_HEIGHT.load(Ordering::Acquire),
        )
        && usize::try_from(message.slot).ok().is_some_and(|slot| {
            slot < GUI_SLOT_COUNT
                && SLOT_STATE[slot].load(Ordering::Acquire) == GUI_SLOT_READY
                && SLOT_GENERATION[slot].load(Ordering::Acquire) == message.generation
        });
    if !valid_release {
        revoke_transport("unauthorized-dvm-slot-release", sequence);
        return;
    }
    let slot = message.slot as usize;
    SLOT_GENERATION[slot].store(0, Ordering::Release);
    SLOT_STATE[slot].store(GUI_SLOT_FREE, Ordering::Release);
    GUI_DVM_ACKED_CONTROL_SEQUENCE.store(sequence, Ordering::Release);
    write_u64(DVM_GUI_SURFACE_POOL_HOST_ACK_OFFSET, sequence);
}

pub(super) fn revoke_transport(reason: &str, sequence: u64) {
    // ORDERING: Release revocation becomes visible before drain/reset work and
    // every peer-ready or acknowledgement field is cleared.
    TRANSPORT_REVOKED.store(true, Ordering::Release);
    GPU_LIFECYCLE.request_drain();
    GPU_EPOCH_RESET_PENDING.store(true, Ordering::Release);
    GPU_SUBMIT_FLAGS.store(0, Ordering::Release);
    GUI_DVM_PEER_READY.store(false, Ordering::Release);
    GUI_DVM_EXPECTED_INVITATION.store(0, Ordering::Release);
    GUI_DVM_ACKED_CONTROL_SEQUENCE.store(sequence, Ordering::Release);
    write_u64(DVM_GUI_SURFACE_POOL_HOST_ACK_OFFSET, sequence);
    let _ = service_gpu_epoch_reset();
    crate::debug::warn!(display, "gui-dvm: transport revoked reason={}", reason);
}

pub(super) fn service_gpu_epoch_reset() -> bool {
    // ORDERING: Acquire observes the complete drain request before trying the
    // zero-claim transition and resetting slot storage.
    if !GPU_EPOCH_RESET_PENDING.load(Ordering::Acquire) {
        return true;
    }
    let Some(retired_epoch) = GPU_LIFECYCLE.finish_drain() else {
        return false;
    };
    reset_gpu_slots();
    let Some(next_epoch) = retired_epoch
        .checked_add(1)
        .and_then(|epoch| u32::try_from(epoch).ok())
        .filter(|epoch| *epoch != 0)
    else {
        // ORDERING: epoch exhaustion publishes permanent revoke before clearing
        // the one-shot reset work item.
        TRANSPORT_REVOKED.store(true, Ordering::Release);
        GPU_EPOCH_RESET_PENDING.store(false, Ordering::Release);
        return false;
    };
    // ORDERING: Release publishes the new epoch before shared invitation and
    // reset-complete publication can make the replacement usable.
    GPU_CONTEXT_EPOCH.store(next_epoch, Ordering::Release);
    write_u32(DVM_GPU_ATLAS_CONTEXT_EPOCH_OFFSET, next_epoch);
    GPU_EPOCH_RESET_PENDING.store(false, Ordering::Release);
    // ORDERING: Acquire prevents reactivation after a concurrent permanent
    // revoke publication.
    if TRANSPORT_REVOKED.load(Ordering::Acquire) {
        return true;
    }
    assert!(
        GPU_LIFECYCLE.activate(u64::from(next_epoch)),
        "gui-dvm lifecycle failed to activate replacement epoch"
    );
    nucleus_core::debug::write_debugcon_only_line(
        alloc::format!(
            "gui-dvm: peer offline lease revoked context_epoch={}",
            next_epoch
        )
        .as_bytes(),
    );
    // Recovery must not depend on unrelated cursor or client damage.
    reinvite_newest_ready_slot();
    true
}

pub(super) fn reset_gpu_slots() {
    assert_eq!(
        GPU_LIFECYCLE.in_flight(),
        0,
        "gui-dvm reset attempted with in-flight transport claims"
    );
    for slot in 0..GPU_SLOT_STATE.len() {
        GPU_SLOT_GENERATION[slot].store(0, Ordering::Release);
        GPU_SLOT_SEQUENCE[slot].store(0, Ordering::Release);
        GPU_SLOT_CONTEXT_ID[slot].store(0, Ordering::Release);
        GPU_SLOT_CONTEXT_EPOCH[slot].store(0, Ordering::Release);
        GPU_SLOT_SUBMIT_VALUE[slot].store(0, Ordering::Release);
        GPU_SLOT_STATE[slot].store(GPU_SLOT_FREE, Ordering::Release);
        if let Some(offset) = dvm_gpu_atlas_invitation_offset(slot as u32) {
            write_u64(offset, 0);
        }
        if let Some(offset) = dvm_gpu_atlas_completion_sequence_offset(slot as u32) {
            write_u64(offset, 0);
        }
        if let Some(offset) = dvm_gpu_atlas_completion_ack_offset(slot as u32) {
            write_u64(offset, 0);
        }
    }
}

pub(super) fn read_gpu_prime_completion() -> Option<DvmGpuPrimeCompletion> {
    let mut bytes = [0_u8; DVM_GPU_PRIME_COMPLETION_BYTES];
    read_bytes(DVM_GPU_ATLAS_PRIME_COMPLETION_OFFSET, &mut bytes)?;
    let completion = DvmGpuPrimeCompletion::decode(&bytes)?;
    (completion.context_id == GPU_CONTEXT_ID
        && completion.context_epoch == GPU_CONTEXT_EPOCH.load(Ordering::Acquire)
        && completion.status == DvmGpuPrimeCompletionStatus::Ready
        && completion.fence_value == GPU_PRIME_FENCE_VALUE)
        .then_some(completion)
}

pub(super) fn signal_gui_dvm(generation: u64) {
    let doorbell = SHARED_DOORBELL_ADDR.load(Ordering::Acquire);
    if doorbell == 0 || generation == 0 || generation & 1 != 0 {
        return;
    }
    if !GUI_DVM_PEER_READY.load(Ordering::Acquire) {
        write_u64(DVM_GUI_SURFACE_POOL_INVITATION_OFFSET, generation);
        GUI_DVM_EXPECTED_INVITATION.store(generation, Ordering::Release);
    }
    fence(Ordering::SeqCst);
    let value = (GUI_DVM_PEER_ID << 16).to_le();
    unsafe {
        (doorbell as *mut u8)
            .add(IVSHMEM_DOORBELL_OFFSET)
            .cast::<u32>()
            .write_volatile(value);
    }
}

pub(super) fn reinvite_newest_ready_slot() {
    if GUI_DVM_PEER_READY.load(Ordering::Acquire) || TRANSPORT_REVOKED.load(Ordering::Acquire) {
        return;
    }
    let mut newest = 0_u64;
    for slot in 0..GUI_SLOT_COUNT {
        if SLOT_STATE[slot].load(Ordering::Acquire) == GUI_SLOT_READY {
            newest = newest.max(SLOT_GENERATION[slot].load(Ordering::Acquire));
        }
    }
    if newest == 0 || newest & 1 != 0 {
        return;
    }
    if GUI_DVM_EXPECTED_INVITATION.load(Ordering::Acquire) != newest {
        signal_gui_dvm(newest);
    }
}

pub(super) fn write_host_record(slot: usize, message: DvmGuiSurfaceMessage) {
    let Some(offset) = DVM_GUI_SURFACE_POOL_HOST_RECORD_OFFSET
        .checked_add(slot.saturating_mul(DvmGuiSurfaceMessage::encoded_len()))
    else {
        return;
    };
    write_bytes(offset, &message.encode());
}

pub(super) fn read_dvm_record() -> Option<DvmGuiSurfaceMessage> {
    let mut bytes = [0_u8; DvmGuiSurfaceMessage::encoded_len()];
    read_bytes(DVM_GUI_SURFACE_POOL_DVM_RECORD_OFFSET, &mut bytes)?;
    DvmGuiSurfaceMessage::decode(&bytes)
}

pub(super) fn write_u64(offset: usize, value: u64) {
    write_bytes(offset, &value.to_le_bytes());
}

pub(super) fn write_u32(offset: usize, value: u32) {
    write_bytes(offset, &value.to_le_bytes());
}

pub(super) fn read_u64(offset: usize) -> u64 {
    let mut bytes = [0_u8; size_of::<u64>()];
    if read_bytes(offset, &mut bytes).is_none() {
        return 0;
    }
    u64::from_le_bytes(bytes)
}

pub(super) fn write_bytes(offset: usize, bytes: &[u8]) {
    let header = SHARED_HEADER_ADDR.load(Ordering::Acquire);
    if header == 0 {
        return;
    }
    unsafe {
        for (index, byte) in bytes.iter().enumerate() {
            (header as *mut u8)
                .add(offset + index)
                .write_volatile(*byte);
        }
    }
    fence(Ordering::SeqCst);
}

pub(super) fn read_bytes(offset: usize, bytes: &mut [u8]) -> Option<()> {
    let header = SHARED_HEADER_ADDR.load(Ordering::Acquire);
    if header == 0 {
        return None;
    }
    unsafe {
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = (header as *const u8).add(offset + index).read_volatile();
        }
    }
    Some(())
}

pub(super) fn slot_offset(slot: usize) -> Option<usize> {
    if slot >= GUI_SLOT_COUNT {
        return None;
    }
    let slot_bytes = usize::try_from(POOL_SLOT_BYTES.load(Ordering::Acquire)).ok()?;
    (DVM_GUI_SURFACE_POOL_HEADER_BYTES as usize).checked_add(slot.checked_mul(slot_bytes)?)
}

/// Whether a registered scanout buffer lies inside the pixel region this
/// kernel published.
///
/// A display provider registers an address only so the present path can blit
/// through it; the provenance of that memory is the kernel's, never the
/// caller's. Linux DRM removes the ambiguity outright by taking a GEM handle
/// instead of an address, so the kernel resolves memory it already owns. This
/// is the same rule expressed as a containment check: an address the kernel
/// did not publish is not a scanout buffer, whatever the caller says.
pub(crate) fn scanout_region_contains(addr: u64, size: u64) -> bool {
    // Bound against the region the kernel owns by construction, not against
    // whatever the pool handshake has published so far. Provenance is a
    // property of the memory, so it must not depend on how much of the
    // transport happens to be installed when a provider registers.
    let region_start = crate::memory::higher_half_addr(GUI_DVM_PIXEL_REGION_PHYS_ADDR);
    if size == 0 {
        return false;
    }
    let (Some(end), Some(region_end)) = (
        addr.checked_add(size),
        region_start.checked_add(GUI_DVM_PIXEL_REGION_BYTES),
    ) else {
        return false;
    };
    addr >= region_start && end <= region_end
}

