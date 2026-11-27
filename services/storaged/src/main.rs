use std::io::Write;
use std::mem::size_of;
use std::slice;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

mod block;
mod read_engine;

use rustos_user_abi::performance::DVM_STORAGE_BOOT_READY_HARD_LIMIT_MS;
use rustos_user_abi::syscall::{
    CommercialMaxCapabilityLeaseWire, CommercialMaxProtocolDescriptorWire,
    CommercialMaxProtocolHeader, CommercialMaxProtocolRequest, CommercialMaxProtocolResponse,
    DvmBlockInfoWire,
    StorageBlockDescriptorWire, StoragedBulkReadResponse, StoragedBulkWriteRequest,
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION, COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS,
    COMMERCIAL_MAX_PROTOCOL_STORAGED, COMMERCIAL_MAX_STORAGED_BLOCK_FLAG_FUA,
    COMMERCIAL_MAX_STORAGED_OP_BLOCK_INVENTORY, COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_FLUSH,
    COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_INFO, COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ,
    COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ_BULK, COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_WRITE,
    COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_WRITE_BULK,
    COMMERCIAL_MAX_STORAGED_OP_PARTITION_SCAN, COMMERCIAL_MAX_STORAGED_OP_ROOT_VOLUME_SELECT,
    COMMERCIAL_MAX_STORAGED_OP_VOLUME_METADATA, IPC_MAX_INLINE_BYTES, IPC_SERVICE_STORAGED,
    IPC_SERVICE_VFSD, PRODUCT_MILESTONE_STORAGE_READY, STORAGED_BULK_READ_RESPONSE_HEADER_BYTES,
    STORAGE_FLAG_READONLY, STORAGE_TRANSPORT_DVM_BLOCK, SYS_RUSTOS_IPC_RECV_WITH_SENDER,
    SYS_RUSTOS_IPC_REPLY,
};

const RECV_BACKOFF: Duration = Duration::from_millis(50);
const DVM_E2E_EVENT_WAIT: Duration = Duration::from_secs(30);
/// Receive threads serving the storaged endpoint. Bulk reads from several
/// vfsd lanes are in flight at once; each needs its own reply slot and the
/// read engine below is the only shared read state.
const RECEIVE_THREADS: usize = 4;
// Endpoint registration proves the service identity only. The generation is
// published after its own read-only media-barrier completion, and every request
// rebinds that liveness proof to current DVM geometry before touching storage.
static DVM_E2E_READY_GENERATION: AtomicU64 = AtomicU64::new(0);
// Read by every request, written only on a readiness transition: a reader lock
// keeps the receive threads from serializing on it, and a monotonic sequence
// counter lets each receive thread reuse its local copy without atomic RMW
// contention on the shared RwLock cache line.
static PROVEN_BLOCK_INFO: std::sync::RwLock<Option<block::BlockInfo>> =
    std::sync::RwLock::new(None);
static PROVEN_BLOCK_INFO_SEQ: AtomicU64 = AtomicU64::new(0);
// A readiness transition may cause many normal loader probes. Keep the first
// one observable, but never let expected `EAGAIN` diagnostics become a DVM
// boot-time work source themselves.
static DVM_NOT_READY_DIAGNOSTIC_EMITTED: AtomicBool = AtomicBool::new(false);
// The readiness owner may legitimately observe the same unavailable state
// many times before a fixed DVM transport appears. Emit only errno
// transitions; a successful generation resets the witness.
static DVM_READINESS_LAST_ERRNO: AtomicU64 = AtomicU64::new(0);

fn main() {
    observability_client::info!("storaged", service, "service started");
    debug_line("storaged: endpoint create begin");
    let endpoint = rustos_svc_runtime::ipc::endpoint_create_with_limits(
        size_of::<StoragedBulkWriteRequest>() as u64,
        0,
    );
    if endpoint < 0 {
        debug_line("storaged: endpoint create failed");
        let _ = writeln!(
            std::io::stderr(),
            "storaged: endpoint create failed errno={}",
            -endpoint
        );
        return;
    }
    debug_line("storaged: endpoint create done");
    debug_line("storaged: endpoint register begin");
    let register =
        rustos_svc_runtime::ipc::register_service_endpoint(IPC_SERVICE_STORAGED, endpoint as u64);
    if register < 0 {
        debug_line(format!("storaged: endpoint register failed errno={}", -register).as_str());
        let _ = writeln!(
            std::io::stderr(),
            "storaged: endpoint register failed errno={}",
            -register
        );
        return;
    }
    debug_line("storaged: storage policy endpoint registered");
    if let Err(error) = start_service_health_worker() {
        debug_line(format!("storaged: heartbeat worker failed error={error}").as_str());
        return;
    }
    if let Err(error) = thread::Builder::new()
        .name("storaged-dvm-ready".to_string())
        .spawn(supervise_dvm_block_readiness)
    {
        debug_line(format!("storaged: dvm readiness worker failed error={error}").as_str());
        // A published endpoint without its sole readiness owner would return
        // `EAGAIN` forever.  Exit so rootd can revoke/restart this lease
        // rather than presenting false storage liveness.
        return;
    }
    if let Err(error) = thread::Builder::new()
        .name("storaged-read-pump".to_string())
        .spawn(|| block::run_read_pump())
    {
        // Without the completion owner every read would wait out its timeout.
        debug_line(format!("storaged: read pump failed error={error}").as_str());
        return;
    }
    for index in 1..RECEIVE_THREADS {
        let spawned = thread::Builder::new()
            .name(format!("storaged-recv-{index}"))
            .spawn(move || serve(endpoint as u64));
        if let Err(error) = spawned {
            // Fewer receivers only lowers read concurrency.
            debug_line(format!("storaged: receive thread {index} failed error={error}").as_str());
            break;
        }
    }
    serve(endpoint as u64);
}

