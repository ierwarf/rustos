use super::*;
use alloc::vec::Vec;
use core::mem::size_of;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use kernel_ipc_runtime::api::{KernelEndpointHandle, KernelReplyHandle, KernelTransferredHandle};
use lazy_static::lazy_static;
use spin::Mutex;

macro_rules! ipc_trace {
    ($($arg:tt)*) => {
        if IPC_TRACE_VERBOSE {
            debug::println_emergency(format_args!($($arg)*));
        }
    };
}

const IPC_TRACE_VERBOSE: bool = false;
const MAX_SERVICE_ENDPOINTS: usize = 16;
const MAX_SERVICE_ENDPOINT_WAITERS: usize = 32;
const SLOW_IPC_THRESHOLD_MS: u64 = 10;
const MAX_SLOW_IPC_LOGS: usize = 20;
const EARLY_IPC_SAMPLE_COUNT: usize = 6;
const SERVICE_IPC_TIMEOUT_MS: u64 = 30_000;
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
// Publication, revocation, and process-exit cleanup must share one mutation
// critical section. The endpoint itself remains the lock-free commit point for
// readers, but a second registrar or an exiting process must not interleave
// between capability preparation and endpoint publication.
static SERVICE_ENDPOINT_REGISTRY_MUTATION: Mutex<()> = Mutex::new(());
// RING3-MIGRATION-REFERENCE END: rootd-owned service endpoint registry state.
static SLOW_IPC_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy)]
struct ServiceEndpointWaiter {
    task_id: u64,
    service_id: u64,
    expected_pid: u64,
}

lazy_static! {
    static ref SERVICE_ENDPOINT_WAITERS: Mutex<Vec<ServiceEndpointWaiter>> = Mutex::new(Vec::new());
}

