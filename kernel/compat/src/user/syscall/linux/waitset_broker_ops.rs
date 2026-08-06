//! Provider-generation registry for the general userspace wait set.
//!
//! - **Owner:** Compat owns waiter records; services own object readiness and
//!   generation advancement.
//! - **Boundary:** Provider observations and endpoint publications are accepted
//!   only from the exact live service owner.
//! - **Lifecycle:** Install exact observations, revalidate, match during arm,
//!   wake on generation change/revoke, then remove on every resume/exit path.
//! - **Concurrency:** The tracked registry lock contains bounded,
//!   allocation-free mutations; scheduler wake occurs after releasing it.
//! - **Failure:** Duplicate registration, capacity, task exit, provider restart,
//!   and stale generation remove or reject only the exact waiter.
//! - **Forbidden:** No provider scan loop, callback under lock, PID-only
//!   identity, or generation decrease.
//! - **Evidence:** `waitset`.
// Ring0 owns only bounded wait tokens and scheduler wakeup. Provider services
// own readiness state and generations; every wake is followed by a provider
// recheck before Linux-visible readiness is returned.
use super::*;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use lazy_static::lazy_static;
use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};
use rustos_user_abi::syscall::{
    WAITSET_MAX_INTERESTS, WAITSET_PROVIDER_INPUTD, WAITSET_PROVIDER_MAX, WAITSET_PROVIDER_NETD,
    WAITSET_PROVIDER_SESSIOND, WAITSET_PROVIDER_VFSD, WAITSET_SIGNAL_FLAG_READY,
    WaitSetSignalBrokerArgs, waitset_signal_shape_valid,
};
// Preallocated system-wide observation slab. One task may arm the ABI maximum;
// the remaining capacity admits ordinary multi-provider waits concurrently.
// Exhaustion is explicit EBUSY and never collapses identities or allocates in
// the raw registry critical section.
const WAITSET_OBSERVATIONS_PER_TASK_BUDGET: usize = 16;
const WAITSET_WAITER_CAPACITY: usize =
    multitask::MAX_SCHEDULER_TASKS * WAITSET_OBSERVATIONS_PER_TASK_BUDGET;
pub(crate) const WAITSET_MAX_OBSERVATIONS: usize = WAITSET_MAX_INTERESTS + 1;
const _: () = assert!(WAITSET_WAITER_CAPACITY >= WAITSET_MAX_OBSERVATIONS);
const INPUT_OPEN_DESCRIPTION_CAPACITY: usize = 256;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProviderObservation {
    pub provider: u16,
    pub object_id: u64,
    pub generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WaitSetWaiter {
    task_id: u64,
    process_id: u64,
    observation: ProviderObservation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InputOpenDescription {
    token: u64,
    access: u16,
    refs: u64,
}

lazy_static! {
    static ref WAITSET_WAITERS: TrackedSpinLock<
        [Option<WaitSetWaiter>; WAITSET_WAITER_CAPACITY],
        { LockClass::CompatWaitset as u8 },
    > = TrackedSpinLock::new([None; WAITSET_WAITER_CAPACITY]);
    static ref INPUT_OPEN_DESCRIPTIONS: TrackedSpinLock<
        [Option<InputOpenDescription>; INPUT_OPEN_DESCRIPTION_CAPACITY],
        { LockClass::InputOpenDescription as u8 },
    > = TrackedSpinLock::new([None; INPUT_OPEN_DESCRIPTION_CAPACITY]);
}

/// The last readiness a provider published for one object.
///
/// Answering "is this object readable" used to require an IPC round trip to the
/// owning service on every wait-set scan, bounded at
/// `WAITSET_PROVIDER_QUERY_TIMEOUT_MS`. That bound races the same service's
/// bulk work, and losing the race once is permanent: the caller cancels its
/// reply, the service's late reply is rejected, and from then on every reply
/// answers an abandoned question. The provider already publishes a signal on
/// every transition, so the readiness it implies is a fact ring0 can keep.
///
/// Slots are claimed once and never released - the set of readiness objects is
/// small and stable, and a slot that could be recycled would let a stale
/// generation reappear under a new identity.
struct PublishedReadiness {
    /// Non-zero once claimed. Written last, with release ordering, so a reader
    /// that observes it also observes `object_id`.
    provider: AtomicU32,
    object_id: AtomicU64,
    /// `generation << 1 | ready`. One word so the pair cannot be torn.
    state: AtomicU64,
}

const PUBLISHED_READINESS_CAPACITY: usize = 16;

#[allow(
    clippy::declare_interior_mutable_const,
    reason = "array initialiser for per-slot atomics; each slot is a distinct object"
)]
const PUBLISHED_READINESS_INIT: PublishedReadiness = PublishedReadiness {
    provider: AtomicU32::new(0),
    object_id: AtomicU64::new(0),
    state: AtomicU64::new(0),
};