/// Run independent exact-incarnation health evidence while the DVM storage
/// readiness worker owns a potentially long media barrier. An unready block
/// device does NOT suspend the supervisor's service heartbeat.
fn start_service_health_worker() -> Result<(), std::io::Error> {
    thread::Builder::new()
        .name("storaged-health".into())
        .spawn(|| {
            let mut health = rustos_svc_runtime::heartbeat::Heartbeat::new(IPC_SERVICE_STORAGED);
            loop {
                let now = rustos_svc_runtime::syscall::monotonic_nanos();
                let _ = health.poll(now);
                thread::sleep(Duration::from_millis(250));
            }
        })
        .map(|_| ())
}

/// Own the only bounded DVM-ready wait outside the receive loop. Event wakes
/// cause an exact geometry recheck; a revoke or successor generation removes
/// the old proof before requests can use it.
fn supervise_dvm_block_readiness() {
    let mut proven_generation = 0_u64;
    loop {
        let deadline = Instant::now() + Duration::from_millis(DVM_STORAGE_BOOT_READY_HARD_LIMIT_MS);
        match block::wait_until_ready(deadline).and_then(|info| {
            if info.generation == proven_generation {
                return Ok(info);
            }
            invalidate_dvm_readiness(&DVM_E2E_READY_GENERATION, &mut proven_generation);
            block::clear_quarantine_for_new_generation(info.generation);
            block::flush_before(info.generation, deadline)?;
            Ok(info)
        }) {
            Ok(info) => {
                let generation = info.generation;
                if generation != proven_generation {
                    if let Ok(mut slot) = PROVEN_BLOCK_INFO.write() {
                        *slot = Some(info);
                    }
                    PROVEN_BLOCK_INFO_SEQ.fetch_add(1, Ordering::Release);
                    DVM_E2E_READY_GENERATION.store(generation, Ordering::Release);
                    note_dvm_request_ready();
                    proven_generation = generation;
                    debug_line(dvm_block_e2e_marker(generation).as_str());
                    let _ = rustos_svc_runtime::ipc::product_milestone(
                        PRODUCT_MILESTONE_STORAGE_READY,
                        generation,
                        0,
                    );
                }
            }
            Err(errno) => {
                invalidate_dvm_readiness(&DVM_E2E_READY_GENERATION, &mut proven_generation);
                log_dvm_readiness_failure(errno);
                thread::sleep(RECV_BACKOFF);
                continue;
            }
        }
        // This is an atomic check-arm-recheck sleep. Completion/revoke wakes
        // revalidate the generation without a timer polling loop.
        let _ = block::wait_for_transport_event(Instant::now() + DVM_E2E_EVENT_WAIT);
    }
}

fn invalidate_dvm_readiness(published: &AtomicU64, proven_generation: &mut u64) {
    published.store(0, Ordering::Release);
    PROVEN_BLOCK_INFO_SEQ.fetch_add(1, Ordering::Release);
    if let Ok(mut slot) = PROVEN_BLOCK_INFO.write() {
        *slot = None;
    }
    // Recovery of the same generation must run the barrier and republish it.
    *proven_generation = 0;
}

fn dvm_e2e_ready_for_current_generation() -> Result<block::BlockInfo, i32> {
    let proven = DVM_E2E_READY_GENERATION.load(Ordering::Acquire);
    if proven != 0 {
        if let Ok(slot) = PROVEN_BLOCK_INFO.read() {
            if let Some(info) = *slot {
                if info.generation == proven {
                    return Ok(info);
                }
            }
        }
    }
    let info = block::info()?;
    if e2e_generation_matches(
        DVM_E2E_READY_GENERATION.load(Ordering::Acquire),
        info.generation,
    ) {
        if let Ok(mut slot) = PROVEN_BLOCK_INFO.write() {
            *slot = Some(info);
        }
        PROVEN_BLOCK_INFO_SEQ.fetch_add(1, Ordering::Release);
        Ok(info)
    } else {
        Err(libc::EAGAIN)
    }
}

/// Lock-free read-side cache for the receive threads: validates both the
/// published readiness generation and the monotonic write sequence so hot bulk
/// reads never bounce the shared `PROVEN_BLOCK_INFO` RwLock cache line.
fn dvm_e2e_ready_cached(
    cache: &mut Option<(u64, block::BlockInfo)>,
) -> Result<block::BlockInfo, i32> {
    let seq1 = PROVEN_BLOCK_INFO_SEQ.load(Ordering::Acquire);
    let proven = DVM_E2E_READY_GENERATION.load(Ordering::Acquire);
    let seq2 = PROVEN_BLOCK_INFO_SEQ.load(Ordering::Acquire);
    if proven != 0 && seq1 == seq2 {
        if let Some((cached_seq, info)) = *cache {
            if cached_seq == seq1 && info.generation == proven {
                return Ok(info);
            }
        }
    }
    let info = dvm_e2e_ready_for_current_generation()?;
    let seq_after = PROVEN_BLOCK_INFO_SEQ.load(Ordering::Acquire);
    let proven_after = DVM_E2E_READY_GENERATION.load(Ordering::Acquire);
    if proven_after == info.generation && PROVEN_BLOCK_INFO_SEQ.load(Ordering::Acquire) == seq_after
    {
        *cache = Some((seq_after, info));
    } else {
        *cache = None;
    }
    Ok(info)
}

