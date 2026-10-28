//! Owner: original claimed frame through architecture and SIMD return preparation.
//! Boundary: borrowed masked CPU turn, exact successful claim and physical banks.
//! Lifecycle: claim receipt -> retained context/MM/placement -> register install -> CPU publication.
//! Concurrency: no fresh frame, allocation, donor or MM admission after the claim.
//! Failure: stale original turn/frame/bank/UID fails closed before hardware writes.
//! Forbidden: copied SchedulerDispatch or scratch Current as execution permission.
//! Evidence: native receipt/plan witnesses; register/RSP acceptance remains separate.
use super::{SchedulerDispatch, SchedulerPhase, frame_publication, runqueue, stack_resources};
use crate::multitask::cpu_local::dispatch_turn::CpuDispatchTurn;
use x86_64::instructions::segmentation::{DS, ES, FS, GS, Segment};
use x86_64::registers::model_specific::{FsBase, KernelGsBase};
use x86_64::structures::gdt::SegmentSelector;
use x86_64::{PhysAddr, VirtAddr};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::multitask) struct ArchitectureState {
    pub(super) root: u64,
    pub(super) stack_top: u64,
    pub(super) fs: u64,
    pub(super) gs: u64,
    pub(super) process: bool,
    pub(super) register_ipc: bool,
}
impl ArchitectureState {
    pub(super) fn original(turn: &CpuDispatchTurn<'static>) -> Option<Self> {
        Some(turn.outgoing_image()?.architecture)
    }
}

/// This receipt is created ONLY by the successful validated claim. Its original
/// frame has already been read under FrameReader and its allocation moves to the
/// physical bank before this receipt is stored. The CPU turn, not a Copy result,
/// owns it through register installation and publication.
pub(in crate::multitask) struct ClaimedReturn {
    pub(super) id: u64,
    pub(super) token: frame_publication::StableFrameToken,
    pub(super) running_generation: u64,
    pub(super) return_to_user: bool,
    pub(super) previous: ArchitectureState,
}

impl ClaimedReturn {
    pub(in crate::multitask) fn slot(&self) -> usize {
        self.token.slot
    }
    pub(in crate::multitask) fn returns_to_user(&self) -> bool { self.return_to_user }
}

pub(in crate::multitask) struct ReturnArchitecture<'a> {
    turn: &'a CpuDispatchTurn<'static>,
    receipt: &'a ClaimedReturn,
    incoming: ArchitectureState,
    changed: bool,
}
impl<'a> ReturnArchitecture<'a> {
    pub(in crate::multitask) fn capture(
        turn: &'a CpuDispatchTurn<'static>,
        dispatch: SchedulerDispatch,
    ) -> Option<Self> {
        Self::capture_checked(turn, dispatch).ok()
    }

    /// `capture` with the first refused check named.
    ///
    /// The numbering is a diagnostic contract, not an ABI: it exists so a fatal
    /// refusal in the IRQ path can say *which* of the claim, placement, owner,
    /// execution-bank, or MM-root checks failed instead of collapsing them all
    /// into one `None`. Tests keep using `capture`, so the success and refusal
    /// conditions are the same expressions as before.
    pub(in crate::multitask) fn capture_checked(
        turn: &'a CpuDispatchTurn<'static>,
        dispatch: SchedulerDispatch,
    ) -> Result<Self, u8> {
        let (cpu, original) = turn.original_current().ok_or(1_u8)?;
        let receipt = turn.claimed_return().ok_or(2_u8)?;
        if !turn.architecture_pending() {
            return Err(3);
        }
        if !super::execution_interval::original_claim_is_active(turn) {
            return Err(4);
        }
        let token = receipt.token;
        if dispatch.previous_slot != original {
            return Err(5);
        }
        if dispatch.next_slot != token.slot {
            return Err(6);
        }
        if dispatch.next_rsp != token.saved_rsp {
            return Err(7);
        }
        if !frame_publication::claimed_frame_is_current(token) {
            return Err(8);
        }
        let owner = runqueue::owner(token.slot);
        if owner.state != runqueue::RunOwnerState::Running {
            return Err(9);
        }
        if owner.cpu != Some(cpu) {
            return Err(10);
        }
        if owner.generation != receipt.running_generation {
            return Err(11);
        }
        if !stack_resources::cpu_leases::owns_prepared(cpu, token.slot) {
            return Err(12);
        }
        if !crate::multitask::current_identity::execution::banks::owns_execution(cpu, token.slot) {
            return Err(13);
        }
        if !turn.has_claimed_placement(token.slot) {
            return Err(14);
        }
        let incoming = stack_resources::context::retained_architecture(
            cpu, token.slot, receipt.id, token.view,
        )
        .ok_or(15_u8)?;
        if incoming.process {
            let root = turn.claimed_mm_root(token.slot, receipt.id).ok_or(16_u8)?;
            if root != incoming.root {
                return Err(17);
            }
        }
        Ok(Self {
            turn,
            receipt,
            incoming,
            changed: original != token.slot,
        })
    }

