pub use crate::ipc::{
    ChannelIdentity, EndpointCallPriority, EndpointReceived, EndpointReceivedWithSender,
    EndpointResponseTake, EndpointResponseWithHandles, EndpointWakeSet, IpcError,
    KernelEndpointHandle, KernelReplyHandle, KernelSharedRegionHandle,
    KernelSharedRegionMappingHold, KernelTransferTicket, KernelTransferredHandle,
    MAX_ENDPOINT_WAKE_TASKS, ProcessIdentity, ServiceIdentity, TransferContext,
};

pub mod endpoint {
    pub use crate::ipc::{
        EndpointCallPriority, EndpointWakeSet, IpcError, KernelEndpointHandle, KernelReplyHandle,
        KernelTransferredHandle,
    };

    pub fn create() -> Result<KernelEndpointHandle, IpcError> {
        crate::ipc::create_endpoint()
    }

    pub fn create_for_task(owner_task_id: u64) -> Result<KernelEndpointHandle, IpcError> {
        crate::ipc::create_endpoint_for_task(Some(owner_task_id))
    }

    pub fn create_for_process(owner_process_id: u64) -> Result<KernelEndpointHandle, IpcError> {
        crate::ipc::create_endpoint_for_process(owner_process_id)
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

    /// Enqueues using a scheduler-derived class. Callers must not translate a
    /// user-supplied request field into this value: it is kernel scheduling
    /// authority, not service protocol policy.
    pub fn enqueue_call_with_handles_and_priority(
        endpoint: KernelEndpointHandle,
        caller_task_id: u64,
        request: &[u8],
        attached_handles: &[KernelTransferredHandle],
        priority: EndpointCallPriority,
    ) -> Result<(KernelReplyHandle, Option<u64>), IpcError> {
        crate::ipc::enqueue_endpoint_call_with_handles_and_priority(
            endpoint,
            caller_task_id,
            request,
            attached_handles,
            priority,
        )
    }

    pub fn receiver_process_for_reply(reply: KernelReplyHandle) -> Option<u64> {
        crate::ipc::endpoint_receiver_process_for_reply(reply)
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
    ) -> Result<Option<super::EndpointReceived>, IpcError> {
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
    ) -> Result<Option<super::EndpointReceivedWithSender>, IpcError> {
        crate::ipc::recv_endpoint_with_sender_and_limits(
            endpoint,
            request_capacity,
            handle_capacity,
        )
    }

    pub fn authorize_receiver(
        endpoint: KernelEndpointHandle,
        receiver_task_id: u64,
    ) -> Result<(), IpcError> {
        crate::ipc::authorize_endpoint_receiver(endpoint, receiver_task_id)
    }

    pub fn authorize_receiver_for_process(
        endpoint: KernelEndpointHandle,
        receiver_process_id: u64,
    ) -> Result<(), IpcError> {
        crate::ipc::authorize_endpoint_receiver_for_process(endpoint, receiver_process_id)
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

    pub fn reply_for_task(
        reply: KernelReplyHandle,
        receiver_task_id: u64,
        response: &[u8],
    ) -> Result<u64, IpcError> {
        crate::ipc::complete_endpoint_reply_for_task(reply, receiver_task_id, response)
    }

    pub fn reply_with_handles_for_task(
        reply: KernelReplyHandle,
        receiver_task_id: u64,
        response: &[u8],
        attached_handles: &[KernelTransferredHandle],
    ) -> Result<u64, IpcError> {
        crate::ipc::complete_endpoint_reply_with_handles_for_task(
            reply,
            receiver_task_id,
            response,
            attached_handles,
        )
    }

    pub fn reply_for_process(
        reply: KernelReplyHandle,
        receiver_process_id: u64,
        response: &[u8],
    ) -> Result<u64, IpcError> {
        crate::ipc::complete_endpoint_reply_for_process(reply, receiver_process_id, response)
    }

    pub fn reply_with_handles_for_process(
        reply: KernelReplyHandle,
        receiver_process_id: u64,
        response: &[u8],
        attached_handles: &[KernelTransferredHandle],
    ) -> Result<u64, IpcError> {
        crate::ipc::complete_endpoint_reply_with_handles_for_process(
            reply,
            receiver_process_id,
            response,
            attached_handles,
        )
    }

    pub fn take_response_detailed(
        reply: KernelReplyHandle,
        handle_capacity: usize,
    ) -> Result<super::EndpointResponseTake, IpcError> {
        crate::ipc::take_endpoint_response_detailed(reply, handle_capacity)
    }

    pub fn cancel_call(reply: KernelReplyHandle, caller_task_id: u64) -> Result<(), IpcError> {
        crate::ipc::cancel_endpoint_call(reply, caller_task_id)
    }

    pub fn cancel_call_with_transfers(
        reply: KernelReplyHandle,
        caller_task_id: u64,
    ) -> Result<alloc::vec::Vec<KernelTransferredHandle>, IpcError> {
        crate::ipc::cancel_endpoint_call_with_transfers(reply, caller_task_id)
    }

    pub fn cancel_calls_for_task(
        task_id: u64,
        release_transfers: impl FnMut(&[KernelTransferredHandle]),
    ) -> usize {
        crate::ipc::cancel_endpoint_calls_for_task(task_id, release_transfers)
    }

    pub fn remove_waiters_for_task(task_id: u64) -> usize {
        crate::ipc::remove_endpoint_waiters_for_task(task_id)
    }

    pub fn fail_owned_by_task(task_id: u64, err: IpcError) -> EndpointWakeSet {
        crate::ipc::fail_endpoints_owned_by_task(task_id, err)
    }

    pub fn fail_owned_by_process(process_id: u64, err: IpcError) -> EndpointWakeSet {
        crate::ipc::fail_endpoints_owned_by_process(process_id, err)
    }
}

pub mod region {
    pub use crate::ipc::IpcError;
    pub use crate::ipc::{KernelSharedRegionHandle, KernelSharedRegionMappingHold};

    pub fn create(byte_len: usize) -> Result<KernelSharedRegionHandle, IpcError> {
        crate::ipc::create_shared_region(byte_len)
    }

    pub fn create_for_process(
        owner_process_id: u64,
        byte_len: usize,
    ) -> Result<KernelSharedRegionHandle, IpcError> {
        crate::ipc::create_shared_region_for_process(owner_process_id, byte_len)
    }

    pub fn map(region: KernelSharedRegionHandle) -> Option<(*mut u8, usize)> {
        crate::ipc::map_shared_region(region)
    }

    pub fn frames(region: KernelSharedRegionHandle) -> Option<alloc::vec::Vec<u64>> {
        crate::ipc::shared_region_frames(region)
    }

    pub fn retain_descriptor(region: KernelSharedRegionHandle) -> bool {
        crate::ipc::retain_shared_region(region)
    }

    pub fn release_descriptor(region: KernelSharedRegionHandle) {
        crate::ipc::release_shared_region(region);
    }

    pub fn acquire_mapping(
        region: KernelSharedRegionHandle,
    ) -> Option<KernelSharedRegionMappingHold> {
        crate::ipc::acquire_shared_region_mapping(region)
    }

    pub fn service_deferred_reclaims(max_pages: usize) -> usize {
        crate::ipc::service_deferred_shared_region_reclaims(max_pages)
    }
}

pub use endpoint::{
    add_receiver_waiter as add_endpoint_receiver_waiter,
    authorize_receiver as authorize_endpoint_receiver,
    authorize_receiver_for_process as authorize_endpoint_receiver_for_process,
    cancel_call as cancel_endpoint_call,
    cancel_call_with_transfers as cancel_endpoint_call_with_transfers,
    cancel_calls_for_task as cancel_endpoint_calls_for_task, create as create_endpoint,
    create_for_process as create_endpoint_for_process, create_for_task as create_endpoint_for_task,
    enqueue_call as enqueue_endpoint_call,
    enqueue_call_with_handles as enqueue_endpoint_call_with_handles,
    enqueue_call_with_handles_and_priority as enqueue_endpoint_call_with_handles_and_priority,
    fail_owned_by_process as fail_endpoints_owned_by_process,
    fail_owned_by_task as fail_endpoints_owned_by_task, recv as recv_endpoint,
    recv_with_limit as recv_endpoint_with_limit,
    recv_with_limits_and_handles as recv_endpoint_with_limits_and_handles,
    recv_with_sender_and_limits as recv_endpoint_with_sender_and_limits,
    remove_waiters_for_task as remove_endpoint_waiters_for_task, reply as complete_endpoint_reply,
    reply_for_process as complete_endpoint_reply_for_process,
    reply_for_task as complete_endpoint_reply_for_task,
    reply_with_handles as complete_endpoint_reply_with_handles,
    reply_with_handles_for_process as complete_endpoint_reply_with_handles_for_process,
    reply_with_handles_for_task as complete_endpoint_reply_with_handles_for_task,
    take_response_detailed as take_endpoint_response_detailed,
};
pub use region::{
    acquire_mapping as acquire_shared_region_mapping, create as create_shared_region,
    create_for_process as create_shared_region_for_process, frames as shared_region_frames,
    map as map_shared_region, release_descriptor as release_shared_region_descriptor,
    retain_descriptor as retain_shared_region_descriptor,
    service_deferred_reclaims as service_deferred_shared_region_reclaims,
};
