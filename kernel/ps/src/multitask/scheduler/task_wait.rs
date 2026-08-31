//! Scheduler adapters for owner-generation-bound wait arms and identities.

use super::runqueue;
use super::{BlockReason, Scheduler};

/// Encodes a block reason for the per-slot wait payload.
#[inline]
fn encoded_reason(reason: BlockReason) -> (u8, u64) {
    match reason {
        BlockReason::None => (runqueue::wait::REASON_NONE, 0),
        BlockReason::Generic => (runqueue::wait::REASON_GENERIC, 0),
        BlockReason::EndpointReceive(endpoint) => {
            (runqueue::wait::REASON_ENDPOINT_RECEIVE, endpoint)
        }
        BlockReason::EndpointReply(reply) => (runqueue::wait::REASON_ENDPOINT_REPLY, reply),
        BlockReason::PagerFault(token) => (runqueue::wait::REASON_PAGER_FAULT, token),
    }
}

/// The slot this CPU is running, when its own execution ownership proves the
/// task is admitted, live, and not retired.
///
/// The owner word is the lifetime authority the catalog flags mirror:
/// `runqueue::retire` drives the word terminal in the same guarded transaction
/// that sets `retired`, and it runs first; a slot admitted `start_suspended` is
/// admitted `Blocked` and cannot reach `Running` before activation clears the
/// flag; and admission publishes the payload before the owner word. So
/// `Running(this cpu)` is strictly stronger than the three catalog reads the
/// guarded arm makes, and it is available without the guard.
fn current_running_slot() -> Option<usize> {
    let slot = super::super::cpu_local::current_cpu_task_slot()?;
    if slot == super::ROOT_TASK_SLOT {
        return None;
    }
    let owner = runqueue::owner(slot);
    let cpu = nucleus_core::util::lockdep::current_cpu_index();
    (owner.state == runqueue::RunOwnerState::Running && owner.cpu == Some(cpu)).then_some(slot)
}

/// Arms the current task's exact wait without the catalog guard.
///
/// `None` means the guard must answer: this CPU has no published current task,
/// it is the root task, or its execution ownership is not `Running` here.
///
/// A wake racing this arm is what the pairing with `commit_block_current_task`
/// exists for, and it stays exact without the guard because the arm and its
/// reason are one store: the racing wake's own single store either precedes it
/// -- which is what the caller's post-arm recheck covers, exactly as it did
/// under the guard -- or follows it and withdraws the arm, so the commit
/// refuses to sleep.
pub(in crate::multitask) fn arm_current_wait(reason: BlockReason) -> Option<bool> {
    let slot = current_running_slot()?;
    if runqueue::wait::blocked(slot) {
        return Some(false);
    }
    let (kind, id) = encoded_reason(reason);
    runqueue::wait::set_ready_since_ticks(slot, 0);
    runqueue::wait::publish_arm(slot, kind, id);
    Some(true)
}

/// Withdraws the current task's arm without the catalog guard.
pub(in crate::multitask) fn cancel_current_wait() -> Option<bool> {
    let slot = current_running_slot()?;
    if !runqueue::wait::armed(slot) || runqueue::wait::blocked(slot) {
        return Some(false);
    }
    runqueue::wait::set_ready_since_ticks(slot, 0);
    runqueue::wait::clear_arm(slot);
    Some(true)
}

/// Commits the current CPU's arm without entering the lifecycle catalog.
///
/// The owner word carries running custody, run intent, arm, and reason in one
/// CAS. A wake that wins first removes the arm and this returns `Some(false)`;
/// a wake that follows a successful commit restores run intent before the
/// schedule trap, so the outgoing continuation is requeued instead of lost.
/// `None` is reserved for callers that lack exact current-CPU ownership and
/// must use the catalog fallback.
pub(in crate::multitask) fn commit_current_wait() -> Option<bool> {
    let slot = current_running_slot()?;
    match runqueue::wait::commit(slot) {
        runqueue::WaitCommitOutcome::Committed => {
            runqueue::wait::set_ready_since_ticks(slot, 0);
            runqueue::wait::set_blocked_since_ticks(slot, crate::arch::rtc::ticks());
            Some(true)
        }
        runqueue::WaitCommitOutcome::WakeWon => {
            runqueue::wait::set_ready_since_ticks(slot, 0);
            Some(false)
        }
        runqueue::WaitCommitOutcome::InvalidOwner => None,
    }
}

