use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

const CPU_CAPACITY: usize = nucleus_core::util::lockdep::MAX_TRACKED_CPUS;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(super) enum RescheduleRoute {
    LocalSafePoint = 1,
    RemoteIpi = 2,
    Timer = 3,
    SyscallTail = 4,
    Rtc = 5,
}

#[repr(C, align(64))]
struct RescheduleObservation {
    lifecycle_gen: AtomicU64,
    /// Bit 0 is the durable pending edge and bits 1..63 are the monotonic
    /// request sequence. Publishing and claiming therefore cannot reproduce
    /// the old `load(goal) -> clear(bit)` lost-generation window.
    request_word: AtomicU64,
    notify_seq: AtomicU64,
    consume_seq: AtomicU64,
    last_route: AtomicU8,
}

impl RescheduleObservation {
    const fn new() -> Self {
        Self {
            lifecycle_gen: AtomicU64::new(0),
            request_word: AtomicU64::new(0),
            notify_seq: AtomicU64::new(0),
            consume_seq: AtomicU64::new(0),
            last_route: AtomicU8::new(0),
        }
    }

    fn admit_generation(&self, generation: u64) {
        assert_ne!(
            generation, 0,
            "reschedule observation requires a live CPU generation"
        );
        match self.lifecycle_gen.compare_exchange(
            0,
            generation,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {}
            Err(current) => assert_eq!(
                current, generation,
                "reschedule observation crossed a CPU lifecycle generation"
            ),
        }
    }
}

const PENDING_BIT: u64 = 1;
const MAX_REQUEST_SEQUENCE: u64 = u64::MAX >> 1;

const fn request_sequence(word: u64) -> u64 {
    word >> 1
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RequestPublication {
    pub(super) sequence: u64,
    pub(super) newly_pending: bool,
}

static RESCHEDULE_OBSERVATIONS: [RescheduleObservation; CPU_CAPACITY] =
    [const { RescheduleObservation::new() }; CPU_CAPACITY];

fn observation(cpu: usize) -> &'static RescheduleObservation {
    RESCHEDULE_OBSERVATIONS
        .get(cpu)
        .expect("reschedule observation CPU index out of range")
}

pub(super) fn publish_request(cpu: usize, generation: u64) -> RequestPublication {
    let observation = observation(cpu);
    observation.admit_generation(generation);
    let mut current = observation.request_word.load(Ordering::Acquire);
    loop {
        let sequence = request_sequence(current)
            .checked_add(1)
            .filter(|sequence| *sequence <= MAX_REQUEST_SEQUENCE)
            .expect("reschedule request sequence exhausted");
        let next = (sequence << 1) | PENDING_BIT;
        match observation.request_word.compare_exchange_weak(
            current,
            next,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                return RequestPublication {
                    sequence,
                    newly_pending: current & PENDING_BIT == 0,
                };
            }
            Err(observed) => current = observed,
        }
    }
}

pub(super) fn request_sequence_snapshot(cpu: usize) -> u64 {
    request_sequence(observation(cpu).request_word.load(Ordering::Acquire))
}

pub(super) fn request_pending(cpu: usize) -> bool {
    observation(cpu).request_word.load(Ordering::Acquire) & PENDING_BIT != 0
}

/// Claim the exact sequence represented by the pending edge. A publisher that
/// races after this CAS necessarily observes pending=0 and creates a new edge;
/// a publisher that races before it is included in the returned sequence.
pub(super) fn claim_pending(cpu: usize) -> Option<u64> {
    let observation = observation(cpu);
    let mut current = observation.request_word.load(Ordering::Acquire);
    loop {
        if current & PENDING_BIT == 0 {
            return None;
        }
        let claimed = current & !PENDING_BIT;
        match observation.request_word.compare_exchange_weak(
            current,
            claimed,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return Some(request_sequence(current)),
            Err(observed) => current = observed,
        }
    }
}

