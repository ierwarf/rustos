use std::io::Write;
use std::mem::size_of;
use std::thread;
use std::time::Duration;

use rustos_user_abi::syscall::{
    CommercialMaxCapabilityLeaseWire, CommercialMaxProtocolDescriptorWire,
    CommercialMaxProtocolRequest, CommercialMaxProtocolResponse, NetdIpcRequest, NetdIpcResponse,
    RustosNetBrokerArgs, COMMERCIAL_MAX_NETD_OP_ADDRESS_BIND, COMMERCIAL_MAX_NETD_OP_FD_TRANSFER,
    COMMERCIAL_MAX_NETD_OP_PACKET_LEASE, COMMERCIAL_MAX_NETD_OP_ROUTE_POLICY,
    COMMERCIAL_MAX_NETD_OP_SOCKET_NAMESPACE, COMMERCIAL_MAX_NETD_OP_SOCKET_OPTIONS,
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION, COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS,
    COMMERCIAL_MAX_PROTOCOL_NETD, IPC_SERVICE_NETD, NETD_IPC_ABI_VERSION,
    SYSCALL_OFFLOAD_OP_LINUX_ACCEPT, SYSCALL_OFFLOAD_OP_LINUX_BIND,
    SYSCALL_OFFLOAD_OP_LINUX_CONNECT, SYSCALL_OFFLOAD_OP_LINUX_GETPEERNAME,
    SYSCALL_OFFLOAD_OP_LINUX_GETSOCKNAME, SYSCALL_OFFLOAD_OP_LINUX_GETSOCKOPT,
    SYSCALL_OFFLOAD_OP_LINUX_LISTEN, SYSCALL_OFFLOAD_OP_LINUX_RECVFROM,
    SYSCALL_OFFLOAD_OP_LINUX_RECVMSG, SYSCALL_OFFLOAD_OP_LINUX_SENDMSG,
    SYSCALL_OFFLOAD_OP_LINUX_SENDTO, SYSCALL_OFFLOAD_OP_LINUX_SETSOCKOPT,
    SYSCALL_OFFLOAD_OP_LINUX_SHUTDOWN, SYSCALL_OFFLOAD_OP_LINUX_SOCKET,
    SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR, SYS_RUSTOS_DEBUG_PRINT, SYS_RUSTOS_IPC_ENDPOINT_CREATE,
    SYS_RUSTOS_IPC_RECV, SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT, SYS_RUSTOS_IPC_REPLY,
    SYS_RUSTOS_NET_BROKER,
};

const RECV_BACKOFF: Duration = Duration::from_millis(1);

fn main() {
    let endpoint = syscall0(SYS_RUSTOS_IPC_ENDPOINT_CREATE);
    if endpoint < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "netd: endpoint create failed errno={}",
            -endpoint
        );
        return;
    }

    let register = syscall2(
        SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT,
        IPC_SERVICE_NETD,
        endpoint as u64,
    );
    if register < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "netd: endpoint register failed errno={}",
            -register
        );
        return;
    }

    debug_line("netd: network policy endpoint registered");
    serve(endpoint as u64);
}

fn serve(endpoint: u64) {
    loop {
        let mut request = CommercialMaxProtocolRequest::default();
        let mut reply_cap = 0_u64;
        let received = syscall4(
            SYS_RUSTOS_IPC_RECV,
            endpoint,
            (&mut request as *mut CommercialMaxProtocolRequest) as u64,
            size_of::<CommercialMaxProtocolRequest>() as u64,
            (&mut reply_cap as *mut u64) as u64,
        );
        if received < 0 {
            thread::sleep(RECV_BACKOFF);
            continue;
        }

        if received as usize == size_of::<CommercialMaxProtocolRequest>() {
            let reply = reply_commercial_request(reply_cap, &request);
            if reply < 0 {
                let _ = writeln!(std::io::stderr(), "netd: reply failed errno={}", -reply);
            }
            continue;
        }

        let request = unsafe {
            &*((&request as *const CommercialMaxProtocolRequest).cast::<NetdIpcRequest>())
        };
        let mut response = NetdIpcResponse {
            version: NETD_IPC_ABI_VERSION,
            op: request.op,
            ..NetdIpcResponse::default()
        };
        response.status = match validate_request(received as usize, &request) {
            Ok(()) => dispatch_request(&request, &mut response),
            Err(errno) => errno,
        };
        let reply = syscall3(
            SYS_RUSTOS_IPC_REPLY,
            reply_cap,
            (&response as *const NetdIpcResponse) as u64,
            size_of::<NetdIpcResponse>() as u64,
        );
        if reply < 0 {
            let _ = writeln!(std::io::stderr(), "netd: reply failed errno={}", -reply);
        }
    }
}

