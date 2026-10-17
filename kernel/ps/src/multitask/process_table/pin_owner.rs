//! Generation-bound physical process-object custody, independent of directory
//! and process-table guards. A static atomic gate outlives the boxed object.
//! Reap closes admission before testing zero; last release requests a rescan.
//! This protects the state-lock allocation, not an old exec MM generation.
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::{MAX_PROCESS_OBJECTS, ProcessHandle};

const CLOSED: u64 = 1 << 31;
const COUNT: u64 = CLOSED - 1;
const GENERATION_SHIFT: u32 = 32;

struct PinCell(AtomicU64);
impl PinCell {
    const fn new() -> Self {
        Self(AtomicU64::new(CLOSED))
    }

    fn bind(&self, generation: u32) {
        assert_ne!(generation, 0, "zero process-pin generation");
        let mut state = self.0.load(Ordering::Acquire);
        loop {
            if state >> GENERATION_SHIFT == u64::from(generation) {
                return;
            }
            assert_eq!(state & COUNT, 0, "pinned process object rebound");
            assert_ne!(state & CLOSED, 0, "open process object rebound");
            let replacement = u64::from(generation) << GENERATION_SHIFT;
            match self.0.compare_exchange_weak(
                state,
                replacement,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => state = observed,
            }
        }
    }

    fn pin(&self, generation: u32) -> bool {
        let mut state = self.0.load(Ordering::Acquire);
        loop {
            if generation == 0
                || state >> GENERATION_SHIFT != u64::from(generation)
                || state & CLOSED != 0
                || state & COUNT == COUNT
            {
                return false;
            }
            match self.0.compare_exchange_weak(
                state,
                state + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => state = observed,
            }
        }
    }

    fn close(&self, generation: u32) -> bool {
        let mut state = self.0.load(Ordering::Acquire);
        loop {
            assert_eq!(
                state >> GENERATION_SHIFT,
                u64::from(generation),
                "foreign process-pin close"
            );
            match self.0.compare_exchange_weak(
                state,
                state | CLOSED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return state & COUNT == 0,
                Err(observed) => state = observed,
            }
        }
    }

    fn release(&self, generation: u32) -> bool {
        let mut state = self.0.load(Ordering::Acquire);
        loop {
            assert_eq!(
                state >> GENERATION_SHIFT,
                u64::from(generation),
                "foreign process-pin release"
            );
            assert_ne!(state & COUNT, 0, "process-pin released twice");
            match self.0.compare_exchange_weak(
                state,
                state - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return state & COUNT == 1 && state & CLOSED != 0,
                Err(observed) => state = observed,
            }
        }
    }
}

static PINS: [PinCell; MAX_PROCESS_OBJECTS] = [const { PinCell::new() }; MAX_PROCESS_OBJECTS];
static REAP_PENDING: AtomicBool = AtomicBool::new(false);

/// Consume-once allocation lifetime, with no lock carried into a caller.
/// Drop is IRQ-safe, allocation-free and never enters the process table.
pub(in crate::multitask) struct ProcessObjectPin {
    handle: ProcessHandle,
}

impl ProcessObjectPin {
    pub(in crate::multitask) fn handle(&self) -> ProcessHandle {
        self.handle
    }
}
impl Drop for ProcessObjectPin {
    fn drop(&mut self) {
        if PINS[self.handle.index()].release(self.handle.generation()) {
            REAP_PENDING.store(true, Ordering::Release);
        }
    }
}

pub(super) fn is_open(handle: ProcessHandle) -> bool {
    PINS.get(handle.index()).is_some_and(|cell| {
        let state = cell.0.load(Ordering::Acquire);
        state >> GENERATION_SHIFT == u64::from(handle.generation()) && state & CLOSED == 0
    })
}

pub(super) fn bind(index: usize, generation: u32) {
    PINS[index].bind(generation);
}
pub(super) fn close(index: usize, generation: u32) -> bool {
    PINS[index].close(generation)
}
pub(super) fn take_reap_notification() -> bool {
    REAP_PENDING.swap(false, Ordering::AcqRel)
}

pub(in crate::multitask) fn pin(handle: ProcessHandle) -> Option<ProcessObjectPin> {
    PINS.get(handle.index())?
        .pin(handle.generation())
        .then(|| ProcessObjectPin { handle })
}

#[cfg(test)]
pub(super) fn reset_for_tests() {
    for cell in &PINS {
        let state = cell.0.load(Ordering::Acquire);
        assert_eq!(
            state & COUNT,
            0,
            "test reset discards process-object custody"
        );
        cell.0.store(CLOSED, Ordering::Release);
    }
    REAP_PENDING.store(false, Ordering::Release);
}

#[cfg(test)]
mod tests;
