use std::io::Write;
use std::mem::size_of;
use std::thread;
use std::time::Duration;

use rustos_user_abi::syscall::{
    CommercialMaxCapabilityLeaseWire, CommercialMaxProtocolDescriptorWire,
    CommercialMaxProtocolRequest, CommercialMaxProtocolResponse, DevmgrdDeviceIoctlRequest,
    DevmgrdDeviceIoctlResponse, DevmgrdDeviceOpenRequest, DevmgrdDeviceOpenResponse,
    DevmgrdIpcRequest, DevmgrdIpcResponse, DevmgrdNodeEntry, IpcReplyWithHandlesArgs,
    RustosDeviceIoctlBrokerArgs, RustosDeviceOpenBrokerArgs,
    COMMERCIAL_MAX_DEVMGRD_OP_DEVICE_EVENT_SUBSCRIBE, COMMERCIAL_MAX_DEVMGRD_OP_DEVICE_MAP,
    COMMERCIAL_MAX_DEVMGRD_OP_DEVICE_OPEN, COMMERCIAL_MAX_DEVMGRD_OP_DEVICE_REGISTRY,
    COMMERCIAL_MAX_DEVMGRD_OP_IOCTL_AUTHORIZE, COMMERCIAL_MAX_PROTOCOL_ABI_VERSION,
    COMMERCIAL_MAX_PROTOCOL_DEVMGRD, COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS,
    COMMERCIAL_MAX_PROTOCOL_SESSIOND, COMMERCIAL_MAX_PROTOCOL_UISERVER,
    COMMERCIAL_MAX_SESSIOND_OP_CONSOLE_ROUTE, COMMERCIAL_MAX_SESSIOND_OP_FOREGROUND_FOCUS,
    COMMERCIAL_MAX_SESSIOND_OP_SESSION_GRAPH, COMMERCIAL_MAX_UISERVER_OP_DISPLAY_METADATA,
    COMMERCIAL_MAX_UISERVER_OP_PRESENT_POLICY, COMMERCIAL_MAX_UISERVER_OP_SURFACE_POLICY,
    DEVMGRD_DEVICE_ACCESS_EVDEV, DEVMGRD_DEVICE_ACCESS_NATIVE, DEVMGRD_DEVICE_ID_CONSOLE,
    DEVMGRD_DEVICE_ID_DISPLAY, DEVMGRD_DEVICE_ID_INPUT, DEVMGRD_DEVICE_RIGHT_ADMIN,
    DEVMGRD_DEVICE_RIGHT_IOCTL, DEVMGRD_DEVICE_RIGHT_MAP, DEVMGRD_DEVICE_RIGHT_READ,
    DEVMGRD_DEVICE_RIGHT_TRANSFER, DEVMGRD_DEVICE_RIGHT_WRITE, DEVMGRD_IPC_ABI_VERSION,
    DEVMGRD_IPC_OP_IOCTL_AUTHORIZE, DEVMGRD_IPC_OP_LOOKUP, DEVMGRD_IPC_OP_OPEN,
    DEVMGRD_IPC_OP_READDIR, DEVMGRD_MAX_DIR_ENTRIES, DEVMGRD_NAME_CAPACITY,
    DEVMGRD_NODE_KIND_DEVICE, DEVMGRD_NODE_KIND_DIR, IPC_MAX_INLINE_BYTES, IPC_SERVICE_DEVMGRD,
    IPC_SERVICE_SESSIOND, IPC_SERVICE_UISERVER, SYS_RUSTOS_DEBUG_PRINT,
    SYS_RUSTOS_DEVICE_IOCTL_BROKER, SYS_RUSTOS_DEVICE_OPEN_BROKER, SYS_RUSTOS_IPC_CALL,
    SYS_RUSTOS_IPC_ENDPOINT_CREATE, SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT, SYS_RUSTOS_IPC_RECV,
    SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT, SYS_RUSTOS_IPC_REPLY,
    SYS_RUSTOS_IPC_REPLY_WITH_HANDLES,
};

const RECV_BACKOFF: Duration = Duration::from_millis(10);

fn main() {
    let endpoint = syscall0(SYS_RUSTOS_IPC_ENDPOINT_CREATE);
    if endpoint < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "devmgrd: endpoint create failed errno={}",
            -endpoint
        );
        return;
    }
    let register = syscall2(
        SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT,
        IPC_SERVICE_DEVMGRD,
        endpoint as u64,
    );
    if register < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "devmgrd: endpoint register failed errno={}",
            -register
        );
        return;
    }

    debug_line("devmgrd: device policy endpoint registered");
    serve(endpoint as u64);
}