static PUBLISHED_READINESS: [PublishedReadiness; PUBLISHED_READINESS_CAPACITY] =
    [PUBLISHED_READINESS_INIT; PUBLISHED_READINESS_CAPACITY];

fn publish_provider_readiness(provider: u16, object_id: u64, generation: u64, ready: bool) {
    let state = (generation << 1) | u64::from(ready);
    for slot in PUBLISHED_READINESS.iter() {
        // ORDERING: Acquire pairs with the Release claim below, so a reader
        // that sees the provider also sees the object id it was claimed for.
        let claimed = slot.provider.load(Ordering::Acquire);
        if claimed == u32::from(provider) && slot.object_id.load(Ordering::Relaxed) == object_id {
            store_readiness_if_newer(slot, state);
            return;
        }
        if claimed != 0 {
            continue;
        }
        // ORDERING: Relaxed is exact here; the Release in the claim below is
        // what publishes this store to any reader that observes the provider.
        slot.object_id.store(object_id, Ordering::Relaxed);
        // ORDERING: AcqRel claims the slot and releases the object id written
        // above; Acquire on failure observes the winner's claim.
        if slot
            .provider
            .compare_exchange(0, u32::from(provider), Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            store_readiness_if_newer(slot, state);
            return;
        }
    }
}

/// A generation never moves backwards, so a publication that lost a race to a
/// newer one must not overwrite it.
fn store_readiness_if_newer(slot: &PublishedReadiness, state: u64) {
    // ORDERING: Acquire pairs with the AcqRel publication below so a competing
    // publisher's newer generation is never overwritten by an older one.
    let mut current = slot.state.load(Ordering::Acquire);
    while state > current {
        // ORDERING: AcqRel publishes the readiness word; Acquire on failure
        // observes the concurrent publication and re-tests the generation.
        match slot
            .state
            .compare_exchange(current, state, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => return,
            Err(observed) => current = observed,
        }
    }
}

/// The readiness and generation a provider last published, if it has published
/// at all. `None` means the wait set must fall back to asking the service.
pub(crate) fn published_provider_readiness(provider: u16, object_id: u64) -> Option<(bool, u64)> {
    for slot in PUBLISHED_READINESS.iter() {
        // ORDERING: Acquire on the provider pairs with the claim's Release, so
        // a slot that matches has its object id and state already visible;
        // Relaxed is therefore exact for the id.
        if slot.provider.load(Ordering::Acquire) == u32::from(provider)
            && slot.object_id.load(Ordering::Relaxed) == object_id
        {
            // ORDERING: Acquire pairs with the publisher's AcqRel so readiness
            // is never read older than the generation it is reported with.
            let state = slot.state.load(Ordering::Acquire);
            let generation = state >> 1;
            if generation == 0 {
                return None;
            }
            return Some((state & 1 != 0, generation));
        }
    }
    None
}

pub(crate) fn register_input_open_description(token: u64, access: u16) -> Result<(), i64> {
    if token == 0 || !matches!(access, INPUTD_ACCESS_NATIVE | INPUTD_ACCESS_EVDEV) {
        return Err(LINUX_EINVAL);
    }
    let mut objects = INPUT_OPEN_DESCRIPTIONS.lock();
    if objects.iter().flatten().any(|object| object.token == token) {
        return Err(LINUX_EEXIST);
    }
    let slot = objects
        .iter_mut()
        .find(|slot| slot.is_none())
        .ok_or(LINUX_EMFILE)?;
    *slot = Some(InputOpenDescription {
        token,
        access,
        refs: 1,
    });
    Ok(())
}

pub(crate) fn acquire_input_open_description(token: u64) -> Result<(), i64> {
    let mut objects = INPUT_OPEN_DESCRIPTIONS.lock();
    let object = objects
        .iter_mut()
        .flatten()
        .find(|object| object.token == token)
        .ok_or(LINUX_EBADF)?;
    object.refs = object.refs.checked_add(1).ok_or(LINUX_EOVERFLOW)?;
    Ok(())
}

