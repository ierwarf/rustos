use std::io::Write;
use std::mem::size_of;
use std::thread;
use std::time::Duration;

use rustos_user_abi::syscall::{
    DevmgrdIpcRequest, DevmgrdIpcResponse, DevmgrdNodeEntry, LinuxSyscallOffloadRequest,
    LinuxSyscallOffloadResponse, RustosDeviceIoctlBrokerArgs, DEVMGRD_IPC_ABI_VERSION,
    DEVMGRD_IPC_OP_LOOKUP, DEVMGRD_IPC_OP_READDIR, DEVMGRD_MAX_DIR_ENTRIES, DEVMGRD_NAME_CAPACITY,
    DEVMGRD_NODE_KIND_DEVICE, DEVMGRD_NODE_KIND_DIR, IPC_MAX_INLINE_BYTES, IPC_SERVICE_DEVMGRD,
    SYSCALL_OFFLOAD_ABI_VERSION, SYSCALL_OFFLOAD_OP_LINUX_IOCTL, SYSCALL_OFFLOAD_PATH_CAPACITY,
    SYS_RUSTOS_DEBUG_PRINT, SYS_RUSTOS_DEVICE_IOCTL_BROKER, SYS_RUSTOS_IPC_ENDPOINT_CREATE,
    SYS_RUSTOS_IPC_RECV, SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT, SYS_RUSTOS_IPC_REPLY,
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