/// Restores run intent for this CPU's exact pager-fault wait.
///
/// This is the rollback half of the fused pager block publication. If slot
/// cancellation already won, the owner word is already runnable and the
/// function reports success without recreating the consumed wait. A different
/// wait identity is never disturbed.
pub(in crate::multitask) fn wake_current_pager_fault_wait(token: u64) -> Option<bool> {
    if token == 0 {
        return Some(false);
    }
    let slot = current_running_slot()?;
    let owner = runqueue::owner(slot);
    let (kind, id) = runqueue::wait::reason(slot);
    if kind == runqueue::wait::REASON_PAGER_FAULT && id == token {
        runqueue::wake_wait(slot);
        return Some(true);
    }
    Some(owner.runnable)
}

impl Scheduler {
    #[inline]
    pub(super) fn slot_blocked(&self, slot: usize) -> bool {
        #[cfg(not(test))]
        {
            runqueue::wait::blocked(slot)
        }
        #[cfg(test)]
        {
            self.contexts[slot]
                .map(|context| context.blocked)
                .unwrap_or(false)
        }
    }

    #[inline]
    pub(super) fn set_slot_blocked(&mut self, slot: usize, blocked: bool) {
        #[cfg(not(test))]
        runqueue::wait::set_blocked(slot, blocked);
        #[cfg(test)]
        if let Some(context) = self.contexts[slot].as_mut() {
            context.blocked = blocked;
        }
    }

    #[inline]
    pub(super) fn slot_ready_since_ticks(&self, slot: usize) -> u64 {
        #[cfg(not(test))]
        {
            runqueue::wait::ready_since_ticks(slot)
        }
        #[cfg(test)]
        {
            self.contexts[slot]
                .map(|context| context.ready_since_ticks)
                .unwrap_or(0)
        }
    }

    #[inline]
    pub(super) fn set_slot_ready_since_ticks(&mut self, slot: usize, ticks: u64) {
        #[cfg(not(test))]
        runqueue::wait::set_ready_since_ticks(slot, ticks);
        #[cfg(test)]
        if let Some(context) = self.contexts[slot].as_mut() {
            context.ready_since_ticks = ticks;
        }
    }

    #[inline]
    pub(super) fn slot_blocked_since_ticks(&self, slot: usize) -> u64 {
        #[cfg(not(test))]
        {
            runqueue::wait::blocked_since_ticks(slot)
        }
        #[cfg(test)]
        {
            self.contexts[slot]
                .map(|context| context.blocked_since_ticks)
                .unwrap_or(0)
        }
    }

    #[inline]
    pub(super) fn set_slot_blocked_since_ticks(&mut self, slot: usize, ticks: u64) {
        #[cfg(not(test))]
        runqueue::wait::set_blocked_since_ticks(slot, ticks);
        #[cfg(test)]
        if let Some(context) = self.contexts[slot].as_mut() {
            context.blocked_since_ticks = ticks;
        }
    }

    /// Arms a race-free block on the current task. Pair with
    /// `commit_block_current_task`; a raced wake clears the arm and makes the
    /// commit refuse to sleep.
    pub(in crate::multitask) fn arm_block_current_task(&mut self) -> bool {
        self.arm_block_current_task_with_reason(BlockReason::Generic)
    }

    pub(in crate::multitask) fn arm_block_current_task_on_endpoint(
        &mut self,
        endpoint: u64,
    ) -> bool {
        (endpoint != 0)
            && self.arm_block_current_task_with_reason(BlockReason::EndpointReceive(endpoint))
    }

    pub(in crate::multitask) fn arm_block_current_task_on_reply(&mut self, reply: u64) -> bool {
        (reply != 0) && self.arm_block_current_task_with_reason(BlockReason::EndpointReply(reply))
    }

