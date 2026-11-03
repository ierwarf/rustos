//! Token-keyed registry of live console open descriptions.
//!
//! - **Owner:** `kernel-ps` owns console open-description identity; the
//!   `ConsoleRegistry` lock class guards one `ConsoleRegistry` value.
//! - **Boundary:** A token is `generation << 32 | (slot + 1)`. Lookup,
//!   reference pinning and release decode the token and touch exactly one
//!   slot: cost is independent of how many descriptions or descriptors exist.
//! - **Lifecycle:** reserve a free slot, build the description outside the
//!   lock, install it, then free the slot when the last descriptor reference
//!   drops. Freeing bumps the slot generation, so a stale token can never
//!   name a later description; a slot whose generation would wrap is retired
//!   rather than reused, so a token value is never issued twice.
//! - **Concurrency:** plain data under the caller's `ConsoleRegistry` lock.
//!   Nothing here allocates: growth builds a chunk first (`build_chunk`) and
//!   installs it under the lock with `grow`.
//! - **Failure:** a stale, forged or never-issued token is "not live".
//!   Exhausting the table ceiling is reported to the caller, not hidden.
//! - **Forbidden:** scanning slots on the lookup/release path, reusing a
//!   token value, allocating while the lock is held.
//! - **Evidence:** unit tests below and the descriptor-lifecycle tests in
//!   `table/tests.rs` and `transfer_registry.rs`.

use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};

use super::{ConsoleOpenDescription, ConsoleStreamKind};
use crate::multitask::slot_dir::ChunkTable;

/// Slots added per growth step.
pub(super) const CONSOLE_REGISTRY_CHUNK: usize = 128;
const NO_SLOT: u32 = u32::MAX;

struct LiveEntry {
    description: Weak<ConsoleOpenDescription>,
    stream: ConsoleStreamKind,
    descriptor_refs: usize,
}

enum SlotState {
    Free { next: u32 },
    /// Token handed out; the description is being built outside the lock.
    Reserved,
    Live(LiveEntry),
    /// Generation exhausted: never reused, so token values stay unique.
    Retired,
}

pub(super) struct ConsoleSlot {
    generation: u32,
    state: SlotState,
}

pub(super) struct ConsoleRegistry {
    slots: ChunkTable<ConsoleSlot, CONSOLE_REGISTRY_CHUNK>,
    free_head: u32,
}

fn token_for(index: usize, generation: u32) -> u64 {
    (u64::from(generation) << 32) | (index as u64 + 1)
}

fn decode(token: u64) -> Option<(usize, u32)> {
    let generation = (token >> 32) as u32;
    let slot = (token & 0xffff_ffff) as u32;
    (generation != 0 && slot != 0).then(|| (slot as usize - 1, generation))
}

/// Builds a growth chunk with no lock held.
pub(super) fn build_chunk() -> Option<Box<[ConsoleSlot]>> {
    ChunkTable::<ConsoleSlot, CONSOLE_REGISTRY_CHUNK>::build(|| ConsoleSlot {
        generation: 1,
        state: SlotState::Free { next: NO_SLOT },
    })
}

impl ConsoleRegistry {
    pub(super) const fn new() -> Self {
        Self {
            slots: ChunkTable::new(),
            free_head: NO_SLOT,
        }
    }

    /// Installs a chunk from `build_chunk` and threads its slots onto the
    /// free list. Allocation-free. False at the table ceiling.
    pub(super) fn grow(&mut self, chunk: Box<[ConsoleSlot]>) -> bool {
        let first = self.slots.len();
        if !self.slots.push_chunk(chunk) {
            return false;
        }
        let end = self.slots.len();
        for index in (first..end).rev() {
            self.slots[index].state = SlotState::Free {
                next: self.free_head,
            };
            self.free_head = index as u32;
        }
        true
    }

