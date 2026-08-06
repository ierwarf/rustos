//! RustOS IPC syscall admission and service-publication authority.
//!
//! - **Owner:** Compat owns syscall envelopes and service grants; the IPC
//!   runtime owns object mechanics and `rootd` owns service policy.
//! - **Boundary:** User buffers, endpoint handles, service IDs, attached
//!   handles, and claimed subjects are untrusted.
//! - **Lifecycle:** Register/lookup grants bind one process and endpoint epoch;
//!   call/reply/timeout/revoke remove exact request authority once.
//! - **Concurrency:** Registry mutation uses tracked ordering and never holds a
//!   local policy lock across synchronous service IPC.
//! - **Failure:** Malformed envelopes, foreign owners, capacity, timeout, peer
//!   exit, and stale publication fail without a leaked grant or late revival.
//! - **Forbidden:** No path/name-based capability, infinite service call,
//!   guessed endpoint authority, or re-registration after final exit.
//! - **Evidence:** `ipc-call`, `endpoint-lifecycle`, `root-authority`,
//!   `service-call-authority`, `commercial-envelope`, and
//!   `input-delivery-lifecycle`.
use super::*;

#[path = "ipc_reply_diagnostics.rs"]
mod diagnostics;
#[path = "ipc_reply_recv.rs"]
mod ipc_reply_recv;
mod subject;

use alloc::vec::Vec;
use core::mem::size_of;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
pub(super) use diagnostics::diagnostic_rate_limit_permit;
use diagnostics::record_ipc_reply_rejection;
pub(super) use subject::{
    current_process_has_service_capability, current_process_with_service_capability,
};

use kernel_ipc_runtime::api::{
    ChannelIdentity, EndpointCallPriority, KernelEndpointHandle, KernelReplyHandle,
    KernelTransferTicket, KernelTransferredHandle, ProcessIdentity, ServiceIdentity,
    TransferContext,
};
use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};

macro_rules! ipc_trace {
    ($($arg:tt)*) => {
        if IPC_TRACE_VERBOSE {
            debug::println_emergency(format_args!($($arg)*));
        }
    };
}

const IPC_TRACE_VERBOSE: bool = false;
const MAX_SERVICE_ENDPOINTS: usize = 16;
// The process table admits at most 32 live process objects. One exact grant per
// process/service pair therefore bounds the complete service-call grant set.
const MAX_SERVICE_CALL_GRANTS: usize = 32 * MAX_SERVICE_ENDPOINTS;
// Admission is per task, not per process. Every schedulable task may wait for
// one service publication without an artificial half-capacity failure.
const MAX_SERVICE_ENDPOINT_WAITERS: usize = multitask::MAX_SCHEDULER_TASKS;
const SLOW_IPC_THRESHOLD_MS: u64 = 10;
// Debugcon is a synchronous, globally contended device. One representative
// slow sample per second preserves observability without letting a degraded
// socket workload consume the CPU needed to recover the UI and its service
// owners. Counters and milestones retain the aggregate evidence.
const MAX_SLOW_IPC_LOGS_PER_SECOND: usize = 1;
const EARLY_IPC_SAMPLE_COUNT: usize = 6;
const SERVICE_IPC_TIMEOUT_MS: u64 = rustos_user_abi::performance::IPC_BULK_DATA_HARD_LIMIT_MS;
// RING3-MIGRATION-REFERENCE START: rootd should own service namespace endpoint
// ownership and capability leases. Ring0 keeps the temporary service registry
// table until rootd can mint narrow broker capabilities.
static LINUX_SYSCALL_ENDPOINT: AtomicU64 = AtomicU64::new(0);
static SERVICE_ENDPOINTS: [AtomicU64; MAX_SERVICE_ENDPOINTS] =
    [const { AtomicU64::new(0) }; MAX_SERVICE_ENDPOINTS];
static SERVICE_ENDPOINT_OWNERS: [AtomicU64; MAX_SERVICE_ENDPOINTS] =
    [const { AtomicU64::new(0) }; MAX_SERVICE_ENDPOINTS];
static SERVICE_ENDPOINT_CAPS: [AtomicU64; MAX_SERVICE_ENDPOINTS] =
    [const { AtomicU64::new(0) }; MAX_SERVICE_ENDPOINTS];
static SERVICE_ENDPOINT_EPOCHS: [AtomicU64; MAX_SERVICE_ENDPOINTS] =
    [const { AtomicU64::new(0) }; MAX_SERVICE_ENDPOINTS];
// Read-mostly authorization cache for the last process granted each service.
// The service epoch is the revocation generation, so a restart invalidates a
// cached hit without a writer-side broadcast. This removes the global grant
// table lock and 512-entry scan from the common service-call path while the
// exact grant table remains the source of truth on a miss.
static SERVICE_LAST_GRANTED_CALLER: [AtomicU64; MAX_SERVICE_ENDPOINTS] =
    [const { AtomicU64::new(0) }; MAX_SERVICE_ENDPOINTS];
static SERVICE_LAST_GRANTED_EPOCH: [AtomicU64; MAX_SERVICE_ENDPOINTS] =
    [const { AtomicU64::new(0) }; MAX_SERVICE_ENDPOINTS];
const SERVICE_ENDPOINT_STABLE_READ_ATTEMPTS: usize = 3;
// Rootd is the root of the service authority graph. The first successful
// publication permanently seals its process identity for this boot. Process
// identities never wrap or alias, so rootd exit is fail-stop until reboot
// instead of reopening the root namespace to an arbitrary ring3 process.
static ROOTD_BOOTSTRAP_OWNER: AtomicU64 = AtomicU64::new(0);
// Publication, revocation, and process-exit cleanup must share one mutation
// critical section. The endpoint itself remains the lock-free commit point for
// readers, but a second registrar or an exiting process must not interleave
// between capability preparation and endpoint publication.
static SERVICE_ENDPOINT_REGISTRY_MUTATION: TrackedSpinLock<
    (),
    { LockClass::ServiceEndpointRegistry as u8 },
> = TrackedSpinLock::new(());
static SERVICE_CALL_GRANTS: TrackedSpinLock<
    [ServiceCallGrant; MAX_SERVICE_CALL_GRANTS],
    { LockClass::ServiceCallGrant as u8 },
> = TrackedSpinLock::new([ServiceCallGrant::empty(); MAX_SERVICE_CALL_GRANTS]);
// RING3-MIGRATION-REFERENCE END: rootd-owned service endpoint registry state.
static IPC_LOG_SAMPLE_COUNT: AtomicUsize = AtomicUsize::new(0);
static SLOW_IPC_LOG_RATE_STATE: AtomicU64 = AtomicU64::new(u64::MAX);

#[derive(Clone, Copy)]
struct ServiceEndpointWaiter {
    task_id: u64,
    service_id: u64,
    expected_pid: u64,
}

struct ServiceEndpointWaiterTable {
    slots: [Option<ServiceEndpointWaiter>; MAX_SERVICE_ENDPOINT_WAITERS],
}

impl ServiceEndpointWaiterTable {
    const fn new() -> Self {
        Self {
            slots: [None; MAX_SERVICE_ENDPOINT_WAITERS],
        }
    }

    fn register(&mut self, waiter: ServiceEndpointWaiter) -> bool {
        if let Some(slot) = self
            .slots
            .iter_mut()
            .find(|slot| slot.is_some_and(|current| current.task_id == waiter.task_id))
        {
            *slot = Some(waiter);
            return true;
        }
        let Some(slot) = self.slots.iter_mut().find(|slot| slot.is_none()) else {
            return false;
        };
        *slot = Some(waiter);
        true
    }

    fn remove_task(&mut self, task_id: u64) -> usize {
        let mut removed = 0usize;
        for slot in &mut self.slots {
            if slot.is_some_and(|waiter| waiter.task_id == task_id) {
                *slot = None;
                removed += 1;
            }
        }
        removed
    }

    fn take_matching(
        &mut self,
        mut predicate: impl FnMut(ServiceEndpointWaiter) -> bool,
    ) -> ([u64; MAX_SERVICE_ENDPOINT_WAITERS], usize) {
        let mut tasks = [0_u64; MAX_SERVICE_ENDPOINT_WAITERS];
        let mut count = 0usize;
        for slot in &mut self.slots {
            let Some(waiter) = *slot else {
                continue;
            };
            if predicate(waiter) {
                tasks[count] = waiter.task_id;
                count += 1;
                *slot = None;
            }
        }
        (tasks, count)
    }
}

#[derive(Clone, Copy)]
struct ServiceCallGrant {
    process_id: u64,
    service_id: u64,
    endpoint_epoch: u64,
}

impl ServiceCallGrant {
    const fn empty() -> Self {
        Self {
            process_id: 0,
            service_id: 0,
            endpoint_epoch: 0,
        }
    }
}

#[derive(Clone, Copy)]
struct ServiceCapabilityAuthorization {
    capability: u64,
    rootd_epoch: Option<u64>,
}

#[derive(Clone, Copy)]
struct PublishedServiceEndpoint {
    endpoint: u64,
    owner: u64,
    epoch: u64,
}

static SERVICE_ENDPOINT_WAITERS: TrackedSpinLock<
    ServiceEndpointWaiterTable,
    { LockClass::ServiceEndpointWaiter as u8 },
> = TrackedSpinLock::new(ServiceEndpointWaiterTable::new());

pub(super) fn is_linux_rustos_ipc_syscall(syscall_number: u64) -> bool {
    matches!(
        syscall_number,
        linux_abi::SYS_RUSTOS_IPC_ENDPOINT_CREATE
            | linux_abi::SYS_RUSTOS_IPC_CALL
            | linux_abi::SYS_RUSTOS_IPC_CALL_BOUNDED
            | linux_abi::SYS_RUSTOS_IPC_RECV
            | linux_abi::SYS_RUSTOS_IPC_TRY_RECV
            | linux_abi::SYS_RUSTOS_IPC_TRY_RECV_WITH_SENDER
            | linux_abi::SYS_RUSTOS_IPC_RECV_WITH_SENDER
            | linux_abi::SYS_RUSTOS_IPC_REPLY_RECV_WITH_SENDER
            | linux_abi::SYS_RUSTOS_IPC_REPLY
            | linux_abi::SYS_RUSTOS_IPC_CALL_WITH_HANDLES
            | linux_abi::SYS_RUSTOS_IPC_CALL_WITH_HANDLES_BOUNDED
            | linux_abi::SYS_RUSTOS_IPC_RECV_WITH_HANDLES
            | linux_abi::SYS_RUSTOS_IPC_REPLY_WITH_HANDLES
            | linux_abi::SYS_RUSTOS_IPC_REGISTER_LINUX_SYSCALL_ENDPOINT
            | linux_abi::SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT
            | linux_abi::SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT
            | linux_abi::SYS_RUSTOS_IPC_WAIT_SERVICE_ENDPOINT
            | linux_abi::SYS_RUSTOS_IPC_VALIDATE_SERVICE_OWNER
    )
}

