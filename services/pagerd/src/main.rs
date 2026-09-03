#![no_std]
//!
//! - **Owner:** `pagerd` owns *pager-backed* fault policy - load ownership,
//!   COW, dirty writeback, eviction, and provider restart - after the kernel
//!   has admitted an exact pager capability and fixed one-shot fault custody.
//!   Anonymous first touch is no longer routed here: an anonymous page has no
//!   backing store and no external owner, so ring0 supplies it directly in the
//!   faulting task's own context (see `docs/ai/pager-protocol-contract.md` §0).
//!   The rendezvous reply path below is therefore live and currently reached
//!   by no dispatch; it is the contract `page_cache` lands on, and it stays
//!   exercised by the unit tests rather than by the running system.
//! - **Boundary:** Every protocol envelope, sender identity, VMA admission,
//!   dispatch request, opaque frame capability, and requested frame right is
//!   untrusted until the matching ABI contract accepts it.
//! - **Lifecycle:** Register one service endpoint, admit pager-owned VMAs,
//!   consume exact fault dispatches, and lose old authority on token,
//!   VMA, process, or service-epoch revocation.
//! - **Concurrency:** One passive pager thread owns the bounded policy state.
//!   It holds no policy lock across receive, reply, or reply-and-wait syscalls.
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
use rustos_svc_runtime::{ipc, pager as pager_rendezvous};
use rustos_user_abi::pager::{
    pager_pressure_name, PagerFaultDispatchWire, PagerProtectRangeWire, PagerReleaseRangeWire,
    PagerVmRegionWire,
};
use rustos_user_abi::syscall::{
    CommercialMaxProtocolRequest, CommercialMaxProtocolResponse,
    COMMERCIAL_MAX_PAGERD_OP_BACKING_OBJECT, COMMERCIAL_MAX_PAGERD_OP_PROTECT_OBJECT,
    COMMERCIAL_MAX_PAGERD_OP_RELEASE_OBJECT, COMMERCIAL_MAX_PROTOCOL_ABI_VERSION,
    COMMERCIAL_MAX_PROTOCOL_PAGERD, IPC_MAX_INLINE_BYTES, IPC_SERVICE_PAGERD,
};