const fn e2e_generation_matches(proven_generation: u64, live_generation: u64) -> bool {
    proven_generation != 0 && proven_generation == live_generation
}

fn transient_dvm_not_ready(errno: i32) -> bool {
    matches!(errno, libc::EAGAIN | libc::ENODEV | libc::ENOSYS)
}

fn log_dvm_readiness_failure(errno: i32) {
    let encoded = readiness_errno_witness(errno);
    if DVM_READINESS_LAST_ERRNO.swap(encoded, Ordering::AcqRel) != encoded {
        debug_line(format!("storaged: dvm-block readiness wait failed errno={errno}").as_str());
    }
}

const fn readiness_errno_witness(errno: i32) -> u64 {
    (errno as u32 as u64).saturating_add(1)
}

fn log_dvm_request_rejection(stage: &str, op: u16, errno: i32) {
    if !transient_dvm_not_ready(errno)
        || !DVM_NOT_READY_DIAGNOSTIC_EMITTED.swap(true, Ordering::AcqRel)
    {
        let _ = writeln!(
            std::io::stderr(),
            "storaged: request rejected stage={stage} op={op} errno={errno}"
        );
    }
}

fn note_dvm_request_ready() {
    DVM_NOT_READY_DIAGNOSTIC_EMITTED.store(false, Ordering::Release);
    DVM_READINESS_LAST_ERRNO.store(0, Ordering::Release);
}

fn dvm_block_e2e_marker(generation: u64) -> String {
    format!(
        "storaged: dvm-block e2e media barrier completed generation={generation} \
         path=vfs-policy->block-broker->shared-ring->linux-dvm->media-barrier"
    )
}

/// One receive thread's bulk-read reply slot. Only `reply_len()` bytes of it
/// are ever sent, so bytes past `payload_len` (left by earlier replies) are
/// never exposed and the slot is never cleared.
struct BulkReplySlot {
    response: Box<StoragedBulkReadResponse>,
}

impl BulkReplySlot {
    fn new() -> Self {
        Self {
            response: Box::new(StoragedBulkReadResponse::zeroed()),
        }
    }
}

fn log_reply_recv_failure(message: &'static str) {
    debug_line(format!("storaged: {message}").as_str());
}

fn serve(endpoint: u64) {
    let mut bulk_slot = BulkReplySlot::new();
    let mut recv_words = vec![0_u64; IPC_MAX_INLINE_BYTES.div_ceil(size_of::<u64>())];
    let mut cached_info: Option<(u64, block::BlockInfo)> = None;
    let mut pending_bulk_reply: Option<(u64, usize)> = None;
    loop {
        let mut reply_cap = 0_u64;
        let mut sender_pid = 0_u64;
        let mut sender_tid = 0_u64;
        let (received, used_fused) =
            if let Some((prev_reply_cap, prev_reply_len)) = pending_bulk_reply.take() {
                // SAFETY: `bulk_slot.response` holds the initialized bulk reply
                // of length `prev_reply_len` and is disjoint from `recv_words`.
                let status = unsafe {
                    rustos_svc_runtime::ipc::reply_then_recv(
                        endpoint,
                        prev_reply_cap,
                        (&*bulk_slot.response as *const StoragedBulkReadResponse).cast::<u8>(),
                        prev_reply_len,
                        recv_words.as_mut_ptr().cast::<u8>(),
                        IPC_MAX_INLINE_BYTES,
                        &mut reply_cap,
                        &mut sender_pid,
                        &mut sender_tid,
                        log_reply_recv_failure,
                    )
                };
                (status, true)
            } else {
                let status = syscall6(
                    SYS_RUSTOS_IPC_RECV_WITH_SENDER,
                    endpoint,
                    recv_words.as_mut_ptr() as u64,
                    IPC_MAX_INLINE_BYTES as u64,
                    (&mut reply_cap as *mut u64) as u64,
                    (&mut sender_pid as *mut u64) as u64,
                    (&mut sender_tid as *mut u64) as u64,
                );
                (status, false)
            };
        if received < 0 {
            if used_fused {
                // `reply_then_recv` already committed or recovered the reply;
                // fall straight into a plain receive without sleeping.
                continue;
            }
            thread::sleep(RECV_BACKOFF);
            continue;
        }
        let received = received as usize;
        // Both request shapes begin with the protocol header, whose `op` selects
        // the shape. A bulk write is header + exactly `payload_len` bytes.
        if received >= size_of::<CommercialMaxProtocolHeader>()
            // SAFETY: `recv_words` is 8-aligned, initialized, and at least
            // `IPC_MAX_INLINE_BYTES` long; the header is its first field.
            && unsafe { &*(recv_words.as_ptr().cast::<CommercialMaxProtocolHeader>()) }.op
                == COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_WRITE_BULK
        {
            // SAFETY: as above; the buffer holds a whole
            // `StoragedBulkWriteRequest`, and only `received` bytes of it are
            // trusted (`payload_of_request`).
            let bulk_req = unsafe { &*(recv_words.as_ptr().cast::<StoragedBulkWriteRequest>()) };
            let exact_sender = bulk_req.header.subject_pid == sender_pid
                && bulk_req.header.subject_tid == sender_tid;
            let owner_status = if exact_sender {
                rustos_svc_runtime::ipc::validate_service_owner(IPC_SERVICE_VFSD, sender_pid)
            } else {
                -(libc::EACCES as i64)
            };
            if !exact_sender || owner_status < 0 {
                reply_bulk_write_error(reply_cap, bulk_req.header, libc::EACCES);
            } else {
                reply_bulk_write_request(reply_cap, bulk_req, received, &mut cached_info);
            }
            continue;
        }
        if received != size_of::<CommercialMaxProtocolRequest>() {
            let _ = writeln!(
                std::io::stderr(),
                "storaged: request rejected stage=request-size received={} expected={} reply_cap={}",
                received,
                size_of::<CommercialMaxProtocolRequest>(),
                reply_cap
            );
            reply_commercial_error(
                reply_cap,
                &CommercialMaxProtocolRequest::default(),
                libc::EINVAL,
            );
            continue;
        }
        // SAFETY: `recv_words` is 8-aligned and `received == size_of::<CommercialMaxProtocolRequest>()`.
        let request = unsafe { &*(recv_words.as_ptr().cast::<CommercialMaxProtocolRequest>()) };
        let exact_sender = request.subject_is_exact_sender(sender_pid, sender_tid);
        let owner_status = if exact_sender {
            rustos_svc_runtime::ipc::validate_service_owner(IPC_SERVICE_VFSD, sender_pid)
        } else {
            -(libc::EACCES as i64)
        };
        if !exact_sender || owner_status < 0 {
            let _ = writeln!(
                std::io::stderr(),
                "storaged: request rejected stage=sender-authority sender_pid={} sender_tid={} subject_pid={} subject_tid={} exact_sender={} owner_errno={} reply_cap={}",
                sender_pid,
                sender_tid,
                request.header.subject_pid,
                request.header.subject_tid,
                exact_sender,
                owner_status.saturating_neg(),
                reply_cap
            );
            reply_commercial_error(reply_cap, request, libc::EACCES);
            continue;
        }
        pending_bulk_reply =
            reply_commercial_request(reply_cap, request, &mut bulk_slot, &mut cached_info);
    }
}