/// Drops one descriptor reference and reports whether it was the final one.
pub(crate) fn release_input_open_description(token: u64) -> Result<bool, i64> {
    let mut objects = INPUT_OPEN_DESCRIPTIONS.lock();
    let slot = objects
        .iter_mut()
        .find(|slot| slot.as_ref().is_some_and(|object| object.token == token))
        .ok_or(LINUX_EBADF)?;
    let object = slot.as_mut().expect("matched input open description");
    if object.refs > 1 {
        object.refs -= 1;
        return Ok(false);
    }
    *slot = None;
    Ok(true)
}

pub(crate) fn input_open_description_access(token: u64) -> Option<u16> {
    INPUT_OPEN_DESCRIPTIONS
        .lock()
        .iter()
        .flatten()
        .find(|object| object.token == token)
        .map(|object| object.access)
}

pub(crate) fn register_waitset_waiters(
    task_id: u64,
    process_id: u64,
    observations: &[ProviderObservation],
) -> Result<(), i64> {
    register_waitset_waiters_faultable(
        task_id,
        process_id,
        observations,
        nucleus_core::util::fault_injection::should_fail("waitset.register"),
    )
}

fn register_waitset_waiters_faultable(
    task_id: u64,
    process_id: u64,
    observations: &[ProviderObservation],
    injected_failure: bool,
) -> Result<(), i64> {
    if task_id == 0
        || process_id == 0
        || observations.is_empty()
        || observations.len() > WAITSET_MAX_OBSERVATIONS
        || observations.iter().any(|observation| {
            observation.provider == 0
                || observation.provider > WAITSET_PROVIDER_MAX
                || observation.generation == 0
        })
    {
        return Err(LINUX_EINVAL);
    }
    if injected_failure {
        return Err(LINUX_EBUSY);
    }
    let mut waiters = WAITSET_WAITERS.lock();
    let existing = waiters
        .iter()
        .flatten()
        .filter(|waiter| waiter.task_id == task_id)
        .count();
    let free = waiters.iter().filter(|slot| slot.is_none()).count();
    if free.saturating_add(existing) < observations.len() {
        return Err(LINUX_EBUSY);
    }
    // Replacement is one bounded transaction: capacity is proved first, the
    // prior set is removed, then the complete new set is installed without an
    // allocator or fallible operation under the registry lock.
    for slot in waiters.iter_mut() {
        if slot
            .as_ref()
            .is_some_and(|waiter| waiter.task_id == task_id)
        {
            *slot = None;
        }
    }
    for observation in observations.iter().copied() {
        let slot = waiters
            .iter_mut()
            .find(|slot| slot.is_none())
            .expect("waitset capacity preflight diverged");
        *slot = Some(WaitSetWaiter {
            task_id,
            process_id,
            observation,
        });
    }
    Ok(())
}

pub(crate) fn remove_waitset_waiters(task_id: u64) -> usize {
    let mut waiters = WAITSET_WAITERS.lock();
    let mut removed = 0;
    for slot in waiters.iter_mut() {
        if slot
            .as_ref()
            .is_some_and(|waiter| waiter.task_id == task_id)
        {
            *slot = None;
            removed += 1;
        }
    }
    removed
}

/// Returns true only while the exact provider observations installed by the
/// caller are still armed. Providers remove their matching slot before
/// waking, so this check closes the window between the final service recheck
/// and the scheduler arm without issuing service IPC from an armed task.
pub(crate) fn waitset_waiters_match(
    task_id: u64,
    process_id: u64,
    observations: &[ProviderObservation],
) -> bool {
    if observations.is_empty() {
        return true;
    }
    let waiters = WAITSET_WAITERS.lock();
    let mut installed = waiters
        .iter()
        .flatten()
        .filter(|waiter| waiter.task_id == task_id && waiter.process_id == process_id);
    installed.clone().count() == observations.len()
        && installed.all(|waiter| observations.contains(&waiter.observation))
}

pub(crate) fn remove_waitset_waiters_for_process(process_id: u64) {
    let mut waiters = WAITSET_WAITERS.lock();
    for slot in waiters.iter_mut() {
        if slot
            .as_ref()
            .is_some_and(|waiter| waiter.process_id == process_id)
        {
            *slot = None;
        }
    }
}

pub(crate) fn revoke_waitset_provider(service_id: u64) {
    let Some(provider) = provider_for_service(service_id) else {
        return;
    };
    wake_matching(provider, None, None);
}

