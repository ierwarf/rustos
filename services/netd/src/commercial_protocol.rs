//! Commercial protocol discovery and capability-description responses.
//!
//! This module describes `netd`'s supported policy surface. Socket state and
//! request execution remain owned by the service state machine in `main.rs`.

use super::*;

pub(super) fn validate_commercial_request(
    request: &CommercialMaxProtocolRequest,
) -> Result<(), i32> {
    if !request.has_valid_envelope() || request.header.protocol != COMMERCIAL_MAX_PROTOCOL_NETD {
        return Err(libc::EINVAL);
    }
    if request.arg0 != 0
        || request.arg1 != 0
        || request.arg2 != 0
        || request.arg3 != 0
        || request.path_len != 0
        || request.payload_len != 0
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

pub(super) fn dispatch_commercial_request(
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
                    ("dup", SYSCALL_OFFLOAD_OP_LINUX_DUP),
                    ("close", SYSCALL_OFFLOAD_OP_LINUX_CLOSE),
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
        COMMERCIAL_MAX_NETD_OP_SOCKET_NAMESPACE
        | SYSCALL_OFFLOAD_OP_LINUX_SOCKET
        | SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR
        | SYSCALL_OFFLOAD_OP_LINUX_DUP
        | SYSCALL_OFFLOAD_OP_LINUX_CLOSE
        | SYSCALL_OFFLOAD_OP_LINUX_SHUTDOWN => 1 << 0,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn request(op: u16) -> CommercialMaxProtocolRequest {
        let mut request = CommercialMaxProtocolRequest::default();
        request.header.protocol = COMMERCIAL_MAX_PROTOCOL_NETD;
        request.header.op = op;
        request
    }

    #[test]
    fn commercial_netd_requests_are_closed_and_canonical() {
        for op in [
            COMMERCIAL_MAX_NETD_OP_SOCKET_NAMESPACE,
            COMMERCIAL_MAX_NETD_OP_SOCKET_OPTIONS,
            COMMERCIAL_MAX_NETD_OP_ADDRESS_BIND,
            COMMERCIAL_MAX_NETD_OP_ROUTE_POLICY,
            COMMERCIAL_MAX_NETD_OP_PACKET_LEASE,
            COMMERCIAL_MAX_NETD_OP_FD_TRANSFER,
        ] {
            assert_eq!(validate_commercial_request(&request(op)), Ok(()));
        }

        let unknown = request(u16::MAX);
        assert_eq!(validate_commercial_request(&unknown), Err(libc::EINVAL));
        let mut response = CommercialMaxProtocolResponse::default();
        assert_eq!(
            dispatch_commercial_request(&unknown, &mut response),
            libc::EINVAL
        );

        let mut malformed = request(COMMERCIAL_MAX_NETD_OP_SOCKET_NAMESPACE);
        malformed.arg0 = 1;
        assert_eq!(validate_commercial_request(&malformed), Err(libc::EINVAL));
        malformed.arg0 = 0;
        malformed.path_len = 1;
        assert_eq!(validate_commercial_request(&malformed), Err(libc::EINVAL));
        malformed.path_len = 0;
        malformed.payload_len = 1;
        assert_eq!(validate_commercial_request(&malformed), Err(libc::EINVAL));
    }

    #[test]
    fn commercial_netd_descriptors_keep_exact_least_authority() {
        let namespace = request(COMMERCIAL_MAX_NETD_OP_SOCKET_NAMESPACE);
        let mut response = CommercialMaxProtocolResponse::default();
        assert_eq!(dispatch_commercial_request(&namespace, &mut response), 0);
        assert_eq!(response.descriptor_count, 5);
        assert_eq!(response.value0, 5);
        let expected_ops = [
            SYSCALL_OFFLOAD_OP_LINUX_SOCKET,
            SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR,
            SYSCALL_OFFLOAD_OP_LINUX_DUP,
            SYSCALL_OFFLOAD_OP_LINUX_CLOSE,
            SYSCALL_OFFLOAD_OP_LINUX_SHUTDOWN,
        ];
        for (descriptor, expected_op) in response.descriptors[..5].iter().zip(expected_ops) {
            assert_eq!(descriptor.protocol, COMMERCIAL_MAX_PROTOCOL_NETD);
            assert_eq!(descriptor.service_id, IPC_SERVICE_NETD);
            assert_eq!(descriptor.op, expected_op);
            assert_eq!(descriptor.capability_mask, 1 << 0);
        }

        let packet = request(COMMERCIAL_MAX_NETD_OP_PACKET_LEASE);
        let mut response = CommercialMaxProtocolResponse::default();
        assert_eq!(dispatch_commercial_request(&packet, &mut response), 0);
        assert_eq!(response.capability.capability_mask, 1 << 4);
        assert_eq!(response.capability.rights_mask, 1 << 4);
        assert_eq!(response.capability.service_id, IPC_SERVICE_NETD);

        let transfer = request(COMMERCIAL_MAX_NETD_OP_FD_TRANSFER);
        let mut response = CommercialMaxProtocolResponse::default();
        assert_eq!(dispatch_commercial_request(&transfer, &mut response), 0);
        assert_eq!(response.descriptor_count, 1);
        assert_eq!(response.descriptors[0].op, SYSCALL_OFFLOAD_OP_LINUX_ACCEPT);
        assert_eq!(response.capability.capability_mask, 1 << 5);
        assert_eq!(response.capability.rights_mask, 1 << 5);
    }
}