    pub(in crate::multitask) fn install(self, fast_ipc: bool) {
        self.install_with(fast_ipc, hardware_effect);
    }

    /// Only privileged effects are modeled. Claim admission, original loans,
    /// branch selection and architecture phase transitions are the production
    /// state machine. This is NOT a hardware acceptance/RSP acknowledgment.
    #[cfg(test)]
    pub(in crate::multitask) fn install_model(self, fast_ipc: bool, effect: impl FnMut(ArchitectureEffect)) {
        self.install_with(fast_ipc, effect);
    }

    fn install_with(self, fast_ipc: bool, mut effect: impl FnMut(ArchitectureEffect)) {
        self.turn.begin_architecture();
        let mut marker = phase_start();
        phase(SchedulerPhase::ArchValidate, &mut marker);
        if self.changed {
            let previous = self.receipt.previous;
            let next = self.incoming;
            if previous.root != next.root {
                effect(ArchitectureEffect::AddressSpace(next.root));
            }
            phase(SchedulerPhase::ArchAddressSpace, &mut marker);
            assert_ne!(next.stack_top, 0, "claimed return has no kernel stack");
            assert_eq!(next.stack_top & 0xf, 0, "claimed return stack is unaligned");
            effect(ArchitectureEffect::PrivilegeStack(next.stack_top));
            effect(ArchitectureEffect::SyscallStack(next.stack_top, next.register_ipc));
            if fast_ipc && previous.root == next.root && !self.receipt.return_to_user {
                if previous.fs != next.fs {
                    effect(ArchitectureEffect::FsBase(next.fs));
                }
                if previous.gs != next.gs {
                    effect(ArchitectureEffect::KernelGsBase(next.gs));
                }
            } else {
                effect(ArchitectureEffect::Segments(next, self.receipt.return_to_user));
            }
            phase(SchedulerPhase::ArchSegments, &mut marker);
        }
        super::ipc_lane::note_installed(self.turn.original_current().map_or(0, |(cpu, _)| cpu),
            self.receipt.token.slot, self.incoming);
        let mut simd_marker = phase_start();
        // Even same-task returns restore the SIMD image: compiled kernel code
        // may use vectors after the IRQ save. This uses the SAME exact receipt.
        effect(ArchitectureEffect::Simd(self.receipt.token.slot));
        phase(SchedulerPhase::ArchSimd, &mut simd_marker);
        phase(SchedulerPhase::ArchRestore, &mut marker);
        self.turn.mark_architecture_ready();
    }
}
/// The register effects of a changed fast-IPC return from `previous` to `next`,
/// the same sequence `install_with` emits. Kernel-mode incoming frames only.
pub(in crate::multitask::scheduler) fn lane_effects(
    previous: ArchitectureState, next: ArchitectureState, to_slot: usize,
    mut effect: impl FnMut(ArchitectureEffect),
) {
    if previous.root != next.root {
        effect(ArchitectureEffect::AddressSpace(next.root));
    }
    assert_ne!(next.stack_top, 0, "lane return has no kernel stack");
    assert_eq!(next.stack_top & 0xf, 0, "lane return stack is unaligned");
    effect(ArchitectureEffect::PrivilegeStack(next.stack_top));
    effect(ArchitectureEffect::SyscallStack(next.stack_top, next.register_ipc));
    if previous.root == next.root {
        if previous.fs != next.fs {
            effect(ArchitectureEffect::FsBase(next.fs));
        }
        if previous.gs != next.gs {
            effect(ArchitectureEffect::KernelGsBase(next.gs));
        }
    } else {
        effect(ArchitectureEffect::Segments(next, false));
    }
    effect(ArchitectureEffect::Simd(to_slot));
}

