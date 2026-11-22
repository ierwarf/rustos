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
    CommercialMaxProtocolRequest, CommercialMaxProtocolResponse, DvmBlockInfoWire,
    StorageBlockDescriptorWire, StoragedBulkReadResponse, StoragedBulkWriteRequest,
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION, COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS,
    COMMERCIAL_MAX_PROTOCOL_STORAGED, COMMERCIAL_MAX_STORAGED_BLOCK_FLAG_FUA,
    COMMERCIAL_MAX_STORAGED_OP_BLOCK_INVENTORY, COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_FLUSH,
    COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_INFO, COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ,
    COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ_BULK, COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_WRITE,
    COMMERCIAL_MAX_STORAGED_OP_PARTITION_SCAN, COMMERCIAL_MAX_STORAGED_OP_ROOT_VOLUME_SELECT,
    COMMERCIAL_MAX_STORAGED_OP_VOLUME_METADATA, IPC_MAX_INLINE_BYTES, IPC_SERVICE_STORAGED,
    IPC_SERVICE_VFSD, PRODUCT_MILESTONE_STORAGE_READY, STORAGED_BULK_READ_PAYLOAD_CAPACITY,
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
static PROVEN_BLOCK_INFO: std::sync::Mutex<Option<block::BlockInfo>> = std::sync::Mutex::new(None);
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
                    if let Ok(mut slot) = PROVEN_BLOCK_INFO.lock() {
                        *slot = Some(info);
                    }
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
    if let Ok(mut slot) = PROVEN_BLOCK_INFO.lock() {
        *slot = None;
    }
    // Recovery of the same generation must run the barrier and republish it.
    *proven_generation = 0;
}

