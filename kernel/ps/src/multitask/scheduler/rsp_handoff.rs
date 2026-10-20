//! Owner: original claimed stack geometry through the assembly RSP ACK.
//! Boundary: sealed original return, never a directory/queue execution grant.
//! Lifecycle: capture before publication; check actual RSP before either ACK.
//! Concurrency: same IRQ-excluded CPU owns the sealed turn and both pin banks.
//! Failure: foreign CPU, old/foreign stack or missing bank is a fail-stop.
//! Forbidden: locks, allocation, output, frame reads or new grants at arrival.
//! Evidence: actual callback and born-resource refused-arrival witness; native
//! RSP inputs are models, not evidence of machine execution.
use super::*;
use core::sync::atomic::Ordering;

#[repr(align(64))]
struct ArrivalCount(core::sync::atomic::AtomicU64);
static HARDWARE_ARRIVALS: [ArrivalCount; nucleus_core::util::lockdep::MAX_TRACKED_CPUS] =
    [const { ArrivalCount(core::sync::atomic::AtomicU64::new(0)) };
        nucleus_core::util::lockdep::MAX_TRACKED_CPUS];

/// Observational, per-CPU and system-wide; not a private workload count, nor
/// an execution grant. Native modeled callbacks deliberately never increment.
pub fn hardware_rsp_arrivals(cpu: usize) -> Option<u64> {
    HARDWARE_ARRIVALS.get(cpu).map(|count| count.0.load(Ordering::Relaxed))
}

#[derive(Clone, Copy)]
struct StackRanges {
    kernel: (u64, u64),
    alternate: (u64, u64),
}
impl StackRanges {
    fn from_view(view: PublishedFrameView) -> Self {
        Self {
            kernel: (view.kernel_stack_base, view.kernel_stack_top),
            alternate: (view.alternate_stack_base, view.alternate_stack_top),
        }
    }
    fn contains(self, rsp: usize) -> bool {
        let rsp = rsp as u64;
        [self.kernel, self.alternate].into_iter().any(|(base, top)| base != 0 && base <= rsp && rsp < top)
    }
}

/// Observation seal retains no new permission: the SAME turn's original
/// physical/MM and UID banks own the storage until its checked assembly ACK.
pub(in crate::multitask) struct RspHandoff {
    cpu: usize,
    from: usize,
    to: usize,
    id: u64,
    generation: u64,
    expected_rsp: usize,
    incoming: StackRanges,
    outgoing: StackRanges,
}
impl RspHandoff {
    pub(in crate::multitask) fn capture(
        turn: &crate::multitask::cpu_local::dispatch_turn::CpuDispatchTurn<'_>,
    ) -> Self {
        let (cpu, from) = turn.original_current().expect("RSP seal lacks original Current");
        let receipt = turn.claimed_return().expect("RSP seal lacks original claim");
        let to = receipt.slot();
        assert_ne!(from, to, "unchanged return needs no outgoing RSP seal");
        let old = frame_publication::return_view(from).expect("RSP seal lacks outgoing frame geometry");
        assert!(stack_resources::current_return_binding(cpu, from, old).is_some(),
            "RSP seal lost original outgoing allocation binding");
        assert!(stack_resources::cpu_leases::owns_prepared(cpu, to),
            "RSP seal lacks original incoming allocation");
        Self {
            cpu, from, to, id: receipt.id, generation: receipt.running_generation,
            expected_rsp: receipt.token.saved_rsp,
            incoming: StackRanges::from_view(receipt.token.view),
            outgoing: StackRanges::from_view(old),
        }
    }
    pub(in crate::multitask) fn assert_arrived(&self, cpu: usize, rsp: usize) {
        assert!(self.cpu == cpu && self.incoming.contains(rsp) && !self.outgoing.contains(rsp),
            "RSP arrival mismatch cpu={} expected_cpu={} from={} to={} uid={} generation={} frame={:#x} actual={:#x}",
            cpu, self.cpu, self.from, self.to, self.id, self.generation, self.expected_rsp, rsp);
        assert!(stack_resources::cpu_leases::owns_current(cpu, self.to)
            && stack_resources::cpu_leases::owns_outgoing(cpu, self.from)
            && current_identity::execution::banks::owns_current(cpu, self.to)
            && current_identity::execution::banks::owns_outgoing(cpu, self.from),
            "RSP arrival lost original incoming/outgoing allocation or UID bank");
        #[cfg(not(test))]
        HARDWARE_ARRIVALS[cpu].0.fetch_add(1, Ordering::Relaxed);
    }
    #[cfg(test)]
    pub(in crate::multitask) fn modeled_rsp(&self) -> usize { self.expected_rsp }
}
