//! Windows NT threads, cross-process memory, security tokens, and SEH exceptions.
//!
//! Replaces `wineserver` / `ntdll/unix` thread, context, process-inspection,
//! and exception-dispatch paths using RustOS `kernel-ps` task creation.

use alloc::vec;
use core::mem::size_of;
use rustos_user_abi::windows::{
    HANDLE_CURRENT_PROCESS, HANDLE_CURRENT_THREAD, PAGE_READWRITE, PAGE_SIZE, STATUS_ACCESS_DENIED,
    STATUS_INFO_LENGTH_MISMATCH, STATUS_INVALID_HANDLE, STATUS_INVALID_INFO_CLASS,
    STATUS_INVALID_PARAMETER, STATUS_NOT_IMPLEMENTED, STATUS_NO_MEMORY, STATUS_SUCCESS,
    WIN32_PROCESS_HANDLE_BASE, WIN32_THREAD_HANDLE_BASE,
};
use x86_64::VirtAddr;
use x86_64::structures::paging::PageTableFlags;

use super::super::SyscallFrame;
use super::nt_dispatch::{
    alloc_win32_zeroed_pages, find_ntdll_export_address, read_stack_arg, read_user_u64,
    write_user_u32_checked, write_user_u64,
};
use crate::io::session::ConsoleSessionHandle;
use crate::memory::paging;
use crate::multitask;
use crate::user::abi::UserAbi;
use crate::user::process_state::{
    Win32ProcessHandleObject, Win32ThreadHandleObject, WindowsThreadRuntimeState,
};
use crate::user::sysops::usermem;

