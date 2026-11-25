//! Sender-authenticated descriptor receive output and admission.
//!
//! - **Owner:** the common receive state machine owns dequeue/wait and reply
//!   custody; this module owns the retained-MM output bundle.
//! - **Boundary:** sender identity comes from the dequeued kernel record, never
//!   request bytes. Exact fd reservations stay invisible until every output
//!   copies out under the same retained address-space bind.
//! - **Failure:** descriptor claim/copyout/commit failures use the shared
//!   transfer rollback path; they never publish a partial fd set.
use super::*;

#[derive(Clone, Copy)]
pub(super) struct HandleOutputs {
    pub(super) fds_ptr: u64,
    pub(super) count_ptr: u64,
    pub(super) capacity: usize,
}

impl HandleOutputs {
    pub(super) fn validate_shape(&self) -> LinuxResult<usize> {
        if self.capacity > rustos_user_abi::syscall::IPC_MAX_TRANSFER_HANDLES || self.count_ptr == 0 {
            return Err(LINUX_EINVAL);
        }
        if self.capacity != 0 && self.fds_ptr == 0 {
            return Err(LINUX_EINVAL);
        }
        self.capacity.checked_mul(size_of::<i32>()).ok_or(LINUX_EINVAL)
    }

    pub(super) fn validate(&self, retained: &multitask::RetainedCurrentUserAddressSpace) -> Result<(), i64> {
        let bytes = self.validate_shape()?;
        if bytes == 0 {
            usermem::validate_retained_user_write_buffers(retained, &[(self.count_ptr, size_of::<u16>())])
        } else {
            usermem::validate_retained_user_write_buffers(retained, &[
                (self.fds_ptr, bytes), (self.count_ptr, size_of::<u16>()),
            ])
        }.map_err(address_space_error_to_linux_errno)
    }
}

pub(super) struct ReceiveOutputs<'a> {
    pub(super) retained_mm: &'a multitask::RetainedCurrentUserAddressSpace,
    pub(super) request_ptr: u64,
    pub(super) reply_cap_ptr: u64,
    pub(super) sender_pid_ptr: u64,
    pub(super) sender_tid_ptr: u64,
    pub(super) include_sender: bool,
    pub(super) include_handles: bool,
    pub(super) handles: HandleOutputs,
}

impl<'a> ReceiveOutputs<'a> {
    pub(super) const fn new(
        retained_mm: &'a multitask::RetainedCurrentUserAddressSpace,
        request_ptr: u64,
        reply_cap_ptr: u64,
        sender: Option<(u64, u64)>,
        handles: Option<HandleOutputs>,
    ) -> Self {
        let (sender_pid_ptr, sender_tid_ptr, include_sender) = match sender {
            Some((pid, tid)) => (pid, tid, true),
            None => (0, 0, false),
        };
        let (handles, include_handles) = match handles {
            Some(handles) => (handles, true),
            None => (HandleOutputs { fds_ptr: 0, count_ptr: 0, capacity: 0 }, false),
        };
        Self {
            retained_mm,
            request_ptr,
            reply_cap_ptr,
            sender_pid_ptr,
            sender_tid_ptr,
            include_sender,
            include_handles,
            handles,
        }
    }

    pub(super) fn write_fast<const N: usize>(
        &self,
        received: kernel_ipc_runtime::api::FastEndpointReceived<N>,
    ) -> Result<usize, i64> {
        self.write(received.reply, received.bytes(), received.caller_process_id, received.caller_task_id, &[])
    }

    pub(super) fn write_queued(
        &self,
        reply: KernelReplyHandle,
        request: &[u8],
        handles: &[KernelTransferredHandle],
        caller: u64,
    ) -> Result<usize, i64> {
        let (pid, tid) = multitask::user_log_ids_for_task(caller).unwrap_or((0, 0));
        self.write(reply, request, pid, tid, handles)
    }

    fn write(
        &self,
        reply: KernelReplyHandle,
        request: &[u8],
        pid: u64,
        tid: u64,
        descriptors: &[KernelTransferredHandle],
    ) -> Result<usize, i64> {
        if descriptors.len() > self.handles.capacity {
            return Err(LINUX_EINVAL);
        }
        let copyout = |fds: &[i32], count: u16| {
            let (reply, pid, tid, count) = (
                reply.raw().to_ne_bytes(), pid.to_ne_bytes(), tid.to_ne_bytes(), count.to_ne_bytes(),
            );
            let mut writes = [
                (self.request_ptr, request),
                (self.reply_cap_ptr, &reply),
                (0, &[]),
                (0, &[]),
                (0, &[]),
                (0, &[]),
            ];
            let mut write_count = 2;
            if self.include_sender {
                writes[write_count] = (self.sender_pid_ptr, &pid);
                writes[write_count + 1] = (self.sender_tid_ptr, &tid);
                write_count += 2;
            }
            if self.include_handles {
                writes[write_count] = (self.handles.fds_ptr, received_fd_bytes(fds));
                writes[write_count + 1] = (self.handles.count_ptr, &count);
                write_count += 2;
            }
            // Request, reply, sender and descriptor outputs precede the shared
            // helper's one exact fd commit. No later sender copy can fail after
            // a descriptor has become visible to another receiver thread.
            usermem::write_retained_user_bytes_batch(self.retained_mm, &writes[..write_count])
                .map_err(address_space_error_to_linux_errno)
        };
        if descriptors.is_empty() {
            // Byte-only/fast messages need no fd reservation or transfer claim.
            copyout(&[], 0)?;
        } else {
            install_received_handles_with_copyout(descriptors, copyout)?;
        }
        Ok(request.len())
    }
}