    /// Arms the current faulting task before a normal-time dispatcher may
    /// publish its request to pagerd. The token is the exact fixed fault-slot
    /// generation, never an endpoint or physical-frame identity.
    pub(in crate::multitask) fn arm_block_current_task_on_pager_fault(
        &mut self,
        token: u64,
    ) -> bool {
        (token != 0) && self.arm_block_current_task_with_reason(BlockReason::PagerFault(token))
    }

    pub(in crate::multitask) fn arm_block_current_task_with_reason(
        &mut self,
        reason: BlockReason,
    ) -> bool {
        let slot = self.current_task_slot();
        if slot == super::ROOT_TASK_SLOT || self.retired[slot] || self.start_suspended[slot] {
            return false;
        }
        if self.slot_blocked(slot) {
            return false;
        }
        if self.contexts[slot].is_none() {
            return false;
        }
        #[cfg(test)]
        {
            let context = self.contexts[slot].as_mut().expect("checked wait context");
            context.test_ready = false;
            context.wake_armed = true;
            context.block_reason = reason;
        }
        self.set_slot_ready_since_ticks(slot, 0);
        self.publish_slot_wait_arm(slot, reason);
        true
    }

    /// Cancels a current arm after its caller has re-checked the condition.
    pub(in crate::multitask) fn cancel_block_current_task(&mut self) -> bool {
        let slot = self.current_task_slot();
        if slot == super::ROOT_TASK_SLOT
            || self.retired[slot]
            || self.start_suspended[slot]
            || !self.slot_wait_armed(slot)
        {
            return false;
        }
        if self.slot_blocked(slot) {
            return false;
        }
        if self.contexts[slot].is_none() {
            return false;
        }
        #[cfg(test)]
        {
            let context = self.contexts[slot].as_mut().expect("checked wait context");
            context.test_ready = false;
            context.wake_armed = false;
            context.block_reason = BlockReason::None;
        }
        self.set_slot_ready_since_ticks(slot, 0);
        self.clear_slot_wait_arm(slot);
        true
    }

    /// Commits an arm. A raced wake returns `Some(false)` rather than sleeping.
    pub(in crate::multitask) fn commit_block_current_task(&mut self) -> Option<bool> {
        let slot = self.current_task_slot();
        if slot == super::ROOT_TASK_SLOT
            || self.retired[slot]
            || self.start_suspended[slot]
            || self.contexts[slot].is_none()
            || self.slot_blocked(slot)
        {
            return None;
        }
        #[cfg(not(test))]
        {
            return match runqueue::wait::commit(slot) {
                runqueue::WaitCommitOutcome::Committed => {
                    self.set_slot_ready_since_ticks(slot, 0);
                    self.set_slot_blocked_since_ticks(slot, crate::arch::rtc::ticks());
                    Some(true)
                }
                runqueue::WaitCommitOutcome::WakeWon => {
                    self.set_slot_ready_since_ticks(slot, 0);
                    Some(false)
                }
                runqueue::WaitCommitOutcome::InvalidOwner => None,
            };
        }
        #[cfg(test)]
        {
            if !self.slot_wait_armed(slot) {
                let context = self.contexts[slot].as_mut()?;
                context.test_ready = false;
                context.block_reason = BlockReason::None;
                self.set_slot_ready_since_ticks(slot, 0);
                self.set_slot_block_reason(slot, BlockReason::None);
                return Some(false);
            }
            let context = self.contexts[slot].as_mut()?;
            context.wake_armed = false;
            context.test_ready = false;
            self.set_slot_blocked(slot, true);
            self.set_slot_ready_since_ticks(slot, 0);
            self.set_slot_blocked_since_ticks(slot, crate::arch::rtc::ticks());
            self.set_slot_wait_armed(slot, false);
            Some(true)
        }
    }

    /// Publishes an arm and its exact reason as one payload transition.
    #[inline]
    pub(super) fn publish_slot_wait_arm(&mut self, slot: usize, reason: BlockReason) {
        #[cfg(not(test))]
        {
            let (kind, id) = encoded_reason(reason);
            runqueue::wait::publish_arm(slot, kind, id);
        }
        #[cfg(test)]
        if let Some(context) = self.contexts[slot].as_mut() {
            context.wake_armed = true;
            context.block_reason = reason;
        }
    }

