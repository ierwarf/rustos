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

use pagerd::library_custody::{self, Custody, Pressure, ReclaimHost};
use pagerd::library_warm::{self, ReadChunk, WarmHost};
use pagerd::{request_sender_is_authorized, PagerFaultError, PagerState};
use rustos_svc_runtime::{ipc, pager as pager_rendezvous};
use rustos_user_abi::pager::{
    pager_pressure_name, PagerAnonymousPolicyWire, PagerFaultDispatchWire, PagerProtectRangeWire,
    PagerReleaseRangeWire, PagerVmRegionWire, MM_BROKER_OP_SET_ANON_POLICY,
    PAGER_FAULT_ABI_VERSION, PAGER_FAULT_RUN_PAGES_MAX, PAGER_MAX_VMAS_PER_PROCESS,
    PAGER_MAX_WIRED_SERVICES,
};
use rustos_user_abi::syscall::{
    CommercialMaxProtocolRequest, CommercialMaxProtocolResponse, RustosMmBrokerArgs,
    COMMERCIAL_MAX_PAGERD_OP_BACKING_OBJECT, COMMERCIAL_MAX_PAGERD_OP_PROTECT_OBJECT,
    COMMERCIAL_MAX_PAGERD_OP_RELEASE_OBJECT, COMMERCIAL_MAX_PROTOCOL_ABI_VERSION,
    COMMERCIAL_MAX_PROTOCOL_PAGERD, IPC_MAX_INLINE_BYTES, IPC_SERVICE_LINUX_SYSCALLD,
    IPC_SERVICE_PAGERD, IPC_SERVICE_ROOTD, IPC_SERVICE_STORAGED, IPC_SERVICE_VFSD,
    MM_BROKER_ABI_VERSION, SYS_RUSTOS_MM_BROKER,
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
    let endpoint = ipc::endpoint_create_with_limits(IPC_MAX_INLINE_BYTES as u64, 0);
    if endpoint < 0 || ipc::register_service_endpoint(IPC_SERVICE_PAGERD, endpoint as u64) < 0 {
        ipc::debug_line("pagerd: endpoint registration failed");
        return;
    }
    ipc::debug_line("pagerd: pager policy endpoint registered");
    // Independent thread: the receive loop (the IPC fast path) is untouched.
    if let Err(errno) = rustos_svc_runtime::heartbeat::spawn_thread(rustos_user_abi::syscall::IPC_SERVICE_PAGERD) {
        let _ = errno;
        ipc::debug_line("pagerd: heartbeat thread unavailable");
    }
    // An endpoint object may be unregistered and reused. Its publication lease
    // epoch, not a per-process constant or endpoint handle, binds cache/fault
    // authority across service restart.
    let epoch = match pager_rendezvous::source_owner_epoch() {
        Ok(epoch) => epoch,
        Err(_) => {
            ipc::debug_line("pagerd: frame-source lease query failed");
            return;
        }
    };
    if !source_control_selfcheck(epoch) {
        ipc::debug_line("pagerd: frame-source-control selfcheck failed");
        return;
    }
    ipc::debug_line("pagerd: frame-source-control selfcheck passed");
    publish_anonymous_policy();
    let mut pager = PagerState::new(epoch);
    serve(endpoint as u64, &mut pager);
}

