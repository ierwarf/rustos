//! Storage-policy broker over the bounded DVM block substrate.
//!
//! - **Owner:** `storaged` owns geometry, timeout, retry, durability, and cache
//!   policy; Compat owns capability admission to ring0 transport.
//! - **Boundary:** Service requests, sectors, buffers, operation IDs, and
//!   provider epochs are untrusted.
//! - **Lifecycle:** Admit signed generation/geometry, submit, poll/cancel,
//!   finish exact ticket, and revoke/rebind only through a newer signed epoch.
//! - **Concurrency:** Transport calls are finite and never expose raw shared
//!   memory or physical addresses to the service.
//! - **Failure:** Timeout, device fault, short completion, revoke, and stale
//!   completion return exact errors without slot reuse or false durability.
//! - **Forbidden:** No VFS direct access, ring0 retry policy, fabricated ready
//!   marker, or generation-only restart authority.
//! - **Evidence:** `dvm-block-startup`, `dvm-volume-io`, and
//!   `durable-block-mutation`.
// Ring0 exposes only the bounded storage-DVM transport to storaged. Physical
// boot-volume discovery and reads are deliberately absent.
use super::*;

use alloc::vec::Vec;
use kernel_io_manager::api::{DvmBlockError, DvmBlockPoll, DvmBlockTicket, block as block_api};
use rustos_user_abi::syscall::{
    BLOCK_BROKER_ABI_VERSION, BLOCK_BROKER_FLAG_FUA, BLOCK_BROKER_INFO_FLAG_READ_ONLY,
    BLOCK_BROKER_KNOWN_FLAGS, BLOCK_BROKER_MAX_IO_BYTES, BLOCK_BROKER_OP_DVM_CANCEL,
    BLOCK_BROKER_OP_DVM_COLLECT, BLOCK_BROKER_OP_DVM_INFO, BLOCK_BROKER_OP_DVM_SUBMIT_FLUSH,
    BLOCK_BROKER_OP_DVM_SUBMIT_READ, BLOCK_BROKER_OP_DVM_SUBMIT_WRITE, BLOCK_BROKER_OP_DVM_WAIT,
    BLOCK_BROKER_WAIT_MAX_TIMEOUT_MS, DvmBlockInfoWire, DvmBlockTicketWire,
    IPC_SERVICE_CAP_STORAGE_POLICY, RustosBlockBrokerArgs,
};

static BLOCK_BROKER_REJECTION_COUNT: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

fn log_block_broker_rejection(stage: &str, args: &RustosBlockBrokerArgs) {
    let count = BLOCK_BROKER_REJECTION_COUNT
        .fetch_add(1, core::sync::atomic::Ordering::Relaxed)
        .saturating_add(1);
    // A malformed caller can otherwise turn synchronous debugcon into the
    // dominant ring0 workload. Preserve the first witnesses and logarithmic
    // aggregate progress without amplifying a fault into a boot-wide stall.
    if count > 4 && !count.is_power_of_two() {
        return;
    }
    let (pid, tid) = multitask::current_user_log_ids().unwrap_or((0, 0));
    nucleus_core::debug::write_debugcon_only_line(
        alloc::format!(
            "dvm-block: broker rejected stage={} count={} pid={} tid={} op={} abi={} expected={} flags={:#x} reserved={:#x} lba={} blocks={} buffer={:#x}/{} timeout_ms={} ticket={}:{} out_ticket={:#x} out_info={:#x}",
            stage,
            count,
            pid,
            tid,
            args.op,
            args.abi_version,
            BLOCK_BROKER_ABI_VERSION,
            args.flags,
            args.reserved0,
            args.lba,
            args.block_count,
            args.buffer_ptr,
            args.buffer_len,
            args.timeout_ms,
            args.ticket.generation,
            args.ticket.request_id,
            args.out_ticket_ptr,
            args.out_info_ptr,
        )
        .as_bytes(),
    );
}

