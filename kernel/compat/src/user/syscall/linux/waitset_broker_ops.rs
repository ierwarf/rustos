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
use core::sync::atomic::{fence, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use lazy_static::lazy_static;
use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};
use rustos_user_abi::syscall::{
    WAITSET_MAX_INTERESTS, WAITSET_PROVIDER_INPUTD, WAITSET_PROVIDER_KERNEL,
    WAITSET_PROVIDER_MAX, WAITSET_PROVIDER_NETD,
    WAITSET_PROVIDER_SESSIOND, WAITSET_PROVIDER_VFSD, WAITSET_SIGNAL_FLAG_READY,
    WaitSetSignalBrokerArgs, waitset_signal_shape_valid,
};
// Preallocated system-wide observation slab. One task may arm the ABI maximum;
// the remaining capacity admits ordinary multi-provider waits concurrently.
// Exhaustion is explicit EBUSY and never collapses identities or allocates in
// the raw registry critical section.
// 8 per task over the 256-slot task table keeps the slab at the 2,048 entries it
// had at 16 per task over 128 slots: the table is built by value before it is
// published (`lazy_static`), so doubling it to ~196 KiB overflowed a 2 MiB
// thread stack in unoptimized builds, and no task arms more than a few providers.
const WAITSET_OBSERVATIONS_PER_TASK_BUDGET: usize = 8;
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

/// Observations of `WAITSET_PROVIDER_KERNEL` currently installed in
/// `WAITSET_WAITERS`. Maintained only under that lock; read lock-free by
/// `wake_local_fd_object` so a pipe write with nobody waiting never scans the
/// waiter slab.
static KERNEL_OBSERVERS: AtomicUsize = AtomicUsize::new(0);

/// Monotonic count of every waitset signal or provider revocation delivered
/// through this module. Sampled before a blocking `epoll_wait` scan and
/// re-checked after `register_waitset_waiters`'s `SeqCst` fence so an
/// undisturbed wait skips the redundant second readiness scan without risking
/// a lost wakeup.
static WAITSET_SIGNAL_EPOCH: AtomicU64 = AtomicU64::new(0);

pub(crate) fn signal_epoch() -> u64 {
    // ORDERING: Acquire pairs with `SeqCst` increments in `bump_signal_epoch`
    // and follows the `SeqCst` fence in `register_waitset_waiters`.
    WAITSET_SIGNAL_EPOCH.load(Ordering::Acquire)
}

fn bump_signal_epoch() {
    // ORDERING: SeqCst participates in the total order with
    // `register_waitset_waiters`'s `SeqCst` fence so a waiter either sees the
    // bumped epoch or has its registration visible to the wake that follows.
    WAITSET_SIGNAL_EPOCH.fetch_add(1, Ordering::SeqCst);
}

const WAITSET_NO_SLOT: u16 = u16::MAX;
const WAITSET_INDEX_BUCKETS: usize = 256;
const _: () = assert!(WAITSET_WAITER_CAPACITY < WAITSET_NO_SLOT as usize);

fn object_bucket(provider: u16, object_id: u64) -> usize {
    let mixed = object_id
        .wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ (u64::from(provider) << 48);
    ((mixed ^ (mixed >> 24) ^ (mixed >> 48)) as usize) & (WAITSET_INDEX_BUCKETS - 1)
}

fn task_bucket(task_id: u64) -> usize {
    let mixed = task_id.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    ((mixed ^ (mixed >> 24) ^ (mixed >> 48)) as usize) & (WAITSET_INDEX_BUCKETS - 1)
}

struct WaiterTable {
    slots: [Option<WaitSetWaiter>; WAITSET_WAITER_CAPACITY],
    active_count: usize,
    free_head: u16,
    next_free: [u16; WAITSET_WAITER_CAPACITY],
    object_heads: [u16; WAITSET_INDEX_BUCKETS],
    object_prev: [u16; WAITSET_WAITER_CAPACITY],
    object_next: [u16; WAITSET_WAITER_CAPACITY],
    task_heads: [u16; WAITSET_INDEX_BUCKETS],
    task_prev: [u16; WAITSET_WAITER_CAPACITY],
    task_next: [u16; WAITSET_WAITER_CAPACITY],
}