fn serve(endpoint: u64) {
    loop {
        let mut request = [0_u8; IPC_MAX_INLINE_BYTES];
        let mut reply_cap = 0_u64;
        let received = syscall4(
            SYS_RUSTOS_IPC_RECV,
            endpoint,
            request.as_mut_ptr() as u64,
            request.len() as u64,
            (&mut reply_cap as *mut u64) as u64,
        );
        if received < 0 {
            thread::sleep(RECV_BACKOFF);
            continue;
        }

        let reply = match received as usize {
            size if size == size_of::<DevmgrdIpcRequest>() => {
                let request = read_unaligned::<DevmgrdIpcRequest>(&request);
                let response = dispatch_registry_request(&request);
                syscall3(
                    SYS_RUSTOS_IPC_REPLY,
                    reply_cap,
                    (&response as *const DevmgrdIpcResponse) as u64,
                    size_of::<DevmgrdIpcResponse>() as u64,
                )
            }
            size if size == size_of::<DevmgrdDeviceOpenRequest>() => {
                let request = read_unaligned::<DevmgrdDeviceOpenRequest>(&request);
                reply_device_open(reply_cap, &request)
            }
            size if size == size_of::<DevmgrdDeviceIoctlRequest>() => {
                let request = read_unaligned::<DevmgrdDeviceIoctlRequest>(&request);
                reply_device_ioctl(reply_cap, &request)
            }
            size if size == size_of::<CommercialMaxProtocolRequest>() => {
                let request = read_unaligned::<CommercialMaxProtocolRequest>(&request);
                reply_commercial_request(reply_cap, &request)
            }
            _ => {
                let response = DevmgrdIpcResponse {
                    status: libc::EINVAL,
                    ..DevmgrdIpcResponse::default()
                };
                syscall3(
                    SYS_RUSTOS_IPC_REPLY,
                    reply_cap,
                    (&response as *const DevmgrdIpcResponse) as u64,
                    size_of::<DevmgrdIpcResponse>() as u64,
                )
            }
        };
        if reply < 0 {
            let _ = writeln!(std::io::stderr(), "devmgrd: reply failed errno={}", -reply);
        }
    }
}

fn reply_commercial_request(reply_cap: u64, request: &CommercialMaxProtocolRequest) -> i64 {
    let mut response = CommercialMaxProtocolResponse {
        header: request.header,
        ..CommercialMaxProtocolResponse::default()
    };
    response.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    response.status = validate_commercial_request(request)
        .and_then(|_| dispatch_commercial_request(request, &mut response))
        .err()
        .unwrap_or(0);
    syscall3(
        SYS_RUSTOS_IPC_REPLY,
        reply_cap,
        (&response as *const CommercialMaxProtocolResponse) as u64,
        size_of::<CommercialMaxProtocolResponse>() as u64,
    )
}

fn reply_device_ioctl(reply_cap: u64, request: &DevmgrdDeviceIoctlRequest) -> i64 {
    let mut response = DevmgrdDeviceIoctlResponse {
        version: DEVMGRD_IPC_ABI_VERSION,
        op: DEVMGRD_IPC_OP_IOCTL_AUTHORIZE,
        ..DevmgrdDeviceIoctlResponse::default()
    };
    response.status =
        if let Some(IoctlPolicyOwner::Sessiond(op)) = ioctl_policy_owner(request.request) {
            if sessiond_executes_ioctl(request.request) {
                match call_sessiond_ioctl(op, request) {
                    Ok(session_response) => {
                        response.value = session_response.value0;
                        response.payload_len = session_response.payload_len;
                        let payload_len = session_response.payload_len as usize;
                        if payload_len <= response.payload.len() {
                            response.payload[..payload_len]
                                .copy_from_slice(&session_response.payload[..payload_len]);
                            0
                        } else {
                            libc::EINVAL
                        }
                    }
                    Err(errno) => errno,
                }
            } else {
                authorize_and_broker_ioctl(request, &mut response)
            }
        } else {
            authorize_and_broker_ioctl(request, &mut response)
        };
    syscall3(
        SYS_RUSTOS_IPC_REPLY,
        reply_cap,
        (&response as *const DevmgrdDeviceIoctlResponse) as u64,
        size_of::<DevmgrdDeviceIoctlResponse>() as u64,
    )
}

fn authorize_and_broker_ioctl(
    request: &DevmgrdDeviceIoctlRequest,
    response: &mut DevmgrdDeviceIoctlResponse,
) -> i32 {
    match authorize_ioctl_request(request) {
        Ok(()) => {
            let args = RustosDeviceIoctlBrokerArgs {
                process_id: request.pid,
                fd: request.fd,
                request: request.request,
                arg: request.arg,
                reserved0: 0,
            };
            let result = syscall1(
                SYS_RUSTOS_DEVICE_IOCTL_BROKER,
                (&args as *const RustosDeviceIoctlBrokerArgs) as u64,
            );
            if result < 0 {
                last_errno()
            } else {
                response.value = result as u64;
                0
            }
        }
        Err(errno) => errno,
    }
}

