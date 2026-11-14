//! Devmgrd-only allocation preparation and exact DVM export custody. No
//! userspace field selects an aperture offset. Provider failure or abandoned
//! requests quarantine ranges until the complete retirement proof exists.
use super::*;
use alloc::vec::Vec;
use driver_domain_protocol::DvmAppBuffer;
use kernel_io_manager::api::app_buffer as transport;
use kernel_ps::api::handles::{app_buffer_pool, BufferCapability, BufferOwner, BufferRights};
use rustos_user_abi::syscall::{RustosAppBufferBrokerArgs, APP_BUFFER_BROKER_BEGIN,
    APP_BUFFER_BROKER_TAKE, APP_BUFFER_BROKER_CANCEL, APP_BUFFER_BROKER_REAP};
use spin::Mutex;

struct Active { capability: BufferCapability, record: DvmAppBuffer }
struct Pending {
    active: Active,
    owner: u64,
    /// Exact incarnation that reserved the descriptor slot; every later effect
    /// on the process revalidates it, so a reused PID or a new image is refused.
    process_generation: u32,
    mm_generation: u32,
    epoch: u64,
    sequence: u64,
    deadline: u64,
    reservation: u64,
    fd: u64,
    cancelled: bool,
    retiring: bool,
}
struct State { pending: Option<Pending>, active: Vec<Active> }
static STATE: Mutex<State> = Mutex::new(State { pending: None, active: Vec::new() });