pub(super) fn dispatch_linux_rustos_ipc_syscall(frame: &SyscallFrame) -> u64 {
    match frame.rax {
        linux_abi::SYS_RUSTOS_IPC_ENDPOINT_CREATE => syscall_linux_rustos_ipc_endpoint_create(),
        linux_abi::SYS_RUSTOS_IPC_CALL => {
            ipc_trace!(
                "ipc dispatch call: rdi={} rsi={:#x} rdx={} r10={:#x} r8={:#x}",
                frame.rdi,
                frame.rsi,
                frame.rdx,
                frame.r10,
                frame.r8
            );
            syscall_linux_rustos_ipc_call(frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8)
        }
        linux_abi::SYS_RUSTOS_IPC_CALL_BOUNDED => syscall_linux_rustos_ipc_call_bounded(
            frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9,
        ),
        linux_abi::SYS_RUSTOS_IPC_RECV => {
            syscall_linux_rustos_ipc_recv(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_RUSTOS_IPC_TRY_RECV => {
            syscall_linux_rustos_ipc_try_recv(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_RUSTOS_IPC_TRY_RECV_WITH_SENDER => {
            syscall_linux_rustos_ipc_try_recv_with_sender(
                frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9,
            )
        }
        linux_abi::SYS_RUSTOS_IPC_RECV_WITH_SENDER => syscall_linux_rustos_ipc_recv_with_sender(
            frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9,
        ),
        linux_abi::SYS_RUSTOS_IPC_REPLY_RECV_WITH_SENDER => {
            ipc_reply_recv::syscall_linux_rustos_ipc_reply_recv_with_sender(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_IPC_VALIDATE_SERVICE_OWNER => {
            syscall_linux_rustos_ipc_validate_service_owner(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_IPC_REPLY => {
            syscall_linux_rustos_ipc_reply(frame.rdi, frame.rsi, frame.rdx)
        }
        linux_abi::SYS_RUSTOS_IPC_CALL_WITH_HANDLES => {
            syscall_linux_rustos_ipc_call_with_handles(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_IPC_CALL_WITH_HANDLES_BOUNDED => {
            syscall_linux_rustos_ipc_call_with_handles_bounded(frame.rdi, frame.rsi)
        }
        linux_abi::SYS_RUSTOS_IPC_RECV_WITH_HANDLES => {
            syscall_linux_rustos_ipc_recv_with_handles(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_IPC_REPLY_WITH_HANDLES => {
            syscall_linux_rustos_ipc_reply_with_handles(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_IPC_REGISTER_LINUX_SYSCALL_ENDPOINT => {
            syscall_linux_rustos_ipc_register_linux_syscall_endpoint(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT => {
            syscall_linux_rustos_ipc_register_service_endpoint(frame.rdi, frame.rsi)
        }
        linux_abi::SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT => {
            syscall_linux_rustos_ipc_lookup_service_endpoint(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_IPC_WAIT_SERVICE_ENDPOINT => {
            syscall_linux_rustos_ipc_wait_service_endpoint(frame.rdi)
        }
        _ => linux_errno(LINUX_ENOSYS),
    }
}

// RING3-MIGRATION-REFERENCE START: rootd should own service endpoint lookup and
// capability lease checks. Ring0 keeps direct table queries only for current
// broker authorization and service-call routing.
pub(super) fn linux_syscall_endpoint() -> Option<KernelEndpointHandle> {
    let raw = service_endpoint_raw(linux_abi::IPC_SERVICE_LINUX_SYSCALLD)
        .unwrap_or_else(|| LINUX_SYSCALL_ENDPOINT.load(Ordering::Acquire));
    (raw != 0).then_some(KernelEndpointHandle::from_raw(raw))
}

pub(super) fn service_endpoint(service_id: u64) -> Option<KernelEndpointHandle> {
    let raw = service_endpoint_raw(service_id)?;
    (raw != 0).then_some(KernelEndpointHandle::from_raw(raw))
}

pub(super) fn service_registered(service_id: u64) -> bool {
    service_endpoint_raw(service_id).is_some_and(|raw| raw != 0)
}

fn record_service_endpoint_milestone(
    name: &'static str,
    service_id: u64,
    process_id: u64,
    endpoint_or_status: u64,
) {
    debug::record_milestone(
        debug::LogCategory::Compat,
        name,
        service_id,
        ((process_id & 0xffff_ffff) << 32) | (endpoint_or_status & 0xffff_ffff),
    );
}

/// 프로세스 종료 시 해당 프로세스가 등록한 모든 IPC 서비스 엔드포인트를 해제한다.
/// stale endpoint가 남아 있으면 이후 호출자가 finite reply deadline을
/// 소모할 때까지 실패가 지연되므로 반드시 프로세스 종료 경로에서 호출해야 한다.
pub(crate) fn cleanup_service_endpoints_for_process(process_id: u64) {
    let mut revoked_services = [0_u64; MAX_SERVICE_ENDPOINTS];
    let mut revoked_count = 0usize;
    let registry_mutation = SERVICE_ENDPOINT_REGISTRY_MUTATION.lock();
    {
        let mut grants = SERVICE_CALL_GRANTS.lock();
        clear_service_call_grants(grants.as_mut_slice(), process_id);
    }
    for last_granted_caller in SERVICE_LAST_GRANTED_CALLER.iter() {
        if last_granted_caller.load(Ordering::Acquire) == process_id {
            last_granted_caller.store(0, Ordering::Release);
        }
    }
    if SERVICE_ENDPOINT_OWNERS[linux_abi::IPC_SERVICE_LINUX_SYSCALLD as usize]
        .load(Ordering::Acquire)
        == process_id
    {
        LINUX_SYSCALL_ENDPOINT.store(0, Ordering::Release);
    }
    for i in 0..MAX_SERVICE_ENDPOINTS {
        if SERVICE_ENDPOINT_OWNERS[i].load(Ordering::Acquire) == process_id {
            // Endpoint zero is the reader-visible revoke commit point. Clear
            // it before the tuple fields and advance the public epoch last,
            // matching the explicit revoke path below.
            SERVICE_ENDPOINTS[i].store(0, Ordering::Release);
            SERVICE_ENDPOINT_OWNERS[i].store(0, Ordering::Release);
            SERVICE_ENDPOINT_CAPS[i].store(0, Ordering::Release);
            advance_service_endpoint_epoch(i).expect("service endpoint epoch exhausted");
            record_service_endpoint_milestone("ipc-service-exit-revoke", i as u64, process_id, 0);
            ipc_trace!(
                "ipc service endpoint revoked on process exit: index={} process={}",
                i,
                process_id
            );
            revoked_services[revoked_count] = i as u64;
            revoked_count += 1;
        }
    }
    drop(registry_mutation);
    for service_id in revoked_services.into_iter().take(revoked_count) {
        super::broker_ops::waitset_broker_ops::revoke_waitset_provider(service_id);
        if service_exit_requires_input_policy_withdrawal(service_id) {
            // The fixed DVM ring publishes a separate policy-consumer lease.
            // Service endpoint revocation must withdraw it in the same
            // process-exit turn; otherwise L0 keeps producing into a ring
            // whose sole semantic consumer no longer exists.
            kernel_io_manager::api::input::transport::withdraw_policy_consumer();
        }
    }
    super::broker_ops::waitset_broker_ops::remove_waitset_waiters_for_process(process_id);
    wake_exited_service_endpoint_waiters(process_id);
}

fn service_exit_requires_input_policy_withdrawal(service_id: u64) -> bool {
    service_id == linux_abi::IPC_SERVICE_INPUTD
}

pub(super) fn current_process_service_capability_snapshot() -> Option<(u64, u64)> {
    let process_id = multitask::current_user_process_id()?;
    let _registry = SERVICE_ENDPOINT_REGISTRY_MUTATION.lock();
    let capabilities = SERVICE_ENDPOINTS
        .iter()
        .zip(SERVICE_ENDPOINT_OWNERS.iter())
        .zip(SERVICE_ENDPOINT_CAPS.iter())
        .filter(|((endpoint, owner), _)| {
            endpoint.load(Ordering::Acquire) != 0 && owner.load(Ordering::Acquire) == process_id
        })
        .fold(0_u64, |caps, (_, entry_caps)| {
            caps | entry_caps.load(Ordering::Acquire)
        });
    Some((process_id, capabilities))
}

fn current_process_has_service_capability_locked(capability: u64, process_id: u64) -> bool {
    if capability == 0 || multitask::is_user_process_exiting(process_id) {
        return false;
    }
    SERVICE_ENDPOINTS
        .iter()
        .zip(SERVICE_ENDPOINT_OWNERS.iter())
        .zip(SERVICE_ENDPOINT_CAPS.iter())
        .any(|((endpoint, owner), caps)| {
            // Readers share the mutation critical section so this three-field
            // tuple cannot combine generations during revoke/republication.
            endpoint.load(Ordering::Acquire) != 0
                && owner.load(Ordering::Acquire) == process_id
                && caps.load(Ordering::Acquire) & capability == capability
        })
}

fn current_process_can_lookup_service_endpoint() -> bool {
    current_process_has_service_capability(
        rustos_user_abi::syscall::IPC_SERVICE_CAP_ROOT_SUPERVISOR,
    )
}

fn service_endpoint_raw(service_id: u64) -> Option<u64> {
    let index = service_index(service_id)?;
    Some(
        stable_published_service_endpoint(index)
            .map(|publication| publication.endpoint)
            .unwrap_or(0),
    )
}

fn stable_published_service_endpoint(index: usize) -> Option<PublishedServiceEndpoint> {
    // Publication stores the endpoint last and revocation clears it first.
    // An epoch/endpoint double-read therefore gives the IPC hot path a stable
    // tuple without bouncing the global mutation-lock cache line on every
    // service call. A concurrent mutation is reported as transient absence;
    // callers already treat service restart/revoke as a bounded failure.
    for _ in 0..SERVICE_ENDPOINT_STABLE_READ_ATTEMPTS {
        let epoch_before = SERVICE_ENDPOINT_EPOCHS[index].load(Ordering::Acquire);
        let endpoint_before = SERVICE_ENDPOINTS[index].load(Ordering::Acquire);
        if endpoint_before == 0 {
            return None;
        }
        let owner = SERVICE_ENDPOINT_OWNERS[index].load(Ordering::Acquire);
        let endpoint_after = SERVICE_ENDPOINTS[index].load(Ordering::Acquire);
        let epoch_after = SERVICE_ENDPOINT_EPOCHS[index].load(Ordering::Acquire);
        if endpoint_before == endpoint_after && epoch_before == epoch_after {
            let endpoint = stable_service_endpoint_snapshot(
                endpoint_before,
                owner,
                multitask::is_user_process_exiting(owner),
            );
            return (endpoint != 0 && epoch_before != 0).then_some(PublishedServiceEndpoint {
                endpoint,
                owner,
                epoch: epoch_before,
            });
        }
        core::hint::spin_loop();
    }
    None
}

fn stable_service_endpoint_snapshot(endpoint: u64, owner: u64, owner_exiting: bool) -> u64 {
    if endpoint == 0 || owner == 0 || owner_exiting {
        0
    } else {
        endpoint
    }
}

fn service_index(service_id: u64) -> Option<usize> {
    let index = usize::try_from(service_id).ok()?;
    (index < MAX_SERVICE_ENDPOINTS).then_some(index)
}

pub(crate) fn process_owns_live_service_endpoint(process_id: u64, service_id: u64) -> bool {
    let Some(index) = service_index(service_id) else {
        return false;
    };
    let _registry = SERVICE_ENDPOINT_REGISTRY_MUTATION.lock();
    SERVICE_ENDPOINTS[index].load(Ordering::Acquire) != 0
        && SERVICE_ENDPOINT_OWNERS[index].load(Ordering::Acquire) == process_id
        && !multitask::is_user_process_exiting(process_id)
}

fn syscall_linux_rustos_ipc_validate_service_owner(args_ptr: u64) -> u64 {
    let args =
        match usermem::read_current_user_struct::<RustosIpcValidateServiceOwnerArgs>(args_ptr) {
            Ok(args) => args,
            Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
        };
    if args.abi_version != IPC_ABI_VERSION
        || args.reserved0 != 0
        || args.flags != 0
        || args.process_id == 0
        || args.reserved1 != 0
        || service_index(args.service_id).is_none()
    {
        return linux_errno(LINUX_EINVAL);
    }
    if !process_owns_live_service_endpoint(args.process_id, args.service_id) {
        return linux_errno(LINUX_EPERM);
    }
    0
}

pub(crate) fn service_endpoint_epoch(service_id: u64) -> Option<u64> {
    let index = service_index(service_id)?;
    let _registry = SERVICE_ENDPOINT_REGISTRY_MUTATION.lock();
    let endpoint = SERVICE_ENDPOINTS[index].load(Ordering::Acquire);
    let owner = SERVICE_ENDPOINT_OWNERS[index].load(Ordering::Acquire);
    let epoch = SERVICE_ENDPOINT_EPOCHS[index].load(Ordering::Acquire);
    (endpoint != 0 && owner != 0 && epoch != 0 && !multitask::is_user_process_exiting(owner))
        .then_some(epoch)
}

fn next_service_endpoint_epoch(current: u64) -> Option<u64> {
    current.checked_add(1).filter(|next| *next != 0)
}

fn rootd_bootstrap_owner_allows(sealed_owner: u64, candidate: u64) -> bool {
    candidate != 0 && (sealed_owner == 0 || sealed_owner == candidate)
}

fn record_service_call_grant(
    grants: &mut [ServiceCallGrant],
    process_id: u64,
    service_id: u64,
    endpoint_epoch: u64,
) -> Result<(), i64> {
    if process_id == 0 || endpoint_epoch == 0 {
        return Err(LINUX_EINVAL);
    }
    if let Some(grant) = grants
        .iter_mut()
        .find(|grant| grant.process_id == process_id && grant.service_id == service_id)
    {
        grant.endpoint_epoch = endpoint_epoch;
        return Ok(());
    }
    let Some(grant) = grants.iter_mut().find(|grant| grant.process_id == 0) else {
        return Err(LINUX_ENOSPC);
    };
    *grant = ServiceCallGrant {
        process_id,
        service_id,
        endpoint_epoch,
    };
    Ok(())
}

fn has_service_call_grant(
    grants: &[ServiceCallGrant],
    process_id: u64,
    service_id: u64,
    endpoint_epoch: u64,
) -> bool {
    process_id != 0
        && endpoint_epoch != 0
        && grants.iter().any(|grant| {
            grant.process_id == process_id
                && grant.service_id == service_id
                && grant.endpoint_epoch == endpoint_epoch
        })
}

fn clear_service_call_grants(grants: &mut [ServiceCallGrant], process_id: u64) {
    for grant in grants
        .iter_mut()
        .filter(|grant| grant.process_id == process_id)
    {
        *grant = ServiceCallGrant::empty();
    }
}

fn rootd_authorization_epoch_matches(
    expected_epoch: u64,
    endpoint: u64,
    owner: u64,
    current_epoch: u64,
    owner_exiting: bool,
) -> bool {
    endpoint != 0
        && owner != 0
        && current_epoch == expected_epoch
        && expected_epoch != 0
        && !owner_exiting
}

/// Called only while `SERVICE_ENDPOINT_REGISTRY_MUTATION` is held.
fn service_authorization_is_current(authorization: ServiceCapabilityAuthorization) -> bool {
    let Some(expected_epoch) = authorization.rootd_epoch else {
        return true;
    };
    let index = linux_abi::IPC_SERVICE_ROOTD as usize;
    let endpoint = SERVICE_ENDPOINTS[index].load(Ordering::Acquire);
    let owner = SERVICE_ENDPOINT_OWNERS[index].load(Ordering::Acquire);
    rootd_authorization_epoch_matches(
        expected_epoch,
        endpoint,
        owner,
        SERVICE_ENDPOINT_EPOCHS[index].load(Ordering::Acquire),
        multitask::is_user_process_exiting(owner),
    )
}

fn validate_endpoint_publication_owner(endpoint: u64, process_id: u64) -> Result<(), i64> {
    kernel_ipc_runtime::api::authorize_endpoint_receiver_for_process(
        KernelEndpointHandle::from_raw(endpoint),
        process_id,
    )
    .map_err(ipc_error_to_linux_errno)
}

fn grant_current_process_service_call(
    service_id: u64,
    expected_endpoint: Option<u64>,
    expected_owner: Option<u64>,
) -> Result<u64, i64> {
    let Some(index) = service_index(service_id) else {
        return Err(LINUX_EINVAL);
    };
    let Some(process_id) = multitask::current_user_process_id() else {
        return Err(LINUX_EINVAL);
    };
    let _registry = SERVICE_ENDPOINT_REGISTRY_MUTATION.lock();
    let endpoint = SERVICE_ENDPOINTS[index].load(Ordering::Acquire);
    let owner = SERVICE_ENDPOINT_OWNERS[index].load(Ordering::Acquire);
    let epoch = SERVICE_ENDPOINT_EPOCHS[index].load(Ordering::Acquire);
    if expected_endpoint.is_some_and(|expected| endpoint != expected)
        || expected_owner.is_some_and(|expected| owner != expected)
    {
        return Err(LINUX_EAGAIN);
    }
    if endpoint == 0 || owner == 0 || epoch == 0 {
        return Err(if expected_endpoint.is_some() {
            LINUX_EAGAIN
        } else {
            LINUX_ENOSYS
        });
    }
    if multitask::is_user_process_exiting(owner) {
        return Err(LINUX_ESRCH);
    }
    {
        let mut grants = SERVICE_CALL_GRANTS.lock();
        record_service_call_grant(grants.as_mut_slice(), process_id, service_id, epoch)?;
    }
    // Publish the epoch before the caller. A reader that observes the caller
    // with Acquire also observes the exact grant generation. Same-caller
    // republish is safe: the old epoch can only cause a conservative miss.
    SERVICE_LAST_GRANTED_EPOCH[index].store(epoch, Ordering::Relaxed);
    SERVICE_LAST_GRANTED_CALLER[index].store(process_id, Ordering::Release);
    Ok(endpoint)
}

fn current_process_granted_service_endpoint(service_id: u64) -> Result<Option<u64>, i64> {
    let index = service_index(service_id).ok_or(LINUX_EINVAL)?;
    let process_id = multitask::current_user_process_id().ok_or(LINUX_EINVAL)?;
    let Some(publication) = stable_published_service_endpoint(index) else {
        return Err(LINUX_ENOSYS);
    };
    if publication.owner == process_id
        || cached_service_call_grant_matches(
            SERVICE_LAST_GRANTED_CALLER[index].load(Ordering::Acquire),
            SERVICE_LAST_GRANTED_EPOCH[index].load(Ordering::Relaxed),
            process_id,
            publication.epoch,
        )
    {
        return Ok(Some(publication.endpoint));
    }
    let granted = {
        let grants = SERVICE_CALL_GRANTS.lock();
        has_service_call_grant(grants.as_slice(), process_id, service_id, publication.epoch)
    };
    if !granted {
        return Ok(None);
    }
    SERVICE_LAST_GRANTED_EPOCH[index].store(publication.epoch, Ordering::Relaxed);
    SERVICE_LAST_GRANTED_CALLER[index].store(process_id, Ordering::Release);
    Ok(Some(publication.endpoint))
}

fn authorize_current_process_ipc_call(endpoint: u64) -> Result<(), i64> {
    if endpoint == 0 {
        return Err(LINUX_EINVAL);
    }
    let Some(process_id) = multitask::current_user_process_id() else {
        return Err(LINUX_EINVAL);
    };
    for index in 0..MAX_SERVICE_ENDPOINTS {
        let Some(publication) = stable_published_service_endpoint(index) else {
            continue;
        };
        if publication.endpoint != endpoint {
            continue;
        }
        if publication.owner == process_id
            || cached_service_call_grant_matches(
                SERVICE_LAST_GRANTED_CALLER[index].load(Ordering::Acquire),
                SERVICE_LAST_GRANTED_EPOCH[index].load(Ordering::Relaxed),
                process_id,
                publication.epoch,
            )
        {
            return Ok(());
        }
        let granted = {
            let grants = SERVICE_CALL_GRANTS.lock();
            has_service_call_grant(
                grants.as_slice(),
                process_id,
                index as u64,
                publication.epoch,
            )
        };
        if !granted {
            return Err(LINUX_EACCES);
        }
        SERVICE_LAST_GRANTED_EPOCH[index].store(publication.epoch, Ordering::Relaxed);
        SERVICE_LAST_GRANTED_CALLER[index].store(process_id, Ordering::Release);
        return Ok(());
    }
    validate_endpoint_publication_owner(endpoint, process_id).map_err(|_| LINUX_EACCES)
}

fn cached_service_call_grant_matches(
    cached_process_id: u64,
    cached_epoch: u64,
    process_id: u64,
    endpoint_epoch: u64,
) -> bool {
    process_id != 0
        && endpoint_epoch != 0
        && cached_process_id == process_id
        && cached_epoch == endpoint_epoch
}

/// Called only while `SERVICE_ENDPOINT_REGISTRY_MUTATION` is held.
fn advance_service_endpoint_epoch(index: usize) -> Result<u64, i64> {
    let current = SERVICE_ENDPOINT_EPOCHS[index].load(Ordering::Relaxed);
    let next = next_service_endpoint_epoch(current).ok_or(LINUX_EOVERFLOW)?;
    SERVICE_ENDPOINT_EPOCHS[index].store(next, Ordering::Release);
    if current != 0 {
        multitask::drop_ipc_transfers_for_service_epoch(index as u64, current);
    }
    Ok(next)
}

fn register_service_endpoint_waiter(waiter: ServiceEndpointWaiter) -> bool {
    SERVICE_ENDPOINT_WAITERS.lock().register(waiter)
}

pub(super) fn remove_service_endpoint_waiter(task_id: u64) -> usize {
    SERVICE_ENDPOINT_WAITERS.lock().remove_task(task_id)
}

fn wake_registered_service_endpoint_waiters(service_id: u64, owner_pid: u64) {
    let (tasks, count) = SERVICE_ENDPOINT_WAITERS.lock().take_matching(|waiter| {
        waiter.service_id == service_id && waiter.expected_pid == owner_pid
    });
    for task_id in tasks.into_iter().take(count) {
        if multitask::wake_task(task_id) {
            multitask::set_next_pick_hint(task_id);
        }
    }
}

fn wake_exited_service_endpoint_waiters(process_id: u64) {
    let (tasks, count) = SERVICE_ENDPOINT_WAITERS
        .lock()
        .take_matching(|waiter| waiter.expected_pid == process_id);
    let mut woke = false;
    for task_id in tasks.into_iter().take(count) {
        if multitask::wake_task(task_id) {
            multitask::set_next_pick_hint(task_id);
            woke = true;
        }
    }
    if woke {
        multitask::request_deferred_reschedule();
    }
}
// RING3-MIGRATION-REFERENCE END: rootd-owned service lookup and capability checks.

pub(super) fn syscall_linux_rustos_ipc_endpoint_create() -> u64 {
    let Some(process_id) = multitask::current_user_process_id() else {
        return linux_errno(LINUX_EINVAL);
    };
    ipc_trace!("ipc endpoint create: process={}", process_id);
    match kernel_ipc_runtime::api::create_endpoint_for_process(process_id) {
        Ok(endpoint) => {
            ipc_trace!(
                "ipc endpoint created: process={} endpoint={}",
                process_id,
                endpoint.raw()
            );
            endpoint.raw()
        }
        Err(err) => linux_errno(ipc_error_to_linux_errno(err)),
    }
}

// RING3-MIGRATION-REFERENCE START: rootd should own service endpoint
// registration, lookup, and capability assignment. Ring0 keeps this bridge
// only while core services still register directly with the IPC substrate.
pub(super) fn syscall_linux_rustos_ipc_register_linux_syscall_endpoint(endpoint: u64) -> u64 {
    if endpoint == 0 {
        return linux_errno(LINUX_EINVAL);
    }
    let Some(process_id) = multitask::current_user_process_id() else {
        return linux_errno(LINUX_EINVAL);
    };
    ipc_trace!(
        "ipc register linux syscall endpoint: process={} endpoint={}",
        process_id,
        endpoint
    );
    if let Err(errno) = validate_endpoint_publication_owner(endpoint, process_id) {
        return linux_errno(errno);
    }
    let authorization = match service_capability(linux_abi::IPC_SERVICE_LINUX_SYSCALLD) {
        Ok(authorization) => authorization,
        Err(errno) => {
            record_service_endpoint_milestone(
                "ipc-service-register-denied",
                linux_abi::IPC_SERVICE_LINUX_SYSCALLD,
                process_id,
                errno as u64,
            );
            return linux_errno(errno);
        }
    };
    let registry_mutation = SERVICE_ENDPOINT_REGISTRY_MUTATION.lock();
    if multitask::is_user_process_exiting(process_id) {
        drop(registry_mutation);
        record_service_endpoint_milestone(
            "ipc-service-register-denied",
            linux_abi::IPC_SERVICE_LINUX_SYSCALLD,
            process_id,
            LINUX_ESRCH as u64,
        );
        return linux_errno(LINUX_ESRCH);
    }
    if !service_authorization_is_current(authorization) {
        drop(registry_mutation);
        return linux_errno(LINUX_EAGAIN);
    }
    SERVICE_ENDPOINT_CAPS[linux_abi::IPC_SERVICE_LINUX_SYSCALLD as usize]
        .store(authorization.capability, Ordering::Release);
    SERVICE_ENDPOINT_OWNERS[linux_abi::IPC_SERVICE_LINUX_SYSCALLD as usize]
        .store(process_id, Ordering::Release);
    if let Err(errno) =
        advance_service_endpoint_epoch(linux_abi::IPC_SERVICE_LINUX_SYSCALLD as usize)
    {
        SERVICE_ENDPOINT_CAPS[linux_abi::IPC_SERVICE_LINUX_SYSCALLD as usize]
            .store(0, Ordering::Release);
        SERVICE_ENDPOINT_OWNERS[linux_abi::IPC_SERVICE_LINUX_SYSCALLD as usize]
            .store(0, Ordering::Release);
        drop(registry_mutation);
        return linux_errno(errno);
    }
    SERVICE_ENDPOINTS[linux_abi::IPC_SERVICE_LINUX_SYSCALLD as usize]
        .store(endpoint, Ordering::Release);
    LINUX_SYSCALL_ENDPOINT.store(endpoint, Ordering::Release);
    drop(registry_mutation);
    ipc_trace!(
        "ipc service registered: service={} endpoint={} owner={} caps={:#x}",
        linux_abi::IPC_SERVICE_LINUX_SYSCALLD,
        endpoint,
        process_id,
        authorization.capability
    );
    record_service_endpoint_milestone(
        "ipc-service-register",
        linux_abi::IPC_SERVICE_LINUX_SYSCALLD,
        process_id,
        endpoint,
    );
    wake_registered_service_endpoint_waiters(linux_abi::IPC_SERVICE_LINUX_SYSCALLD, process_id);
    0
}

pub(super) fn syscall_linux_rustos_ipc_register_service_endpoint(
    service_id: u64,
    endpoint: u64,
) -> u64 {
    let Some(index) = service_index(service_id) else {
        return linux_errno(LINUX_EINVAL);
    };
    let Some(process_id) = multitask::current_user_process_id() else {
        return linux_errno(LINUX_EINVAL);
    };
    ipc_trace!(
        "ipc register service endpoint: service={} process={} endpoint={}",
        service_id,
        process_id,
        endpoint
    );
    if endpoint == 0 {
        let registry_mutation = SERVICE_ENDPOINT_REGISTRY_MUTATION.lock();
        let owner = SERVICE_ENDPOINT_OWNERS[index].load(Ordering::Acquire);
        let registered_endpoint = SERVICE_ENDPOINTS[index].load(Ordering::Acquire);
        if owner == 0 && registered_endpoint == 0 {
            return 0;
        }
        if owner != process_id
            && !current_process_has_service_capability_locked(
                rustos_user_abi::syscall::IPC_SERVICE_CAP_ROOT_SUPERVISOR,
                process_id,
            )
        {
            record_service_endpoint_milestone(
                "ipc-service-revoke-denied",
                service_id,
                process_id,
                LINUX_EPERM as u64,
            );
            return linux_errno(LINUX_EPERM);
        }
        SERVICE_ENDPOINTS[index].store(0, Ordering::Release);
        SERVICE_ENDPOINT_OWNERS[index].store(0, Ordering::Release);
        SERVICE_ENDPOINT_CAPS[index].store(0, Ordering::Release);
        advance_service_endpoint_epoch(index).expect("service endpoint epoch exhausted");
        drop(registry_mutation);
        super::broker_ops::waitset_broker_ops::revoke_waitset_provider(service_id);
        record_service_endpoint_milestone("ipc-service-revoke", service_id, process_id, 0);
        ipc_trace!("ipc service revoked: service={}", service_id);
        return 0;
    }
    if let Err(errno) = validate_endpoint_publication_owner(endpoint, process_id) {
        return linux_errno(errno);
    }
    let authorization = match service_capability(service_id) {
        Ok(authorization) => authorization,
        Err(errno) => {
            record_service_endpoint_milestone(
                "ipc-service-register-denied",
                service_id,
                process_id,
                errno as u64,
            );
            return linux_errno(errno);
        }
    };
    let registry_mutation = SERVICE_ENDPOINT_REGISTRY_MUTATION.lock();
    if multitask::is_user_process_exiting(process_id) {
        drop(registry_mutation);
        record_service_endpoint_milestone(
            "ipc-service-register-denied",
            service_id,
            process_id,
            LINUX_ESRCH as u64,
        );
        return linux_errno(LINUX_ESRCH);
    }
    if SERVICE_ENDPOINTS[index].load(Ordering::Acquire) != 0 {
        drop(registry_mutation);
        record_service_endpoint_milestone(
            "ipc-service-register-busy",
            service_id,
            process_id,
            LINUX_EBUSY as u64,
        );
        return linux_errno(LINUX_EBUSY);
    }
    if !service_authorization_is_current(authorization) {
        drop(registry_mutation);
        return linux_errno(LINUX_EAGAIN);
    }
    let sealed_rootd_owner = ROOTD_BOOTSTRAP_OWNER.load(Ordering::Acquire);
    if service_id == linux_abi::IPC_SERVICE_ROOTD
        && !rootd_bootstrap_owner_allows(sealed_rootd_owner, process_id)
    {
        drop(registry_mutation);
        return linux_errno(LINUX_EPERM);
    }
    if let Err(errno) = advance_service_endpoint_epoch(index) {
        drop(registry_mutation);
        return linux_errno(errno);
    }
    if service_id == linux_abi::IPC_SERVICE_ROOTD && sealed_rootd_owner == 0 {
        ROOTD_BOOTSTRAP_OWNER.store(process_id, Ordering::Release);
    }
    SERVICE_ENDPOINT_CAPS[index].store(authorization.capability, Ordering::Release);
    SERVICE_ENDPOINT_OWNERS[index].store(process_id, Ordering::Release);
    // Publish the endpoint last. Acquire readers that observe it also observe
    // the rootd-authorized owner and capability written above.
    SERVICE_ENDPOINTS[index].store(endpoint, Ordering::Release);
    drop(registry_mutation);
    ipc_trace!(
        "ipc service registered: service={} endpoint={} owner={} caps={:#x}",
        service_id,
        endpoint,
        process_id,
        authorization.capability
    );
    record_service_endpoint_milestone("ipc-service-register", service_id, process_id, endpoint);
    wake_registered_service_endpoint_waiters(service_id, process_id);
    0
}

fn service_capability(service_id: u64) -> Result<ServiceCapabilityAuthorization, i64> {
    if service_id == linux_abi::IPC_SERVICE_ROOTD {
        return Ok(ServiceCapabilityAuthorization {
            capability: rustos_user_abi::syscall::IPC_SERVICE_CAP_ROOT_SUPERVISOR,
            rootd_epoch: None,
        });
    }
    let rootd_epoch = service_endpoint_epoch(linux_abi::IPC_SERVICE_ROOTD).ok_or(LINUX_ENOENT)?;
    // Endpoint registration is a rare authority transition, but it is on the
    // boot-critical path. Keep the two kernel-stamped ends of the rootd
    // round-trip visible so a delayed registrar is not misattributed to the
    // local endpoint table or an unrelated storage request.
    record_service_endpoint_milestone(
        "ipc-service-capability-request",
        service_id,
        multitask::current_user_process_id().unwrap_or_default(),
        rootd_epoch,
    );
    let capability = service_capability_via_rootd(service_id)?;
    record_service_endpoint_milestone(
        "ipc-service-capability-reply",
        service_id,
        multitask::current_user_process_id().unwrap_or_default(),
        capability,
    );
    Ok(ServiceCapabilityAuthorization {
        capability,
        rootd_epoch: Some(rootd_epoch),
    })
}

pub(super) fn validate_commercial_response_envelope(
    request: &CommercialMaxProtocolRequest,
    response: &CommercialMaxProtocolResponse,
) -> Result<(), i64> {
    if !response.is_valid_envelope_for(request) {
        return Err(LINUX_EINVAL);
    }
    Ok(())
}

fn service_capability_via_rootd(service_id: u64) -> Result<u64, i64> {
    let mut request = CommercialMaxProtocolRequest::default();
    request.header.version = rustos_user_abi::syscall::COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    request.header.protocol = rustos_user_abi::syscall::COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR;
    request.header.op = rustos_user_abi::syscall::COMMERCIAL_MAX_ROOTD_OP_SERVICE_CAPABILITY;
    let Some(snapshot) = multitask::current_user_snapshot() else {
        return Err(LINUX_EINVAL);
    };
    request.header.subject_pid = snapshot.process_id();
    request.header.subject_tid = snapshot.thread_id();
    request.arg0 = service_id;
    let response = call_service_endpoint_with_class(
        linux_abi::IPC_SERVICE_ROOTD,
        as_bytes(&request),
        ServiceIpcClass::BootControl,
    )?;
    if response.len() != size_of::<CommercialMaxProtocolResponse>() {
        return Err(LINUX_EINVAL);
    }
    let response = read_unaligned::<CommercialMaxProtocolResponse>(response.as_slice());
    validate_commercial_response_envelope(&request, &response)?;
    if response.descriptor_count != 0 || response.payload_len != 0 || response.value1 != 0 {
        return Err(LINUX_EINVAL);
    }
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    if response.value0 == 0 {
        return Err(LINUX_EINVAL);
    }
    Ok(response.value0)
}

pub(super) fn syscall_linux_rustos_ipc_lookup_service_endpoint(service_id: u64) -> u64 {
    ipc_trace!("ipc lookup service endpoint: service={}", service_id);
    // Rootd is the authorization broker for service lookup, so its own
    // endpoint must remain directly discoverable as bootstrap substrate.
    if service_id != linux_abi::IPC_SERVICE_ROOTD {
        match current_process_granted_service_endpoint(service_id) {
            Ok(Some(endpoint)) => return endpoint,
            Ok(None) => {}
            Err(errno) => return linux_errno(errno),
        }
    }
    if service_id != linux_abi::IPC_SERVICE_ROOTD
        && !current_process_can_lookup_service_endpoint()
        && let Err(errno) = authorize_service_lookup_via_rootd(service_id)
    {
        return linux_errno(errno);
    }
    match grant_current_process_service_call(service_id, None, None) {
        Ok(endpoint) => endpoint,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_rustos_ipc_wait_service_endpoint(args_ptr: u64) -> u64 {
    let args = match usermem::read_current_user_struct::<RustosIpcWaitServiceEndpointArgs>(args_ptr)
    {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.abi_version != IPC_WAIT_SERVICE_ENDPOINT_ABI_VERSION
        || args.reserved0 != 0
        || args.flags != 0
        || service_index(args.service_id).is_none()
        || args.expected_pid == 0
        || args.timeout_ms == 0
        || args.timeout_ms > IPC_WAIT_SERVICE_ENDPOINT_MAX_TIMEOUT_MS
    {
        return linux_errno(LINUX_EINVAL);
    }
    if args.service_id != linux_abi::IPC_SERVICE_ROOTD
        && !current_process_can_lookup_service_endpoint()
        && let Err(errno) = authorize_service_lookup_via_rootd(args.service_id)
    {
        return linux_errno(errno);
    }

    let Some(task_id) = multitask::current_task_id() else {
        return linux_errno(LINUX_EINVAL);
    };
    let ticks_per_second = crate::arch::rtc::ticks_per_second().max(1);
    let timeout_ticks = args
        .timeout_ms
        .saturating_mul(ticks_per_second)
        .saturating_add(999)
        .saturating_div(1000)
        .max(1);
    let deadline_tick = crate::arch::rtc::ticks().saturating_add(timeout_ticks);

    loop {
        match service_endpoint_for_expected_process(args.service_id, args.expected_pid) {
            Ok(Some(endpoint)) => {
                let endpoint = match grant_current_process_service_call(
                    args.service_id,
                    Some(endpoint),
                    Some(args.expected_pid),
                ) {
                    Ok(endpoint) => endpoint,
                    Err(LINUX_EAGAIN) => continue,
                    Err(errno) => return linux_errno(errno),
                };
                remove_service_endpoint_waiter(task_id);
                crate::arch::rtc::disarm_sleep_waiter(task_id);
                return endpoint;
            }
            Ok(None) => {}
            Err(errno) => return linux_errno(errno),
        }
        if multitask::is_user_process_exiting(args.expected_pid) {
            remove_service_endpoint_waiter(task_id);
            crate::arch::rtc::disarm_sleep_waiter(task_id);
            return linux_errno(LINUX_ESRCH);
        }
        if crate::arch::rtc::ticks() >= deadline_tick {
            remove_service_endpoint_waiter(task_id);
            crate::arch::rtc::disarm_sleep_waiter(task_id);
            return linux_errno(LINUX_ETIMEDOUT);
        }
        if !multitask::arm_block_current_task() {
            return linux_errno(LINUX_EINVAL);
        }
        if !register_service_endpoint_waiter(ServiceEndpointWaiter {
            task_id,
            service_id: args.service_id,
            expected_pid: args.expected_pid,
        }) {
            let _ = multitask::cancel_block_current_task();
            return linux_errno(LINUX_EBUSY);
        }

        // Close the register-before-sleep race: endpoint registration may
        // have completed between the first check and waiter insertion.
        match service_endpoint_for_expected_process(args.service_id, args.expected_pid) {
            Ok(Some(_)) => {
                remove_service_endpoint_waiter(task_id);
                let _ = multitask::cancel_block_current_task();
                continue;
            }
            Ok(None) => {}
            Err(errno) => {
                remove_service_endpoint_waiter(task_id);
                let _ = multitask::cancel_block_current_task();
                return linux_errno(errno);
            }
        }
        if multitask::is_user_process_exiting(args.expected_pid)
            || crate::arch::rtc::ticks() >= deadline_tick
        {
            remove_service_endpoint_waiter(task_id);
            let _ = multitask::cancel_block_current_task();
            continue;
        }
        if !crate::arch::rtc::arm_sleep_waiter_until_tick(task_id, deadline_tick) {
            remove_service_endpoint_waiter(task_id);
            let _ = multitask::cancel_block_current_task();
            return linux_errno(LINUX_EBUSY);
        }
        match multitask::commit_block_current_task_and_yield() {
            Some(true) => {}
            Some(false) => {}
            None => {
                remove_service_endpoint_waiter(task_id);
                crate::arch::rtc::disarm_sleep_waiter(task_id);
                return linux_errno(LINUX_EINVAL);
            }
        }
        remove_service_endpoint_waiter(task_id);
        crate::arch::rtc::disarm_sleep_waiter(task_id);
    }
}

fn service_endpoint_for_expected_process(
    service_id: u64,
    expected_pid: u64,
) -> Result<Option<u64>, i64> {
    let index = service_index(service_id).ok_or(LINUX_EINVAL)?;
    let _registry = SERVICE_ENDPOINT_REGISTRY_MUTATION.lock();
    let endpoint = SERVICE_ENDPOINTS[index].load(Ordering::Acquire);
    if endpoint == 0 {
        return Ok(None);
    }
    let owner = SERVICE_ENDPOINT_OWNERS[index].load(Ordering::Acquire);
    if owner != expected_pid {
        return Err(LINUX_EBUSY);
    }
    if multitask::is_user_process_exiting(owner) {
        return Err(LINUX_ESRCH);
    }
    Ok(Some(endpoint))
}

fn authorize_service_lookup_via_rootd(service_id: u64) -> Result<(), i64> {
    let mut request = CommercialMaxProtocolRequest::default();
    request.header.version = rustos_user_abi::syscall::COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    request.header.protocol = rustos_user_abi::syscall::COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR;
    request.header.op = rustos_user_abi::syscall::COMMERCIAL_MAX_ROOTD_OP_SERVICE_LOOKUP;
    let Some(snapshot) = multitask::current_user_snapshot() else {
        return Err(LINUX_EINVAL);
    };
    request.header.subject_pid = snapshot.process_id();
    request.header.subject_tid = snapshot.thread_id();
    request.arg0 = service_id;
    let response = call_service_endpoint_with_class(
        linux_abi::IPC_SERVICE_ROOTD,
        as_bytes(&request),
        ServiceIpcClass::BootControl,
    )?;
    if response.len() != size_of::<CommercialMaxProtocolResponse>() {
        return Err(LINUX_EINVAL);
    }
    let response = read_unaligned::<CommercialMaxProtocolResponse>(response.as_slice());
    validate_commercial_response_envelope(&request, &response)?;
    if response.status != 0 {
        if response.descriptor_count != 0
            || response.payload_len != 0
            || response.value0 != 0
            || response.value1 != 0
        {
            return Err(LINUX_EINVAL);
        }
        return Err(response.status.unsigned_abs() as i64);
    }
    if response.descriptor_count != 0
        || response.payload_len != 0
        || response.value0 != service_id
        || response.value1 != 0
    {
        return Err(LINUX_EINVAL);
    }
    Ok(())
}
// RING3-MIGRATION-REFERENCE END: rootd-owned service registration and capability policy.

pub(super) fn syscall_linux_rustos_ipc_call(
    endpoint: u64,
    request_ptr: u64,
    request_len: u64,
    reply_ptr: u64,
    reply_capacity: u64,
) -> u64 {
    syscall_linux_rustos_ipc_call_with_timeout(
        endpoint,
        request_ptr,
        request_len,
        reply_ptr,
        reply_capacity,
        SERVICE_IPC_TIMEOUT_MS,
    )
}

pub(super) fn syscall_linux_rustos_ipc_call_bounded(
    endpoint: u64,
    request_ptr: u64,
    request_len: u64,
    reply_ptr: u64,
    reply_capacity: u64,
    timeout_ms: u64,
) -> u64 {
    if !bounded_ipc_call_timeout_is_valid(timeout_ms) {
        return linux_errno(LINUX_EINVAL);
    }
    syscall_linux_rustos_ipc_call_with_timeout(
        endpoint,
        request_ptr,
        request_len,
        reply_ptr,
        reply_capacity,
        timeout_ms,
    )
}

const fn bounded_ipc_call_timeout_is_valid(timeout_ms: u64) -> bool {
    timeout_ms > 0 && timeout_ms <= SERVICE_IPC_TIMEOUT_MS
}

fn syscall_linux_rustos_ipc_call_with_timeout(
    endpoint: u64,
    request_ptr: u64,
    request_len: u64,
    reply_ptr: u64,
    reply_capacity: u64,
    timeout_ms: u64,
) -> u64 {
    if let Err(errno) = authorize_current_process_ipc_call(endpoint) {
        return linux_errno(errno);
    }
    let endpoint = KernelEndpointHandle::from_raw(endpoint);
    ipc_trace!(
        "ipc call start: endpoint={} request_ptr={:#x} request_len={} reply_ptr={:#x} reply_capacity={}",
        endpoint.raw(),
        request_ptr,
        request_len,
        reply_ptr,
        reply_capacity
    );
    let start_ticks = crate::arch::rtc::ticks();
    let request = match copy_request_from_user(request_ptr, request_len) {
        Ok(request) => request,
        Err(errno) => return linux_errno(errno),
    };
    ipc_trace!(
        "ipc call copied: endpoint={} request_len={}",
        endpoint.raw(),
        request.len()
    );
    let copy_ticks = crate::arch::rtc::ticks();
    let reply = match enqueue_call_and_wake(endpoint, request.as_slice()) {
        Ok(reply) => reply,
        Err(errno) => return linux_errno(errno),
    };
    ipc_trace!(
        "ipc call enqueued: endpoint={} request_len={} reply_cap={}",
        endpoint.raw(),
        request.len(),
        reply.raw()
    );
    let send_ticks = crate::arch::rtc::ticks();
    ipc_trace!("ipc call waiting: endpoint={}", endpoint.raw());
    let response = match wait_for_service_reply_with_timeout(reply, timeout_ms) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    let wait_ticks = crate::arch::rtc::ticks();
    let Ok(reply_capacity) = usize::try_from(reply_capacity) else {
        return linux_errno(LINUX_EINVAL);
    };
    if response.len() > reply_capacity {
        return linux_errno(LINUX_EOVERFLOW);
    }
    if !response.is_empty()
        && let Err(err) = usermem::write_current_user_bytes(reply_ptr, response.as_slice())
    {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let write_ticks = crate::arch::rtc::ticks();
    log_slow_ipc_call(
        "call",
        endpoint.raw(),
        start_ticks,
        copy_ticks,
        copy_ticks,
        send_ticks,
        wait_ticks,
        write_ticks,
        request.len(),
        response.len(),
    );
    response.len() as u64
}

pub(super) fn syscall_linux_rustos_ipc_recv(
    endpoint: u64,
    request_ptr: u64,
    request_capacity: u64,
    reply_cap_ptr: u64,
) -> u64 {
    let endpoint = KernelEndpointHandle::from_raw(endpoint);
    let Some(task_id) = multitask::current_task_id() else {
        return linux_errno(LINUX_EINVAL);
    };
    let Some(process_id) = multitask::current_user_process_id() else {
        return linux_errno(LINUX_EINVAL);
    };
    if let Err(err) =
        kernel_ipc_runtime::api::authorize_endpoint_receiver_for_process(endpoint, process_id)
    {
        return linux_errno(ipc_error_to_linux_errno(err));
    }
    ipc_trace!(
        "ipc recv start: endpoint={} request_ptr={:#x} request_capacity={} reply_cap_ptr={:#x}",
        endpoint.raw(),
        request_ptr,
        request_capacity,
        reply_cap_ptr
    );
    let Ok(request_capacity) = usize::try_from(request_capacity) else {
        return linux_errno(LINUX_EINVAL);
    };
    loop {
        match kernel_ipc_runtime::api::recv_endpoint_with_sender_and_limits(
            endpoint,
            request_capacity,
            0,
        ) {
            Ok(Some((reply, request, _handles, caller_task_id))) => {
                ipc_trace!(
                    "ipc recv delivered: endpoint={} request_len={} reply_cap={}",
                    endpoint.raw(),
                    request.len(),
                    reply.raw()
                );
                if !request.is_empty()
                    && let Err(err) = usermem::write_current_user_bytes(request_ptr, &request)
                {
                    return linux_errno(address_space_error_to_linux_errno(err));
                }
                if let Err(err) =
                    usermem::write_current_user_bytes(reply_cap_ptr, &reply.raw().to_ne_bytes())
                {
                    return linux_errno(address_space_error_to_linux_errno(err));
                }
                let _ = multitask::inherit_ipc_priority(reply.raw(), caller_task_id, task_id);
                return request.len() as u64;
            }
            Ok(None) => {
                if !multitask::arm_block_current_task() {
                    return linux_errno(LINUX_EINVAL);
                }
                let pending = match kernel_ipc_runtime::api::add_endpoint_receiver_waiter(
                    endpoint, task_id,
                ) {
                    Ok(pending) => pending,
                    Err(err) => {
                        let _ = multitask::cancel_block_current_task();
                        return linux_errno(ipc_error_to_linux_errno(err));
                    }
                };
                ipc_trace!(
                    "ipc recv wait armed: endpoint={} task={} pending={}",
                    endpoint.raw(),
                    task_id,
                    pending
                );
                if pending {
                    let _ = multitask::cancel_block_current_task();
                    continue;
                }
                match multitask::commit_block_current_task_and_yield() {
                    Some(true) => {}
                    Some(false) => {}
                    None => return linux_errno(LINUX_EINVAL),
                }
            }
            Err(err) => return linux_errno(ipc_error_to_linux_errno(err)),
        }
    }
}

pub(super) fn syscall_linux_rustos_ipc_try_recv(
    endpoint: u64,
    request_ptr: u64,
    request_capacity: u64,
    reply_cap_ptr: u64,
) -> u64 {
    ipc_trace!(
        "ipc try recv start: endpoint={} request_ptr={:#x} request_capacity={} reply_cap_ptr={:#x}",
        endpoint,
        request_ptr,
        request_capacity,
        reply_cap_ptr
    );
    recv_endpoint_once(endpoint, request_ptr, request_capacity, reply_cap_ptr)
        .map_or_else(linux_errno, |received| received as u64)
}

pub(super) fn syscall_linux_rustos_ipc_try_recv_with_sender(
    endpoint: u64,
    request_ptr: u64,
    request_capacity: u64,
    reply_cap_ptr: u64,
    sender_pid_ptr: u64,
    sender_tid_ptr: u64,
) -> u64 {
    // Sender identity is metadata on a receive capability, not rootd policy.
    // `recv_endpoint_once_with_sender` re-authorizes the exact endpoint owner
    // before exposing either request bytes or kernel-stamped caller identity.
    recv_endpoint_once_with_sender(
        endpoint,
        request_ptr,
        request_capacity,
        reply_cap_ptr,
        sender_pid_ptr,
        sender_tid_ptr,
    )
    .map_or_else(linux_errno, |received| received as u64)
}

pub(super) fn syscall_linux_rustos_ipc_recv_with_sender(
    endpoint: u64,
    request_ptr: u64,
    request_capacity: u64,
    reply_cap_ptr: u64,
    sender_pid_ptr: u64,
    sender_tid_ptr: u64,
) -> u64 {
    let (endpoint, task_id, _process_id, request_capacity) = match prepare_recv_with_sender(
        endpoint,
        request_ptr,
        request_capacity,
        reply_cap_ptr,
        sender_pid_ptr,
        sender_tid_ptr,
    ) {
        Ok(prepared) => prepared,
        Err(errno) => return linux_errno(errno),
    };
    match recv_with_sender_blocking_prepared(
        endpoint,
        task_id,
        request_ptr,
        request_capacity,
        reply_cap_ptr,
        sender_pid_ptr,
        sender_tid_ptr,
    ) {
        Ok((received, _yielded)) => received as u64,
        Err((errno, _yielded)) => linux_errno(errno),
    }
}

fn prepare_recv_with_sender(
    endpoint: u64,
    request_ptr: u64,
    request_capacity: u64,
    reply_cap_ptr: u64,
    sender_pid_ptr: u64,
    sender_tid_ptr: u64,
) -> Result<(KernelEndpointHandle, u64, u64, usize), i64> {
    let endpoint = KernelEndpointHandle::from_raw(endpoint);
    let task_id = multitask::current_task_id().ok_or(LINUX_EINVAL)?;
    let process_id = multitask::current_user_process_id().ok_or(LINUX_EINVAL)?;
    kernel_ipc_runtime::api::authorize_endpoint_receiver_for_process(endpoint, process_id)
        .map_err(ipc_error_to_linux_errno)?;
    let request_capacity = usize::try_from(request_capacity).map_err(|_| LINUX_EINVAL)?;
    if request_capacity > rustos_user_abi::syscall::IPC_MAX_INLINE_BYTES {
        return Err(LINUX_EINVAL);
    }
    if request_capacity > 0 {
        usermem::validate_current_user_write_buffer(request_ptr, request_capacity)
            .map_err(address_space_error_to_linux_errno)?;
    }
    usermem::validate_current_user_write_buffer(reply_cap_ptr, size_of::<u64>())
        .map_err(address_space_error_to_linux_errno)?;
    usermem::validate_current_user_write_buffer(sender_pid_ptr, size_of::<u64>())
        .map_err(address_space_error_to_linux_errno)?;
    usermem::validate_current_user_write_buffer(sender_tid_ptr, size_of::<u64>())
        .map_err(address_space_error_to_linux_errno)?;
    Ok((endpoint, task_id, process_id, request_capacity))
}

/// Receives on an already-authorized endpoint after every user output range
/// has been validated.  The boolean records whether this invocation actually
/// committed a block and crossed the scheduler; reply-receive uses it to avoid
/// issuing a redundant syscall-tail reschedule after the exact caller already
/// received its direct handoff.
fn recv_with_sender_blocking_prepared(
    endpoint: KernelEndpointHandle,
    task_id: u64,
    request_ptr: u64,
    request_capacity: usize,
    reply_cap_ptr: u64,
    sender_pid_ptr: u64,
    sender_tid_ptr: u64,
) -> Result<(usize, bool), (i64, bool)> {
    let mut yielded = false;
    loop {
        match kernel_ipc_runtime::api::recv_endpoint_with_sender_and_limits(
            endpoint,
            request_capacity,
            0,
        ) {
            Ok(Some((reply, request, _handles, caller_task_id))) => {
                let (sender_pid, sender_tid) =
                    multitask::user_log_ids_for_task(caller_task_id).unwrap_or((0, 0));
                if !request.is_empty() {
                    usermem::write_current_user_bytes(request_ptr, &request)
                        .map_err(|err| (address_space_error_to_linux_errno(err), yielded))?;
                }
                usermem::write_current_user_bytes(reply_cap_ptr, &reply.raw().to_ne_bytes())
                    .map_err(|err| (address_space_error_to_linux_errno(err), yielded))?;
                usermem::write_current_user_bytes(sender_pid_ptr, &sender_pid.to_ne_bytes())
                    .map_err(|err| (address_space_error_to_linux_errno(err), yielded))?;
                usermem::write_current_user_bytes(sender_tid_ptr, &sender_tid.to_ne_bytes())
                    .map_err(|err| (address_space_error_to_linux_errno(err), yielded))?;
                let _ = multitask::inherit_ipc_priority(reply.raw(), caller_task_id, task_id);
                return Ok((request.len(), yielded));
            }
            Ok(None) => {
                if !multitask::arm_block_current_task() {
                    return Err((LINUX_EINVAL, yielded));
                }
                let pending = match kernel_ipc_runtime::api::add_endpoint_receiver_waiter(
                    endpoint, task_id,
                ) {
                    Ok(pending) => pending,
                    Err(err) => {
                        let _ = multitask::cancel_block_current_task();
                        return Err((ipc_error_to_linux_errno(err), yielded));
                    }
                };
                if pending {
                    let _ = multitask::cancel_block_current_task();
                    continue;
                }
                match multitask::commit_block_current_task_and_yield() {
                    Some(true) => yielded = true,
                    Some(false) => continue,
                    None => return Err((LINUX_EINVAL, yielded)),
                }
            }
            Err(err) => return Err((ipc_error_to_linux_errno(err), yielded)),
        }
    }
}

fn recv_endpoint_once(
    endpoint: u64,
    request_ptr: u64,
    request_capacity: u64,
    reply_cap_ptr: u64,
) -> Result<usize, i64> {
    let endpoint = KernelEndpointHandle::from_raw(endpoint);
    let process_id = multitask::current_user_process_id().ok_or(LINUX_EINVAL)?;
    kernel_ipc_runtime::api::authorize_endpoint_receiver_for_process(endpoint, process_id)
        .map_err(ipc_error_to_linux_errno)?;
    let request_capacity = usize::try_from(request_capacity).map_err(|_| LINUX_EINVAL)?;
    let receiver_task_id = multitask::current_task_id().ok_or(LINUX_EINVAL)?;
    match kernel_ipc_runtime::api::recv_endpoint_with_sender_and_limits(
        endpoint,
        request_capacity,
        0,
    ) {
        Ok(Some((reply, request, _handles, caller_task_id))) => {
            if !request.is_empty() {
                usermem::write_current_user_bytes(request_ptr, &request)
                    .map_err(address_space_error_to_linux_errno)?;
            }
            usermem::write_current_user_bytes(reply_cap_ptr, &reply.raw().to_ne_bytes())
                .map_err(address_space_error_to_linux_errno)?;
            let _ = multitask::inherit_ipc_priority(reply.raw(), caller_task_id, receiver_task_id);
            Ok(request.len())
        }
        Ok(None) => Err(LINUX_EAGAIN),
        Err(err) => Err(ipc_error_to_linux_errno(err)),
    }
}

fn recv_endpoint_once_with_sender(
    endpoint: u64,
    request_ptr: u64,
    request_capacity: u64,
    reply_cap_ptr: u64,
    sender_pid_ptr: u64,
    sender_tid_ptr: u64,
) -> Result<usize, i64> {
    let endpoint = KernelEndpointHandle::from_raw(endpoint);
    let process_id = multitask::current_user_process_id().ok_or(LINUX_EINVAL)?;
    kernel_ipc_runtime::api::authorize_endpoint_receiver_for_process(endpoint, process_id)
        .map_err(ipc_error_to_linux_errno)?;
    let request_capacity = usize::try_from(request_capacity).map_err(|_| LINUX_EINVAL)?;
    if request_capacity > rustos_user_abi::syscall::IPC_MAX_INLINE_BYTES {
        return Err(LINUX_EINVAL);
    }
    if request_capacity > 0 {
        usermem::validate_current_user_write_buffer(request_ptr, request_capacity)
            .map_err(address_space_error_to_linux_errno)?;
    }
    usermem::validate_current_user_write_buffer(reply_cap_ptr, size_of::<u64>())
        .map_err(address_space_error_to_linux_errno)?;
    usermem::validate_current_user_write_buffer(sender_pid_ptr, size_of::<u64>())
        .map_err(address_space_error_to_linux_errno)?;
    usermem::validate_current_user_write_buffer(sender_tid_ptr, size_of::<u64>())
        .map_err(address_space_error_to_linux_errno)?;

    match kernel_ipc_runtime::api::recv_endpoint_with_sender_and_limits(
        endpoint,
        request_capacity,
        0,
    ) {
        Ok(Some((reply, request, _handles, caller_task_id))) => {
            let (sender_pid, sender_tid) =
                multitask::user_log_ids_for_task(caller_task_id).unwrap_or((0, 0));
            if !request.is_empty() {
                usermem::write_current_user_bytes(request_ptr, &request)
                    .map_err(address_space_error_to_linux_errno)?;
            }
            usermem::write_current_user_bytes(reply_cap_ptr, &reply.raw().to_ne_bytes())
                .map_err(address_space_error_to_linux_errno)?;
            usermem::write_current_user_bytes(sender_pid_ptr, &sender_pid.to_ne_bytes())
                .map_err(address_space_error_to_linux_errno)?;
            usermem::write_current_user_bytes(sender_tid_ptr, &sender_tid.to_ne_bytes())
                .map_err(address_space_error_to_linux_errno)?;
            let receiver_task_id = multitask::current_task_id().ok_or(LINUX_EINVAL)?;
            let _ = multitask::inherit_ipc_priority(reply.raw(), caller_task_id, receiver_task_id);
            Ok(request.len())
        }
        Ok(None) => Err(LINUX_EAGAIN),
        Err(err) => Err(ipc_error_to_linux_errno(err)),
    }
}

pub(super) fn syscall_linux_rustos_ipc_reply(
    reply: u64,
    response_ptr: u64,
    response_len: u64,
) -> u64 {
    let start_ticks = crate::arch::rtc::ticks();
    let response = match copy_request_from_user(response_ptr, response_len) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    let copy_ticks = crate::arch::rtc::ticks();
    let Some(receiver_process_id) = multitask::current_user_process_id() else {
        return linux_errno(LINUX_EINVAL);
    };
    let task_id = match kernel_ipc_runtime::api::complete_endpoint_reply_for_process(
        KernelReplyHandle::from_raw(reply),
        receiver_process_id,
        response.as_slice(),
    ) {
        Ok(task_id) => task_id,
        Err(err) => {
            record_ipc_reply_rejection(reply, receiver_process_id, err);
            return linux_errno(ipc_error_to_linux_errno(err));
        }
    };
    let _ = multitask::release_ipc_priority(reply);
    let reply_ticks = crate::arch::rtc::ticks();
    let woke = multitask::wake_task(task_id);
    // Direct hand-back to the caller: the service is about to wait on its
    // endpoint again, so donate the remaining quantum to the original caller
    // instead of round-robining away from a freshly-completed reply.
    if woke && multitask::set_next_synchronous_pick_hint(task_id) {
        // The common syscall tail consumes this request with IF enabled. The
        // shared synchronous IPC FIFO is burst-bounded, so direct hand-back
        // cannot erase the scheduler's overdue-task fairness turn.
        multitask::request_deferred_reschedule();
    }
    log_slow_ipc_reply(
        "reply",
        reply,
        start_ticks,
        copy_ticks,
        reply_ticks,
        response.len(),
    );
    0
}

pub(super) fn syscall_linux_rustos_ipc_call_with_handles(args_ptr: u64) -> u64 {
    syscall_linux_rustos_ipc_call_with_handles_timeout(args_ptr, SERVICE_IPC_TIMEOUT_MS)
}

pub(super) fn syscall_linux_rustos_ipc_call_with_handles_bounded(
    args_ptr: u64,
    timeout_ms: u64,
) -> u64 {
    if !bounded_ipc_call_timeout_is_valid(timeout_ms) {
        return linux_errno(LINUX_EINVAL);
    }
    syscall_linux_rustos_ipc_call_with_handles_timeout(args_ptr, timeout_ms)
}

fn syscall_linux_rustos_ipc_call_with_handles_timeout(args_ptr: u64, timeout_ms: u64) -> u64 {
    let args = match usermem::read_current_user_struct::<
        rustos_user_abi::syscall::IpcCallWithHandlesArgs,
    >(args_ptr)
    {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.reserved0 != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    if let Err(errno) = authorize_current_process_ipc_call(args.endpoint) {
        return linux_errno(errno);
    }

    let start_ticks = crate::arch::rtc::ticks();
    let request = match copy_request_from_user(args.request_ptr, args.request_len) {
        Ok(request) => request,
        Err(errno) => return linux_errno(errno),
    };
    let copy_ticks = crate::arch::rtc::ticks();
    let send_handles = match export_current_fds_for_ipc(args.send_fds_ptr, args.send_fd_count) {
        Ok(handles) => handles,
        Err(errno) => return linux_errno(errno),
    };
    let export_ticks = crate::arch::rtc::ticks();
    let reply = match enqueue_call_and_wake_with_handles(
        KernelEndpointHandle::from_raw(args.endpoint),
        request.as_slice(),
        send_handles.as_slice(),
    ) {
        Ok(reply) => reply,
        Err(errno) => {
            drop_transfer_descriptors(send_handles.as_slice());
            return linux_errno(errno);
        }
    };
    let send_ticks = crate::arch::rtc::ticks();

    let (response, reply_handles) = match wait_for_service_reply_with_handle_limit_after(
        reply,
        rustos_user_abi::syscall::IPC_MAX_TRANSFER_HANDLES,
        timeout_ms,
    ) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    let wait_ticks = crate::arch::rtc::ticks();
    let Ok(reply_capacity) = usize::try_from(args.reply_capacity) else {
        drop_transfer_descriptors(reply_handles.as_slice());
        return linux_errno(LINUX_EINVAL);
    };
    if response.len() > reply_capacity {
        drop_transfer_descriptors(reply_handles.as_slice());
        return linux_errno(LINUX_EOVERFLOW);
    }
    if reply_capacity > 0
        && let Err(err) =
            usermem::validate_current_user_write_buffer(args.reply_ptr, reply_capacity)
    {
        drop_transfer_descriptors(reply_handles.as_slice());
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    if usize::from(args.recv_fd_capacity) < reply_handles.len() {
        drop_transfer_descriptors(reply_handles.as_slice());
        return linux_errno(LINUX_EOVERFLOW);
    }
    if let Err(errno) = validate_received_handle_outputs(
        args.recv_fds_ptr,
        args.recv_fd_count_ptr,
        reply_handles.len(),
    ) {
        drop_transfer_descriptors(reply_handles.as_slice());
        return linux_errno(errno);
    }
    if !response.is_empty()
        && let Err(err) = usermem::write_current_user_bytes(args.reply_ptr, response.as_slice())
    {
        drop_transfer_descriptors(reply_handles.as_slice());
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    if let Err(errno) = install_received_handles(
        reply_handles.as_slice(),
        args.recv_fds_ptr,
        args.recv_fd_count_ptr,
    ) {
        drop_transfer_descriptors(reply_handles.as_slice());
        return linux_errno(errno);
    }
    let write_ticks = crate::arch::rtc::ticks();
    log_slow_ipc_call(
        "call-with-handles",
        args.endpoint,
        start_ticks,
        copy_ticks,
        export_ticks,
        send_ticks,
        wait_ticks,
        write_ticks,
        request.len(),
        response.len(),
    );
    response.len() as u64
}

pub(super) fn syscall_linux_rustos_ipc_recv_with_handles(args_ptr: u64) -> u64 {
    let args = match usermem::read_current_user_struct::<
        rustos_user_abi::syscall::IpcRecvWithHandlesArgs,
    >(args_ptr)
    {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.reserved0 != 0 || args.reserved1 != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    let endpoint = KernelEndpointHandle::from_raw(args.endpoint);
    let Some(task_id) = multitask::current_task_id() else {
        return linux_errno(LINUX_EINVAL);
    };
    let Some(process_id) = multitask::current_user_process_id() else {
        return linux_errno(LINUX_EINVAL);
    };
    if let Err(err) =
        kernel_ipc_runtime::api::authorize_endpoint_receiver_for_process(endpoint, process_id)
    {
        return linux_errno(ipc_error_to_linux_errno(err));
    }
    let Ok(request_capacity) = usize::try_from(args.request_capacity) else {
        return linux_errno(LINUX_EINVAL);
    };
    if request_capacity > rustos_user_abi::syscall::IPC_MAX_INLINE_BYTES {
        return linux_errno(LINUX_EINVAL);
    }
    if usize::from(args.recv_fd_capacity) > rustos_user_abi::syscall::IPC_MAX_TRANSFER_HANDLES {
        return linux_errno(LINUX_EINVAL);
    }
    if request_capacity > 0
        && let Err(err) =
            usermem::validate_current_user_write_buffer(args.request_ptr, request_capacity)
    {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    if let Err(err) =
        usermem::validate_current_user_write_buffer(args.reply_cap_ptr, size_of::<u64>())
    {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    if let Err(errno) =
        validate_received_handle_outputs(args.recv_fds_ptr, args.recv_fd_count_ptr, 0)
    {
        return linux_errno(errno);
    }

    loop {
        match kernel_ipc_runtime::api::recv_endpoint_with_sender_and_limits(
            endpoint,
            request_capacity,
            usize::from(args.recv_fd_capacity),
        ) {
            Ok(Some((reply, request, handles, caller_task_id))) => {
                if let Err(errno) = validate_received_handle_outputs(
                    args.recv_fds_ptr,
                    args.recv_fd_count_ptr,
                    handles.len(),
                ) {
                    drop_transfer_descriptors(handles.as_slice());
                    return linux_errno(errno);
                }
                if !request.is_empty()
                    && let Err(err) = usermem::write_current_user_bytes(args.request_ptr, &request)
                {
                    drop_transfer_descriptors(handles.as_slice());
                    return linux_errno(address_space_error_to_linux_errno(err));
                }
                if let Err(err) = usermem::write_current_user_bytes(
                    args.reply_cap_ptr,
                    &reply.raw().to_ne_bytes(),
                ) {
                    drop_transfer_descriptors(handles.as_slice());
                    return linux_errno(address_space_error_to_linux_errno(err));
                }
                if let Err(errno) = install_received_handles(
                    handles.as_slice(),
                    args.recv_fds_ptr,
                    args.recv_fd_count_ptr,
                ) {
                    drop_transfer_descriptors(handles.as_slice());
                    return linux_errno(errno);
                }
                let _ = multitask::inherit_ipc_priority(reply.raw(), caller_task_id, task_id);
                return request.len() as u64;
            }
            Ok(None) => {
                if !multitask::arm_block_current_task() {
                    return linux_errno(LINUX_EINVAL);
                }
                let pending = match kernel_ipc_runtime::api::add_endpoint_receiver_waiter(
                    endpoint, task_id,
                ) {
                    Ok(pending) => pending,
                    Err(err) => {
                        let _ = multitask::cancel_block_current_task();
                        return linux_errno(ipc_error_to_linux_errno(err));
                    }
                };
                if pending {
                    let _ = multitask::cancel_block_current_task();
                    continue;
                }
                match multitask::commit_block_current_task_and_yield() {
                    Some(true) => {}
                    Some(false) => {}
                    None => return linux_errno(LINUX_EINVAL),
                }
            }
            Err(err) => return linux_errno(ipc_error_to_linux_errno(err)),
        }
    }
}

pub(super) fn syscall_linux_rustos_ipc_reply_with_handles(args_ptr: u64) -> u64 {
    let args = match usermem::read_current_user_struct::<
        rustos_user_abi::syscall::IpcReplyWithHandlesArgs,
    >(args_ptr)
    {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.reserved0 != 0 || args.reserved1 != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    let response = match copy_request_from_user(args.response_ptr, args.response_len) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    let send_handles = match export_current_fds_for_ipc(args.send_fds_ptr, args.send_fd_count) {
        Ok(handles) => handles,
        Err(errno) => return linux_errno(errno),
    };
    let Some(receiver_process_id) = multitask::current_user_process_id() else {
        drop_transfer_descriptors(send_handles.as_slice());
        return linux_errno(LINUX_EINVAL);
    };
    let task_id = match kernel_ipc_runtime::api::complete_endpoint_reply_with_handles_for_process(
        KernelReplyHandle::from_raw(args.reply_cap),
        receiver_process_id,
        response.as_slice(),
        send_handles.as_slice(),
    ) {
        Ok(task_id) => task_id,
        Err(err) => {
            drop_transfer_descriptors(send_handles.as_slice());
            record_ipc_reply_rejection(args.reply_cap, receiver_process_id, err);
            return linux_errno(ipc_error_to_linux_errno(err));
        }
    };
    let _ = multitask::release_ipc_priority(args.reply_cap);
    if multitask::wake_task(task_id) && multitask::set_next_synchronous_pick_hint(task_id) {
        multitask::request_deferred_reschedule();
    }
    0
}

pub(super) fn call_linux_syscall_endpoint(request: &[u8]) -> Result<Vec<u8>, i64> {
    let endpoint = linux_syscall_endpoint().ok_or(LINUX_ENOSYS)?;
    let start_ticks = crate::arch::rtc::ticks();
    let reply = enqueue_call_and_wake(endpoint, request)?;
    let send_ticks = crate::arch::rtc::ticks();
    let response = wait_for_service_reply(reply)?;
    let reply_ticks = crate::arch::rtc::ticks();
    log_slow_ipc_call(
        "linux-syscall",
        endpoint.raw(),
        start_ticks,
        start_ticks,
        start_ticks,
        send_ticks,
        reply_ticks,
        reply_ticks,
        request.len(),
        response.len(),
    );
    Ok(response)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ServiceIpcClass {
    ReadinessQuery,
    InteractiveControl,
    BootControl,
    BulkData,
}

impl ServiceIpcClass {
    pub(super) const fn timeout_ms(self) -> u64 {
        match self {
            Self::ReadinessQuery => rustos_user_abi::performance::IPC_READINESS_QUERY_HARD_LIMIT_MS,
            Self::InteractiveControl => {
                rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS
            }
            Self::BootControl => rustos_user_abi::performance::IPC_BOOT_CONTROL_HARD_LIMIT_MS,
            Self::BulkData => rustos_user_abi::performance::IPC_BULK_DATA_HARD_LIMIT_MS,
        }
    }

    const fn cap_timeout_ms(self, requested_timeout_ms: u64) -> u64 {
        let requested = if requested_timeout_ms == 0 {
            1
        } else {
            requested_timeout_ms
        };
        let hard_limit = self.timeout_ms();
        if requested < hard_limit {
            requested
        } else {
            hard_limit
        }
    }
}

/// Diagnostic count of caller-side service reply deadline expiries.
static SERVICE_REPLY_TIMEOUTS: AtomicU64 = AtomicU64::new(0);
const EARLY_SERVICE_REPLY_TIMEOUT_SAMPLES: u64 = 8;

pub(super) fn call_service_endpoint_with_class(
    service_id: u64,
    request: &[u8],
    class: ServiceIpcClass,
) -> Result<Vec<u8>, i64> {
    call_service_endpoint_with_timeout(service_id, request, class.timeout_ms())
}

pub(super) fn call_service_endpoint_with_class_deadline(
    service_id: u64,
    request: &[u8],
    class: ServiceIpcClass,
    requested_timeout_ms: u64,
) -> Result<Vec<u8>, i64> {
    call_service_endpoint_with_timeout(
        service_id,
        request,
        class.cap_timeout_ms(requested_timeout_ms),
    )
}

fn call_service_endpoint_with_timeout(
    service_id: u64,
    request: &[u8],
    timeout_ms: u64,
) -> Result<Vec<u8>, i64> {
    let endpoint = service_endpoint(service_id).ok_or(LINUX_ENOSYS)?;
    let start_ticks = crate::arch::rtc::ticks();
    let reply = enqueue_call_and_wake(endpoint, request)?;
    let send_ticks = crate::arch::rtc::ticks();
    let response = wait_for_service_reply_with_timeout(reply, timeout_ms)?;
    let reply_ticks = crate::arch::rtc::ticks();
    log_slow_ipc_call(
        "service",
        endpoint.raw(),
        start_ticks,
        start_ticks,
        start_ticks,
        send_ticks,
        reply_ticks,
        reply_ticks,
        request.len(),
        response.len(),
    );
    Ok(response)
}

pub(super) fn call_service_endpoint_with_received_entries(
    service_id: u64,
    request: &[u8],
    handle_capacity: usize,
) -> Result<(Vec<u8>, Vec<multitask::TransferredHandleEntry>), i64> {
    let endpoint = service_endpoint(service_id).ok_or(LINUX_ENOSYS)?;
    let start_ticks = crate::arch::rtc::ticks();
    let reply = enqueue_call_and_wake(endpoint, request)?;
    let (response, descriptors) = wait_for_service_reply_with_handle_limit(reply, handle_capacity)?;
    let reply_ticks = crate::arch::rtc::ticks();
    let entries = take_transfer_entries(descriptors.as_slice())?;
    log_slow_ipc_call(
        "service-handles",
        endpoint.raw(),
        start_ticks,
        start_ticks,
        start_ticks,
        reply_ticks,
        reply_ticks,
        reply_ticks,
        request.len(),
        response.len(),
    );
    Ok((response, entries))
}

fn enqueue_call_and_wake(
    endpoint: KernelEndpointHandle,
    request: &[u8],
) -> Result<KernelReplyHandle, i64> {
    enqueue_call_and_wake_with_handles(endpoint, request, &[])
}

fn enqueue_call_and_wake_with_handles(
    endpoint: KernelEndpointHandle,
    request: &[u8],
    attached_handles: &[KernelTransferredHandle],
) -> Result<KernelReplyHandle, i64> {
    let task_id = multitask::current_task_id().ok_or(LINUX_EINVAL)?;
    // Queue priority is scheduler authority: derive it from the live task
    // slot, never from request bytes controlled by ring3. Sampling occurs
    // before the IPC slot lock, preserving the scheduler -> IPC lock order.
    let priority = if multitask::task_has_system_scheduling_class(task_id) {
        EndpointCallPriority::System
    } else {
        EndpointCallPriority::Ordinary
    };
    // A full donation table degrades the call's priority; it does not fail the
    // call.
    //
    // Priority inheritance is a scheduling optimisation. Returning `ENOSPC`
    // when the table is full turns a transient scheduling condition into a
    // terminal I/O error for the caller, and the caller has no way to tell the
    // two apart — the same defect shape as netd answering "not ready yet" with
    // `ENOSYS`, which `V5-DEADLINE-012` names. It cost a Wayland client: the
    // compositor's socket write returned `ENOSPC`, `wayland-server` treats that
    // as fatal, and WayClick's surface was retired mid-session with
    // `alive=true`.
    //
    // seL4 and QNX both degrade rather than fail here: without a donated
    // scheduling context the server runs at its own priority, which is slower,
    // not incorrect. The audit's own counterexample for
    // `V5-SCHED-DONATION-002` describes the same expectation — a caller that
    // cannot donate blocks without the boost.
    let donation_required =
        priority == EndpointCallPriority::System && multitask::reserve_ipc_priority(task_id);
    let priority = if priority == EndpointCallPriority::System && !donation_required {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Sched,
            "ipc-donation-capacity-degraded",
            task_id,
            0,
        );
        EndpointCallPriority::Ordinary
    } else {
        priority
    };
    let (reply, receiver_to_wake) =
        match kernel_ipc_runtime::api::enqueue_endpoint_call_with_handles_and_priority(
            endpoint,
            task_id,
            request,
            attached_handles,
            priority,
        ) {
            Ok(enqueued) => enqueued,
            Err(error) => {
                if donation_required {
                    let _ = multitask::cancel_ipc_priority_reservation(task_id);
                }
                return Err(ipc_error_to_linux_errno(error));
            }
        };
    ipc_trace!(
        "ipc call queued: endpoint={} receiver_to_wake={:?}",
        endpoint.raw(),
        receiver_to_wake
    );
    let receiver_process_id = kernel_ipc_runtime::api::endpoint::receiver_process_for_reply(reply);
    let mut donation_admitted = !donation_required;
    if receiver_to_wake.is_none()
        && let Some(receiver_process_id) = receiver_process_id
    {
        if donation_required {
            donation_admitted = multitask::bind_ipc_priority_to_process_worker(
                reply.raw(),
                task_id,
                receiver_process_id,
            )
            .is_some();
        } else {
            let _ = multitask::set_next_process_pick_hint(receiver_process_id);
        }
    }
    if receiver_to_wake.is_none() && donation_required && !donation_admitted {
        // No receiver was parked at publication time. Transfer the bounded
        // reservation to the reply itself; the concrete `IPC_RECV` worker
        // binds it before observing the request, while timeout/revoke can
        // release the same exact reply without process-wide boosting.
        donation_admitted = multitask::attach_reserved_ipc_priority(reply.raw(), task_id);
    }
    if let Some(receiver_task_id) = receiver_to_wake {
        // The reply capability is the lifetime authority for this donation.
        // Install it before the wake/handoff so a User-class server that is
        // directly needed by a System caller is eligible for the very next
        // pick, rather than being indefinitely deferred by unrelated System
        // pollers.
        let inherited = if donation_required {
            multitask::bind_reserved_ipc_priority(reply.raw(), task_id, receiver_task_id)
        } else {
            true
        };
        donation_admitted |= inherited;
        let woke = multitask::wake_task(receiver_task_id);
        ipc_trace!(
            "ipc call wake: endpoint={} receiver_task={} woke={} inherited={}",
            endpoint.raw(),
            receiver_task_id,
            woke,
            inherited,
        );
        // L4-style direct handoff hint: the caller still returns to arm its
        // reply wait before yielding, so a fast service reply cannot race a
        // not-yet-armed waiter. `wait_for_reply` performs the actual yield
        // after the wait state is committed.
        let _ = multitask::set_next_synchronous_pick_hint(receiver_task_id);
    }
    if !donation_admitted {
        // The scheduling edge could not be installed. This used to cancel the
        // reply and return `ENOSPC`, on the reasoning that a System caller must
        // not block after silently losing its donation. That reasoning was
        // wrong about the consequence: the caller does not block indefinitely,
        // because `wait_for_reply` arms a bounded deadline and returns
        // `ETIMEDOUT`, and the direct handoff hint below still runs. What the
        // cancellation did produce was a terminal `ENOSPC` on a syscall that
        // had already succeeded — observed as uiserver dying inside a thread
        // spawn with "failed to allocate an alternative stack: No space left on
        // device", which killed the compositor and the whole FPS proof with it.
        //
        // Donation capacity is a scheduling condition. Degrade the priority
        // edge and keep the call, exactly as a full donation table already
        // degrades at reservation time.
        let _ = multitask::cancel_ipc_priority_reservation(task_id);
        debug::record_milestone(
            debug::LogCategory::Sched,
            "ipc-donation-bind-degraded",
            task_id,
            reply.raw(),
        );
    }
    Ok(reply)
}

fn wait_for_service_reply(reply: KernelReplyHandle) -> Result<Vec<u8>, i64> {
    wait_for_service_reply_with_timeout(reply, SERVICE_IPC_TIMEOUT_MS)
}

fn wait_for_service_reply_with_timeout(
    reply: KernelReplyHandle,
    timeout_ms: u64,
) -> Result<Vec<u8>, i64> {
    match wait_for_reply_with_deadline(reply, 0, service_ipc_deadline_tick_after(timeout_ms)) {
        Ok(response) => Ok(response.0),
        Err(errno) => {
            // An abandoned reply capability is the start of a failure chain
            // that surfaces far from here: the service replies late, the reply
            // is rejected, a capability is never installed, and the next call
            // returns a permission error. Recording the exact deadline that
            // expired is what makes that chain attributable to its cause
            // instead of to the permission error it eventually looks like.
            record_service_reply_timeout(reply, timeout_ms, errno);
            Err(errno)
        }
    }
}

/// Publishes the exact caller-side deadline that a service reply missed.
///
/// Bounded to the first few occurrences and then to exponentially spaced
/// counts, matching the reply-rejection diagnostics this pairs with.
fn record_service_reply_timeout(reply: KernelReplyHandle, timeout_ms: u64, errno: i64) {
    // ORDERING: Relaxed is exact; this counter owns diagnostics only and
    // orders nothing.
    let total = SERVICE_REPLY_TIMEOUTS.fetch_add(1, Ordering::Relaxed) + 1;
    if total > EARLY_SERVICE_REPLY_TIMEOUT_SAMPLES && !total.is_power_of_two() {
        return;
    }
    nucleus_core::debug::record_milestone(
        nucleus_core::debug::LogCategory::Compat,
        "ipc-service-reply-timeout",
        reply.raw(),
        ((timeout_ms & 0xffff_ffff) << 32) | (errno.unsigned_abs() & 0xffff_ffff),
    );
}

fn wait_for_service_reply_with_handle_limit(
    reply: KernelReplyHandle,
    handle_capacity: usize,
) -> Result<(Vec<u8>, Vec<KernelTransferredHandle>), i64> {
    wait_for_service_reply_with_handle_limit_after(reply, handle_capacity, SERVICE_IPC_TIMEOUT_MS)
}

fn wait_for_service_reply_with_handle_limit_after(
    reply: KernelReplyHandle,
    handle_capacity: usize,
    timeout_ms: u64,
) -> Result<(Vec<u8>, Vec<KernelTransferredHandle>), i64> {
    wait_for_reply_with_deadline(
        reply,
        handle_capacity,
        service_ipc_deadline_tick_after(timeout_ms),
    )
}

fn wait_for_reply_with_deadline(
    reply: KernelReplyHandle,
    handle_capacity: usize,
    deadline_tick: u64,
) -> Result<(Vec<u8>, Vec<KernelTransferredHandle>), i64> {
    let caller_task_id = multitask::current_task_id().ok_or(LINUX_EINVAL)?;
    loop {
        match take_endpoint_response_for_wait(reply, handle_capacity) {
            Ok(Some(response)) => {
                disarm_reply_deadline_waiter(caller_task_id);
                return Ok(response);
            }
            Ok(None) => {}
            Err(errno) => {
                disarm_reply_deadline_waiter(caller_task_id);
                record_ipc_reply_wait_failure(reply, caller_task_id, errno);
                return Err(errno);
            }
        }
        if reply_deadline_expired(deadline_tick) {
            cancel_reply_wait(reply, caller_task_id, ReplyCancelReason::DeadlineBeforeArm);
            return Err(LINUX_ETIMEDOUT);
        }
        if !multitask::arm_block_current_task() {
            cancel_reply_wait(reply, caller_task_id, ReplyCancelReason::InvalidArm);
            return Err(LINUX_EINVAL);
        }
        if !arm_reply_deadline_waiter(caller_task_id, deadline_tick) {
            let _ = multitask::cancel_block_current_task();
            multitask::yield_now();
            continue;
        }
        // Re-poll after arming. If the replier completed the response between our
        // first take and arming, the wake_task call landed before arm_block, so
        // wake_armed would be set but no further wake arrives; re-checking the
        // queue here picks up that response without sleeping.
        match take_endpoint_response_for_wait(reply, handle_capacity) {
            Ok(Some(response)) => {
                disarm_reply_deadline_waiter(caller_task_id);
                let _ = multitask::cancel_block_current_task();
                return Ok(response);
            }
            Ok(None) => {}
            Err(errno) => {
                disarm_reply_deadline_waiter(caller_task_id);
                let _ = multitask::cancel_block_current_task();
                record_ipc_reply_wait_failure(reply, caller_task_id, errno);
                return Err(errno);
            }
        }
        if reply_deadline_expired(deadline_tick) {
            disarm_reply_deadline_waiter(caller_task_id);
            let _ = multitask::wake_task(caller_task_id);
            cancel_reply_wait(reply, caller_task_id, ReplyCancelReason::DeadlineAfterArm);
            return Err(LINUX_ETIMEDOUT);
        }
        match multitask::commit_block_current_task_and_yield() {
            Some(true) => {
                disarm_reply_deadline_waiter(caller_task_id);
            }
            Some(false) => {
                disarm_reply_deadline_waiter(caller_task_id);
                continue;
            }
            None => {
                disarm_reply_deadline_waiter(caller_task_id);
                cancel_reply_wait(reply, caller_task_id, ReplyCancelReason::InvalidCommit);
                return Err(LINUX_EINVAL);
            }
        }
    }
}

type EndpointWaitResponse = (Vec<u8>, Vec<KernelTransferredHandle>);

fn take_endpoint_response_for_wait(
    reply: KernelReplyHandle,
    handle_capacity: usize,
) -> Result<Option<EndpointWaitResponse>, i64> {
    match kernel_ipc_runtime::api::take_endpoint_response_detailed(reply, handle_capacity) {
        Ok(kernel_ipc_runtime::api::EndpointResponseTake::Pending) => Ok(None),
        Ok(kernel_ipc_runtime::api::EndpointResponseTake::Response(response)) => {
            let _ = multitask::release_ipc_priority(reply.raw());
            Ok(Some(response))
        }
        Ok(kernel_ipc_runtime::api::EndpointResponseTake::Error {
            error,
            discarded_request_handles,
        }) => {
            let _ = multitask::release_ipc_priority(reply.raw());
            drop_transfer_descriptors(discarded_request_handles.as_slice());
            Err(ipc_error_to_linux_errno(error))
        }
        Err(err) => Err(ipc_error_to_linux_errno(err)),
    }
}

fn service_ipc_deadline_tick_after(timeout_ms: u64) -> u64 {
    let ticks_per_second = crate::arch::rtc::ticks_per_second().max(1);
    let timeout_ticks = timeout_ms
        .saturating_mul(ticks_per_second)
        .saturating_add(999)
        .saturating_div(1000)
        .max(1);
    crate::arch::rtc::ticks().saturating_add(timeout_ticks)
}

fn reply_deadline_expired(deadline_tick: u64) -> bool {
    crate::arch::rtc::ticks() >= deadline_tick
}

fn arm_reply_deadline_waiter(task_id: u64, deadline_tick: u64) -> bool {
    crate::arch::rtc::arm_sleep_waiter_until_tick(task_id, deadline_tick)
}

fn disarm_reply_deadline_waiter(task_id: u64) {
    crate::arch::rtc::disarm_sleep_waiter(task_id);
}

#[repr(u64)]
#[derive(Clone, Copy)]
enum ReplyCancelReason {
    DeadlineBeforeArm = 1,
    InvalidArm = 2,
    DeadlineAfterArm = 3,
    InvalidCommit = 4,
}

fn cancel_reply_wait(reply: KernelReplyHandle, caller_task_id: u64, reason: ReplyCancelReason) {
    let result =
        kernel_ipc_runtime::api::cancel_endpoint_call_with_transfers(reply, caller_task_id);
    if let Ok(discarded) = &result {
        let _ = multitask::release_ipc_priority(reply.raw());
        drop_transfer_descriptors(discarded.as_slice());
    }
    let status = u64::from(result.is_err());
    let milestone = match reason {
        ReplyCancelReason::DeadlineBeforeArm | ReplyCancelReason::DeadlineAfterArm => {
            "ipc-reply-timeout"
        }
        ReplyCancelReason::InvalidArm | ReplyCancelReason::InvalidCommit => "ipc-reply-cancelled",
    };
    debug::record_milestone(
        debug::LogCategory::Compat,
        milestone,
        reply.raw(),
        ((caller_task_id & 0xffff_ffff) << 32) | ((reason as u64) << 1) | status,
    );
    ipc_trace!(
        "ipc reply cancellation: reply={} caller={} reason={} cancel={:?}",
        reply.raw(),
        caller_task_id,
        reason as u64,
        result
    );
}

fn record_ipc_reply_wait_failure(reply: KernelReplyHandle, caller_task_id: u64, errno: i64) {
    debug::record_milestone(
        debug::LogCategory::Compat,
        "ipc-reply-wait-failed",
        reply.raw(),
        ((caller_task_id & 0xffff_ffff) << 32) | (errno.unsigned_abs() & 0xffff_ffff),
    );
}

fn export_current_fds_for_ipc(
    fds_ptr: u64,
    fd_count: u16,
) -> Result<Vec<KernelTransferredHandle>, i64> {
    let fd_count = usize::from(fd_count);
    if fd_count == 0 {
        return Ok(Vec::new());
    }
    if fd_count > rustos_user_abi::syscall::IPC_MAX_TRANSFER_HANDLES || fds_ptr == 0 {
        return Err(LINUX_EINVAL);
    }

    let fds = read_user_fd_array(fds_ptr, fd_count)?;
    export_current_fds_for_transfer(fds.as_slice())
}

pub(super) fn export_current_fds_for_transfer(
    fds: &[i32],
) -> Result<Vec<KernelTransferredHandle>, i64> {
    if fds.len() > rustos_user_abi::syscall::IPC_MAX_TRANSFER_HANDLES {
        return Err(LINUX_EINVAL);
    }
    let Some(entries) = multitask::with_current_user_process_state(|_, _, process_state| {
        let mut entries = Vec::with_capacity(fds.len());
        for &fd in fds {
            if fd < 0 {
                return Err(LINUX_EBADF);
            }
            let fd = fd as u64;
            let Some(entry) = process_state.handles().get_entry(fd) else {
                return Err(LINUX_EBADF);
            };
            let Some(transferred) = multitask::TransferredHandleEntry::from_entry(entry.clone())
            else {
                return Err(LINUX_EACCES);
            };
            entries.push(transferred);
        }
        Ok(entries)
    }) else {
        return Err(LINUX_EINVAL);
    };
    let entries = entries?;

    let service_refs = service_transfer_refs(&entries);
    let mut acquired_refs = Vec::with_capacity(service_refs.len());
    for handle_ref in service_refs {
        if let Err(errno) = super::service_ops::acquire_service_handle_ref(&handle_ref) {
            super::service_ops::release_service_handle_refs(&acquired_refs);
            return Err(errno);
        }
        acquired_refs.push(handle_ref);
    }

    match multitask::register_ipc_transfer_entries(entries) {
        Ok(descriptors) => Ok(descriptors),
        Err(err) => {
            super::service_ops::release_service_handle_refs(&acquired_refs);
            Err(ipc_transfer_error_to_linux_errno(err))
        }
    }
}

fn read_user_fd_array(fds_ptr: u64, fd_count: usize) -> Result<Vec<i32>, i64> {
    let byte_len = fd_count.checked_mul(size_of::<i32>()).ok_or(LINUX_EINVAL)?;
    let mut bytes = alloc::vec![0_u8; byte_len];
    usermem::copy_from_current_user_exact(fds_ptr, &mut bytes)
        .map_err(address_space_error_to_linux_errno)?;
    let mut fds = Vec::with_capacity(fd_count);
    for chunk in bytes.chunks_exact(size_of::<i32>()) {
        fds.push(i32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Ok(fds)
}

fn validate_received_handle_outputs(
    fds_ptr: u64,
    fd_count_ptr: u64,
    fd_count: usize,
) -> Result<(), i64> {
    if fd_count > rustos_user_abi::syscall::IPC_MAX_TRANSFER_HANDLES {
        return Err(LINUX_EINVAL);
    }
    if fd_count_ptr == 0 {
        return Err(LINUX_EINVAL);
    }
    usermem::validate_current_user_write_buffer(fd_count_ptr, size_of::<u16>())
        .map_err(address_space_error_to_linux_errno)?;
    if fd_count == 0 {
        return Ok(());
    }
    if fds_ptr == 0 {
        return Err(LINUX_EINVAL);
    }
    usermem::validate_current_user_write_buffer(fds_ptr, fd_count * size_of::<i32>())
        .map_err(address_space_error_to_linux_errno)?;
    Ok(())
}

fn install_received_handles(
    descriptors: &[KernelTransferredHandle],
    fds_ptr: u64,
    fd_count_ptr: u64,
) -> Result<(), i64> {
    validate_received_handle_outputs(fds_ptr, fd_count_ptr, descriptors.len())?;
    let entries = take_transfer_entries(descriptors)?;
    let service_refs = service_transfer_refs(&entries);
    let Some(slots) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        process_state.handles_mut().reserve_slots(entries.len())
    }) else {
        super::service_ops::release_service_handle_refs_bounded(&service_refs);
        return Err(LINUX_EINVAL);
    };
    let Some((reservation_id, slots)) = slots else {
        super::service_ops::release_service_handle_refs_bounded(&service_refs);
        return Err(LINUX_EMFILE);
    };
    let fds = match slots
        .iter()
        .copied()
        .map(i32::try_from)
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(fds) => fds,
        Err(_) => {
            cancel_received_handle_reservations(reservation_id, &slots);
            super::service_ops::release_service_handle_refs_bounded(&service_refs);
            return Err(LINUX_EOVERFLOW);
        }
    };

    if !fds.is_empty() {
        let bytes = unsafe {
            core::slice::from_raw_parts(fds.as_ptr().cast::<u8>(), fds.len() * size_of::<i32>())
        };
        if let Err(err) = usermem::write_current_user_bytes(fds_ptr, bytes) {
            cancel_received_handle_reservations(reservation_id, &slots);
            super::service_ops::release_service_handle_refs_bounded(&service_refs);
            return Err(address_space_error_to_linux_errno(err));
        }
    }
    let count = match u16::try_from(fds.len()) {
        Ok(count) => count,
        Err(_) => {
            cancel_received_handle_reservations(reservation_id, &slots);
            super::service_ops::release_service_handle_refs_bounded(&service_refs);
            return Err(LINUX_EOVERFLOW);
        }
    };
    if let Err(err) = usermem::write_current_user_bytes(fd_count_ptr, &count.to_ne_bytes()) {
        cancel_received_handle_reservations(reservation_id, &slots);
        super::service_ops::release_service_handle_refs_bounded(&service_refs);
        return Err(address_space_error_to_linux_errno(err));
    }
    let committed = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        process_state
            .handles_mut()
            .commit_reserved_transfers(reservation_id, &slots, entries)
    });
    match committed {
        Some(Ok(())) => Ok(()),
        Some(Err(entries)) => {
            cancel_received_handle_reservations(reservation_id, &slots);
            drop(entries);
            super::service_ops::release_service_handle_refs_bounded(&service_refs);
            Err(LINUX_EBUSY)
        }
        None => {
            // The closure capture drops the uncommitted entries, including
            // their console references, when the process disappears.
            super::service_ops::release_service_handle_refs_bounded(&service_refs);
            Err(LINUX_EINVAL)
        }
    }
}

fn cancel_received_handle_reservations(reservation_id: u64, slots: &[u64]) {
    let _ = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        process_state
            .handles_mut()
            .cancel_reservations(reservation_id, slots);
    });
}

pub(super) fn install_transfer_descriptors_for_current_process(
    descriptors: &[KernelTransferredHandle],
) -> Result<Vec<i32>, i64> {
    let entries = take_transfer_entries(descriptors)?;
    install_transfer_entries_for_current_process(entries)
}

pub(super) fn transfer_tickets_for_descriptors(
    descriptors: &[KernelTransferredHandle],
    context: TransferContext,
) -> Result<Vec<KernelTransferTicket>, i64> {
    multitask::bind_ipc_transfer_tickets(descriptors, context)
        .map_err(ipc_transfer_error_to_linux_errno)
}

pub(super) fn commit_transfer_tickets_enqueue(tickets: &[KernelTransferTicket]) -> Result<(), i64> {
    multitask::commit_ipc_transfer_enqueue(tickets).map_err(ipc_transfer_error_to_linux_errno)
}

pub(super) struct PreparedTransferInstall {
    reservation_id: u64,
    slots: Vec<u64>,
    entries: Option<Vec<multitask::TransferredHandleEntry>>,
    service_refs: Vec<super::service_ops::ServiceHandleRef>,
    committed: bool,
}

impl PreparedTransferInstall {
    pub(super) fn fds(&self) -> Result<Vec<i32>, i64> {
        self.slots
            .iter()
            .copied()
            .map(|fd| i32::try_from(fd).map_err(|_| LINUX_EOVERFLOW))
            .collect()
    }

    pub(super) fn commit(mut self) -> Result<(), i64> {
        let entries = self.entries.take().ok_or(LINUX_ESTALE)?;
        let committed = multitask::with_current_user_process_state_mut(|_, _, process_state| {
            process_state.handles_mut().commit_reserved_transfers(
                self.reservation_id,
                self.slots.as_slice(),
                entries,
            )
        });
        match committed {
            Some(Ok(())) => {
                self.committed = true;
                self.service_refs.clear();
                Ok(())
            }
            Some(Err(entries)) => {
                self.entries = Some(entries);
                Err(LINUX_ESTALE)
            }
            None => Err(LINUX_EINVAL),
        }
    }
}

impl Drop for PreparedTransferInstall {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        cancel_received_handle_reservations(self.reservation_id, self.slots.as_slice());
        super::service_ops::release_service_handle_refs_bounded(self.service_refs.as_slice());
    }
}

pub(super) fn prepare_transfer_tickets_for_current_process(
    tickets: &[KernelTransferTicket],
    receiver: ProcessIdentity,
    service: ServiceIdentity,
    channel: ChannelIdentity,
    stream_pos: u64,
    receiver_open_description: u64,
) -> Result<PreparedTransferInstall, i64> {
    multitask::bind_ipc_transfer_receiver_by_tickets(
        tickets,
        receiver,
        service,
        channel,
        stream_pos,
        receiver_open_description,
    )
    .map_err(ipc_transfer_error_to_linux_errno)?;
    let entries = multitask::claim_ipc_transfer_entries_by_tickets(
        tickets,
        receiver,
        service,
        channel,
        stream_pos,
        receiver_open_description,
    )
    .map_err(ipc_transfer_error_to_linux_errno)?;
    let service_refs = service_transfer_refs(&entries);
    let reservation = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        process_state.handles_mut().reserve_slots(entries.len())
    })
    .flatten();
    let Some((reservation_id, slots)) = reservation else {
        super::service_ops::release_service_handle_refs_bounded(&service_refs);
        return Err(LINUX_EMFILE);
    };
    Ok(PreparedTransferInstall {
        reservation_id,
        slots,
        entries: Some(entries),
        service_refs,
        committed: false,
    })
}

fn install_transfer_entries_for_current_process(
    entries: Vec<multitask::TransferredHandleEntry>,
) -> Result<Vec<i32>, i64> {
    let service_refs = service_transfer_refs(&entries);
    let Some(fds) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        if !process_state
            .handles()
            .can_install_additional(entries.len())
        {
            return Err(LINUX_EMFILE);
        }
        let mut fds: Vec<i32> = Vec::with_capacity(entries.len());
        for entry in entries {
            let Some(fd) = process_state.handles_mut().install_transferred(entry) else {
                for installed in fds.iter().copied() {
                    let _ = process_state.handles_mut().close(installed as u64);
                }
                return Err(LINUX_EMFILE);
            };
            let Ok(fd) = i32::try_from(fd) else {
                let _ = process_state.handles_mut().close(fd);
                for installed in fds.iter().copied() {
                    let _ = process_state.handles_mut().close(installed as u64);
                }
                return Err(LINUX_EOVERFLOW);
            };
            fds.push(fd);
        }
        Ok(fds)
    }) else {
        super::service_ops::release_service_handle_refs_bounded(&service_refs);
        return Err(LINUX_EINVAL);
    };
    match fds {
        Ok(fds) => Ok(fds),
        Err(errno) => {
            super::service_ops::release_service_handle_refs_bounded(&service_refs);
            Err(errno)
        }
    }
}

pub(super) fn drop_transfer_tickets(tickets: &[KernelTransferTicket]) {
    if tickets.is_empty() {
        return;
    }
    multitask::drop_ipc_transfer_tickets(tickets);
    let _ = service_deferred_transfer_releases();
}

fn take_transfer_entries(
    descriptors: &[KernelTransferredHandle],
) -> Result<Vec<multitask::TransferredHandleEntry>, i64> {
    multitask::take_ipc_transfer_entries(descriptors).map_err(ipc_transfer_error_to_linux_errno)
}

pub(super) fn drop_transfer_descriptors(descriptors: &[KernelTransferredHandle]) {
    if descriptors.is_empty() {
        return;
    }
    multitask::drop_ipc_transfer_descriptors(descriptors);
    let _ = service_deferred_transfer_releases();
}

pub(crate) fn service_deferred_transfer_releases() -> usize {
    // Every entry can trigger a bounded provider reconciliation. Keep the
    // housekeeping quantum to one entry so a transfer-drop burst cannot turn
    // dozens of individually bounded calls into a multi-frame scheduler stall.
    const MAX_RELEASES_PER_TURN: usize = 1;
    let entries = multitask::take_deferred_ipc_transfer_drops(MAX_RELEASES_PER_TURN);
    let count = entries.len();
    release_transfer_entries(&entries);
    count
}

pub(super) fn drop_transfer_entries(entries: Vec<multitask::TransferredHandleEntry>) {
    release_transfer_entries(&entries);
}

fn service_transfer_refs(
    entries: &[multitask::TransferredHandleEntry],
) -> Vec<super::service_ops::ServiceHandleRef> {
    entries
        .iter()
        .filter_map(|entry| {
            super::service_ops::service_handle_ref_for_handle(entry.entry().handle())
        })
        .collect()
}

fn release_transfer_entries(entries: &[multitask::TransferredHandleEntry]) {
    let refs = service_transfer_refs(entries);
    super::service_ops::release_service_handle_refs_bounded(&refs);
}

pub(super) fn release_input_transfer_token(token: u64) {
    super::service_ops::release_service_handle_refs_bounded(&[
        super::service_ops::ServiceHandleRef::Input(token),
    ]);
}

fn ipc_transfer_error_to_linux_errno(err: multitask::IpcTransferRegistryError) -> i64 {
    match err {
        multitask::IpcTransferRegistryError::Exhausted => LINUX_ENOMEM,
        multitask::IpcTransferRegistryError::BindingMismatch => LINUX_EPERM,
        multitask::IpcTransferRegistryError::InvalidDescriptor => LINUX_EINVAL,
        multitask::IpcTransferRegistryError::InvalidState => LINUX_ESTALE,
        multitask::IpcTransferRegistryError::StaleDescriptor => LINUX_ESTALE,
    }
}

fn copy_request_from_user(user_ptr: u64, user_len: u64) -> Result<Vec<u8>, i64> {
    let len = usize::try_from(user_len).map_err(|_| LINUX_EINVAL)?;
    if len == 0 || len > rustos_user_abi::syscall::IPC_MAX_INLINE_BYTES {
        return Err(LINUX_EINVAL);
    }
    let mut bytes = alloc::vec![0_u8; len];
    usermem::copy_from_current_user_exact(user_ptr, &mut bytes)
        .map_err(address_space_error_to_linux_errno)?;
    Ok(bytes)
}

fn ipc_error_to_linux_errno(err: kernel_ipc_runtime::api::IpcError) -> i64 {
    match err {
        kernel_ipc_runtime::api::IpcError::InvalidHandle
        | kernel_ipc_runtime::api::IpcError::InvalidArgument => LINUX_EINVAL,
        kernel_ipc_runtime::api::IpcError::PermissionDenied => LINUX_EPERM,
        kernel_ipc_runtime::api::IpcError::PeerClosed => LINUX_EPIPE,
        kernel_ipc_runtime::api::IpcError::BufferTooSmall => LINUX_EOVERFLOW,
        kernel_ipc_runtime::api::IpcError::NoMemory => LINUX_ENOMEM,
    }
}

fn ticks_elapsed_ms(start_ticks: u64, end_ticks: u64) -> u64 {
    let ticks_per_second = crate::arch::rtc::ticks_per_second().max(1);
    end_ticks
        .saturating_sub(start_ticks)
        .saturating_mul(1000)
        .saturating_div(ticks_per_second)
}

fn maybe_log_slow_ipc<F>(elapsed_ms: u64, log: F)
where
    F: FnOnce(),
{
    let sample_index = IPC_LOG_SAMPLE_COUNT.fetch_add(1, Ordering::Relaxed);
    if sample_index >= EARLY_IPC_SAMPLE_COUNT && elapsed_ms < SLOW_IPC_THRESHOLD_MS {
        return;
    }
    let ticks_per_second = crate::arch::rtc::ticks_per_second().max(1);
    let window = crate::arch::rtc::ticks() / ticks_per_second;
    if !diagnostic_rate_limit_permit(
        &SLOW_IPC_LOG_RATE_STATE,
        window,
        MAX_SLOW_IPC_LOGS_PER_SECOND as u8,
    ) {
        return;
    }
    log();
}

#[allow(
    clippy::too_many_arguments,
    reason = "diagnostic-only phase timestamps remain explicit so call-site ordering is reviewable"
)]
fn log_slow_ipc_call(
    kind: &str,
    endpoint: u64,
    start_ticks: u64,
    copy_ticks: u64,
    export_ticks: u64,
    send_ticks: u64,
    wait_ticks: u64,
    write_ticks: u64,
    request_len: usize,
    response_len: usize,
) {
    let total_ms = ticks_elapsed_ms(start_ticks, write_ticks);
    maybe_log_slow_ipc(total_ms, || {
        let copy_ms = ticks_elapsed_ms(start_ticks, copy_ticks);
        let export_ms = ticks_elapsed_ms(copy_ticks, export_ticks);
        let send_ms = ticks_elapsed_ms(export_ticks, send_ticks);
        let wait_ms = ticks_elapsed_ms(send_ticks, wait_ticks);
        let write_ms = ticks_elapsed_ms(wait_ticks, write_ticks);
        debug::println_emergency(format_args!(
            "ipc slow {}: endpoint={} total_ms={} copy_ms={} export_ms={} send_ms={} wait_ms={} write_ms={} request_len={} response_len={}",
            kind,
            endpoint,
            total_ms,
            copy_ms,
            export_ms,
            send_ms,
            wait_ms,
            write_ms,
            request_len,
            response_len,
        ));
    });
}

fn log_slow_ipc_reply(
    kind: &str,
    reply: u64,
    start_ticks: u64,
    copy_ticks: u64,
    reply_ticks: u64,
    response_len: usize,
) {
    let total_ms = ticks_elapsed_ms(start_ticks, reply_ticks);
    maybe_log_slow_ipc(total_ms, || {
        let copy_ms = ticks_elapsed_ms(start_ticks, copy_ticks);
        let reply_ms = ticks_elapsed_ms(copy_ticks, reply_ticks);
        debug::println_emergency(format_args!(
            "ipc slow {}: reply={} total_ms={} copy_ms={} reply_ms={} response_len={}",
            kind, reply, total_ms, copy_ms, reply_ms, response_len,
        ));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_ipc_calls_share_the_finite_service_deadline() {
        const {
            assert!(SERVICE_IPC_TIMEOUT_MS > 0);
            assert!(SERVICE_IPC_TIMEOUT_MS <= IPC_WAIT_SERVICE_ENDPOINT_MAX_TIMEOUT_MS);
        }
        assert_eq!(
            ServiceIpcClass::ReadinessQuery.timeout_ms(),
            rustos_user_abi::performance::IPC_READINESS_QUERY_HARD_LIMIT_MS
        );
        assert_eq!(
            ServiceIpcClass::InteractiveControl.timeout_ms(),
            rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS
        );
        assert_eq!(
            ServiceIpcClass::BootControl.timeout_ms(),
            rustos_user_abi::performance::IPC_BOOT_CONTROL_HARD_LIMIT_MS
        );
        assert_eq!(
            ServiceIpcClass::BulkData.timeout_ms(),
            SERVICE_IPC_TIMEOUT_MS
        );
        assert_eq!(
            ServiceIpcClass::ReadinessQuery.cap_timeout_ms(u64::MAX),
            rustos_user_abi::performance::IPC_READINESS_QUERY_HARD_LIMIT_MS
        );
        assert!(!bounded_ipc_call_timeout_is_valid(0));
        assert!(bounded_ipc_call_timeout_is_valid(
            rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS
        ));
        assert!(!bounded_ipc_call_timeout_is_valid(
            SERVICE_IPC_TIMEOUT_MS + 1
        ));
        assert_eq!(
            rustos_user_abi::syscall::SYS_RUSTOS_IPC_CALL_WITH_HANDLES_BOUNDED,
            0x5255_0045
        );
        assert_eq!(ServiceIpcClass::BootControl.cap_timeout_ms(37), 37);
        assert_eq!(ServiceIpcClass::InteractiveControl.cap_timeout_ms(0), 1);
    }

    #[test]
    fn retired_task_cleanup_removes_service_endpoint_waiter_exactly_once() {
        let task_id = u64::MAX - 401;
        assert!(register_service_endpoint_waiter(ServiceEndpointWaiter {
            task_id,
            service_id: linux_abi::IPC_SERVICE_VFSD,
            expected_pid: u64::MAX - 402,
        }));
        assert_eq!(remove_service_endpoint_waiter(task_id), 1);
        assert_eq!(remove_service_endpoint_waiter(task_id), 0);
    }

    #[test]
    fn service_endpoint_waiter_rearm_replaces_without_allocating_another_slot() {
        let mut table = ServiceEndpointWaiterTable::new();
        assert!(table.register(ServiceEndpointWaiter {
            task_id: 41,
            service_id: linux_abi::IPC_SERVICE_VFSD,
            expected_pid: 51,
        }));
        assert!(table.register(ServiceEndpointWaiter {
            task_id: 41,
            service_id: linux_abi::IPC_SERVICE_NETD,
            expected_pid: 61,
        }));
        let (_, old_count) = table.take_matching(|waiter| waiter.expected_pid == 51);
        assert_eq!(old_count, 0);
        let (tasks, new_count) = table.take_matching(|waiter| waiter.expected_pid == 61);
        assert_eq!(new_count, 1);
        assert_eq!(tasks[0], 41);
    }

    #[test]
    #[allow(
        clippy::assertions_on_constants,
        reason = "the mutation witness must compile a reduced capacity and fail at runtime"
    )]
    fn service_endpoint_waiter_capacity_covers_every_scheduler_task() {
        assert!(MAX_SERVICE_ENDPOINT_WAITERS >= multitask::MAX_SCHEDULER_TASKS);
    }

    #[test]
    fn service_endpoint_epoch_changes_on_every_publication_boundary() {
        assert_eq!(next_service_endpoint_epoch(0), Some(1));
        assert_eq!(next_service_endpoint_epoch(1), Some(2));
        assert_eq!(next_service_endpoint_epoch(u64::MAX), None);
    }

    #[test]
    fn stable_service_endpoint_snapshot_rejects_revoked_owners() {
        assert_eq!(SERVICE_ENDPOINT_STABLE_READ_ATTEMPTS, 3);
        assert_eq!(stable_service_endpoint_snapshot(47, 41, false), 47);
        assert_eq!(stable_service_endpoint_snapshot(0, 41, false), 0);
        assert_eq!(stable_service_endpoint_snapshot(47, 0, false), 0);
        assert_eq!(stable_service_endpoint_snapshot(47, 41, true), 0);
    }

    #[test]
    fn cached_service_call_grant_is_exact_process_and_epoch() {
        assert!(cached_service_call_grant_matches(41, 7, 41, 7));
        assert!(!cached_service_call_grant_matches(41, 7, 42, 7));
        assert!(!cached_service_call_grant_matches(41, 7, 41, 8));
        assert!(!cached_service_call_grant_matches(0, 7, 0, 7));
        assert!(!cached_service_call_grant_matches(41, 0, 41, 0));
    }

    #[test]
    fn inputd_owner_exit_withdraws_the_separate_ring_policy_lease() {
        assert!(service_exit_requires_input_policy_withdrawal(
            linux_abi::IPC_SERVICE_INPUTD
        ));
        assert!(!service_exit_requires_input_policy_withdrawal(
            linux_abi::IPC_SERVICE_NETD
        ));
    }

    #[test]
    fn root_service_publication_is_boot_owner_sealed_and_epoch_bound() {
        assert!(rootd_bootstrap_owner_allows(0, 41));
        assert!(rootd_bootstrap_owner_allows(41, 41));
        assert!(!rootd_bootstrap_owner_allows(41, 42));
        assert!(!rootd_bootstrap_owner_allows(0, 0));

        assert!(rootd_authorization_epoch_matches(7, 101, 41, 7, false));
        assert!(!rootd_authorization_epoch_matches(7, 101, 41, 8, false));
        assert!(!rootd_authorization_epoch_matches(7, 0, 41, 7, false));
        assert!(!rootd_authorization_epoch_matches(7, 101, 41, 7, true));
    }

    #[test]
    fn service_call_grants_are_exact_epoch_bounded_and_revocable() {
        let mut grants = [ServiceCallGrant::empty(); 2];
        assert_eq!(record_service_call_grant(&mut grants, 41, 3, 7), Ok(()));
        assert!(has_service_call_grant(&grants, 41, 3, 7));
        assert!(!has_service_call_grant(&grants, 42, 3, 7));
        assert!(!has_service_call_grant(&grants, 41, 3, 8));

        assert_eq!(record_service_call_grant(&mut grants, 41, 3, 8), Ok(()));
        assert!(!has_service_call_grant(&grants, 41, 3, 7));
        assert!(has_service_call_grant(&grants, 41, 3, 8));

        assert_eq!(record_service_call_grant(&mut grants, 42, 4, 9), Ok(()));
        assert_eq!(
            record_service_call_grant(&mut grants, 43, 5, 10),
            Err(LINUX_ENOSPC)
        );
        clear_service_call_grants(&mut grants, 41);
        assert!(!has_service_call_grant(&grants, 41, 3, 8));
        assert!(has_service_call_grant(&grants, 42, 4, 9));
    }

    fn matching_commercial_response()
    -> (CommercialMaxProtocolRequest, CommercialMaxProtocolResponse) {
        let mut request = CommercialMaxProtocolRequest::default();
        request.header.protocol =
            rustos_user_abi::syscall::COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR;
        request.header.op = rustos_user_abi::syscall::COMMERCIAL_MAX_ROOTD_OP_SERVICE_LOOKUP;
        request.header.service_id = linux_abi::IPC_SERVICE_ROOTD;
        request.header.subject_pid = 41;
        request.header.subject_tid = 43;
        request.header.ticket = 47;
        let response = CommercialMaxProtocolResponse {
            header: request.header,
            ..CommercialMaxProtocolResponse::default()
        };
        (request, response)
    }

    #[test]
    fn commercial_response_envelope_is_bound_to_request_and_bounded() {
        let (request, response) = matching_commercial_response();
        assert_eq!(
            validate_commercial_response_envelope(&request, &response),
            Ok(())
        );

        let mut wrong_subject = response;
        wrong_subject.header.subject_tid += 1;
        assert_eq!(
            validate_commercial_response_envelope(&request, &wrong_subject),
            Err(LINUX_EINVAL)
        );

        let mut reserved = response;
        reserved.reserved1 = 1;
        assert_eq!(
            validate_commercial_response_envelope(&request, &reserved),
            Err(LINUX_EINVAL)
        );

        let mut too_many_descriptors = response;
        too_many_descriptors.descriptor_count = (too_many_descriptors.descriptors.len() + 1) as u16;
        assert_eq!(
            validate_commercial_response_envelope(&request, &too_many_descriptors),
            Err(LINUX_EINVAL)
        );

        let mut oversized_capability_label = response;
        oversized_capability_label.capability.label_len =
            (oversized_capability_label.capability.label.len() + 1) as u16;
        assert_eq!(
            validate_commercial_response_envelope(&request, &oversized_capability_label),
            Err(LINUX_EINVAL)
        );

        let mut malformed_descriptor = response;
        malformed_descriptor.descriptor_count = 1;
        malformed_descriptor.descriptors[0].reserved0 = 1;
        assert_eq!(
            validate_commercial_response_envelope(&request, &malformed_descriptor),
            Err(LINUX_EINVAL)
        );
    }
}