fn reply_bulk_write_error(
    reply_cap: u64,
    header: rustos_user_abi::syscall::CommercialMaxProtocolHeader,
    status: i32,
) {
    let mut response = CommercialMaxProtocolResponse {
        header,
        status,
        ..CommercialMaxProtocolResponse::default()
    };
    response.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    let reply = syscall3(
        SYS_RUSTOS_IPC_REPLY,
        reply_cap,
        (&response as *const CommercialMaxProtocolResponse) as u64,
        size_of::<CommercialMaxProtocolResponse>() as u64,
    );
    if reply < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "storaged: bulk write reply failed errno={}",
            -reply
        );
    }
}

fn reply_bulk_write_request(
    reply_cap: u64,
    request: &StoragedBulkWriteRequest,
    received: usize,
    cached_info: &mut Option<(u64, block::BlockInfo)>,
) {
    let mut response = CommercialMaxProtocolResponse {
        header: request.header,
        ..CommercialMaxProtocolResponse::default()
    };
    response.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    response.status = if !request.has_valid_shape() {
        libc::EINVAL
    } else {
        match dvm_e2e_ready_cached(cached_info).and_then(|info| {
            if request.generation != info.generation {
                return Err(libc::EAGAIN);
            }
            let payload = request.payload_of_request(received).ok_or(libc::EINVAL)?;
            let fua = u64::from(request.flags) & COMMERCIAL_MAX_STORAGED_BLOCK_FLAG_FUA != 0;
            block::write_with_info(
                info,
                request.generation,
                request.lba,
                request.block_count,
                payload,
                fua,
            )?;
            response.value0 = request.generation;
            response.value1 = request.payload_len as u64;
            Ok(())
        }) {
            Ok(()) => {
                note_dvm_request_ready();
                0
            }
            Err(errno) => {
                log_dvm_request_rejection("dispatch", request.header.op, errno);
                errno
            }
        }
    };
    let reply = syscall3(
        SYS_RUSTOS_IPC_REPLY,
        reply_cap,
        (&response as *const CommercialMaxProtocolResponse) as u64,
        size_of::<CommercialMaxProtocolResponse>() as u64,
    );
    if reply < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "storaged: bulk write reply failed errno={}",
            -reply
        );
    }
}

fn reply_commercial_error(reply_cap: u64, request: &CommercialMaxProtocolRequest, status: i32) {
    let mut response = CommercialMaxProtocolResponse {
        header: request.header,
        status,
        ..CommercialMaxProtocolResponse::default()
    };
    response.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    let reply = syscall3(
        SYS_RUSTOS_IPC_REPLY,
        reply_cap,
        (&response as *const CommercialMaxProtocolResponse) as u64,
        size_of::<CommercialMaxProtocolResponse>() as u64,
    );
    if reply < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "storaged: reply failed op={} reply_cap={} errno={}",
            request.header.op,
            reply_cap,
            -reply
        );
    }
}