    /// Withdraws an arm and its reason as one payload transition.
    #[inline]
    pub(super) fn clear_slot_wait_arm(&mut self, slot: usize) {
        #[cfg(not(test))]
        {
            let _ = slot;
            runqueue::wait::clear_arm(slot);
        }
        #[cfg(test)]
        if let Some(context) = self.contexts[slot].as_mut() {
            context.wake_armed = false;
            context.block_reason = BlockReason::None;
        }
    }

    #[inline]
    pub(super) fn slot_wait_armed(&self, slot: usize) -> bool {
        #[cfg(not(test))]
        {
            runqueue::wait::armed(slot)
        }
        #[cfg(test)]
        {
            self.contexts[slot]
                .map(|context| context.wake_armed)
                .unwrap_or(false)
        }
    }

    #[inline]
    pub(super) fn set_slot_wait_armed(&mut self, slot: usize, armed: bool) {
        #[cfg(not(test))]
        runqueue::wait::set_armed(slot, armed);
        #[cfg(test)]
        if let Some(context) = self.contexts[slot].as_mut() {
            context.wake_armed = armed;
        }
    }

    #[inline]
    pub(super) fn slot_block_reason(&self, slot: usize) -> BlockReason {
        #[cfg(not(test))]
        {
            let (kind, id) = runqueue::wait::reason(slot);
            match kind {
                runqueue::wait::REASON_NONE => BlockReason::None,
                runqueue::wait::REASON_GENERIC => BlockReason::Generic,
                runqueue::wait::REASON_ENDPOINT_RECEIVE if id != 0 => {
                    BlockReason::EndpointReceive(id)
                }
                runqueue::wait::REASON_ENDPOINT_REPLY if id != 0 => BlockReason::EndpointReply(id),
                runqueue::wait::REASON_PAGER_FAULT if id != 0 => BlockReason::PagerFault(id),
                _ => panic!("scheduler wait payload has invalid exact reason"),
            }
        }
        #[cfg(test)]
        {
            self.contexts[slot]
                .map(|context| context.block_reason)
                .unwrap_or(BlockReason::None)
        }
    }

    #[inline]
    pub(super) fn set_slot_block_reason(&mut self, slot: usize, reason: BlockReason) {
        #[cfg(not(test))]
        {
            let (kind, id) = encoded_reason(reason);
            runqueue::wait::set_reason(slot, kind, id);
        }
        #[cfg(test)]
        if let Some(context) = self.contexts[slot].as_mut() {
            context.block_reason = reason;
        }
    }

    #[inline]
    pub(super) fn initialize_slot_wait_state(&mut self, slot: usize) {
        #[cfg(not(test))]
        runqueue::wait::initialize(slot);
        #[cfg(test)]
        let _ = slot;
    }
}

#[cfg(test)]
mod current_wait_tests {
    use super::*;
    use crate::multitask::cpu_local::{install_test_current_owner, test_publication_lock};

    const CPU: usize = 0;
    const SLOT: usize = 17;
    const OTHER_SLOT: usize = 18;

    struct Published {
        _serial: std::sync::MutexGuard<'static, ()>,
        _runqueue: std::sync::MutexGuard<'static, ()>,
        _cpu: crate::multitask::cpu_local::TestCpuPublicationRestore,
    }

    impl Drop for Published {
        fn drop(&mut self) {
            runqueue::reset_before_publication();
        }
    }

    fn running_here(slot: usize) -> Published {
        let serial = test_publication_lock();
        let runqueue_serial = runqueue::test_serial_guard();
        runqueue::reset_before_publication();
        runqueue::weight::initialize(SLOT, super::super::NICE_0_LOAD);
        runqueue::weight::initialize(OTHER_SLOT, super::super::NICE_0_LOAD);
        runqueue::admit_running(slot, CPU);
        runqueue::admit_blocked(if slot == SLOT { OTHER_SLOT } else { SLOT });
        let cpu = install_test_current_owner(CPU, slot);
        Published {
            _serial: serial,
            _runqueue: runqueue_serial,
            _cpu: cpu,
        }
    }