pub(super) fn syscall_linux_rustos_waitset_signal_broker(args_ptr: u64) -> u64 {
    let args = match usermem::read_current_user_struct::<WaitSetSignalBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if !waitset_signal_shape_valid(&args) {
        return linux_errno(LINUX_EINVAL);
    }
    let Some(service_id) = service_for_provider(args.provider) else {
        return linux_errno(LINUX_EINVAL);
    };
    let Some(process_id) = multitask::current_user_process_id() else {
        return linux_errno(LINUX_EPERM);
    };
    if !ipc_ops::process_owns_live_service_endpoint(process_id, service_id) {
        return linux_errno(LINUX_EPERM);
    }
    publish_provider_readiness(
        args.provider,
        args.object_id,
        args.generation,
        args.flags & WAITSET_SIGNAL_FLAG_READY != 0,
    );
    wake_matching(args.provider, Some(args.object_id), Some(args.generation));
    0
}

fn wake_matching(provider: u16, object_id: Option<u64>, generation: Option<u64>) {
    let mut task_ids = [0_u64; WAITSET_WAITER_CAPACITY];
    let count = take_matching_waiters(provider, object_id, generation, &mut task_ids);
    for task_id in task_ids.into_iter().take(count) {
        let _ = multitask::wake_task(task_id);
    }
}

fn take_matching_waiters(
    provider: u16,
    object_id: Option<u64>,
    generation: Option<u64>,
    task_ids: &mut [u64; WAITSET_WAITER_CAPACITY],
) -> usize {
    let mut count = 0usize;
    {
        let mut waiters = WAITSET_WAITERS.lock();
        for waiter in waiters.iter().flatten() {
            let matches = {
                let observation = waiter.observation;
                observation.provider == provider
                    && object_id.is_none_or(|object| observation.object_id == object)
                    && generation
                        .is_none_or(|value| generation_advances(observation.generation, value))
            };
            if !matches {
                continue;
            }
            if !task_ids[..count].contains(&waiter.task_id) {
                task_ids[count] = waiter.task_id;
                count += 1;
            }
        }
        for slot in waiters.iter_mut() {
            if slot
                .as_ref()
                .is_some_and(|waiter| task_ids[..count].contains(&waiter.task_id))
            {
                *slot = None;
            }
        }
    }
    count
}

fn generation_advances(observed: u64, published: u64) -> bool {
    observed != 0 && published > observed
}

fn service_for_provider(provider: u16) -> Option<u64> {
    match provider {
        WAITSET_PROVIDER_VFSD => Some(IPC_SERVICE_VFSD),
        WAITSET_PROVIDER_NETD => Some(IPC_SERVICE_NETD),
        WAITSET_PROVIDER_INPUTD => Some(IPC_SERVICE_INPUTD),
        WAITSET_PROVIDER_SESSIOND => Some(IPC_SERVICE_SESSIOND),
        _ => None,
    }
}