/// Hands ring0 the anonymous-mapping policy this pager wants enforced.
///
/// These are arbitration decisions - how much one fault populates, how many
/// demand-paged regions a process may hold, which services stay wired - not
/// mechanism, so they belong here. They lived as ring0 constants only because
/// the sole transport was a synchronous call on the fault path, which measured
/// 5.7 ms p99 on `mmap`. Publication stays off the fault path: ring0 reads a
/// coherent sequence snapshot, and the sole IRQ-off reader falls back to its
/// compiled-in run length if a concurrent update remains unstable.
///
/// Published after the endpoint exists, because ring0 authorizes this by the
/// caller owning the pager service endpoint rather than by a PID. Ring0 keeps
/// its own default until this lands, so a failure here leaves the system on
/// that default rather than half-configured. Later pressure-policy updates use
/// the same sequence publication instead of mutating fields in place.
/// Prove the live native supply/withdrawal route before accepting source-backed
/// work. This is a one-page capability/lifetime check, not file-pager readiness.
fn source_control_selfcheck(epoch: u64) -> bool {
    let key = rustos_user_abi::syscall::FrameSourceKeyWire {
        namespace: u64::MAX, object: 1, object_generation: epoch,
        content_generation: 1, page_offset: 0,
    };
    let token = match pager_rendezvous::source_supply(key, u64::from(rustos_user_abi::pager::VM_PROT_READ), b"source-control-check") {
        Ok(token) => token,
        Err(_) => return false,
    };
    pager_rendezvous::source_retire(token).is_ok()
        && pager_rendezvous::source_owner_epoch() == Ok(epoch)
}

fn publish_anonymous_policy() {
    let mut wired_services = [0_u64; PAGER_MAX_WIRED_SERVICES];
    // The services the whole system starts through. Ring0 answers anonymous
    // faults itself now, so the recursive-fault cycle this list was built for
    // cannot form; it stays as a conservative hold on boot-time memory
    // behaviour, and it is this pager's call to keep or drop.
    wired_services[0] = IPC_SERVICE_PAGERD;
    wired_services[1] = IPC_SERVICE_ROOTD;
    wired_services[2] = IPC_SERVICE_VFSD;
    wired_services[3] = IPC_SERVICE_STORAGED;
    wired_services[4] = IPC_SERVICE_LINUX_SYSCALLD;
    let policy = PagerAnonymousPolicyWire {
        version: PAGER_FAULT_ABI_VERSION,
        reserved0: 0,
        fault_run_pages: PAGER_FAULT_RUN_PAGES_MAX,
        process_vma_ceiling: PAGER_MAX_VMAS_PER_PROCESS as u32,
        demand_enabled: 1,
        wired_services,
        reserved1: [0; 2],
    };
    let args = RustosMmBrokerArgs {
        abi_version: MM_BROKER_ABI_VERSION,
        op: MM_BROKER_OP_SET_ANON_POLICY,
        addr: (&policy as *const PagerAnonymousPolicyWire) as u64,
        ..RustosMmBrokerArgs::default()
    };
    // SAFETY: the broker takes one pointer to a caller-owned `repr(C)` struct
    // and copies it out before returning; `args` and the `policy` it points at
    // both outlive this call, and ring0 validates every field before it can
    // become policy.
    let status = unsafe {
        rustos_svc_runtime::syscall::syscall1(
            SYS_RUSTOS_MM_BROKER,
            (&args as *const RustosMmBrokerArgs) as u64,
        )
    };
    if status < 0 {
        ipc::debug_line("pagerd: anonymous policy publication refused");
    } else {
        ipc::debug_line("pagerd: anonymous policy published");
    }
}