pub(super) fn record_notification(cpu: usize, generation: u64, sequence: u64) {
    let observation = observation(cpu);
    observation.admit_generation(generation);
    assert!(
        sequence <= request_sequence(observation.request_word.load(Ordering::Acquire)),
        "reschedule notification exceeds published request sequence"
    );
    observation.notify_seq.fetch_max(sequence, Ordering::AcqRel);
}

pub(super) fn record_consumption(cpu: usize, goal: u64, route: RescheduleRoute) {
    if goal == 0 {
        return;
    }
    let observation = observation(cpu);
    let request = request_sequence(observation.request_word.load(Ordering::Acquire));
    assert!(
        goal <= request,
        "reschedule consumer exceeded its request goal"
    );
    let previous = observation.consume_seq.fetch_max(goal, Ordering::AcqRel);
    observation.last_route.store(route as u8, Ordering::Release);
    if previous == 0 {
        #[cfg(not(test))]
        {
            let lifecycle = observation.lifecycle_gen.load(Ordering::Acquire);
            let notify = observation.notify_seq.load(Ordering::Acquire);
            // Keep the release gate on the structured milestone path used by
            // every other SMP event. arg1 retains the first route in its high
            // byte and the consumed request sequence in the remaining bits.
            crate::debug::record_milestone(
                crate::debug::LogCategory::Sched,
                "smp-resched-route",
                cpu as u64,
                ((route as u64) << 56) | (goal & ((1_u64 << 56) - 1)),
            );
            crate::debug::println!(
                "rustos: milestone name=smp-resched-route cpu={} lifecycle={} seq={} route={} notify={}",
                cpu,
                lifecycle,
                goal,
                route_name(route),
                notify
            );
        }
    }
}

#[cfg(not(test))]
const fn route_name(route: RescheduleRoute) -> &'static str {
    match route {
        RescheduleRoute::LocalSafePoint => "local-safe-point",
        RescheduleRoute::RemoteIpi => "remote-ipi",
        RescheduleRoute::Timer => "timer",
        RescheduleRoute::SyscallTail => "syscall-tail",
        RescheduleRoute::Rtc => "rtc",
    }
}

#[cfg(test)]
pub(super) fn snapshot(cpu: usize) -> (u64, u64, bool, u64, u64, u8) {
    let observation = observation(cpu);
    let request_word = observation.request_word.load(Ordering::Acquire);
    (
        observation.lifecycle_gen.load(Ordering::Acquire),
        request_sequence(request_word),
        request_word & PENDING_BIT != 0,
        observation.notify_seq.load(Ordering::Acquire),
        observation.consume_seq.load(Ordering::Acquire),
        observation.last_route.load(Ordering::Acquire),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_may_coalesce_requests_but_consumption_reaches_the_goal() {
        let cpu = CPU_CAPACITY - 1;
        let first = publish_request(cpu, 77);
        let second = publish_request(cpu, 77);
        assert!(first.newly_pending);
        assert!(!second.newly_pending);
        assert_eq!(second.sequence, first.sequence + 1);
        let claimed = claim_pending(cpu).expect("coalesced request");
        assert_eq!(claimed, second.sequence);
        record_notification(cpu, 77, claimed);
        record_consumption(cpu, claimed, RescheduleRoute::RemoteIpi);
        assert_eq!(
            snapshot(cpu),
            (
                77,
                second.sequence,
                false,
                second.sequence,
                second.sequence,
                RescheduleRoute::RemoteIpi as u8
            )
        );
    }

    #[test]
    fn publication_after_claim_must_create_a_new_pending_edge() {
        let cpu = CPU_CAPACITY - 2;
        let first = publish_request(cpu, 88);
        assert_eq!(claim_pending(cpu), Some(first.sequence));
        let second = publish_request(cpu, 88);
        assert!(second.newly_pending);
        assert!(request_pending(cpu));
        assert_eq!(claim_pending(cpu), Some(second.sequence));
    }
}