fn reply_commercial_request(
    reply_cap: u64,
    request: &CommercialMaxProtocolRequest,
    bulk_slot: &mut BulkReplySlot,
    cached_info: &mut Option<(u64, block::BlockInfo)>,
) -> Option<(u64, usize)> {
    if request.header.op == COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ_BULK {
        return reply_bulk_read(reply_cap, request, bulk_slot, cached_info);
    }
    let mut response = CommercialMaxProtocolResponse {
        header: request.header,
        ..CommercialMaxProtocolResponse::default()
    };
    response.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    response.status = match validate_commercial_request(request) {
        Err(errno) => {
            let _ = writeln!(
                std::io::stderr(),
                "storaged: request rejected stage=envelope op={} errno={errno}",
                request.header.op
            );
            errno
        }
        Ok(()) => match dvm_e2e_ready_cached(cached_info)
            .and_then(|info| dispatch_commercial(request, &mut response, info))
        {
            Ok(()) => {
                note_dvm_request_ready();
                0
            }
            Err(errno) => {
                log_dvm_request_rejection("dispatch", request.header.op, errno);
                errno
            }
        },
    };
    let reply = syscall3(
        SYS_RUSTOS_IPC_REPLY,
        reply_cap,
        (&response as *const CommercialMaxProtocolResponse) as u64,
        size_of::<CommercialMaxProtocolResponse>() as u64,
    );
    if reply < 0 {
        let _ = writeln!(std::io::stderr(), "storaged: reply failed errno={}", -reply);
    }
    None
}

fn reply_bulk_read(
    reply_cap: u64,
    request: &CommercialMaxProtocolRequest,
    slot: &mut BulkReplySlot,
    cached_info: &mut Option<(u64, block::BlockInfo)>,
) -> Option<(u64, usize)> {
    let response = &mut *slot.response;
    response.header = request.header;
    response.status = 0;
    response.payload_len = 0;
    response.generation = request.arg0;
    response.lba = request.arg1;
    response.block_count = request.arg2;
    response.reserved0 = 0;

    let status = match validate_commercial_request(request) {
        Err(errno) => {
            let _ = writeln!(
                std::io::stderr(),
                "storaged: request rejected stage=envelope op={} errno={errno}",
                request.header.op
            );
            errno
        }
        Ok(()) => match dvm_e2e_ready_cached(cached_info).and_then(|info| {
            require_request_generation(request, info)?;
            block::read_into_with_info(
                info,
                request.arg0,
                request.arg1,
                request.arg2,
                &mut response.payload,
            )
        }) {
            Ok(written) => {
                response.payload_len = written as u32;
                note_dvm_request_ready();
                0
            }
            Err(errno) => {
                log_dvm_request_rejection("dispatch", request.header.op, errno);
                errno
            }
        },
    };
    response.status = status;
    // Header plus exactly the bytes read; an error reply is the header alone.
    // The slot's stale bytes past `payload_len` are never sent.
    let reply_len = response.reply_len().unwrap_or_else(|| {
        // Unreachable: the engine bounds `written` by the payload capacity.
        response.payload_len = 0;
        response.status = libc::EIO;
        STORAGED_BULK_READ_RESPONSE_HEADER_BYTES
    });
    if !block::needs_read_ahead_top_up() {
        // Fast path: fuse this reply with the next receive in one
        // `SYS_RUSTOS_IPC_REPLY_RECV_WITH_SENDER` syscall.
        return Some((reply_cap, reply_len));
    }
    let reply = syscall3(
        SYS_RUSTOS_IPC_REPLY,
        reply_cap,
        (response as *const StoragedBulkReadResponse) as u64,
        reply_len as u64,
    );
    if reply < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "storaged: bulk reply failed errno={}",
            -reply
        );
    }
    // Stream read-ahead runs after the reply so its submit syscalls overlap the
    // requester's work on its next request.
    block::service_read_ahead();
    None
}

fn dispatch_commercial(
    request: &CommercialMaxProtocolRequest,
    response: &mut CommercialMaxProtocolResponse,
    info: block::BlockInfo,
) -> Result<(), i32> {
    match request.header.op {
        COMMERCIAL_MAX_STORAGED_OP_BLOCK_INVENTORY | COMMERCIAL_MAX_STORAGED_OP_PARTITION_SCAN => {
            let descriptors = list_descriptors(info)?;
            response.value0 = descriptors.len() as u64;
            fill_storage_descriptors(&descriptors, request.header.op, response);
            Ok(())
        }
        COMMERCIAL_MAX_STORAGED_OP_ROOT_VOLUME_SELECT
        | COMMERCIAL_MAX_STORAGED_OP_VOLUME_METADATA => {
            let descriptor = selected_root_descriptor(info)?;
            response.value0 = descriptor.id as u64;
            response.descriptor_count = 1;
            response.descriptors[0] = storage_descriptor(&descriptor, request.header.op);
            response.capability = storage_capability(&descriptor);
            response.payload_len = write_payload_struct(&descriptor, &mut response.payload);
            Ok(())
        }
        COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_INFO => {
            response.value0 = info.generation;
            response.value1 = info.capacity_sectors;
            let wire = DvmBlockInfoWire {
                generation: info.generation,
                capacity_sectors: info.capacity_sectors,
                features: info.features,
                logical_block_size: info.logical_block_size,
                physical_block_size: info.physical_block_size,
                flags: info.flags,
                reserved0: 0,
            };
            response.payload_len = write_payload_struct(&wire, &mut response.payload);
            Ok(())
        }
        COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ => {
            require_request_generation(request, info)?;
            let written = block::read_into_with_info(
                info,
                request.arg0,
                request.arg1,
                request.arg2,
                &mut response.payload,
            )?;
            response.value0 = request.arg0;
            response.value1 = written as u64;
            response.payload_len = written as u32;
            Ok(())
        }
        COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_WRITE => {
            require_request_generation(request, info)?;
            if info.flags & rustos_user_abi::syscall::BLOCK_BROKER_INFO_FLAG_READ_ONLY != 0 {
                return Err(libc::EROFS);
            }
            let payload = request
                .payload
                .get(..request.payload_len as usize)
                .ok_or(libc::EINVAL)?;
            let fua = request.arg3 & COMMERCIAL_MAX_STORAGED_BLOCK_FLAG_FUA != 0;
            block::write_with_info(info, request.arg0, request.arg1, request.arg2, payload, fua)?;
            response.value0 = request.arg0;
            response.value1 = request.payload_len as u64;
            Ok(())
        }
        COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_FLUSH => {
            require_request_generation(request, info)?;
            block::flush_with_info(info, request.arg0)?;
            response.value0 = request.arg0;
            Ok(())
        }
        _ => Err(libc::EINVAL),
    }
}

