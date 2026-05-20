use std::io::Write;
use std::mem::size_of;
use std::thread;
use std::time::Duration;

use rustos_user_abi::syscall::{
    DevmgrdDeviceOpenRequest, DevmgrdDeviceOpenResponse, DevmgrdIpcRequest, DevmgrdIpcResponse,
    DevmgrdNodeEntry, IpcReplyWithHandlesArgs, LinuxSyscallOffloadRequest,
    LinuxSyscallOffloadResponse, RustosDeviceIoctlBrokerArgs, RustosDeviceOpenBrokerArgs,
    DEVMGRD_DEVICE_ACCESS_EVDEV, DEVMGRD_DEVICE_ACCESS_NATIVE, DEVMGRD_DEVICE_ID_CONSOLE,
    DEVMGRD_DEVICE_ID_DISPLAY, DEVMGRD_DEVICE_ID_INPUT, DEVMGRD_DEVICE_RIGHT_ADMIN,
    DEVMGRD_DEVICE_RIGHT_IOCTL, DEVMGRD_DEVICE_RIGHT_MAP, DEVMGRD_DEVICE_RIGHT_READ,
    DEVMGRD_DEVICE_RIGHT_TRANSFER, DEVMGRD_DEVICE_RIGHT_WRITE, DEVMGRD_IPC_ABI_VERSION,
    DEVMGRD_IPC_OP_LOOKUP, DEVMGRD_IPC_OP_OPEN, DEVMGRD_IPC_OP_READDIR, DEVMGRD_MAX_DIR_ENTRIES,
    DEVMGRD_NAME_CAPACITY, DEVMGRD_NODE_KIND_DEVICE, DEVMGRD_NODE_KIND_DIR, IPC_MAX_INLINE_BYTES,
    IPC_SERVICE_DEVMGRD, SYSCALL_OFFLOAD_ABI_VERSION, SYSCALL_OFFLOAD_OP_LINUX_IOCTL,
    SYSCALL_OFFLOAD_PATH_CAPACITY, SYS_RUSTOS_DEBUG_PRINT, SYS_RUSTOS_DEVICE_IOCTL_BROKER,
    SYS_RUSTOS_DEVICE_OPEN_BROKER, SYS_RUSTOS_IPC_ENDPOINT_CREATE, SYS_RUSTOS_IPC_RECV,
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
            size if size == size_of::<LinuxSyscallOffloadRequest>() => {
                let request = read_unaligned::<LinuxSyscallOffloadRequest>(&request);
                let mut response = LinuxSyscallOffloadResponse {
                    op: request.op,
                    ..LinuxSyscallOffloadResponse::default()
                };
                response.status = match validate_request(received as usize, &request) {
                    Ok(()) => dispatch_request(&request, &mut response),
                    Err(errno) => errno,
                };
                syscall3(
                    SYS_RUSTOS_IPC_REPLY,
                    reply_cap,
                    (&response as *const LinuxSyscallOffloadResponse) as u64,
                    size_of::<LinuxSyscallOffloadResponse>() as u64,
                )
            }
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
            _ => {
                let response = LinuxSyscallOffloadResponse {
                    status: libc::EINVAL,
                    ..LinuxSyscallOffloadResponse::default()
                };
                syscall3(
                    SYS_RUSTOS_IPC_REPLY,
                    reply_cap,
                    (&response as *const LinuxSyscallOffloadResponse) as u64,
                    size_of::<LinuxSyscallOffloadResponse>() as u64,
                )
            }
        };
        if reply < 0 {
            let _ = writeln!(std::io::stderr(), "devmgrd: reply failed errno={}", -reply);
        }
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
    let writable = request.open_flags & libc::O_ACCMODE as u64 != libc::O_RDONLY as u64;
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

fn dispatch_request(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) -> i32 {
    match request.op {
        SYSCALL_OFFLOAD_OP_LINUX_IOCTL => dispatch_ioctl(request, response),
        _ => libc::EINVAL,
    }
}

fn dispatch_ioctl(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) -> i32 {
    let args = RustosDeviceIoctlBrokerArgs {
        process_id: request.pid,
        fd: request.dirfd,
        request: request.flags,
        arg: request.arg1,
        reserved0: 0,
    };
    let result = syscall1(
        SYS_RUSTOS_DEVICE_IOCTL_BROKER,
        (&args as *const RustosDeviceIoctlBrokerArgs) as u64,
    );
    if result < 0 {
        return last_errno();
    }
    let bytes = (result as u64).to_le_bytes();
    response.payload[..bytes.len()].copy_from_slice(&bytes);
    response.payload_len = bytes.len() as u32;
    0
}

fn validate_request(received: usize, request: &LinuxSyscallOffloadRequest) -> Result<(), i32> {
    if received != size_of::<LinuxSyscallOffloadRequest>()
        || request.version != SYSCALL_OFFLOAD_ABI_VERSION
        || request.reserved0 != 0
        || request.path_len as usize > SYSCALL_OFFLOAD_PATH_CAPACITY
    {
        return Err(libc::EINVAL);
    }
    match request.op {
        SYSCALL_OFFLOAD_OP_LINUX_IOCTL => Ok(()),
        _ => Err(libc::EINVAL),
    }
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
