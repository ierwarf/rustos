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
    request_seq: AtomicU64,
    notify_seq: AtomicU64,
    consume_seq: AtomicU64,
    last_route: AtomicU8,
}

impl RescheduleObservation {
    const fn new() -> Self {
        Self {
            lifecycle_gen: AtomicU64::new(0),
            request_seq: AtomicU64::new(0),
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

static RESCHEDULE_OBSERVATIONS: [RescheduleObservation; CPU_CAPACITY] =
    [const { RescheduleObservation::new() }; CPU_CAPACITY];

fn observation(cpu: usize) -> &'static RescheduleObservation {
    RESCHEDULE_OBSERVATIONS
        .get(cpu)
        .expect("reschedule observation CPU index out of range")
}

pub(super) fn publish_request(cpu: usize, generation: u64) -> u64 {
    let observation = observation(cpu);
    observation.admit_generation(generation);
    observation
        .request_seq
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(1).filter(|next| *next != 0)
        })
        .expect("reschedule request sequence exhausted")
        + 1
}

pub(super) fn request_goal(cpu: usize) -> u64 {
    observation(cpu).request_seq.load(Ordering::Acquire)
}

pub(super) fn record_notification(cpu: usize, generation: u64, sequence: u64) {
    let observation = observation(cpu);
    observation.admit_generation(generation);
    assert!(
        sequence <= observation.request_seq.load(Ordering::Acquire),
        "reschedule notification exceeds published request sequence"
    );
    observation.notify_seq.store(sequence, Ordering::Release);
}

pub(super) fn record_consumption(cpu: usize, goal: u64, route: RescheduleRoute) {
    if goal == 0 {
        return;
    }
    let observation = observation(cpu);
    let request = observation.request_seq.load(Ordering::Acquire);
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
pub(super) fn snapshot(cpu: usize) -> (u64, u64, u64, u64, u8) {
    let observation = observation(cpu);
    (
        observation.lifecycle_gen.load(Ordering::Acquire),
        observation.request_seq.load(Ordering::Acquire),
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
        assert_eq!(second, first + 1);
        record_notification(cpu, 77, second);
        record_consumption(cpu, second, RescheduleRoute::RemoteIpi);
        assert_eq!(
            snapshot(cpu),
            (77, second, second, second, RescheduleRoute::RemoteIpi as u8)
        );
    }
}