fn require_request_generation(
    request: &CommercialMaxProtocolRequest,
    info: block::BlockInfo,
) -> Result<(), i32> {
    if request.arg0 == info.generation {
        Ok(())
    } else {
        Err(libc::EAGAIN)
    }
}

fn selected_root_descriptor(info: block::BlockInfo) -> Result<StorageBlockDescriptorWire, i32> {
    list_descriptors(info).map(|[descriptor]| descriptor)
}

fn list_descriptors(info: block::BlockInfo) -> Result<[StorageBlockDescriptorWire; 1], i32> {
    let sectors_per_block = u64::from(info.logical_block_size / 512);
    if sectors_per_block == 0 || !info.capacity_sectors.is_multiple_of(sectors_per_block) {
        return Err(libc::EIO);
    }
    let path = b"/dev/dvm-block0";
    let mut descriptor = StorageBlockDescriptorWire {
        id: 1,
        transport: STORAGE_TRANSPORT_DVM_BLOCK,
        flags: if info.flags & rustos_user_abi::syscall::BLOCK_BROKER_INFO_FLAG_READ_ONLY != 0 {
            STORAGE_FLAG_READONLY
        } else {
            0
        },
        logical_block_size: info.logical_block_size,
        start_block: 0,
        block_count: info.capacity_sectors / sectors_per_block,
        path_len: path.len() as u32,
        reserved0: 0,
        ..StorageBlockDescriptorWire::default()
    };
    descriptor.path[..path.len()].copy_from_slice(path);
    Ok([descriptor])
}

fn validate_commercial_request(request: &CommercialMaxProtocolRequest) -> Result<(), i32> {
    if !request.has_valid_envelope() || request.header.protocol != COMMERCIAL_MAX_PROTOCOL_STORAGED
    {
        return Err(libc::EINVAL);
    }
    match request.header.op {
        COMMERCIAL_MAX_STORAGED_OP_BLOCK_INVENTORY
        | COMMERCIAL_MAX_STORAGED_OP_PARTITION_SCAN
        | COMMERCIAL_MAX_STORAGED_OP_ROOT_VOLUME_SELECT
        | COMMERCIAL_MAX_STORAGED_OP_VOLUME_METADATA
            if request.path_len == 0
                && request.payload_len == 0
                && request.arg0 == 0
                && request.arg1 == 0
                && request.arg2 == 0
                && request.arg3 == 0 =>
        {
            Ok(())
        }
        COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_INFO
            if request.path_len == 0
                && request.payload_len == 0
                && request.arg0 == 0
                && request.arg1 == 0
                && request.arg2 == 0
                && request.arg3 == 0 =>
        {
            Ok(())
        }
        COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ
        | COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ_BULK
            if request.path_len == 0
                && request.payload_len == 0
                && request.arg0 != 0
                && request.arg2 != 0
                && request.arg3 == 0 =>
        {
            Ok(())
        }
        COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_WRITE
            if request.path_len == 0
                && request.payload_len != 0
                && request.arg0 != 0
                && request.arg2 != 0
                && request.arg3 & !COMMERCIAL_MAX_STORAGED_BLOCK_FLAG_FUA == 0 =>
        {
            Ok(())
        }
        COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_FLUSH
            if request.path_len == 0
                && request.payload_len == 0
                && request.arg0 != 0
                && request.arg1 == 0
                && request.arg2 == 0
                && request.arg3 == 0 =>
        {
            Ok(())
        }
        _ => Err(libc::EINVAL),
    }
}

fn fill_storage_descriptors(
    descriptors: &[StorageBlockDescriptorWire],
    op: u16,
    response: &mut CommercialMaxProtocolResponse,
) {
    let count = descriptors
        .len()
        .min(COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS);
    response.descriptor_count = count as u16;
    for (index, descriptor) in descriptors.iter().take(count).enumerate() {
        response.descriptors[index] = storage_descriptor(descriptor, op);
    }
}

fn storage_descriptor(
    descriptor: &StorageBlockDescriptorWire,
    op: u16,
) -> CommercialMaxProtocolDescriptorWire {
    let mut wire = CommercialMaxProtocolDescriptorWire {
        protocol: COMMERCIAL_MAX_PROTOCOL_STORAGED,
        op,
        flags: descriptor.flags,
        service_id: descriptor.id as u64,
        value0: descriptor.block_count,
        value1: descriptor.start_block,
        ..CommercialMaxProtocolDescriptorWire::default()
    };
    let name_len = (descriptor.path_len as usize)
        .min(descriptor.path.len())
        .min(wire.name.len());
    wire.name_len = name_len as u16;
    wire.name[..name_len].copy_from_slice(&descriptor.path[..name_len]);
    wire
}