pub(in crate::multitask::scheduler) fn lane_install(
    cpu: usize, previous: ArchitectureState, next: ArchitectureState, to_slot: usize, to_id: u64,
) {
    #[cfg(not(test))]
    lane_effects(previous, next, to_slot, |effect| match effect {
        // SAFETY: the lane holds the masked IRQ scope and the incoming task's
        // Prepared bank, and names its bound task id.
        ArchitectureEffect::Simd(slot) => unsafe {
            runqueue::simd_tls::restore_simd_state_lane(slot, to_id)
        },
        // The lane already excludes interrupts and holds this CPU's index: the
        // two per-CPU stack writes need neither a second mask nor a second
        // index derivation.
        ArchitectureEffect::PrivilegeStack(top) => {
            crate::arch::gdt::set_privilege_stack_masked(cpu, top)
        }
        ArchitectureEffect::SyscallStack(top, register_ipc) => {
            crate::user::syscall::set_kernel_stack_top_masked(cpu, top, register_ipc)
        }
        other => hardware_effect(other),
    });
    #[cfg(test)]
    let _ = (cpu, to_id);
    // Native tests model the privileged effects, as `install_model` does.
    #[cfg(test)]
    lane_effects(previous, next, to_slot, |effect| LANE_EFFECTS.with(|log| log.borrow_mut().push(effect)));
}

#[cfg(test)]
std::thread_local! {
    pub(in crate::multitask::scheduler) static LANE_EFFECTS:
        core::cell::RefCell<std::vec::Vec<ArchitectureEffect>> = const { core::cell::RefCell::new(std::vec::Vec::new()) };
}

/// Scalar descriptions of effects, never execution or lifetime credentials.
/// The only producer holds ReturnArchitecture's original borrowed turn/receipt
/// across EACH effect, including SIMD and the final ready transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::multitask) enum ArchitectureEffect {
    AddressSpace(u64),
    PrivilegeStack(u64),
    SyscallStack(u64, bool),
    FsBase(u64),
    KernelGsBase(u64),
    Segments(ArchitectureState, bool),
    Simd(usize),
}
fn hardware_effect(effect: ArchitectureEffect) {
    match effect {
        ArchitectureEffect::AddressSpace(root) => crate::memory::paging::load_address_space_phys(PhysAddr::new(root)),
        ArchitectureEffect::PrivilegeStack(top) => crate::arch::gdt::set_privilege_stack(top),
        ArchitectureEffect::SyscallStack(top, register_ipc) => crate::user::syscall::set_kernel_stack_top(top, register_ipc),
        ArchitectureEffect::FsBase(base) => FsBase::write(VirtAddr::new(base)),
        ArchitectureEffect::KernelGsBase(base) => KernelGsBase::write(VirtAddr::new(base)),
        ArchitectureEffect::Segments(state, user) => install_segments(state, user),
        ArchitectureEffect::Simd(slot) => runqueue::simd_tls::restore_simd_state(slot),
    }
}

/// Initial architecture retains BOTH original image and masked Current turn.
/// Physical backing/MM belongs to the existing bank, not a scalar root lookup.
/// There is no saved-frame read or fresh dispatch/funding/placement admission.
pub(super) struct InitialArchitecture<'a> {
    image: &'a super::bootstrap::HardwareScope<'a>,
    turn: &'a CpuDispatchTurn<'static>,
    state: ArchitectureState,
}
impl<'a> InitialArchitecture<'a> {
    #[cfg(test)]
    pub(super) fn test_state(&self) -> (u64, u64) {
        (self.state.root, self.state.stack_top)
    }
    pub(super) fn capture(
        image: &'a super::bootstrap::HardwareScope<'a>,
        turn: &'a CpuDispatchTurn<'static>,
    ) -> Option<Self> {
        let (cpu, slot) = turn.original_current()?;
        image.assert_current(cpu);
        if image.slot() != slot || !super::execution_interval::original_initial_is_active(turn) {
            return None;
        }
        let state = ArchitectureState::original(turn)?;
        if state.process || state.root == 0 || state.stack_top == 0 || state.stack_top & 0xf != 0 {
            return None;
        }
        Some(Self { image, turn, state })
    }
    pub(super) fn install(self) {
        let (cpu, _) = self.turn.original_current().expect("initial hardware lost Current");
        self.image.assert_current(cpu);
        assert!(super::execution_interval::original_initial_is_active(self.turn));
        let mut marker = phase_start();
        phase(SchedulerPhase::ArchValidate, &mut marker);
        crate::memory::paging::load_address_space_phys(PhysAddr::new(self.state.root));
        phase(SchedulerPhase::ArchAddressSpace, &mut marker);
        crate::arch::gdt::set_privilege_stack(self.state.stack_top);
        crate::user::syscall::set_kernel_stack_top(self.state.stack_top, self.state.register_ipc);
        install_segments(self.state, false);
        phase(SchedulerPhase::ArchSegments, &mut marker);
    }
}