fn reply_commercial_request(reply_cap: u64, request: &CommercialMaxProtocolRequest) -> i64 {
    let mut response = CommercialMaxProtocolResponse {
        header: request.header,
        ..CommercialMaxProtocolResponse::default()
    };
    response.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    response.status = match validate_commercial_request(request) {
        Ok(()) => dispatch_commercial_request(request, &mut response),
        Err(errno) => errno,
    };
    syscall3(
        SYS_RUSTOS_IPC_REPLY,
        reply_cap,
        (&response as *const CommercialMaxProtocolResponse) as u64,
        size_of::<CommercialMaxProtocolResponse>() as u64,
    )
}

fn dispatch_request(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    let args = RustosNetBrokerArgs {
        process_id: request.pid,
        op: request.op,
        reserved0: 0,
        reserved1: 0,
        arg0: request.arg0,
        arg1: request.arg1,
        arg2: request.arg2,
        arg3: request.arg3,
        arg4: request.arg4,
        arg5: request.arg5,
    };
    let result = syscall1(
        SYS_RUSTOS_NET_BROKER,
        (&args as *const RustosNetBrokerArgs) as u64,
    );
    if result < 0 {
        return last_errno();
    }
    response.value = result as u64;
    0
}

fn validate_request(received: usize, request: &NetdIpcRequest) -> Result<(), i32> {
    if received != size_of::<NetdIpcRequest>()
        || request.version != NETD_IPC_ABI_VERSION
        || request.flags != 0
        || request.reserved0 != 0
        || request.pid == 0
        || request.tid == 0
    {
        return Err(libc::EINVAL);
    }
    match request.op {
        SYSCALL_OFFLOAD_OP_LINUX_SOCKET
        | SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR
        | SYSCALL_OFFLOAD_OP_LINUX_BIND
        | SYSCALL_OFFLOAD_OP_LINUX_LISTEN
        | SYSCALL_OFFLOAD_OP_LINUX_ACCEPT
        | SYSCALL_OFFLOAD_OP_LINUX_CONNECT
        | SYSCALL_OFFLOAD_OP_LINUX_SENDTO
        | SYSCALL_OFFLOAD_OP_LINUX_GETSOCKNAME
        | SYSCALL_OFFLOAD_OP_LINUX_GETPEERNAME
        | SYSCALL_OFFLOAD_OP_LINUX_SETSOCKOPT
        | SYSCALL_OFFLOAD_OP_LINUX_GETSOCKOPT
        | SYSCALL_OFFLOAD_OP_LINUX_SHUTDOWN
        | SYSCALL_OFFLOAD_OP_LINUX_SENDMSG
        | SYSCALL_OFFLOAD_OP_LINUX_RECVMSG
        | SYSCALL_OFFLOAD_OP_LINUX_RECVFROM => Ok(()),
        _ => Err(libc::EINVAL),
    }
}

fn validate_commercial_request(request: &CommercialMaxProtocolRequest) -> Result<(), i32> {
    if request.header.version != COMMERCIAL_MAX_PROTOCOL_ABI_VERSION
        || request.header.protocol != COMMERCIAL_MAX_PROTOCOL_NETD
        || request.path_len as usize > request.path.len()
        || request.payload_len as usize > request.payload.len()
    {
        return Err(libc::EINVAL);
    }
    match request.header.op {
        COMMERCIAL_MAX_NETD_OP_SOCKET_NAMESPACE
        | COMMERCIAL_MAX_NETD_OP_SOCKET_OPTIONS
        | COMMERCIAL_MAX_NETD_OP_ADDRESS_BIND
        | COMMERCIAL_MAX_NETD_OP_ROUTE_POLICY
        | COMMERCIAL_MAX_NETD_OP_PACKET_LEASE
        | COMMERCIAL_MAX_NETD_OP_FD_TRANSFER => Ok(()),
        _ => Err(libc::EINVAL),
    }
}