pub(super) fn syscall_linux_rustos_ipc_recv_with_handles_and_sender(args_ptr: u64) -> u64 {
    linux_result(receive(args_ptr).map(|len| len as u64))
}

fn receive(args_ptr: u64) -> Result<usize, i64> {
    let retained = usermem::current_user_address_space().map_err(address_space_error_to_linux_errno)?;
    let args: rustos_user_abi::syscall::IpcRecvWithHandlesAndSenderArgs =
        read_retained_user_struct_as(&retained, args_ptr)?;
    if !rustos_user_abi::syscall::ipc_handle_receive_shape_valid(&args) {
        return Err(LINUX_EINVAL);
    }
    let (endpoint, task, _, capacity, retained) = prepare_recv_with_sender_retained(
        args.endpoint, args.request_ptr, args.request_capacity, args.reply_cap_ptr,
        args.sender_pid_ptr, args.sender_tid_ptr, retained,
    )?;
    let handles = HandleOutputs {
        fds_ptr: args.recv_fds_ptr, count_ptr: args.recv_fd_count_ptr, capacity: usize::from(args.recv_fd_capacity),
    };
    handles.validate(&retained)?;
    let outputs = ReceiveOutputs::new(
        &retained,
        args.request_ptr,
        args.reply_cap_ptr,
        Some((args.sender_pid_ptr, args.sender_tid_ptr)),
        Some(handles),
    );
    recv_with_sender_blocking_prepared_inner::<{ kernel_ipc_runtime::api::IPC_FAST_INLINE_BYTES }>(
        endpoint, task, capacity, None, None, ReceiveDestination::UserWithHandles(outputs),
    ).map(|(len, _)| len).map_err(|(errno, _)| errno)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authenticated_handle_receive_is_classified_and_keeps_deferred_reply_custody() {
        use rustos_user_abi::syscall::*;
        assert!(is_linux_rustos_ipc_syscall(SYS_RUSTOS_IPC_RECV_WITH_HANDLES_AND_SENDER));
        assert!(is_linux_rustos_ipc_syscall(SYS_RUSTOS_IPC_REPLY_RECV_WITH_HANDLES_AND_SENDER));
        assert!(consumes_deferred_fast_reply(SYS_RUSTOS_IPC_RECV_WITH_HANDLES_AND_SENDER));
        assert!(!consumes_deferred_fast_reply(SYS_RUSTOS_IPC_REPLY_RECV_WITH_HANDLES_AND_SENDER));
    }

    #[test]
    fn authenticated_handle_receive_binds_all_outputs_before_descriptor_publication() {
        // Source witness, not production user-copy fault injection.
        let output = include_str!("handle_receive.rs").split("#[cfg(test)]").next().unwrap();
        assert!(output.contains("received.caller_process_id, received.caller_task_id"));
        assert!(output.contains("user_log_ids_for_task(caller)"));
        for field in ["(self.sender_pid_ptr, &pid)", "(self.sender_tid_ptr, &tid)",
            "(self.handles.fds_ptr, received_fd_bytes(fds))", "(self.handles.count_ptr, &count)"] {
            assert!(output.contains(field), "missing output in the retained copyout bundle: {field}");
        }
        let admit = output.split_once("fn receive(args_ptr:").unwrap().1;
        assert!(admit.find("handles.validate(&retained)?").unwrap()
            < admit.find("recv_with_sender_blocking_prepared_inner").unwrap());
        let common = include_str!("../ipc_ops.rs");
        let install = common.split_once("fn install_received_handles_with_copyout(").unwrap().1;
        assert!(install.find("copyout(&fds, count)").unwrap()
            < install.find(".commit_reserved_transfers(").unwrap());
        let receive = common.split_once("fn recv_with_sender_blocking_prepared_inner<const N: usize>(").unwrap().1;
        assert!(receive.contains("destination.handle_capacity()"));
        assert!(receive.contains(".write_queued_handles(reply, &request, &handles, caller_task_id)"));
        let fused = include_str!("../ipc_reply_recv.rs").split("#[cfg(test)]").next().unwrap();
        assert!(fused.find("user.validate_write(handles.count_ptr").unwrap()
            < fused.find("complete_fast_reply_for_task(").unwrap());
        assert!(fused.find("user.validate_write(handles.fds_ptr").unwrap()
            < fused.find("complete_endpoint_reply_for_process_with_custody(").unwrap());
        assert!(fused.contains("ReceiveDestination::UserWithHandles"));
        assert!(fused.contains("fast_reply::DeferredFastReply::new(completed_reply, completion)"));
    }

    #[test]
    fn handle_outputs_shape_rejects_excess_capacity_and_null_pointers_pre_dequeue() {
        assert_eq!(
            HandleOutputs { fds_ptr: 0, count_ptr: 0x1000, capacity: 0 }.validate_shape(),
            Ok(0)
        );
        assert_eq!(
            HandleOutputs {
                fds_ptr: 0x2000,
                count_ptr: 0x1000,
                capacity: rustos_user_abi::syscall::IPC_MAX_TRANSFER_HANDLES,
            }
            .validate_shape(),
            Ok(rustos_user_abi::syscall::IPC_MAX_TRANSFER_HANDLES * size_of::<i32>())
        );
        for invalid in [
            HandleOutputs { fds_ptr: 0, count_ptr: 0, capacity: 0 },
            HandleOutputs { fds_ptr: 0, count_ptr: 0x1000, capacity: 1 },
            HandleOutputs {
                fds_ptr: 0x2000,
                count_ptr: 0x1000,
                capacity: rustos_user_abi::syscall::IPC_MAX_TRANSFER_HANDLES + 1,
            },
        ] {
            assert_eq!(invalid.validate_shape(), Err(LINUX_EINVAL));
        }
    }
}