const THREAD_CREATE_FLAGS_CREATE_SUSPENDED: u64 = 0x0000_0004;
const PS_ATTRIBUTE_CLIENT_ID: u64 = 0x0001_0003;
const PS_ATTRIBUTE_TEB_ADDRESS: u64 = 0x0001_0004;
const DEFAULT_THREAD_STACK_RESERVE: u64 = 1024 * 1024;
const DEFAULT_THREAD_STACK_COMMIT: u64 = 64 * 1024;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ClientId64 {
    unique_process: u64,
    unique_thread: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PsAttribute64 {
    attribute: u64,
    size: u64,
    value: u64,
    return_length: u64,
}

pub(super) fn resolve_thread_id(handle: u64) -> Option<u64> {
    if handle == HANDLE_CURRENT_THREAD {
        return multitask::current_user_thread_id();
    }
    multitask::with_current_user_process_state_mut(|_, _, st| {
        st.win32_objects()
            .threads
            .iter()
            .find(|t| t.handle == handle)
            .map(|t| t.tid)
    })
    .flatten()
}

pub(super) fn resolve_process_id(handle: u64) -> Option<u64> {
    if handle == HANDLE_CURRENT_PROCESS {
        return multitask::current_user_process_id();
    }
    multitask::with_current_user_process_state_mut(|_, _, st| {
        st.win32_objects()
            .processes
            .iter()
            .find(|p| p.handle == handle)
            .map(|p| p.pid)
    })
    .flatten()
}

pub(super) fn nt_create_thread_ex(frame: &SyscallFrame) -> u64 {
    let (handle_ptr, _access, _oa_ptr, proc_handle) =
        (frame.r10, frame.rdx, frame.r8, frame.r9);
    let start_routine = read_stack_arg(frame, 4).unwrap_or(0);
    let argument = read_stack_arg(frame, 5).unwrap_or(0);
    let create_flags = read_stack_arg(frame, 6).unwrap_or(0);
    let _zero_bits = read_stack_arg(frame, 7).unwrap_or(0);
    let stack_commit = read_stack_arg(frame, 8).unwrap_or(0);
    let stack_reserve = read_stack_arg(frame, 9).unwrap_or(0);
    let attr_list_ptr = read_stack_arg(frame, 10).unwrap_or(0);

    if handle_ptr == 0 || start_routine == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    if proc_handle != HANDLE_CURRENT_PROCESS {
        let cur_pid = multitask::current_user_process_id();
        if resolve_process_id(proc_handle) != cur_pid {
            return STATUS_INVALID_HANDLE;
        }
    }
    let Some(pid) = multitask::current_user_process_id() else {
        return STATUS_INVALID_PARAMETER;
    };
    let console_session = multitask::current_user_snapshot()
        .map(|s| s.console_session())
        .unwrap_or(ConsoleSessionHandle::from_raw(0));

    let reserve = stack_reserve
        .max(DEFAULT_THREAD_STACK_RESERVE)
        .next_multiple_of(PAGE_SIZE)
        .min(16 * 1024 * 1024);
    let commit = stack_commit
        .max(DEFAULT_THREAD_STACK_COMMIT)
        .next_multiple_of(PAGE_SIZE)
        .min(reserve);

    let rw_flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::USER_ACCESSIBLE
        | PageTableFlags::NO_EXECUTE;

    let Some(stack_base) = alloc_win32_zeroed_pages(reserve, PAGE_READWRITE as u32, rw_flags)
    else {
        return STATUS_NO_MEMORY;
    };
    let stack_top = stack_base + reserve;
    let _ = commit;

    let teb_size = 4 * PAGE_SIZE;
    let Some(teb_va) = alloc_win32_zeroed_pages(teb_size, PAGE_READWRITE as u32, rw_flags) else {
        return STATUS_NO_MEMORY;
    };

    let (peb_va, ntdll_rtl_start) = multitask::with_current_user_process_state_mut(|_, _, st| {
        let peb = st.windows_runtime().map(|r| r.peb_address).unwrap_or(0);
        (peb, find_ntdll_export_address(st, "RtlUserThreadStart"))
    })
    .unwrap_or((0, None));

    // Populate minimum x64 TEB fields (Tib, Self, Peb, Locale).
    let mut teb_page = vec![0_u8; PAGE_SIZE as usize];
    teb_page[0x00..0x08].copy_from_slice(&u64::MAX.to_le_bytes()); // ExceptionList = -1
    teb_page[0x08..0x10].copy_from_slice(&stack_top.to_le_bytes()); // Tib.StackBase
    teb_page[0x10..0x18].copy_from_slice(&stack_base.to_le_bytes()); // Tib.StackLimit
    teb_page[0x20..0x28].copy_from_slice(&0x1e00_u64.to_le_bytes()); // FiberData
    teb_page[0x30..0x38].copy_from_slice(&teb_va.to_le_bytes()); // Tib.Self
    teb_page[0x60..0x68].copy_from_slice(&peb_va.to_le_bytes()); // Peb
    teb_page[0x108..0x10c].copy_from_slice(&0x0409_u32.to_le_bytes()); // CurrentLocale
    teb_page[0x1478..0x1480].copy_from_slice(&stack_base.to_le_bytes()); // DeallocationStack
    if usermem::write_current_user_bytes(teb_va, &teb_page).is_err() {
        return STATUS_NO_MEMORY;
    }

    // Reserve shadow space + return slot cleanly aligned on x64 ABI (rsp % 16 == 8 before entry).
    let child_rsp = stack_top - 0x28;
    let (entry_rip, rcx_arg, rdx_arg) = if let Some(rtl_start) = ntdll_rtl_start {
        (rtl_start, start_routine, argument)
    } else {
        (start_routine, argument, 0)
    };

    let mut bootstrap = multitask::UserTaskBootstrap::new(
        UserAbi::Windows,
        VirtAddr::new(entry_rip),
        VirtAddr::new(child_rsp),
    );
    bootstrap.console_session = console_session;
    bootstrap.windows_thread_state = Some(WindowsThreadRuntimeState::new(0, teb_va));
    bootstrap.registers = multitask::UserTaskRegisters {
        rcx: rcx_arg,
        rdx: rdx_arg,
        r11: 0x202,
        ..multitask::UserTaskRegisters::default()
    };

    let child_tid = match multitask::spawn_user_thread_suspended(bootstrap) {
        Ok(tid) => tid,
        Err(_) => return STATUS_NO_MEMORY,
    };

    // Stamp ClientId + RealClientId in child TEB (+0x40..+0x50, +0x17c8..+0x17d8).
    let mut cid_bytes = [0_u8; 16];
    cid_bytes[0..8].copy_from_slice(&pid.to_le_bytes());
    cid_bytes[8..16].copy_from_slice(&child_tid.to_le_bytes());
    let _ = usermem::write_current_user_bytes(teb_va + 0x40, &cid_bytes);
    let _ = usermem::write_current_user_bytes(teb_va + 0x17c8, &cid_bytes);

    let suspended = (create_flags & THREAD_CREATE_FLAGS_CREATE_SUSPENDED) != 0;
    let thread_handle = multitask::with_current_user_process_state_mut(|_, _, st| {
        let objs = st.win32_objects_mut();
        let h = WIN32_THREAD_HANDLE_BASE + objs.next_thread_seq + 1;
        objs.next_thread_seq += 1;
        objs.threads.push(Win32ThreadHandleObject {
            handle: h,
            tid: child_tid,
            teb_address: teb_va,
            start_address: start_routine,
            suspended,
        });
        h
    });
    let Some(thread_handle) = thread_handle else {
        let _ = multitask::terminate_user_task(child_tid);
        return STATUS_NO_MEMORY;
    };

    if write_user_u64(handle_ptr, thread_handle) != STATUS_SUCCESS {
        let _ = multitask::terminate_user_task(child_tid);
        return STATUS_INVALID_PARAMETER;
    }

    if attr_list_ptr != 0
        && let Ok(total_len) = read_user_u64(attr_list_ptr)
        && total_len >= 8 + size_of::<PsAttribute64>() as u64
    {
        let count = ((total_len as usize) - 8) / size_of::<PsAttribute64>();
        for i in 0..count.min(8) {
            let off = attr_list_ptr + 8 + (i * size_of::<PsAttribute64>()) as u64;
            let Ok(attr) = usermem::read_current_user_struct::<PsAttribute64>(off) else {
                break;
            };
            if attr.attribute == PS_ATTRIBUTE_CLIENT_ID && attr.value != 0 && attr.size >= 16 {
                let _ = usermem::write_current_user_bytes(attr.value, &cid_bytes);
            } else if attr.attribute == PS_ATTRIBUTE_TEB_ADDRESS
                && attr.value != 0
                && attr.size >= 8
            {
                let _ = write_user_u64(attr.value, teb_va);
            }
        }
    }

    if !suspended {
        if !multitask::activate_suspended_user_task(child_tid) {
            let _ = multitask::terminate_user_task(child_tid);
            return STATUS_NO_MEMORY;
        }
        multitask::set_next_spawn_pick_hint(child_tid);
        multitask::request_deferred_reschedule();
    }

    STATUS_SUCCESS
}

pub(super) fn nt_create_thread(frame: &SyscallFrame) -> u64 {
    // Legacy NtCreateThread(ThreadHandle, DesiredAccess, ObjectAttributes, ProcessHandle,
    //   ClientId, ThreadContext, InitialTeb, CreateSuspended)
    let (handle_ptr, _access, _oa_ptr, proc_handle) =
        (frame.r10, frame.rdx, frame.r8, frame.r9);
    let client_id_ptr = read_stack_arg(frame, 4).unwrap_or(0);
    let context_ptr = read_stack_arg(frame, 5).unwrap_or(0);
    let initial_teb_ptr = read_stack_arg(frame, 6).unwrap_or(0);
    let create_suspended = read_stack_arg(frame, 7).unwrap_or(0) != 0;
    if handle_ptr == 0 || context_ptr == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    if proc_handle != HANDLE_CURRENT_PROCESS
        && resolve_process_id(proc_handle) != multitask::current_user_process_id()
    {
        return STATUS_INVALID_HANDLE;
    }
    let Some(pid) = multitask::current_user_process_id() else {
        return STATUS_INVALID_PARAMETER;
    };
    let mut ctx_buf = [0_u8; 1232];
    if usermem::copy_from_current_user_exact(context_ptr, &mut ctx_buf).is_err() {
        return STATUS_INVALID_PARAMETER;
    }
    let rip = u64::from_le_bytes(ctx_buf[0xf8..0x100].try_into().unwrap_or([0; 8]));
    let rsp = u64::from_le_bytes(ctx_buf[0x98..0xa0].try_into().unwrap_or([0; 8]));
    let rcx = u64::from_le_bytes(ctx_buf[0x80..0x88].try_into().unwrap_or([0; 8]));
    let rdx = u64::from_le_bytes(ctx_buf[0x88..0x90].try_into().unwrap_or([0; 8]));
    if rip < paging::USER_SPACE_BASE || rsp < paging::USER_SPACE_BASE {
        return STATUS_INVALID_PARAMETER;
    }
    let _ = initial_teb_ptr;
    let rw_flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::USER_ACCESSIBLE
        | PageTableFlags::NO_EXECUTE;
    let Some(teb_va) = alloc_win32_zeroed_pages(4 * PAGE_SIZE, PAGE_READWRITE as u32, rw_flags)
    else {
        return STATUS_NO_MEMORY;
    };
    let mut teb_page = vec![0_u8; PAGE_SIZE as usize];
    teb_page[0x30..0x38].copy_from_slice(&teb_va.to_le_bytes());
    let _ = usermem::write_current_user_bytes(teb_va, &teb_page);

    let mut bootstrap =
        multitask::UserTaskBootstrap::new(UserAbi::Windows, VirtAddr::new(rip), VirtAddr::new(rsp));
    bootstrap.windows_thread_state = Some(WindowsThreadRuntimeState::new(0, teb_va));
    bootstrap.registers = multitask::UserTaskRegisters {
        rcx,
        rdx,
        r11: 0x202,
        ..multitask::UserTaskRegisters::default()
    };
    let Ok(child_tid) = multitask::spawn_user_thread_suspended(bootstrap) else {
        return STATUS_NO_MEMORY;
    };
    let mut cid_bytes = [0_u8; 16];
    cid_bytes[0..8].copy_from_slice(&pid.to_le_bytes());
    cid_bytes[8..16].copy_from_slice(&child_tid.to_le_bytes());
    let _ = usermem::write_current_user_bytes(teb_va + 0x40, &cid_bytes);
    if client_id_ptr != 0 {
        let _ = usermem::write_current_user_bytes(client_id_ptr, &cid_bytes);
    }

    let handle = multitask::with_current_user_process_state_mut(|_, _, st| {
        let objs = st.win32_objects_mut();
        let h = WIN32_THREAD_HANDLE_BASE + objs.next_thread_seq + 1;
        objs.next_thread_seq += 1;
        objs.threads.push(Win32ThreadHandleObject {
            handle: h,
            tid: child_tid,
            teb_address: teb_va,
            start_address: rip,
            suspended: create_suspended,
        });
        h
    });
    let Some(handle) = handle else {
        let _ = multitask::terminate_user_task(child_tid);
        return STATUS_NO_MEMORY;
    };
    if !create_suspended {
        let _ = multitask::activate_suspended_user_task(child_tid);
        multitask::request_deferred_reschedule();
    }
    write_user_u64(handle_ptr, handle)
}

pub(super) fn nt_resume_thread(thread_handle: u64, prev_count_ptr: u64) -> u64 {
    let mut was_suspended = false;
    let tid = multitask::with_current_user_process_state_mut(|_, _, st| {
        let th = st
            .win32_objects_mut()
            .threads
            .iter_mut()
            .find(|t| t.handle == thread_handle)?;
        was_suspended = th.suspended;
        th.suspended = false;
        Some(th.tid)
    })
    .flatten();
    let Some(tid) = tid else {
        return if thread_handle == HANDLE_CURRENT_THREAD {
            if prev_count_ptr != 0 {
                let _ = write_user_u32_checked(prev_count_ptr, 0);
            }
            STATUS_SUCCESS
        } else {
            STATUS_INVALID_HANDLE
        };
    };
    if was_suspended {
        let _ = multitask::activate_suspended_user_task(tid);
        multitask::set_next_spawn_pick_hint(tid);
        multitask::request_deferred_reschedule();
    }
    if prev_count_ptr != 0 {
        return write_user_u32_checked(prev_count_ptr, u32::from(was_suspended));
    }
    STATUS_SUCCESS
}

pub(super) fn nt_suspend_thread(thread_handle: u64, prev_count_ptr: u64) -> u64 {
    if resolve_thread_id(thread_handle).is_none() {
        return STATUS_INVALID_HANDLE;
    }
    if prev_count_ptr != 0 {
        return write_user_u32_checked(prev_count_ptr, 0);
    }
    STATUS_SUCCESS
}

pub(super) fn nt_open_thread(frame: &SyscallFrame) -> u64 {
    let (handle_ptr, _access, _oa_ptr, client_id_ptr) =
        (frame.r10, frame.rdx, frame.r8, frame.r9);
    if handle_ptr == 0 || client_id_ptr == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let Ok(cid) = usermem::read_current_user_struct::<ClientId64>(client_id_ptr) else {
        return STATUS_INVALID_PARAMETER;
    };
    let tid = cid.unique_thread;
    let cur_pid = multitask::current_user_process_id().unwrap_or(0);
    if tid == 0
        || multitask::user_target_abi_if_alive(
            if cid.unique_process != 0 {
                cid.unique_process
            } else {
                cur_pid
            },
            tid,
        ) != Some(UserAbi::Windows)
    {
        return STATUS_INVALID_HANDLE;
    }
    let handle = multitask::with_current_user_process_state_mut(|_, _, st| {
        let objs = st.win32_objects_mut();
        let h = WIN32_THREAD_HANDLE_BASE + objs.next_thread_seq + 1;
        objs.next_thread_seq += 1;
        objs.threads.push(Win32ThreadHandleObject {
            handle: h,
            tid,
            teb_address: 0,
            start_address: 0,
            suspended: false,
        });
        h
    });
    match handle {
        Some(h) => write_user_u64(handle_ptr, h),
        None => STATUS_NO_MEMORY,
    }
}

pub(super) fn nt_open_process(frame: &SyscallFrame) -> u64 {
    let (handle_ptr, _access, _oa_ptr, client_id_ptr) =
        (frame.r10, frame.rdx, frame.r8, frame.r9);
    if handle_ptr == 0 || client_id_ptr == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let Ok(cid) = usermem::read_current_user_struct::<ClientId64>(client_id_ptr) else {
        return STATUS_INVALID_PARAMETER;
    };
    let pid = cid.unique_process;
    if pid == 0 || multitask::user_target_abi_if_alive(pid, 0).is_none() {
        return STATUS_INVALID_HANDLE;
    }
    let handle = multitask::with_current_user_process_state_mut(|_, _, st| {
        let objs = st.win32_objects_mut();
        let h = WIN32_PROCESS_HANDLE_BASE + objs.next_process_seq + 1;
        objs.next_process_seq += 1;
        objs.processes.push(Win32ProcessHandleObject { handle: h, pid });
        h
    });
    match handle {
        Some(h) => write_user_u64(handle_ptr, h),
        None => STATUS_NO_MEMORY,
    }
}

pub(super) fn nt_read_or_write_virtual_memory(frame: &SyscallFrame, is_write: bool) -> u64 {
    let (proc_handle, base_addr, buf_ptr, num_bytes) =
        (frame.r10, frame.rdx, frame.r8, frame.r9 as usize);
    let ret_bytes_ptr = read_stack_arg(frame, 4).unwrap_or(0);
    let Some(target_pid) = resolve_process_id(proc_handle) else {
        return STATUS_INVALID_HANDLE;
    };
    if multitask::current_user_process_id() != Some(target_pid) {
        return STATUS_ACCESS_DENIED;
    }
    if num_bytes > 16 * 1024 * 1024 {
        return STATUS_INVALID_PARAMETER;
    }
    if num_bytes > 0 {
        let mut tmp = vec![0_u8; num_bytes];
        if is_write {
            if usermem::copy_from_current_user_exact(buf_ptr, &mut tmp).is_err()
                || usermem::write_current_user_bytes(base_addr, &tmp).is_err()
            {
                return STATUS_ACCESS_DENIED;
            }
        } else if usermem::copy_from_current_user_exact(base_addr, &mut tmp).is_err()
            || usermem::write_current_user_bytes(buf_ptr, &tmp).is_err()
        {
            return STATUS_ACCESS_DENIED;
        }
    }
    if ret_bytes_ptr != 0 {
        let _ = write_user_u64(ret_bytes_ptr, num_bytes as u64);
    }
    STATUS_SUCCESS
}

pub(super) fn nt_get_context_thread(
    frame: &SyscallFrame,
    thread_handle: u64,
    context_ptr: u64,
) -> u64 {
    let Some(target_tid) = resolve_thread_id(thread_handle) else {
        return STATUS_INVALID_HANDLE;
    };
    if context_ptr == 0 || Some(target_tid) != multitask::current_user_thread_id() {
        return if context_ptr == 0 {
            STATUS_INVALID_PARAMETER
        } else {
            STATUS_NOT_IMPLEMENTED
        };
    }
    let mut ctx = [0_u8; 1232];
    if usermem::copy_from_current_user_exact(context_ptr, &mut ctx).is_err() {
        return STATUS_INVALID_PARAMETER;
    }
    ctx[0x44..0x48].copy_from_slice(&(frame.user_rflags as u32).to_le_bytes());
    ctx[0x78..0x80].copy_from_slice(&frame.rax.to_le_bytes());
    ctx[0x80..0x88].copy_from_slice(&frame.user_rip.to_le_bytes());
    ctx[0x88..0x90].copy_from_slice(&frame.rdx.to_le_bytes());
    ctx[0x90..0x98].copy_from_slice(&frame.rbx.to_le_bytes());
    ctx[0x98..0xa0].copy_from_slice(&frame.user_rsp.to_le_bytes());
    ctx[0xa0..0xa8].copy_from_slice(&frame.rbp.to_le_bytes());
    ctx[0xa8..0xb0].copy_from_slice(&frame.rsi.to_le_bytes());
    ctx[0xb0..0xb8].copy_from_slice(&frame.rdi.to_le_bytes());
    ctx[0xb8..0xc0].copy_from_slice(&frame.r8.to_le_bytes());
    ctx[0xc0..0xc8].copy_from_slice(&frame.r9.to_le_bytes());
    ctx[0xc8..0xd0].copy_from_slice(&frame.r10.to_le_bytes());
    ctx[0xd0..0xd8].copy_from_slice(&frame.user_rflags.to_le_bytes());
    ctx[0xd8..0xe0].copy_from_slice(&frame.r12.to_le_bytes());
    ctx[0xe0..0xe8].copy_from_slice(&frame.r13.to_le_bytes());
    ctx[0xe8..0xf0].copy_from_slice(&frame.r14.to_le_bytes());
    ctx[0xf0..0xf8].copy_from_slice(&frame.r15.to_le_bytes());
    ctx[0xf8..0x100].copy_from_slice(&frame.user_rip.to_le_bytes());
    if usermem::write_current_user_bytes(context_ptr, &ctx).is_err() {
        return STATUS_INVALID_PARAMETER;
    }
    STATUS_SUCCESS
}

pub(super) fn nt_set_context_thread(
    frame: &mut SyscallFrame,
    thread_handle: u64,
    context_ptr: u64,
) -> u64 {
    let Some(target_tid) = resolve_thread_id(thread_handle) else {
        return STATUS_INVALID_HANDLE;
    };
    if context_ptr == 0 || Some(target_tid) != multitask::current_user_thread_id() {
        return if context_ptr == 0 {
            STATUS_INVALID_PARAMETER
        } else {
            STATUS_NOT_IMPLEMENTED
        };
    }
    super::nt_dispatch::nt_continue(frame, context_ptr)
}

/// Dispatches a first-chance user exception raised via `NtRaiseException` onto
/// `ntdll!KiUserExceptionDispatcher` using Wine's x64 signal-frame stack layout:
/// `[RSP + 0x000] = CONTEXT`, `[RSP + 0x4f0] = EXCEPTION_RECORD`,
/// `[RSP + 0x590] = machine frame (Rip, CS, EFlags, Rsp, SS)`.
pub(super) fn nt_raise_exception(
    frame: &mut SyscallFrame,
    exc_record_ptr: u64,
    context_ptr: u64,
    first_chance: u64,
) -> u64 {
    if exc_record_ptr == 0 || context_ptr == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let mut rec_buf = [0_u8; 152];
    let mut ctx_buf = [0_u8; 1232];
    if usermem::copy_from_current_user_exact(exc_record_ptr, &mut rec_buf).is_err()
        || usermem::copy_from_current_user_exact(context_ptr, &mut ctx_buf).is_err()
    {
        return STATUS_INVALID_PARAMETER;
    }
    let exc_code = u32::from_le_bytes(rec_buf[0..4].try_into().unwrap_or([0; 4]));

    let dispatcher = if first_chance != 0 {
        multitask::with_current_user_process_state_mut(|_, _, st| {
            find_ntdll_export_address(st, "KiUserExceptionDispatcher")
        })
        .flatten()
    } else {
        None
    };

    if let Some(ki_disp) = dispatcher {
        let orig_rip = u64::from_le_bytes(ctx_buf[0xf8..0x100].try_into().unwrap_or([0; 8]));
        let orig_rsp = u64::from_le_bytes(ctx_buf[0x98..0xa0].try_into().unwrap_or([0; 8]));
        let orig_eflags = u32::from_le_bytes(ctx_buf[0x44..0x48].try_into().unwrap_or([0; 4]));
        // Wine's KiUserExceptionDispatcher reserves 0x590 bytes before the machine frame,
        // placing CONTEXT at [rsp+0x000] and EXCEPTION_RECORD at [rsp+0x4f0].
        const STACK_FRAME_SIZE: u64 = 0x5c0;
        let base_rsp = if orig_rsp >= paging::USER_SPACE_BASE + STACK_FRAME_SIZE + 0x100 {
            (orig_rsp & !0xf) - STACK_FRAME_SIZE
        } else {
            (frame.user_rsp & !0xf) - STACK_FRAME_SIZE
        };
        let mut stack_frame = vec![0_u8; STACK_FRAME_SIZE as usize];
        stack_frame[0..1232].copy_from_slice(&ctx_buf);
        stack_frame[0x4f0..0x4f0 + 152].copy_from_slice(&rec_buf);
        stack_frame[0x590..0x598].copy_from_slice(&orig_rip.to_le_bytes());
        stack_frame[0x598..0x5a0].copy_from_slice(&0x33_u64.to_le_bytes()); // CS
        stack_frame[0x5a0..0x5a8].copy_from_slice(&(orig_eflags as u64).to_le_bytes());
        stack_frame[0x5a8..0x5b0].copy_from_slice(&orig_rsp.to_le_bytes());
        stack_frame[0x5b0..0x5b8].copy_from_slice(&0x2b_u64.to_le_bytes()); // SS
        if usermem::write_current_user_bytes(base_rsp, &stack_frame).is_ok() {
            frame.user_rsp = base_rsp;
            frame.user_rip = ki_disp;
            frame.rdx = base_rsp;
            return frame.rax;
        }
    }

    if let Some(pid) = multitask::current_user_process_id() {
        super::super::linux::record_linux_process_termination(pid, ((exc_code & 0xff) << 8) as i32);
    }
    multitask::exit_current_user_process()
}

pub(super) fn nt_open_token(frame: &SyscallFrame) -> u64 {
    let (_proc_or_thread, _access, handle_ptr) = (frame.r10, frame.rdx, frame.r8);
    if handle_ptr == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    write_user_u64(handle_ptr, 0x3000_0001)
}

pub(super) fn nt_query_information_token(frame: &SyscallFrame) -> u64 {
    let (_token, info_class, info_ptr, length) = (
        frame.r10,
        (frame.rdx as u32) as u64,
        frame.r8,
        (frame.r9 as u32) as usize,
    );
    let ret_len_ptr = read_stack_arg(frame, 4).unwrap_or(0);
    match info_class {
        8 /* TokenType */ | 18 /* TokenElevationType */ | 20 /* TokenElevation */ => {
            if ret_len_ptr != 0 {
                let _ = write_user_u32_checked(ret_len_ptr, 4);
            }
            if length < 4 || info_ptr == 0 {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let val = if info_class == 8 { 1_u32 } else { 0_u32 };
            write_user_u32_checked(info_ptr, val)
        }
        12 /* TokenSessionId */ | 19 /* TokenLinkedToken */ => {
            if ret_len_ptr != 0 {
                let _ = write_user_u32_checked(ret_len_ptr, 4);
            }
            if length < 4 || info_ptr == 0 {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            write_user_u32_checked(info_ptr, 1)
        }
        25 /* TokenIntegrityLevel */ => {
            // 16-byte SID_AND_ATTRIBUTES + 12-byte S-1-16-8192 (Medium) SID = 28 bytes
            if ret_len_ptr != 0 {
                let _ = write_user_u32_checked(ret_len_ptr, 28);
            }
            if length < 28 || info_ptr == 0 {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let sid_ptr = info_ptr + 16;
            let mut buf = [0_u8; 28];
            buf[0..8].copy_from_slice(&sid_ptr.to_le_bytes());
            buf[8..12].copy_from_slice(&0x60_u32.to_le_bytes()); // SE_GROUP_INTEGRITY | ENABLED
            buf[16] = 1; // Revision
            buf[17] = 1; // SubAuthorityCount
            buf[23] = 16; // SECURITY_MANDATORY_LABEL_AUTHORITY
            buf[24..28].copy_from_slice(&0x2000_u32.to_le_bytes());
            if usermem::write_current_user_bytes(info_ptr, &buf).is_err() {
                return STATUS_INVALID_PARAMETER;
            }
            STATUS_SUCCESS
        }
        _ => STATUS_INVALID_INFO_CLASS,
    }
}

pub(super) fn nt_allocate_locally_unique_id(luid_ptr: u64) -> u64 {
    use core::sync::atomic::{AtomicU64, Ordering};
    static NEXT_LUID: AtomicU64 = AtomicU64::new(0x1000);
    if luid_ptr == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let val = NEXT_LUID.fetch_add(1, Ordering::Relaxed);
    write_user_u64(luid_ptr, val)
}

pub(super) fn nt_query_security_object(frame: &SyscallFrame) -> u64 {
    let (_handle, _sec_info, buf_ptr, length) = (
        frame.r10,
        frame.rdx,
        frame.r8,
        (frame.r9 as u32) as usize,
    );
    let ret_len_ptr = read_stack_arg(frame, 4).unwrap_or(0);
    // Minimal self-relative SECURITY_DESCRIPTOR (20 bytes).
    if ret_len_ptr != 0 {
        let _ = write_user_u32_checked(ret_len_ptr, 20);
    }
    if length < 20 || buf_ptr == 0 {
        return rustos_user_abi::windows::STATUS_BUFFER_TOO_SMALL;
    }
    let mut sd = [0_u8; 20];
    sd[0] = 1; // Revision
    sd[2..4].copy_from_slice(&0x8000_u16.to_le_bytes()); // SE_SELF_RELATIVE
    if usermem::write_current_user_bytes(buf_ptr, &sd).is_err() {
        return STATUS_INVALID_PARAMETER;
    }
    STATUS_SUCCESS
}