fn dispatch_commercial_request(
    request: &CommercialMaxProtocolRequest,
    response: &mut CommercialMaxProtocolResponse,
) -> i32 {
    match request.header.op {
        COMMERCIAL_MAX_NETD_OP_SOCKET_NAMESPACE => {
            fill_net_descriptors(
                response,
                &[
                    ("socket", SYSCALL_OFFLOAD_OP_LINUX_SOCKET),
                    ("socketpair", SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR),
                    ("shutdown", SYSCALL_OFFLOAD_OP_LINUX_SHUTDOWN),
                ],
            );
            0
        }
        COMMERCIAL_MAX_NETD_OP_SOCKET_OPTIONS => {
            fill_net_descriptors(
                response,
                &[
                    ("setsockopt", SYSCALL_OFFLOAD_OP_LINUX_SETSOCKOPT),
                    ("getsockopt", SYSCALL_OFFLOAD_OP_LINUX_GETSOCKOPT),
                ],
            );
            0
        }
        COMMERCIAL_MAX_NETD_OP_ADDRESS_BIND => {
            fill_net_descriptors(
                response,
                &[
                    ("bind", SYSCALL_OFFLOAD_OP_LINUX_BIND),
                    ("listen", SYSCALL_OFFLOAD_OP_LINUX_LISTEN),
                    ("getsockname", SYSCALL_OFFLOAD_OP_LINUX_GETSOCKNAME),
                    ("getpeername", SYSCALL_OFFLOAD_OP_LINUX_GETPEERNAME),
                ],
            );
            0
        }
        COMMERCIAL_MAX_NETD_OP_ROUTE_POLICY => {
            fill_net_descriptors(response, &[("connect", SYSCALL_OFFLOAD_OP_LINUX_CONNECT)]);
            0
        }
        COMMERCIAL_MAX_NETD_OP_PACKET_LEASE => {
            fill_net_descriptors(
                response,
                &[
                    ("sendto", SYSCALL_OFFLOAD_OP_LINUX_SENDTO),
                    ("sendmsg", SYSCALL_OFFLOAD_OP_LINUX_SENDMSG),
                    ("recvfrom", SYSCALL_OFFLOAD_OP_LINUX_RECVFROM),
                    ("recvmsg", SYSCALL_OFFLOAD_OP_LINUX_RECVMSG),
                ],
            );
            response.capability = net_capability("packet", request.header.op);
            0
        }
        COMMERCIAL_MAX_NETD_OP_FD_TRANSFER => {
            fill_net_descriptors(response, &[("accept", SYSCALL_OFFLOAD_OP_LINUX_ACCEPT)]);
            response.capability = net_capability("fd-transfer", request.header.op);
            0
        }
        _ => libc::EINVAL,
    }
}

fn fill_net_descriptors(response: &mut CommercialMaxProtocolResponse, entries: &[(&str, u16)]) {
    let count = entries.len().min(COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS);
    response.descriptor_count = count as u16;
    response.value0 = entries.len() as u64;
    for (index, (name, op)) in entries.iter().take(count).enumerate() {
        response.descriptors[index] = net_descriptor(name, *op);
    }
}

fn net_descriptor(name: &str, offload_op: u16) -> CommercialMaxProtocolDescriptorWire {
    let mut descriptor = CommercialMaxProtocolDescriptorWire {
        protocol: COMMERCIAL_MAX_PROTOCOL_NETD,
        op: offload_op,
        flags: 0,
        service_id: IPC_SERVICE_NETD,
        capability_mask: net_capability_mask(offload_op),
        value0: offload_op as u64,
        value1: 0,
        ..CommercialMaxProtocolDescriptorWire::default()
    };
    copy_label(name, &mut descriptor.name, &mut descriptor.name_len);
    descriptor
}

fn net_capability(label: &str, op: u16) -> CommercialMaxCapabilityLeaseWire {
    let mut capability = CommercialMaxCapabilityLeaseWire {
        lease_id: ((COMMERCIAL_MAX_PROTOCOL_NETD as u64) << 32) | u64::from(op),
        service_id: IPC_SERVICE_NETD,
        capability_mask: net_capability_mask(op),
        rights_mask: net_capability_mask(op),
        ..CommercialMaxCapabilityLeaseWire::default()
    };
    copy_label(label, &mut capability.label, &mut capability.label_len);
    capability
}

fn net_capability_mask(op: u16) -> u64 {
    match op {
        COMMERCIAL_MAX_NETD_OP_SOCKET_NAMESPACE | SYSCALL_OFFLOAD_OP_LINUX_SOCKET => 1 << 0,
        COMMERCIAL_MAX_NETD_OP_SOCKET_OPTIONS
        | SYSCALL_OFFLOAD_OP_LINUX_SETSOCKOPT
        | SYSCALL_OFFLOAD_OP_LINUX_GETSOCKOPT => 1 << 1,
        COMMERCIAL_MAX_NETD_OP_ADDRESS_BIND
        | SYSCALL_OFFLOAD_OP_LINUX_BIND
        | SYSCALL_OFFLOAD_OP_LINUX_LISTEN => 1 << 2,
        COMMERCIAL_MAX_NETD_OP_ROUTE_POLICY | SYSCALL_OFFLOAD_OP_LINUX_CONNECT => 1 << 3,
        COMMERCIAL_MAX_NETD_OP_PACKET_LEASE
        | SYSCALL_OFFLOAD_OP_LINUX_SENDTO
        | SYSCALL_OFFLOAD_OP_LINUX_SENDMSG
        | SYSCALL_OFFLOAD_OP_LINUX_RECVFROM
        | SYSCALL_OFFLOAD_OP_LINUX_RECVMSG => 1 << 4,
        COMMERCIAL_MAX_NETD_OP_FD_TRANSFER | SYSCALL_OFFLOAD_OP_LINUX_ACCEPT => 1 << 5,
        _ => 0,
    }
}

fn copy_label(label: &str, target: &mut [u8], len: &mut u16) {
    let bytes = label.as_bytes();
    let count = bytes.len().min(target.len());
    target[..count].copy_from_slice(&bytes[..count]);
    *len = count as u16;
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
