//! Bounded user-copy for netd datagrams. No routing, DNS or ICMP policy here.
use super::*;
use rustos_user_abi::syscall::netd_message;

fn address_from_user(ptr: u64, len: u64) -> Result<Vec<u8>, i64> {
    if ptr == 0 {
        return Ok(Vec::new());
    }
    if len < netd_message::IPV4_ADDRESS_SIZE as u64 {
        return Err(LINUX_EINVAL);
    }
    let mut bytes = alloc::vec![0; netd_message::IPV4_ADDRESS_SIZE];
    usermem::copy_from_current_user_exact(ptr, &mut bytes)
        .map_err(address_space_error_to_linux_errno)?;
    Ok(bytes)
}

pub(super) fn populate_send(request: &mut NetdIpcRequest) -> Result<(), i64> {
    let (bytes, address) = if request.op == SYSCALL_OFFLOAD_OP_LINUX_SENDMSG {
        let header = usermem::read_current_user_struct::<linux_abi::LinuxMsghdr>(request.arg1)
            .map_err(address_space_error_to_linux_errno)?;
        if header.msg_controllen != 0 {
            return Err(LINUX_EOPNOTSUPP);
        }
        let bytes = if header.msg_iovlen == 0 {
            Vec::new()
        } else {
            read_current_iovec_payload(header.msg_iov, header.msg_iovlen)?
        };
        (
            bytes,
            address_from_user(header.msg_name, u64::from(header.msg_namelen))?,
        )
    } else {
        let len = usize::try_from(request.arg2).map_err(|_| LINUX_EMSGSIZE)?;
        if len
            > NETD_IPC_PAYLOAD_CAPACITY
                - netd_message::HEADER_SIZE
                - netd_message::IPV4_ADDRESS_SIZE
        {
            return Err(LINUX_EMSGSIZE);
        }
        let mut bytes = alloc::vec![0; len];
        if len != 0 {
            usermem::copy_from_current_user_exact(request.arg1, &mut bytes)
                .map_err(address_space_error_to_linux_errno)?;
        }
        (bytes, address_from_user(request.arg4, request.arg5)?)
    };
    let len = netd_message::encode(&mut request.payload, &bytes, &[], 0, &address)
        .ok_or(LINUX_EMSGSIZE)?;
    request.payload_len = len as u32;
    Ok(())
}

pub(super) fn consume_receive(
    request: &NetdIpcRequest,
    response: &NetdIpcResponse,
) -> Result<(), i64> {
    let bytes = response
        .payload
        .get(..response.payload_len as usize)
        .ok_or(LINUX_EINVAL)?;
    let message = netd_message::decode(bytes).ok_or(LINUX_EINVAL)?;
    if message.address.len() != netd_message::IPV4_ADDRESS_SIZE {
        return Err(LINUX_EINVAL);
    }
    let is_recvmsg = request.op == SYSCALL_OFFLOAD_OP_LINUX_RECVMSG;
    let flags = if is_recvmsg {
        request.arg2
    } else {
        request.arg3
    };
    let error_queue = flags & linux_abi::MSG_ERRQUEUE != 0;
    if (error_queue && !valid_error_control(message.control))
        || (!error_queue && !message.control.is_empty())
    {
        return Err(LINUX_EINVAL);
    }
    let requested = if is_recvmsg {
        u64::from(request.payload_len)
    } else {
        request.arg2
    };
    if !received_length_valid(
        message.data.len(),
        response.value,
        requested,
        flags,
        message.flags,
    ) {
        return Err(LINUX_EINVAL);
    }
    if is_recvmsg {
        let mut header = usermem::read_current_user_struct::<linux_abi::LinuxMsghdr>(request.arg1)
            .map_err(address_space_error_to_linux_errno)?;
        let iovecs = if header.msg_iovlen == 0 {
            Vec::new()
        } else {
            read_current_iovecs_payload(header.msg_iov, header.msg_iovlen)?
        };
        write_current_iovec_payload(&iovecs, message.data)?;
        if header.msg_name != 0 {
            let len = (header.msg_namelen as usize).min(message.address.len());
            if len != 0 {
                usermem::write_current_user_bytes(header.msg_name, &message.address[..len])
                    .map_err(address_space_error_to_linux_errno)?;
            }
            header.msg_namelen = message.address.len() as u32;
        } else {
            header.msg_namelen = 0;
        }
        header.msg_flags = message.flags;
        let mut written = 0;
        if !message.control.is_empty() {
            if header.msg_control != 0 && header.msg_controllen >= 16 {
                written = usize::try_from(header.msg_controllen)
                    .unwrap_or(usize::MAX)
                    .min(message.control.len());
                let mut control = message.control[..written].to_vec();
                control[..8].copy_from_slice(&(written as u64).to_ne_bytes());
                usermem::write_current_user_bytes(header.msg_control, &control)
                    .map_err(address_space_error_to_linux_errno)?;
            }
            if written < message.control.len() {
                header.msg_flags |= linux_abi::MSG_CTRUNC as u32;
            }
        }
        header.msg_controllen = written as u64;
        usermem::write_current_user_struct(request.arg1, &header)
            .map_err(address_space_error_to_linux_errno)
    } else {
        if !message.data.is_empty() {
            usermem::write_current_user_bytes(request.arg1, message.data)
                .map_err(address_space_error_to_linux_errno)?;
        }
        if request.arg4 != 0 {
            write_current_sockaddr_payload(request.arg4, request.arg5, message.address)?;
        }
        Ok(())
    }
}