    /// Takes a free slot and returns its token, or `None` when none is free.
    pub(super) fn reserve(&mut self) -> Option<u64> {
        let index = self.free_head;
        if index == NO_SLOT {
            return None;
        }
        let slot = &mut self.slots[index as usize];
        let SlotState::Free { next } = slot.state else {
            unreachable!("console free list names a non-free slot");
        };
        self.free_head = next;
        slot.state = SlotState::Reserved;
        Some(token_for(index as usize, slot.generation))
    }

    /// Publishes the description for a token returned by `reserve`.
    pub(super) fn install(
        &mut self,
        token: u64,
        description: &Arc<ConsoleOpenDescription>,
        stream: ConsoleStreamKind,
    ) {
        let slot = self
            .slot_mut(token)
            .expect("reserved console token vanished before install");
        assert!(
            matches!(slot.state, SlotState::Reserved),
            "console token installed twice"
        );
        slot.state = SlotState::Live(LiveEntry {
            description: Arc::downgrade(description),
            stream,
            descriptor_refs: 1,
        });
    }

    /// Frees entries whose description is gone without a final release. Only
    /// run when the free list is empty, so the common path never scans.
    pub(super) fn reclaim_orphans(&mut self) {
        for index in 0..self.slots.len() {
            let dead = matches!(
                &self.slots[index].state,
                SlotState::Live(entry) if entry.description.strong_count() == 0
            );
            if dead {
                self.free_slot(index);
            }
        }
    }

    pub(super) fn stream(&self, token: u64) -> Option<ConsoleStreamKind> {
        self.live(token).map(|entry| entry.stream)
    }

    /// True once no descriptor-table reference remains (or never existed).
    pub(super) fn is_last_reference(&self, token: u64) -> bool {
        self.live(token).is_none()
    }

    pub(super) fn try_acquire(&mut self, token: u64) -> bool {
        let Some(entry) = self.live_mut(token) else {
            return false;
        };
        entry.descriptor_refs = entry
            .descriptor_refs
            .checked_add(1)
            .expect("console descriptor reference count exhausted");
        true
    }

    /// Drops one descriptor reference; true when it was the last, which also
    /// frees the slot.
    pub(super) fn release(&mut self, token: u64) -> bool {
        let entry = self
            .live_mut(token)
            .expect("live console description missing from registry");
        entry.descriptor_refs -= 1;
        if entry.descriptor_refs != 0 {
            return false;
        }
        let (index, _) = decode(token).expect("live token decodes");
        self.free_slot(index);
        true
    }

    fn free_slot(&mut self, index: usize) {
        let head = self.free_head;
        let slot = &mut self.slots[index];
        match slot.generation.checked_add(1) {
            Some(next_generation) => {
                slot.generation = next_generation;
                slot.state = SlotState::Free { next: head };
                self.free_head = index as u32;
            }
            None => slot.state = SlotState::Retired,
        }
    }

    fn slot(&self, token: u64) -> Option<&ConsoleSlot> {
        let (index, generation) = decode(token)?;
        self.slots
            .get(index)
            .filter(|slot| slot.generation == generation)
    }

    fn slot_mut(&mut self, token: u64) -> Option<&mut ConsoleSlot> {
        let (index, generation) = decode(token)?;
        self.slots
            .get_mut(index)
            .filter(|slot| slot.generation == generation)
    }

    fn live(&self, token: u64) -> Option<&LiveEntry> {
        match &self.slot(token)?.state {
            SlotState::Live(entry)
                if entry.descriptor_refs != 0 && entry.description.strong_count() != 0 =>
            {
                Some(entry)
            }
            _ => None,
        }
    }