rustos_svc_runtime::entry!(service_main);

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    rustos_svc_runtime::syscall::exit_group(101)
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
    let mut dispatch = PagerFaultDispatchWire::default();
    loop {
        let mut reply_cap = 0;
        let mut sender_pid = 0;
        let mut sender_tid = 0;
        let received = unsafe {
            ipc::try_recv_with_sender(
                endpoint,
                bytes.as_mut_ptr(),
                bytes.len(),
                &mut reply_cap,
                &mut sender_pid,
                &mut sender_tid,
            )
        };
        if received >= 0 {
            let response = handle_request(pager, received as usize, &bytes, sender_pid, sender_tid);
            unsafe {
                ipc::reply(
                    reply_cap,
                    (&response as *const CommercialMaxProtocolResponse).cast::<u8>(),
                    size_of::<CommercialMaxProtocolResponse>(),
                );
            }
        }

        // The fixed rendezvous is also this passive server's blocking receive.
        // Generic endpoint arrival wakes it with EAGAIN, returning control to
        // the policy request loop without a polling timer.
        // SAFETY: `dispatch` is writable for the synchronous fixed-size copy.
        if unsafe { pager_rendezvous::fault_wait(&mut dispatch) } != 0 {
            continue;
        }
        // A refused fault still has to be answered. The default reply is not
        // canonical for this dispatch, so ring0 rejects it, cancels the grant
        // and wakes the faulting task without a mapping - which the task sees
        // as a re-fault, not as a diagnosis. Name the cause here or the reason
        // a thread is looping on one address exists nowhere.
        let reply = match pager.resolve_anonymous_first_touch(dispatch) {
            Ok(reply) => reply,
            Err(error) => {
                report_refused_fault(error, dispatch.request.virtual_address);
                Default::default()
            }
        };
        // Reply and wait stay two entries on purpose. Merging them into one
        // `ReplyRecv`-shaped call measured as no change on a single-threaded
        // first-touch probe, and it removed the only thing that interleaved
        // this loop's two arrival sources: returning to user mode. A pager
        // with faults continuously queued then never drained its generic
        // endpoint, the `mmap` admission calls that create new demand-paged
        // regions timed out, and the fault path went quiet for the rest of the
        // boot. Making control win the tie instead simply starved faults.
        // Since anonymous faults stopped arriving here at all, the merge has
        // no remaining benefit to weigh against that: do not reintroduce it.
        //
        // SAFETY: the fixed reply is copied synchronously and remains bound to
        // the dispatch received by this thread.
        let _ = unsafe { pager_rendezvous::fault_reply(reply) };
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
    let result: Result<Option<rustos_user_abi::pager::PagerFaultReplyWire>, PagerFaultError> =
        match request.header.op {
            COMMERCIAL_MAX_PAGERD_OP_BACKING_OBJECT => {
                decode_payload::<PagerVmRegionWire>(&request).and_then(|region| {
                    pager.admit_region(region)?;
                    Ok(None)
                })
            }
            COMMERCIAL_MAX_PAGERD_OP_RELEASE_OBJECT => {
                decode_payload::<PagerReleaseRangeWire>(&request).and_then(|release| {
                    pager.release_range(release)?;
                    Ok(None)
                })
            }
            COMMERCIAL_MAX_PAGERD_OP_PROTECT_OBJECT => {
                decode_payload::<PagerProtectRangeWire>(&request).and_then(|protect| {
                    pager.protect_range(protect)?;
                    Ok(None)
                })
            }
            // Anonymous demand faults use the worker-bound fixed rendezvous. A
            // generic endpoint request may not carry a frame capability or become
            // a fallback transport for the exception path.
            _ if request.header.op
                == rustos_user_abi::syscall::COMMERCIAL_MAX_PAGERD_OP_FAULT_RESOLVE =>
            {
                Err(PagerFaultError::NotManaged)
            }
            _ => Err(PagerFaultError::NotManaged),
        };
    match result {
        Ok(Some(reply)) => write_payload(&mut response, &reply),
        Ok(None) => {}
        Err(error) => {
            response.status = errno(error);
            // The caller has to know *which* bounded table refused, because a
            // full region table, a split with no free slot and a malformed
            // range all reach it as one status. `value1` carries that code so
            // the broker can retry a split refusal and only a split refusal.
            response.value1 = u64::from(error.pressure_code());
            if error.pressure_code() != rustos_user_abi::pager::PAGER_PRESSURE_UNSPECIFIED {
                ipc::debug_line(pager_pressure_name(error.pressure_code()));
            }
        }
    }
    response.value0 = pager.epoch();
    response
}

/// Names the cause of a fault this pager could not resolve.
///
/// One line per distinct cause, not per fault: a thread that re-faults on the
/// same refused address would otherwise turn its own diagnosis into the
/// machine's dominant cost, which is how an earlier per-event log became a
/// 30-second stall.
fn report_refused_fault(error: PagerFaultError, address: u64) {
    use core::sync::atomic::{AtomicU8, Ordering};

    static REPORTED: AtomicU8 = AtomicU8::new(0);
    let bit = 1_u8 << refusal_index(error);
    if REPORTED.fetch_or(bit, Ordering::Relaxed) & bit != 0 {
        return;
    }
    let _ = address;
    ipc::debug_line(match error {
        // A managed VMA whose region this pager does not hold is the exact
        // ring0/pagerd divergence the shared range-edit rule exists to prevent.
        PagerFaultError::NotManaged => "pagerd: fault refused, no region for a ring0-managed range",
        PagerFaultError::Stale => "pagerd: fault refused, stale epoch or replayed token",
        PagerFaultError::Malformed => "pagerd: fault refused, malformed dispatch",
        PagerFaultError::EpochExhausted => "pagerd: fault refused, epoch exhausted",
        PagerFaultError::Pressure(code) => pager_pressure_name(code),
    });
}

const fn refusal_index(error: PagerFaultError) -> u32 {
    match error {
        PagerFaultError::Malformed => 0,
        PagerFaultError::Stale => 1,
        PagerFaultError::NotManaged => 2,
        PagerFaultError::Pressure(_) => 3,
        PagerFaultError::EpochExhausted => 4,
    }
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
        PagerFaultError::Pressure(_) => 11,
        PagerFaultError::EpochExhausted => 75,
    }
}