fn live_owner() -> Result<(u64, u64), i64> {
    let pid = multitask::current_user_process_id().ok_or(LINUX_EPERM)?;
    let (owner, epoch) = ipc_ops::live_service_endpoint_owner_and_epoch(IPC_SERVICE_DEVMGRD)
        .ok_or(LINUX_EPERM)?;
    if owner != pid { return Err(LINUX_EPERM); }
    Ok((pid, epoch))
}
fn terr(error: transport::AppBufferTransportError) -> i64 {
    match error {
        transport::AppBufferTransportError::Busy => LINUX_EBUSY,
        transport::AppBufferTransportError::Invalid => LINUX_EINVAL,
        transport::AppBufferTransportError::Stale => LINUX_ESTALE,
        transport::AppBufferTransportError::Exhausted => LINUX_EOVERFLOW,
        transport::AppBufferTransportError::Unavailable => LINUX_ENODEV,
        transport::AppBufferTransportError::Provider(_) => LINUX_EIO,
    }
}
fn cancel_slot(p: &Pending) {
    let _ = multitask::with_process_state_by_generation_mut(
        p.owner, p.process_generation, p.mm_generation, |state| {
            state.handles_mut().cancel_reservations(p.reservation, &[p.fd]);
        });
}
/// Range that was allocated but never published to the provider: nothing can
/// hold an export, so it is retired locally and reclaimed (scrubbed) by the next
/// reap instead of leaking aperture space.
fn abandon_unpublished(capability: BufferCapability) {
    let id = capability.identity();
    {
        let mut pool = app_buffer_pool();
        let _ = pool.revoke(id);
        let _ = pool.provider_retired(id);
    }
    drop(capability);
}
fn begin(args: &RustosAppBufferBrokerArgs, owner: u64, epoch: u64) -> Result<u64, i64> {
    if args.sequence != 0 || !(1..=1000).contains(&args.timeout_ms) { return Err(LINUX_EINVAL); }
    let image = u64::from(args.stride).checked_mul(u64::from(args.height)).ok_or(LINUX_EINVAL)?;
    let length = image.checked_add(4095).ok_or(LINUX_EINVAL)? & !4095;
    let (_, identity) = multitask::current_user_ipc_identity().ok_or(LINUX_ESRCH)?;
    let (process_generation, mm_generation) = (identity.process_generation(), identity.mm_generation());
    let mut state = STATE.lock();
    if state.pending.is_some() { return Err(LINUX_EBUSY); }
    state.active.try_reserve(1).map_err(|_| LINUX_ENOMEM)?;
    let owner_id = BufferOwner { service_id: IPC_SERVICE_DEVMGRD, generation: epoch };
    let cap = app_buffer_pool().allocate(owner_id, length, BufferRights::ALL).map_err(|_| LINUX_ENOMEM)?;
    let (offset, _) = app_buffer_pool().range(cap.identity()).map_err(|_| LINUX_ESTALE)?;
    let record = DvmAppBuffer { provider_generation: epoch, allocation: cap.identity().allocation,
        offset, length, width: args.width, height: args.height, stride: args.stride, format: args.format };
    if !record.is_valid() { abandon_unpublished(cap); return Err(LINUX_EINVAL); }
    let reserved = multitask::with_current_user_process_state_mut(|_, _, process| {
        let (reservation, slots) = process.handles_mut().reserve_slots(1)?;
        if !process.handles_mut().prepare_reserved_device_buffer(reservation, slots[0]) {
            process.handles_mut().cancel_reservations(reservation, &slots);
            return None;
        }
        Some((reservation, slots))
    });
    let (reservation, slots) = match reserved {
        Some(Some(reserved)) => reserved,
        failure => {
            abandon_unpublished(cap);
            return Err(if failure.is_none() { LINUX_ESRCH } else { LINUX_EMFILE });
        }
    };
    let fd = slots[0];
    let deadline = crate::arch::rtc::ticks().saturating_add(u64::from(args.timeout_ms)
        .saturating_mul(crate::arch::rtc::ticks_per_second().max(1)).div_ceil(1000).max(1));
    let published = ipc_ops::with_devmgrd_publication(owner, epoch, || transport::begin(&cap, record, false));
    let sequence = match published {
        Some(Ok(sequence)) => sequence,
        failure => {
            let _ = multitask::with_process_state_by_generation_mut(
                owner, process_generation, mm_generation, |process| {
                    process.handles_mut().cancel_reservations(reservation, &[fd]);
                });
            abandon_unpublished(cap);
            return Err(match failure { Some(Err(err)) => terr(err), _ => LINUX_EPERM });
        }
    };
    state.pending = Some(Pending { active: Active { capability: cap, record }, owner,
        process_generation, mm_generation, epoch,
        sequence, deadline, reservation, fd, cancelled: false, retiring: false });
    Ok(sequence)
}
fn progress(owner: u64, epoch: u64, requested: Option<u64>, cancel: bool) -> Result<u64, i64> {
    let mut state = STATE.lock();
    let Some(mut pending) = state.pending.take() else { return Err(LINUX_EAGAIN); };
    if pending.sequence == 0 {
        // A previous retire attempt could not publish; retry it before anything else.
        match transport::begin(&pending.active.capability, pending.active.record, true) {
            Ok(seq) => { pending.sequence = seq; pending.retiring = true; }
            Err(err) => { state.pending = Some(pending); return Err(terr(err)); }
        }
    }
    if requested.is_some_and(|seq| seq != pending.sequence && !pending.retiring) {
        state.pending = Some(pending);
        return Err(LINUX_ESTALE);
    }
    if cancel || owner != pending.owner || epoch != pending.epoch || crate::arch::rtc::ticks() >= pending.deadline {
        if !pending.cancelled { cancel_slot(&pending); }
        pending.cancelled = true;
    }
    match transport::take(pending.sequence) {
        Ok(None) => { state.pending = Some(pending); Err(LINUX_EAGAIN) }
        Err(transport::AppBufferTransportError::Unavailable) => { state.pending = Some(pending); Err(LINUX_ENODEV) }
        Err(err) => {
            // The transport transaction is gone but custody is not: the range
            // stays quarantined and a provider revoke is (re)attempted. It is
            // never treated as retired without the provider's acknowledgement.
            if !pending.cancelled { cancel_slot(&pending); }
            pending.cancelled = true;
            pending.retiring = false;
            match transport::begin(&pending.active.capability, pending.active.record, true) {
                Ok(seq) => { pending.sequence = seq; pending.retiring = true; }
                Err(_) => { pending.sequence = 0; }
            }
            state.pending = Some(pending);
            Err(terr(err))
        }
        // Retirement acknowledged: reports cancellation, never a descriptor.
        Ok(Some(cap)) if pending.retiring => { drop(cap); Err(LINUX_ECANCELED) }
        Ok(Some(cap)) if pending.cancelled => {
            match transport::begin(&cap, pending.active.record, true) {
                Ok(seq) => { pending.sequence = seq; pending.retiring = true; state.pending = Some(pending); Err(LINUX_EAGAIN) }
                Err(err) => { state.pending = Some(pending); Err(terr(err)) }
            }
        }
        Ok(Some(cap)) => {
            // Commit only after the timely response take, while exact service
            // publication is still live. Capacity was prepared before locking.
            let committed = ipc_ops::with_devmgrd_publication(owner, epoch, || {
                if crate::arch::rtc::ticks() >= pending.deadline { return false; }
                multitask::with_process_state_by_generation_mut(
                    pending.owner, pending.process_generation, pending.mm_generation, |process| {
                        process.handles_mut().commit_reserved_device_buffer(pending.reservation, pending.fd, cap)
                    }).unwrap_or(false)
            }).unwrap_or(false);
            if committed {
                let fd = pending.fd;
                state.active.push(pending.active);
                Ok(fd)
            } else {
                cancel_slot(&pending);
                pending.cancelled = true;
                // Preserve custody even when publication lost its lease race.
                match transport::begin(&pending.active.capability, pending.active.record, true) {
                    Ok(seq) => { pending.sequence = seq; pending.retiring = true; }
                    Err(_) => {}
                }
                state.pending = Some(pending);
                Err(LINUX_ETIMEDOUT)
            }
        }
    }
}
/// Scrub every retired, unreferenced range before it can be reallocated. The
/// pool lock is never held across the scrub: the slot stays occupied (and so
/// unallocatable) until `reclaim_after_scrub` after the zeroing finished.
fn reclaim_retired() -> Result<(), i64> {
    loop {
        let (id, offset, len) = {
            let pool = app_buffer_pool();
            let Some(id) = pool.next_reclaimable() else { return Ok(()); };
            let (offset, len) = pool.reclaimable_range(id).map_err(|_| LINUX_EBUSY)?;
            (id, offset, len)
        };
        if !transport::scrub_range(offset, len) { return Err(LINUX_ENODEV); }
        app_buffer_pool().reclaim_after_scrub(id).map_err(|_| LINUX_EBUSY)?;
        // Emitted only after the scrub succeeded; packed like the publish record.
        crate::debug::record_milestone(
            crate::debug::LogCategory::Compat,
            "app-dmabuf-range-reclaimed",
            0,
            ((offset / 4096) << 32) | (len / 4096),
        );
    }
}
fn reap(owner: u64, epoch: u64) -> Result<u64, i64> {
    if STATE.lock().pending.is_some() {
        return match progress(owner, epoch, None, false) {
            Err(LINUX_ECANCELED) => Ok(0),
            other => other,
        };
    }
    reclaim_retired()?;
    let mut state = STATE.lock();
    let Some(index) = state.active.iter().position(|active| {
        active.record.provider_generation != epoch || active.capability.custody_references() == 2
    }) else { return Ok(0); };
    let active = state.active.swap_remove(index);
    match transport::begin(&active.capability, active.record, true) {
        Ok(sequence) => {
            state.pending = Some(Pending { active, owner, process_generation: 1, mm_generation: 1, epoch, sequence, deadline: u64::MAX,
                reservation: 0, fd: 0, cancelled: true, retiring: true });
            Ok(0)
        }
        Err(err) => { state.active.push(active); Err(terr(err)) }
    }
}
pub(super) fn syscall_linux_rustos_app_buffer_broker(ptr: u64) -> u64 {
    let args = match usermem::read_current_user_struct::<RustosAppBufferBrokerArgs>(ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    let (owner, epoch) = match live_owner() { Ok(subject) => subject, Err(e) => return linux_errno(e) };
    let result = match args.op {
        APP_BUFFER_BROKER_BEGIN => begin(&args, owner, epoch),
        APP_BUFFER_BROKER_TAKE => progress(owner, epoch, Some(args.sequence), false),
        APP_BUFFER_BROKER_CANCEL => progress(owner, epoch, Some(args.sequence), true),
        APP_BUFFER_BROKER_REAP => reap(owner, epoch),
        _ => Err(LINUX_EINVAL),
    };
    result.unwrap_or_else(linux_errno)
}

/// Client entry: forwards one allocation to devmgrd, which owns policy and the
/// broker. The pid/tid are kernel-stamped and the received handle must be a
/// device buffer; anything else is dropped and refused.
pub(super) fn syscall_linux_rustos_app_buffer_alloc(ptr: u64) -> u64 {
    use rustos_user_abi::syscall::{
        DevmgrdAppBufferRequest, DevmgrdAppBufferResponse, RustosAppBufferAllocArgs,
        APP_BUFFER_IPC_ALLOCATE, APP_BUFFER_IPC_VERSION,
    };
    let args = match usermem::read_current_user_struct::<RustosAppBufferAllocArgs>(ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.reserved != [0; 2] { return linux_errno(LINUX_EINVAL); }
    // Early refusal only; the authoritative check runs atomically with the
    // install below because other threads may allocate while devmgrd works.
    let requested = u64::from(args.stride).saturating_mul(u64::from(args.height));
    match multitask::with_current_user_process_state_mut(|_, _, process| {
        process.handles().device_buffer_held_bytes()
    }) {
        Some(held) if !client_quota_admits(held, requested) => return linux_errno(LINUX_ENOMEM),
        Some(_) => {}
        None => return linux_errno(LINUX_ESRCH),
    }
    let Some(snapshot) = multitask::current_user_snapshot() else { return linux_errno(LINUX_ESRCH); };
    let request = DevmgrdAppBufferRequest {
        version: APP_BUFFER_IPC_VERSION,
        op: APP_BUFFER_IPC_ALLOCATE,
        pid: snapshot.process_id(),
        tid: snapshot.thread_id(),
        width: args.width,
        height: args.height,
        stride: args.stride,
        format: args.format,
        reserved: [0; 2],
    };
    let (response, entries) = match ipc_ops::call_service_endpoint_with_received_entries(
        IPC_SERVICE_DEVMGRD, as_bytes(&request), 1) {
        Ok(reply) => reply,
        Err(errno) => return linux_errno(errno),
    };
    if response.len() != core::mem::size_of::<DevmgrdAppBufferResponse>() {
        ipc_ops::drop_transfer_entries(entries);
        return linux_errno(LINUX_EINVAL);
    }
    let response = read_unaligned::<DevmgrdAppBufferResponse>(response.as_slice());
    if response.version != APP_BUFFER_IPC_VERSION || response.status < 0 {
        ipc_ops::drop_transfer_entries(entries);
        return linux_errno(LINUX_EINVAL);
    }
    if response.status != 0 {
        ipc_ops::drop_transfer_entries(entries);
        return linux_errno(i64::from(response.status));
    }
    let mut entries = entries.into_iter();
    let (Some(entry), None) = (entries.next(), entries.next()) else {
        return linux_errno(LINUX_EINVAL);
    };
    if !matches!(entry.entry().handle(), multitask::KernelHandle::DeviceBuffer(_)) {
        ipc_ops::drop_transfer_entries(alloc::vec![entry]);
        return linux_errno(LINUX_EINVAL);
    }
    let length = match entry.entry().handle() {
        multitask::KernelHandle::DeviceBuffer(capability) => capability.len(),
        _ => 0,
    };
    // Quota, Job charge and install are one critical section on the caller's
    // table. The charge group is read first (lock-free published-identity
    // path); `None` (kernel/unpublished caller) bills nothing, as elsewhere.
    let group = multitask::current_user_job_charge_group();
    let installed = multitask::with_current_user_process_state_mut(|_, _, process| {
        if !client_quota_admits(process.handles().device_buffer_held_bytes(), length) {
            return Err((LINUX_ENOMEM, Some(entry)));
        }
        let capability = match entry.entry().handle() {
            multitask::KernelHandle::DeviceBuffer(capability) => capability.clone(),
            _ => return Err((LINUX_EINVAL, Some(entry))),
        };
        let billed = match group {
            Some(group) => {
                if capability.bill(group).is_err() {
                    return Err((LINUX_ENOMEM, Some(entry)));
                }
                true
            }
            None => false,
        };
        match process.handles_mut().install_transferred(entry) {
            Some(fd) => {
                drop(capability);
                Ok(fd)
            }
            None => {
                if billed {
                    capability.unbill();
                }
                Err((LINUX_EMFILE, None))
            }
        }
    });
    match installed {
        Some(Ok(fd)) => {
            crate::debug::record_milestone(
                crate::debug::LogCategory::Compat,
                "app-dmabuf-allocation-granted",
                fd as u64,
                length as u64,
            );
            fd
        }
        Some(Err((errno, refused))) => {
            if let Some(entry) = refused {
                ipc_ops::drop_transfer_entries(alloc::vec![entry]);
            }
            linux_errno(errno)
        }
        None => linux_errno(LINUX_ESRCH),
    }
}

/// Per-holder admission: the caller's table may hold at most
/// `APP_BUFFER_CLIENT_QUOTA_BYTES` of distinct app DMA-BUF aperture bytes.
fn client_quota_admits(held: u64, requested: u64) -> bool {
    held.checked_add(requested).is_some_and(|total| {
        total <= rustos_user_abi::syscall::APP_BUFFER_CLIENT_QUOTA_BYTES
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_user_abi::syscall::APP_BUFFER_CLIENT_QUOTA_BYTES as QUOTA;

    #[test]
    fn client_quota_bounds_distinct_held_bytes_without_overflow() {
        assert!(client_quota_admits(0, QUOTA));
        assert!(!client_quota_admits(0, QUOTA + 1));
        assert!(client_quota_admits(QUOTA - 4096, 4096));
        assert!(!client_quota_admits(QUOTA - 4096, 4097));
        assert!(!client_quota_admits(u64::MAX, 1));
        assert!(!client_quota_admits(1, u64::MAX));
    }

    #[test]
    fn every_provider_transport_error_maps_to_a_distinct_fail_closed_errno() {
        use transport::AppBufferTransportError as E;
        assert_eq!(terr(E::Busy), LINUX_EBUSY);
        assert_eq!(terr(E::Stale), LINUX_ESTALE);
        assert_eq!(terr(E::Exhausted), LINUX_EOVERFLOW);
        assert_eq!(terr(E::Unavailable), LINUX_ENODEV);
        assert_eq!(terr(E::Invalid), LINUX_EINVAL);
        assert_eq!(terr(E::Provider(3)), LINUX_EIO);
    }
}