impl WaiterTable {
    fn boxed() -> alloc::boxed::Box<Self> {
        let mut table = alloc::boxed::Box::new(Self {
            slots: [None; WAITSET_WAITER_CAPACITY],
            active_count: 0,
            free_head: 0,
            next_free: [WAITSET_NO_SLOT; WAITSET_WAITER_CAPACITY],
            object_heads: [WAITSET_NO_SLOT; WAITSET_INDEX_BUCKETS],
            object_prev: [WAITSET_NO_SLOT; WAITSET_WAITER_CAPACITY],
            object_next: [WAITSET_NO_SLOT; WAITSET_WAITER_CAPACITY],
            task_heads: [WAITSET_NO_SLOT; WAITSET_INDEX_BUCKETS],
            task_prev: [WAITSET_NO_SLOT; WAITSET_WAITER_CAPACITY],
            task_next: [WAITSET_NO_SLOT; WAITSET_WAITER_CAPACITY],
        });
        for idx in 0..WAITSET_WAITER_CAPACITY - 1 {
            table.next_free[idx] = (idx + 1) as u16;
        }
        table
    }

    fn free_count(&self) -> usize {
        WAITSET_WAITER_CAPACITY - self.active_count
    }

    fn count_for_task(&self, task_id: u64) -> usize {
        let mut count = 0;
        let mut cur = self.task_heads[task_bucket(task_id)];
        while cur != WAITSET_NO_SLOT {
            let idx = cur as usize;
            if let Some(waiter) = self.slots[idx]
                && waiter.task_id == task_id
            {
                count += 1;
            }
            cur = self.task_next[idx];
        }
        count
    }

    fn install(&mut self, waiter: WaitSetWaiter) {
        let idx = self.free_head as usize;
        debug_assert!(self.free_head != WAITSET_NO_SLOT && self.slots[idx].is_none());
        self.free_head = self.next_free[idx];
        self.next_free[idx] = WAITSET_NO_SLOT;
        self.slots[idx] = Some(waiter);
        self.active_count += 1;
        if waiter.observation.provider == WAITSET_PROVIDER_KERNEL {
            KERNEL_OBSERVERS.fetch_add(1, Ordering::Relaxed);
        }

        let ob = object_bucket(waiter.observation.provider, waiter.observation.object_id);
        let old_ob = self.object_heads[ob];
        self.object_prev[idx] = WAITSET_NO_SLOT;
        self.object_next[idx] = old_ob;
        if old_ob != WAITSET_NO_SLOT {
            self.object_prev[old_ob as usize] = idx as u16;
        }
        self.object_heads[ob] = idx as u16;

        let tb = task_bucket(waiter.task_id);
        let old_tb = self.task_heads[tb];
        self.task_prev[idx] = WAITSET_NO_SLOT;
        self.task_next[idx] = old_tb;
        if old_tb != WAITSET_NO_SLOT {
            self.task_prev[old_tb as usize] = idx as u16;
        }
        self.task_heads[tb] = idx as u16;
    }