fn serve(endpoint: u64, pager: &mut PagerState) {
    let mut bytes = [0_u8; IPC_MAX_INLINE_BYTES];
    let mut dispatch = PagerFaultDispatchWire::default();
    loop {
        // Retries until the kernel admits the heartbeat thread (procd may not
        // be up when pagerd starts); a single relaxed load once it is running.
        rustos_svc_runtime::heartbeat::ensure_thread(rustos_user_abi::syscall::IPC_SERVICE_PAGERD);
        warm_worker::ensure();
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

/// Boot-time shared-library cache warm-up on its own thread.
///
/// It never runs on the serve/fault thread: it waits for vfsd, which may
/// itself need this pager to answer requests, and the cache is only a
/// performance hint. Failure leaves every mapping on the eager-copy path.
mod warm_worker {
    use super::*;
    use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use rustos_user_abi::syscall::{
        LibraryIdentityWire, VfsFileIdentityWire, VfsIpcRequest, VfsIpcResponse,
        VFS_IPC_OP_LIBRARY_READ, VFS_LIBRARY_READ_MAX_BYTES,
    };

    const SYS_GETPID: u64 = 39;
    const SYS_GETTID: u64 = 186;
    const STACK_BYTES: usize = 32 * 1024;
    const CALL_LIMIT_MS: u64 = 5_000;
    const RETRY_MS: u64 = 250;
    const MAX_ATTEMPTS: u32 = 240;
    const RECLAIM_POLL_MS: u64 = 100;

    #[repr(align(16))]
    // LAYOUT: the field is only addressed, never read, as the worker's stack.
    struct Stack(#[allow(dead_code)] [u8; STACK_BYTES]);
    struct Buffers {
        request: VfsIpcRequest,
        response: VfsIpcResponse,
        manifest: [u8; rustos_library_cache_policy::MAX_MANIFEST_BYTES],
        chunk: [u8; VFS_LIBRARY_READ_MAX_BYTES],
    }
    // Owned exclusively by the single warm thread once it is started.
    static mut STACK: Stack = Stack([0; STACK_BYTES]);
    static mut BUFFERS: Buffers = Buffers {
        // SAFETY: the request and response wire structs are plain-old-data for
        // which the all-zero bit pattern is a valid value.
        request: unsafe { core::mem::zeroed() },
        response: unsafe { core::mem::zeroed() },
        manifest: [0; rustos_library_cache_policy::MAX_MANIFEST_BYTES],
        chunk: [0; VFS_LIBRARY_READ_MAX_BYTES],
    };
    static STARTED: AtomicBool = AtomicBool::new(false);
    static ATTEMPTS: AtomicU32 = AtomicU32::new(0);

    /// Retried from the serve loop: `clone` answers ENOSYS until procd plans
    /// thread creation. One attempt per 32 calls, one relaxed load afterwards.
    pub fn ensure() {
        if STARTED.load(Ordering::Relaxed) || ATTEMPTS.fetch_add(1, Ordering::Relaxed) % 32 != 0 {
            return;
        }
        #[cfg(all(target_arch = "x86_64", not(test)))]
        {
            // ORDERING: AcqRel claims the single spawn; the failure Release below
            // republishes "not started" only after the spawn attempt returned.
            // SAFETY: the stack is a private static handed to exactly one thread
            // (guarded by the compare_exchange below) and never reused.
            if STARTED.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
                return;
            }
            // SAFETY: only the compare_exchange winner reaches this point, so the
            // static stack is not aliased; the address stays in bounds.
            let top = unsafe { (core::ptr::addr_of_mut!(STACK) as *mut u8).add(STACK_BYTES) } as u64;
            // SAFETY: `top` is the end of a private stack that lives for the
            // program and `entry` never returns into this frame.
            let spawned = unsafe { rustos_svc_runtime::worker::spawn(top, entry) };
            if spawned.is_ok() {
                ipc::debug_line("pagerd: library warm thread spawned");
            }
            if let Err(errno) = spawned {
                // ORDERING: Release publishes the stack as unclaimed again.
                STARTED.store(false, Ordering::Release);
                // Name the first refusal; retries stay one per 32 receive-loop turns.
                static REPORTED: AtomicBool = AtomicBool::new(false);
                if !REPORTED.swap(true, Ordering::Relaxed) {
                    let digits = decimal(errno);
                    let mut line = [0_u8; 64];
                    let prefix = b"pagerd: library warm spawn refused errno=";
                    line[..prefix.len()].copy_from_slice(prefix);
                    line[prefix.len()..prefix.len() + digits.len()].copy_from_slice(&digits);
                    if let Ok(text) = core::str::from_utf8(&line[..prefix.len() + digits.len()]) {
                        ipc::debug_line(text);
                    }
                }
            }
        }
    }

    /// Fixed-capacity signed decimal for the give-up diagnostic (no allocator).
    fn decimal(value: i64) -> heapless_decimal::Decimal {
        heapless_decimal::Decimal::new(value)
    }

    mod heapless_decimal {
        pub struct Decimal {
            bytes: [u8; 21],
            start: usize,
        }
        impl Decimal {
            pub fn new(value: i64) -> Self {
                let mut bytes = [0_u8; 21];
                let mut at = bytes.len();
                let negative = value < 0;
                let mut magnitude = value.unsigned_abs();
                loop {
                    at -= 1;
                    bytes[at] = b'0' + (magnitude % 10) as u8;
                    magnitude /= 10;
                    if magnitude == 0 {
                        break;
                    }
                }
                if negative {
                    at -= 1;
                    bytes[at] = b'-';
                }
                Self { bytes, start: at }
            }
        }
        impl core::ops::Deref for Decimal {
            type Target = [u8];
            fn deref(&self) -> &[u8] {
                &self.bytes[self.start..]
            }
        }
    }

    extern "C" fn entry() -> ! {
        // SAFETY: only this thread touches BUFFERS after the guarded spawn.
        let buffers = unsafe { &mut *core::ptr::addr_of_mut!(BUFFERS) };
        // No allocation on the very first line: it proves the thread runs.
        ipc::debug_bytes(b"pagerd: library warm thread started\n");
        let mut last_endpoint = 0_i64;
        let mut last_host_error = 0_i32;
        let mut settled = false;
        // Custody outlives retried runs: an object sealed by an earlier,
        // interrupted run stays tracked (a later run sees it as already present).
        let mut custody = Custody::new();
        for _ in 0..MAX_ATTEMPTS {
            let endpoint = ipc::lookup_service_endpoint(IPC_SERVICE_VFSD);
            last_endpoint = endpoint as i64;
            let pid = unsafe { rustos_svc_runtime::syscall::syscall0(SYS_GETPID) };
            let tid = unsafe { rustos_svc_runtime::syscall::syscall0(SYS_GETTID) };
            let (pid, tid) = if pid > 0 && tid > 0 {
                (pid as u64, tid as u64)
            } else {
                (0, 0)
            };
            if endpoint > 0 && pid != 0 {
                let mut host = VfsHost { endpoint: endpoint as u64, pid, tid, request: &mut buffers.request, response: &mut buffers.response, serial: 0, custody: &mut custody };
                match library_warm::warm(&mut host, &mut buffers.manifest, &mut buffers.chunk) {
                    Ok(report) => {
                        settled = true;
                        if report.sealed > 0 || report.present > 0 {
                            ipc::debug_line("pagerd: library cache warmed");
                            if report.refused > 0 {
                                // Rows are skipped one by one; name how many and
                                // the first cause so a partial cache is visible.
                                let (stage, code) = report
                                    .first_refusal
                                    .map_or((-1, 0), |skip| (skip.stage as i64, skip.code));
                                let mut line = [0_u8; 112];
                                let mut at = 0;
                                for part in [
                                    &b"pagerd: library cache warm partial sealed="[..],
                                    &decimal(i64::from(report.sealed))[..],
                                    &b" refused="[..],
                                    &decimal(i64::from(report.refused))[..],
                                    &b" stage="[..],
                                    &decimal(stage)[..],
                                    &b" code="[..],
                                    &decimal(code)[..],
                                ] {
                                    let take = part.len().min(line.len() - at);
                                    line[at..at + take].copy_from_slice(&part[..take]);
                                    at += take;
                                }
                                if let Ok(text) = core::str::from_utf8(&line[..at]) {
                                    ipc::debug_line(text);
                                }
                            }
                        } else {
                            // Name the first refused row once: stage is the
                            // `library_warm::Stage` ordinal, code its errno.
                            let (stage, code) = report
                                .first_refusal
                                .map_or((-1, 0), |skip| (skip.stage as i64, skip.code));
                            let mut line = [0_u8; 96];
                            let mut at = 0;
                            for part in [
                                &b"pagerd: library cache warm sealed nothing stage="[..],
                                &decimal(stage)[..],
                                &b" code="[..],
                                &decimal(code)[..],
                                &b" refused="[..],
                                &decimal(i64::from(report.refused))[..],
                            ] {
                                let take = part.len().min(line.len() - at);
                                line[at..at + take].copy_from_slice(&part[..take]);
                                at += take;
                            }
                            if let Ok(text) = core::str::from_utf8(&line[..at]) {
                                ipc::debug_line(text);
                            }
                        }
                        break;
                    }
                    Err(library_warm::WarmError::Manifest) => {
                        settled = true;
                        ipc::debug_line("pagerd: library cache manifest refused");
                        break;
                    }
                    Err(library_warm::WarmError::Host(errno)) => last_host_error = errno,
                }
            }
            rustos_svc_runtime::syscall::sleep_millis(RETRY_MS);
        }
        if !settled {
            // Exhausted retries: never fail silently, name the last cause.
            let mut line = [0_u8; 96];
            let mut at = 0;
            for part in [
                &b"pagerd: library warm gave up vfsd-endpoint="[..],
                &decimal(last_endpoint)[..],
                &b" host-error="[..],
                &decimal(last_host_error as i64)[..],
            ] {
                let take = part.len().min(line.len() - at);
                line[at..at + take].copy_from_slice(&part[..take]);
                at += take;
            }
            if let Ok(text) = core::str::from_utf8(&line[..at]) {
                ipc::debug_line(text);
            }
        }
        // The process lives for the service. From here this thread is the
        // cache custodian: it gives warmed objects back, coldest first, when
        // the kernel reports memory pressure. Retirement is policy (here);
        // ring0 frees only frames no mapping or pin still holds.
        let mut lowest_free = u64::MAX;
        let mut announced = false;
        loop {
            let sampled = (KernelReclaim).pressure();
            if let Ok(sample) = sampled {
                if !announced || sample.free_pages < lowest_free / 2 {
                    announced = true;
                    lowest_free = lowest_free.min(sample.free_pages);
                    let mut line = [0_u8; 128];
                    let mut at = 0;
                    for part in [
                        &b"pagerd: library custodian sample free="[..],
                        &decimal(sample.free_pages as i64)[..],
                        &b" min="[..],
                        &decimal(sample.min_free_pages as i64)[..],
                        &b" kept="[..],
                        &decimal(custody.objects() as i64)[..],
                    ] {
                        let take = part.len().min(line.len() - at);
                        line[at..at + take].copy_from_slice(&part[..take]);
                        at += take;
                    }
                    if let Ok(text) = core::str::from_utf8(&line[..at]) {
                        ipc::debug_line(text);
                    }
                }
                if let Some(done) =
                    library_custody::reclaim_with_pressure(&mut custody, &mut KernelReclaim, sample)
                {
                    let mut line = [0_u8; 112];
                    let mut at = 0;
                    for part in [
                        &b"pagerd: library cache reclaimed objects="[..],
                        &decimal(i64::from(done.objects))[..],
                        &b" pages="[..],
                        &decimal(done.pages as i64)[..],
                        &b" free="[..],
                        &decimal(done.free_pages as i64)[..],
                        &b" kept="[..],
                        &decimal(custody.objects() as i64)[..],
                    ] {
                        let take = part.len().min(line.len() - at);
                        line[at..at + take].copy_from_slice(&part[..take]);
                        at += take;
                    }
                    if let Ok(text) = core::str::from_utf8(&line[..at]) {
                        ipc::debug_line(text);
                    }
                }
            }
            if custody.objects() == 0 {
                // Nothing held to reclaim; stop polling until service teardown.
                loop {
                    rustos_svc_runtime::syscall::sleep_millis(60_000);
                }
            }
            rustos_svc_runtime::syscall::sleep_millis(RECLAIM_POLL_MS);
        }
    }

    /// The kernel half of the retention pass: read-only counters and retire.
    struct KernelReclaim;
    impl ReclaimHost for KernelReclaim {
        fn pressure(&mut self) -> Result<Pressure, i64> {
            pager_rendezvous::library_pressure().map(|wire| Pressure {
                free_pages: wire.free_pages,
                min_free_pages: wire.min_free_pages,
                watermark_denials: wire.watermark_denials,
            })
        }
        fn retire(&mut self, object: u64) -> Result<(), i64> {
            pager_rendezvous::library_retire(object).map(|_| ())
        }
    }

    struct VfsHost<'a> {
        endpoint: u64,
        pid: u64,
        tid: u64,
        request: &'a mut VfsIpcRequest,
        response: &'a mut VfsIpcResponse,
        serial: u64,
        custody: &'a mut Custody,
    }

    impl WarmHost for VfsHost<'_> {
        fn read(&mut self, path: &str, offset: u64, out: &mut [u8]) -> Result<ReadChunk, i32> {
            const IDENTITY: usize = VfsFileIdentityWire::WIRE_BYTES;
            if path.len() > self.request.path.len() || out.len() > VFS_LIBRARY_READ_MAX_BYTES {
                return Err(22);
            }
            self.serial += 1;
            self.request.version = 0;
            self.request.op = VFS_IPC_OP_LIBRARY_READ;
            self.request.flags = 0;
            self.request.pid = self.pid;
            self.request.tid = self.tid;
            self.request.session_handle = 0;
            self.request.uid = 0;
            self.request.gid = 0;
            self.request.euid = 0;
            self.request.egid = 0;
            self.request.fd = 0;
            self.request.dirfd = 0;
            self.request.remote_id = 0;
            self.request.arg0 = offset;
            self.request.arg1 = out.len() as u64;
            self.request.arg2 = 0;
            self.request.arg3 = 0;
            self.request.operation_hi = self.pid;
            self.request.operation_lo = self.serial;
            self.request.path_len = path.len() as u32;
            self.request.payload_len = 0;
            self.request.path[..path.len()].copy_from_slice(path.as_bytes());
            // SAFETY: both buffers are exclusive, fixed-size and outlive the call.
            let got = unsafe {
                ipc::call_bounded(
                    self.endpoint,
                    (self.request as *const VfsIpcRequest).cast::<u8>(),
                    size_of::<VfsIpcRequest>(),
                    (self.response as *mut VfsIpcResponse).cast::<u8>(),
                    size_of::<VfsIpcResponse>(),
                    CALL_LIMIT_MS,
                )
            };
            if got < 0 {
                return Err((-got) as i32);
            }
            let response = &*self.response;
            if got as usize != size_of::<VfsIpcResponse>() {
                return Err(71);
            }
            if response.status != 0 {
                return Err(response.status.unsigned_abs() as i32);
            }
            let payload = response.payload_len as usize;
            if payload < IDENTITY || payload - IDENTITY > out.len() || payload > response.payload.len() {
                return Err(71);
            }
            let identity = VfsFileIdentityWire::from_bytes(&response.payload[..IDENTITY]).ok_or(71)?;
            let len = payload - IDENTITY;
            out[..len].copy_from_slice(&response.payload[IDENTITY..payload]);
            Ok(ReadChunk { len, file_len: response.value, attested: response.aux == 1, identity })
        }

        fn create(&mut self, identity: LibraryIdentityWire) -> Result<u64, i64> {
            pager_rendezvous::library_create(identity)
        }
        fn supply(&mut self, object: u64, page: u64, bytes: &[u8]) -> Result<(), i64> {
            pager_rendezvous::library_supply_page(object, page, bytes)
        }
        fn seal(&mut self, object: u64) -> Result<(), i64> {
            pager_rendezvous::library_seal(object)
        }
        fn retire(&mut self, object: u64) {
            let _ = pager_rendezvous::library_retire(object);
        }
        fn published(&mut self, rank: u16, object: u64, pages: u64) {
            // A full table leaves the object published but unreclaimable until
            // the lease ends; the manifest bound (128) equals the table size.
            let _ = self.custody.record(object, pages, rank);
        }
    }
}