    fn live_mut(&mut self, token: u64) -> Option<&mut LiveEntry> {
        match &mut self.slot_mut(token)?.state {
            SlotState::Live(entry)
                if entry.descriptor_refs != 0 && entry.description.strong_count() != 0 =>
            {
                Some(entry)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;

    fn registry() -> ConsoleRegistry {
        let mut registry = ConsoleRegistry::new();
        assert!(registry.grow(build_chunk().unwrap()));
        registry
    }

    fn open(
        registry: &mut ConsoleRegistry,
        stream: ConsoleStreamKind,
    ) -> (u64, Arc<ConsoleOpenDescription>) {
        let token = registry.reserve().expect("free slot");
        let description = Arc::new(ConsoleOpenDescription { token, stream });
        registry.install(token, &description, stream);
        (token, description)
    }

    #[test]
    fn tokens_are_unique_nonzero_and_resolve_in_one_slot() {
        let mut registry = registry();
        let (a, _ka) = open(&mut registry, ConsoleStreamKind::Input);
        let (b, _kb) = open(&mut registry, ConsoleStreamKind::Output);
        assert!(a != 0 && b != 0 && a != b);
        assert_eq!(registry.stream(a), Some(ConsoleStreamKind::Input));
        assert_eq!(registry.stream(b), Some(ConsoleStreamKind::Output));
        assert_eq!(registry.stream(0), None);
        assert_eq!(registry.stream(u64::MAX), None);
    }

    #[test]
    fn last_release_frees_the_slot_and_stale_tokens_never_alias() {
        let mut registry = registry();
        let (first, keep) = open(&mut registry, ConsoleStreamKind::Input);
        assert!(registry.try_acquire(first));
        assert!(!registry.release(first));
        assert!(!registry.is_last_reference(first));
        assert!(registry.release(first));
        assert!(registry.is_last_reference(first));
        assert!(!registry.try_acquire(first), "no resurrection");
        let (second, _keep2) = open(&mut registry, ConsoleStreamKind::Error);
        assert_ne!(first, second, "slot reuse must change the token");
        assert_eq!(registry.stream(first), None);
        assert_eq!(registry.stream(second), Some(ConsoleStreamKind::Error));
        drop(keep);
    }

    #[test]
    fn reserved_but_uninstalled_tokens_are_not_live() {
        let mut registry = registry();
        let token = registry.reserve().unwrap();
        assert_eq!(registry.stream(token), None);
        assert!(!registry.try_acquire(token));
    }

    #[test]
    fn exhaustion_reclaims_orphans_before_growing() {
        let mut registry = registry();
        let mut live = Vec::new();
        for _ in 0..CONSOLE_REGISTRY_CHUNK {
            live.push(open(&mut registry, ConsoleStreamKind::Output));
        }
        assert_eq!(registry.reserve(), None, "table is full");
        // Drop every description without a final release: they are orphans.
        let tokens: Vec<u64> = live.drain(..).map(|(token, _)| token).collect();
        registry.reclaim_orphans();
        for token in &tokens {
            assert_eq!(registry.stream(*token), None);
        }
        assert!(registry.reserve().is_some(), "orphans were reclaimed");
    }

    #[test]
    fn exhausted_generation_retires_the_slot_instead_of_reusing_a_token() {
        let mut registry = registry();
        // Slot 0 is the free-list head after growth.
        registry.slots[0].generation = u32::MAX;
        let (token, keep) = open(&mut registry, ConsoleStreamKind::Input);
        assert_eq!(token >> 32, u64::from(u32::MAX));
        assert!(registry.release(token));
        assert!(matches!(registry.slots[0].state, SlotState::Retired));
        drop(keep);
        // No later reservation can name slot 0 again.
        for _ in 0..CONSOLE_REGISTRY_CHUNK - 1 {
            let next = registry.reserve().unwrap();
            assert_ne!(next & 0xffff_ffff, 1);
        }
        assert_eq!(registry.reserve(), None);
    }

    #[test]
    fn registry_grows_by_chunk() {
        let mut registry = registry();
        let mut keep = Vec::new();
        for _ in 0..CONSOLE_REGISTRY_CHUNK {
            keep.push(open(&mut registry, ConsoleStreamKind::Output));
        }
        assert!(registry.grow(build_chunk().unwrap()));
        let (token, _d) = open(&mut registry, ConsoleStreamKind::Input);
        assert_eq!(registry.stream(token), Some(ConsoleStreamKind::Input));
    }
}