    fn remove_slot(&mut self, idx: usize) {
        let Some(waiter) = self.slots[idx].take() else {
            return;
        };
        self.active_count -= 1;
        if waiter.observation.provider == WAITSET_PROVIDER_KERNEL {
            KERNEL_OBSERVERS.fetch_sub(1, Ordering::Relaxed);
        }

        let ob = object_bucket(waiter.observation.provider, waiter.observation.object_id);
        let op = self.object_prev[idx];
        let on = self.object_next[idx];
        if op != WAITSET_NO_SLOT {
            self.object_next[op as usize] = on;
        } else {
            self.object_heads[ob] = on;
        }
        if on != WAITSET_NO_SLOT {
            self.object_prev[on as usize] = op;
        }
        self.object_prev[idx] = WAITSET_NO_SLOT;
        self.object_next[idx] = WAITSET_NO_SLOT;

        let tb = task_bucket(waiter.task_id);
        let tp = self.task_prev[idx];
        let tn = self.task_next[idx];
        if tp != WAITSET_NO_SLOT {
            self.task_next[tp as usize] = tn;
        } else {
            self.task_heads[tb] = tn;
        }
        if tn != WAITSET_NO_SLOT {
            self.task_prev[tn as usize] = tp;
        }
        self.task_prev[idx] = WAITSET_NO_SLOT;
        self.task_next[idx] = WAITSET_NO_SLOT;

        self.next_free[idx] = self.free_head;
        self.free_head = idx as u16;
    }

    fn remove_for_task(&mut self, task_id: u64) -> usize {
        let mut removed = 0;
        let mut cur = self.task_heads[task_bucket(task_id)];
        while cur != WAITSET_NO_SLOT {
            let idx = cur as usize;
            let next = self.task_next[idx];
            if self.slots[idx].is_some_and(|waiter| waiter.task_id == task_id) {
                self.remove_slot(idx);
                removed += 1;
            }
            cur = next;
        }
        removed
    }

    fn remove_for_process(&mut self, process_id: u64) {
        for idx in 0..WAITSET_WAITER_CAPACITY {
            if self.slots[idx].is_some_and(|waiter| waiter.process_id == process_id) {
                self.remove_slot(idx);
            }
        }
    }
}

lazy_static! {
    static ref WAITSET_WAITERS: TrackedSpinLock<
        alloc::boxed::Box<WaiterTable>,
        { LockClass::CompatWaitset as u8 },
    > = TrackedSpinLock::new(WaiterTable::boxed());
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
    // Slot claiming is a two-word write (object id, then provider). Two
    // publishers for different objects of one provider could both pass the
    // `claimed == 0` test on the same free slot, and the CAS loser's object-id
    // store would retarget the winner's slot to a different object. The
    // registry lock serialises claims; readers stay lock-free. Nothing below
    // takes another lock.
    let _claims = WAITSET_WAITERS.lock();
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
    let existing = waiters.count_for_task(task_id);
    let free = waiters.free_count();
    if free.saturating_add(existing) < observations.len() {
        return Err(LINUX_EBUSY);
    }
    // Replacement is one bounded transaction: capacity is proved first, the
    // prior set is removed, then the complete new set is installed without an
    // allocator or fallible operation under the registry lock.
    waiters.remove_for_task(task_id);
    for observation in observations.iter().copied() {
        waiters.install(WaitSetWaiter {
            task_id,
            process_id,
            observation,
        });
    }
    drop(waiters);
    // ORDERING: pairs with the fence in `wake_local_fd_object`: either its counter load
    // sees this registration, or the caller's post-registration recheck of the
    // object sees the state change that load would otherwise have missed.
    fence(Ordering::SeqCst);
    Ok(())
}

pub(crate) fn remove_waitset_waiters(task_id: u64) -> usize {
    WAITSET_WAITERS.lock().remove_for_task(task_id)
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
    let mut cur = waiters.task_heads[task_bucket(task_id)];
    while cur != WAITSET_NO_SLOT {
        let idx = cur as usize;
        if let Some(waiter) = waiters.slots[idx] {
            if waiter.task_id == task_id && waiter.process_id == process_id {
                if !observations.contains(&waiter.observation) {
                    return false;
                }
                matched += 1;
            }
        }
        cur = waiters.task_next[idx];
    }
    matched == observations.len()
}

pub(crate) fn remove_waitset_waiters_for_process(process_id: u64) {
    WAITSET_WAITERS.lock().remove_for_process(process_id);
}