fn dispatch_registry_request(request: &DevmgrdIpcRequest) -> DevmgrdIpcResponse {
    let mut response = DevmgrdIpcResponse {
        op: request.op,
        ..DevmgrdIpcResponse::default()
    };
    response.status = match validate_registry_request(request) {
        Ok(path) => match request.op {
            DEVMGRD_IPC_OP_LOOKUP => lookup_device_node(path, &mut response),
            DEVMGRD_IPC_OP_READDIR => read_device_dir(path, &mut response),
            _ => libc::EINVAL,
        },
        Err(errno) => errno,
    };
    response
}

fn reply_device_open(reply_cap: u64, request: &DevmgrdDeviceOpenRequest) -> i64 {
    let mut response = DevmgrdDeviceOpenResponse {
        version: DEVMGRD_IPC_ABI_VERSION,
        op: DEVMGRD_IPC_OP_OPEN,
        ..DevmgrdDeviceOpenResponse::default()
    };
    let mut send_fd = -1_i64;
    response.status = match validate_device_open_request(request) {
        Ok(policy) => {
            response.device_id = policy.device_id;
            response.access = policy.access;
            response.rights = policy.rights;
            let args = RustosDeviceOpenBrokerArgs {
                abi_version: DEVMGRD_IPC_ABI_VERSION,
                device_id: policy.device_id,
                access: policy.access,
                reserved0: 0,
                rights: policy.rights,
                open_flags: request.open_flags,
                reserved1: 0,
            };
            let result = syscall1(
                SYS_RUSTOS_DEVICE_OPEN_BROKER,
                (&args as *const RustosDeviceOpenBrokerArgs) as u64,
            );
            if result < 0 {
                last_errno()
            } else {
                send_fd = result;
                0
            }
        }
        Err(errno) => errno,
    };
    if response.status != 0 {
        return syscall3(
            SYS_RUSTOS_IPC_REPLY,
            reply_cap,
            (&response as *const DevmgrdDeviceOpenResponse) as u64,
            size_of::<DevmgrdDeviceOpenResponse>() as u64,
        );
    }

    let send_fd_u64 = send_fd as u64;
    let args = IpcReplyWithHandlesArgs {
        reply_cap,
        response_ptr: (&response as *const DevmgrdDeviceOpenResponse) as u64,
        response_len: size_of::<DevmgrdDeviceOpenResponse>() as u64,
        send_fds_ptr: (&send_fd_u64 as *const u64) as u64,
        send_fd_count: 1,
        reserved0: 0,
        reserved1: 0,
    };
    let reply = syscall1(
        SYS_RUSTOS_IPC_REPLY_WITH_HANDLES,
        (&args as *const IpcReplyWithHandlesArgs) as u64,
    );
    if send_fd >= 0 {
        unsafe {
            libc::close(send_fd as libc::c_int);
        }
    }
    reply
}

#[derive(Clone, Copy)]
struct DeviceOpenPolicy {
    device_id: u16,
    access: u16,
    rights: u64,
}

fn validate_device_open_request(
    request: &DevmgrdDeviceOpenRequest,
) -> Result<DeviceOpenPolicy, i32> {
    if request.version != DEVMGRD_IPC_ABI_VERSION
        || request.op != DEVMGRD_IPC_OP_OPEN
        || request.flags != 0
        || request.reserved0 != 0
        || request.pid == 0
        || request.tid == 0
    {
        return Err(libc::EINVAL);
    }
    let path = validate_open_path(request)?;
    device_open_policy(path, request.open_flags)
}

fn device_open_policy(path: &str, open_flags: u64) -> Result<DeviceOpenPolicy, i32> {
    let writable = open_flags & libc::O_ACCMODE as u64 != libc::O_RDONLY as u64;
    match path {
        "/dev/console0" => Ok(DeviceOpenPolicy {
            device_id: DEVMGRD_DEVICE_ID_CONSOLE,
            access: DEVMGRD_DEVICE_ACCESS_NATIVE,
            rights: native_device_rights(),
        }),
        "/dev/display0" | "/dev/dri/card0" => Ok(DeviceOpenPolicy {
            device_id: DEVMGRD_DEVICE_ID_DISPLAY,
            access: DEVMGRD_DEVICE_ACCESS_NATIVE,
            rights: native_device_rights(),
        }),
        "/dev/input0" if !writable => Ok(DeviceOpenPolicy {
            device_id: DEVMGRD_DEVICE_ID_INPUT,
            access: DEVMGRD_DEVICE_ACCESS_NATIVE,
            rights: input_read_rights(),
        }),
        "/dev/input/event0" if !writable => Ok(DeviceOpenPolicy {
            device_id: DEVMGRD_DEVICE_ID_INPUT,
            access: DEVMGRD_DEVICE_ACCESS_EVDEV,
            rights: input_read_rights(),
        }),
        "/dev/input0" | "/dev/input/event0" => Err(libc::EACCES),
        _ => Err(libc::ENOENT),
    }
}

