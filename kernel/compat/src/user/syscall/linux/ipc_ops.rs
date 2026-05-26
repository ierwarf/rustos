use super::*;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::mem::size_of;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use kernel_ipc_runtime::api::{KernelEndpointHandle, KernelReplyHandle, KernelTransferredHandle};
use lazy_static::lazy_static;
use spin::Mutex;

const MAX_SERVICE_ENDPOINTS: usize = 16;
const MAX_PENDING_TRANSFER_OBJECTS: usize = 1024;
const SLOW_IPC_THRESHOLD_MS: u64 = 10;
const MAX_SLOW_IPC_LOGS: usize = 20;
const EARLY_IPC_SAMPLE_COUNT: usize = 6;
static LINUX_SYSCALL_ENDPOINT: AtomicU64 = AtomicU64::new(0);
static SERVICE_ENDPOINTS: [AtomicU64; MAX_SERVICE_ENDPOINTS] =
    [const { AtomicU64::new(0) }; MAX_SERVICE_ENDPOINTS];
static SERVICE_ENDPOINT_OWNERS: [AtomicU64; MAX_SERVICE_ENDPOINTS] =
    [const { AtomicU64::new(0) }; MAX_SERVICE_ENDPOINTS];
static SERVICE_ENDPOINT_CAPS: [AtomicU64; MAX_SERVICE_ENDPOINTS] =
    [const { AtomicU64::new(0) }; MAX_SERVICE_ENDPOINTS];
static NEXT_TRANSFER_ID: AtomicU64 = AtomicU64::new(1);
static SLOW_IPC_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);

lazy_static! {
    static ref TRANSFER_OBJECTS: Mutex<BTreeMap<u64, multitask::TransferredHandleEntry>> =
        Mutex::new(BTreeMap::new());
}

pub(super) fn is_linux_rustos_ipc_syscall(syscall_number: u64) -> bool {
    matches!(
        syscall_number,
        linux_abi::SYS_RUSTOS_IPC_ENDPOINT_CREATE
            | linux_abi::SYS_RUSTOS_IPC_CALL
            | linux_abi::SYS_RUSTOS_IPC_RECV
            | linux_abi::SYS_RUSTOS_IPC_TRY_RECV
            | linux_abi::SYS_RUSTOS_IPC_REPLY
            | linux_abi::SYS_RUSTOS_IPC_CALL_WITH_HANDLES
            | linux_abi::SYS_RUSTOS_IPC_RECV_WITH_HANDLES
            | linux_abi::SYS_RUSTOS_IPC_REPLY_WITH_HANDLES
            | linux_abi::SYS_RUSTOS_IPC_REGISTER_LINUX_SYSCALL_ENDPOINT
            | linux_abi::SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT
            | linux_abi::SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT
    )
}

