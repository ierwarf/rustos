//! Scheduler adapters for the per-slot dispatch address-space root.
//!
//! Host tests retain a private `TaskContext` fixture; production uses the
//! owner-generation-bound runqueue payload and never reads the global catalog
//! merely to load or profile a task root.

use super::Scheduler;
#[cfg(not(test))]
use super::runqueue;

impl Scheduler {
    #[inline]
    pub(super) fn slot_address_space_root(&self, slot: usize) -> u64 {
        #[cfg(not(test))]
        {
            runqueue::address_space::root(slot)
        }
        #[cfg(test)]
        {
            self.contexts[slot]
                .map(|context| context.address_space_root)
                .unwrap_or(0)
        }
    }

    #[inline]
    pub(super) fn initialize_slot_address_space_root(&mut self, slot: usize, value: u64) {
        #[cfg(not(test))]
        runqueue::address_space::initialize(slot, value);
        #[cfg(test)]
        if let Some(context) = self.contexts[slot].as_mut() {
            context.address_space_root = value;
        }
    }

    #[inline]
    pub(super) fn set_slot_address_space_root(&mut self, slot: usize, value: u64) {
        #[cfg(not(test))]
        runqueue::address_space::replace(slot, value);
        #[cfg(test)]
        if let Some(context) = self.contexts[slot].as_mut() {
            context.address_space_root = value;
        }
    }
}
