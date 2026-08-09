//! Kernel-owned admission for the private Ring3 SMP qualification workload.
//!
//! `sessiond` may bind exactly one of its still-suspended deferred children
//! before `PROC_ACTIVATE`. Ring0, not the debug payload, owns the process/mm
//! identities, service endpoint epoch, immutable work/deadline, per-worker
//! phase state, and evidence binding ID. No caller-supplied PID, path, CPU, or
//! work value can refresh that authority after the bind linearization point.

use super::*;
use core::sync::atomic::{AtomicU64, Ordering};
use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};
use rustos_user_abi::syscall::{
    RustosSmpQualificationBindArgs, SMP_QUALIFICATION_MAX_WORKERS, SMP_QUALIFICATION_WORK_BITS,
    SMP_QUALIFICATION_WORK_MASK,
};

const SMP_QUALIFICATION_EXEC_PATH: &str = "apps/smpqual/smpqual.elf";
const SMP_QUALIFICATION_BINDING_ID_MAX: u64 = u64::MAX >> SMP_QUALIFICATION_WORK_BITS;

static NEXT_SMP_QUALIFICATION_BINDING_ID: AtomicU64 = AtomicU64::new(1);

pub(super) fn smp_qualification_exec_path_matches(exec_path: &str) -> bool {
    exec_path == SMP_QUALIFICATION_EXEC_PATH
}

/// This registry is intentionally separate from the process broker registry:
/// activate acquires ProcBrokerRegistry before this class, while debug phase
/// admission never holds process/service state locks during debug output.
static SMP_QUALIFICATION_BINDING: TrackedSpinLock<
    SmpQualificationRegistry,
    { LockClass::ServiceEndpointRegistry as u8 },
> = TrackedSpinLock::new(SmpQualificationRegistry::empty());

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct QualificationIdentity {
    process_id: u64,
    process_generation: u32,
    mm_generation: u32,
}