pub(super) fn syscall_linux_rustos_block_broker(args_ptr: u64) -> u64 {
    let args = match usermem::read_current_user_struct::<RustosBlockBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.abi_version != BLOCK_BROKER_ABI_VERSION
        || args.reserved0 != 0
        || args.flags & !BLOCK_BROKER_KNOWN_FLAGS != 0
    {
        log_block_broker_rejection("envelope", &args);
        return linux_errno(LINUX_EINVAL);
    }

    match args.op {
        BLOCK_BROKER_OP_DVM_INFO
        | BLOCK_BROKER_OP_DVM_SUBMIT_READ
        | BLOCK_BROKER_OP_DVM_SUBMIT_WRITE
        | BLOCK_BROKER_OP_DVM_SUBMIT_FLUSH
        | BLOCK_BROKER_OP_DVM_COLLECT
        | BLOCK_BROKER_OP_DVM_CANCEL
        | BLOCK_BROKER_OP_DVM_WAIT => {
            if !ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_STORAGE_POLICY) {
                if let Some((process_id, capabilities)) =
                    ipc_ops::current_process_service_capability_snapshot()
                {
                    nucleus_core::debug::write_debugcon_only_line(
                        alloc::format!(
                            "dvm-block: broker denied pid={} capabilities={:#x} required={:#x}",
                            process_id,
                            capabilities,
                            IPC_SERVICE_CAP_STORAGE_POLICY,
                        )
                        .as_bytes(),
                    );
                } else {
                    nucleus_core::debug::write_debugcon_only_line(
                        b"dvm-block: broker denied pid=none capabilities=none",
                    );
                }
                return linux_errno(LINUX_EPERM);
            }
            broker_dvm(&args)
        }
        _ => linux_errno(LINUX_EINVAL),
    }
}

fn broker_dvm(args: &RustosBlockBrokerArgs) -> u64 {
    let result = match args.op {
        BLOCK_BROKER_OP_DVM_INFO => broker_dvm_info(args),
        BLOCK_BROKER_OP_DVM_SUBMIT_READ => broker_dvm_submit(args, false),
        BLOCK_BROKER_OP_DVM_SUBMIT_WRITE => broker_dvm_submit(args, true),
        BLOCK_BROKER_OP_DVM_SUBMIT_FLUSH => broker_dvm_flush(args),
        BLOCK_BROKER_OP_DVM_COLLECT => broker_dvm_collect(args),
        BLOCK_BROKER_OP_DVM_CANCEL => broker_dvm_cancel(args),
        BLOCK_BROKER_OP_DVM_WAIT => broker_dvm_wait(args),
        _ => {
            nucleus_core::debug::write_debugcon_only_line(
                b"dvm-block: broker rejected stage=operation",
            );
            Err(LINUX_EINVAL)
        }
    };
    match result {
        Ok(value) => value,
        Err(errno) => {
            if errno == LINUX_EINVAL {
                log_block_broker_rejection("operation", args);
            }
            linux_errno(errno)
        }
    }
}

fn broker_dvm_info(args: &RustosBlockBrokerArgs) -> Result<u64, i64> {
    if args.flags != 0
        || args.out_info_ptr == 0
        || args.out_ticket_ptr != 0
        || args.ticket != DvmBlockTicketWire::default()
        || args.buffer_ptr != 0
        || args.buffer_len != 0
        || args.lba != 0
        || args.block_count != 0
        || args.timeout_ms != 0
    {
        return Err(LINUX_EINVAL);
    }
    let info = block_api::dvm_info().map_err(dvm_error_to_linux_errno)?;
    let wire = DvmBlockInfoWire {
        generation: info.generation,
        capacity_sectors: info.capacity_sectors,
        features: info.features,
        logical_block_size: info.logical_block_size,
        physical_block_size: info.physical_block_size,
        flags: if info.read_only {
            BLOCK_BROKER_INFO_FLAG_READ_ONLY
        } else {
            0
        },
        reserved0: 0,
    };
    usermem::write_current_user_struct(args.out_info_ptr, &wire)
        .map_err(address_space_error_to_linux_errno)?;
    Ok(0)
}

fn broker_dvm_submit(args: &RustosBlockBrokerArgs, write: bool) -> Result<u64, i64> {
    if args.out_ticket_ptr == 0
        || args.out_info_ptr != 0
        || args.ticket != DvmBlockTicketWire::default()
        || args.block_count == 0
        || args.timeout_ms != 0
        || (!write && (args.flags != 0 || args.buffer_ptr != 0))
        || (write && args.buffer_ptr == 0)
    {
        return Err(LINUX_EINVAL);
    }
    let info = block_api::dvm_info().map_err(dvm_error_to_linux_errno)?;
    let (sector, byte_len) =
        checked_dvm_range(args, info.logical_block_size, info.capacity_sectors)?;
    let ticket = if write {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(byte_len)
            .map_err(|_| LINUX_ENOMEM)?;
        bytes.resize(byte_len, 0);
        usermem::copy_from_current_user_exact(args.buffer_ptr, &mut bytes)
            .map_err(address_space_error_to_linux_errno)?;
        block_api::submit_dvm_write(sector, &bytes, args.flags & BLOCK_BROKER_FLAG_FUA != 0)
    } else {
        block_api::submit_dvm_read(sector, u32::try_from(byte_len).map_err(|_| LINUX_EINVAL)?)
    }
    .map_err(dvm_error_to_linux_errno)?;
    write_dvm_ticket(args.out_ticket_ptr, ticket).inspect_err(|_| {
        let _ = block_api::cancel_dvm(ticket);
    })?;
    Ok(0)
}

