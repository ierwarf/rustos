pub use crate::ipc::{
    EndpointWakeSet, IpcError, KernelEndpointHandle, KernelReplyHandle, KernelSharedRegionHandle,
    KernelTransferredHandle,
};

pub mod endpoint {
    pub use crate::ipc::{
        EndpointWakeSet, IpcError, KernelEndpointHandle, KernelReplyHandle, KernelTransferredHandle,
    };

    pub fn create() -> Result<KernelEndpointHandle, IpcError> {
        crate::ipc::create_endpoint()
    }

    pub fn create_for_task(owner_task_id: u64) -> Result<KernelEndpointHandle, IpcError> {
        crate::ipc::create_endpoint_for_task(Some(owner_task_id))
    }

    pub fn enqueue_call(
        endpoint: KernelEndpointHandle,
        caller_task_id: u64,
        request: &[u8],
    ) -> Result<(KernelReplyHandle, Option<u64>), IpcError> {
        crate::ipc::enqueue_endpoint_call(endpoint, caller_task_id, request)
    }

    pub fn enqueue_call_with_handles(
        endpoint: KernelEndpointHandle,
        caller_task_id: u64,
        request: &[u8],
        attached_handles: &[KernelTransferredHandle],
    ) -> Result<(KernelReplyHandle, Option<u64>), IpcError> {
        crate::ipc::enqueue_endpoint_call_with_handles(
            endpoint,
            caller_task_id,
            request,
            attached_handles,
        )
    }

    pub fn recv(
        endpoint: KernelEndpointHandle,
    ) -> Result<Option<(KernelReplyHandle, alloc::vec::Vec<u8>)>, IpcError> {
        crate::ipc::recv_endpoint(endpoint)
    }

    pub fn recv_with_limit(
        endpoint: KernelEndpointHandle,
        request_capacity: usize,
    ) -> Result<Option<(KernelReplyHandle, alloc::vec::Vec<u8>)>, IpcError> {
        crate::ipc::recv_endpoint_with_limits(endpoint, request_capacity)
    }

    pub fn recv_with_limits_and_handles(
        endpoint: KernelEndpointHandle,
        request_capacity: usize,
        handle_capacity: usize,
    ) -> Result<
        Option<(
            KernelReplyHandle,
            alloc::vec::Vec<u8>,
            alloc::vec::Vec<KernelTransferredHandle>,
        )>,
        IpcError,
    > {
        crate::ipc::recv_endpoint_with_limits_and_handles(
            endpoint,
            request_capacity,
            handle_capacity,
        )
    }

    pub fn recv_with_sender_and_limits(
        endpoint: KernelEndpointHandle,
        request_capacity: usize,
        handle_capacity: usize,
    ) -> Result<
        Option<(
            KernelReplyHandle,
            alloc::vec::Vec<u8>,
            alloc::vec::Vec<KernelTransferredHandle>,
            u64,
        )>,
        IpcError,
    > {
        crate::ipc::recv_endpoint_with_sender_and_limits(
            endpoint,
            request_capacity,
            handle_capacity,
        )
    }

    pub fn add_receiver_waiter(
        endpoint: KernelEndpointHandle,
        task_id: u64,
    ) -> Result<bool, IpcError> {
        crate::ipc::add_endpoint_receiver_waiter(endpoint, task_id)
    }

    pub fn reply(reply: KernelReplyHandle, response: &[u8]) -> Result<u64, IpcError> {
        crate::ipc::complete_endpoint_reply(reply, response)
    }

    pub fn reply_with_handles(
        reply: KernelReplyHandle,
        response: &[u8],
        attached_handles: &[KernelTransferredHandle],
    ) -> Result<u64, IpcError> {
        crate::ipc::complete_endpoint_reply_with_handles(reply, response, attached_handles)
    }

    pub fn take_response(
        reply: KernelReplyHandle,
    ) -> Result<Option<alloc::vec::Vec<u8>>, IpcError> {
        crate::ipc::take_endpoint_response(reply)
    }

    pub fn take_response_with_handle_limit(
        reply: KernelReplyHandle,
        handle_capacity: usize,
    ) -> Result<
        Option<(
            alloc::vec::Vec<u8>,
            alloc::vec::Vec<KernelTransferredHandle>,
        )>,
        IpcError,
    > {
        crate::ipc::take_endpoint_response_with_handle_limit(reply, handle_capacity)
    }

    pub fn cancel_call(reply: KernelReplyHandle, caller_task_id: u64) -> Result<(), IpcError> {
        crate::ipc::cancel_endpoint_call(reply, caller_task_id)
    }

    pub fn remove_waiters_for_task(task_id: u64) -> usize {
        crate::ipc::remove_endpoint_waiters_for_task(task_id)
    }

    pub fn fail_owned_by_task(task_id: u64, err: IpcError) -> EndpointWakeSet {
        crate::ipc::fail_endpoints_owned_by_task(task_id, err)
    }
}

pub mod region {
    pub use crate::ipc::IpcError;
    pub use crate::ipc::KernelSharedRegionHandle;

    pub fn create(byte_len: usize) -> Result<KernelSharedRegionHandle, IpcError> {
        crate::ipc::create_shared_region(byte_len)
    }

    pub fn map(region: KernelSharedRegionHandle) -> Option<(*mut u8, usize)> {
        crate::ipc::map_shared_region(region)
    }

    pub fn frames(region: KernelSharedRegionHandle) -> Option<alloc::vec::Vec<u64>> {
        crate::ipc::shared_region_frames(region)
    }
}

pub use endpoint::{
    add_receiver_waiter as add_endpoint_receiver_waiter, cancel_call as cancel_endpoint_call,
    create as create_endpoint, create_for_task as create_endpoint_for_task,
    enqueue_call as enqueue_endpoint_call,
    enqueue_call_with_handles as enqueue_endpoint_call_with_handles,
    fail_owned_by_task as fail_endpoints_owned_by_task, recv as recv_endpoint,
    recv_with_limit as recv_endpoint_with_limit,
    recv_with_limits_and_handles as recv_endpoint_with_limits_and_handles,
    recv_with_sender_and_limits as recv_endpoint_with_sender_and_limits,
    remove_waiters_for_task as remove_endpoint_waiters_for_task, reply as complete_endpoint_reply,
    reply_with_handles as complete_endpoint_reply_with_handles,
    take_response as take_endpoint_response,
    take_response_with_handle_limit as take_endpoint_response_with_handle_limit,
};
pub use region::{
    create as create_shared_region, frames as shared_region_frames, map as map_shared_region,
};