fn validate_open_path(request: &DevmgrdDeviceOpenRequest) -> Result<&str, i32> {
    let len = request.path_len as usize;
    if len == 0 || len > request.path.len() {
        return Err(libc::EINVAL);
    }
    std::str::from_utf8(&request.path[..len]).map_err(|_| libc::EINVAL)
}

fn native_device_rights() -> u64 {
    DEVMGRD_DEVICE_RIGHT_READ
        | DEVMGRD_DEVICE_RIGHT_WRITE
        | DEVMGRD_DEVICE_RIGHT_IOCTL
        | DEVMGRD_DEVICE_RIGHT_ADMIN
        | DEVMGRD_DEVICE_RIGHT_MAP
        | DEVMGRD_DEVICE_RIGHT_TRANSFER
}

fn input_read_rights() -> u64 {
    DEVMGRD_DEVICE_RIGHT_READ | DEVMGRD_DEVICE_RIGHT_TRANSFER
}

fn authorize_ioctl_request(request: &DevmgrdDeviceIoctlRequest) -> Result<(), i32> {
    if request.version != DEVMGRD_IPC_ABI_VERSION
        || request.op != DEVMGRD_IPC_OP_IOCTL_AUTHORIZE
        || request.flags != 0
        || request.payload_len as usize > request.payload.len()
        || request.reserved1 != 0
        || request.reserved0 != 0
        || request.pid == 0
        || request.tid == 0
    {
        return Err(libc::EINVAL);
    }
    match ioctl_policy_owner(request.request) {
        Some(IoctlPolicyOwner::Sessiond(_)) if sessiond_broker_commits_ioctl(request.request) => {
            Ok(())
        }
        Some(IoctlPolicyOwner::Sessiond(op)) => authorize_session_ioctl(op, request.request),
        Some(IoctlPolicyOwner::Uiserver(op)) => authorize_uiserver_ioctl(op, request.request),
        None => Err(libc::ENOTTY),
    }
}

fn sessiond_executes_ioctl(request_number: u64) -> bool {
    matches!(
        request_number,
        rustos_user_abi::console::CONSOLE_IOCTL_GET_STATE
            | rustos_user_abi::console::CONSOLE_IOCTL_SNAPSHOT_SESSIONS
            | rustos_user_abi::console::CONSOLE_IOCTL_SNAPSHOT_SESSION_OUTPUT
            | rustos_user_abi::console::CONSOLE_IOCTL_SEND_INPUT_EVENT
    )
}

fn sessiond_broker_commits_ioctl(request_number: u64) -> bool {
    matches!(
        request_number,
        rustos_user_abi::console::CONSOLE_IOCTL_CREATE_SESSION
            | rustos_user_abi::console::CONSOLE_IOCTL_CLOSE_SESSION
            | rustos_user_abi::console::CONSOLE_IOCTL_BIND_CURRENT_SESSION
            | rustos_user_abi::console::CONSOLE_IOCTL_SET_SESSION_STATE
            | rustos_user_abi::console::CONSOLE_IOCTL_SET_FOCUS
    )
}

fn call_sessiond_ioctl(
    op: u16,
    ioctl_request: &DevmgrdDeviceIoctlRequest,
) -> Result<CommercialMaxProtocolResponse, i32> {
    if ioctl_request.version != DEVMGRD_IPC_ABI_VERSION
        || ioctl_request.op != DEVMGRD_IPC_OP_IOCTL_AUTHORIZE
        || ioctl_request.flags != 0
        || ioctl_request.payload_len as usize > ioctl_request.payload.len()
        || ioctl_request.reserved1 != 0
        || ioctl_request.reserved0 != 0
        || ioctl_request.pid == 0
        || ioctl_request.tid == 0
    {
        return Err(libc::EINVAL);
    }
    let endpoint = syscall1(SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT, IPC_SERVICE_SESSIOND);
    if endpoint <= 0 {
        return Err(libc::ENOSYS);
    }
    let mut request = CommercialMaxProtocolRequest::default();
    request.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    request.header.protocol = COMMERCIAL_MAX_PROTOCOL_SESSIOND;
    request.header.op = op;
    request.header.subject_pid = ioctl_request.pid;
    request.header.subject_tid = ioctl_request.tid;
    request.arg0 = ioctl_request.request;
    request.arg1 = ioctl_request.fd;
    request.arg2 = ioctl_request.session_handle;
    request.payload_len = ioctl_request.payload_len;
    let payload_len = ioctl_request.payload_len as usize;
    request.payload[..payload_len].copy_from_slice(&ioctl_request.payload[..payload_len]);

    let mut response = CommercialMaxProtocolResponse::default();
    let result = syscall5(
        SYS_RUSTOS_IPC_CALL,
        endpoint as u64,
        (&request as *const CommercialMaxProtocolRequest) as u64,
        size_of::<CommercialMaxProtocolRequest>() as u64,
        (&mut response as *mut CommercialMaxProtocolResponse) as u64,
        size_of::<CommercialMaxProtocolResponse>() as u64,
    );
    if result < 0 {
        return Err(last_errno());
    }
    if result as usize != size_of::<CommercialMaxProtocolResponse>()
        || response.header.version != COMMERCIAL_MAX_PROTOCOL_ABI_VERSION
        || response.header.protocol != COMMERCIAL_MAX_PROTOCOL_SESSIOND
        || response.header.op != op
        || response.payload_len as usize > response.payload.len()
    {
        return Err(libc::EINVAL);
    }
    if response.status == 0 {
        Ok(response)
    } else {
        Err(response.status)
    }
}