fn broker_dvm_flush(args: &RustosBlockBrokerArgs) -> Result<u64, i64> {
    if args.flags != 0
        || args.out_ticket_ptr == 0
        || args.out_info_ptr != 0
        || args.ticket != DvmBlockTicketWire::default()
        || args.lba != 0
        || args.block_count != 0
        || args.buffer_ptr != 0
        || args.buffer_len != 0
        || args.timeout_ms != 0
    {
        return Err(LINUX_EINVAL);
    }
    let ticket = block_api::submit_dvm_flush().map_err(dvm_error_to_linux_errno)?;
    write_dvm_ticket(args.out_ticket_ptr, ticket).inspect_err(|_| {
        let _ = block_api::cancel_dvm(ticket);
    })?;
    Ok(0)
}

fn broker_dvm_collect(args: &RustosBlockBrokerArgs) -> Result<u64, i64> {
    if args.flags != 0
        || args.out_ticket_ptr != 0
        || args.out_info_ptr != 0
        || args.lba != 0
        || args.block_count != 0
        || args.timeout_ms != 0
        || args.buffer_len > BLOCK_BROKER_MAX_IO_BYTES as u64
        || (args.buffer_len == 0) != (args.buffer_ptr == 0)
    {
        nucleus_core::debug::write_debugcon_only_line(
            b"dvm-block: collect rejected stage=envelope",
        );
        return Err(LINUX_EINVAL);
    }
    let ticket = read_dvm_ticket(args.ticket).inspect_err(|_| {
        nucleus_core::debug::write_debugcon_only_line(b"dvm-block: collect rejected stage=ticket");
    })?;
    let byte_len = usize::try_from(args.buffer_len).map_err(|_| LINUX_EINVAL)?;
    if byte_len != 0 {
        usermem::validate_current_user_write_buffer(args.buffer_ptr, byte_len)
            .map_err(address_space_error_to_linux_errno)
            .inspect_err(|_| {
                nucleus_core::debug::write_debugcon_only_line(
                    b"dvm-block: collect rejected stage=user-buffer",
                );
            })?;
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(byte_len)
        .map_err(|_| LINUX_ENOMEM)?;
    bytes.resize(byte_len, 0);
    let poll = match block_api::poll_dvm(ticket, &mut bytes) {
        Ok(poll) => poll,
        Err(error @ (DvmBlockError::DeviceFault | DvmBlockError::Unsupported)) => {
            let _ = block_api::finish_dvm(ticket);
            return Err(dvm_error_to_linux_errno(error));
        }
        Err(error) => {
            nucleus_core::debug::write_debugcon_only_line(
                b"dvm-block: collect rejected stage=poll",
            );
            return Err(dvm_error_to_linux_errno(error));
        }
    };
    match poll {
        DvmBlockPoll::Pending => Err(LINUX_EAGAIN),
        DvmBlockPoll::Completed(completed) => {
            if completed != 0 {
                usermem::write_current_user_bytes(args.buffer_ptr, &bytes[..completed])
                    .map_err(address_space_error_to_linux_errno)?;
            }
            block_api::finish_dvm(ticket)
                .map_err(dvm_error_to_linux_errno)
                .inspect_err(|_| {
                    nucleus_core::debug::write_debugcon_only_line(
                        b"dvm-block: collect rejected stage=finish",
                    );
                })?;
            Ok(completed as u64)
        }
    }
}

fn broker_dvm_cancel(args: &RustosBlockBrokerArgs) -> Result<u64, i64> {
    if args.flags != 0
        || args.out_ticket_ptr != 0
        || args.out_info_ptr != 0
        || args.lba != 0
        || args.block_count != 0
        || args.buffer_ptr != 0
        || args.buffer_len != 0
        || args.timeout_ms != 0
    {
        return Err(LINUX_EINVAL);
    }
    block_api::cancel_dvm(read_dvm_ticket(args.ticket)?).map_err(dvm_error_to_linux_errno)?;
    Ok(0)
}

fn broker_dvm_wait(args: &RustosBlockBrokerArgs) -> Result<u64, i64> {
    if args.flags != 0
        || args.out_ticket_ptr != 0
        || args.out_info_ptr != 0
        || args.ticket != DvmBlockTicketWire::default()
        || !(1..=BLOCK_BROKER_WAIT_MAX_TIMEOUT_MS).contains(&args.timeout_ms)
        || args.lba != 0
        || args.block_count != 0
        || args.buffer_ptr != 0
        || args.buffer_len != 0
    {
        return Err(LINUX_EINVAL);
    }
    let ticks_per_second = crate::arch::rtc::ticks_per_second().max(1);
    let timeout_ticks = args
        .timeout_ms
        .saturating_mul(ticks_per_second)
        .div_ceil(1000)
        .max(1);
    let deadline_tick = crate::arch::rtc::ticks().saturating_add(timeout_ticks);
    loop {
        if block_api::dvm_completion_or_fault_pending() {
            return Ok(0);
        }
        if crate::arch::rtc::ticks() >= deadline_tick {
            return Err(LINUX_ETIMEDOUT);
        }
        let task_id = multitask::current_task_id().ok_or(LINUX_EINVAL)?;
        if !multitask::arm_block_current_task() {
            return Err(LINUX_EINVAL);
        }
        if !block_api::arm_dvm_waiter(task_id) {
            let _ = multitask::cancel_block_current_task();
            return Err(LINUX_EBUSY);
        }
        if block_api::dvm_completion_or_fault_pending() {
            block_api::disarm_dvm_waiter(task_id);
            let _ = multitask::cancel_block_current_task();
            continue;
        }
        if !crate::arch::rtc::arm_sleep_waiter_until_tick(task_id, deadline_tick) {
            block_api::disarm_dvm_waiter(task_id);
            let _ = multitask::cancel_block_current_task();
            return Err(LINUX_EBUSY);
        }
        match multitask::commit_block_current_task_and_yield() {
            Some(true) => {}
            Some(false) => {}
            None => {
                block_api::disarm_dvm_waiter(task_id);
                crate::arch::rtc::disarm_sleep_waiter(task_id);
                return Err(LINUX_EINVAL);
            }
        }
        block_api::disarm_dvm_waiter(task_id);
        crate::arch::rtc::disarm_sleep_waiter(task_id);
    }
}

fn checked_dvm_range(
    args: &RustosBlockBrokerArgs,
    logical_block_size: u32,
    capacity_sectors: u64,
) -> Result<(u64, usize), i64> {
    if logical_block_size < 512
        || !logical_block_size.is_power_of_two()
        || !logical_block_size.is_multiple_of(512)
    {
        return Err(LINUX_EIO);
    }
    let byte_len = args
        .block_count
        .checked_mul(u64::from(logical_block_size))
        .ok_or(LINUX_EINVAL)?;
    if byte_len == 0 || byte_len != args.buffer_len || byte_len > BLOCK_BROKER_MAX_IO_BYTES as u64 {
        return Err(LINUX_EINVAL);
    }
    let sectors_per_block = u64::from(logical_block_size / 512);
    let sector = args
        .lba
        .checked_mul(sectors_per_block)
        .ok_or(LINUX_EINVAL)?;
    let sector_count = args
        .block_count
        .checked_mul(sectors_per_block)
        .ok_or(LINUX_EINVAL)?;
    if sector
        .checked_add(sector_count)
        .is_none_or(|end| end > capacity_sectors)
    {
        return Err(LINUX_EINVAL);
    }
    Ok((sector, usize::try_from(byte_len).map_err(|_| LINUX_EINVAL)?))
}

fn write_dvm_ticket(out_ptr: u64, ticket: DvmBlockTicket) -> Result<(), i64> {
    usermem::write_current_user_struct(
        out_ptr,
        &DvmBlockTicketWire {
            generation: ticket.generation,
            request_id: ticket.request_id,
            data_slot: ticket.data_slot,
            reserved0: 0,
        },
    )
    .map_err(address_space_error_to_linux_errno)
}

fn read_dvm_ticket(ticket: DvmBlockTicketWire) -> Result<DvmBlockTicket, i64> {
    if ticket.generation == 0 || ticket.request_id == 0 || ticket.reserved0 != 0 {
        return Err(LINUX_EINVAL);
    }
    Ok(DvmBlockTicket {
        generation: ticket.generation,
        request_id: ticket.request_id,
        data_slot: ticket.data_slot,
    })
}

fn dvm_error_to_linux_errno(error: DvmBlockError) -> i64 {
    match error {
        DvmBlockError::Unavailable => LINUX_ENODEV,
        DvmBlockError::Busy => LINUX_EAGAIN,
        DvmBlockError::Invalid => LINUX_EINVAL,
        DvmBlockError::Protocol | DvmBlockError::DeviceFault => LINUX_EIO,
        DvmBlockError::Revoked => LINUX_ESTALE,
        DvmBlockError::Unsupported => LINUX_ENOSYS,
        DvmBlockError::Cancelled => LINUX_EINTR,
    }
}
