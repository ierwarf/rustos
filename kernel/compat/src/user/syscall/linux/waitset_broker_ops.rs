// Ring0 owns only bounded wait tokens and scheduler wakeup. Provider services
// own readiness state and generations; every wake is followed by a provider
// recheck before Linux-visible readiness is returned.
use super::*;
use alloc::collections::BTreeMap;
use lazy_static::lazy_static;
use rustos_user_abi::syscall::{
    WAITSET_ABI_VERSION, WAITSET_GLOBAL_OBJECT_ID, WAITSET_PROVIDER_INPUTD, WAITSET_PROVIDER_MAX,
    WAITSET_PROVIDER_NETD, WAITSET_PROVIDER_SESSIOND, WAITSET_PROVIDER_VFSD,
    WaitSetSignalBrokerArgs,
};
use spin::Mutex;

const WAITSET_WAITER_CAPACITY: usize =
    multitask::MAX_SCHEDULER_TASKS * WAITSET_PROVIDER_MAX as usize;
const INPUT_OPEN_DESCRIPTION_CAPACITY: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
    access: u16,
    refs: u64,
}

lazy_static! {
    static ref WAITSET_WAITERS: Mutex<[Option<WaitSetWaiter>; WAITSET_WAITER_CAPACITY]> =
        Mutex::new([None; WAITSET_WAITER_CAPACITY]);
    static ref INPUT_OPEN_DESCRIPTIONS: Mutex<BTreeMap<u64, InputOpenDescription>> =
        Mutex::new(BTreeMap::new());
}

pub(crate) fn register_input_open_description(token: u64, access: u16) -> Result<(), i64> {
    if token == 0 || !matches!(access, INPUTD_ACCESS_NATIVE | INPUTD_ACCESS_EVDEV) {
        return Err(LINUX_EINVAL);
    }
    let mut objects = INPUT_OPEN_DESCRIPTIONS.lock();
    if objects.contains_key(&token) {
        return Err(LINUX_EEXIST);
    }
    if objects.len() >= INPUT_OPEN_DESCRIPTION_CAPACITY {
        return Err(LINUX_EMFILE);
    }
    objects.insert(token, InputOpenDescription { access, refs: 1 });
    Ok(())
}

pub(crate) fn acquire_input_open_description(token: u64) -> Result<(), i64> {
    let mut objects = INPUT_OPEN_DESCRIPTIONS.lock();
    let object = objects.get_mut(&token).ok_or(LINUX_EBADF)?;
    object.refs = object.refs.checked_add(1).ok_or(LINUX_EOVERFLOW)?;
    Ok(())
}

/// Drops one descriptor reference and reports whether it was the final one.
pub(crate) fn release_input_open_description(token: u64) -> Result<bool, i64> {
    let mut objects = INPUT_OPEN_DESCRIPTIONS.lock();
    let object = objects.get_mut(&token).ok_or(LINUX_EBADF)?;
    if object.refs > 1 {
        object.refs -= 1;
        return Ok(false);
    }
    objects.remove(&token);
    Ok(true)
}

pub(crate) fn input_open_description_access(token: u64) -> Option<u16> {
    INPUT_OPEN_DESCRIPTIONS
        .lock()
        .get(&token)
        .map(|object| object.access)
}

pub(crate) fn register_waitset_waiters(
    task_id: u64,
    process_id: u64,
    observations: &[ProviderObservation],
) -> Result<(), i64> {
    if task_id == 0
        || process_id == 0
        || observations.is_empty()
        || observations.len() > WAITSET_PROVIDER_MAX as usize
        || observations.iter().any(|observation| {
            observation.provider == 0
                || observation.provider > WAITSET_PROVIDER_MAX
                || observation.object_id != WAITSET_GLOBAL_OBJECT_ID
                || observation.generation == 0
        })
    {
        return Err(LINUX_EINVAL);
    }
    let mut waiters = WAITSET_WAITERS.lock();
    for slot in waiters.iter_mut() {
        if slot.is_some_and(|waiter| {
            waiter.task_id == task_id || !multitask::is_user_task_alive(waiter.task_id)
        }) {
            *slot = None;
        }
    }
    let free = waiters.iter().filter(|slot| slot.is_none()).count();
    if free < observations.len() {
        return Err(LINUX_EBUSY);
    }
    for observation in observations {
        let slot = waiters
            .iter_mut()
            .find(|slot| slot.is_none())
            .expect("wait-set capacity changed while locked");
        *slot = Some(WaitSetWaiter {
            task_id,
            process_id,
            observation: *observation,
        });
    }
    Ok(())
}