fn storage_capability(descriptor: &StorageBlockDescriptorWire) -> CommercialMaxCapabilityLeaseWire {
    let mut wire = CommercialMaxCapabilityLeaseWire {
        lease_id: descriptor.id as u64,
        service_id: IPC_SERVICE_STORAGED,
        generation: descriptor.id as u64,
        ..CommercialMaxCapabilityLeaseWire::default()
    };
    let label_len = (descriptor.path_len as usize)
        .min(descriptor.path.len())
        .min(wire.label.len());
    wire.label_len = label_len as u16;
    wire.label[..label_len].copy_from_slice(&descriptor.path[..label_len]);
    wire
}

fn write_payload_struct<T>(value: &T, dest: &mut [u8]) -> u32 {
    let bytes = unsafe { slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) };
    let count = bytes.len().min(dest.len());
    dest[..count].copy_from_slice(&bytes[..count]);
    count as u32
}

fn syscall3(number: u64, arg0: u64, arg1: u64, arg2: u64) -> i64 {
    unsafe { rustos_svc_runtime::syscall::syscall3(number, arg0, arg1, arg2) }
}

fn syscall6(number: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> i64 {
    unsafe { rustos_svc_runtime::syscall::syscall6(number, arg0, arg1, arg2, arg3, arg4, arg5) }
}

fn debug_line(message: &str) {
    let bytes = message.as_bytes();
    let len = bytes.len().min(1023);
    let mut line = [0_u8; 1024];
    line[..len].copy_from_slice(&bytes[..len]);
    line[len] = b'\n';
    rustos_svc_runtime::ipc::debug_bytes(&line[..len + 1]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(op: u16) -> CommercialMaxProtocolRequest {
        let mut request = CommercialMaxProtocolRequest::default();
        request.header.protocol = COMMERCIAL_MAX_PROTOCOL_STORAGED;
        request.header.op = op;
        request
    }

    #[test]
    fn commercial_storage_operations_reject_unconsumed_fields() {
        let mut inventory = request(COMMERCIAL_MAX_STORAGED_OP_BLOCK_INVENTORY);
        assert_eq!(validate_commercial_request(&inventory), Ok(()));
        inventory.path_len = 1;
        inventory.path[0] = b'x';
        assert_eq!(validate_commercial_request(&inventory), Err(libc::EINVAL));

        let mut info = request(COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_INFO);
        info.arg0 = 1;
        assert_eq!(validate_commercial_request(&info), Err(libc::EINVAL));

        let mut bulk = request(COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ_BULK);
        bulk.arg0 = 7;
        bulk.arg1 = 11;
        bulk.arg2 = 15;
        assert_eq!(validate_commercial_request(&bulk), Ok(()));
        bulk.payload_len = 1;
        assert_eq!(validate_commercial_request(&bulk), Err(libc::EINVAL));
    }

    #[test]
    fn bulk_read_reuses_read_authority_instead_of_minting_a_new_right() {
        // Authority is the service's single typed capability; an op carries no
        // right of its own. The bulk variant is therefore the same request
        // class as a plain read: identical admission and refusal.
        for (arg0, arg2, arg3, payload_len) in [
            (7, 15, 0, 0),
            (0, 15, 0, 0),
            (7, 0, 0, 0),
            (7, 15, 1, 0),
            (7, 15, 0, 1),
        ] {
            let shape = |op| {
                let mut r = request(op);
                r.arg0 = arg0;
                r.arg1 = 11;
                r.arg2 = arg2;
                r.arg3 = arg3;
                r.payload_len = payload_len;
                validate_commercial_request(&r)
            };
            assert_eq!(
                shape(COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ_BULK),
                shape(COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ)
            );
        }
    }

    #[test]
    fn dvm_block_e2e_marker_names_the_complete_authority_path() {
        assert_eq!(
            dvm_block_e2e_marker(7),
            "storaged: dvm-block e2e media barrier completed generation=7 \
             path=vfs-policy->block-broker->shared-ring->linux-dvm->media-barrier"
        );
    }

    #[test]
    fn storage_requests_require_the_exact_proven_generation() {
        assert!(e2e_generation_matches(7, 7));
        assert!(!e2e_generation_matches(0, 7));
        assert!(!e2e_generation_matches(7, 8));
    }

    #[test]
    fn readiness_failure_invalidates_local_and_published_proof() {
        let published = AtomicU64::new(7);
        let mut proven = 7;
        invalidate_dvm_readiness(&published, &mut proven);
        assert_eq!(published.load(Ordering::Acquire), 0);
        // The next observation of generation 7 must take the barrier branch.
        assert_ne!(proven, 7);
    }

    #[test]
    fn stale_io_generation_is_rejected_before_a_dvm_submission() {
        let mut read = request(COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ);
        read.arg0 = 7;
        let live = block::BlockInfo {
            generation: 8,
            capacity_sectors: 1024,
            logical_block_size: 512,
            physical_block_size: 512,
            features: 0,
            flags: 0,
        };
        assert_eq!(require_request_generation(&read, live), Err(libc::EAGAIN));
        read.arg0 = live.generation;
        assert_eq!(require_request_generation(&read, live), Ok(()));
    }

    #[test]
    fn only_expected_readiness_absence_is_rate_limited() {
        assert!(transient_dvm_not_ready(libc::EAGAIN));
        assert!(transient_dvm_not_ready(libc::ENODEV));
        assert!(transient_dvm_not_ready(libc::ENOSYS));
        assert!(!transient_dvm_not_ready(libc::EIO));
        assert!(!transient_dvm_not_ready(libc::EACCES));
        assert_ne!(readiness_errno_witness(libc::ENODEV), 0);
        assert_eq!(
            readiness_errno_witness(libc::ENODEV),
            readiness_errno_witness(libc::ENODEV)
        );
        assert_ne!(
            readiness_errno_witness(libc::ENODEV),
            readiness_errno_witness(libc::EIO)
        );
    }

    #[test]
    fn read_only_device_rejects_write_dispatch_with_erofs_and_marks_descriptor_readonly() {
        let ro_info = block::BlockInfo {
            generation: 7,
            capacity_sectors: 1024,
            logical_block_size: 512,
            physical_block_size: 512,
            features: 0,
            flags: rustos_user_abi::syscall::BLOCK_BROKER_INFO_FLAG_READ_ONLY,
        };

        let descriptors = list_descriptors(ro_info).expect("ro descriptors");
        assert_eq!(descriptors.len(), 1);
        assert_ne!(descriptors[0].flags & STORAGE_FLAG_READONLY, 0);

        let root = selected_root_descriptor(ro_info).expect("ro root descriptor");
        assert_ne!(root.flags & STORAGE_FLAG_READONLY, 0);

        let mut inv_resp = CommercialMaxProtocolResponse::default();
        dispatch_commercial(
            &request(COMMERCIAL_MAX_STORAGED_OP_BLOCK_INVENTORY),
            &mut inv_resp,
            ro_info,
        )
        .expect("inventory succeeds on ro device");
        assert_eq!(inv_resp.descriptor_count, 1);
        assert_ne!(inv_resp.descriptors[0].flags & STORAGE_FLAG_READONLY, 0);

        let mut write_req = request(COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_WRITE);
        write_req.arg0 = ro_info.generation;
        write_req.arg1 = 0;
        write_req.arg2 = 1;
        write_req.payload_len = 512;
        assert_eq!(validate_commercial_request(&write_req), Ok(()));

        let mut write_resp = CommercialMaxProtocolResponse::default();
        assert_eq!(
            dispatch_commercial(&write_req, &mut write_resp, ro_info),
            Err(libc::EROFS)
        );
    }

    #[test]
    fn proven_block_info_cache_tracks_sequence_and_invalidates_cleanly() {
        let sample = block::BlockInfo {
            generation: 11,
            capacity_sectors: 4096,
            logical_block_size: 512,
            physical_block_size: 512,
            features: 0,
            flags: 0,
        };
        if let Ok(mut slot) = PROVEN_BLOCK_INFO.write() {
            *slot = Some(sample);
        }
        PROVEN_BLOCK_INFO_SEQ.fetch_add(1, Ordering::Release);
        DVM_E2E_READY_GENERATION.store(sample.generation, Ordering::Release);

        let mut cache = None;
        assert_eq!(dvm_e2e_ready_cached(&mut cache), Ok(sample));
        assert!(cache.is_some());
        // Second call serves directly from the thread-local cache.
        assert_eq!(dvm_e2e_ready_cached(&mut cache), Ok(sample));

        let mut proven = sample.generation;
        invalidate_dvm_readiness(&DVM_E2E_READY_GENERATION, &mut proven);
        assert_eq!(proven, 0);
        // After invalidation the stale local cache entry must never be returned.
        let seq1 = PROVEN_BLOCK_INFO_SEQ.load(Ordering::Acquire);
        assert_ne!(cache.unwrap().0, seq1);
        assert_eq!(DVM_E2E_READY_GENERATION.load(Ordering::Acquire), 0);
    }

    #[test]
    fn writable_device_omits_readonly_flag_and_permits_write_validation() {
        let rw_info = block::BlockInfo {
            generation: 7,
            capacity_sectors: 1024,
            logical_block_size: 512,
            physical_block_size: 512,
            features: 0,
            flags: 0,
        };

        let descriptors = list_descriptors(rw_info).expect("rw descriptors");
        assert_eq!(descriptors.len(), 1);
        assert_eq!(descriptors[0].flags & STORAGE_FLAG_READONLY, 0);

        let root = selected_root_descriptor(rw_info).expect("rw root descriptor");
        assert_eq!(root.flags & STORAGE_FLAG_READONLY, 0);

        let mut root_resp = CommercialMaxProtocolResponse::default();
        dispatch_commercial(
            &request(COMMERCIAL_MAX_STORAGED_OP_ROOT_VOLUME_SELECT),
            &mut root_resp,
            rw_info,
        )
        .expect("root select succeeds on rw device");
        assert_eq!(root_resp.descriptor_count, 1);
        assert_eq!(root_resp.descriptors[0].flags & STORAGE_FLAG_READONLY, 0);

        let mut write_req = request(COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_WRITE);
        write_req.arg0 = rw_info.generation;
        write_req.arg1 = 0;
        write_req.arg2 = 2;
        write_req.arg3 = COMMERCIAL_MAX_STORAGED_BLOCK_FLAG_FUA;
        write_req.payload_len = 1024;
        assert_eq!(validate_commercial_request(&write_req), Ok(()));
    }
}

// Native ABI revision stamp (hint, not authority); see docs/architecture/evolution-rules.md.
rustos_user_abi::native_abi_note!();
