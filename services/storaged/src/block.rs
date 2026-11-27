//! Storage-provider generation, geometry, cache, and durability policy.
//!
//! - **Owner:** `storaged` owns block timeout/retry/cache/durability policy;
//!   Compat and `kernel-io-manager` expose bounded transport.
//! - **Boundary:** DVM readiness, signed geometry, completions, operation IDs,
//!   and client ranges are untrusted.
//! - **Lifecycle:** Admit provider epoch, validate/submit, settle exact ticket,
//!   cache only complete reads, flush durable mutations, invalidate on revoke,
//!   and rebind a newer signed epoch.
//! - **Concurrency:** Queue/cache state is bounded and never held across a
//!   blocking broker call.
//! - **Failure:** Timeout, cancel, device fault, short/stale completion,
//!   restart, and geometry change reject or invalidate before reuse.
//! - **Forbidden:** No fabricated readiness, unsigned rebind, cache across
//!   generation, native driver, or false FUA/flush success.
//! - **Evidence:** `dvm-block-startup`, `dvm-read-cache`,
//!   `dvm-volume-io`, and `durable-block-mutation`.
use std::mem::size_of;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::read_engine::{Engine, Geometry, PumpScratch, Transport, COMPLETION_TIMEOUT};
use rustos_user_abi::syscall::{
    DvmBlockInfoWire, DvmBlockTicketWire, RustosBlockBrokerArgs, BLOCK_BROKER_ABI_VERSION,
    BLOCK_BROKER_FLAG_FUA, BLOCK_BROKER_INFO_FLAG_READ_ONLY, BLOCK_BROKER_OP_DVM_CANCEL,
    BLOCK_BROKER_OP_DVM_COLLECT, BLOCK_BROKER_OP_DVM_INFO, BLOCK_BROKER_OP_DVM_SUBMIT_FLUSH,
    BLOCK_BROKER_OP_DVM_SUBMIT_READ, BLOCK_BROKER_OP_DVM_SUBMIT_WRITE, BLOCK_BROKER_OP_DVM_WAIT,
    SYS_RUSTOS_BLOCK_BROKER,
};

const WAIT_SLICE_MILLIS: u64 = 1_000;
/// Short safety backstop when a bounded pump pass stopped early after
/// collecting a ready prefix: normal in-order completions wake via IRQ almost
/// immediately, while a rare out-of-order tail completion is caught by a full
/// sweep if no further IRQ arrives within this slice.
const TRUNCATED_WAIT_SLICE_MILLIS: u64 = 2;
// Windowed read cache, in-flight tracking and stream read-ahead live in
// `read_engine`; this module is its DVM transport and the write/flush policy.
static ENGINE: Engine = Engine::new();
// A timeout does not cancel Linux I/O. Never re-admit the timed-out
// generation after an uncertain operation; only a strictly newer DVM
// generation with proven quiescence may clear quarantine.
static QUARANTINED_GENERATION: AtomicU64 = AtomicU64::new(0);

fn require_unquarantined_for(quarantined_gen: &AtomicU64, generation: u64) -> Result<(), i32> {
    // ORDERING: Acquire pairs with the Release publication in `latch_quarantine`,
    // so a request admitted after the quarantine sees it.
    let q = quarantined_gen.load(Ordering::Acquire);
    if q != 0 && generation <= q {
        Err(libc::EIO)
    } else {
        Ok(())
    }
}

pub(super) fn clear_quarantine_for_new_generation(new_generation: u64) {
    // ORDERING: Acquire read of the quarantined generation published by
    // `quarantine_provider`.
    let q = QUARANTINED_GENERATION.load(Ordering::Acquire);
    if q != 0 && new_generation > q {
        // ORDERING: AcqRel CAS lifts exactly the generation read above; a
        // concurrent newer quarantine makes it fail and stays in force.
        let _ = QUARANTINED_GENERATION.compare_exchange(q, 0, Ordering::AcqRel, Ordering::Acquire);
    }
}

fn quarantine_provider(ticket: DvmBlockTicketWire, cause: &str) {
    latch_quarantine(&QUARANTINED_GENERATION, ticket.generation);
    clear_read_cache();
    eprintln!(
        "storaged: dvm provider quarantined generation={} request_id={} slot={} cause={cause} quiescence=unproven",
        ticket.generation, ticket.request_id, ticket.data_slot
    );
}