#[derive(Clone, Copy)]
enum IoctlPolicyOwner {
    Sessiond(u16),
    Uiserver(u16),
}

fn ioctl_policy_owner(request_number: u64) -> Option<IoctlPolicyOwner> {
    match request_number {
        rustos_user_abi::device::DISPLAY_IOCTL_GET_INFO => Some(IoctlPolicyOwner::Uiserver(
            COMMERCIAL_MAX_UISERVER_OP_DISPLAY_METADATA,
        )),
        rustos_user_abi::device::DISPLAY_IOCTL_CREATE_SURFACE => Some(IoctlPolicyOwner::Uiserver(
            COMMERCIAL_MAX_UISERVER_OP_SURFACE_POLICY,
        )),
        rustos_user_abi::device::DISPLAY_IOCTL_PRESENT
        | rustos_user_abi::device::DISPLAY_IOCTL_PRESENT_RECT => Some(IoctlPolicyOwner::Uiserver(
            COMMERCIAL_MAX_UISERVER_OP_PRESENT_POLICY,
        )),
        rustos_user_abi::console::CONSOLE_IOCTL_GET_STATE
        | rustos_user_abi::console::CONSOLE_IOCTL_SNAPSHOT_SESSIONS => Some(
            IoctlPolicyOwner::Sessiond(COMMERCIAL_MAX_SESSIOND_OP_SESSION_GRAPH),
        ),
        rustos_user_abi::console::CONSOLE_IOCTL_SET_FOCUS => Some(IoctlPolicyOwner::Sessiond(
            COMMERCIAL_MAX_SESSIOND_OP_FOREGROUND_FOCUS,
        )),
        rustos_user_abi::console::CONSOLE_IOCTL_SNAPSHOT_SESSION_OUTPUT
        | rustos_user_abi::console::CONSOLE_IOCTL_SEND_INPUT_EVENT
        | rustos_user_abi::console::CONSOLE_IOCTL_CREATE_SESSION
        | rustos_user_abi::console::CONSOLE_IOCTL_CLOSE_SESSION
        | rustos_user_abi::console::CONSOLE_IOCTL_BIND_CURRENT_SESSION
        | rustos_user_abi::console::CONSOLE_IOCTL_SET_SESSION_STATE => Some(
            IoctlPolicyOwner::Sessiond(COMMERCIAL_MAX_SESSIOND_OP_CONSOLE_ROUTE),
        ),
        _ => None,
    }
}

fn authorize_session_ioctl(op: u16, request_number: u64) -> Result<(), i32> {
    let endpoint = syscall1(SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT, IPC_SERVICE_SESSIOND);
    if endpoint <= 0 {
        return Err(libc::ENOSYS);
    }
    let mut request = CommercialMaxProtocolRequest::default();
    request.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    request.header.protocol = COMMERCIAL_MAX_PROTOCOL_SESSIOND;
    request.header.op = op;
    request.arg0 = request_number;
    let mut response = CommercialMaxProtocolResponse::default();
    let result = syscall5(
        SYS_RUSTOS_IPC_CALL,
        endpoint as u64,
        (&request as *const CommercialMaxProtocolRequest) as u64,
        size_of::<CommercialMaxProtocolRequest>() as u64,
        (&mut response as *mut CommercialMaxProtocolResponse) as u64,
        size_of::<CommercialMaxProtocolResponse>() as u64,
    );
    if result < 0 {
        return Err(last_errno());
    }
    if result as usize != size_of::<CommercialMaxProtocolResponse>()
        || response.header.version != COMMERCIAL_MAX_PROTOCOL_ABI_VERSION
        || response.header.protocol != COMMERCIAL_MAX_PROTOCOL_SESSIOND
        || response.header.op != op
    {
        return Err(libc::EINVAL);
    }
    if response.status == 0 {
        Ok(())
    } else {
        Err(response.status)
    }
}