fn valid_error_control(bytes: &[u8]) -> bool {
    bytes.len() == 48
        && u64::from_ne_bytes(bytes[..8].try_into().unwrap()) == 48
        && u32::from_ne_bytes(bytes[8..12].try_into().unwrap()) == linux_abi::SOL_IP as u32
        && u32::from_ne_bytes(bytes[12..16].try_into().unwrap()) == linux_abi::IP_RECVERR as u32
        && (1..=4095).contains(&u32::from_ne_bytes(bytes[16..20].try_into().unwrap()))
        && bytes[20] == 2
        && matches!(bytes[21], 3 | 11)
        && bytes[23] == 0
        && u16::from_ne_bytes(bytes[32..34].try_into().unwrap()) == linux_abi::AF_INET as u16
}

fn received_length_valid(
    copied: usize,
    value: u64,
    capacity: u64,
    input_flags: u64,
    output_flags: u32,
) -> bool {
    let truncated = linux_abi::MSG_TRUNC as u32;
    let error_queue = linux_abi::MSG_ERRQUEUE as u32;
    output_flags & !(truncated | error_queue) == 0
        && (output_flags & error_queue != 0) == (input_flags & u64::from(error_queue) != 0)
        && copied as u64 <= capacity
        && (value == copied as u64
            || (input_flags & u64::from(truncated) != 0
                && output_flags & truncated != 0
                && value > copied as u64
                && value <= 65535))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_queue_control_is_bounded_typed_and_never_scm_rights() {
        let mut bytes = [0; 48];
        bytes[..8].copy_from_slice(&48_u64.to_ne_bytes());
        bytes[12..16].copy_from_slice(&(linux_abi::IP_RECVERR as u32).to_ne_bytes());
        bytes[16..20].copy_from_slice(&111_u32.to_ne_bytes());
        bytes[20] = 2;
        bytes[21] = 3;
        bytes[32..34].copy_from_slice(&(linux_abi::AF_INET as u16).to_ne_bytes());
        assert!(valid_error_control(&bytes));
        bytes[12..16].copy_from_slice(&(linux_abi::SCM_RIGHTS as u32).to_ne_bytes());
        assert!(!valid_error_control(&bytes));
        assert!(!valid_error_control(&bytes[..47]));
        let err = linux_abi::MSG_ERRQUEUE as u32;
        assert!(received_length_valid(0, 0, 0, u64::from(err), err));
        assert!(!received_length_valid(0, 0, 0, 0, err));
    }

    #[test]
    fn only_explicit_truncation_can_report_more_than_copied_bytes() {
        let trunc = linux_abi::MSG_TRUNC as u32;
        assert!(received_length_valid(2, 5, 2, u64::from(trunc), trunc));
        assert!(received_length_valid(0, 0, 0, 0, 0));
        assert!(!received_length_valid(2, 5, 2, 0, trunc));
        assert!(!received_length_valid(2, 5, 2, u64::from(trunc), 0));
        assert!(!received_length_valid(3, 3, 2, 0, 0));
        assert!(!received_length_valid(2, 1, 2, 0, 0));
        assert!(!received_length_valid(2, 2, 2, 0, 1 << 31));
    }
}