pub(crate) fn revoke_waitset_provider(service_id: u64) {
    let Some(provider) = provider_for_service(service_id) else {
        return;
    };
    bump_signal_epoch();
    wake_matching(provider, None, None);
}

pub(super) fn syscall_linux_rustos_waitset_signal_broker(args_ptr: u64) -> u64 {
    linux_unit_result((|| {
        let args: WaitSetSignalBrokerArgs = read_user_struct(args_ptr)?;
        // Published readiness packs `generation << 1 | ready` into one word, so the
        // top generation bit would be silently dropped and could regress the
        // monotonic state.
        if !waitset_signal_shape_valid(&args) || args.generation >> 63 != 0 {
            return Err(LINUX_EINVAL);
        }
        let service_id = service_for_provider(args.provider).ok_or(LINUX_EINVAL)?;
        let process_id = multitask::current_user_process_id().ok_or(LINUX_EPERM)?;
        if !ipc_ops::process_owns_live_service_endpoint(process_id, service_id) {
            return Err(LINUX_EPERM);
        }
        publish_provider_readiness(
            args.provider,
            args.object_id,
            args.generation,
            args.flags & WAITSET_SIGNAL_FLAG_READY != 0,
        );
        bump_signal_epoch();
        wake_matching(args.provider, Some(args.object_id), Some(args.generation));
        Ok(())
    })())
}

/// Wakes every waiter armed on kernel-local fd object `token` at a generation
/// older than `generation`. The caller publishes this after releasing the
/// object's own lock. Waiters are removed before they are woken (the same
/// contract as provider signals); a woken task re-registers if it still has to
/// wait.
pub(crate) fn wake_local_fd_object(token: u64, generation: u64) {
    const BATCH: usize = 8;
    // Bump the global signal epoch before checking `KERNEL_OBSERVERS` so a
    // waiter racing into `register_waitset_waiters` observes the epoch change
    // even if its observer count was not yet visible here.
    bump_signal_epoch();
    if KERNEL_OBSERVERS.load(Ordering::Relaxed) == 0 {
        return;
    }
    loop {
        let mut task_ids = [0_u64; BATCH];
        let (count, more) = take_local_waiters(token, generation, &mut task_ids);
        for task_id in task_ids.into_iter().take(count) {
            let _ = multitask::wake_task(task_id);
        }
        if !more {
            return;
        }
    }
}