pub(crate) fn remove_waitset_waiters(task_id: u64) {
    let mut waiters = WAITSET_WAITERS.lock();
    for slot in waiters.iter_mut() {
        if slot.is_some_and(|waiter| waiter.task_id == task_id) {
            *slot = None;
        }
    }
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
    let mut matched = 0usize;
    for waiter in waiters
        .iter()
        .flatten()
        .filter(|waiter| waiter.task_id == task_id && waiter.process_id == process_id)
    {
        if !observations.contains(&waiter.observation) {
            return false;
        }
        matched += 1;
    }
    matched == observations.len()
}

pub(crate) fn remove_waitset_waiters_for_process(process_id: u64) {
    let mut waiters = WAITSET_WAITERS.lock();
    for slot in waiters.iter_mut() {
        if slot.is_some_and(|waiter| waiter.process_id == process_id) {
            *slot = None;
        }
    }
}

pub(crate) fn revoke_waitset_provider(service_id: u64) {
    let Some(provider) = provider_for_service(service_id) else {
        return;
    };
    wake_matching(provider, WAITSET_GLOBAL_OBJECT_ID, None);
}

pub(super) fn syscall_linux_rustos_waitset_signal_broker(args_ptr: u64) -> u64 {
    let args = match usermem::read_current_user_struct::<WaitSetSignalBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.abi_version != WAITSET_ABI_VERSION
        || args.flags != 0
        || args.reserved0 != 0
        || args.object_id != WAITSET_GLOBAL_OBJECT_ID
        || args.generation == 0
    {
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
    wake_matching(args.provider, args.object_id, Some(args.generation));
    0
}

fn wake_matching(provider: u16, object_id: u64, generation: Option<u64>) {
    let mut task_ids = [0_u64; WAITSET_WAITER_CAPACITY];
    let mut count = 0usize;
    {
        let mut waiters = WAITSET_WAITERS.lock();
        for slot in waiters.iter_mut() {
            let Some(waiter) = *slot else {
                continue;
            };
            if waiter.observation.provider != provider
                || waiter.observation.object_id != object_id
                || generation
                    .is_some_and(|value| !generation_advances(waiter.observation.generation, value))
            {
                continue;
            }
            task_ids[count] = waiter.task_id;
            count += 1;
            *slot = None;
        }
    }
    for task_id in task_ids.into_iter().take(count) {
        let _ = multitask::wake_task(task_id);
    }
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
        register_input_open_description, register_waitset_waiters, release_input_open_description,
        remove_waitset_waiters, service_for_provider, waitset_waiters_match,
    };
    use rustos_user_abi::syscall::{
        INPUTD_ACCESS_EVDEV, IPC_SERVICE_INPUTD, IPC_SERVICE_NETD, IPC_SERVICE_SESSIOND,
        IPC_SERVICE_VFSD, WAITSET_GLOBAL_OBJECT_ID, WAITSET_PROVIDER_INPUTD, WAITSET_PROVIDER_NETD,
        WAITSET_PROVIDER_SESSIOND, WAITSET_PROVIDER_VFSD,
    };

    #[test]
    fn readiness_generation_requires_a_strict_monotonic_advance() {
        assert!(!generation_advances(0, 1));
        assert!(!generation_advances(7, 7));
        assert!(!generation_advances(7, 6));
        assert!(generation_advances(7, 8));
    }

    #[test]
    fn waiter_capacity_covers_every_scheduler_task_provider_pair() {
        assert_eq!(
            WAITSET_WAITER_CAPACITY,
            super::multitask::MAX_SCHEDULER_TASKS
                * rustos_user_abi::syscall::WAITSET_PROVIDER_MAX as usize
        );
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
        let task_id = u64::MAX - 101;
        let process_id = u64::MAX - 102;
        let observations = [ProviderObservation {
            provider: WAITSET_PROVIDER_NETD,
            object_id: WAITSET_GLOBAL_OBJECT_ID,
            generation: 7,
        }];
        register_waitset_waiters(task_id, process_id, &observations).expect("register waiter");
        assert!(waitset_waiters_match(task_id, process_id, &observations));
        remove_waitset_waiters(task_id);
        assert!(!waitset_waiters_match(task_id, process_id, &observations));
    }
}