pub(super) fn dispatch_linux_rustos_ipc_syscall(frame: &SyscallFrame) -> u64 {
    match frame.rax {
        linux_abi::SYS_RUSTOS_IPC_ENDPOINT_CREATE => syscall_linux_rustos_ipc_endpoint_create(),
        linux_abi::SYS_RUSTOS_IPC_CALL => {
            syscall_linux_rustos_ipc_call(frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8)
        }
        linux_abi::SYS_RUSTOS_IPC_RECV => {
            syscall_linux_rustos_ipc_recv(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_RUSTOS_IPC_TRY_RECV => {
            syscall_linux_rustos_ipc_try_recv(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
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
        _ => linux_errno(LINUX_ENOSYS),
    }
}

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

/// 프로세스 종료 시 해당 프로세스가 등록한 모든 IPC 서비스 엔드포인트를 해제한다.
/// stale endpoint가 남아 있으면 이후 호출자가 wait_for_reply에서 무한 대기하게 되므로
/// 반드시 프로세스 종료 경로에서 호출해야 한다.
pub(crate) fn cleanup_service_endpoints_for_process(process_id: u64) {
    if SERVICE_ENDPOINT_OWNERS[linux_abi::IPC_SERVICE_LINUX_SYSCALLD as usize]
        .load(Ordering::Acquire)
        == process_id
    {
        LINUX_SYSCALL_ENDPOINT.store(0, Ordering::Release);
    }
    for i in 0..MAX_SERVICE_ENDPOINTS {
        if SERVICE_ENDPOINT_OWNERS[i].load(Ordering::Acquire) == process_id {
            SERVICE_ENDPOINTS[i].store(0, Ordering::Release);
            SERVICE_ENDPOINT_OWNERS[i].store(0, Ordering::Release);
            SERVICE_ENDPOINT_CAPS[i].store(0, Ordering::Release);
            debug::println!(
                "ipc service endpoint revoked on process exit: index={} process={}",
                i,
                process_id
            );
        }
    }
}

pub(super) fn current_process_has_service_capability(capability: u64) -> bool {
    if capability == 0 {
        return false;
    }
    let Some(process_id) = multitask::current_user_process_id() else {
        return false;
    };
    SERVICE_ENDPOINT_OWNERS
        .iter()
        .zip(SERVICE_ENDPOINT_CAPS.iter())
        .any(|(owner, caps)| {
            owner.load(Ordering::Acquire) == process_id
                && caps.load(Ordering::Acquire) & capability == capability
        })
}

pub(super) fn current_process_has_any_service_capability(capability_mask: u64) -> bool {
    if capability_mask == 0 {
        return false;
    }
    let Some(process_id) = multitask::current_user_process_id() else {
        return false;
    };
    SERVICE_ENDPOINT_OWNERS
        .iter()
        .zip(SERVICE_ENDPOINT_CAPS.iter())
        .any(|(owner, caps)| {
            owner.load(Ordering::Acquire) == process_id
                && caps.load(Ordering::Acquire) & capability_mask != 0
        })
}

fn service_endpoint_raw(service_id: u64) -> Option<u64> {
    service_index(service_id).map(|index| SERVICE_ENDPOINTS[index].load(Ordering::Acquire))
}

fn service_index(service_id: u64) -> Option<usize> {
    let index = usize::try_from(service_id).ok()?;
    (index < MAX_SERVICE_ENDPOINTS).then_some(index)
}

pub(super) fn syscall_linux_rustos_ipc_endpoint_create() -> u64 {
    let Some(task_id) = multitask::current_task_id() else {
        return linux_errno(LINUX_EINVAL);
    };
    match kernel_ipc_runtime::api::create_endpoint_for_task(task_id) {
        Ok(endpoint) => {
            debug::println!(
                "ipc endpoint created: task={} endpoint={}",
                task_id,
                endpoint.raw()
            );
            endpoint.raw()
        }
        Err(err) => linux_errno(ipc_error_to_linux_errno(err)),
    }
}

pub(super) fn syscall_linux_rustos_ipc_register_linux_syscall_endpoint(endpoint: u64) -> u64 {
    if endpoint == 0 {
        return linux_errno(LINUX_EINVAL);
    }
    let Some(process_id) = multitask::current_user_process_id() else {
        return linux_errno(LINUX_EINVAL);
    };
    LINUX_SYSCALL_ENDPOINT.store(endpoint, Ordering::Release);
    SERVICE_ENDPOINTS[linux_abi::IPC_SERVICE_LINUX_SYSCALLD as usize]
        .store(endpoint, Ordering::Release);
    SERVICE_ENDPOINT_OWNERS[linux_abi::IPC_SERVICE_LINUX_SYSCALLD as usize]
        .store(process_id, Ordering::Release);
    SERVICE_ENDPOINT_CAPS[linux_abi::IPC_SERVICE_LINUX_SYSCALLD as usize].store(
        rustos_user_abi::syscall::IPC_SERVICE_CAP_LINUX_SYSCALL_POLICY,
        Ordering::Release,
    );
    debug::println!(
        "ipc service registered: service={} endpoint={} owner={} caps={:#x}",
        linux_abi::IPC_SERVICE_LINUX_SYSCALLD,
        endpoint,
        process_id,
        rustos_user_abi::syscall::IPC_SERVICE_CAP_LINUX_SYSCALL_POLICY
    );
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
    if endpoint == 0 {
        SERVICE_ENDPOINTS[index].store(0, Ordering::Release);
        SERVICE_ENDPOINT_OWNERS[index].store(0, Ordering::Release);
        SERVICE_ENDPOINT_CAPS[index].store(0, Ordering::Release);
        debug::println!("ipc service revoked: service={}", service_id);
        return 0;
    }
    if SERVICE_ENDPOINTS[index].load(Ordering::Acquire) != 0 {
        return linux_errno(LINUX_EBUSY);
    }
    let capability = service_capability(service_id);
    if capability == 0 {
        return linux_errno(LINUX_EINVAL);
    }
    SERVICE_ENDPOINTS[index].store(endpoint, Ordering::Release);
    SERVICE_ENDPOINT_OWNERS[index].store(process_id, Ordering::Release);
    SERVICE_ENDPOINT_CAPS[index].store(capability, Ordering::Release);
    debug::println!(
        "ipc service registered: service={} endpoint={} owner={} caps={:#x}",
        service_id,
        endpoint,
        process_id,
        capability
    );
    0
}

fn service_capability(service_id: u64) -> u64 {
    match service_id {
        linux_abi::IPC_SERVICE_LINUX_SYSCALLD => {
            rustos_user_abi::syscall::IPC_SERVICE_CAP_LINUX_SYSCALL_POLICY
        }
        linux_abi::IPC_SERVICE_VFSD => rustos_user_abi::syscall::IPC_SERVICE_CAP_VFS_POLICY,
        linux_abi::IPC_SERVICE_NETD => rustos_user_abi::syscall::IPC_SERVICE_CAP_NET_POLICY,
        linux_abi::IPC_SERVICE_DEVMGRD => rustos_user_abi::syscall::IPC_SERVICE_CAP_DEVICE_POLICY,
        linux_abi::IPC_SERVICE_DRIVERD => rustos_user_abi::syscall::IPC_SERVICE_CAP_DRIVER_POLICY,
        linux_abi::IPC_SERVICE_LOADERD => rustos_user_abi::syscall::IPC_SERVICE_CAP_PROCESS_LOADER,
        linux_abi::IPC_SERVICE_STORAGED => rustos_user_abi::syscall::IPC_SERVICE_CAP_STORAGE_POLICY,
        linux_abi::IPC_SERVICE_INPUTD => rustos_user_abi::syscall::IPC_SERVICE_CAP_INPUT_POLICY,
        linux_abi::IPC_SERVICE_PROCD => rustos_user_abi::syscall::IPC_SERVICE_CAP_PROCESS_POLICY,
        linux_abi::IPC_SERVICE_ROOTD => {
            rustos_user_abi::syscall::IPC_SERVICE_CAP_ROOT_SUPERVISOR
                | rustos_user_abi::syscall::IPC_SERVICE_CAP_PROCESS_POLICY
        }
        linux_abi::IPC_SERVICE_SESSIOND => rustos_user_abi::syscall::IPC_SERVICE_CAP_SESSION_POLICY,
        linux_abi::IPC_SERVICE_PAGERD => rustos_user_abi::syscall::IPC_SERVICE_CAP_PAGER_POLICY,
        linux_abi::IPC_SERVICE_SERVICE_DRIVERD => {
            rustos_user_abi::syscall::IPC_SERVICE_CAP_SERVICE_DRIVER_POLICY
        }
        linux_abi::IPC_SERVICE_UISERVER => rustos_user_abi::syscall::IPC_SERVICE_CAP_UI_POLICY,
        _ => 0,
    }
}

pub(super) fn syscall_linux_rustos_ipc_lookup_service_endpoint(service_id: u64) -> u64 {
    let Some(raw) = service_endpoint_raw(service_id) else {
        return linux_errno(LINUX_EINVAL);
    };
    if raw == 0 {
        return linux_errno(LINUX_ENOSYS);
    }
    raw
}

pub(super) fn syscall_linux_rustos_ipc_call(
    endpoint: u64,
    request_ptr: u64,
    request_len: u64,
    reply_ptr: u64,
    reply_capacity: u64,
) -> u64 {
    let endpoint = KernelEndpointHandle::from_raw(endpoint);
    let start_ticks = crate::arch::rtc::ticks();
    let request = match copy_request_from_user(request_ptr, request_len) {
        Ok(request) => request,
        Err(errno) => return linux_errno(errno),
    };
    let copy_ticks = crate::arch::rtc::ticks();
    let reply = match enqueue_call_and_wake(endpoint, request.as_slice()) {
        Ok(reply) => reply,
        Err(errno) => return linux_errno(errno),
    };
    let send_ticks = crate::arch::rtc::ticks();
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
    let Ok(request_capacity) = usize::try_from(request_capacity) else {
        return linux_errno(LINUX_EINVAL);
    };
    loop {
        match kernel_ipc_runtime::api::recv_endpoint_with_limit(endpoint, request_capacity) {
            Ok(Some((reply, request))) => {
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
                return request.len() as u64;
            }
            Ok(None) => {
                let Some(task_id) = multitask::current_task_id() else {
                    return linux_errno(LINUX_EINVAL);
                };
                if !multitask::arm_block_current_task() {
                    return linux_errno(LINUX_EINVAL);
                }
                let pending = match kernel_ipc_runtime::api::add_endpoint_receiver_waiter(
                    endpoint, task_id,
                ) {
                    Ok(pending) => pending,
                    Err(err) => {
                        let _ = multitask::commit_block_current_task();
                        return linux_errno(ipc_error_to_linux_errno(err));
                    }
                };
                if pending {
                    let _ = multitask::commit_block_current_task();
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
    recv_endpoint_once(endpoint, request_ptr, request_capacity, reply_cap_ptr)
        .map_or_else(linux_errno, |received| received as u64)
}

fn recv_endpoint_once(
    endpoint: u64,
    request_ptr: u64,
    request_capacity: u64,
    reply_cap_ptr: u64,
) -> Result<usize, i64> {
    let endpoint = KernelEndpointHandle::from_raw(endpoint);
    let request_capacity = usize::try_from(request_capacity).map_err(|_| LINUX_EINVAL)?;
    match kernel_ipc_runtime::api::recv_endpoint_with_limit(endpoint, request_capacity) {
        Ok(Some((reply, request))) => {
            if !request.is_empty() {
                usermem::write_current_user_bytes(request_ptr, &request)
                    .map_err(address_space_error_to_linux_errno)?;
            }
            usermem::write_current_user_bytes(reply_cap_ptr, &reply.raw().to_ne_bytes())
                .map_err(address_space_error_to_linux_errno)?;
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
    let task_id = match kernel_ipc_runtime::api::complete_endpoint_reply(
        KernelReplyHandle::from_raw(reply),
        response.as_slice(),
    ) {
        Ok(task_id) => task_id,
        Err(err) => return linux_errno(ipc_error_to_linux_errno(err)),
    };
    let reply_ticks = crate::arch::rtc::ticks();
    let _ = multitask::wake_task(task_id);
    // Direct hand-back to the caller: the service is about to wait on its
    // endpoint again, so donate the remaining quantum to the original caller
    // instead of round-robining away from a freshly-completed reply.
    multitask::set_next_pick_hint(task_id);
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
    if let Err(errno) = install_received_handles(
        reply_handles.as_slice(),
        args.recv_fds_ptr,
        args.recv_fd_count_ptr,
    ) {
        return linux_errno(errno);
    }
    if !response.is_empty() {
        if let Err(err) = usermem::write_current_user_bytes(args.reply_ptr, response.as_slice()) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
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
        match kernel_ipc_runtime::api::recv_endpoint_with_limits_and_handles(
            endpoint,
            request_capacity,
            usize::from(args.recv_fd_capacity),
        ) {
            Ok(Some((reply, request, handles))) => {
                if let Err(errno) = validate_received_handle_outputs(
                    args.recv_fds_ptr,
                    args.recv_fd_count_ptr,
                    handles.len(),
                ) {
                    return linux_errno(errno);
                }
                if let Err(errno) = install_received_handles(
                    handles.as_slice(),
                    args.recv_fds_ptr,
                    args.recv_fd_count_ptr,
                ) {
                    return linux_errno(errno);
                }
                if !request.is_empty() {
                    if let Err(err) = usermem::write_current_user_bytes(args.request_ptr, &request)
                    {
                        return linux_errno(address_space_error_to_linux_errno(err));
                    }
                }
                if let Err(err) = usermem::write_current_user_bytes(
                    args.reply_cap_ptr,
                    &reply.raw().to_ne_bytes(),
                ) {
                    return linux_errno(address_space_error_to_linux_errno(err));
                }
                return request.len() as u64;
            }
            Ok(None) => {
                let Some(task_id) = multitask::current_task_id() else {
                    return linux_errno(LINUX_EINVAL);
                };
                if !multitask::arm_block_current_task() {
                    return linux_errno(LINUX_EINVAL);
                }
                let pending = match kernel_ipc_runtime::api::add_endpoint_receiver_waiter(
                    endpoint, task_id,
                ) {
                    Ok(pending) => pending,
                    Err(err) => {
                        let _ = multitask::commit_block_current_task();
                        return linux_errno(ipc_error_to_linux_errno(err));
                    }
                };
                if pending {
                    let _ = multitask::commit_block_current_task();
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
    let task_id = match kernel_ipc_runtime::api::complete_endpoint_reply_with_handles(
        KernelReplyHandle::from_raw(args.reply_cap),
        response.as_slice(),
        send_handles.as_slice(),
    ) {
        Ok(task_id) => task_id,
        Err(err) => {
            drop_transfer_descriptors(send_handles.as_slice());
            return linux_errno(ipc_error_to_linux_errno(err));
        }
    };
    let _ = multitask::wake_task(task_id);
    multitask::set_next_pick_hint(task_id);
    0
}

pub(super) fn call_linux_syscall_endpoint(request: &[u8]) -> Result<Vec<u8>, i64> {
    let endpoint = linux_syscall_endpoint().ok_or(LINUX_ENOSYS)?;
    let start_ticks = crate::arch::rtc::ticks();
    let reply = enqueue_call_and_wake(endpoint, request)?;
    let send_ticks = crate::arch::rtc::ticks();
    let response = wait_for_reply(reply)?;
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
    let endpoint = service_endpoint(service_id).ok_or(LINUX_ENOSYS)?;
    let start_ticks = crate::arch::rtc::ticks();
    let reply = enqueue_call_and_wake(endpoint, request)?;
    let send_ticks = crate::arch::rtc::ticks();
    let response = wait_for_reply(reply)?;
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
    let (response, descriptors) = wait_for_reply_with_handle_limit(reply, handle_capacity)?;
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
    if let Some(receiver_task_id) = receiver_to_wake {
        let _ = multitask::wake_task(receiver_task_id);
        // L4-style direct switch: hint the scheduler to run the receiver next,
        // so the caller's subsequent `yield_now` in `wait_for_reply` immediately
        // hands the CPU to the service task. Without this, the receiver waits
        // for a full round-robin trip — which on TCG dominates per-IPC latency
        // and balloons service-spawn into multiple seconds.
        multitask::set_next_pick_hint(receiver_task_id);
    }
    Ok(reply)
}

fn wait_for_reply(reply: KernelReplyHandle) -> Result<Vec<u8>, i64> {
    Ok(wait_for_reply_with_handle_limit(reply, 0)?.0)
}

fn wait_for_reply_with_handle_limit(
    reply: KernelReplyHandle,
    handle_capacity: usize,
) -> Result<(Vec<u8>, Vec<KernelTransferredHandle>), i64> {
    loop {
        match kernel_ipc_runtime::api::take_endpoint_response_with_handle_limit(
            reply,
            handle_capacity,
        ) {
            Ok(Some(response)) => return Ok(response),
            Ok(None) => {
                if !multitask::arm_block_current_task() {
                    return Err(LINUX_EINVAL);
                }
                // Re-poll after arming. If the replier completed the response between our
                // first take and arming, the wake_task call landed before arm_block, so
                // wake_armed would be set but no further wake arrives; re-checking the
                // queue here picks up that response without sleeping.
                match kernel_ipc_runtime::api::take_endpoint_response_with_handle_limit(
                    reply,
                    handle_capacity,
                ) {
                    Ok(Some(response)) => {
                        let _ = multitask::commit_block_current_task();
                        return Ok(response);
                    }
                    Ok(None) => {}
                    Err(err) => {
                        let _ = multitask::commit_block_current_task();
                        return Err(ipc_error_to_linux_errno(err));
                    }
                }
                match multitask::commit_block_current_task() {
                    Some(true) => multitask::yield_now(),
                    Some(false) => multitask::yield_now(),
                    None => return Err(LINUX_EINVAL),
                }
            }
            Err(err) => return Err(ipc_error_to_linux_errno(err)),
        }
    }
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
    let Some(entries) = multitask::with_current_user_process_state(|_, _, process_state| {
        let mut entries = Vec::with_capacity(fds.len());
        for fd in fds {
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

    let mut objects = TRANSFER_OBJECTS.lock();
    if objects.len().saturating_add(entries.len()) > MAX_PENDING_TRANSFER_OBJECTS {
        return Err(LINUX_ENOMEM);
    }

    let mut inserted_ids = Vec::with_capacity(entries.len());
    let mut descriptors = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(transfer_id) = allocate_transfer_id(&objects) else {
            for transfer_id in inserted_ids {
                objects.remove(&transfer_id);
            }
            return Err(LINUX_ENOMEM);
        };
        let Some(descriptor) = entry.ipc_descriptor(transfer_id) else {
            for transfer_id in inserted_ids {
                objects.remove(&transfer_id);
            }
            return Err(LINUX_EINVAL);
        };
        objects.insert(transfer_id, entry);
        inserted_ids.push(transfer_id);
        descriptors.push(descriptor);
    }
    Ok(descriptors)
}

fn allocate_transfer_id(objects: &BTreeMap<u64, multitask::TransferredHandleEntry>) -> Option<u64> {
    for _ in 0..MAX_PENDING_TRANSFER_OBJECTS {
        let id = NEXT_TRANSFER_ID.fetch_add(1, Ordering::Relaxed);
        if id != 0 && !objects.contains_key(&id) {
            return Some(id);
        }
    }
    None
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
    let Some(fds) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        let mut fds = Vec::with_capacity(entries.len());
        for entry in entries {
            let fd = process_state.handles_mut().install_transferred(entry);
            let Ok(fd) = i32::try_from(fd) else {
                return Err(LINUX_EOVERFLOW);
            };
            fds.push(fd);
        }
        Ok(fds)
    }) else {
        return Err(LINUX_EINVAL);
    };
    let fds = fds?;

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

fn take_transfer_entries(
    descriptors: &[KernelTransferredHandle],
) -> Result<Vec<multitask::TransferredHandleEntry>, i64> {
    let mut objects = TRANSFER_OBJECTS.lock();
    for descriptor in descriptors {
        let Some(entry) = objects.get(&descriptor.transfer_id()) else {
            return Err(LINUX_ESTALE);
        };
        if entry.ipc_descriptor(descriptor.transfer_id()) != Some(*descriptor) {
            return Err(LINUX_EINVAL);
        }
    }

    let mut entries = Vec::with_capacity(descriptors.len());
    for descriptor in descriptors {
        let Some(entry) = objects.remove(&descriptor.transfer_id()) else {
            return Err(LINUX_ESTALE);
        };
        entries.push(entry);
    }
    Ok(entries)
}

fn drop_transfer_descriptors(descriptors: &[KernelTransferredHandle]) {
    if descriptors.is_empty() {
        return;
    }
    let mut objects = TRANSFER_OBJECTS.lock();
    for descriptor in descriptors {
        objects.remove(&descriptor.transfer_id());
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
        debug::println!(
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

fn log_slow_ipc_recv(
    kind: &str,
    endpoint: u64,
    start_ticks: u64,
    recv_ticks: u64,
    request_len: usize,
) {
    let total_ms = ticks_elapsed_ms(start_ticks, recv_ticks);
    maybe_log_slow_ipc(total_ms, || {
        debug::println!(
            "ipc slow {}: endpoint={} total_ms={} request_len={}",
            kind,
            endpoint,
            total_ms,
            request_len,
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
        debug::println!(
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