/// Removes up to `task_ids.len()` distinct tasks with a matching kernel-local
/// observation; `more` reports that another distinct task still matches.
fn take_local_waiters(token: u64, generation: u64, task_ids: &mut [u64]) -> (usize, bool) {
    let mut count = 0_usize;
    let mut more = false;
    let mut waiters = WAITSET_WAITERS.lock();
    let mut cur = waiters.object_heads[object_bucket(WAITSET_PROVIDER_KERNEL, token)];
    while cur != WAITSET_NO_SLOT {
        let idx = cur as usize;
        let next = waiters.object_next[idx];
        if let Some(waiter) = waiters.slots[idx] {
            let observation = waiter.observation;
            if observation.provider == WAITSET_PROVIDER_KERNEL
                && observation.object_id == token
                && generation_advances(observation.generation, generation)
                && !task_ids[..count].contains(&waiter.task_id)
            {
                if count == task_ids.len() {
                    more = true;
                    break;
                }
                task_ids[count] = waiter.task_id;
                count += 1;
            }
        }
        cur = next;
    }
    for &task_id in &task_ids[..count] {
        waiters.remove_for_task(task_id);
    }
    (count, more)
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
        if let Some(object) = object_id {
            let mut cur = waiters.object_heads[object_bucket(provider, object)];
            while cur != WAITSET_NO_SLOT {
                let idx = cur as usize;
                let next = waiters.object_next[idx];
                if let Some(waiter) = waiters.slots[idx] {
                    let observation = waiter.observation;
                    if observation.provider == provider
                        && observation.object_id == object
                        && generation
                            .is_none_or(|value| generation_advances(observation.generation, value))
                        && !task_ids[..count].contains(&waiter.task_id)
                    {
                        task_ids[count] = waiter.task_id;
                        count += 1;
                    }
                }
                cur = next;
            }
        } else {
            for waiter in waiters.slots.iter().flatten() {
                let observation = waiter.observation;
                if observation.provider == provider
                    && generation
                        .is_none_or(|value| generation_advances(observation.generation, value))
                    && !task_ids[..count].contains(&waiter.task_id)
                {
                    task_ids[count] = waiter.task_id;
                    count += 1;
                }
            }
        }
        for &task_id in &task_ids[..count] {
            waiters.remove_for_task(task_id);
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
        service_for_provider, take_local_waiters, take_matching_waiters, waitset_waiters_match,
    };
    use rustos_user_abi::syscall::{
        WAITSET_PROVIDER_KERNEL, INPUTD_ACCESS_EVDEV, IPC_SERVICE_INPUTD, IPC_SERVICE_NETD, IPC_SERVICE_SESSIOND,
        IPC_SERVICE_VFSD, WAITSET_PROVIDER_INPUTD, WAITSET_PROVIDER_NETD,
        WAITSET_PROVIDER_SESSIOND, WAITSET_PROVIDER_VFSD,
    };
    use spin::Mutex;

    static WAITSET_TEST_GUARD: Mutex<()> = Mutex::new(());

    #[test]
    fn concurrent_publications_never_retarget_a_claimed_readiness_slot() {
        extern crate std;
        let _guard = WAITSET_TEST_GUARD.lock();
        let publishers = [0_u64, 1, 2, 3].map(|lane| {
            std::thread::spawn(move || {
                for step in 0..3_u64 {
                    let object = 0x7e57_0000 + lane * 3 + step;
                    super::publish_provider_readiness(
                        WAITSET_PROVIDER_SESSIOND,
                        object,
                        100 + object,
                        object % 2 == 0,
                    );
                }
            })
        });
        for publisher in publishers {
            publisher.join().unwrap();
        }
        for object in 0x7e57_0000_u64..0x7e57_0000 + 12 {
            assert_eq!(
                super::published_provider_readiness(WAITSET_PROVIDER_SESSIOND, object),
                Some((object % 2 == 0, 100 + object)),
                "object {object:#x} lost or aliased its own readiness"
            );
        }
    }

    #[test]
    fn readiness_generation_requires_a_strict_monotonic_advance() {
        assert!(!generation_advances(0, 1));
        assert!(!generation_advances(7, 7));
        assert!(!generation_advances(7, 6));
        assert!(generation_advances(7, 8));
    }

    #[test]
    fn waiter_capacity_admits_one_maximal_arm_and_bounded_concurrency() {
        struct WaitsetCleanup([u64; 2]);

        impl Drop for WaitsetCleanup {
            fn drop(&mut self) {
                for task_id in self.0 {
                    super::remove_waitset_waiters(task_id);
                }
            }
        }

        let _guard = WAITSET_TEST_GUARD.lock();
        let task_a = u64::MAX - 21;
        let task_b = u64::MAX - 22;
        let process_a = u64::MAX - 23;
        let process_b = u64::MAX - 24;
        let first = (0..super::WAITSET_MAX_OBSERVATIONS)
            .map(|index| ProviderObservation {
                provider: WAITSET_PROVIDER_NETD,
                object_id: index as u64 + 1,
                generation: 8,
            })
            .collect::<alloc::vec::Vec<_>>();
        let second = [ProviderObservation {
            provider: WAITSET_PROVIDER_NETD,
            object_id: u64::MAX - 52,
            generation: 9,
        }];
        let _cleanup = WaitsetCleanup([task_a, task_b]);

        register_waitset_waiters(task_a, process_a, &first).expect("arm one maximal waiter set");
        register_waitset_waiters(task_b, process_b, &second).expect("arm second waiter");
        assert!(waitset_waiters_match(task_a, process_a, &first));
        assert!(waitset_waiters_match(task_b, process_b, &second));

        let mut wake_tasks = [0_u64; WAITSET_WAITER_CAPACITY];
        let count = take_matching_waiters(
            WAITSET_PROVIDER_NETD,
            Some(first[0].object_id),
            Some(9),
            &mut wake_tasks,
        );
        assert_eq!(&wake_tasks[..count], &[task_a]);
        assert!(!waitset_waiters_match(task_a, process_a, &first));
        assert!(waitset_waiters_match(task_b, process_b, &second));
    }

    #[test]
    fn waitset_capacity_preflight_allows_exact_replacement_when_full() {
        struct WaitsetCleanup(alloc::vec::Vec<u64>);

        impl Drop for WaitsetCleanup {
            fn drop(&mut self) {
                for task_id in self.0.iter().copied() {
                    super::remove_waitset_waiters(task_id);
                }
            }
        }

        let _guard = WAITSET_TEST_GUARD.lock();
        let mut cleanup = WaitsetCleanup(alloc::vec::Vec::new());
        let mut remaining = WAITSET_WAITER_CAPACITY;
        let mut first = None;
        while remaining != 0 {
            let count = remaining.min(super::WAITSET_MAX_OBSERVATIONS);
            let task_id = u64::MAX - 1_000 - cleanup.0.len() as u64;
            let process_id = task_id - 100;
            let observations = (0..count)
                .map(|index| ProviderObservation {
                    provider: WAITSET_PROVIDER_NETD,
                    object_id: index as u64 + 1,
                    generation: task_id,
                })
                .collect::<alloc::vec::Vec<_>>();
            register_waitset_waiters(task_id, process_id, &observations)
                .expect("fill exact waiter capacity");
            if first.is_none() {
                first = Some((task_id, process_id, observations.clone()));
            }
            cleanup.0.push(task_id);
            remaining -= count;
        }
        assert_eq!(
            super::WAITSET_WAITERS.lock().active_count,
            WAITSET_WAITER_CAPACITY
        );

        let (task_id, process_id, observations) = first.expect("one exact-capacity waiter set");
        register_waitset_waiters(task_id, process_id, &observations)
            .expect("an exact replacement owns its existing capacity");
        assert!(waitset_waiters_match(task_id, process_id, &observations));
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
    fn waitset_match_requires_exact_task_process_pair() {
        struct WaitsetCleanup(u64);

        impl Drop for WaitsetCleanup {
            fn drop(&mut self) {
                super::remove_waitset_waiters(self.0);
            }
        }

        let _guard = WAITSET_TEST_GUARD.lock();
        let task_a = u64::MAX - 111;
        let task_b = u64::MAX - 112;
        let process_a = u64::MAX - 113;
        let process_b = u64::MAX - 114;
        let observations = [ProviderObservation {
            provider: WAITSET_PROVIDER_NETD,
            object_id: 43,
            generation: 9,
        }];
        let _cleanup = WaitsetCleanup(task_a);

        register_waitset_waiters(task_a, process_a, &observations)
            .expect("register exact task/process waiter");
        assert!(!waitset_waiters_match(task_a, process_b, &observations));
        assert!(!waitset_waiters_match(task_b, process_a, &observations));
        assert!(waitset_waiters_match(task_a, process_a, &observations));

        // Only a matching commit may consume the waiter and nominate its task
        // for scheduler wakeup. The two cross-pairs leave it installed.
        let mut wake_tasks = [0_u64; super::WAITSET_WAITER_CAPACITY];
        let count =
            take_matching_waiters(WAITSET_PROVIDER_NETD, Some(43), Some(10), &mut wake_tasks);
        assert_eq!(&wake_tasks[..count], &[task_a]);
        assert!(!waitset_waiters_match(task_a, process_a, &observations));
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
        let colliding_object = (42..)
            .find(|&id| {
                super::object_bucket(WAITSET_PROVIDER_NETD, id)
                    == super::object_bucket(WAITSET_PROVIDER_NETD, 41)
            })
            .expect("hash bucket collision for object 41");
        let second = [ProviderObservation {
            provider: WAITSET_PROVIDER_NETD,
            object_id: colliding_object,
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

    #[test]
    fn kernel_local_waiters_are_counted_matched_and_removed_exactly() {
        use core::sync::atomic::Ordering;
        let _guard = WAITSET_TEST_GUARD.lock();
        let (task_a, task_b, task_c, process) =
            (u64::MAX - 61, u64::MAX - 62, u64::MAX - 63, u64::MAX - 64);
        let token = u64::MAX - 65;
        let before = super::KERNEL_OBSERVERS.load(Ordering::Relaxed);
        let at = |generation| {
            [ProviderObservation {
                provider: WAITSET_PROVIDER_KERNEL,
                object_id: token,
                generation,
            }]
        };
        register_waitset_waiters(task_a, process, &at(4)).unwrap();
        register_waitset_waiters(task_b, process, &at(9)).unwrap();
        // A foreign-provider observation with the same object id never matches.
        register_waitset_waiters(
            task_c,
            process,
            &[ProviderObservation {
                provider: WAITSET_PROVIDER_NETD,
                object_id: token,
                generation: 1,
            }],
        )
        .unwrap();
        assert_eq!(super::KERNEL_OBSERVERS.load(Ordering::Relaxed), before + 2);

        let mut woken = [0_u64; 8];
        // Generation 9 advances task A (observed 4) but not task B (observed 9).
        let (count, more) = take_local_waiters(token, 9, &mut woken);
        assert_eq!((count, more, woken[0]), (1, false, task_a));
        assert_eq!(super::KERNEL_OBSERVERS.load(Ordering::Relaxed), before + 1);
        assert!(!waitset_waiters_match(task_a, process, &at(4)), "woken waiters are removed");
        assert!(waitset_waiters_match(task_b, process, &at(9)), "an equal generation is not a wake");
        assert!(waitset_waiters_match(task_c, process, &[ProviderObservation {
            provider: WAITSET_PROVIDER_NETD,
            object_id: token,
            generation: 1,
        }]));

        // Re-registering the same task replaces its observation without
        // double-counting.
        register_waitset_waiters(task_b, process, &at(9)).unwrap();
        assert_eq!(super::KERNEL_OBSERVERS.load(Ordering::Relaxed), before + 1);
        remove_waitset_waiters(task_b);
        remove_waitset_waiters(task_c);
        assert_eq!(super::KERNEL_OBSERVERS.load(Ordering::Relaxed), before);
    }

    #[test]
    fn kernel_local_wake_batches_report_when_more_waiters_remain() {
        let _guard = WAITSET_TEST_GUARD.lock();
        let process = u64::MAX - 70;
        let token = u64::MAX - 71;
        let tasks = [u64::MAX - 72, u64::MAX - 73, u64::MAX - 74];
        for task in tasks {
            register_waitset_waiters(
                task,
                process,
                &[ProviderObservation {
                    provider: WAITSET_PROVIDER_KERNEL,
                    object_id: token,
                    generation: 1,
                }],
            )
            .unwrap();
        }
        let mut first = [0_u64; 2];
        let (count, more) = take_local_waiters(token, 2, &mut first);
        assert_eq!((count, more), (2, true));
        let mut second = [0_u64; 2];
        let (count, more) = take_local_waiters(token, 2, &mut second);
        assert_eq!((count, more), (1, false));
        for task in tasks {
            remove_waitset_waiters(task);
        }
    }

    #[test]
    fn userspace_cannot_signal_the_kernel_provider() {
        assert_eq!(service_for_provider(WAITSET_PROVIDER_KERNEL), None);
    }
}