impl From<multitask::ProcessIdentity> for QualificationIdentity {
    fn from(identity: multitask::ProcessIdentity) -> Self {
        Self {
            process_id: identity.process_id(),
            process_generation: identity.process_generation(),
            mm_generation: identity.mm_generation(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkerPhase {
    Empty,
    Ready,
    Started,
    Finished,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WorkerBinding {
    tid: u64,
    cpu: u32,
    phase: WorkerPhase,
}

impl WorkerBinding {
    const EMPTY: Self = Self {
        tid: 0,
        cpu: 0,
        phase: WorkerPhase::Empty,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BindingState {
    BoundSuspended,
    Active { deadline_tick: u64 },
    Terminal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SmpQualificationBinding {
    binding_id: u64,
    owner: QualificationIdentity,
    owner_endpoint_epoch: u64,
    target: QualificationIdentity,
    workers: u32,
    work_units: u64,
    deadline_ms: u32,
    state: BindingState,
    worker: [WorkerBinding; SMP_QUALIFICATION_MAX_WORKERS as usize],
}

#[derive(Debug)]
struct SmpQualificationRegistry {
    binding: Option<SmpQualificationBinding>,
}

impl SmpQualificationRegistry {
    const fn empty() -> Self {
        Self { binding: None }
    }
}

fn install_bound_smp_qualification(
    registry: &mut SmpQualificationRegistry,
    binding: SmpQualificationBinding,
) -> Result<(), i64> {
    if registry
        .binding
        .as_ref()
        .is_some_and(|existing| existing.state != BindingState::Terminal)
    {
        return Err(LINUX_EBUSY);
    }
    registry.binding = Some(binding);
    Ok(())
}

impl SmpQualificationBinding {
    fn new(
        binding_id: u64,
        owner: QualificationIdentity,
        owner_endpoint_epoch: u64,
        target: QualificationIdentity,
        args: &RustosSmpQualificationBindArgs,
    ) -> Self {
        Self {
            binding_id,
            owner,
            owner_endpoint_epoch,
            target,
            workers: args.workers,
            work_units: args.work_units,
            deadline_ms: args.deadline_ms,
            state: BindingState::BoundSuspended,
            worker: [WorkerBinding::EMPTY; SMP_QUALIFICATION_MAX_WORKERS as usize],
        }
    }

    fn target_matches(&self, target: QualificationIdentity) -> bool {
        self.target == target
    }

    fn target_pid_matches(&self, target_pid: u64) -> bool {
        self.target.process_id == target_pid
    }

    fn active_endpoint_matches(&self, owner: QualificationIdentity, endpoint_epoch: u64) -> bool {
        self.owner == owner && self.owner_endpoint_epoch == endpoint_epoch
    }

    fn activate(
        &mut self,
        owner: QualificationIdentity,
        endpoint_epoch: u64,
        target: QualificationIdentity,
        now_tick: u64,
        ticks_per_second: u64,
    ) -> Result<(), i64> {
        if !self.target_matches(target) || !self.active_endpoint_matches(owner, endpoint_epoch) {
            self.state = BindingState::Terminal;
            return Err(LINUX_EPERM);
        }
        if self.state != BindingState::BoundSuspended {
            return Err(LINUX_EBUSY);
        }
        let Some(deadline_tick) =
            qualification_deadline_tick(now_tick, ticks_per_second, self.deadline_ms)
        else {
            self.state = BindingState::Terminal;
            return Err(LINUX_ETIMEDOUT);
        };
        self.state = BindingState::Active { deadline_tick };
        Ok(())
    }

    fn terminate(&mut self) {
        self.state = BindingState::Terminal;
    }

    fn admit_phase(
        &mut self,
        milestone: u64,
        target: QualificationIdentity,
        owner: QualificationIdentity,
        endpoint_epoch: u64,
        packed_worker: u64,
        supplied_work_units: u64,
        current_cpu: u32,
        current_tid: u64,
        now_tick: u64,
    ) -> Result<u64, i64> {
        if self.target_pid_matches(target.process_id) && !self.target_matches(target) {
            self.terminate();
            return Err(LINUX_EPERM);
        }
        if !self.target_matches(target) {
            return Err(LINUX_EPERM);
        }
        if !self.active_endpoint_matches(owner, endpoint_epoch) {
            self.terminate();
            return Err(LINUX_EPERM);
        }
        let deadline_tick = match self.state {
            BindingState::Active { deadline_tick } => deadline_tick,
            BindingState::BoundSuspended => return Err(LINUX_EBUSY),
            BindingState::Terminal => return Err(LINUX_EPERM),
        };
        if now_tick >= deadline_tick {
            self.terminate();
            return Err(LINUX_ETIMEDOUT);
        }
        if !linux_abi::smp_qualification_worker_shape_valid(
            packed_worker,
            supplied_work_units,
            current_cpu,
        ) || supplied_work_units != self.work_units
            || current_tid == 0
        {
            return Err(LINUX_EINVAL);
        }
        let (observed_cpu, worker_id) = linux_abi::unpack_smp_qualification_worker(packed_worker);
        if observed_cpu != worker_id || worker_id >= self.workers {
            return Err(LINUX_EINVAL);
        }
        let worker_index = worker_id as usize;
        match milestone {
            linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_READY => {
                if self.worker[worker_index].phase != WorkerPhase::Empty
                    || self.worker[..self.workers as usize]
                        .iter()
                        .any(|known| known.tid == current_tid && known.phase != WorkerPhase::Empty)
                {
                    return Err(LINUX_EINVAL);
                }
                self.worker[worker_index] = WorkerBinding {
                    tid: current_tid,
                    cpu: observed_cpu,
                    phase: WorkerPhase::Ready,
                };
            }
            linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_START => {
                if !self.ready_barrier_complete()
                    || self.worker[worker_index].phase != WorkerPhase::Ready
                    || self.worker[worker_index].tid != current_tid
                    || self.worker[worker_index].cpu != observed_cpu
                {
                    return Err(LINUX_EINVAL);
                }
                self.worker[worker_index].phase = WorkerPhase::Started;
            }
            linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_FINISH => {
                if self.worker[worker_index].phase != WorkerPhase::Started
                    || self.worker[worker_index].tid != current_tid
                    || self.worker[worker_index].cpu != observed_cpu
                {
                    return Err(LINUX_EINVAL);
                }
                self.worker[worker_index].phase = WorkerPhase::Finished;
            }
            linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_COMPLETE => {
                if worker_id != 0
                    || self.worker[worker_index].phase != WorkerPhase::Finished
                    || self.worker[worker_index].tid != current_tid
                    || self.worker[worker_index].cpu != observed_cpu
                    || !self.all_workers_finished()
                {
                    return Err(LINUX_EINVAL);
                }
                self.terminate();
            }
            _ => return Err(LINUX_EINVAL),
        }
        encode_binding_work(self.binding_id, self.work_units).ok_or(LINUX_EINVAL)
    }

    fn ready_barrier_complete(&self) -> bool {
        self.worker[..self.workers as usize]
            .iter()
            .all(|worker| matches!(worker.phase, WorkerPhase::Ready | WorkerPhase::Started))
    }

    fn all_workers_finished(&self) -> bool {
        self.worker[..self.workers as usize]
            .iter()
            .all(|worker| worker.phase == WorkerPhase::Finished)
    }
}

/// Strict, pure conversion of a millisecond deadline into the monotonic RTC
/// domain. The MMIO clock is sampled by the caller; this helper contains every
/// overflow and rounding decision and never refreshes an existing deadline.
fn qualification_deadline_tick(
    now_tick: u64,
    ticks_per_second: u64,
    deadline_ms: u32,
) -> Option<u64> {
    if deadline_ms == 0 || deadline_ms > linux_abi::SMP_QUALIFICATION_MAX_DEADLINE_MS {
        return None;
    }
    let ticks = u128::from(deadline_ms)
        .checked_mul(u128::from(ticks_per_second.max(1)))?
        .checked_add(999)?
        / 1_000;
    now_tick.checked_add(u64::try_from(ticks).ok()?)
}

fn allocate_binding_id() -> Option<u64> {
    NEXT_SMP_QUALIFICATION_BINDING_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            (current != 0 && current <= SMP_QUALIFICATION_BINDING_ID_MAX)
                .then(|| current.checked_add(1))
                .flatten()
        })
        .ok()
}

fn encode_binding_work(binding_id: u64, work_units: u64) -> Option<u64> {
    (binding_id != 0
        && binding_id <= SMP_QUALIFICATION_BINDING_ID_MAX
        && work_units != 0
        && work_units <= SMP_QUALIFICATION_WORK_MASK)
        .then_some((binding_id << SMP_QUALIFICATION_WORK_BITS) | work_units)
}

#[cfg(test)]
fn decode_binding_work(value: u64) -> (u64, u64) {
    (
        value >> SMP_QUALIFICATION_WORK_BITS,
        value & SMP_QUALIFICATION_WORK_MASK,
    )
}

fn qualification_phase(milestone: u64) -> bool {
    matches!(
        milestone,
        linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_READY
            | linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_START
            | linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_FINISH
            | linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_COMPLETE
    )
}

/// A phase is linearized only while the exact `sessiond` endpoint generation
/// remains live. The second observation is deliberately made after the FSM
/// lock is released and before debug output, so it cannot invert the
/// endpoint-lock -> binding-lock order or serialize output under either lock.
fn live_sessiond_endpoint_matches(owner: QualificationIdentity, epoch: u64) -> bool {
    let Some((endpoint_owner_pid, observed_epoch)) =
        ipc_ops::live_service_endpoint_owner_and_epoch(IPC_SERVICE_SESSIOND)
    else {
        return false;
    };
    endpoint_snapshot_is_exact(
        owner,
        epoch,
        multitask::live_user_process_identity_by_pid(endpoint_owner_pid)
            .map(QualificationIdentity::from)
            .zip(Some(observed_epoch)),
    )
}

fn endpoint_snapshot_is_exact(
    expected_owner: QualificationIdentity,
    expected_epoch: u64,
    observed: Option<(QualificationIdentity, u64)>,
) -> bool {
    observed == Some((expected_owner, expected_epoch))
}

fn terminalize_binding_after_endpoint_revalidation(binding_id: u64, target: QualificationIdentity) {
    let mut registry = SMP_QUALIFICATION_BINDING.lock();
    if let Some(binding) = registry.binding.as_mut()
        && binding.binding_id == binding_id
        && binding.target_matches(target)
    {
        binding.terminate();
    }
}

pub(super) fn syscall_linux_rustos_smp_qualification_bind(args_ptr: u64) -> u64 {
    let args = match usermem::read_current_user_struct::<RustosSmpQualificationBindArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if !linux_abi::smp_qualification_bind_shape_valid(&args) {
        return linux_errno(LINUX_EINVAL);
    }
    let Some(owner) = multitask::current_user_process_identity() else {
        return linux_errno(LINUX_ESRCH);
    };
    let owner_identity = QualificationIdentity::from(owner);
    let Some((_, endpoint_epoch)) =
        ipc_ops::live_service_endpoint_owner_and_epoch(IPC_SERVICE_SESSIOND)
    else {
        return linux_errno(LINUX_EPERM);
    };
    if !live_sessiond_endpoint_matches(owner_identity, endpoint_epoch) {
        return linux_errno(LINUX_EPERM);
    }
    let Some(target) = multitask::live_user_process_identity_with_exact_exec_path(
        args.target_pid,
        SMP_QUALIFICATION_EXEC_PATH,
    ) else {
        return linux_errno(LINUX_EPERM);
    };
    let owner_process = owner;
    let target_process = target;
    let owner = owner_identity;
    let target = QualificationIdentity::from(target_process);
    match super::proc_broker_ops::with_deferred_activation_authority_for_smp_bind(
        args.target_pid,
        owner_process,
        target_process,
        || {
            let binding_id = allocate_binding_id().ok_or(LINUX_EOVERFLOW)?;
            let binding =
                SmpQualificationBinding::new(binding_id, owner, endpoint_epoch, target, &args);
            install_bound_smp_qualification(&mut SMP_QUALIFICATION_BINDING.lock(), binding)
        },
    ) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

/// Called while proc activation owns the deferred-child one-shot authority.
/// It starts the immutable absolute deadline before scheduler publication; the
/// suspended target cannot emit evidence until that publication commits.
pub(super) fn prepare_smp_qualification_activation(
    owner: multitask::ProcessIdentity,
    target: multitask::ProcessIdentity,
    qualification_required: bool,
) -> Result<bool, i64> {
    // Ordinary deferred children must not acquire a dependency on SESSIOND.
    // The kernel-derived flag is immutable in the ProcBroker authority, so
    // only the reserved private workload proceeds into binding validation.
    if !qualification_required {
        return Ok(false);
    }
    let owner = QualificationIdentity::from(owner);
    let target = QualificationIdentity::from(target);
    let Some((endpoint_owner_pid, endpoint_epoch)) =
        ipc_ops::published_service_endpoint_owner_and_epoch(IPC_SERVICE_SESSIOND)
    else {
        return Err(LINUX_EPERM);
    };
    let ticks_per_second = crate::arch::rtc::ticks_per_second();
    let mut registry = SMP_QUALIFICATION_BINDING.lock();
    let Some(binding) = registry.binding.as_mut() else {
        return activation_without_matching_binding(qualification_required);
    };
    if !binding.target_pid_matches(target.process_id) {
        return activation_without_matching_binding(qualification_required);
    }
    if endpoint_owner_pid != owner.process_id {
        binding.terminate();
        return Err(LINUX_EPERM);
    }
    let now_tick = crate::arch::rtc::ticks();
    binding.activate(owner, endpoint_epoch, target, now_tick, ticks_per_second)?;
    Ok(true)
}

fn activation_without_matching_binding(qualification_required: bool) -> Result<bool, i64> {
    if qualification_required {
        Err(LINUX_EPERM)
    } else {
        Ok(false)
    }
}

/// Roll back a deadline armed for a scheduler activation that failed before
/// runnable publication. Terminal state makes the failed one-shot authority
/// unobservable and permits only a fresh bind of a fresh suspended child.
pub(super) fn abort_smp_qualification_activation(target: multitask::ProcessIdentity) {
    let target = QualificationIdentity::from(target);
    let mut registry = SMP_QUALIFICATION_BINDING.lock();
    if let Some(binding) = registry.binding.as_mut()
        && binding.target_matches(target)
        && matches!(binding.state, BindingState::Active { .. })
    {
        binding.terminate();
    }
}

/// Admit one debug milestone and return the kernel-stamped encoded work word.
/// This function never emits debug output while its binding lock is held.
pub(super) fn admit_smp_qualification_milestone(
    milestone: u64,
    packed_worker: u64,
    supplied_work_units: u64,
    current_cpu: usize,
) -> Result<u64, i64> {
    if !qualification_phase(milestone) {
        return Err(LINUX_EINVAL);
    }
    let current_cpu = u32::try_from(current_cpu).map_err(|_| LINUX_EINVAL)?;
    let Some((current_pid, current_tid)) = multitask::current_user_log_ids() else {
        return Err(LINUX_ESRCH);
    };
    let Some(target) = multitask::current_user_process_identity() else {
        return Err(LINUX_ESRCH);
    };
    if target.process_id() != current_pid {
        return Err(LINUX_EPERM);
    }
    let Some((_, endpoint_epoch)) =
        ipc_ops::live_service_endpoint_owner_and_epoch(IPC_SERVICE_SESSIOND)
    else {
        return Err(LINUX_EPERM);
    };
    let target = QualificationIdentity::from(target);
    let (encoded_work, binding_id, owner) = {
        let mut registry = SMP_QUALIFICATION_BINDING.lock();
        let Some(binding) = registry.binding.as_mut() else {
            return Err(LINUX_EPERM);
        };
        // The deadline observation and phase mutation share this lock-held
        // linearization point; queueing for the registry cannot turn a late
        // phase into an accepted pre-deadline observation.
        let now_tick = crate::arch::rtc::ticks();
        let encoded_work = binding.admit_phase(
            milestone,
            target,
            binding.owner,
            endpoint_epoch,
            packed_worker,
            supplied_work_units,
            current_cpu,
            current_tid,
            now_tick,
        )?;
        (encoded_work, binding.binding_id, binding.owner)
    };
    if !live_sessiond_endpoint_matches(owner, endpoint_epoch) {
        terminalize_binding_after_endpoint_revalidation(binding_id, target);
        return Err(LINUX_EPERM);
    }
    Ok(encoded_work)
}

/// Process teardown calls this after revoking service endpoints and deferred
/// activation records. It deliberately matches a PID only for cleanup: the
/// process table has already linearized termination, so no new generation can
/// inherit the old binding.
pub(super) fn revoke_smp_qualification_for_process(process_id: u64) {
    if process_id == 0 {
        return;
    }
    let mut registry = SMP_QUALIFICATION_BINDING.lock();
    if let Some(binding) = registry.binding.as_mut()
        && (binding.owner.process_id == process_id || binding.target.process_id == process_id)
    {
        binding.terminate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORK: u64 = 1_000_000;

    fn identity(process_id: u64) -> QualificationIdentity {
        QualificationIdentity {
            process_id,
            process_generation: 7,
            mm_generation: 11,
        }
    }

    fn args(workers: u32) -> RustosSmpQualificationBindArgs {
        RustosSmpQualificationBindArgs {
            abi_version: linux_abi::SMP_QUALIFICATION_BIND_ABI_VERSION,
            target_pid: 41,
            workers,
            work_units: WORK,
            deadline_ms: 5_000,
            ..RustosSmpQualificationBindArgs::default()
        }
    }

    fn active_binding(workers: u32) -> SmpQualificationBinding {
        let mut binding =
            SmpQualificationBinding::new(9, identity(1), 3, identity(41), &args(workers));
        binding
            .activate(identity(1), 3, identity(41), 100, 1_000)
            .expect("activate exact suspended binding");
        binding
    }

    fn admit(
        binding: &mut SmpQualificationBinding,
        milestone: u64,
        worker: u32,
        tid: u64,
        now: u64,
    ) -> Result<u64, i64> {
        binding.admit_phase(
            milestone,
            identity(41),
            identity(1),
            3,
            linux_abi::pack_smp_qualification_worker(worker, worker),
            WORK,
            worker,
            tid,
            now,
        )
    }

    #[test]
    fn private_exec_and_missing_binding_activation_are_fail_closed() {
        assert!(smp_qualification_exec_path_matches(
            "apps/smpqual/smpqual.elf"
        ));
        assert!(!smp_qualification_exec_path_matches(
            "/apps/smpqual/smpqual.elf"
        ));
        assert_eq!(activation_without_matching_binding(true), Err(LINUX_EPERM));
        assert_eq!(activation_without_matching_binding(false), Ok(false));

        let source = include_str!("smp_qualification_ops.rs");
        let activation = source
            .split("pub(super) fn prepare_smp_qualification_activation")
            .nth(1)
            .and_then(|rest| {
                rest.split("pub(super) fn abort_smp_qualification_activation")
                    .next()
            })
            .expect("qualification activation preparation");
        let ordinary_guard = activation
            .find("if !qualification_required")
            .expect("ordinary activation bypass");
        let endpoint_lookup = activation
            .find("published_service_endpoint_owner_and_epoch")
            .expect("qualification endpoint check");
        assert!(ordinary_guard < endpoint_lookup);
    }

    #[test]
    fn exact_worker_topologies_bind_and_complete_once() {
        for workers in [1, 2, 4, 8] {
            let mut binding = active_binding(workers);
            for worker in 0..workers {
                assert!(
                    admit(
                        &mut binding,
                        linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_READY,
                        worker,
                        100 + worker as u64,
                        101,
                    )
                    .is_ok()
                );
            }
            for worker in 0..workers {
                assert!(
                    admit(
                        &mut binding,
                        linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_START,
                        worker,
                        100 + worker as u64,
                        102,
                    )
                    .is_ok()
                );
            }
            for worker in 0..workers {
                assert!(
                    admit(
                        &mut binding,
                        linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_FINISH,
                        worker,
                        100 + worker as u64,
                        103,
                    )
                    .is_ok()
                );
            }
            if workers > 1 {
                let before = binding.clone();
                assert_eq!(
                    admit(
                        &mut binding,
                        linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_COMPLETE,
                        1,
                        101,
                        104,
                    ),
                    Err(LINUX_EINVAL)
                );
                assert_eq!(binding, before);
            }
            let encoded = admit(
                &mut binding,
                linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_COMPLETE,
                0,
                100,
                104,
            )
            .expect("worker zero completes after every finish");
            assert_eq!(decode_binding_work(encoded), (9, WORK));
            assert_eq!(
                admit(
                    &mut binding,
                    linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_COMPLETE,
                    0,
                    100,
                    105,
                ),
                Err(LINUX_EPERM)
            );
        }
    }

    #[test]
    fn ready_barrier_identity_and_phase_rejections_are_atomic() {
        let mut binding = active_binding(2);
        let before = binding.clone();
        assert_eq!(
            admit(
                &mut binding,
                linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_START,
                0,
                100,
                101,
            ),
            Err(LINUX_EINVAL)
        );
        assert_eq!(binding, before);

        assert_eq!(
            admit(
                &mut binding,
                linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_FINISH,
                0,
                100,
                102,
            ),
            Err(LINUX_EINVAL)
        );
        assert_eq!(binding, before);

        assert!(
            admit(
                &mut binding,
                linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_READY,
                0,
                100,
                101,
            )
            .is_ok()
        );
        let before = binding.clone();
        assert_eq!(
            admit(
                &mut binding,
                linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_START,
                0,
                100,
                102,
            ),
            Err(LINUX_EINVAL)
        );
        assert_eq!(binding, before);
        assert_eq!(
            admit(
                &mut binding,
                linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_FINISH,
                0,
                100,
                102,
            ),
            Err(LINUX_EINVAL)
        );
        assert_eq!(binding, before);
        assert_eq!(
            admit(
                &mut binding,
                linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_READY,
                1,
                100,
                102,
            ),
            Err(LINUX_EINVAL)
        );
        assert_eq!(binding, before);
        assert_eq!(
            binding.admit_phase(
                linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_READY,
                identity(41),
                identity(1),
                3,
                linux_abi::pack_smp_qualification_worker(3, 1),
                WORK,
                1,
                101,
                102,
            ),
            Err(LINUX_EINVAL)
        );
        assert_eq!(binding, before);
    }

    #[test]
    fn immutable_work_deadline_and_endpoint_generation_fail_closed() {
        let mut binding = active_binding(1);
        let deadline = binding.state;
        assert_eq!(
            binding.admit_phase(
                linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_READY,
                identity(41),
                identity(1),
                3,
                linux_abi::pack_smp_qualification_worker(0, 0),
                WORK - 1,
                0,
                100,
                101,
            ),
            Err(LINUX_EINVAL)
        );
        assert_eq!(binding.state, deadline);
        assert_eq!(
            binding.admit_phase(
                linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_READY,
                identity(41),
                identity(1),
                4,
                linux_abi::pack_smp_qualification_worker(0, 0),
                WORK,
                0,
                100,
                101,
            ),
            Err(LINUX_EPERM)
        );
        assert_eq!(binding.state, BindingState::Terminal);

        let mut expired = active_binding(1);
        assert_eq!(
            admit(
                &mut expired,
                linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_READY,
                0,
                100,
                5_100,
            ),
            Err(LINUX_ETIMEDOUT)
        );
        assert_eq!(expired.state, BindingState::Terminal);
    }

    #[test]
    fn pid_generation_and_mm_generation_substitution_terminally_revoke() {
        let mut binding = active_binding(1);
        let substituted_generation = QualificationIdentity {
            process_generation: 8,
            ..identity(41)
        };
        assert_eq!(
            binding.admit_phase(
                linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_READY,
                substituted_generation,
                identity(1),
                3,
                linux_abi::pack_smp_qualification_worker(0, 0),
                WORK,
                0,
                100,
                101,
            ),
            Err(LINUX_EPERM)
        );
        assert_eq!(binding.state, BindingState::Terminal);

        let mut binding = active_binding(1);
        let substituted_mm = QualificationIdentity {
            mm_generation: 12,
            ..identity(41)
        };
        assert_eq!(
            binding.admit_phase(
                linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_READY,
                substituted_mm,
                identity(1),
                3,
                linux_abi::pack_smp_qualification_worker(0, 0),
                WORK,
                0,
                100,
                101,
            ),
            Err(LINUX_EPERM)
        );
        assert_eq!(binding.state, BindingState::Terminal);
    }

    #[test]
    fn suspended_binding_cannot_admit_a_phase_before_activation() {
        let mut binding = SmpQualificationBinding::new(9, identity(1), 3, identity(41), &args(1));
        assert_eq!(
            admit(
                &mut binding,
                linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_READY,
                0,
                100,
                101,
            ),
            Err(LINUX_EBUSY)
        );
        assert_eq!(binding.state, BindingState::BoundSuspended);
    }

    #[test]
    fn deadline_conversion_is_pure_and_never_refreshes() {
        assert_eq!(qualification_deadline_tick(10, 1_000, 5_000), Some(5_010));
        assert_eq!(qualification_deadline_tick(u64::MAX, 1_000, 1), None);
        assert_eq!(qualification_deadline_tick(10, 1_000, 0), None);
        assert_eq!(
            qualification_deadline_tick(
                10,
                1_000,
                linux_abi::SMP_QUALIFICATION_MAX_DEADLINE_MS + 1
            ),
            None
        );
    }

    #[test]
    fn bind_after_active_is_rejected_without_replacing_the_binding() {
        let mut registry = SmpQualificationRegistry {
            binding: Some(active_binding(1)),
        };
        let before = registry.binding.clone();
        assert_eq!(
            install_bound_smp_qualification(
                &mut registry,
                SmpQualificationBinding::new(10, identity(1), 3, identity(42), &args(1)),
            ),
            Err(LINUX_EBUSY)
        );
        assert_eq!(registry.binding, before);
    }

    #[test]
    fn post_admission_endpoint_revalidation_rejects_revoke_and_terminalizes() {
        let mut binding = active_binding(1);
        assert!(
            admit(
                &mut binding,
                linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_READY,
                0,
                100,
                101,
            )
            .is_ok()
        );
        assert!(!endpoint_snapshot_is_exact(
            identity(1),
            3,
            Some((identity(1), 4))
        ));
        binding.terminate();
        assert_eq!(binding.state, BindingState::Terminal);
    }

    #[test]
    fn complete_cannot_precede_every_worker_finish() {
        let mut binding = active_binding(2);
        for worker in 0..2 {
            assert!(
                admit(
                    &mut binding,
                    linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_READY,
                    worker,
                    100 + worker as u64,
                    101,
                )
                .is_ok()
            );
        }
        for worker in 0..2 {
            assert!(
                admit(
                    &mut binding,
                    linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_START,
                    worker,
                    100 + worker as u64,
                    102,
                )
                .is_ok()
            );
        }
        assert!(
            admit(
                &mut binding,
                linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_FINISH,
                0,
                100,
                103,
            )
            .is_ok()
        );
        let before = binding.clone();
        assert_eq!(
            admit(
                &mut binding,
                linux_abi::PRODUCT_MILESTONE_SMP_QUALIFICATION_COMPLETE,
                0,
                100,
                104,
            ),
            Err(LINUX_EINVAL)
        );
        assert_eq!(binding, before);
    }

    #[test]
    fn smp_qualification_binding_requires_exact_private_exec_path() {
        let source = include_str!("smp_qualification_ops.rs");
        assert!(
            source.contains(
                "const SMP_QUALIFICATION_EXEC_PATH: &str = \"apps/smpqual/smpqual.elf\";"
            )
        );
        let bind = source
            .split("pub(super) fn syscall_linux_rustos_smp_qualification_bind")
            .nth(1)
            .and_then(|rest| {
                rest.split("pub(super) fn prepare_smp_qualification_activation")
                    .next()
            })
            .expect("qualification bind entrypoint");
        assert!(bind.contains("live_user_process_identity_with_exact_exec_path"));
        assert!(bind.contains("SMP_QUALIFICATION_EXEC_PATH"));
        assert!(!bind.contains("live_user_process_identity_by_pid(args.target_pid)"));
    }

    #[test]
    fn smp_qualification_binding_and_phase_require_kernel_identity_and_live_sessiond_epoch() {
        let source = include_str!("smp_qualification_ops.rs");
        let bind = source
            .split("pub(super) fn syscall_linux_rustos_smp_qualification_bind")
            .nth(1)
            .and_then(|rest| {
                rest.split("pub(super) fn prepare_smp_qualification_activation")
                    .next()
            })
            .expect("qualification bind entrypoint");
        let phase = source
            .split("pub(super) fn admit_smp_qualification_milestone")
            .nth(1)
            .and_then(|rest| {
                rest.split("pub(super) fn revoke_smp_qualification_for_process")
                    .next()
            })
            .expect("qualification phase admission");
        assert!(bind.contains("current_user_process_identity"));
        assert!(bind.contains("live_sessiond_endpoint_matches"));
        assert!(phase.contains("current_user_process_identity"));
        assert!(phase.contains("live_sessiond_endpoint_matches"));
        assert!(phase.contains("terminalize_binding_after_endpoint_revalidation"));
    }

    #[test]
    fn proc_locked_activation_does_not_reacquire_process_table_liveness() {
        let source = include_str!("smp_qualification_ops.rs");
        let activation = source
            .split("pub(super) fn prepare_smp_qualification_activation")
            .nth(1)
            .and_then(|rest| {
                rest.split("pub(super) fn abort_smp_qualification_activation")
                    .next()
            })
            .expect("qualification activation preparation");
        assert!(activation.contains("published_service_endpoint_owner_and_epoch"));
        assert!(!activation.contains("live_user_process_identity_by_pid"));
        assert!(!activation.contains("live_sessiond_endpoint_matches"));
    }

    #[test]
    fn bind_registration_is_linearized_with_deferred_activation_authority() {
        let bind_source = include_str!("smp_qualification_ops.rs");
        let bind = bind_source
            .split("pub(super) fn syscall_linux_rustos_smp_qualification_bind")
            .nth(1)
            .and_then(|rest| {
                rest.split("pub(super) fn prepare_smp_qualification_activation")
                    .next()
            })
            .expect("qualification bind entrypoint");
        let broker_source = include_str!("proc_broker_ops.rs");
        let transaction = broker_source
            .split("pub(super) fn with_deferred_activation_authority_for_smp_bind")
            .nth(1)
            .and_then(|rest| rest.split("fn allocate_nonwrapping_broker_identity").next())
            .expect("deferred activation bind transaction");
        let lock = transaction
            .find("let activations = DEFERRED_ACTIVATIONS.lock()")
            .expect("deferred authority lock");
        let register = transaction
            .find("let result = register()")
            .expect("binding registration callback");
        let release = transaction
            .find("drop(activations)")
            .expect("deferred authority release");
        assert!(bind.contains("with_deferred_activation_authority_for_smp_bind"));
        assert!(transaction[..register].contains("authority.qualification_required"));
        assert!(broker_source.contains("smp_qualification_exec_path_matches(exec_path.as_str())"));
        assert!(lock < register && register < release);
    }

    #[test]
    fn process_cleanup_revokes_qualification_before_deferred_authority_reuse() {
        let broker_source = include_str!("proc_broker_ops.rs");
        let cleanup = broker_source
            .split("pub(super) fn cleanup_proc_broker_state_for_process")
            .nth(1)
            .and_then(|rest| {
                rest.split("pub(super) fn cleanup_proc_broker_exec_state_for_thread")
                    .next()
            })
            .expect("process broker terminal cleanup");
        let revoke = cleanup
            .find("revoke_smp_qualification_for_process(process_id)")
            .expect("qualification binding revoke");
        let deferred = cleanup
            .find("let mut activations = DEFERRED_ACTIVATIONS.lock()")
            .expect("deferred activation cleanup");
        assert!(revoke < deferred);
    }
}