fn latch_quarantine(quarantined: &AtomicU64, generation: u64) {
    // ORDERING: Release publishes quarantine before cache invalidation. An
    // older completion timeout must never overwrite a newer quarantine and
    // accidentally readmit that newer, still-uncertain provider generation.
    quarantined.fetch_max(generation.max(1), Ordering::AcqRel);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct BlockInfo {
    pub generation: u64,
    pub capacity_sectors: u64,
    pub logical_block_size: u32,
    pub physical_block_size: u32,
    pub features: u64,
    pub flags: u32,
}

/// Read the current transport state without turning an ordinary not-ready
/// observation into a service-wide startup wait. The readiness supervisor owns
/// the bounded wait; request turns return `EAGAIN` to their caller instead.
pub(super) fn info() -> Result<BlockInfo, i32> {
    info_once()
}

/// Wait for asynchronous DVM bringup with the kernel's atomic
/// check-arm-recheck waiter. Only the singleton readiness supervisor uses this
/// path, never a caller-owned storage RPC turn.
pub(super) fn wait_until_ready(deadline: Instant) -> Result<BlockInfo, i32> {
    loop {
        match info_once() {
            Ok(info) => return Ok(info),
            Err(libc::EAGAIN) => wait_for_transport_event(deadline)?,
            Err(error) => return Err(error),
        }
    }
}

fn info_once() -> Result<BlockInfo, i32> {
    let mut wire = DvmBlockInfoWire::default();
    let args = RustosBlockBrokerArgs {
        abi_version: BLOCK_BROKER_ABI_VERSION,
        op: BLOCK_BROKER_OP_DVM_INFO,
        out_info_ptr: (&mut wire as *mut DvmBlockInfoWire) as u64,
        ..RustosBlockBrokerArgs::default()
    };
    syscall_block(&args)?;
    require_unquarantined_for(&QUARANTINED_GENERATION, wire.generation)?;
    if wire.generation == 0
        || wire.capacity_sectors == 0
        || wire.logical_block_size < 512
        || !wire.logical_block_size.is_power_of_two()
        || !wire.logical_block_size.is_multiple_of(512)
        || wire.physical_block_size < wire.logical_block_size
        || !wire
            .physical_block_size
            .is_multiple_of(wire.logical_block_size)
        || wire.reserved0 != 0
    {
        return Err(libc::EIO);
    }
    Ok(BlockInfo {
        generation: wire.generation,
        capacity_sectors: wire.capacity_sectors,
        logical_block_size: wire.logical_block_size,
        physical_block_size: wire.physical_block_size,
        features: wire.features,
        flags: wire.flags,
    })
}

/// Sleep through the asynchronous DVM startup window using the kernel's
/// atomic check-arm-recheck waiter.  A timeout is terminal for this request;
/// callers may retry the service operation, but no host/bootstrap storage
/// fallback is selected.
pub(super) fn wait_for_transport_event(deadline: Instant) -> Result<(), i32> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let timeout_ms = bounded_wait_timeout_ms(remaining).ok_or(libc::ETIMEDOUT)?;
    let wait = RustosBlockBrokerArgs {
        abi_version: BLOCK_BROKER_ABI_VERSION,
        op: BLOCK_BROKER_OP_DVM_WAIT,
        timeout_ms,
        ..RustosBlockBrokerArgs::default()
    };
    match syscall_block(&wait) {
        Ok(_) | Err(libc::EINTR) => Ok(()),
        Err(libc::ETIMEDOUT) if Instant::now() < deadline => Ok(()),
        Err(error) => Err(error),
    }
}

fn bounded_wait_timeout_ms(remaining: Duration) -> Option<u64> {
    if remaining.is_zero() {
        return None;
    }
    Some(
        u64::try_from(remaining.as_millis())
            .unwrap_or(u64::MAX)
            .clamp(1, WAIT_SLICE_MILLIS),
    )
}

/// The DVM block broker as the read engine's transport.
struct DvmTransport;

impl Transport for DvmTransport {
    fn submit_read(
        &self,
        generation: u64,
        lba: u64,
        block_count: u64,
        byte_len: usize,
    ) -> Result<DvmBlockTicketWire, i32> {
        submit(
            generation,
            BLOCK_BROKER_OP_DVM_SUBMIT_READ,
            lba,
            block_count,
            0,
            std::ptr::null(),
            byte_len,
        )
    }

