#![no_std]
//!
//! - **Owner:** `pagerd` owns anonymous-fault policy after the kernel has
//!   admitted an exact pager capability and fixed one-shot fault custody.
//! - **Boundary:** Every protocol envelope, sender identity, VMA admission,
//!   dispatch request, opaque frame capability, and requested frame right is
//!   untrusted until the matching ABI contract accepts it.
//! - **Lifecycle:** Register one service endpoint, admit the pager-owned VMA,
//!   consume one dispatched anonymous fault, issue one exact reply, and lose
//!   the old authority on token, VMA, process, or service-epoch revocation.
//! - **Concurrency:** The service loop serializes its bounded policy state;
//!   it holds no policy lock across endpoint receive or reply publication.
//! - **Failure:** Malformed envelopes, foreign senders, stale generations,
//!   non-demand/protection faults, and rights expansion return an explicit
//!   error without reusing a frame capability or fault token.
//! - **Forbidden:** No physical address, PID-only authority, generic request
//!   `arg0` frame grant, W+X frame right, or kernel-policy fallback.
//! - **Evidence:** `pager-fault-slot-lifecycle`,
//!   `pager-frame-grant-lifecycle`, pagerd unit tests, ABI tests, and
//!   `pager-fault-slot-lifecycle` TLA+ mutations.
#![no_main]

use core::mem::size_of;
#[cfg(not(test))]
use core::panic::PanicInfo;

use pagerd::{request_sender_is_authorized, PagerFaultError, PagerState};
use rustos_svc_runtime::ipc;
use rustos_user_abi::pager::{PagerFaultDispatchWire, PagerVmRegionWire};
use rustos_user_abi::syscall::{
    CommercialMaxProtocolRequest, CommercialMaxProtocolResponse,
    COMMERCIAL_MAX_PAGERD_OP_BACKING_OBJECT, COMMERCIAL_MAX_PAGERD_OP_FAULT_RESOLVE,
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION, COMMERCIAL_MAX_PROTOCOL_PAGERD, IPC_MAX_INLINE_BYTES,
    IPC_SERVICE_PAGERD,
};

rustos_svc_runtime::entry!(service_main);

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    loop {}
}

#[no_mangle]
pub extern "C" fn rust_eh_personality() {}

fn service_main() {
    let endpoint = ipc::endpoint_create();
    if endpoint < 0 || ipc::register_service_endpoint(IPC_SERVICE_PAGERD, endpoint as u64) < 0 {
        ipc::debug_line("pagerd: endpoint registration failed");
        return;
    }
    ipc::debug_line("pagerd: pager policy endpoint registered");
    let mut pager = PagerState::new(1);
    serve(endpoint as u64, &mut pager);
}

fn serve(endpoint: u64, pager: &mut PagerState) {
    let mut bytes = [0_u8; IPC_MAX_INLINE_BYTES];
    loop {
        let mut reply_cap = 0;
        let mut sender_pid = 0;
        let mut sender_tid = 0;
        let received = unsafe {
            ipc::recv_with_sender(
                endpoint,
                bytes.as_mut_ptr(),
                bytes.len(),
                &mut reply_cap,
                &mut sender_pid,
                &mut sender_tid,
            )
        };
        if received < 0 {
            continue;
        }
        let response = handle_request(pager, received as usize, &bytes, sender_pid, sender_tid);
        unsafe {
            ipc::reply(
                reply_cap,
                (&response as *const CommercialMaxProtocolResponse).cast::<u8>(),
                size_of::<CommercialMaxProtocolResponse>(),
            );
        }
    }
}

fn handle_request(
    pager: &mut PagerState,
    received: usize,
    bytes: &[u8],
    sender_pid: u64,
    sender_tid: u64,
) -> CommercialMaxProtocolResponse {
    if received != size_of::<CommercialMaxProtocolRequest>() {
        return error_response(22);
    }
    let request = read_unaligned::<CommercialMaxProtocolRequest>(bytes);
    let mut response = CommercialMaxProtocolResponse {
        header: request.header,
        ..CommercialMaxProtocolResponse::default()
    };
    response.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    if !request.has_valid_envelope()
        || !request_sender_is_authorized(&request, sender_pid, sender_tid)
        || request.header.protocol != COMMERCIAL_MAX_PROTOCOL_PAGERD
        || request.header.service_id != IPC_SERVICE_PAGERD
        || request.path_len != 0
    {
        response.status = 13;
        return response;
    }
    let result = match request.header.op {
        COMMERCIAL_MAX_PAGERD_OP_BACKING_OBJECT => decode_payload::<PagerVmRegionWire>(&request)
            .and_then(|region| {
                pager.admit_region(region)?;
                Ok(None)
            }),
        COMMERCIAL_MAX_PAGERD_OP_FAULT_RESOLVE => {
            decode_payload::<PagerFaultDispatchWire>(&request)
                .and_then(|dispatch| pager.resolve_anonymous_first_touch(dispatch).map(Some))
        }
        _ => Err(PagerFaultError::NotManaged),
    };
    match result {
        Ok(Some(reply)) => write_payload(&mut response, &reply),
        Ok(None) => {}
        Err(error) => response.status = errno(error),
    }
    response.value0 = pager.epoch();
    response
}

fn decode_payload<T: Copy>(request: &CommercialMaxProtocolRequest) -> Result<T, PagerFaultError> {
    if request.payload_len as usize != size_of::<T>() {
        return Err(PagerFaultError::Malformed);
    }
    Ok(read_unaligned(&request.payload))
}

fn write_payload<T: Copy>(response: &mut CommercialMaxProtocolResponse, value: &T) {
    let len = size_of::<T>();
    unsafe {
        core::ptr::copy_nonoverlapping(
            (value as *const T).cast::<u8>(),
            response.payload.as_mut_ptr(),
            len,
        );
    }
    response.payload_len = len as u32;
}

fn read_unaligned<T: Copy>(bytes: &[u8]) -> T {
    assert!(bytes.len() >= size_of::<T>());
    unsafe { core::ptr::read_unaligned(bytes.as_ptr().cast::<T>()) }
}

fn error_response(status: i32) -> CommercialMaxProtocolResponse {
    CommercialMaxProtocolResponse {
        status,
        ..CommercialMaxProtocolResponse::default()
    }
}

const fn errno(error: PagerFaultError) -> i32 {
    match error {
        PagerFaultError::Malformed => 22,
        PagerFaultError::Stale => 116,
        PagerFaultError::NotManaged => 95,
        PagerFaultError::Pressure => 11,
        PagerFaultError::EpochExhausted => 75,
    }
}