fn authorize_uiserver_ioctl(op: u16, request_number: u64) -> Result<(), i32> {
    let endpoint = syscall1(SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT, IPC_SERVICE_UISERVER);
    if endpoint <= 0 {
        return Err(libc::ENOSYS);
    }
    let mut request = CommercialMaxProtocolRequest::default();
    request.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    request.header.protocol = COMMERCIAL_MAX_PROTOCOL_UISERVER;
    request.header.op = op;
    request.arg0 = request_number;
    let mut response = CommercialMaxProtocolResponse::default();
    let result = syscall5(
        SYS_RUSTOS_IPC_CALL,
        endpoint as u64,
        (&request as *const CommercialMaxProtocolRequest) as u64,
        size_of::<CommercialMaxProtocolRequest>() as u64,
        (&mut response as *mut CommercialMaxProtocolResponse) as u64,
        size_of::<CommercialMaxProtocolResponse>() as u64,
    );
    if result < 0 {
        return Err(last_errno());
    }
    if result as usize != size_of::<CommercialMaxProtocolResponse>()
        || response.header.version != COMMERCIAL_MAX_PROTOCOL_ABI_VERSION
        || response.header.protocol != COMMERCIAL_MAX_PROTOCOL_UISERVER
        || response.header.op != op
    {
        return Err(libc::EINVAL);
    }
    if response.status == 0 {
        Ok(())
    } else {
        Err(response.status)
    }
}

fn validate_commercial_request(request: &CommercialMaxProtocolRequest) -> Result<(), i32> {
    if request.header.version != COMMERCIAL_MAX_PROTOCOL_ABI_VERSION
        || request.header.protocol != COMMERCIAL_MAX_PROTOCOL_DEVMGRD
        || request.header.flags != 0
        || request.path_len as usize > request.path.len()
        || request.payload_len as usize > request.payload.len()
    {
        return Err(libc::EINVAL);
    }
    match request.header.op {
        COMMERCIAL_MAX_DEVMGRD_OP_DEVICE_REGISTRY
        | COMMERCIAL_MAX_DEVMGRD_OP_DEVICE_OPEN
        | COMMERCIAL_MAX_DEVMGRD_OP_IOCTL_AUTHORIZE
        | COMMERCIAL_MAX_DEVMGRD_OP_DEVICE_MAP
        | COMMERCIAL_MAX_DEVMGRD_OP_DEVICE_EVENT_SUBSCRIBE => Ok(()),
        _ => Err(libc::EINVAL),
    }
}

fn dispatch_commercial_request(
    request: &CommercialMaxProtocolRequest,
    response: &mut CommercialMaxProtocolResponse,
) -> Result<(), i32> {
    match request.header.op {
        COMMERCIAL_MAX_DEVMGRD_OP_DEVICE_REGISTRY => {
            fill_device_descriptors(response, DEVICE_DESCRIPTORS);
            Ok(())
        }
        COMMERCIAL_MAX_DEVMGRD_OP_DEVICE_OPEN => {
            let path = commercial_request_path(request)?;
            let policy = device_open_policy(path, request.arg0)?;
            response.descriptor_count = 1;
            response.descriptors[0] = device_descriptor(path, policy);
            response.capability = device_capability(path, policy);
            response.value0 = policy.device_id as u64;
            response.value1 = policy.rights;
            Ok(())
        }
        COMMERCIAL_MAX_DEVMGRD_OP_IOCTL_AUTHORIZE => match ioctl_policy_owner(request.arg0) {
            Some(IoctlPolicyOwner::Sessiond(op)) => {
                response.value0 = request.arg0;
                response.value1 = op as u64;
                response.descriptor_count = 1;
                response.descriptors[0] = CommercialMaxProtocolDescriptorWire {
                    protocol: COMMERCIAL_MAX_PROTOCOL_SESSIOND,
                    op,
                    service_id: IPC_SERVICE_SESSIOND,
                    capability_mask: 1,
                    value0: request.arg0,
                    ..CommercialMaxProtocolDescriptorWire::default()
                };
                Ok(())
            }
            Some(IoctlPolicyOwner::Uiserver(op)) => {
                response.value0 = request.arg0;
                response.value1 = op as u64;
                response.descriptor_count = 1;
                response.descriptors[0] = CommercialMaxProtocolDescriptorWire {
                    protocol: COMMERCIAL_MAX_PROTOCOL_UISERVER,
                    op,
                    service_id: IPC_SERVICE_UISERVER,
                    capability_mask: 1,
                    value0: request.arg0,
                    ..CommercialMaxProtocolDescriptorWire::default()
                };
                Ok(())
            }
            None => Err(libc::ENOTTY),
        },
        COMMERCIAL_MAX_DEVMGRD_OP_DEVICE_MAP => {
            fill_device_descriptors(response, DISPLAY_DEVICE_DESCRIPTORS);
            Ok(())
        }
        COMMERCIAL_MAX_DEVMGRD_OP_DEVICE_EVENT_SUBSCRIBE => {
            fill_device_descriptors(response, INPUT_DEVICE_DESCRIPTORS);
            Ok(())
        }
        _ => Err(libc::EINVAL),
    }
}