    #[test]
    fn an_arm_publishes_its_exact_reason_with_the_arm_itself() {
        let published = running_here(SLOT);
        assert_eq!(
            arm_current_wait(BlockReason::EndpointReply(0x4242)),
            Some(true)
        );
        assert!(runqueue::wait::armed(SLOT));
        assert_eq!(
            runqueue::wait::reason(SLOT),
            (runqueue::wait::REASON_ENDPOINT_REPLY, 0x4242)
        );

        // Withdrawal takes both back together, which is what makes a racing
        // wake's single store indivisible against the arm's.
        assert_eq!(cancel_current_wait(), Some(true));
        assert!(!runqueue::wait::armed(SLOT));
        // The identity is meaningless without a kind and is deliberately left
        // for the next arm to overwrite; only the kind is authority.
        assert_eq!(runqueue::wait::reason(SLOT).0, runqueue::wait::REASON_NONE);
        drop(published);
    }

    #[test]
    fn pager_fault_arm_preserves_only_its_fixed_slot_token() {
        let published = running_here(SLOT);
        assert_eq!(
            arm_current_wait(BlockReason::PagerFault(0xface)),
            Some(true)
        );
        assert!(runqueue::wait::armed(SLOT));
        assert_eq!(
            runqueue::wait::reason(SLOT),
            (runqueue::wait::REASON_PAGER_FAULT, 0xface)
        );
        assert_eq!(cancel_current_wait(), Some(true));
        assert_eq!(runqueue::wait::reason(SLOT).0, runqueue::wait::REASON_NONE);
        drop(published);
    }

    #[test]
    fn pager_fault_commit_rollback_restores_only_the_exact_wait() {
        let published = running_here(SLOT);
        assert_eq!(
            arm_current_wait(BlockReason::PagerFault(0xface)),
            Some(true)
        );
        assert_eq!(commit_current_wait(), Some(true));
        assert!(!runqueue::owner(SLOT).runnable);

        assert_eq!(wake_current_pager_fault_wait(0xbeef), Some(false));
        assert!(!runqueue::owner(SLOT).runnable);
        assert_eq!(wake_current_pager_fault_wait(0xface), Some(true));
        assert!(runqueue::owner(SLOT).runnable);
        assert_eq!(runqueue::wait::reason(SLOT).0, runqueue::wait::REASON_NONE);
        drop(published);
    }

    #[test]
    fn a_slot_this_cpu_does_not_run_defers_to_the_catalog() {
        let published = running_here(SLOT);
        // The blocked peer is live but not this CPU's execution owner, so the
        // reader may not decide for it. Publishing it as current without
        // running ownership is exactly the case the owner word rejects.
        drop(published);

        let _serial = test_publication_lock();
        let _runqueue = runqueue::test_serial_guard();
        runqueue::reset_before_publication();
        runqueue::weight::initialize(SLOT, super::super::NICE_0_LOAD);
        runqueue::admit_blocked(SLOT);
        let cpu = install_test_current_owner(CPU, SLOT);
        assert_eq!(arm_current_wait(BlockReason::Generic), None);
        assert_eq!(cancel_current_wait(), None);
        drop(cpu);
        runqueue::reset_before_publication();
    }

    #[test]
    fn the_root_slot_and_an_already_blocked_slot_never_arm_here() {
        let published = running_here(SLOT);
        runqueue::wait::set_blocked(SLOT, true);
        assert_eq!(arm_current_wait(BlockReason::Generic), Some(false));
        assert!(!runqueue::wait::armed(SLOT));
        assert_eq!(cancel_current_wait(), Some(false));
        runqueue::wait::set_blocked(SLOT, false);
        drop(published);

        let root = running_here(super::super::ROOT_TASK_SLOT);
        assert_eq!(arm_current_wait(BlockReason::Generic), None);
        drop(root);
    }
}