/// Borrow an ALREADY executing user-owned exception continuation. No saved
/// frame is consumed, no new execution/MM activation or return claim is minted.
/// The original Current allocation includes its exact context MM execution pin;
/// borrowing the wait loan and mask keeps it alive through the last register.
pub(in crate::multitask) struct CurrentArchitecture<'a> {
    current: &'a super::CurrentWait,
    scope: &'a nucleus_core::util::lockdep::DispatchPreemption<'a>,
    state: ArchitectureState,
}
impl<'a> CurrentArchitecture<'a> {
    pub(in crate::multitask) fn capture(
        scope: &'a nucleus_core::util::lockdep::DispatchPreemption<'a>,
        current: &'a super::CurrentWait,
    ) -> Option<Self> {
        if !super::ipc_publication::scoped(scope) || scope.cpu() != current.cpu() {
            return None;
        }
        let pin = current.original_pin();
        let view = frame_publication::return_view(pin.slot())?;
        if !view.user_mode_task { return None; }
        let state = stack_resources::context::retained_architecture(
            scope.cpu(), pin.slot(), pin.id(), view)?;
        if !state.process || state.stack_top == 0 || state.stack_top & 0xf != 0 { return None; }
        Some(Self { current, scope, state })
    }
    pub(in crate::multitask) fn install(self) {
        // SAME original loan/mask remains borrowed until this consuming method
        // completes; scalar root is never a freestanding lifetime credential.
        assert_eq!(self.scope.cpu(), self.current.cpu());
        assert!(self.scope.is_masked());
        let mut marker = phase_start();
        crate::memory::paging::load_address_space_phys(PhysAddr::new(self.state.root));
        phase(SchedulerPhase::ArchAddressSpace, &mut marker);
        crate::arch::gdt::set_privilege_stack(self.state.stack_top);
        crate::user::syscall::set_kernel_stack_top(self.state.stack_top, self.state.register_ipc);
        install_segments(self.state, true);
        phase(SchedulerPhase::ArchSegments, &mut marker);
    }
}

/// Hardware effect only. Callers retain their ORIGINAL claimed or executing
/// Current custody; this immutable state alone is not an admission API.
fn install_segments(state: ArchitectureState, user: bool) {
    let selector = if user { SegmentSelector(0) } else { crate::arch::gdt::kernel_data_selector() };
    if DS::get_reg() != selector || ES::get_reg() != selector
        || FS::get_reg() != selector || GS::get_reg() != selector {
        // SAFETY: both callers retain exact physical Current/Prepared and MM
        // custody, with own CPU IRQ exclusion through these register effects.
        unsafe {
            DS::set_reg(selector);
            ES::set_reg(selector);
            FS::set_reg(selector);
            GS::set_reg(selector);
        }
    }
    FsBase::write(VirtAddr::new(state.fs));
    crate::user::syscall::prepare_for_context_return(user, state.gs);
}

#[cfg(test)]
mod tests;

fn phase_start() -> u64 {
    #[cfg(rustos_scheduler_phase_profile)]
    {
        crate::arch::clock::monotonic_nanos()
    }
    #[cfg(not(rustos_scheduler_phase_profile))]
    {
        0
    }
}
fn phase(phase: SchedulerPhase, marker: &mut u64) {
    #[cfg(rustos_scheduler_phase_profile)]
    {
        let now = crate::arch::clock::monotonic_nanos();
        super::runtime_profile::owner::local(|row| row.phase(phase, now.saturating_sub(*marker)));
        *marker = now;
    }
    #[cfg(not(rustos_scheduler_phase_profile))]
    {
        let _ = (phase, marker);
    }
}