fn commercial_request_path(request: &CommercialMaxProtocolRequest) -> Result<&str, i32> {
    let len = request.path_len as usize;
    if len == 0 || len > request.path.len() {
        return Err(libc::EINVAL);
    }
    std::str::from_utf8(&request.path[..len]).map_err(|_| libc::EINVAL)
}

fn validate_registry_request(request: &DevmgrdIpcRequest) -> Result<&str, i32> {
    if request.version != DEVMGRD_IPC_ABI_VERSION || request.flags != 0 || request.reserved0 != 0 {
        return Err(libc::EINVAL);
    }
    let len = request.path_len as usize;
    if len == 0 || len > request.path.len() {
        return Err(libc::EINVAL);
    }
    std::str::from_utf8(&request.path[..len]).map_err(|_| libc::EINVAL)
}

const DEVICE_DESCRIPTORS: &[(&str, DeviceOpenPolicy)] = &[
    (
        "/dev/console0",
        DeviceOpenPolicy {
            device_id: DEVMGRD_DEVICE_ID_CONSOLE,
            access: DEVMGRD_DEVICE_ACCESS_NATIVE,
            rights: DEVMGRD_DEVICE_RIGHT_READ
                | DEVMGRD_DEVICE_RIGHT_WRITE
                | DEVMGRD_DEVICE_RIGHT_IOCTL
                | DEVMGRD_DEVICE_RIGHT_ADMIN
                | DEVMGRD_DEVICE_RIGHT_MAP
                | DEVMGRD_DEVICE_RIGHT_TRANSFER,
        },
    ),
    (
        "/dev/display0",
        DeviceOpenPolicy {
            device_id: DEVMGRD_DEVICE_ID_DISPLAY,
            access: DEVMGRD_DEVICE_ACCESS_NATIVE,
            rights: DEVMGRD_DEVICE_RIGHT_READ
                | DEVMGRD_DEVICE_RIGHT_WRITE
                | DEVMGRD_DEVICE_RIGHT_IOCTL
                | DEVMGRD_DEVICE_RIGHT_ADMIN
                | DEVMGRD_DEVICE_RIGHT_MAP
                | DEVMGRD_DEVICE_RIGHT_TRANSFER,
        },
    ),
    (
        "/dev/dri/card0",
        DeviceOpenPolicy {
            device_id: DEVMGRD_DEVICE_ID_DISPLAY,
            access: DEVMGRD_DEVICE_ACCESS_NATIVE,
            rights: DEVMGRD_DEVICE_RIGHT_READ
                | DEVMGRD_DEVICE_RIGHT_WRITE
                | DEVMGRD_DEVICE_RIGHT_IOCTL
                | DEVMGRD_DEVICE_RIGHT_ADMIN
                | DEVMGRD_DEVICE_RIGHT_MAP
                | DEVMGRD_DEVICE_RIGHT_TRANSFER,
        },
    ),
    (
        "/dev/input0",
        DeviceOpenPolicy {
            device_id: DEVMGRD_DEVICE_ID_INPUT,
            access: DEVMGRD_DEVICE_ACCESS_NATIVE,
            rights: DEVMGRD_DEVICE_RIGHT_READ | DEVMGRD_DEVICE_RIGHT_TRANSFER,
        },
    ),
    (
        "/dev/input/event0",
        DeviceOpenPolicy {
            device_id: DEVMGRD_DEVICE_ID_INPUT,
            access: DEVMGRD_DEVICE_ACCESS_EVDEV,
            rights: DEVMGRD_DEVICE_RIGHT_READ | DEVMGRD_DEVICE_RIGHT_TRANSFER,
        },
    ),
];

const DISPLAY_DEVICE_DESCRIPTORS: &[(&str, DeviceOpenPolicy)] =
    &[DEVICE_DESCRIPTORS[1], DEVICE_DESCRIPTORS[2]];
const INPUT_DEVICE_DESCRIPTORS: &[(&str, DeviceOpenPolicy)] =
    &[DEVICE_DESCRIPTORS[3], DEVICE_DESCRIPTORS[4]];

fn fill_device_descriptors(
    response: &mut CommercialMaxProtocolResponse,
    descriptors: &[(&str, DeviceOpenPolicy)],
) {
    let count = descriptors
        .len()
        .min(COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS);
    response.descriptor_count = count as u16;
    response.value0 = descriptors.len() as u64;
    for (index, (path, policy)) in descriptors.iter().take(count).enumerate() {
        response.descriptors[index] = device_descriptor(path, *policy);
    }
}