    fn try_collect(&self, ticket: DvmBlockTicketWire, buf: &mut [u8]) -> Result<bool, i32> {
        try_collect(ticket, Some(buf))
    }

    fn cancel(&self, ticket: DvmBlockTicketWire) {
        cancel_best_effort(ticket);
    }

    fn timed_out(&self, ticket: DvmBlockTicketWire) {
        quarantine_provider(ticket, "completion-timeout");
        cancel_best_effort(ticket);
    }
}

fn read_geometry(info: BlockInfo) -> Option<Geometry> {
    let sectors_per_block = u64::from(info.logical_block_size / 512);
    Geometry::new(
        info.generation,
        info.logical_block_size as usize,
        info.capacity_sectors.checked_div(sectors_per_block)?,
    )
}

pub(super) fn read_into_with_info(
    info: BlockInfo,
    expected_generation: u64,
    lba: u64,
    block_count: u64,
    out: &mut [u8],
) -> Result<usize, i32> {
    require_generation(info, expected_generation).inspect_err(|errno| {
        clear_read_cache();
        eprintln!(
            "storaged: dvm read rejected stage=generation expected={} actual={} errno={errno}",
            expected_generation, info.generation
        );
    })?;
    let byte_len = checked_byte_len(info, lba, block_count).inspect_err(|errno| {
        eprintln!(
            "storaged: dvm read rejected stage=range lba={lba} blocks={block_count} logical={} capacity_sectors={} errno={errno}",
            info.logical_block_size, info.capacity_sectors
        );
    })?;
    if byte_len > out.len() {
        return Err(libc::EOVERFLOW);
    }
    // A cached window must not outlive a quarantine of its generation.
    require_unquarantined_for(&QUARANTINED_GENERATION, info.generation)?;
    let geometry = read_geometry(info).ok_or(libc::EINVAL)?;
    ENGINE
        .read(&DvmTransport, geometry, lba, block_count, out)
        .inspect_err(|errno| {
            eprintln!(
                "storaged: dvm read rejected stage=engine lba={lba} blocks={block_count} errno={errno}"
            );
        })
}

/// Whether any stream or freed in-flight capacity wants a read-ahead top-up.
pub(super) fn needs_read_ahead_top_up() -> bool {
    ENGINE.needs_top_up()
}

/// Refills stream read-ahead after a reply, so its submit syscalls overlap the
/// requester's work on its next request.
pub(super) fn service_read_ahead() {
    ENGINE.top_up(&DvmTransport);
}

/// Owns completion collection for the read engine: collects what the DVM has
/// finished, wakes the readers waiting on it, and otherwise sleeps in the
/// kernel until the transport signals a completion.

