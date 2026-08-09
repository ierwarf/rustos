//! Terminal DVM block transport revoke observability.
//!
//! - **Owner:** The DVM block transport owner assigns the terminal reason and
//!   clears its exact generation; storage retry and recovery remain in `storaged`.
//! - **Boundary:** Shared flags and cursors are untrusted observations stamped
//!   before the transport clears readiness or pending slot authority.
//! - **Lifecycle:** The existing revoked guard admits one report, then clears
//!   pending requests, RustOS readiness, and wakes blocked consumers.
//! - **Concurrency:** The parent state lock serializes revoke; reporting uses no
//!   allocation, blocking wait, new lock, or additional synchronization.
//! - **Failure:** Every malformed header, readiness withdrawal, and invalid
//!   completion path records one stable nonzero reason and remains terminal.
//! - **Forbidden:** No retry, recovery, storage policy, fallback, or admission
//!   change belongs in this observer.
//! - **Evidence:** `dvm-block-transport-revoked` plus the fixed debugcon line
//!   bind the reason, generation, flags, and four ring cursors.

use super::*;

/// Closed, kernel-stamped cause of one terminal DVM block transport revoke.
///
/// Keep values stable: the milestone is postmortem evidence consumed outside
/// this transport owner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u64)]
pub(super) enum DvmBlockRevokeReason {
    HeaderImmutableMismatch = 1,
    EpochSignatureMismatch = 2,
    UnknownFlags = 3,
    RustosReadyLost = 4,
    StaticFlagsChanged = 5,
    DvmReadyWithdrawn = 6,
    CursorInvalid = 7,
    Decode = 8,
    SlotInvalid = 9,
    NoPending = 10,
    Duplicate = 11,
    Mismatch = 12,
    #[cfg(test)]
    TestManual = 13,
}

impl DvmBlockRevokeReason {
    #[cfg(test)]
    pub(super) const ALL: [Self; 13] = [
        Self::HeaderImmutableMismatch,
        Self::EpochSignatureMismatch,
        Self::UnknownFlags,
        Self::RustosReadyLost,
        Self::StaticFlagsChanged,
        Self::DvmReadyWithdrawn,
        Self::CursorInvalid,
        Self::Decode,
        Self::SlotInvalid,
        Self::NoPending,
        Self::Duplicate,
        Self::Mismatch,
        Self::TestManual,
    ];

    #[cfg(not(test))]
    const fn name(self) -> &'static str {
        match self {
            Self::HeaderImmutableMismatch => "header-immutable-mismatch",
            Self::EpochSignatureMismatch => "epoch-signature-mismatch",
            Self::UnknownFlags => "unknown-flags",
            Self::RustosReadyLost => "rustos-ready-lost",
            Self::StaticFlagsChanged => "static-flags-changed",
            Self::DvmReadyWithdrawn => "dvm-ready-withdrawn",
            Self::CursorInvalid => "completion-cursor-invalid",
            Self::Decode => "completion-decode-invalid",
            Self::SlotInvalid => "completion-slot-invalid",
            Self::NoPending => "completion-slot-vacant",
            Self::Duplicate => "completion-duplicate",
            Self::Mismatch => "completion-request-mismatch",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct DvmBlockRevokeObservation {
    pub(super) reason: DvmBlockRevokeReason,
    pub(super) generation: u64,
    pub(super) flags: u32,
    pub(super) expected_fixed_flags: u32,
    pub(super) request_producer: u64,
    pub(super) request_consumer: u64,
    pub(super) completion_producer: u64,
    pub(super) completion_consumer: u64,
}

impl DvmBlockState {
    pub(super) fn revoke(&mut self, reason: DvmBlockRevokeReason) {
        let _ = self.revoke_with_observer(reason, report_transport_revoke);
    }

    pub(super) fn revoke_with_observer(
        &mut self,
        reason: DvmBlockRevokeReason,
        observer: impl FnOnce(DvmBlockRevokeObservation),
    ) -> bool {
        if self.revoked {
            return false;
        }
        let observation = DvmBlockRevokeObservation {
            reason,
            generation: self.geometry.generation,
            flags: load_u32(self.base, FLAGS_OFFSET, Ordering::Acquire),
            expected_fixed_flags: self.geometry.flags & !DVM_BLOCK_FLAG_DVM_READY,
            request_producer: load_u64(self.base, REQUEST_PRODUCER_OFFSET, Ordering::Acquire),
            request_consumer: load_u64(self.base, REQUEST_CONSUMER_OFFSET, Ordering::Acquire),
            completion_producer: load_u64(self.base, COMPLETION_PRODUCER_OFFSET, Ordering::Acquire),
            completion_consumer: load_u64(self.base, COMPLETION_CONSUMER_OFFSET, Ordering::Acquire),
        };
        // LIFECYCLE: The existing terminal guard wins before reporting, so one
        // revoke has one immutable pre-clear observation and no later path can
        // erase or duplicate its cause.
        self.revoked = true;
        observer(observation);
        self.pending = [None; QUEUE_DEPTH];
        fetch_and_u32(
            self.base,
            FLAGS_OFFSET,
            !DVM_BLOCK_FLAG_RUSTOS_READY,
            Ordering::AcqRel,
        );
        IRQ_PENDING.store(true, Ordering::Release);
        wake_waiters();
        true
    }
}

#[cfg(not(test))]
fn report_transport_revoke(observation: DvmBlockRevokeObservation) {
    nucleus_core::debug::record_milestone(
        nucleus_core::debug::LogCategory::Storage,
        "dvm-block-transport-revoked",
        observation.reason as u64,
        observation.generation,
    );
    let mut reason_code = [0_u8; 20];
    let mut generation = [0_u8; 20];
    let mut flags = [0_u8; 20];
    let mut expected_fixed_flags = [0_u8; 20];
    let mut request_producer = [0_u8; 20];
    let mut request_consumer = [0_u8; 20];
    let mut completion_producer = [0_u8; 20];
    let mut completion_consumer = [0_u8; 20];
    nucleus_core::debug::write_debugcon_only_parts_line(&[
        b"dvm-block: transport revoked reason=",
        observation.reason.name().as_bytes(),
        b" code=",
        decimal_u64(observation.reason as u64, &mut reason_code),
        b" generation=",
        decimal_u64(observation.generation, &mut generation),
        b" flags=",
        decimal_u64(u64::from(observation.flags), &mut flags),
        b" expected_fixed_flags=",
        decimal_u64(
            u64::from(observation.expected_fixed_flags),
            &mut expected_fixed_flags,
        ),
        b" request_producer=",
        decimal_u64(observation.request_producer, &mut request_producer),
        b" request_consumer=",
        decimal_u64(observation.request_consumer, &mut request_consumer),
        b" completion_producer=",
        decimal_u64(observation.completion_producer, &mut completion_producer),
        b" completion_consumer=",
        decimal_u64(observation.completion_consumer, &mut completion_consumer),
    ]);
}

#[cfg(test)]
fn report_transport_revoke(_observation: DvmBlockRevokeObservation) {}

#[cfg(not(test))]
fn decimal_u64(mut value: u64, buffer: &mut [u8; 20]) -> &[u8] {
    let mut start = buffer.len();
    loop {
        start -= 1;
        buffer[start] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            return &buffer[start..];
        }
    }
}