pub(super) fn is_linux_rustos_ipc_syscall(syscall_number: u64) -> bool {
    matches!(
        syscall_number,
        linux_abi::SYS_RUSTOS_IPC_ENDPOINT_CREATE
            | linux_abi::SYS_RUSTOS_IPC_CALL
            | linux_abi::SYS_RUSTOS_IPC_RECV
            | linux_abi::SYS_RUSTOS_IPC_TRY_RECV
            | linux_abi::SYS_RUSTOS_IPC_TRY_RECV_WITH_SENDER
            | linux_abi::SYS_RUSTOS_IPC_RECV_WITH_SENDER
            | linux_abi::SYS_RUSTOS_IPC_REPLY
            | linux_abi::SYS_RUSTOS_IPC_CALL_WITH_HANDLES
            | linux_abi::SYS_RUSTOS_IPC_RECV_WITH_HANDLES
            | linux_abi::SYS_RUSTOS_IPC_REPLY_WITH_HANDLES
            | linux_abi::SYS_RUSTOS_IPC_REGISTER_LINUX_SYSCALL_ENDPOINT
            | linux_abi::SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT
            | linux_abi::SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT
            | linux_abi::SYS_RUSTOS_IPC_WAIT_SERVICE_ENDPOINT
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
        linux_abi::SYS_RUSTOS_IPC_REPLY => {
            syscall_linux_rustos_ipc_reply(frame.rdi, frame.rsi, frame.rdx)
        }
        linux_abi::SYS_RUSTOS_IPC_CALL_WITH_HANDLES => {
            syscall_linux_rustos_ipc_call_with_handles(frame.rdi)
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
/// stale endpoint가 남아 있으면 이후 호출자가 wait_for_reply에서 무한 대기하게 되므로
/// 반드시 프로세스 종료 경로에서 호출해야 한다.
pub(crate) fn cleanup_service_endpoints_for_process(process_id: u64) {
    let mut revoked_services = [0_u64; MAX_SERVICE_ENDPOINTS];
    let mut revoked_count = 0usize;
    let registry_mutation = SERVICE_ENDPOINT_REGISTRY_MUTATION.lock();
    if SERVICE_ENDPOINT_OWNERS[linux_abi::IPC_SERVICE_LINUX_SYSCALLD as usize]
        .load(Ordering::Acquire)
        == process_id
    {
        LINUX_SYSCALL_ENDPOINT.store(0, Ordering::Release);
    }
    for i in 0..MAX_SERVICE_ENDPOINTS {
        if SERVICE_ENDPOINT_OWNERS[i].load(Ordering::Acquire) == process_id {
            advance_service_endpoint_epoch(i).expect("service endpoint epoch exhausted");
            SERVICE_ENDPOINTS[i].store(0, Ordering::Release);
            SERVICE_ENDPOINT_OWNERS[i].store(0, Ordering::Release);
            SERVICE_ENDPOINT_CAPS[i].store(0, Ordering::Release);
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
    }
    super::broker_ops::waitset_broker_ops::remove_waitset_waiters_for_process(process_id);
    wake_exited_service_endpoint_waiters(process_id);
}

pub(super) fn current_process_has_service_capability(capability: u64) -> bool {
    if capability == 0 {
        return false;
    }
    let Some(process_id) = multitask::current_user_process_id() else {
        return false;
    };
    let _registry = SERVICE_ENDPOINT_REGISTRY_MUTATION.lock();
    current_process_has_service_capability_locked(capability, process_id)
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
    let _registry = SERVICE_ENDPOINT_REGISTRY_MUTATION.lock();
    service_endpoint_raw_locked(index)
}

fn service_endpoint_raw_locked(index: usize) -> Option<u64> {
    let endpoint = SERVICE_ENDPOINTS[index].load(Ordering::Acquire);
    if endpoint == 0 {
        return Some(0);
    }
    let owner = SERVICE_ENDPOINT_OWNERS[index].load(Ordering::Acquire);
    if owner == 0 || multitask::is_user_process_exiting(owner) {
        return Some(0);
    }
    Some(endpoint)
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

/// Called only while `SERVICE_ENDPOINT_REGISTRY_MUTATION` is held.
fn advance_service_endpoint_epoch(index: usize) -> Result<u64, i64> {
    let current = SERVICE_ENDPOINT_EPOCHS[index].load(Ordering::Relaxed);
    let next = next_service_endpoint_epoch(current).ok_or(LINUX_EOVERFLOW)?;
    SERVICE_ENDPOINT_EPOCHS[index].store(next, Ordering::Release);
    Ok(next)
}

/// Cross-class handoff is scheduler authority. Accept it only from the live
/// netd endpoint and only on an exact compact-v2 local-socket wait/data reply.
fn netd_response_requests_latency_handoff(response: &[u8], is_live_netd_owner: bool) -> bool {
    if !is_live_netd_owner || response.len() < NETD_IPC_RESPONSE_HEADER_SIZE {
        return false;
    }
    let version = u16::from_ne_bytes([response[0], response[1]]);
    let op = u16::from_ne_bytes([response[2], response[3]]);
    let status = i32::from_ne_bytes([response[4], response[5], response[6], response[7]]);
    let reserved0 = u32::from_ne_bytes([response[8], response[9], response[10], response[11]]);
    let value = u64::from_ne_bytes([
        response[16],
        response[17],
        response[18],
        response[19],
        response[20],
        response[21],
        response[22],
        response[23],
    ]);
    let payload_len =
        u32::from_ne_bytes([response[24], response[25], response[26], response[27]]) as usize;
    let flags = u32::from_ne_bytes([response[28], response[29], response[30], response[31]]);
    let authorized_op = matches!(
        op,
        SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET
            | SYSCALL_OFFLOAD_OP_LINUX_SENDTO
            | SYSCALL_OFFLOAD_OP_LINUX_SENDMSG
            | SYSCALL_OFFLOAD_OP_LINUX_RECVFROM
            | SYSCALL_OFFLOAD_OP_LINUX_RECVMSG
    );
    let successful_event_or_transfer = status == 0 && value != 0;
    let completed_nonblocking_recv_drain = status == LINUX_EAGAIN as i32
        && value == 0
        && payload_len == 0
        && matches!(
            op,
            SYSCALL_OFFLOAD_OP_LINUX_RECVFROM | SYSCALL_OFFLOAD_OP_LINUX_RECVMSG
        );
    version == NETD_IPC_ABI_VERSION
        && authorized_op
        && (successful_event_or_transfer || completed_nonblocking_recv_drain)
        && reserved0 == 0
        && flags == NETD_IPC_RESPONSE_FLAG_LATENCY_HANDOFF
        && NETD_IPC_RESPONSE_HEADER_SIZE.checked_add(payload_len) == Some(response.len())
}

fn register_service_endpoint_waiter(waiter: ServiceEndpointWaiter) -> bool {
    let mut waiters = SERVICE_ENDPOINT_WAITERS.lock();
    waiters.retain(|current| current.task_id != waiter.task_id);
    if waiters.len() >= MAX_SERVICE_ENDPOINT_WAITERS {
        return false;
    }
    waiters.push(waiter);
    true
}

fn remove_service_endpoint_waiter(task_id: u64) {
    SERVICE_ENDPOINT_WAITERS
        .lock()
        .retain(|waiter| waiter.task_id != task_id);
}

fn wake_registered_service_endpoint_waiters(service_id: u64, owner_pid: u64) {
    let tasks = {
        let mut waiters = SERVICE_ENDPOINT_WAITERS.lock();
        let mut tasks = Vec::new();
        waiters.retain(|waiter| {
            let matched = waiter.service_id == service_id && waiter.expected_pid == owner_pid;
            if matched {
                tasks.push(waiter.task_id);
            }
            !matched
        });
        tasks
    };
    for task_id in tasks {
        if multitask::wake_task(task_id) {
            multitask::set_next_pick_hint(task_id);
        }
    }
}

fn wake_exited_service_endpoint_waiters(process_id: u64) {
    let tasks = {
        let mut waiters = SERVICE_ENDPOINT_WAITERS.lock();
        let mut tasks = Vec::new();
        waiters.retain(|waiter| {
            if waiter.expected_pid == process_id {
                tasks.push(waiter.task_id);
                false
            } else {
                true
            }
        });
        tasks
    };
    let mut woke = false;
    for task_id in tasks {
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
    let capability = match service_capability(linux_abi::IPC_SERVICE_LINUX_SYSCALLD) {
        Ok(capability) => capability,
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
    SERVICE_ENDPOINT_CAPS[linux_abi::IPC_SERVICE_LINUX_SYSCALLD as usize]
        .store(capability, Ordering::Release);
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
        capability
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
    let capability = match service_capability(service_id) {
        Ok(capability) => capability,
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
    SERVICE_ENDPOINT_CAPS[index].store(capability, Ordering::Release);
    SERVICE_ENDPOINT_OWNERS[index].store(process_id, Ordering::Release);
    if let Err(errno) = advance_service_endpoint_epoch(index) {
        SERVICE_ENDPOINT_CAPS[index].store(0, Ordering::Release);
        SERVICE_ENDPOINT_OWNERS[index].store(0, Ordering::Release);
        drop(registry_mutation);
        return linux_errno(errno);
    }
    // Publish the endpoint last. Acquire readers that observe it also observe
    // the rootd-authorized owner and capability written above.
    SERVICE_ENDPOINTS[index].store(endpoint, Ordering::Release);
    drop(registry_mutation);
    ipc_trace!(
        "ipc service registered: service={} endpoint={} owner={} caps={:#x}",
        service_id,
        endpoint,
        process_id,
        capability
    );
    record_service_endpoint_milestone("ipc-service-register", service_id, process_id, endpoint);
    wake_registered_service_endpoint_waiters(service_id, process_id);
    0
}

fn service_capability(service_id: u64) -> Result<u64, i64> {
    match service_capability_via_rootd(service_id) {
        Ok(capability) => Ok(capability),
        Err(_) if service_id == linux_abi::IPC_SERVICE_ROOTD => {
            Ok(rustos_user_abi::syscall::IPC_SERVICE_CAP_ROOT_SUPERVISOR)
        }
        Err(errno) => Err(errno),
    }
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
    let response = call_service_endpoint(linux_abi::IPC_SERVICE_ROOTD, as_bytes(&request))?;
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
    if service_id != linux_abi::IPC_SERVICE_ROOTD && !current_process_can_lookup_service_endpoint()
    {
        if let Err(errno) = authorize_service_lookup_via_rootd(service_id) {
            return linux_errno(errno);
        }
    }
    let Some(raw) = service_endpoint_raw(service_id) else {
        return linux_errno(LINUX_EINVAL);
    };
    if raw == 0 {
        return linux_errno(LINUX_ENOSYS);
    }
    raw
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
        match multitask::commit_block_current_task() {
            Some(true) => multitask::yield_now(),
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
    let response = call_service_endpoint(linux_abi::IPC_SERVICE_ROOTD, as_bytes(&request))?;
    if response.len() != size_of::<CommercialMaxProtocolResponse>() {
        return Err(LINUX_EINVAL);
    }
    let response = read_unaligned::<CommercialMaxProtocolResponse>(response.as_slice());
    validate_commercial_response_envelope(&request, &response)?;
    if response.descriptor_count != 0
        || response.payload_len != 0
        || response.value0 != service_id
        || response.value1 != 0
    {
        return Err(LINUX_EINVAL);
    }
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
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
    let response = match wait_for_reply(reply) {
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
    if !response.is_empty() {
        if let Err(err) = usermem::write_current_user_bytes(reply_ptr, response.as_slice()) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
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
                if !request.is_empty() {
                    if let Err(err) = usermem::write_current_user_bytes(request_ptr, &request) {
                        return linux_errno(address_space_error_to_linux_errno(err));
                    }
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
                match multitask::commit_block_current_task() {
                    Some(true) => multitask::yield_now(),
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
    if !current_process_has_service_capability(
        rustos_user_abi::syscall::IPC_SERVICE_CAP_ROOT_SUPERVISOR,
    ) {
        return linux_errno(LINUX_EPERM);
    }
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
    if !current_process_has_service_capability(
        rustos_user_abi::syscall::IPC_SERVICE_CAP_ROOT_SUPERVISOR,
    ) {
        return linux_errno(LINUX_EPERM);
    }
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
    let Ok(request_capacity) = usize::try_from(request_capacity) else {
        return linux_errno(LINUX_EINVAL);
    };
    if request_capacity > rustos_user_abi::syscall::IPC_MAX_INLINE_BYTES {
        return linux_errno(LINUX_EINVAL);
    }
    if request_capacity > 0 {
        if let Err(err) = usermem::validate_current_user_write_buffer(request_ptr, request_capacity)
        {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
    }
    if let Err(err) = usermem::validate_current_user_write_buffer(reply_cap_ptr, size_of::<u64>()) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    if let Err(err) = usermem::validate_current_user_write_buffer(sender_pid_ptr, size_of::<u64>())
    {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    if let Err(err) = usermem::validate_current_user_write_buffer(sender_tid_ptr, size_of::<u64>())
    {
        return linux_errno(address_space_error_to_linux_errno(err));
    }

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
                    if let Err(err) = usermem::write_current_user_bytes(request_ptr, &request) {
                        return linux_errno(address_space_error_to_linux_errno(err));
                    }
                }
                if let Err(err) =
                    usermem::write_current_user_bytes(reply_cap_ptr, &reply.raw().to_ne_bytes())
                {
                    return linux_errno(address_space_error_to_linux_errno(err));
                }
                if let Err(err) =
                    usermem::write_current_user_bytes(sender_pid_ptr, &sender_pid.to_ne_bytes())
                {
                    return linux_errno(address_space_error_to_linux_errno(err));
                }
                if let Err(err) =
                    usermem::write_current_user_bytes(sender_tid_ptr, &sender_tid.to_ne_bytes())
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
                if pending {
                    let _ = multitask::cancel_block_current_task();
                    continue;
                }
                match multitask::commit_block_current_task() {
                    Some(true) => multitask::yield_now(),
                    Some(false) => continue,
                    None => return linux_errno(LINUX_EINVAL),
                }
            }
            Err(err) => return linux_errno(ipc_error_to_linux_errno(err)),
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
    let live_netd_owner = process_owns_live_service_endpoint(receiver_process_id, IPC_SERVICE_NETD);
    let latency_handoff =
        netd_response_requests_latency_handoff(response.as_slice(), live_netd_owner);
    let task_id = match kernel_ipc_runtime::api::complete_endpoint_reply_for_process(
        KernelReplyHandle::from_raw(reply),
        receiver_process_id,
        response.as_slice(),
    ) {
        Ok(task_id) => task_id,
        Err(err) => return linux_errno(ipc_error_to_linux_errno(err)),
    };
    let _ = multitask::release_ipc_priority(reply);
    let reply_ticks = crate::arch::rtc::ticks();
    let _ = multitask::wake_task(task_id);
    // Direct hand-back to the caller: the service is about to wait on its
    // endpoint again, so donate the remaining quantum to the original caller
    // instead of round-robining away from a freshly-completed reply.
    if latency_handoff && multitask::set_next_latency_pick_hint(task_id) {
        // The generic deferred-reschedule bit is intentionally cleared on
        // syscall exit. Arm the existing user-return PIT edge so this trusted
        // event wakeup is observed promptly without switching away from a
        // live kernel syscall continuation.
        multitask::request_user_return_reschedule();
    } else {
        multitask::set_next_pick_hint(task_id);
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

    let (response, reply_handles) = match wait_for_reply_with_handle_limit(
        reply,
        rustos_user_abi::syscall::IPC_MAX_TRANSFER_HANDLES,
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
    if reply_capacity > 0 {
        if let Err(err) =
            usermem::validate_current_user_write_buffer(args.reply_ptr, reply_capacity)
        {
            drop_transfer_descriptors(reply_handles.as_slice());
            return linux_errno(address_space_error_to_linux_errno(err));
        }
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
    if !response.is_empty() {
        if let Err(err) = usermem::write_current_user_bytes(args.reply_ptr, response.as_slice()) {
            drop_transfer_descriptors(reply_handles.as_slice());
            return linux_errno(address_space_error_to_linux_errno(err));
        }
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
    if request_capacity > 0 {
        if let Err(err) =
            usermem::validate_current_user_write_buffer(args.request_ptr, request_capacity)
        {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
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
                if !request.is_empty() {
                    if let Err(err) = usermem::write_current_user_bytes(args.request_ptr, &request)
                    {
                        drop_transfer_descriptors(handles.as_slice());
                        return linux_errno(address_space_error_to_linux_errno(err));
                    }
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
                match multitask::commit_block_current_task() {
                    Some(true) => multitask::yield_now(),
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
            return linux_errno(ipc_error_to_linux_errno(err));
        }
    };
    let _ = multitask::release_ipc_priority(args.reply_cap);
    let _ = multitask::wake_task(task_id);
    multitask::set_next_pick_hint(task_id);
    multitask::request_deferred_reschedule();
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

pub(super) fn call_service_endpoint(service_id: u64, request: &[u8]) -> Result<Vec<u8>, i64> {
    call_service_endpoint_with_timeout(service_id, request, SERVICE_IPC_TIMEOUT_MS)
}

pub(super) fn call_service_endpoint_with_timeout(
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
    let (reply, receiver_to_wake) = kernel_ipc_runtime::api::enqueue_endpoint_call_with_handles(
        endpoint,
        task_id,
        request,
        attached_handles,
    )
    .map_err(ipc_error_to_linux_errno)?;
    ipc_trace!(
        "ipc call queued: endpoint={} receiver_to_wake={:?}",
        endpoint.raw(),
        receiver_to_wake
    );
    let receiver_process_id = kernel_ipc_runtime::api::endpoint::receiver_process_for_reply(reply);
    if let Some(receiver_process_id) = receiver_process_id {
        let _ =
            multitask::inherit_ipc_priority_for_process(reply.raw(), task_id, receiver_process_id);
        if receiver_to_wake.is_none() {
            let _ = multitask::set_next_process_pick_hint(receiver_process_id);
        }
    }
    if let Some(receiver_task_id) = receiver_to_wake {
        // The reply capability is the lifetime authority for this donation.
        // Install it before the wake/handoff so a User-class server that is
        // directly needed by a System caller is eligible for the very next
        // pick, rather than being indefinitely deferred by unrelated System
        // pollers.
        let inherited = multitask::inherit_ipc_priority(reply.raw(), task_id, receiver_task_id);
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
        multitask::set_next_pick_hint(receiver_task_id);
    }
    Ok(reply)
}

fn wait_for_reply(reply: KernelReplyHandle) -> Result<Vec<u8>, i64> {
    Ok(wait_for_reply_with_handle_limit(reply, 0)?.0)
}

fn wait_for_service_reply(reply: KernelReplyHandle) -> Result<Vec<u8>, i64> {
    wait_for_service_reply_with_timeout(reply, SERVICE_IPC_TIMEOUT_MS)
}

fn wait_for_service_reply_with_timeout(
    reply: KernelReplyHandle,
    timeout_ms: u64,
) -> Result<Vec<u8>, i64> {
    Ok(
        wait_for_reply_with_deadline(reply, 0, Some(service_ipc_deadline_tick_after(timeout_ms)))?
            .0,
    )
}

fn wait_for_service_reply_with_handle_limit(
    reply: KernelReplyHandle,
    handle_capacity: usize,
) -> Result<(Vec<u8>, Vec<KernelTransferredHandle>), i64> {
    wait_for_reply_with_deadline(reply, handle_capacity, Some(service_ipc_deadline_tick()))
}

fn wait_for_reply_with_handle_limit(
    reply: KernelReplyHandle,
    handle_capacity: usize,
) -> Result<(Vec<u8>, Vec<KernelTransferredHandle>), i64> {
    wait_for_reply_with_deadline(reply, handle_capacity, None)
}

fn wait_for_reply_with_deadline(
    reply: KernelReplyHandle,
    handle_capacity: usize,
    deadline_tick: Option<u64>,
) -> Result<(Vec<u8>, Vec<KernelTransferredHandle>), i64> {
    let caller_task_id = if deadline_tick.is_some() {
        Some(multitask::current_task_id().ok_or(LINUX_EINVAL)?)
    } else {
        None
    };
    loop {
        match take_endpoint_response_for_wait(reply, handle_capacity) {
            Ok(Some(response)) => {
                disarm_reply_deadline_waiter(caller_task_id);
                return Ok(response);
            }
            Ok(None) => {}
            Err(errno) => {
                disarm_reply_deadline_waiter(caller_task_id);
                return Err(errno);
            }
        }
        if reply_deadline_expired(deadline_tick) {
            cancel_deadline_reply(reply, caller_task_id);
            return Err(LINUX_ETIMEDOUT);
        }
        if !multitask::arm_block_current_task() {
            cancel_deadline_reply(reply, caller_task_id);
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
                return Err(errno);
            }
        }
        if reply_deadline_expired(deadline_tick) {
            disarm_reply_deadline_waiter(caller_task_id);
            if let Some(task_id) = caller_task_id {
                let _ = multitask::wake_task(task_id);
            }
            let _ = multitask::commit_block_current_task();
            cancel_deadline_reply(reply, caller_task_id);
            return Err(LINUX_ETIMEDOUT);
        }
        match multitask::commit_block_current_task() {
            Some(true) => {
                multitask::yield_now();
                disarm_reply_deadline_waiter(caller_task_id);
            }
            Some(false) => {
                disarm_reply_deadline_waiter(caller_task_id);
                continue;
            }
            None => {
                disarm_reply_deadline_waiter(caller_task_id);
                cancel_deadline_reply(reply, caller_task_id);
                return Err(LINUX_EINVAL);
            }
        }
    }
}

fn take_endpoint_response_for_wait(
    reply: KernelReplyHandle,
    handle_capacity: usize,
) -> Result<Option<(Vec<u8>, Vec<KernelTransferredHandle>)>, i64> {
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

fn service_ipc_deadline_tick() -> u64 {
    service_ipc_deadline_tick_after(SERVICE_IPC_TIMEOUT_MS)
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

fn reply_deadline_expired(deadline_tick: Option<u64>) -> bool {
    deadline_tick.is_some_and(|deadline_tick| crate::arch::rtc::ticks() >= deadline_tick)
}

fn arm_reply_deadline_waiter(task_id: Option<u64>, deadline_tick: Option<u64>) -> bool {
    if let (Some(task_id), Some(deadline_tick)) = (task_id, deadline_tick) {
        crate::arch::rtc::arm_sleep_waiter_until_tick(task_id, deadline_tick)
    } else {
        true
    }
}

fn disarm_reply_deadline_waiter(task_id: Option<u64>) {
    if let Some(task_id) = task_id {
        crate::arch::rtc::disarm_sleep_waiter(task_id);
    }
}

fn cancel_deadline_reply(reply: KernelReplyHandle, caller_task_id: Option<u64>) {
    let Some(caller_task_id) = caller_task_id else {
        return;
    };
    let result =
        kernel_ipc_runtime::api::cancel_endpoint_call_with_transfers(reply, caller_task_id);
    if let Ok(discarded) = &result {
        let _ = multitask::release_ipc_priority(reply.raw());
        drop_transfer_descriptors(discarded.as_slice());
    }
    let status = u64::from(result.is_err());
    debug::record_milestone(
        debug::LogCategory::Compat,
        "ipc-reply-timeout",
        reply.raw(),
        ((caller_task_id & 0xffff_ffff) << 32) | status,
    );
    ipc_trace!(
        "ipc reply timeout: reply={} caller={} cancel={:?}",
        reply.raw(),
        caller_task_id,
        result
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
    for handle_ref in service_refs.iter().copied() {
        if let Err(errno) = super::service_ops::acquire_service_handle_ref(handle_ref) {
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
    let fds = install_transfer_descriptors_for_current_process(descriptors)?;

    if !fds.is_empty() {
        let bytes = unsafe {
            core::slice::from_raw_parts(fds.as_ptr().cast::<u8>(), fds.len() * size_of::<i32>())
        };
        usermem::write_current_user_bytes(fds_ptr, bytes)
            .map_err(address_space_error_to_linux_errno)?;
    }
    let count = u16::try_from(fds.len()).map_err(|_| LINUX_EOVERFLOW)?;
    usermem::write_current_user_bytes(fd_count_ptr, &count.to_ne_bytes())
        .map_err(address_space_error_to_linux_errno)?;
    Ok(())
}

pub(super) fn install_transfer_descriptors_for_current_process(
    descriptors: &[KernelTransferredHandle],
) -> Result<Vec<i32>, i64> {
    let entries = take_transfer_entries(descriptors)?;
    let service_refs = service_transfer_refs(&entries);
    let Some(fds) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        if !process_state
            .handles()
            .can_install_additional(entries.len())
        {
            return Err(LINUX_EMFILE);
        }
        let mut fds = Vec::with_capacity(entries.len());
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
    const MAX_RELEASES_PER_TURN: usize = 32;
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
        multitask::IpcTransferRegistryError::InvalidDescriptor => LINUX_EINVAL,
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
    let sample_index = SLOW_IPC_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if sample_index >= MAX_SLOW_IPC_LOGS {
        return;
    }
    if sample_index >= EARLY_IPC_SAMPLE_COUNT && elapsed_ms < SLOW_IPC_THRESHOLD_MS {
        return;
    }
    log();
}

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
        ipc_trace!(
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
        );
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
        ipc_trace!(
            "ipc slow {}: reply={} total_ms={} copy_ms={} reply_ms={} response_len={}",
            kind,
            reply,
            total_ms,
            copy_ms,
            reply_ms,
            response_len,
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_endpoint_epoch_changes_on_every_publication_boundary() {
        assert_eq!(next_service_endpoint_epoch(0), Some(1));
        assert_eq!(next_service_endpoint_epoch(1), Some(2));
        assert_eq!(next_service_endpoint_epoch(u64::MAX), None);
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

    fn ready_netd_poll_response() -> [u8; NETD_IPC_RESPONSE_HEADER_SIZE] {
        let mut response = [0_u8; NETD_IPC_RESPONSE_HEADER_SIZE];
        response[0..2].copy_from_slice(&NETD_IPC_ABI_VERSION.to_ne_bytes());
        response[2..4].copy_from_slice(&SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET.to_ne_bytes());
        response[16..24].copy_from_slice(&1_u64.to_ne_bytes());
        response[28..32].copy_from_slice(&NETD_IPC_RESPONSE_FLAG_LATENCY_HANDOFF.to_ne_bytes());
        response
    }

    #[test]
    fn only_live_netd_authorized_socket_response_requests_latency_handoff() {
        let response = ready_netd_poll_response();
        assert!(netd_response_requests_latency_handoff(&response, true));
        assert!(!netd_response_requests_latency_handoff(&response, false));

        let mut wrong_op = response;
        wrong_op[2..4].copy_from_slice(&SYSCALL_OFFLOAD_OP_LINUX_BIND.to_ne_bytes());
        assert!(!netd_response_requests_latency_handoff(&wrong_op, true));

        let mut local_data = response;
        local_data[2..4].copy_from_slice(&SYSCALL_OFFLOAD_OP_LINUX_RECVMSG.to_ne_bytes());
        assert!(netd_response_requests_latency_handoff(&local_data, true));

        let mut no_readiness = response;
        no_readiness[16..24].copy_from_slice(&0_u64.to_ne_bytes());
        assert!(!netd_response_requests_latency_handoff(&no_readiness, true));

        let mut completed_drain = no_readiness;
        completed_drain[2..4].copy_from_slice(&SYSCALL_OFFLOAD_OP_LINUX_RECVMSG.to_ne_bytes());
        completed_drain[4..8].copy_from_slice(&(LINUX_EAGAIN as i32).to_ne_bytes());
        assert!(netd_response_requests_latency_handoff(
            &completed_drain,
            true
        ));

        let mut unknown_flags = response;
        unknown_flags[28..32]
            .copy_from_slice(&(NETD_IPC_RESPONSE_FLAG_LATENCY_HANDOFF | (1 << 31)).to_ne_bytes());
        assert!(!netd_response_requests_latency_handoff(
            &unknown_flags,
            true
        ));
    }
}