pub(super) fn run_read_pump() -> ! {
    let mut idle_wakes = 0_u32;
    let mut force_full_sweep = false;
    let mut scratch = PumpScratch::default();
    loop {
        let allow_early_stop = !force_full_sweep;
        force_full_sweep = false;
        let status = ENGINE.pump_once_bounded(&DvmTransport, &mut scratch, allow_early_stop);
        if status.progressed != 0 {
            idle_wakes = 0;
            // Refill read-ahead right after freeing in-flight slots so the DVM
            // pipeline stays saturated and receive threads can stay on the
            // fused REPLY_RECV fast path without stalling on top_up.
            ENGINE.top_up(&DvmTransport);
        }
        if status.in_flight == 0 {
            idle_wakes = 0;
            ENGINE.park_until_work(Duration::from_millis(WAIT_SLICE_MILLIS));
            continue;
        }
        if status.truncated && (status.has_waited || status.progressed == 0) {
            // A reader is still blocked on an in-flight window or a bounded
            // pass found nothing; sweep every in-flight ticket immediately.
            force_full_sweep = true;
            continue;
        }
        let slice_ms = if status.truncated {
            TRUNCATED_WAIT_SLICE_MILLIS
        } else {
            WAIT_SLICE_MILLIS
        };
        let woke = wait_for_transport_event(Instant::now() + Duration::from_millis(slice_ms));
        if status.truncated && woke == Err(libc::ETIMEDOUT) {
            // No new IRQ arrived during the short safety window after an
            // early-stopped pass; force one full sweep to catch any
            // out-of-order completion that was drained earlier.
            force_full_sweep = true;
            continue;
        }
        // A transport in a terminal state reports an event forever; do not spin.
        idle_wakes = if status.progressed == 0 && woke.is_ok() {
            idle_wakes.saturating_add(1)
        } else {
            0
        };
        if woke.is_err() || idle_wakes >= 64 {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

pub(super) fn write_with_info(
    info: BlockInfo,
    expected_generation: u64,
    lba: u64,
    block_count: u64,
    bytes: &[u8],
    fua: bool,
) -> Result<(), i32> {
    write_admitted_with(info, expected_generation, lba, block_count, bytes, || {
        let ticket = submit(
            expected_generation,
            BLOCK_BROKER_OP_DVM_SUBMIT_WRITE,
            lba,
            block_count,
            if fua { BLOCK_BROKER_FLAG_FUA } else { 0 },
            bytes.as_ptr(),
            bytes.len(),
        )?;
        // A write that times out or fails may still land later, and a read
        // between the pre-write clear and that landing would repopulate the
        // cache with old bytes. Drop the cache on error; on success only the
        // overwritten blocks need invalidation so FAT/dir reads stay cached.
        let result = wait_and_collect(ticket, None);
        if result.is_err() {
            // Neither a local timeout nor a transport error proves that the
            // submitted mutation did not reach the media. Do not replay it.
            quarantine_provider(ticket, "write-outcome-unknown");
            clear_read_cache();
        } else {
            invalidate_read_cache_range(expected_generation, lba, block_count);
        }
        result
    })
}

fn write_admitted_with(
    info: BlockInfo,
    expected_generation: u64,
    lba: u64,
    block_count: u64,
    bytes: &[u8],
    write: impl FnOnce() -> Result<(), i32>,
) -> Result<(), i32> {
    require_generation(info, expected_generation)?;
    if info.flags & BLOCK_BROKER_INFO_FLAG_READ_ONLY != 0 {
        return Err(libc::EROFS);
    }
    validate_write_extent(info, lba, block_count, bytes.len())?;
    invalidate_read_cache_range(info.generation, lba, block_count);
    write()
}

fn validate_write_extent(
    info: BlockInfo,
    lba: u64,
    block_count: u64,
    payload_len: usize,
) -> Result<(), i32> {
    if checked_byte_len(info, lba, block_count)? != payload_len {
        return Err(libc::EINVAL);
    }
    Ok(())
}

fn clear_read_cache() {
    ENGINE.clear();
}

fn invalidate_read_cache_range(generation: u64, lba: u64, block_count: u64) {
    ENGINE.invalidate_range(generation, lba, block_count);
}

pub(super) fn flush_with_info(info: BlockInfo, expected_generation: u64) -> Result<(), i32> {
    flush_with_info_before(
        info,
        expected_generation,
        Instant::now() + COMPLETION_TIMEOUT,
    )
}

pub(super) fn flush_before(expected_generation: u64, deadline: Instant) -> Result<(), i32> {
    flush_with_info_before(info()?, expected_generation, deadline)
}

fn flush_with_info_before(
    info: BlockInfo,
    expected_generation: u64,
    deadline: Instant,
) -> Result<(), i32> {
    require_generation(info, expected_generation)?;
    let mut ticket = DvmBlockTicketWire::default();
    let args = RustosBlockBrokerArgs {
        abi_version: BLOCK_BROKER_ABI_VERSION,
        op: BLOCK_BROKER_OP_DVM_SUBMIT_FLUSH,
        expected_generation,
        out_ticket_ptr: (&mut ticket as *mut DvmBlockTicketWire) as u64,
        ..RustosBlockBrokerArgs::default()
    };
    require_unquarantined_for(&QUARANTINED_GENERATION, expected_generation)?;
    syscall_block(&args)?;
    validate_ticket(ticket)?;
    wait_and_collect_before(ticket, None, deadline)
}

fn submit(
    expected_generation: u64,
    operation: u16,
    lba: u64,
    block_count: u64,
    flags: u32,
    buffer: *const u8,
    buffer_len: usize,
) -> Result<DvmBlockTicketWire, i32> {
    let mut ticket = DvmBlockTicketWire::default();
    let args = RustosBlockBrokerArgs {
        abi_version: BLOCK_BROKER_ABI_VERSION,
        op: operation,
        expected_generation,
        lba,
        block_count,
        buffer_ptr: buffer as u64,
        buffer_len: buffer_len as u64,
        flags,
        out_ticket_ptr: (&mut ticket as *mut DvmBlockTicketWire) as u64,
        ..RustosBlockBrokerArgs::default()
    };
    require_unquarantined_for(&QUARANTINED_GENERATION, expected_generation)?;
    syscall_block(&args)?;
    validate_ticket(ticket)?;
    Ok(ticket)
}

fn wait_and_collect(ticket: DvmBlockTicketWire, read_buffer: Option<&mut [u8]>) -> Result<(), i32> {
    wait_and_collect_before(ticket, read_buffer, Instant::now() + COMPLETION_TIMEOUT)
}

/// One non-blocking attempt to collect `ticket`: `Ok(true)` once it completed
/// and `read_buffer` holds its bytes, `Ok(false)` while it is still pending.
fn try_collect(ticket: DvmBlockTicketWire, read_buffer: Option<&mut [u8]>) -> Result<bool, i32> {
    let (buffer_ptr, buffer_len) = read_buffer.map_or((0, 0), |buffer| {
        (buffer.as_mut_ptr() as u64, buffer.len() as u64)
    });
    let collect = RustosBlockBrokerArgs {
        abi_version: BLOCK_BROKER_ABI_VERSION,
        op: BLOCK_BROKER_OP_DVM_COLLECT,
        buffer_ptr,
        buffer_len,
        ticket,
        ..RustosBlockBrokerArgs::default()
    };
    match syscall_block(&collect) {
        Ok(completed) if completed == buffer_len => Ok(true),
        Ok(_) => Err(libc::EIO),
        Err(libc::EAGAIN) => Ok(false),
        Err(error) => {
            eprintln!(
                "storaged: dvm completion rejected stage=collect request_id={} slot={} errno={error}",
                ticket.request_id, ticket.data_slot
            );
            Err(error)
        }
    }
}

fn wait_and_collect_before(
    ticket: DvmBlockTicketWire,
    mut read_buffer: Option<&mut [u8]>,
    deadline: Instant,
) -> Result<(), i32> {
    validate_ticket(ticket)?;
    loop {
        if try_collect(ticket, read_buffer.as_deref_mut())? {
            return Ok(());
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            quarantine_provider(ticket, "completion-timeout");
            cancel_best_effort(ticket);
            return Err(libc::ETIMEDOUT);
        }
        let Some(timeout_ms) = bounded_wait_timeout_ms(remaining) else {
            quarantine_provider(ticket, "completion-timeout");
            cancel_best_effort(ticket);
            return Err(libc::ETIMEDOUT);
        };
        let wait = RustosBlockBrokerArgs {
            abi_version: BLOCK_BROKER_ABI_VERSION,
            op: BLOCK_BROKER_OP_DVM_WAIT,
            timeout_ms,
            ..RustosBlockBrokerArgs::default()
        };
        match syscall_block(&wait) {
            Ok(_) | Err(libc::EINTR) => {}
            Err(libc::ETIMEDOUT) if Instant::now() < deadline => {}
            Err(error) => {
                if error == libc::ETIMEDOUT {
                    quarantine_provider(ticket, "completion-timeout");
                }
                eprintln!(
                    "storaged: dvm completion rejected stage=wait request_id={} slot={} timeout_ms={timeout_ms} errno={error}",
                    ticket.request_id, ticket.data_slot
                );
                cancel_best_effort(ticket);
                return Err(error);
            }
        }
    }
}

fn cancel_best_effort(ticket: DvmBlockTicketWire) {
    let args = RustosBlockBrokerArgs {
        abi_version: BLOCK_BROKER_ABI_VERSION,
        op: BLOCK_BROKER_OP_DVM_CANCEL,
        ticket,
        ..RustosBlockBrokerArgs::default()
    };
    let _ = syscall_block(&args);
}

fn require_generation(info: BlockInfo, expected: u64) -> Result<(), i32> {
    if expected == 0 || info.generation != expected {
        Err(libc::ESTALE)
    } else {
        Ok(())
    }
}

fn checked_byte_len(info: BlockInfo, lba: u64, block_count: u64) -> Result<usize, i32> {
    if block_count == 0 {
        return Err(libc::EINVAL);
    }
    let byte_len = block_count
        .checked_mul(u64::from(info.logical_block_size))
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value != 0)
        .ok_or(libc::EINVAL)?;
    let sectors_per_block = u64::from(info.logical_block_size / 512);
    let end_sector = lba
        .checked_add(block_count)
        .and_then(|end| end.checked_mul(sectors_per_block))
        .ok_or(libc::EINVAL)?;
    if end_sector > info.capacity_sectors {
        return Err(libc::EINVAL);
    }
    Ok(byte_len)
}

fn validate_ticket(ticket: DvmBlockTicketWire) -> Result<(), i32> {
    if ticket.generation == 0
        || ticket.request_id == 0
        || ticket.data_slot >= 64
        || ticket.reserved0 != 0
    {
        Err(libc::EIO)
    } else {
        Ok(())
    }
}

fn syscall_block(args: &RustosBlockBrokerArgs) -> Result<u64, i32> {
    debug_assert_eq!(
        size_of::<RustosBlockBrokerArgs>(),
        std::mem::size_of_val(args)
    );
    // SAFETY: `args` is borrowed for the whole call, so the pointer stays valid and
    // aligned; the kernel copies the struct by value and never retains the
    // pointer. Buffer/info pointers inside it are user addresses checked by the
    // kernel's user-copy path, so a bad one yields an errno, not a fault.
    let status = unsafe {
        rustos_svc_runtime::syscall::syscall1(
            SYS_RUSTOS_BLOCK_BROKER,
            (args as *const RustosBlockBrokerArgs) as u64,
        )
    };
    if status < 0 {
        Err((-status) as i32)
    } else {
        Ok(status as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests that reach the process-wide `ENGINE` (a valid write invalidates it)
    /// run one at a time so one test's invalidation cannot clear another's seed.
    static ENGINE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn engine_test_guard() -> std::sync::MutexGuard<'static, ()> {
        ENGINE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn quarantined_provider_rejects_admission() {
        let quarantined = AtomicU64::new(0);
        assert_eq!(require_unquarantined_for(&quarantined, 7), Ok(()));
        // ORDERING: Release pairs with Acquire in `require_unquarantined_for`.
        quarantined.store(7, Ordering::Release);
        assert_eq!(require_unquarantined_for(&quarantined, 7), Err(libc::EIO));
    }

    #[test]
    fn late_old_timeout_cannot_lower_a_newer_provider_quarantine() {
        let quarantined = AtomicU64::new(0);
        latch_quarantine(&quarantined, 9);
        latch_quarantine(&quarantined, 7);
        // ORDERING: Observe the final latch value as production readers do.
        // This single-threaded assertion does not require synchronization.
        assert_eq!(quarantined.load(Ordering::Acquire), 9);
        for generation in [0, 7, 8, 9] {
            assert_eq!(require_unquarantined_for(&quarantined, generation), Err(libc::EIO));
        }
        assert_eq!(require_unquarantined_for(&quarantined, 10), Ok(()));
    }

    fn info() -> BlockInfo {
        BlockInfo {
            generation: 7,
            capacity_sectors: 8192,
            logical_block_size: 4096,
            physical_block_size: 4096,
            features: 1,
            flags: 0,
        }
    }

    #[test]
    fn range_checks_use_logical_blocks_but_bind_sector_capacity() {
        assert_eq!(checked_byte_len(info(), 4, 2), Ok(8192));
        assert_eq!(checked_byte_len(info(), 1023, 2), Err(libc::EINVAL));
        assert_eq!(checked_byte_len(info(), u64::MAX, 1), Err(libc::EINVAL));
    }

    #[test]
    fn write_extent_requires_exact_declared_count_before_submission() {
        let info = BlockInfo {
            logical_block_size: 512,
            ..info()
        };
        assert_eq!(validate_write_extent(info, 0, 1, 1024), Err(libc::EINVAL));
        assert_eq!(validate_write_extent(info, 0, 2, 512), Err(libc::EINVAL));
        assert_eq!(
            validate_write_extent(info, 0, u64::MAX, 512),
            Err(libc::EINVAL)
        );
        assert_eq!(validate_write_extent(info, 0, 0, 0), Err(libc::EINVAL));
        assert_eq!(
            validate_write_extent(info, 8191, 2, 1024),
            Err(libc::EINVAL)
        );
        assert_eq!(validate_write_extent(info, 0, 2, 1024), Ok(()));
    }

    #[test]
    fn write_extent_refusal_has_no_cache_or_submission_side_effects() {
        let _engine = engine_test_guard();
        let info = BlockInfo {
            logical_block_size: 512,
            ..info()
        };
        let bytes = [0_u8; 1024];
        let mut side_effects = 0;
        for count in [0, 1, 3, u64::MAX] {
            assert_eq!(
                write_admitted_with(info, 7, 0, count, &bytes, || {
                    side_effects += 1;
                    Ok(())
                }),
                Err(libc::EINVAL)
            );
        }
        assert_eq!(
            write_admitted_with(info, 6, 0, 2, &bytes, || {
                side_effects += 1;
                Ok(())
            }),
            Err(libc::ESTALE)
        );
        assert_eq!(side_effects, 0);
        assert_eq!(
            write_admitted_with(info, 7, 0, 2, &bytes, || {
                side_effects += 1;
                Err(libc::EIO)
            }),
            Err(libc::EIO)
        );
        assert_eq!(
            side_effects, 1,
            "valid requests submit once and preserve failures"
        );
    }

    #[test]
    fn generation_mismatch_is_stale_not_a_fallback() {
        assert_eq!(require_generation(info(), 6), Err(libc::ESTALE));
        assert_eq!(require_generation(info(), 7), Ok(()));
    }

    #[test]
    fn startup_wait_slice_is_bounded_and_nonzero() {
        assert_eq!(bounded_wait_timeout_ms(Duration::ZERO), None);
        assert_eq!(bounded_wait_timeout_ms(Duration::from_nanos(1)), Some(1));
        assert_eq!(
            bounded_wait_timeout_ms(Duration::from_millis(WAIT_SLICE_MILLIS + 1)),
            Some(WAIT_SLICE_MILLIS)
        );
    }

    #[test]
    fn quarantine_rejects_same_generation_and_recovers_on_new_generation() {
        let q = AtomicU64::new(7);
        assert_eq!(require_unquarantined_for(&q, 7), Err(libc::EIO));
        assert_eq!(require_unquarantined_for(&q, 0), Err(libc::EIO));
        assert_eq!(require_unquarantined_for(&q, 8), Ok(()));
        // ORDERING: single-threaded test; the orderings mirror production.
        QUARANTINED_GENERATION.store(7, Ordering::Release);
        clear_quarantine_for_new_generation(7);
        // ORDERING: single-threaded test read.
        assert_eq!(QUARANTINED_GENERATION.load(Ordering::Acquire), 7);
        clear_quarantine_for_new_generation(6);
        // ORDERING: single-threaded test read.
        assert_eq!(
            QUARANTINED_GENERATION.load(Ordering::Acquire),
            7,
            "an older generation never lifts quarantine"
        );
        clear_quarantine_for_new_generation(8);
        // ORDERING: single-threaded test read.
        assert_eq!(QUARANTINED_GENERATION.load(Ordering::Acquire), 0);
    }

    #[test]
    fn read_only_device_rejects_write_admission_with_erofs_and_preserves_cache() {
        let ro_info = BlockInfo {
            flags: BLOCK_BROKER_INFO_FLAG_READ_ONLY,
            logical_block_size: 512,
            ..info()
        };
        let bytes = [0_u8; 512];
        let mut side_effects = 0;
        assert_eq!(
            write_admitted_with(ro_info, 7, 0, 1, &bytes, || {
                side_effects += 1;
                Ok(())
            }),
            Err(libc::EROFS)
        );
        assert_eq!(side_effects, 0);
        assert_eq!(
            write_with_info(ro_info, 7, 0, 1, &bytes, false),
            Err(libc::EROFS)
        );
    }

    #[test]
    fn writable_device_permits_multi_block_write_and_invalidates_read_cache() {
        let _engine = engine_test_guard();
        let rw_info = BlockInfo {
            flags: 0,
            logical_block_size: 512,
            ..info()
        };
        let geometry = read_geometry(rw_info).expect("geometry");
        ENGINE.seed_ready(geometry, 0, vec![0x5a; crate::read_engine::WINDOW_BYTES]);
        assert!(ENGINE.has_ready(0));

        let bytes = [0xa5_u8; 1024];
        let mut submitted = false;
        assert_eq!(
            write_admitted_with(rw_info, rw_info.generation, 0, 2, &bytes, || {
                // The cached window must already be invalidated before submission.
                assert!(!ENGINE.has_ready(0));
                submitted = true;
                Ok(())
            }),
            Ok(())
        );
        assert!(submitted);
    }
}
