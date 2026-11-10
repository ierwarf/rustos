//! Owner-word access for the IPC lane, split from `runqueue.rs` for size.
//!
//! - **Owner:** the lane's claim is the table row `Blocked --LaneClaim-->
//!   Running`, whose duty is `ResidentTake`: the lane, not the residency hook,
//!   settles the resident cell of the generation it leaves (it takes it).
//! - **Evidence:** `irq_entry_tests/lane.rs`, `ownership::tests`.
use super::*;

/// A committed lane claim. Holds the `ResidentTake` duty: the caller must take
/// the resident cell of the generation it left ([`Self::take_resident`]).
#[must_use = "a lane claim owes the take of its resident cell (I1)"]
pub(in crate::multitask::scheduler) struct LaneClaim {
    running: RunOwnerSnapshot,
    duty: EnqueueDuty,
}

impl LaneClaim {
    /// The Running word the claim published.
    pub(in crate::multitask::scheduler) fn running(&self) -> RunOwnerSnapshot {
        self.running
    }

    /// Takes the loans parked for the generation this claim left. `None` means
    /// the cell was not parked for it, which the lane treats as a broken
    /// residency handshake (it only claims a cell it observed parked).
    pub(in crate::multitask::scheduler) fn take_resident(self) -> Option<super::super::resident::Loans> {
        let Self { running, duty } = self;
        let slot = duty.slot();
        duty.discharge(DutyKind::ResidentTake);
        super::super::resident::take(slot, running.generation - 1)
    }
}

/// The lane's claim CAS: `expected` must be the parked `Blocked` word.
pub(in crate::multitask::scheduler) fn lane_claim(
    slot: usize,
    expected: RunOwnerSnapshot,
    cpu: usize,
) -> Result<LaneClaim, RunOwnerSnapshot> {
    let committed = transition(slot, expected, OwnerEvent::LaneClaim, Some(cpu))?;
    let running = committed.next;
    Ok(LaneClaim {
        running,
        duty: committed.into_duty(),
    })
}