fn device_descriptor(path: &str, policy: DeviceOpenPolicy) -> CommercialMaxProtocolDescriptorWire {
    let mut descriptor = CommercialMaxProtocolDescriptorWire {
        protocol: COMMERCIAL_MAX_PROTOCOL_DEVMGRD,
        op: COMMERCIAL_MAX_DEVMGRD_OP_DEVICE_OPEN,
        service_id: policy.device_id as u64,
        capability_mask: policy.rights,
        value0: policy.access as u64,
        value1: policy.rights,
        ..CommercialMaxProtocolDescriptorWire::default()
    };
    copy_label(
        path.as_bytes(),
        &mut descriptor.name,
        &mut descriptor.name_len,
    );
    descriptor
}

fn device_capability(path: &str, policy: DeviceOpenPolicy) -> CommercialMaxCapabilityLeaseWire {
    let mut capability = CommercialMaxCapabilityLeaseWire {
        lease_id: policy.device_id as u64,
        service_id: IPC_SERVICE_DEVMGRD,
        capability_mask: policy.rights,
        rights_mask: policy.rights,
        generation: policy.access as u64,
        ..CommercialMaxCapabilityLeaseWire::default()
    };
    copy_label(
        path.as_bytes(),
        &mut capability.label,
        &mut capability.label_len,
    );
    capability
}

fn copy_label(src: &[u8], dest: &mut [u8], len: &mut u16) {
    let count = src.len().min(dest.len());
    dest[..count].copy_from_slice(&src[..count]);
    *len = count as u16;
}

fn lookup_device_node(path: &str, response: &mut DevmgrdIpcResponse) -> i32 {
    response.kind = match path {
        "/dev" | "/dev/input" | "/dev/dri" => DEVMGRD_NODE_KIND_DIR,
        "/dev/console0" | "/dev/display0" | "/dev/input0" | "/dev/input/event0"
        | "/dev/dri/card0" => DEVMGRD_NODE_KIND_DEVICE,
        _ => return libc::ENOENT,
    };
    0
}

fn read_device_dir(path: &str, response: &mut DevmgrdIpcResponse) -> i32 {
    let entries: &[(&str, u16)] = match path {
        "/dev" => &[
            ("console0", DEVMGRD_NODE_KIND_DEVICE),
            ("display0", DEVMGRD_NODE_KIND_DEVICE),
            ("input0", DEVMGRD_NODE_KIND_DEVICE),
            ("input", DEVMGRD_NODE_KIND_DIR),
            ("dri", DEVMGRD_NODE_KIND_DIR),
        ],
        "/dev/input" => &[("event0", DEVMGRD_NODE_KIND_DEVICE)],
        "/dev/dri" => &[("card0", DEVMGRD_NODE_KIND_DEVICE)],
        _ => return libc::ENOENT,
    };
    if entries.len() > DEVMGRD_MAX_DIR_ENTRIES {
        return libc::EOVERFLOW;
    }
    for (index, (name, kind)) in entries.iter().enumerate() {
        let Some(entry) = encode_node_entry(name, *kind) else {
            return libc::EOVERFLOW;
        };
        response.entries[index] = entry;
    }
    response.entry_count = entries.len() as u32;
    response.kind = DEVMGRD_NODE_KIND_DIR;
    0
}

fn encode_node_entry(name: &str, kind: u16) -> Option<DevmgrdNodeEntry> {
    let bytes = name.as_bytes();
    if bytes.len() > DEVMGRD_NAME_CAPACITY {
        return None;
    }
    let mut entry = DevmgrdNodeEntry {
        name_len: bytes.len() as u16,
        kind,
        ..DevmgrdNodeEntry::default()
    };
    entry.name[..bytes.len()].copy_from_slice(bytes);
    Some(entry)
}

fn syscall0(number: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long) as i64 }
}

fn syscall1(number: u64, arg0: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0) as i64 }
}

fn syscall2(number: u64, arg0: u64, arg1: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1) as i64 }
}

fn syscall3(number: u64, arg0: u64, arg1: u64, arg2: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1, arg2) as i64 }
}

fn syscall4(number: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1, arg2, arg3) as i64 }
}

fn syscall5(number: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1, arg2, arg3, arg4) as i64 }
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

fn debug_line(message: &str) {
    let _ = syscall2(
        SYS_RUSTOS_DEBUG_PRINT,
        message.as_ptr() as u64,
        message.len() as u64,
    );
    let _ = syscall2(SYS_RUSTOS_DEBUG_PRINT, b"\n".as_ptr() as u64, 1);
}

fn read_unaligned<T: Copy>(bytes: &[u8]) -> T {
    assert!(bytes.len() >= size_of::<T>());
    unsafe { bytes.as_ptr().cast::<T>().read_unaligned() }
}