fn provider_for_service(service_id: u64) -> Option<u16> {
    match service_id {
        IPC_SERVICE_VFSD => Some(WAITSET_PROVIDER_VFSD),
        IPC_SERVICE_NETD => Some(WAITSET_PROVIDER_NETD),
        IPC_SERVICE_INPUTD => Some(WAITSET_PROVIDER_INPUTD),
        IPC_SERVICE_SESSIOND => Some(WAITSET_PROVIDER_SESSIOND),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ProviderObservation, WAITSET_WAITER_CAPACITY, acquire_input_open_description,
        generation_advances, input_open_description_access, provider_for_service,
        register_input_open_description, register_waitset_waiters,
        register_waitset_waiters_faultable, release_input_open_description, remove_waitset_waiters,
        service_for_provider, take_matching_waiters, waitset_waiters_match,
    };
    use rustos_user_abi::syscall::{
        INPUTD_ACCESS_EVDEV, IPC_SERVICE_INPUTD, IPC_SERVICE_NETD, IPC_SERVICE_SESSIOND,
        IPC_SERVICE_VFSD, WAITSET_PROVIDER_INPUTD, WAITSET_PROVIDER_NETD,
        WAITSET_PROVIDER_SESSIOND, WAITSET_PROVIDER_VFSD,
    };
    use spin::Mutex;

    static WAITSET_TEST_GUARD: Mutex<()> = Mutex::new(());

    #[test]
    fn readiness_generation_requires_a_strict_monotonic_advance() {
        assert!(!generation_advances(0, 1));
        assert!(!generation_advances(7, 7));
        assert!(!generation_advances(7, 6));
        assert!(generation_advances(7, 8));
    }

    #[test]
    fn waiter_capacity_admits_one_maximal_arm_and_bounded_concurrency() {
        assert_eq!(
            WAITSET_WAITER_CAPACITY,
            super::multitask::MAX_SCHEDULER_TASKS * super::WAITSET_OBSERVATIONS_PER_TASK_BUDGET
        );
        assert!(WAITSET_WAITER_CAPACITY >= super::WAITSET_MAX_OBSERVATIONS);
    }

    #[test]
    fn waitset_provider_authority_maps_to_one_exact_service() {
        assert_eq!(
            service_for_provider(WAITSET_PROVIDER_VFSD),
            Some(IPC_SERVICE_VFSD)
        );
        assert_eq!(
            service_for_provider(WAITSET_PROVIDER_NETD),
            Some(IPC_SERVICE_NETD)
        );
        assert_eq!(
            service_for_provider(WAITSET_PROVIDER_INPUTD),
            Some(IPC_SERVICE_INPUTD)
        );
        assert_eq!(
            service_for_provider(WAITSET_PROVIDER_SESSIOND),
            Some(IPC_SERVICE_SESSIOND)
        );
        assert_eq!(
            provider_for_service(IPC_SERVICE_NETD),
            Some(WAITSET_PROVIDER_NETD)
        );
        assert_eq!(service_for_provider(0), None);
        assert_eq!(provider_for_service(u64::MAX), None);
    }

    #[test]
    fn input_open_description_survives_dup_until_the_final_close() {
        let token = u64::MAX - 17;
        register_input_open_description(token, INPUTD_ACCESS_EVDEV)
            .expect("unique input open description");
        acquire_input_open_description(token).expect("dup reference");
        assert_eq!(
            input_open_description_access(token),
            Some(INPUTD_ACCESS_EVDEV)
        );
        assert!(!release_input_open_description(token).expect("first close"));
        assert_eq!(
            input_open_description_access(token),
            Some(INPUTD_ACCESS_EVDEV)
        );
        assert!(release_input_open_description(token).expect("last close"));
        assert_eq!(input_open_description_access(token), None);
    }

    #[test]
    fn waiter_removal_before_scheduler_arm_is_detected_by_presence() {
        let _guard = WAITSET_TEST_GUARD.lock();
        let task_id = u64::MAX - 101;
        let process_id = u64::MAX - 102;
        let observations = [ProviderObservation {
            provider: WAITSET_PROVIDER_NETD,
            object_id: 41,
            generation: 7,
        }];
        register_waitset_waiters(task_id, process_id, &observations).expect("register waiter");
        assert!(waitset_waiters_match(task_id, process_id, &observations));
        assert_eq!(remove_waitset_waiters(task_id), 1);
        assert_eq!(remove_waitset_waiters(task_id), 0);
        assert!(!waitset_waiters_match(task_id, process_id, &observations));
    }

    #[test]
    fn waitset_registration_fault_preserves_existing_observations() {
        let _guard = WAITSET_TEST_GUARD.lock();
        let task_id = u64::MAX - 201;
        let process_id = u64::MAX - 202;
        let original = [ProviderObservation {
            provider: WAITSET_PROVIDER_NETD,
            object_id: 41,
            generation: 11,
        }];
        let replacement = [ProviderObservation {
            provider: WAITSET_PROVIDER_VFSD,
            object_id: 42,
            generation: 12,
        }];
        register_waitset_waiters(task_id, process_id, &original).expect("register waiter");
        assert_eq!(
            register_waitset_waiters_faultable(task_id, process_id, &replacement, true),
            Err(super::LINUX_EBUSY)
        );
        assert!(waitset_waiters_match(task_id, process_id, &original));
        assert!(!waitset_waiters_match(task_id, process_id, &replacement));
        remove_waitset_waiters(task_id);
    }

    #[test]
    fn exact_object_publication_never_removes_a_foreign_wait_set() {
        let _guard = WAITSET_TEST_GUARD.lock();
        let first_task = u64::MAX - 301;
        let second_task = u64::MAX - 302;
        let process_id = u64::MAX - 303;
        let first = [ProviderObservation {
            provider: WAITSET_PROVIDER_NETD,
            object_id: 41,
            generation: 7,
        }];
        let second = [ProviderObservation {
            provider: WAITSET_PROVIDER_NETD,
            object_id: 42,
            generation: 7,
        }];
        register_waitset_waiters(first_task, process_id, &first).expect("first exact waiter");
        register_waitset_waiters(second_task, process_id, &second).expect("second exact waiter");

        let mut task_ids = [0_u64; super::WAITSET_WAITER_CAPACITY];
        let count = take_matching_waiters(WAITSET_PROVIDER_NETD, Some(41), Some(8), &mut task_ids);

        assert_eq!(&task_ids[..count], &[first_task]);
        assert!(!waitset_waiters_match(first_task, process_id, &first));
        assert!(waitset_waiters_match(second_task, process_id, &second));
        remove_waitset_waiters(second_task);
    }
}