fn dvm_e2e_ready_for_current_generation() -> Result<block::BlockInfo, i32> {
    let proven = DVM_E2E_READY_GENERATION.load(Ordering::Acquire);
    if proven != 0 {
        if let Ok(slot) = PROVEN_BLOCK_INFO.lock() {
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
        if let Ok(mut slot) = PROVEN_BLOCK_INFO.lock() {
            *slot = Some(info);
        }
        Ok(info)
    } else {
        Err(libc::EAGAIN)
    }
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

/// One receive thread's bulk-read reply slot. The payload is kept zero beyond
/// the last reply's length without clearing all of it on every reply.
struct BulkReplySlot {
    response: Box<StoragedBulkReadResponse>,
    /// Bytes of `response.payload` that may be non-zero.
    dirty: usize,
}

impl BulkReplySlot {
    fn new() -> Self {
        Self {
            response: Box::new(StoragedBulkReadResponse::zeroed()),
            dirty: STORAGED_BULK_READ_PAYLOAD_CAPACITY,
        }
    }
}

fn serve(endpoint: u64) {
    let mut bulk_slot = BulkReplySlot::new();
    let mut recv_words = vec![0_u64; IPC_MAX_INLINE_BYTES.div_ceil(size_of::<u64>())];
    loop {
        let mut reply_cap = 0_u64;
        let mut sender_pid = 0_u64;
        let mut sender_tid = 0_u64;
        let received = syscall6(
            SYS_RUSTOS_IPC_RECV_WITH_SENDER,
            endpoint,
            recv_words.as_mut_ptr() as u64,
            IPC_MAX_INLINE_BYTES as u64,
            (&mut reply_cap as *mut u64) as u64,
            (&mut sender_pid as *mut u64) as u64,
            (&mut sender_tid as *mut u64) as u64,
        );
        if received < 0 {
            thread::sleep(RECV_BACKOFF);
            continue;
        }
        if received as usize == size_of::<StoragedBulkWriteRequest>() {
            // SAFETY: `recv_words` is 8-aligned and has at least `IPC_MAX_INLINE_BYTES`
            // bytes (`== size_of::<StoragedBulkWriteRequest>()`).
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
                reply_bulk_write_request(reply_cap, bulk_req);
            }
            continue;
        }
        if received as usize != size_of::<CommercialMaxProtocolRequest>() {
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
        reply_commercial_request(reply_cap, request, &mut bulk_slot);
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

fn reply_bulk_write_request(reply_cap: u64, request: &StoragedBulkWriteRequest) {
    let mut response = CommercialMaxProtocolResponse {
        header: request.header,
        ..CommercialMaxProtocolResponse::default()
    };
    response.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    response.status = if !request.has_valid_shape() {
        libc::EINVAL
    } else {
        match dvm_e2e_ready_for_current_generation().and_then(|info| {
            if request.generation != info.generation {
                return Err(libc::EAGAIN);
            }
            let payload = request
                .payload
                .get(..request.payload_len as usize)
                .ok_or(libc::EINVAL)?;
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
) {
    if request.header.op == COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ_BULK {
        reply_bulk_read(reply_cap, request, bulk_slot);
        return;
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
        Ok(()) => match dvm_e2e_ready_for_current_generation()
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
}

/// Keeps the reused bulk-read slot zero beyond the reply's length without
/// clearing all of it on every reply. After a success only the bytes a previous
/// reply left past this length are cleared; after a failure the partially
/// filled extent is unknown, so all of it is cleared.
fn settle_bulk_payload_tail(payload: &mut [u8], dirty: &mut usize, ok: bool, written_len: usize) {
    if ok {
        let prev = core::mem::replace(dirty, written_len).min(payload.len());
        if prev > written_len {
            payload[written_len..prev].fill(0);
        }
    } else {
        *dirty = 0;
        payload.fill(0);
    }
}

fn reply_bulk_read(
    reply_cap: u64,
    request: &CommercialMaxProtocolRequest,
    slot: &mut BulkReplySlot,
) {
    let response = &mut *slot.response;
    response.header = request.header;
    response.status = 0;
    response.payload_len = 0;
    response.generation = request.arg0;
    response.lba = request.arg1;
    response.block_count = request.arg2;
    response.reserved0 = 0;

    let mut written_len = 0_usize;
    let status = match validate_commercial_request(request) {
        Err(errno) => {
            let _ = writeln!(
                std::io::stderr(),
                "storaged: request rejected stage=envelope op={} errno={errno}",
                request.header.op
            );
            errno
        }
        Ok(()) => match dvm_e2e_ready_for_current_generation().and_then(|info| {
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
                written_len = written;
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
    settle_bulk_payload_tail(
        &mut response.payload,
        &mut slot.dirty,
        status == 0,
        written_len,
    );
    let reply = syscall3(
        SYS_RUSTOS_IPC_REPLY,
        reply_cap,
        (response as *const StoragedBulkReadResponse) as u64,
        size_of::<StoragedBulkReadResponse>() as u64,
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
    let descriptors = list_descriptors(info)?;
    descriptors
        .iter()
        .copied()
        .min_by_key(root_selection_rank)
        .ok_or(libc::ENODEV)
}

fn root_selection_rank(descriptor: &StorageBlockDescriptorWire) -> (u8, u8, u32) {
    let partition_rank = u8::from(descriptor.start_block == 0);
    let readonly_rank = u8::from((descriptor.flags & STORAGE_FLAG_READONLY) != 0);
    (partition_rank, readonly_rank, descriptor.id)
}

fn list_descriptors(info: block::BlockInfo) -> Result<Vec<StorageBlockDescriptorWire>, i32> {
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
    Ok(vec![descriptor])
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

    #[test]
    fn bulk_payload_tail_is_zero_after_shorter_and_failed_replies() {
        let mut dirty = 8_usize;
        let mut slot = [0xEE_u8; 8];
        settle_bulk_payload_tail(&mut slot, &mut dirty, true, 3);
        assert_eq!(slot, [0xEE, 0xEE, 0xEE, 0, 0, 0, 0, 0]);
        slot[..6].fill(0xAA);
        settle_bulk_payload_tail(&mut slot, &mut dirty, true, 6);
        assert_eq!(slot[3..], [0xAA, 0xAA, 0xAA, 0, 0]);
        // A failed read may leave bytes anywhere; the whole slot is cleared.
        slot.fill(0x77);
        settle_bulk_payload_tail(&mut slot, &mut dirty, false, 0);
        assert_eq!(slot, [0; 8]);
        assert_eq!(dirty, 0);
    }
}

// Native ABI revision stamp (hint, not authority); see docs/architecture/evolution-rules.md.
rustos_user_abi::native_abi_note!();
